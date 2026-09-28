use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::mem::size_of;
use std::path::{Path, PathBuf};

use syrox_lang::{
    CanonicalItemIdentity, CanonicalType, PrimitiveValue, RealizedProgram, RealizedRootOutcome,
    ResourceClaim, Value,
};

use crate::AuthenticatedStandardLibrary;
use crate::store::{ContentDigest, MAX_STORE_BLOB_BYTES};

mod builds;
pub use builds::PlanBuild;
mod apps;
pub use apps::PlanApplication;
mod display;
use display::canonical_display_bytes;

pub const MAX_PLAN_PACKAGE_NODES: usize = 65_536;
pub const MAX_PLAN_PACKAGE_EDGES: usize = 262_144;
pub const MAX_PLAN_PACKAGE_WORK: usize = 4_000_000;
pub const MAX_PACKAGE_ID_BYTES: usize = 255;
pub const MAX_PLAN_SOURCE_REQUESTS: usize = 262_144;
const ACQUISITION_PATH: &[&str] = &["std", "pkg", "Acquisition"];
const SOURCE_REQUEST_PATH: &[&str] = &["std", "pkg", "SourceRequest"];
const SOURCE_URL_PATH: &[&str] = &["std", "pkg", "SourceUrl"];
const SOURCE_DIGEST_PATH: &[&str] = &["std", "pkg", "SourceDigest"];
const SOURCE_LIMIT_PATH: &[&str] = &["std", "pkg", "SourceLimit"];
/// Maximum nodes and strings visited while projecting a realized program.
pub const MAX_PLAN_PROJECTION_WORK: usize = 4_000_000;
/// Maximum accounted bytes retained by a projected plan.
pub const MAX_PLAN_RETAINED_BYTES: usize = 64 * 1024 * 1024;
/// Maximum bytes in the canonical `Plan` display encoding.
pub const MAX_PLAN_DISPLAY_BYTES: usize = 64 * 1024 * 1024;

const PACKAGE_PATH: &[&str] = &["std", "pkg", "Package"];
const PACKAGE_ID_PATH: &[&str] = &["std", "pkg", "PackageId"];
const DEPENDENCY_PATH: &[&str] = &["std", "pkg", "Dependency"];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlanPackageId(String);

impl PlanPackageId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PlanPackageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanPackage {
    id: PlanPackageId,
    export: Option<String>,
    dependencies: Vec<PlanPackageId>,
}

impl PlanPackage {
    pub const fn id(&self) -> &PlanPackageId {
        &self.id
    }

    pub fn dependencies(&self) -> impl ExactSizeIterator<Item = &PlanPackageId> {
        self.dependencies.iter()
    }

    /// Root project export selectable by the public reference syntax.
    pub fn export(&self) -> Option<&str> {
        self.export.as_deref()
    }
}

/// A pinned source requested by one explicit acquisition operation, not a graph edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanSourceRequest {
    url: String,
    digest: ContentDigest,
    maximum_bytes: u64,
    owner: Option<syrox_lang::SourceDomainId>,
}

