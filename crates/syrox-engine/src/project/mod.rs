#[cfg(target_os = "linux")]
mod analyze;
#[cfg(target_os = "linux")]
#[cfg(target_os = "linux")]
mod evaluation;
#[cfg(target_os = "linux")]
pub use evaluation::{LockedProject, ProjectEvaluation, open_locked_project_with};
mod editor;
#[cfg(target_os = "linux")]
mod loader;
#[cfg(target_os = "linux")]
mod locator;
mod standard_library;
pub use editor::{
    ProjectAnalysis, ProjectAnalysisError, ProjectAnalysisLockStatus, ProjectAnalysisSnapshot,
};
#[cfg(target_os = "linux")]
pub use editor::{open_project_analysis_with, open_standard_library_analysis_with};

#[cfg(target_os = "linux")]
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::string::FromUtf8Error;

use syrox_lang::{
    CheckPolicy, Diagnostic, EvaluationEnvironment, EvaluationLimits, EvaluationSetupError,
    MAX_SOURCE_BYTES, MAX_SOURCES, RealizedProgram, Source, SourceDomainId, SourceError, SourceId,
    SourceSet,
};
use thiserror::Error;

use crate::lock::MAX_LOCK_BYTES;

/// Maximum bytes accepted across `main.srx`, inputs, and supplied standard-library sources.
pub const MAX_PROJECT_BYTES: usize = 16 * 1024 * 1024;
/// Maximum directory entries visited while loading a project.
pub const MAX_DIRECTORY_ENTRIES: usize = 65_536;
/// Maximum filesystem operations charged by the project loader.
pub const MAX_PROJECT_WORK: usize = 262_144;
/// Maximum depth below one input directory.
pub const MAX_DIRECTORY_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectLimits {
    pub max_total_bytes: usize,
    pub max_sources: usize,
    pub max_directory_entries: usize,
    pub max_work: usize,
    pub max_directory_depth: usize,
}

impl Default for ProjectLimits {
    fn default() -> Self {
        Self {
            max_total_bytes: MAX_PROJECT_BYTES,
            max_sources: MAX_SOURCES,
            max_directory_entries: MAX_DIRECTORY_ENTRIES,
            max_work: MAX_PROJECT_WORK,
            max_directory_depth: MAX_DIRECTORY_DEPTH,
        }
    }
}

/// Source text authenticated and pinned as standard-library content by the caller.
///
/// Constructing this value is an assertion at the engine trust boundary. The CLI does not
/// construct these values from project text or filesystem names.
#[derive(Clone, Debug)]
pub struct AuthenticatedStandardSource {
    source: Source,
}

impl AuthenticatedStandardSource {
    pub fn from_authenticated(
        name: impl Into<std::sync::Arc<str>>,
        text: impl Into<std::sync::Arc<str>>,
    ) -> Result<Self, SourceError> {
        Ok(Self {
            source: Source::new(name, text)?,
        })
    }

    pub fn name(&self) -> &str {
        self.source.name()
    }

    pub fn text(&self) -> &str {
        self.source.text()
    }
}

/// A caller-authenticated, canonically ordered standard-library snapshot.
#[derive(Clone, Debug)]
pub struct AuthenticatedStandardLibrary {
    sources: Vec<AuthenticatedStandardSource>,
    digest: [u8; 32],
}

impl AuthenticatedStandardLibrary {
    /// Asserts that `sources` were authenticated by the caller.
    pub fn from_authenticated(
        mut sources: Vec<AuthenticatedStandardSource>,
    ) -> Result<Self, StandardLibraryError> {
        if sources.is_empty() || sources.len() > MAX_SOURCES - 1 {
            return Err(StandardLibraryError::InvalidSourceCount);
        }
        sources.sort_by(|left, right| left.name().cmp(right.name()));
        if sources.iter().any(|source| {
            source.name().len() > MAX_SOURCE_BYTES
                || !crate::lock::is_canonical_logical_path(source.name().as_bytes())
        }) {
            return Err(StandardLibraryError::InvalidSourceName);
        }
        if sources
            .windows(2)
            .any(|pair| pair[0].name() == pair[1].name())
        {
            return Err(StandardLibraryError::DuplicateSource);
        }
        let digest = crate::lock::standard_library_digest(&sources);
        Ok(Self { sources, digest })
    }

