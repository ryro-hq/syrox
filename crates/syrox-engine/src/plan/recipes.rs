use syrox_lang::RealizedRoot;

use super::{
    AuthenticatedStandardLibrary, CanonicalItemIdentity, CanonicalType, PlanError,
    ProjectionBudget, RealizedProgram, Value, decode_package_id, exact_nominal,
};

/// A borrowed contract value from a root or one of its recipe fields. All
/// components share the already evaluated recipe's identity and provenance.
pub(super) struct Component<'a> {
    root: &'a RealizedRoot,
    ty: &'a CanonicalType,
    value: &'a Value,
}

impl Component<'_> {
    pub(super) fn name(&self) -> &str {
        self.root.name()
    }
    pub(super) fn identity(&self) -> &CanonicalItemIdentity {
        self.root.identity()
    }
    pub(super) fn is_selected(&self) -> bool {
        self.root.is_selected()
    }
    pub(super) const fn ty(&self) -> &CanonicalType {
        self.ty
    }
    pub(super) const fn value(&self) -> &Value {
        self.value
    }
}

pub(super) fn components<'a>(
    realized: &'a RealizedProgram,
    standard_library: Option<&AuthenticatedStandardLibrary>,
    budget: &mut ProjectionBudget,
) -> Result<Vec<Component<'a>>, PlanError> {
    let mut result = Vec::new();
    for root in realized.roots() {
        let (Some(ty), Some(value)) = (root.ty(), root.value()) else {
            return Err(PlanError::UnrealizedRoot {
                root: budget.string(root.name())?,
            });
        };
        if standard_library.is_some() && recipe_kind(ty).is_some() {
            recipe_components(root, ty, value, &mut result, budget)?;
        } else {
            push(&mut result, Component { root, ty, value }, budget)?;
        }
    }
    Ok(result)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RecipeKind {
    Basic,
    Application,
    Catalog,
}

fn recipe_kind(ty: &CanonicalType) -> Option<RecipeKind> {
    match ty {
        CanonicalType::Specialization {
            template,
            arguments,
        } if template.domain() == syrox_lang::SourceDomainId::standard_library()
            && arguments.len() == 1 =>
        {
            match template.path() {
                [std, pkg, name] if std == "std" && pkg == "pkg" => match name.as_str() {
                    "Recipe" => Some(RecipeKind::Basic),
                    "ApplicationRecipe" => Some(RecipeKind::Application),
                    _ => None,
                },
                _ => None,
            }
        }
        CanonicalType::Nominal(identity)
            if identity.domain() == syrox_lang::SourceDomainId::standard_library()
                && identity.path() == ["std", "pkg", "CatalogRecipe"] =>
        {
            Some(RecipeKind::Catalog)
        }
        _ => None,
    }
}

fn recipe_components<'a>(
    root: &'a RealizedRoot,
    ty: &'a CanonicalType,
    value: &'a Value,
    result: &mut Vec<Component<'a>>,
    budget: &mut ProjectionBudget,
) -> Result<(), PlanError> {
    let name = budget.string(root.name())?;
    let invalid = |reason| PlanError::InvalidRecipe {
        root: name.clone(),
        reason,
    };
    let Value::Struct { fields, .. } = value else {
        return Err(invalid("expected a recipe struct"));
    };
    let kind = recipe_kind(ty);
    let names: &[&str] = if kind == Some(RecipeKind::Basic) {
        &["package", "acquisition", "build"]
    } else {
        &[
            "package",
            "acquisition",
            "build",
            "build_inputs",
            "application",
        ]
    };
    if fields.len() != names.len()
        || !fields
            .iter()
            .zip(names)
            .all(|((name, _), expected)| name == expected)
    {
        return Err(invalid("unexpected recipe fields"));
    }
    let Value::Struct {
        fields: package_fields,
        ..
    } = &fields[0].1
    else {
        return Err(invalid("expected Package"));
    };
    let package = package_fields
        .first()
        .and_then(|(_, value)| decode_package_id(value))
        .ok_or_else(|| invalid("recipe package has no PackageId"))?;
    for (index, (field, value)) in fields.iter().enumerate() {
        budget.charge_work()?;
        let value = if kind == Some(RecipeKind::Catalog) && index >= 3 {
            let contract = if index == 3 {
                "BuildInputs"
            } else {
                "Application"
            };
            match unwrap_optional(value, contract) {
                OptionalComponent::Present(value) => value,
                OptionalComponent::Absent => continue,
                OptionalComponent::Invalid => {
                    return Err(invalid("invalid optional recipe component"));
                }
            }
        } else {
            value
        };
        let value = if index == 2 {
            unwrap_build(value)
                .ok_or_else(|| invalid("recipe build has no supported build contract"))?
        } else {
            value
        };
        let Value::Struct { ty, fields, .. } = value else {
            return Err(invalid("recipe component must be a contract struct"));
        };
        let contract = match index {
            0 => "Package",
            1 => "Acquisition",
            3 => "BuildInputs",
            4 => "Application",
            _ if exact_nominal(ty, &["std", "pkg", "AutotoolsBuild"]) => "AutotoolsBuild",
            _ if exact_nominal(ty, &["std", "pkg", "GlibcBuild"]) => "GlibcBuild",
            _ => return Err(invalid("recipe build has no supported build contract")),
        };
        if !exact_nominal(ty, &["std", "pkg", contract]) {
            return Err(invalid("recipe component has the wrong nominal contract"));
        }
        if field != "package"
            && fields
                .first()
                .and_then(|(_, value)| decode_package_id(value))
                != Some(package)
        {
            return Err(invalid("recipe components must refer to the same package"));
        }
        push(result, Component { root, ty, value }, budget)?;
    }
    Ok(())
}

enum OptionalComponent<'a> {
    Invalid,
    Absent,
    Present(&'a Value),
}

fn unwrap_optional<'a>(value: &'a Value, contract: &str) -> OptionalComponent<'a> {
    let Value::Variant { ty, index, payload } = value else {
        return OptionalComponent::Invalid;
    };
    let CanonicalType::Specialization {
        template,
        arguments,
    } = ty.as_ref()
    else {
        return OptionalComponent::Invalid;
    };
    if template.domain() != syrox_lang::SourceDomainId::standard_library()
        || template.path() != ["std", "option", "Option"]
        || arguments.len() != 1
        || !exact_nominal(&arguments[0], &["std", "pkg", contract])
    {
        return OptionalComponent::Invalid;
    }
    match (*index, payload.as_slice()) {
        (0, []) => OptionalComponent::Absent,
        (1, [value]) => OptionalComponent::Present(value),
        _ => OptionalComponent::Invalid,
    }
}

fn unwrap_build(value: &Value) -> Option<&Value> {
    let Value::Variant { ty, index, payload } = value else {
        return Some(value);
    };
    if !exact_nominal(ty, &["std", "pkg", "Build"]) {
        return None;
    }
    let [build @ Value::Struct { ty: backend, .. }] = payload.as_slice() else {
        return None;
    };
    let path = match index {
        0 => &["std", "pkg", "AutotoolsBuild"][..],
        1 => &["std", "pkg", "GlibcBuild"][..],
        _ => return None,
    };
    exact_nominal(backend, path).then_some(build)
}

fn push<'a>(
    result: &mut Vec<Component<'a>>,
    component: Component<'a>,
    budget: &mut ProjectionBudget,
) -> Result<(), PlanError> {
    budget.node::<Component<'a>>()?;
    // Account for geometric Vec growth as well as the live borrowed component.
    budget.bytes(size_of::<Component<'a>>())?;
    result.push(component);
    Ok(())
}
