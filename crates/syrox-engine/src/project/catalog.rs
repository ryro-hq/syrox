//! Deterministic, locked projection of an input's exported recipe factories.
use std::collections::BTreeSet;

use syrox_lang::{
    ExpressionKind, ItemKind, OutputKind, ParsedProgram, SourceSet, StringPart, TypeKind,
    parse_sources,
};

use super::{CheckFailure, LoadedProjectInput};

const RECIPE_TYPES: &[&str] = &[
    "Package",
    "Acquisition",
    "AutotoolsBuild",
    "GlibcBuild",
    "BuildInputs",
    "Application",
];

fn type_name(ty: &syrox_lang::Type) -> Option<&str> {
    let TypeKind::Named { path, arguments } = &ty.kind else {
        return None;
    };
    if !arguments.is_empty() || path.segments.len() != 2 || path.segments[0].text != "std" {
        return None;
    }
    Some(path.segments[1].text.as_str())
}

fn recipe_build(ty: &syrox_lang::Type) -> Option<(&str, bool)> {
    let TypeKind::Named { path, arguments } = &ty.kind else {
        return None;
    };
    if path.segments.len() != 2 || path.segments[0].text != "std" || arguments.len() != 1 {
        return None;
    }
    let application = match path.segments[1].text.as_str() {
        "Recipe" => false,
        "ApplicationRecipe" => true,
        _ => return None,
    };
    let build = type_name(&arguments[0])?;
    ["AutotoolsBuild", "GlibcBuild"]
        .contains(&build)
        .then_some((build, application))
}

fn invalid(reason: impl Into<String>) -> CheckFailure {
    CheckFailure::InvalidCatalog {
        reason: reason.into(),
    }
}

type FactoryExport = (String, String, String);

fn add_factory(
    name: &str,
    function: &str,
    result: &syrox_lang::Type,
    selected: &str,
    factories: &mut BTreeSet<String>,
    exports: &mut Vec<FactoryExport>,
) -> Result<(), CheckFailure> {
    let recipe = recipe_build(result);
    let direct = type_name(result).filter(|ty| RECIPE_TYPES.contains(ty));
    if recipe.is_none() && direct.is_none() {
        return Ok(());
    }
    if !factories.insert(name.to_owned()) {
        return Err(invalid("duplicate factory export across recipe files"));
    }
    let call = format!("{selected}::{function}()");
    if let Some((build, application)) = recipe {
        let recipe = if application {
            "ApplicationRecipe"
        } else {
            "Recipe"
        };
        exports.push((name.to_owned(), format!("{recipe}<std::{build}>"), call));
    } else if let Some(ty) = direct {
        exports.push((name.to_owned(), ty.to_owned(), call));
    }
    Ok(())
}

pub(super) fn selected_input(main: &ParsedProgram) -> Result<Option<String>, CheckFailure> {
    let mut selected = None;
    for item in &main.items {
        let ItemKind::Outputs(outputs) = &item.kind else {
            continue;
        };
        for output in &outputs.entries {
            let OutputKind::Value { ty, value, .. } = &output.kind else {
                continue;
            };
            if type_name(ty) != Some("Catalog") {
                continue;
            }
            if selected.is_some() {
                return Err(invalid("declare only one std::Catalog root"));
            }
            let ExpressionKind::Struct {
                path,
                fields,
                type_arguments,
            } = &value.kind
            else {
                return Err(invalid("catalog must use an exact std::Catalog literal"));
            };
            if !type_arguments.is_empty()
                || path.segments.len() != 2
                || path.segments[0].text != "std"
                || path.segments[1].text != "Catalog"
                || fields.len() != 1
                || fields[0].name.text != "input"
            {
                return Err(invalid("catalog must name one input"));
            }
            let ExpressionKind::String(name) = &fields[0].value.kind else {
                return Err(invalid("catalog input must be a literal name"));
            };
            let [StringPart::Text { source: alias, .. }] = name.parts.as_slice() else {
                return Err(invalid("catalog input must be an unescaped literal name"));
            };
            selected = Some(alias.clone());
        }
    }
    Ok(selected)
}

pub(super) fn generated_exports(
    sources: &SourceSet,
    main: &ParsedProgram,
    inputs: &[LoadedProjectInput],
) -> Result<Option<String>, CheckFailure> {
    let Some(selected) = selected_input(main)? else {
        return Ok(None);
    };
    let Some(input) = inputs.iter().find(|input| input.name == selected) else {
        return Err(invalid(format!("input `{selected}` is not declared")));
    };
    if !input.locator().starts_with("modules:") {
        return Err(invalid(format!(
            "catalog input `{selected}` requires a modules: locator"
        )));
    }
    let parsed = parse_sources(sources).map_err(|errors| CheckFailure::Diagnostics {
        input: sources.clone(),
        errors,
    })?;
    let mut exports = Vec::new();
    let mut factories = BTreeSet::new();
    for source in parsed
        .iter()
        .filter(|source| source.domain() == input.domain)
    {
        let module_name = source.module().join("::");
        let access = if module_name.is_empty() {
            selected.clone()
        } else {
            format!("{selected}::{module_name}")
        };
        for item in &source.program().items {
            match &item.kind {
                ItemKind::Function(function) if item.public && function.parameters.is_empty() => {
                    if let Some(result) = &function.result {
                        add_factory(
                            if function.name.text == "recipe" {
                                source.module().last().map_or("recipe", String::as_str)
                            } else {
                                &function.name.text
                            },
                            &function.name.text,
                            result,
                            &access,
                            &mut factories,
                            &mut exports,
                        )?;
                    }
                }
                ItemKind::Outputs(outputs) => {
                    for output in &outputs.entries {
                        if let OutputKind::Function {
                            name, signature, ..
                        } = &output.kind
                            && signature.parameters.is_empty()
                        {
                            add_factory(
                                &name.text,
                                &name.text,
                                &signature.result,
                                &access,
                                &mut factories,
                                &mut exports,
                            )?;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    exports.sort_by(|left, right| left.0.cmp(&right.0));
    if exports.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(invalid("duplicate factory export across recipe files"));
    }
    if exports.is_empty() {
        return Err(invalid("input has no supported recipe factories"));
    }
    let mut generated = String::from("outputs {\n");
    for (name, ty, expression) in exports {
        use std::fmt::Write as _;
        writeln!(generated, "    {name}: std::{ty} = {expression};")
            .expect("writing to String is infallible");
    }
    generated.push_str("}\n");
    Ok(Some(generated))
}