    pub fn sources(&self) -> impl ExactSizeIterator<Item = &AuthenticatedStandardSource> {
        self.sources.iter()
    }

    pub const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum StandardLibraryError {
    #[error("standard library must contain between 1 and {} sources", MAX_SOURCES - 1)]
    InvalidSourceCount,
    #[error("standard-library source names must be unique")]
    DuplicateSource,
    #[error("standard-library source names must be nonempty and bounded")]
    InvalidSourceName,
}

#[derive(Clone, Debug)]
pub struct CheckConfiguration {
    pub standard_library: Option<AuthenticatedStandardLibrary>,
    pub policy: CheckPolicy,
    pub environment: EvaluationEnvironment,
    pub evaluation_limits: EvaluationLimits,
    pub project_limits: ProjectLimits,
}

impl Default for CheckConfiguration {
    fn default() -> Self {
        let policy = CheckPolicy::default();
        let environment = EvaluationEnvironment::new(&policy);
        Self {
            standard_library: None,
            policy,
            environment,
            evaluation_limits: EvaluationLimits::default(),
            project_limits: ProjectLimits::default(),
        }
    }
}

#[derive(Debug)]
pub struct LoadedProjectSource {
    relative_path: PathBuf,
    source_id: SourceId,
}

#[derive(Debug)]
pub struct LoadedProjectAsset {
    relative_path: PathBuf,
    size: u64,
    digest: [u8; 32],
}

impl LoadedProjectAsset {
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }
    pub const fn size(&self) -> u64 {
        self.size
    }
    pub const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

impl LoadedProjectSource {
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    pub const fn source_id(&self) -> SourceId {
        self.source_id
    }
}

#[derive(Debug)]
pub struct LoadedProjectInput {
    name: String,
    locator: String,
    domain: SourceDomainId,
    files: Vec<LoadedProjectSource>,
}

impl LoadedProjectInput {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn locator(&self) -> &str {
        &self.locator
    }

    pub const fn domain(&self) -> SourceDomainId {
        self.domain
    }

    pub fn files(&self) -> impl ExactSizeIterator<Item = &LoadedProjectSource> {
        self.files.iter()
    }
}

#[derive(Debug)]
pub struct LoadedProject {
    path: PathBuf,
    #[cfg(target_os = "linux")]
    root: crate::linux_fd::OpenedPath,
    sources: SourceSet,
    main_source: SourceId,
    inputs: Vec<LoadedProjectInput>,
    assets: Vec<LoadedProjectAsset>,
    #[cfg(target_os = "linux")]
    child_edges: Vec<crate::lock::graph::ProjectEdge>,
    #[cfg(target_os = "linux")]
    children: Vec<(String, std::sync::Arc<LoadedProject>)>,
}

impl LoadedProject {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub const fn sources(&self) -> &SourceSet {
        &self.sources
    }

    pub fn main_source(&self) -> &Source {
        self.sources
            .get(self.main_source)
            .expect("loaded project retains its main source")
    }

    pub fn inputs(&self) -> impl ExactSizeIterator<Item = &LoadedProjectInput> {
        self.inputs.iter()
    }

    pub fn assets(&self) -> impl ExactSizeIterator<Item = &LoadedProjectAsset> {
        self.assets.iter()
    }

    #[cfg_attr(not(target_os = "linux"), allow(clippy::unused_self))]
    pub(crate) fn child_edges(&self) -> &[crate::lock::graph::ProjectEdge] {
        #[cfg(target_os = "linux")]
        {
            &self.child_edges
        }
        #[cfg(not(target_os = "linux"))]
        {
            &[]
        }
    }
}

#[derive(Debug)]
pub struct ValidatedProject {
    loaded: LoadedProject,
    declarations: usize,
    realized: RealizedProgram,
}

impl ValidatedProject {
    pub const fn loaded(&self) -> &LoadedProject {
        &self.loaded
    }

    pub const fn declarations(&self) -> usize {
        self.declarations
    }

