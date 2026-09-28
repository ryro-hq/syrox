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

fn recipe_kind(ty: &CanonicalType) -> Option<bool> {
    let CanonicalType::Specialization {
        template,
        arguments,
    } = ty
    else {
        return None;
    };
    if template.domain() != syrox_lang::SourceDomainId::standard_library() || arguments.len() != 1 {
        return None;
    }
    match template.path() {
        [std, pkg, name] if std == "std" && pkg == "pkg" => match name.as_str() {
            "Recipe" => Some(false),
            "ApplicationRecipe" => Some(true),
            _ => None,
        },
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
    let names: &[&str] = if recipe_kind(ty) == Some(true) {
        &[
            "package",
            "acquisition",
            "build",
            "build_inputs",
            "application",
        ]
    } else {
        &["package", "acquisition", "build"]
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