impl PlanSourceRequest {
    pub fn url(&self) -> &str {
        &self.url
    }
    pub const fn digest(&self) -> ContentDigest {
        self.digest
    }
    pub const fn maximum_bytes(&self) -> u64 {
        self.maximum_bytes
    }
    pub const fn owner(&self) -> Option<syrox_lang::SourceDomainId> {
        self.owner
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanAcquisition {
    package: PlanPackageId,
    sources: Vec<PlanSourceRequest>,
}

impl PlanAcquisition {
    pub const fn package(&self) -> &PlanPackageId {
        &self.package
    }
    pub fn sources(&self) -> impl ExactSizeIterator<Item = &PlanSourceRequest> {
        self.sources.iter()
    }
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("function values cannot be serialized into a plan")]
    FunctionValue,
    #[error("build root `{root}` is invalid: {reason}")]
    InvalidBuild { root: String, reason: &'static str },
    #[error("duplicate build for package `{package}`")]
    DuplicateBuild { package: PlanPackageId },
    #[error("project must declare at most one std::DefaultBuild")]
    DuplicateDefaultBuild,
    #[error("application root `{root}` is invalid: {reason}")]
    InvalidApplication { root: String, reason: &'static str },
    #[error("duplicate application for package `{package}`")]
    DuplicateApplication { package: PlanPackageId },
    #[error("project must declare at most one std::DefaultApplication")]
    DuplicateDefaultApplication,
    #[error("acquisition root `{root}` has malformed exact std::pkg::Acquisition value: {reason}")]
    MalformedAcquisition { root: String, reason: &'static str },
    #[error("acquisition for missing package `{package}`")]
    MissingAcquisitionPackage { package: PlanPackageId },
    #[error("duplicate acquisition for package `{package}`")]
    DuplicateAcquisition { package: PlanPackageId },
    #[error("acquisition for `{package}` has invalid source request: {reason}")]
    InvalidSourceRequest {
        package: PlanPackageId,
        reason: &'static str,
    },
    #[error("acquisition source count exceeds the {MAX_PLAN_SOURCE_REQUESTS}-request limit")]
    SourceRequestLimit,
    #[error("realized root `{root}` did not produce a value")]
    UnrealizedRoot { root: String },
    #[error("realized root `{root}` has no canonical type")]
    MissingRootType { root: String },
    #[error("package root `{root}` has malformed exact std::pkg::Package value: {reason}")]
    MalformedPackage { root: String, reason: &'static str },
    #[error("invalid package ID `{id}`; expected ASCII [a-z0-9][a-z0-9+._-]{{0,254}}")]
    InvalidPackageId { id: String },
    #[error("duplicate package ID `{id}`")]
    DuplicatePackageId { id: PlanPackageId },
    #[error("package `{package}` repeats dependency `{dependency}`")]
    DuplicateDependency {
        package: PlanPackageId,
        dependency: PlanPackageId,
    },
    #[error("package `{package}` depends on missing package `{dependency}`")]
    MissingDependency {
        package: PlanPackageId,
        dependency: PlanPackageId,
    },
    #[error("package dependency cycle: {}", display_cycle(.packages))]
    Cycle { packages: Vec<PlanPackageId> },
    #[error("package dependency graph exceeds the {MAX_PLAN_PACKAGE_NODES}-node limit")]
    NodeLimit,
    #[error("package dependency graph exceeds the {MAX_PLAN_PACKAGE_EDGES}-edge limit")]
    EdgeLimit,
    #[error("package dependency graph exceeds the {MAX_PLAN_PACKAGE_WORK}-work-unit limit")]
    WorkLimit,
    #[error("plan projection exceeds its {limit}-work-unit limit")]
    ProjectionWorkLimit { limit: usize },
    #[error("plan projection exceeds its {limit}-retained-byte limit")]
    ProjectionRetainedBytesLimit { limit: usize },
    #[error("canonical plan display exceeds its {limit}-byte limit")]
    DisplayBytesLimit { limit: usize },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanStandardLibrary {
    digest: [u8; 32],
}

impl PlanStandardLibrary {
    pub const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlanType {
    Unit,
    Int,
    Str,
    Nominal {
        domain: u32,
        path: Vec<String>,
    },
    Specialization {
        domain: u32,
        path: Vec<String>,
        arguments: Vec<PlanType>,
    },
    List(Box<PlanType>),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlanValue {
    Unit,
    Int(i64),
    Str(String),
    Nominal {
        ty: PlanType,
        value: Box<PlanValue>,
        resource: bool,
    },
    List {
        ty: PlanType,
        items: Vec<PlanValue>,
    },
    Struct {
        ty: PlanType,
        fields: Vec<(String, PlanValue)>,
    },
    Variant {
        ty: PlanType,
        index: u32,
        payload: Vec<PlanValue>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlanClaim {
    ty: PlanType,
    value: PlanValue,
    scope_root_domain: u32,
    scope_root_path: Vec<String>,
    scope_name: Option<String>,
    boundary: u64,
}

impl PlanClaim {
    pub const fn ty(&self) -> &PlanType {
        &self.ty
    }

    pub const fn value(&self) -> &PlanValue {
        &self.value
    }

    pub const fn scope_root_domain(&self) -> u32 {
        self.scope_root_domain
    }

    pub fn scope_root_path(&self) -> &[String] {
        &self.scope_root_path
    }

    pub fn scope_name(&self) -> Option<&str> {
        self.scope_name.as_deref()
    }

    pub const fn boundary(&self) -> u64 {
        self.boundary
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanRoot {
    name: String,
    domain: u32,
    path: Vec<String>,
    ty: PlanType,
    value: PlanValue,
    claims: Vec<PlanClaim>,
}

impl PlanRoot {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn domain(&self) -> u32 {
        self.domain
    }

    pub fn path(&self) -> &[String] {
        &self.path
    }

    pub const fn ty(&self) -> &PlanType {
        &self.ty
    }

    pub const fn value(&self) -> &PlanValue {
        &self.value
    }

    pub fn claims(&self) -> impl ExactSizeIterator<Item = &PlanClaim> {
        self.claims.iter()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    lock_digest: [u8; 32],
    policy_identity: String,
    standard_library: Option<PlanStandardLibrary>,
    packages: Vec<PlanPackage>,
    acquisitions: Vec<PlanAcquisition>,
    builds: Vec<PlanBuild>,
    default_build: Option<PlanPackageId>,
    applications: Vec<PlanApplication>,
    default_application: Option<PlanPackageId>,
    package_edges: usize,
    roots: Vec<PlanRoot>,
    canonical_display_bytes: usize,
    project_roots: BTreeMap<syrox_lang::SourceDomainId, PathBuf>,
}

impl Plan {
    pub(crate) fn bind_project_roots(
        &mut self,
        roots: BTreeMap<syrox_lang::SourceDomainId, PathBuf>,
    ) {
        self.project_roots = roots;
    }

    pub(crate) fn project_root(&self, owner: syrox_lang::SourceDomainId) -> Option<&Path> {
        self.project_roots.get(&owner).map(PathBuf::as_path)
    }
    pub const fn lock_digest(&self) -> &[u8; 32] {
        &self.lock_digest
    }

    pub fn policy_identity(&self) -> &str {
        &self.policy_identity
    }

    pub const fn standard_library(&self) -> Option<&PlanStandardLibrary> {
        self.standard_library.as_ref()
    }

    /// Packages in dependency-first topological order, with lexical ties.
    pub fn packages(&self) -> impl ExactSizeIterator<Item = &PlanPackage> {
        self.packages.iter()
    }

    pub fn acquisitions(&self) -> impl ExactSizeIterator<Item = &PlanAcquisition> {
        self.acquisitions.iter()
    }

    pub fn builds(&self) -> impl ExactSizeIterator<Item = &PlanBuild> {
        self.builds.iter()
    }

    pub const fn default_build(&self) -> Option<&PlanPackageId> {
        self.default_build.as_ref()
    }

    pub fn applications(&self) -> impl ExactSizeIterator<Item = &PlanApplication> {
        self.applications.iter()
    }

    pub const fn default_application(&self) -> Option<&PlanPackageId> {
        self.default_application.as_ref()
    }

    pub fn roots(&self) -> impl ExactSizeIterator<Item = &PlanRoot> {
        self.roots.iter()
    }

    pub(crate) fn from_realized(
        realized: &RealizedProgram,
        lock_digest: [u8; 32],
        policy_identity: &str,
        standard_library: Option<&AuthenticatedStandardLibrary>,
    ) -> Result<Self, PlanError> {
        let mut budget = ProjectionBudget::new(MAX_PLAN_PROJECTION_WORK, MAX_PLAN_RETAINED_BYTES);
        budget.node::<Plan>()?;
        let policy_identity = budget.string(policy_identity)?;
        let plan_standard_library = standard_library
            .map(|library| {
                budget.node::<PlanStandardLibrary>()?;
                Ok(PlanStandardLibrary {
                    digest: *library.digest(),
                })
            })
            .transpose()?;
        let root_count = realized.roots().len();
        let mut roots = budget.collection::<PlanRoot>(root_count)?;
        for root in realized.roots() {
            budget.node::<PlanRoot>()?;
            let RealizedRootOutcome::Value(value) = root.outcome() else {
                return Err(PlanError::UnrealizedRoot {
                    root: budget.string(root.name())?,
                });
            };
            let Some(ty) = root.ty() else {
                return Err(PlanError::MissingRootType {
                    root: budget.string(root.name())?,
                });
            };
            let identity = root.identity();
            let mut claims = budget.collection::<PlanClaim>(root.claims().len())?;
            for claim in root.claims() {
                claims.push(plan_claim(claim, &mut budget)?);
            }
            claims.sort();
            roots.push(PlanRoot {
                name: budget.string(root.name())?,
                domain: identity.domain().as_u32(),
                path: plan_path(identity.path(), &mut budget)?,
                ty: plan_type(ty, &mut budget)?,
                value: plan_value(value, &mut budget)?,
                claims,
            });
        }
        roots.sort_by(|left, right| (&left.domain, &left.path).cmp(&(&right.domain, &right.path)));
        let (packages, package_edges) = extract_packages(realized, standard_library, &mut budget)?;
        let acquisitions =
            extract_acquisitions(realized, &packages, standard_library, &mut budget)?;
        let (builds, default_build) = builds::extract(
            realized,
            &packages,
            &acquisitions,
            standard_library,
            &mut budget,
        )?;
        let (applications, default_application) =
            apps::extract(realized, &packages, &builds, standard_library, &mut budget)?;
        for build in &builds {
            if let Some(development) = build.development()
                && !applications.iter().any(|app| {
                    app.package() == build.package()
                        && app.loader() == Some(development)
                        && app.libraries().any(|library| library == development)
                })
            {
                return Err(PlanError::InvalidBuild {
                    root: build.package().as_str().to_owned(),
                    reason: "development input must match the declared loader and library provider",
                });
            }
        }
        let mut plan = Self {
            lock_digest,
            policy_identity,
            standard_library: plan_standard_library,
            packages,
            acquisitions,
            builds,
            default_build,
            applications,
            default_application,
            package_edges,
            roots,
            canonical_display_bytes: 0,
            project_roots: BTreeMap::new(),
        };
        plan.canonical_display_bytes = canonical_display_bytes(&plan, MAX_PLAN_DISPLAY_BYTES)?;
        Ok(plan)
    }
}

#[derive(Debug)]
struct PackageCandidate<'a> {
    id: &'a str,
    export: Option<&'a str>,
    dependencies: Vec<&'a str>,
}

#[derive(Debug)]
struct Work(usize);

impl Work {
    fn charge(&mut self) -> Result<(), PlanError> {
        self.0 = self.0.checked_add(1).ok_or(PlanError::WorkLimit)?;
        if self.0 > MAX_PLAN_PACKAGE_WORK {
            return Err(PlanError::WorkLimit);
        }
        Ok(())
    }
}

#[derive(Debug)]
struct ProjectionBudget {
    work: usize,
    retained_bytes: usize,
    work_limit: usize,
    retained_bytes_limit: usize,
}

impl ProjectionBudget {
    const fn new(work_limit: usize, retained_bytes_limit: usize) -> Self {
        Self {
            work: 0,
            retained_bytes: 0,
            work_limit,
            retained_bytes_limit,
        }
    }

    fn charge_work(&mut self) -> Result<(), PlanError> {
        self.work = self
            .work
            .checked_add(1)
            .ok_or(PlanError::ProjectionWorkLimit {
                limit: self.work_limit,
            })?;
        if self.work > self.work_limit {
            return Err(PlanError::ProjectionWorkLimit {
                limit: self.work_limit,
            });
        }
        Ok(())
    }

    fn bytes(&mut self, bytes: usize) -> Result<(), PlanError> {
        self.retained_bytes = self.retained_bytes.checked_add(bytes).ok_or(
            PlanError::ProjectionRetainedBytesLimit {
                limit: self.retained_bytes_limit,
            },
        )?;
        if self.retained_bytes > self.retained_bytes_limit {
            return Err(PlanError::ProjectionRetainedBytesLimit {
                limit: self.retained_bytes_limit,
            });
        }
        Ok(())
    }

    fn node<T>(&mut self) -> Result<(), PlanError> {
        self.charge_work()?;
        self.bytes(size_of::<T>())
    }

    fn collection<T>(&mut self, len: usize) -> Result<Vec<T>, PlanError> {
        let bytes =
            len.checked_mul(size_of::<T>())
                .ok_or(PlanError::ProjectionRetainedBytesLimit {
                    limit: self.retained_bytes_limit,
                })?;
        self.bytes(bytes)?;
        Ok(Vec::with_capacity(len))
    }

    fn string(&mut self, value: &str) -> Result<String, PlanError> {
        self.charge_work()?;
        self.bytes(size_of::<String>())?;
        self.bytes(value.len())?;
        Ok(value.to_owned())
    }
}

#[allow(clippy::too_many_lines)]
fn extract_packages(
    realized: &RealizedProgram,
    standard_library: Option<&AuthenticatedStandardLibrary>,
    budget: &mut ProjectionBudget,
) -> Result<(Vec<PlanPackage>, usize), PlanError> {
    if standard_library.is_none() {
        return Ok((Vec::new(), 0));
    }

    let mut exact_roots: Vec<_> = realized
        .roots()
        .filter(|root| {
            root.identity().domain() == syrox_lang::SourceDomainId::project()
                && root.ty().is_some_and(|ty| exact_nominal(ty, PACKAGE_PATH))
        })
        .collect();
    exact_roots.sort_by(|left, right| {
        (left.identity(), left.name()).cmp(&(right.identity(), right.name()))
    });
    if exact_roots.len() > MAX_PLAN_PACKAGE_NODES {
        return Err(PlanError::NodeLimit);
    }

    let mut work = Work(0);
    let mut edge_count = 0_usize;
    let mut candidates = Vec::with_capacity(exact_roots.len());
    for root in exact_roots {
        work.charge()?;
        let Some(value) = root.value() else {
            return Err(PlanError::UnrealizedRoot {
                root: budget.string(root.name())?,
            });
        };
        let mut candidate = decode_package(root.name(), value, &mut edge_count, &mut work, budget)?;
        if root.identity().path().len() == 1 {
            candidate.export = Some(root.name());
        }
        candidates.push(candidate);
    }
    candidates.sort_by(|left, right| {
        (&left.id, &left.dependencies).cmp(&(&right.id, &right.dependencies))
    });

    let mut invalid = BTreeSet::new();
    for candidate in &candidates {
        work.charge()?;
        if !valid_package_id(candidate.id) {
            invalid.insert(candidate.id);
        }
        for dependency in &candidate.dependencies {
            work.charge()?;
            if !valid_package_id(dependency) {
                invalid.insert(*dependency);
            }
        }
    }
    if let Some(id) = invalid.into_iter().next() {
        return Err(PlanError::InvalidPackageId {
            id: budget.string(id)?,
        });
    }

    for pair in candidates.windows(2) {
        work.charge()?;
        if pair[0].id == pair[1].id {
            return Err(PlanError::DuplicatePackageId {
                id: PlanPackageId(budget.string(pair[0].id)?),
            });
        }
    }

    let mut duplicate_edges = BTreeSet::new();
    for candidate in &mut candidates {
        candidate.dependencies.sort_unstable();
        for pair in candidate.dependencies.windows(2) {
            work.charge()?;
            if pair[0] == pair[1] {
                duplicate_edges.insert((candidate.id, pair[0]));
            }
        }
    }
    if let Some((package, dependency)) = duplicate_edges.into_iter().next() {
        return Err(PlanError::DuplicateDependency {
            package: PlanPackageId(budget.string(package)?),
            dependency: PlanPackageId(budget.string(dependency)?),
        });
    }

    let mut nodes = BTreeMap::new();
    let mut exports = BTreeMap::new();
    for candidate in candidates {
        work.charge()?;
        exports.insert(candidate.id, candidate.export);
        nodes.insert(candidate.id, candidate.dependencies);
    }
    let mut missing = BTreeSet::new();
    for (package, dependencies) in &nodes {
        for dependency in dependencies {
            work.charge()?;
            if !nodes.contains_key(dependency) {
                missing.insert((*package, *dependency));
            }
        }
    }
    if let Some((package, dependency)) = missing.into_iter().next() {
        return Err(PlanError::MissingDependency {
            package: PlanPackageId(budget.string(package)?),
            dependency: PlanPackageId(budget.string(dependency)?),
        });
    }

    let mut indegree = BTreeMap::new();
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (package, dependencies) in &nodes {
        work.charge()?;
        indegree.insert(*package, dependencies.len());
        for dependency in dependencies {
            work.charge()?;
            dependents.entry(*dependency).or_default().push(*package);
        }
    }
    let mut ready = BTreeSet::new();
    for (&package, &count) in &indegree {
        work.charge()?;
        if count == 0 {
            ready.insert(package);
        }
    }
    let mut order = Vec::with_capacity(nodes.len());
    while let Some(package) = ready.pop_first() {
        work.charge()?;
        order.push(package);
        if let Some(items) = dependents.get(package) {
            for dependent in items {
                work.charge()?;
                let count = indegree
                    .get_mut(dependent)
                    .expect("validated package dependency has an indegree");
                *count -= 1;
                if *count == 0 {
                    ready.insert(dependent);
                }
            }
        }
    }
    if order.len() != nodes.len() {
        let mut unresolved = BTreeSet::new();
        for (&package, &count) in &indegree {
            work.charge()?;
            if count > 0 {
                unresolved.insert(package);
            }
        }
        let packages = cycle_witness(&nodes, &unresolved, &mut work, budget)?;
        return Err(PlanError::Cycle { packages });
    }

    let mut packages = budget.collection::<PlanPackage>(order.len())?;
    for id in order {
        budget.node::<PlanPackage>()?;
        let mut dependencies = budget.collection::<PlanPackageId>(nodes[id].len())?;
        for dependency in &nodes[id] {
            budget.node::<PlanPackageId>()?;
            dependencies.push(PlanPackageId(budget.string(dependency)?));
        }
        budget.node::<PlanPackageId>()?;
        packages.push(PlanPackage {
            id: PlanPackageId(budget.string(id)?),
            export: exports[id].map(|name| budget.string(name)).transpose()?,
            dependencies,
        });
    }
    Ok((packages, edge_count))
}

#[allow(clippy::too_many_lines)]
fn extract_acquisitions(
    realized: &RealizedProgram,
    packages: &[PlanPackage],
    standard_library: Option<&AuthenticatedStandardLibrary>,
    budget: &mut ProjectionBudget,
) -> Result<Vec<PlanAcquisition>, PlanError> {
    if standard_library.is_none() {
        return Ok(Vec::new());
    }
    let mut roots: Vec<_> = realized
        .roots()
        .filter(|root| {
            root.identity().domain() == syrox_lang::SourceDomainId::project()
                && root
                    .ty()
                    .is_some_and(|ty| exact_nominal(ty, ACQUISITION_PATH))
        })
        .collect();
    roots.sort_by(|left, right| {
        (left.identity(), left.name()).cmp(&(right.identity(), right.name()))
    });
    if roots.len() > MAX_PLAN_PACKAGE_NODES {
        return Err(PlanError::NodeLimit);
    }
    let mut package_ids = BTreeSet::new();
    for package in packages {
        budget.node::<PlanPackageId>()?;
        package_ids.insert(package.id.as_str());
    }
    let mut acquisitions = budget.collection::<PlanAcquisition>(roots.len())?;
    let mut source_count = 0_usize;
    for root in roots {
        budget.node::<PlanAcquisition>()?;
        let root_name = budget.string(root.name())?;
        let malformed = |reason| PlanError::MalformedAcquisition {
            root: root_name.clone(),
            reason,
        };
        let Some(Value::Struct { ty, fields, .. }) = root.value() else {
            return Err(malformed("value is not a struct"));
        };
        if !exact_nominal(ty, ACQUISITION_PATH)
            || fields.len() != 2
            || fields[0].0 != "package"
            || fields[1].0 != "sources"
        {
            return Err(malformed("fields are not exactly `package`, `sources`"));
        }
        let package = decode_package_id(&fields[0].1)
            .ok_or_else(|| malformed("`package` is not PackageId"))?;
        if !valid_package_id(package) {
            return Err(PlanError::InvalidPackageId {
                id: budget.string(package)?,
            });
        }
        if !package_ids.contains(package) {
            return Err(PlanError::MissingAcquisitionPackage {
                package: PlanPackageId(budget.string(package)?),
            });
        }
        let package_id = PlanPackageId(budget.string(package)?);
        let Value::List { ty, items } = &fields[1].1 else {
            return Err(malformed("`sources` is not a list"));
        };
        if !matches!(ty.as_ref(), CanonicalType::List(item) if exact_nominal(item, SOURCE_REQUEST_PATH))
        {
            return Err(malformed("`sources` has the wrong list type"));
        }
        source_count = source_count
            .checked_add(items.len())
            .ok_or(PlanError::SourceRequestLimit)?;
        if source_count > MAX_PLAN_SOURCE_REQUESTS {
            return Err(PlanError::SourceRequestLimit);
        }
        let mut sources = budget.collection::<PlanSourceRequest>(items.len())?;
        for item in items {
            budget.node::<PlanSourceRequest>()?;
            let invalid = |reason| PlanError::InvalidSourceRequest {
                package: package_id.clone(),
                reason,
            };
            let Value::Struct { ty, fields, owner } = item else {
                return Err(invalid("value is not a SourceRequest struct"));
            };
            if !exact_nominal(ty, SOURCE_REQUEST_PATH)
                || fields.len() != 3
                || fields[0].0 != "url"
                || fields[1].0 != "sha256"
                || fields[2].0 != "maximum_bytes"
            {
                return Err(invalid(
                    "fields are not exactly `url`, `sha256`, `maximum_bytes`",
                ));
            }
            let url = decode_nominal_string(&fields[0].1, SOURCE_URL_PATH)
                .ok_or_else(|| invalid("`url` is not SourceUrl"))?;
            if !valid_source_url(url) {
                return Err(invalid("unsupported or malformed source URL"));
            }
            let digest_text = decode_nominal_string(&fields[1].1, SOURCE_DIGEST_PATH)
                .ok_or_else(|| invalid("`sha256` is not SourceDigest"))?;
            let digest = digest_text
                .parse()
                .map_err(|_| invalid("invalid SHA-256 digest"))?;
            let limit = match &fields[2].1 {
                Value::Nominal {
                    ty,
                    value: PrimitiveValue::Int(value),
                    resource: false,
                } if exact_nominal(ty, SOURCE_LIMIT_PATH) => {
                    u64::try_from(*value).map_err(|_| invalid("negative source byte limit"))?
                }
                _ => return Err(invalid("`maximum_bytes` is not SourceLimit")),
            };
            if limit > MAX_STORE_BLOB_BYTES {
                return Err(invalid("source byte limit exceeds the Store maximum"));
            }
            sources.push(PlanSourceRequest {
                url: budget.string(url)?,
                digest,
                maximum_bytes: limit,
                owner: *owner,
            });
        }
        acquisitions.push(PlanAcquisition {
            package: package_id,
            sources,
        });
    }
    acquisitions.sort_by(|left, right| left.package.cmp(&right.package));
    for pair in acquisitions.windows(2) {
        if pair[0].package == pair[1].package {
            return Err(PlanError::DuplicateAcquisition {
                package: pair[0].package.clone(),
            });
        }
    }
    Ok(acquisitions)
}

fn decode_nominal_string<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    match value {
        Value::Nominal {
            ty,
            value: PrimitiveValue::Str(value),
            resource: false,
        } if exact_nominal(ty, path) => Some(value),
        _ => None,
    }
}

fn valid_source_url(value: &str) -> bool {
    if let Some(path) = value.strip_prefix("project:") {
        return crate::build::relative_path(path);
    }
    if value.starts_with("file:///") {
        return crate::local_source::parse_file_url(value).is_ok();
    }
    crate::https_source::validate_https_url(value).is_ok()
}

fn decode_package<'a>(
    root: &str,
    value: &'a Value,
    edge_count: &mut usize,
    work: &mut Work,
    budget: &mut ProjectionBudget,
) -> Result<PackageCandidate<'a>, PlanError> {
    let Value::Struct { ty, fields, .. } = value else {
        return Err(malformed_package(root, "value is not a struct", budget)?);
    };
    if !exact_nominal(ty, PACKAGE_PATH) {
        return Err(malformed_package(
            root,
            "struct has the wrong canonical type",
            budget,
        )?);
    }
    if fields.len() != 2 || fields[0].0 != "id" || fields[1].0 != "dependencies" {
        return Err(malformed_package(
            root,
            "fields are not exactly `id`, `dependencies`",
            budget,
        )?);
    }
    let Some(id) = decode_package_id(&fields[0].1) else {
        return Err(malformed_package(root, "`id` is not PackageId", budget)?);
    };
    let Value::List {
        ty: dependencies_ty,
        items,
    } = &fields[1].1
    else {
        return Err(malformed_package(
            root,
            "`dependencies` is not a list",
            budget,
        )?);
    };
    if !matches!(dependencies_ty.as_ref(), CanonicalType::List(item) if exact_nominal(item, DEPENDENCY_PATH))
    {
        return Err(malformed_package(
            root,
            "`dependencies` has the wrong list type",
            budget,
        )?);
    }
    *edge_count = edge_count
        .checked_add(items.len())
        .ok_or(PlanError::EdgeLimit)?;
    if *edge_count > MAX_PLAN_PACKAGE_EDGES {
        return Err(PlanError::EdgeLimit);
    }
    let mut dependencies = Vec::with_capacity(items.len());
    for dependency in items {
        work.charge()?;
        let Value::Struct { ty, fields, .. } = dependency else {
            return Err(malformed_package(
                root,
                "dependency is not a Dependency struct",
                budget,
            )?);
        };
        if !exact_nominal(ty, DEPENDENCY_PATH) || fields.len() != 1 || fields[0].0 != "package" {
            return Err(malformed_package(
                root,
                "dependency is not exact std::pkg::Dependency",
                budget,
            )?);
        }
        let Some(id) = decode_package_id(&fields[0].1) else {
            return Err(malformed_package(
                root,
                "dependency `package` is not PackageId",
                budget,
            )?);
        };
        dependencies.push(id);
    }
    Ok(PackageCandidate {
        id,
        export: None,
        dependencies,
    })
}

fn malformed_package(
    root: &str,
    reason: &'static str,
    budget: &mut ProjectionBudget,
) -> Result<PlanError, PlanError> {
    Ok(PlanError::MalformedPackage {
        root: budget.string(root)?,
        reason,
    })
}

fn decode_package_id(value: &Value) -> Option<&str> {
    match value {
        Value::Nominal {
            ty,
            value: PrimitiveValue::Str(value),
            resource: false,
        } if exact_nominal(ty, PACKAGE_ID_PATH) => Some(value),
        _ => None,
    }
}

fn exact_nominal(ty: &CanonicalType, path: &[&str]) -> bool {
    matches!(ty, CanonicalType::Nominal(identity)
        if identity.domain() == syrox_lang::SourceDomainId::standard_library()
            && identity.path().iter().map(String::as_str).eq(path.iter().copied()))
}

pub(crate) fn valid_package_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_PACKAGE_ID_BYTES
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().skip(1).all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'+' | b'.' | b'_' | b'-')
        })
}

fn cycle_witness(
    nodes: &BTreeMap<&str, Vec<&str>>,
    unresolved: &BTreeSet<&str>,
    work: &mut Work,
    budget: &mut ProjectionBudget,
) -> Result<Vec<PlanPackageId>, PlanError> {
    let mut state: BTreeMap<&str, u8> = BTreeMap::new();
    for &start in unresolved {
        work.charge()?;
        if state.get(start).copied().unwrap_or(0) != 0 {
            continue;
        }
        state.insert(start, 1);
        let mut path = vec![start];
        let mut stack = vec![(start, 0_usize)];
        while let Some((node, next)) = stack.last_mut() {
            work.charge()?;
            let dependencies = &nodes[*node];
            while *next < dependencies.len() && !unresolved.contains(dependencies[*next]) {
                work.charge()?;
                *next += 1;
            }
            if *next == dependencies.len() {
                state.insert(node, 2);
                stack.pop();
                path.pop();
                continue;
            }
            let dependency = dependencies[*next];
            *next += 1;
            match state.get(dependency).copied().unwrap_or(0) {
                0 => {
                    state.insert(dependency, 1);
                    path.push(dependency);
                    stack.push((dependency, 0));
                }
                1 => {
                    let begin = path
                        .iter()
                        .position(|item| *item == dependency)
                        .expect("gray dependency is on the iterative DFS path");
                    let mut cycle: Vec<_> = path[begin..].to_vec();
                    let minimum = cycle
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, id)| *id)
                        .expect("a cycle is nonempty")
                        .0;
                    cycle.rotate_left(minimum);
                    let mut result = Vec::new();
                    for id in cycle {
                        budget.node::<PlanPackageId>()?;
                        result.push(PlanPackageId(budget.string(id)?));
                    }
                    return Ok(result);
                }
                _ => {}
            }
        }
    }
    unreachable!("unresolved Kahn nodes contain a cycle")
}

fn display_cycle(packages: &[PlanPackageId]) -> String {
    packages
        .iter()
        .map(PlanPackageId::as_str)
        .collect::<Vec<_>>()
        .join(" -> ")
}

fn plan_path(path: &[String], budget: &mut ProjectionBudget) -> Result<Vec<String>, PlanError> {
    let mut projected = budget.collection::<String>(path.len())?;
    for component in path {
        projected.push(budget.string(component)?);
    }
    Ok(projected)
}

fn plan_identity(
    identity: &CanonicalItemIdentity,
    budget: &mut ProjectionBudget,
) -> Result<(u32, Vec<String>), PlanError> {
    Ok((
        identity.domain().as_u32(),
        plan_path(identity.path(), budget)?,
    ))
}

fn plan_type(ty: &CanonicalType, budget: &mut ProjectionBudget) -> Result<PlanType, PlanError> {
    budget.node::<PlanType>()?;
    Ok(match ty {
        CanonicalType::Unit => PlanType::Unit,
        CanonicalType::Int => PlanType::Int,
        CanonicalType::Str => PlanType::Str,
        CanonicalType::Nominal(item) => {
            let (domain, path) = plan_identity(item, budget)?;
            PlanType::Nominal { domain, path }
        }
        CanonicalType::Specialization {
            template,
            arguments,
        } => {
            let (domain, path) = plan_identity(template, budget)?;
            let mut plan_arguments = budget.collection::<PlanType>(arguments.len())?;
            for argument in arguments {
                plan_arguments.push(plan_type(argument, budget)?);
            }
            PlanType::Specialization {
                domain,
                path,
                arguments: plan_arguments,
            }
        }
        CanonicalType::List(item) => PlanType::List(Box::new(plan_type(item, budget)?)),
        CanonicalType::Function { .. } => return Err(PlanError::FunctionValue),
    })
}

fn primitive(
    value: &PrimitiveValue,
    budget: &mut ProjectionBudget,
) -> Result<PlanValue, PlanError> {
    budget.node::<PlanValue>()?;
    Ok(match value {
        PrimitiveValue::Int(value) => PlanValue::Int(*value),
        PrimitiveValue::Str(value) => PlanValue::Str(budget.string(value)?),
    })
}

fn plan_value(value: &Value, budget: &mut ProjectionBudget) -> Result<PlanValue, PlanError> {
    budget.node::<PlanValue>()?;
    Ok(match value {
        Value::Function { .. } | Value::Closure { .. } | Value::VariantConstructor { .. } => {
            return Err(PlanError::FunctionValue);
        }
        Value::Unit => PlanValue::Unit,
        Value::Int(value) => PlanValue::Int(*value),
        Value::Str(value) => PlanValue::Str(budget.string(value)?),
        Value::Nominal {
            ty,
            value,
            resource,
        } => PlanValue::Nominal {
            ty: plan_type(ty, budget)?,
            value: Box::new(primitive(value, budget)?),
            resource: *resource,
        },
        Value::List { ty, items } => {
            let mut plan_items = budget.collection::<PlanValue>(items.len())?;
            for item in items {
                plan_items.push(plan_value(item, budget)?);
            }
            PlanValue::List {
                ty: plan_type(ty, budget)?,
                items: plan_items,
            }
        }
        Value::Struct { ty, fields, .. } => {
            let mut plan_fields = budget.collection::<(String, PlanValue)>(fields.len())?;
            for (name, value) in fields {
                plan_fields.push((budget.string(name)?, plan_value(value, budget)?));
            }
            PlanValue::Struct {
                ty: plan_type(ty, budget)?,
                fields: plan_fields,
            }
        }
        Value::Variant { ty, index, payload } => {
            let mut fields = budget.collection::<PlanValue>(payload.len())?;
            for field in payload {
                fields.push(plan_value(field, budget)?);
            }
            PlanValue::Variant {
                ty: plan_type(ty, budget)?,
                index: *index,
                payload: fields,
            }
        }
    })
}

fn plan_claim(
    claim: &ResourceClaim,
    budget: &mut ProjectionBudget,
) -> Result<PlanClaim, PlanError> {
    budget.node::<PlanClaim>()?;
    let (scope_root_domain, scope_root_path) = plan_identity(&claim.key.scope.root, budget)?;
    Ok(PlanClaim {
        ty: plan_type(&claim.key.ty, budget)?,
        value: primitive(&claim.key.value, budget)?,
        scope_root_domain,
        scope_root_path,
        scope_name: claim
            .key
            .scope
            .name
            .as_deref()
            .map(|name| budget.string(name))
            .transpose()?,
        boundary: claim.key.scope.boundary,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn nominal(path: &[&str]) -> Arc<CanonicalType> {
        Arc::new(CanonicalType::Nominal(Arc::new(
            CanonicalItemIdentity::new(
                syrox_lang::SourceDomainId::standard_library(),
                path.iter().copied(),
            )
            .unwrap(),
        )))
    }

    #[test]
    fn malformed_exact_package_value_is_not_ignored() {
        let value = Value::Struct {
            owner: None,
            ty: nominal(PACKAGE_PATH),
            fields: vec![("id".to_owned(), Value::Str("not nominal".to_owned()))],
        };
        let mut edges = 0;
        let error = decode_package(
            "root",
            &value,
            &mut edges,
            &mut Work(0),
            &mut ProjectionBudget::new(usize::MAX, usize::MAX),
        )
        .unwrap_err();
        assert_eq!(
            error,
            PlanError::MalformedPackage {
                root: "root".to_owned(),
                reason: "fields are not exactly `id`, `dependencies`",
            }
        );
    }

    #[test]
    fn package_id_grammar_has_exact_ascii_and_length_boundaries() {
        assert!(valid_package_id("0"));
        assert!(valid_package_id("a+._-09"));
        assert!(valid_package_id(&"a".repeat(MAX_PACKAGE_ID_BYTES)));
        for invalid in ["", "A", "a/", "a:", "é"] {
            assert!(!valid_package_id(invalid), "{invalid}");
        }
        assert!(!valid_package_id(&"a".repeat(MAX_PACKAGE_ID_BYTES + 1)));
    }

    #[test]
    fn package_work_limit_is_checked_before_more_work() {
        let mut at_limit = Work(MAX_PLAN_PACKAGE_WORK);
        assert_eq!(at_limit.charge(), Err(PlanError::WorkLimit));
        let mut below_limit = Work(MAX_PLAN_PACKAGE_WORK - 1);
        assert_eq!(below_limit.charge(), Ok(()));
    }

    #[test]
    fn projection_work_has_an_exact_boundary() {
        let mut exact = ProjectionBudget::new(1, usize::MAX);
        assert_eq!(exact.string("x").unwrap(), "x");
        assert_eq!(
            exact.string("y"),
            Err(PlanError::ProjectionWorkLimit { limit: 1 })
        );
    }

    #[test]
    fn projection_retained_bytes_are_charged_before_string_clone() {
        let exact_limit = size_of::<String>() + 3;
        let mut exact = ProjectionBudget::new(1, exact_limit);
        assert_eq!(exact.string("abc").unwrap(), "abc");

        let mut over = ProjectionBudget::new(1, exact_limit - 1);
        assert_eq!(
            over.string("abc"),
            Err(PlanError::ProjectionRetainedBytesLimit {
                limit: exact_limit - 1
            })
        );
    }

    #[test]
    fn recursive_value_projection_charges_every_node_and_string() {
        let value = Value::List {
            ty: Arc::new(CanonicalType::Unit),
            items: vec![Value::Str("x".to_owned())],
        };
        let mut exact = ProjectionBudget::new(4, usize::MAX);
        assert!(plan_value(&value, &mut exact).is_ok());

        let mut over = ProjectionBudget::new(3, usize::MAX);
        assert_eq!(
            plan_value(&value, &mut over),
            Err(PlanError::ProjectionWorkLimit { limit: 3 })
        );
    }

    #[test]
    fn canonical_display_has_an_exact_byte_boundary_without_rendering_a_copy() {
        let mut plan = Plan {
            lock_digest: [0; 32],
            policy_identity: "test".to_owned(),
            standard_library: None,
            packages: Vec::new(),
            acquisitions: Vec::new(),
            builds: Vec::new(),
            default_build: None,
            applications: Vec::new(),
            default_application: None,
            package_edges: 0,
            roots: Vec::new(),
            canonical_display_bytes: 0,
            project_roots: BTreeMap::new(),
        };
        let bytes = canonical_display_bytes(&plan, usize::MAX).unwrap();
        assert_eq!(canonical_display_bytes(&plan, bytes), Ok(bytes));
        assert_eq!(
            canonical_display_bytes(&plan, bytes - 1),
            Err(PlanError::DisplayBytesLimit { limit: bytes - 1 })
        );

        plan.canonical_display_bytes = bytes;
        assert_eq!(plan.to_string().len(), bytes);
    }
}