    pub const fn realized(&self) -> &RealizedProgram {
        &self.realized
    }

    pub fn report(&self) -> CheckReport {
        CheckReport {
            path: self.loaded.path.clone(),
            declarations: self.declarations,
            realized_roots: self.realized.roots().len(),
        }
    }
}

#[derive(Debug)]
pub struct CheckReport {
    pub path: PathBuf,
    pub declarations: usize,
    pub realized_roots: usize,
}

#[derive(Debug)]
pub struct LockReport {
    status: crate::LockStatus,
    check: CheckReport,
    digest: [u8; 32],
}

impl LockReport {
    pub const fn status(&self) -> crate::LockStatus {
        self.status
    }

    pub const fn check(&self) -> &CheckReport {
        &self.check
    }

    pub const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

#[derive(Debug, Error)]
pub enum LockPublicationError {
    #[error("lock publication failed before replacement: {source}")]
    BeforeRename {
        #[source]
        source: io::Error,
    },
    #[error("lock publication failed before replacement ({source}) and cleanup failed: {cleanup}")]
    CleanupFailed {
        source: io::Error,
        cleanup: LockCleanupError,
    },
}

#[derive(Debug, Error)]
pub enum LockCleanupError {
    #[error("temporary removal failed: {source}")]
    Removal {
        #[source]
        source: io::Error,
    },
    #[error("cleanup directory synchronization failed: {source}")]
    Synchronization {
        #[source]
        source: io::Error,
    },
    #[error(
        "temporary removal failed ({removal}) and cleanup directory synchronization failed ({synchronization})"
    )]
    RemovalAndSynchronization {
        removal: io::Error,
        synchronization: io::Error,
    },
}

