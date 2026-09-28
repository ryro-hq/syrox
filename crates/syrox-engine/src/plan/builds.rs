use super::{
    AuthenticatedStandardLibrary, BTreeMap, PlanAcquisition, PlanError, PlanPackage, PlanPackageId,
    PrimitiveValue, ProjectionBudget, Value, decode_nominal_string, decode_package_id,
    exact_nominal,
};

const BUILD_PATH: &[&str] = &["std", "pkg", "AutotoolsBuild"];
const GLIBC_PATH: &[&str] = &["std", "pkg", "GlibcBuild"];
const DEFAULT_PATH: &[&str] = &["std", "pkg", "DefaultBuild"];

/// Recipe-owned parameters for the first bounded protocol. No filesystem or
/// toolchain authority is carried by this pure description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanBuild {
    package: PlanPackageId,
    source_directory: String,
    entry: String,
    timeout_seconds: u32,
    protocol: crate::BuildProtocol,
    development: Option<PlanPackageId>,
}

impl PlanBuild {
    pub(crate) const fn is_glibc(&self) -> bool {
        matches!(self.protocol, crate::BuildProtocol::Glibc)
    }
    pub const fn package(&self) -> &PlanPackageId {
        &self.package
    }
    pub const fn protocol(&self) -> &'static str {
        match self.protocol {
            crate::BuildProtocol::Autotools => "autotools",
            crate::BuildProtocol::Glibc => "glibc",
        }
    }
    pub fn source_directory(&self) -> &str {
        &self.source_directory
    }
    pub fn entry(&self) -> &str {
        &self.entry
    }
    pub const fn timeout_seconds(&self) -> u32 {
        self.timeout_seconds
    }
    pub const fn development(&self) -> Option<&PlanPackageId> {
        self.development.as_ref()
    }

    pub(crate) fn request(&self) -> crate::BuildSpecification {
        crate::BuildSpecification {
            package: self.package.as_str().to_owned(),
            source_directory: self.source_directory.clone(),
            entry: self.entry.clone(),
            timeout_seconds: self.timeout_seconds,
            mode: if self.is_glibc() {
                crate::BuildMode::Glibc
            } else {
                crate::BuildMode::Autotools { provider: None }
            },
        }
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn extract(
    components: &[super::recipes::Component<'_>],
    packages: &[PlanPackage],
    acquisitions: &[PlanAcquisition],
    standard_library: Option<&AuthenticatedStandardLibrary>,
    budget: &mut ProjectionBudget,
) -> Result<(Vec<PlanBuild>, Option<PlanPackageId>), PlanError> {
    if standard_library.is_none() {
        return Ok((Vec::new(), None));
    }
    let mut roots: Vec<_> = components
        .iter()
        .filter(|root| {
            root.identity().domain() == syrox_lang::SourceDomainId::project()
                && (exact_nominal(root.ty(), BUILD_PATH)
                    || exact_nominal(root.ty(), GLIBC_PATH)
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
    let mut sources = BTreeMap::new();
    for acquisition in acquisitions {
        budget.node::<&PlanAcquisition>()?;
        sources.insert(acquisition.package().as_str(), acquisition.sources().len());
    }
    let mut builds = budget.collection::<PlanBuild>(roots.len())?;
    let mut default = None;
    for root in roots {
        budget.node::<PlanBuild>()?;
        let name = budget.string(root.name())?;
        let invalid = |reason| PlanError::InvalidBuild {
            root: name.clone(),
            reason,
        };
        let Value::Struct { ty, fields, .. } = root.value() else {
            return Err(invalid("expected an exact std build struct"));
        };
        let is_default = exact_nominal(ty, DEFAULT_PATH);
        let glibc = exact_nominal(ty, GLIBC_PATH);
        let names: &[&str] = if is_default {
            &["package"]
        } else {
            &["package", "source_directory", "entry", "timeout_seconds"]
        };
        if fields.len() != names.len()
            || !fields
                .iter()
                .zip(names)
                .all(|((name, _), expected)| name == expected)
        {
            return Err(invalid("unexpected build fields"));
        }
        let id =
            decode_package_id(&fields[0].1).ok_or_else(|| invalid("package is not PackageId"))?;
        let package = by_id
            .get(id)
            .ok_or_else(|| invalid("build refers to a missing package"))?;
        if is_default {
            if default.is_some() {
                return Err(PlanError::DuplicateDefaultBuild);
            }
            if root.identity().path().len() != 1 || package.export().is_none() {
                return Err(invalid(
                    "default build must select a root project package export",
                ));
            }
            default = Some(PlanPackageId(budget.string(id)?));
            continue;
        }
        if package.dependencies().len() != 0 || sources.get(id) != Some(&1) {
            return Err(invalid(
                "host build requires no dependencies and exactly one source",
            ));
        }
        let directory = decode_nominal_string(&fields[1].1, &["std", "pkg", "SourceDirectory"])
            .ok_or_else(|| invalid("source_directory is not SourceDirectory"))?;
        let entry = decode_nominal_string(&fields[2].1, &["std", "pkg", "InstalledEntry"])
            .ok_or_else(|| invalid("entry is not InstalledEntry"))?;
        let timeout = match &fields[3].1 {
            Value::Nominal {
                ty,
                value: PrimitiveValue::Int(value),
                resource: false,
            } if exact_nominal(ty, &["std", "pkg", "BuildTimeout"]) => u32::try_from(*value).ok(),
            _ => None,
        }
        .filter(|value| (1..=if glibc { 1800 } else { 300 }).contains(value))
        .ok_or_else(|| invalid("timeout_seconds exceeds the protocol deadline"))?;
        if !crate::build::relative_path(directory)
            || directory.contains('/')
            || !crate::build::relative_path(entry)
        {
            return Err(invalid(
                "invalid build request: expected canonical relative directory and entry",
            ));
        }
        if glibc && entry != "usr/lib/ld-linux-x86-64.so.2" {
            return Err(invalid("glibc requires the exact ELF loader entry"));
        }
        builds.push(PlanBuild {
            package: PlanPackageId(budget.string(id)?),
            source_directory: budget.string(directory)?,
            entry: budget.string(entry)?,
            timeout_seconds: timeout,
            protocol: if glibc {
                crate::BuildProtocol::Glibc
            } else {
                crate::BuildProtocol::Autotools
            },
            development: None,
        });
    }
    builds.sort_by(|left, right| left.package.cmp(&right.package));
    for pair in builds.windows(2) {
        if pair[0].package == pair[1].package {
            return Err(PlanError::DuplicateBuild {
                package: pair[0].package.clone(),
            });
        }
    }
    extract_build_inputs(components, &by_id, &mut builds, budget)?;
    if let Some(package) = &default
        && builds
            .binary_search_by(|build| build.package.cmp(package))
            .is_err()
    {
        return Err(PlanError::InvalidBuild {
            root: "default".into(),
            reason: "default package has no build description",
        });
    }
    Ok((builds, default))
}

const INPUTS_PATH: &[&str] = &["std", "pkg", "BuildInputs"];
const OUTPUT_PATH: &[&str] = &["std", "pkg", "BuildOutput"];

fn extract_build_inputs(
    components: &[super::recipes::Component<'_>],
    packages: &BTreeMap<&str, &PlanPackage>,
    builds: &mut [PlanBuild],
    budget: &mut ProjectionBudget,
) -> Result<(), PlanError> {
    use super::CanonicalType;
    let mut seen = std::collections::BTreeSet::new();
    for root in components.iter().filter(|root| {
        root.identity().domain() == syrox_lang::SourceDomainId::project()
            && exact_nominal(root.ty(), INPUTS_PATH)
    }) {
        budget.node::<PlanBuild>()?;
        let name = budget.string(root.name())?;
        let invalid = |reason| PlanError::InvalidBuild {
            root: name.clone(),
            reason,
        };
        let Value::Struct { ty, fields, .. } = root.value() else {
            return Err(invalid("expected exact std build inputs"));
        };
        if !exact_nominal(ty, INPUTS_PATH)
            || fields.len() != 2
            || fields[0].0 != "package"
            || fields[1].0 != "selected"
        {
            return Err(invalid("invalid build input fields"));
        }
        let owner = decode_package_id(&fields[0].1).ok_or_else(|| invalid("invalid package"))?;
        let index = builds
            .binary_search_by(|build| build.package.as_str().cmp(owner))
            .map_err(|_| invalid("build inputs require an existing build"))?;
        if !seen.insert(owner.to_owned()) || builds[index].is_glibc() {
            return Err(invalid("duplicate or unsupported build inputs"));
        }
        let Value::List { ty, items } = &fields[1].1 else {
            return Err(invalid("outputs must be a list"));
        };
        if !matches!(ty.as_ref(), CanonicalType::List(item) if exact_nominal(item, OUTPUT_PATH))
            || items.len() != 1
        {
            return Err(invalid(
                "this build requires exactly one development output",
            ));
        }
        let Value::Struct { ty, fields, .. } = &items[0] else {
            return Err(invalid("invalid development output"));
        };
        if !exact_nominal(ty, OUTPUT_PATH)
            || fields.len() != 2
            || fields[0].0 != "package"
            || fields[1].0 != "output"
        {
            return Err(invalid("invalid development output fields"));
        }
        let provider =
            decode_package_id(&fields[0].1).ok_or_else(|| invalid("invalid provider"))?;
        let output = decode_nominal_string(&fields[1].1, &["std", "pkg", "OutputName"])
            .ok_or_else(|| invalid("invalid output name"))?;
        if output != "dev"
            || provider == owner
            || !packages.contains_key(provider)
            || !builds
                .binary_search_by(|build| build.package.as_str().cmp(provider))
                .is_ok_and(|index| builds[index].is_glibc())
        {
            return Err(invalid(
                "development input requires another glibc build's dev output",
            ));
        }
        builds[index].development = Some(PlanPackageId(budget.string(provider)?));
    }
    Ok(())
}
