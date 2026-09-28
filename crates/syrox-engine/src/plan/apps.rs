use super::{
    AuthenticatedStandardLibrary, BTreeMap, BTreeSet, CanonicalType, PlanBuild, PlanError,
    PlanPackage, PlanPackageId, ProjectionBudget, Value, decode_package_id, exact_nominal,
};

const APPLICATION_PATH: &[&str] = &["std", "pkg", "Application"];
const DEFAULT_PATH: &[&str] = &["std", "pkg", "DefaultApplication"];
const LOADER_PATH: &[&str] = &["std", "pkg", "RuntimeLoader"];
const LIBRARY_PATH: &[&str] = &["std", "pkg", "RuntimeLibrary"];
const MAX_RUNTIME_LIBRARIES: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanApplication {
    package: PlanPackageId,
    loader: Option<PlanPackageId>,
    libraries: Vec<PlanPackageId>,
}

impl PlanApplication {
    pub const fn package(&self) -> &PlanPackageId {
        &self.package
    }
    pub const fn loader(&self) -> Option<&PlanPackageId> {
        self.loader.as_ref()
    }
    pub fn libraries(&self) -> impl ExactSizeIterator<Item = &PlanPackageId> {
        self.libraries.iter()
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn extract(
    components: &[super::recipes::Component<'_>],
    packages: &[PlanPackage],
    builds: &[PlanBuild],
    standard_library: Option<&AuthenticatedStandardLibrary>,
    budget: &mut ProjectionBudget,
) -> Result<(Vec<PlanApplication>, Option<PlanPackageId>), PlanError> {
    if standard_library.is_none() {
        return Ok((Vec::new(), None));
    }
    let mut roots: Vec<_> = components
        .iter()
        .filter(|root| {
            root.identity().domain() == syrox_lang::SourceDomainId::project()
                && (exact_nominal(root.ty(), APPLICATION_PATH)
                    || exact_nominal(root.ty(), DEFAULT_PATH))
        })
        .collect();
    roots.sort_by(|left, right| left.identity().cmp(right.identity()));
    if roots.len() > super::MAX_PLAN_PACKAGE_NODES {
        return Err(PlanError::NodeLimit);
    }
    let mut by_id = BTreeMap::new();
    for package in packages {
        budget.node::<&PlanPackage>()?;
        by_id.insert(package.id().as_str(), package);
    }
    let mut build_ids = BTreeSet::new();
    for build in builds {
        budget.node::<&PlanBuild>()?;
        build_ids.insert(build.package().as_str());
    }
    let mut apps = budget.collection::<PlanApplication>(roots.len())?;
    let mut default = None;
    for root in roots {
        budget.node::<PlanApplication>()?;
        let name = budget.string(root.name())?;
        let invalid = |reason| PlanError::InvalidApplication {
            root: name.clone(),
            reason,
        };
        let Value::Struct { ty, fields, .. } = root.value() else {
            return Err(invalid("expected an exact std application struct"));
        };
        let is_default = exact_nominal(ty, DEFAULT_PATH);
        let names: &[&str] = if is_default {
            &["package"]
        } else {
            &["package", "loader", "libraries"]
        };
        if fields.len() != names.len()
            || !fields
                .iter()
                .zip(names)
                .all(|((name, _), expected)| name == expected)
        {
            return Err(invalid("unexpected application fields"));
        }
        let id =
            decode_package_id(&fields[0].1).ok_or_else(|| invalid("package is not PackageId"))?;
        let package = by_id
            .get(id)
            .ok_or_else(|| invalid("application refers to a missing package"))?;
        if package.export().is_none() || !build_ids.contains(id) {
            return Err(invalid(
                "application requires a public buildable package export",
            ));
        }
        if is_default {
            if default.is_some() {
                return Err(PlanError::DuplicateDefaultApplication);
            }
            if root.identity().path().len() != 1 {
                return Err(invalid("default application must be a project root"));
            }
            default = Some(PlanPackageId(budget.string(id)?));
            continue;
        }
        let loader = decode_role(
            &fields[1].1,
            LOADER_PATH,
            1,
            &by_id,
            &build_ids,
            budget,
            &invalid,
        )?;
        let mut libraries = decode_role(
            &fields[2].1,
            LIBRARY_PATH,
            MAX_RUNTIME_LIBRARIES,
            &by_id,
            &build_ids,
            budget,
            &invalid,
        )?;
        let loader = loader.into_iter().next();
        let mut distinct = BTreeSet::new();
        for library in &libraries {
            if !distinct.insert(library.as_str().to_owned()) {
                return Err(invalid("duplicate runtime provider"));
            }
        }
        if distinct.contains(id) {
            return Err(invalid(
                "application cannot supply its own runtime provider",
            ));
        }
        libraries.sort();
        apps.push(PlanApplication {
            package: PlanPackageId(budget.string(id)?),
            loader,
            libraries,
        });
    }
    apps.sort_by(|left, right| left.package.cmp(&right.package));
    for pair in apps.windows(2) {
        if pair[0].package == pair[1].package {
            return Err(PlanError::DuplicateApplication {
                package: pair[0].package.clone(),
            });
        }
    }
    if let Some(id) = &default
        && apps.binary_search_by(|app| app.package.cmp(id)).is_err()
    {
        return Err(PlanError::InvalidApplication {
            root: "default".into(),
            reason: "default package has no application description",
        });
    }
    Ok((apps, default))
}

fn decode_role(
    value: &Value,
    path: &[&str],
    limit: usize,
    packages: &BTreeMap<&str, &PlanPackage>,
    builds: &BTreeSet<&str>,
    budget: &mut ProjectionBudget,
    invalid: &impl Fn(&'static str) -> PlanError,
) -> Result<Vec<PlanPackageId>, PlanError> {
    let Value::List { ty, items } = value else {
        return Err(invalid("runtime role is not a list"));
    };
    if !matches!(ty.as_ref(), CanonicalType::List(item) if exact_nominal(item, path)) {
        return Err(invalid("runtime role has the wrong list type"));
    }
    if items.len() > limit {
        return Err(invalid("too many runtime providers"));
    }
    let mut result = budget.collection::<PlanPackageId>(items.len())?;
    for item in items {
        budget.node::<PlanPackageId>()?;
        let Value::Struct { ty, fields, .. } = item else {
            return Err(invalid("runtime provider is not a struct"));
        };
        if !exact_nominal(ty, path) || fields.len() != 1 || fields[0].0 != "package" {
            return Err(invalid("runtime provider fields are invalid"));
        }
        let id = decode_package_id(&fields[0].1)
            .ok_or_else(|| invalid("runtime provider package is not PackageId"))?;
        if !packages.contains_key(id) || !builds.contains(id) {
            return Err(invalid("runtime provider has no buildable package"));
        }
        result.push(PlanPackageId(budget.string(id)?));
    }
    Ok(result)
}