#[derive(Debug, Error)]
pub enum ProjectOperationError {
    #[error(transparent)]
    Editor(#[from] ProjectAnalysisError),
    #[error("project has no value output `{name}`")]
    MissingOutput { name: String },
    #[error("catalog package set is invalid: {reason}")]
    InvalidPackageSet { reason: &'static str },
    #[error("package set has no entry `{key}`")]
    MissingPackageReference { key: String },
    #[error(transparent)]
    Check(#[from] CheckFailure),
    #[error("project has no {name}")]
    MissingLock { name: &'static str },
    #[error("project lock is malformed or noncanonical: {source}")]
    MalformedLock {
        #[source]
        source: crate::LockFormatError,
    },
    #[error("project sources have drifted from the lock")]
    ProjectDrift,
    #[error("project graph lock has drifted from its authenticated child snapshots")]
    GraphDrift,
    #[error(
        "project asset `{path}` is not authenticated in the consuming project with the requested digest"
    )]
    UnpinnedProjectAsset { path: String },
    #[error("project source `{path}` has no authenticated project origin")]
    UnknownProjectSourceOrigin { path: String },
    #[error("project graph lock is malformed or noncanonical: {source}")]
    MalformedGraphLock { source: crate::LockFormatError },
    #[error("authenticated standard library has drifted from the lock")]
    StandardLibraryDrift,
    #[error("project lock is a symbolic link")]
    LockSymlink,
    #[error("project lock is not a regular file")]
    LockNonRegular,
    #[error("project lock has multiple hard links")]
    LockHardLinked,
    #[error("cannot access project lock: {source}")]
    LockIo {
        #[source]
        source: io::Error,
    },
    #[error("cannot remove obsolete graph lock: {source}")]
    GraphRemoval { source: io::Error },
    #[error(transparent)]
    LockPublication(#[from] LockPublicationError),
    #[error(
        "project lock was replaced but its identity or publication durability could not be confirmed: {source}"
    )]
    LockPublicationUncertain {
        #[source]
        source: io::Error,
    },
    #[error("the current lock requires the default `syrox.empty` policy and matching environment")]
    PolicyProvenanceRequired,
    #[error("generated canonical lock exceeds the {MAX_LOCK_BYTES}-byte limit")]
    GeneratedLockTooLarge,
    #[error(transparent)]
    Plan(#[from] crate::PlanError),
}

#[derive(Debug, Error)]
pub enum CheckFailure {
    #[error("secure project loading requires Linux; macOS support is not implemented yet")]
    UnsupportedPlatform,
    #[error("the Linux kernel does not provide the required openat2 secure traversal: {source}")]
    UnsupportedKernel {
        #[source]
        source: io::Error,
    },
    #[error("configured project limit `{field}` is {configured}, above the hard maximum {maximum}")]
    InvalidProjectLimit {
        field: &'static str,
        configured: usize,
        maximum: usize,
    },
    #[error("cannot inspect {path}: {source}")]
    Inspect { path: PathBuf, source: io::Error },
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("{path} exceeds the {MAX_SOURCE_BYTES}-byte source limit")]
    TooLarge { path: PathBuf },
    #[error("project sources exceed the configured {limit}-byte total limit")]
    ProjectTooLarge { limit: usize },
    #[error("project source count exceeds the configured {limit}-source limit")]
    TooManySources { limit: usize },
    #[error("project directory entries exceed the configured {limit}-entry limit")]
    TooManyDirectoryEntries { limit: usize },
    #[error("project loading exceeds the configured {limit}-work-unit limit")]
    WorkLimit { limit: usize },
    #[error("{path} exceeds the configured input directory depth of {limit}")]
    DirectoryDepth { path: PathBuf, limit: usize },
    #[error("{path} is not valid UTF-8: {source}")]
    InvalidUtf8 {
        path: PathBuf,
        source: FromUtf8Error,
    },
    #[error("path contains a non-UTF-8 component: {path:?}")]
    InvalidPathEncoding { path: PathBuf },
    #[error("project directory is missing main.srx: {path}")]
    MissingMain { path: PathBuf },
    #[error("expected a regular .srx source file: {path}")]
    InvalidSourceFile { path: PathBuf },
    #[error("expected a regular asset file: {path}")]
    InvalidAssetFile { path: PathBuf },
    #[error("expected an input directory: {path}")]
    InputNotDirectory { path: PathBuf },
    #[error("symbolic links are not allowed in project sources: {path}")]
    SymbolicLink { path: PathBuf },
    #[error("input `{name}` has unsupported locator `{locator}`; expected path:<relative-path>")]
    UnsupportedLocator { name: String, locator: String },
    #[error("input `{name}` path must be relative and contain no `..`: {path}")]
    UnsafeInputPath { name: String, path: PathBuf },
    #[error("duplicate project input name `{name}`")]
    DuplicateInputName { name: String },
    #[error("invalid modules input: {reason}")]
    InvalidModuleInput { reason: String },
    #[error("child project {path} has no {name}")]
    MissingChildLock { path: PathBuf, name: &'static str },
    #[error("child project {path} lock could not be verified: {reason}")]
    InvalidChildLock { path: PathBuf, reason: String },
    #[error("project snapshot contains a cycle or repeated directory: {path}")]
    ChildProjectCycle { path: PathBuf },
    #[error("project graph exceeds the configured input directory depth of {limit}")]
    ProjectGraphDepth { limit: usize },
    #[error("project has more than {limit} imported project edges")]
    TooManyProjectEdges { limit: usize },
    #[error("input paths overlap and would alias source authority: {first} and {second}")]
    AliasedInputPaths { first: PathBuf, second: PathBuf },
    #[error("source files are aliases of the same filesystem object: {first} and {second}")]
    AliasedSourceFiles { first: PathBuf, second: PathBuf },
    #[error(transparent)]
    InvalidSource(#[from] SourceError),
    #[error("source contains {} error(s)", .errors.len())]
    Diagnostics {
        input: SourceSet,
        errors: Vec<Diagnostic>,
    },
    #[error("evaluation setup failed: {source}")]
    EvaluationSetup {
        #[source]
        source: EvaluationSetupError,
    },
    #[error("evaluation failed for {} root(s)", .failed_roots.len())]
    Evaluation {
        input: SourceSet,
        errors: Vec<Diagnostic>,
        failed_roots: Vec<String>,
    },
}

pub fn check_path_with(
    path: &Path,
    configuration: &CheckConfiguration,
) -> Result<CheckReport, CheckFailure> {
    validate_project_limits(configuration.project_limits)?;
    check_path_platform(path, configuration)
}

pub fn check_project(path: &Path) -> Result<CheckReport, CheckFailure> {
    check_project_with(path, &CheckConfiguration::default())
}

pub fn check_project_with(
    path: &Path,
    configuration: &CheckConfiguration,
) -> Result<CheckReport, CheckFailure> {
    validate_project_limits(configuration.project_limits)?;
    check_project_platform(path, configuration)
}

pub fn validate_project_with(
    path: &Path,
    configuration: &CheckConfiguration,
) -> Result<ValidatedProject, CheckFailure> {
    validate_project_limits(configuration.project_limits)?;
    validate_project_platform(path, configuration)
}

pub fn lock_project(path: &Path) -> Result<LockReport, ProjectOperationError> {
    lock_project_with(path, &CheckConfiguration::default())
}

pub fn lock_project_with(
    path: &Path,
    configuration: &CheckConfiguration,
) -> Result<LockReport, ProjectOperationError> {
    validate_project_limits(configuration.project_limits)?;
    validate_lock_policy(configuration)?;
    lock_project_platform(path, configuration)
}

pub fn check_project_lock(path: &Path) -> Result<LockReport, ProjectOperationError> {
    check_project_lock_with(path, &CheckConfiguration::default())
}

pub fn check_project_lock_with(
    path: &Path,
    configuration: &CheckConfiguration,
) -> Result<LockReport, ProjectOperationError> {
    validate_project_limits(configuration.project_limits)?;
    validate_lock_policy(configuration)?;
    check_project_lock_platform(path, configuration)
}

pub fn plan_project(path: &Path) -> Result<crate::Plan, ProjectOperationError> {
    plan_project_with(path, &CheckConfiguration::default())
}

pub fn plan_project_with(
    path: &Path,
    configuration: &CheckConfiguration,
) -> Result<crate::Plan, ProjectOperationError> {
    validate_project_limits(configuration.project_limits)?;
    validate_lock_policy(configuration)?;
    plan_project_platform(path, configuration)
}

/// Project only the requested top-level value outputs from one verified
/// snapshot. Repeated names select the same root; dependencies expressed as
/// value references remain lazy. Package-name dependency expansion is separate.
pub fn plan_project_outputs_with(
    path: &Path,
    configuration: &CheckConfiguration,
    outputs: &[&str],
) -> Result<crate::Plan, ProjectOperationError> {
    #[cfg(target_os = "linux")]
    {
        let project = open_locked_project_with(path, configuration)?;
        let mut evaluation = project.evaluation()?;
        for output in outputs {
            evaluation.evaluate_root(output)?;
        }
        evaluation.into_plan()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (path, configuration, outputs);
        Err(CheckFailure::UnsupportedPlatform.into())
    }
}

/// Plan one catalog key and its providers from a verified snapshot. Projects
/// without a typed set retain their ordinary complete-Plan lookup behavior.
pub fn plan_project_package_with(
    path: &Path,
    configuration: &CheckConfiguration,
    key: &str,
) -> Result<crate::Plan, ProjectOperationError> {
    #[cfg(target_os = "linux")]
    {
        let locked = open_locked_project_with(path, configuration)?;
        let mut evaluation = locked.evaluation()?;
        if evaluation.select_package(key)? {
            evaluation.into_selected_plan()
        } else {
            evaluation.evaluate_all()?;
            evaluation.into_plan()
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (path, configuration, key);
        Err(CheckFailure::UnsupportedPlatform.into())
    }
}

fn validate_project_limits(limits: ProjectLimits) -> Result<(), CheckFailure> {
    for (field, configured, maximum) in [
        ("max_total_bytes", limits.max_total_bytes, MAX_PROJECT_BYTES),
        ("max_sources", limits.max_sources, MAX_SOURCES),
        (
            "max_directory_entries",
            limits.max_directory_entries,
            MAX_DIRECTORY_ENTRIES,
        ),
        ("max_work", limits.max_work, MAX_PROJECT_WORK),
        (
            "max_directory_depth",
            limits.max_directory_depth,
            MAX_DIRECTORY_DEPTH,
        ),
    ] {
        if configured > maximum {
            return Err(CheckFailure::InvalidProjectLimit {
                field,
                configured,
                maximum,
            });
        }
    }
    Ok(())
}

fn validate_lock_policy(configuration: &CheckConfiguration) -> Result<(), ProjectOperationError> {
    let default = CheckPolicy::default();
    if configuration.policy != default || configuration.environment.policy() != &default {
        return Err(ProjectOperationError::PolicyProvenanceRequired);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn check_path_platform(
    path: &Path,
    configuration: &CheckConfiguration,
) -> Result<CheckReport, CheckFailure> {
    loader::check_path_linux(path, configuration)
}

#[cfg(not(target_os = "linux"))]
fn check_path_platform(
    _path: &Path,
    _configuration: &CheckConfiguration,
) -> Result<CheckReport, CheckFailure> {
    Err(CheckFailure::UnsupportedPlatform)
}

#[cfg(target_os = "linux")]
fn check_project_platform(
    path: &Path,
    configuration: &CheckConfiguration,
) -> Result<CheckReport, CheckFailure> {
    loader::check_project_linux(path, configuration)
}

#[cfg(not(target_os = "linux"))]
fn check_project_platform(
    _path: &Path,
    _configuration: &CheckConfiguration,
) -> Result<CheckReport, CheckFailure> {
    Err(CheckFailure::UnsupportedPlatform)
}

#[cfg(target_os = "linux")]
fn validate_project_platform(
    path: &Path,
    configuration: &CheckConfiguration,
) -> Result<ValidatedProject, CheckFailure> {
    loader::validate_project_linux(path, configuration)
}

#[cfg(not(target_os = "linux"))]
fn validate_project_platform(
    _path: &Path,
    _configuration: &CheckConfiguration,
) -> Result<ValidatedProject, CheckFailure> {
    Err(CheckFailure::UnsupportedPlatform)
}

#[cfg(target_os = "linux")]
fn expected_lock(
    loaded: &LoadedProject,
    configuration: &CheckConfiguration,
) -> Result<crate::lock::LockManifest, ProjectOperationError> {
    crate::lock::LockManifest::generate(loaded, configuration.standard_library.as_ref())
        .map_err(map_generated_lock_error)
}

#[cfg(target_os = "linux")]
fn map_generated_lock_error(source: crate::LockFormatError) -> ProjectOperationError {
    match source {
        crate::LockFormatError::TooLarge => ProjectOperationError::GeneratedLockTooLarge,
        source => ProjectOperationError::MalformedLock { source },
    }
}

#[cfg(target_os = "linux")]
fn read_lock(
    project: &LoadedProject,
) -> Result<Option<crate::lock::LockManifest>, ProjectOperationError> {
    let bytes = crate::linux_fd::read_bounded_beneath(
        project.root.fd(),
        Path::new(crate::LOCK_FILE_NAME),
        MAX_LOCK_BYTES,
    )
    .map_err(map_lock_io)?;
    bytes
        .map(|bytes| {
            crate::lock::LockManifest::parse(&bytes)
                .map_err(|source| ProjectOperationError::MalformedLock { source })
        })
        .transpose()
}

#[cfg(target_os = "linux")]
fn map_lock_io(source: crate::linux_fd::LockIoError) -> ProjectOperationError {
    match source {
        crate::linux_fd::LockIoError::Symlink => ProjectOperationError::LockSymlink,
        crate::linux_fd::LockIoError::NonRegular => ProjectOperationError::LockNonRegular,
        crate::linux_fd::LockIoError::HardLinked => ProjectOperationError::LockHardLinked,
        crate::linux_fd::LockIoError::TooLarge => ProjectOperationError::MalformedLock {
            source: crate::LockFormatError::TooLarge,
        },
        crate::linux_fd::LockIoError::Io(source) => ProjectOperationError::LockIo { source },
    }
}

#[cfg(target_os = "linux")]
fn map_lock_publication(error: crate::linux_fd::WriteAtomicError) -> ProjectOperationError {
    match error {
        crate::linux_fd::WriteAtomicError::BeforeRename {
            source,
            cleanup: None,
        } => LockPublicationError::BeforeRename { source }.into(),
        crate::linux_fd::WriteAtomicError::BeforeRename {
            source,
            cleanup: Some(cleanup),
        } => LockPublicationError::CleanupFailed {
            source,
            cleanup: map_lock_cleanup(cleanup),
        }
        .into(),
        crate::linux_fd::WriteAtomicError::AfterRename { source } => {
            ProjectOperationError::LockPublicationUncertain { source }
        }
    }
}

#[cfg(target_os = "linux")]
fn map_lock_cleanup(source: crate::linux_fd::CleanupError) -> LockCleanupError {
    match source {
        crate::linux_fd::CleanupError::Removal(source) => LockCleanupError::Removal { source },
        crate::linux_fd::CleanupError::Synchronization(source) => {
            LockCleanupError::Synchronization { source }
        }
        crate::linux_fd::CleanupError::RemovalAndSynchronization {
            removal,
            synchronization,
        } => LockCleanupError::RemovalAndSynchronization {
            removal,
            synchronization,
        },
    }
}

#[cfg(target_os = "linux")]
fn compare_lock(
    actual: &crate::lock::LockManifest,
    expected: &crate::lock::LockManifest,
) -> Result<(), ProjectOperationError> {
    match actual.drift(expected) {
        None => Ok(()),
        Some(crate::lock::LockDrift::Project) => Err(ProjectOperationError::ProjectDrift),
        Some(crate::lock::LockDrift::StandardLibrary) => {
            Err(ProjectOperationError::StandardLibraryDrift)
        }
        Some(crate::lock::LockDrift::Graph) => Err(ProjectOperationError::GraphDrift),
    }
}

#[cfg(target_os = "linux")]
const GRAPH_LOCK_FILE: &str = "Syrox.graph.lock";

#[cfg(target_os = "linux")]
fn read_graph_lock(
    project: &LoadedProject,
) -> Result<Option<crate::lock::graph::GraphLock>, ProjectOperationError> {
    let bytes = crate::linux_fd::read_bounded_beneath(
        project.root.fd(),
        Path::new(GRAPH_LOCK_FILE),
        MAX_LOCK_BYTES,
    )
    .map_err(map_lock_io)?;
    bytes
        .map(|bytes| {
            crate::lock::graph::GraphLock::parse(&bytes)
                .map_err(|source| ProjectOperationError::MalformedGraphLock { source })
        })
        .transpose()
}

#[cfg(target_os = "linux")]
fn lock_project_platform(
    path: &Path,
    configuration: &CheckConfiguration,
) -> Result<LockReport, ProjectOperationError> {
    let loaded = loader::load_project_linux(path, configuration)?;
    let checked = analyze::check_loaded(&loaded, configuration)?;
    let expected = expected_lock(&loaded, configuration)?;
    let existing = read_lock(&loaded)?;
    let old_graph = read_graph_lock(&loaded)?;
    let status = match (&existing, &old_graph) {
        (None, _) => crate::LockStatus::Created,
        (Some(existing), None) if existing.data() == expected.data() => {
            crate::LockStatus::Unchanged
        }
        _ => crate::LockStatus::Updated,
    };
    if status != crate::LockStatus::Unchanged {
        crate::linux_fd::write_atomic_beneath(
            loaded.root.fd(),
            Path::new(crate::LOCK_FILE_NAME),
            expected.data(),
        )
        .map_err(map_lock_publication)?;
    }
    if old_graph.is_some() {
        let opened =
            crate::linux_fd::open_existing_regular(loaded.root.fd(), Path::new(GRAPH_LOCK_FILE))
                .map_err(|source| ProjectOperationError::GraphRemoval { source })?;
        crate::linux_fd::unlink_opened(
            loaded.root.fd(),
            Path::new(GRAPH_LOCK_FILE),
            opened.metadata().identity(),
        )
        .map_err(|source| ProjectOperationError::GraphRemoval { source })?;
        crate::linux_fd::sync_directory(loaded.root.fd())
            .map_err(|source| ProjectOperationError::GraphRemoval { source })?;
    }
    Ok(LockReport {
        status,
        check: CheckReport {
            path: loaded.path.clone(),
            declarations: checked.resolved().parsed().declaration_count(),
            realized_roots: 0,
        },
        digest: *expected.digest(),
    })
}

#[cfg(not(target_os = "linux"))]
fn lock_project_platform(
    _path: &Path,
    _configuration: &CheckConfiguration,
) -> Result<LockReport, ProjectOperationError> {
    Err(CheckFailure::UnsupportedPlatform.into())
}

#[cfg(target_os = "linux")]
fn check_project_lock_platform(
    path: &Path,
    configuration: &CheckConfiguration,
) -> Result<LockReport, ProjectOperationError> {
    let project = open_locked_project_with(path, configuration)?;
    Ok(LockReport {
        status: crate::LockStatus::Unchanged,
        check: project.check_report(),
        digest: *project.lock_digest(),
    })
}

#[cfg(not(target_os = "linux"))]
fn check_project_lock_platform(
    _path: &Path,
    _configuration: &CheckConfiguration,
) -> Result<LockReport, ProjectOperationError> {
    Err(CheckFailure::UnsupportedPlatform.into())
}

#[cfg(target_os = "linux")]
fn plan_project_platform(
    path: &Path,
    configuration: &CheckConfiguration,
) -> Result<crate::Plan, ProjectOperationError> {
    let project = open_locked_project_with(path, configuration)?;
    let mut evaluation = project.evaluation()?;
    if evaluation.select_all_packages()? {
        evaluation.include_project_outputs()?;
        evaluation.into_selected_plan()
    } else {
        evaluation.evaluate_all()?;
        evaluation.into_plan()
    }
}

#[cfg(target_os = "linux")]
fn project_plan(
    loaded: &LoadedProject,
    realized: &RealizedProgram,
    digest: [u8; 32],
    configuration: &CheckConfiguration,
) -> Result<crate::Plan, ProjectOperationError> {
    analyze::require_success(loaded, realized)?;
    let mut plan = crate::Plan::from_realized(
        realized,
        digest,
        configuration.policy.identity(),
        configuration.standard_library.as_ref(),
    )
    .map_err(ProjectOperationError::from)?;
    let mut roots = BTreeMap::new();
    collect_project_roots(loaded, loaded, SourceDomainId::project(), &mut roots);
    for source in plan
        .acquisitions()
        .flat_map(crate::PlanAcquisition::sources)
    {
        if let Some(path) = source.url().strip_prefix("project:") {
            let Some(owner) = source.owner().and_then(|domain| roots.get(&domain)) else {
                return Err(ProjectOperationError::UnknownProjectSourceOrigin {
                    path: path.to_owned(),
                });
            };
            let Some(asset_path) = path.strip_prefix("assets/") else {
                continue;
            };
            let relative = Path::new("assets").join(asset_path);
            if !owner.assets().any(|asset| {
                asset.relative_path() == relative
                    && asset.digest() == source.digest().as_bytes()
                    && asset.size() <= source.maximum_bytes()
            }) {
                return Err(ProjectOperationError::UnpinnedProjectAsset {
                    path: relative.display().to_string(),
                });
            }
        }
    }
    plan.bind_project_roots(
        roots
            .into_iter()
            .map(|(domain, project)| (domain, project.path().to_path_buf()))
            .collect(),
    );
    Ok(plan)
}

#[cfg(target_os = "linux")]
fn collect_project_roots<'a>(
    root: &LoadedProject,
    project: &'a LoadedProject,
    domain: SourceDomainId,
    roots: &mut BTreeMap<SourceDomainId, &'a LoadedProject>,
) {
    if roots.insert(domain, project).is_some() {
        return;
    }
    for (alias, child) in &project.children {
        let child_domain = root
            .sources()
            .child_project_domain(domain, alias)
            .expect("validated child has a source domain");
        collect_project_roots(root, child, child_domain, roots);
    }
}

#[cfg(not(target_os = "linux"))]
fn plan_project_platform(
    _path: &Path,
    _configuration: &CheckConfiguration,
) -> Result<crate::Plan, ProjectOperationError> {
    Err(CheckFailure::UnsupportedPlatform.into())
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
