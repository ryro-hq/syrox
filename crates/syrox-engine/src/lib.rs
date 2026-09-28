//! Effect boundaries and validated operations for Syrox.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("syrox-engine supports Linux; macOS support is planned but not implemented yet");

#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
mod build;
mod config;
mod host;
mod https_source;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod linux_fd;
mod local_source;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod lock;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod plan;
mod project;
mod realize;
#[cfg(target_os = "linux")]
mod runtime;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod runtime_native;
#[cfg(target_os = "linux")]
pub use runtime_native::runtime_helper;
mod source_archive;
mod store;

pub use build::{
    BuildCancellation, BuildError, BuildExecution, BuildIndexReport, BuildMode, BuildProtocol,
    BuildProvider, BuildRecoveryEntry, BuildRecoveryReport, BuildRecoveryState, BuildResult,
    BuildSpecification, build_autotools, build_autotools_with_cancellation, check_build_host,
    rebuild_build_index, recover_builds, run_build_gate, run_build_worker,
};
#[cfg(target_os = "linux")]
pub use build::{
    MaterializationError, MaterializedArtifact, clear_materializations, materialize_artifact,
    materialize_named_artifact, recover_materializations,
};
pub use config::{ConfigurationError, LocalCatalog, UserConfiguration};
pub use host::{
    HostCapability, HostCapabilityStatus, HostInspectError, HostInspection, inspect_host,
    inspect_host_with_probe,
};
pub use https_source::{
    HttpsAcquisition, HttpsError, HttpsSourceRequest, HttpsTransportPolicy,
    MAX_HTTPS_DEADLINE_SECONDS, MAX_HTTPS_HEADER_BYTES, MAX_HTTPS_REDIRECTS, MAX_HTTPS_URL_BYTES,
    acquire_https, acquire_https_with_cancellation,
};
pub use local_source::{
    AuthorizedLocalDirectory, LocalAcquisition, LocalSourceError, LocalSourceRequest,
    MAX_LOCAL_SOURCE_URL_BYTES, acquire_local, acquire_local_with_cancellation,
};
pub use lock::{LOCK_FILE_NAME, LockFormatError, LockStatus, MAX_LOCK_BYTES};
pub use plan::{
    MAX_PACKAGE_ID_BYTES, MAX_PLAN_DISPLAY_BYTES, MAX_PLAN_PACKAGE_EDGES, MAX_PLAN_PACKAGE_NODES,
    MAX_PLAN_PACKAGE_WORK, MAX_PLAN_PROJECTION_WORK, MAX_PLAN_RETAINED_BYTES,
    MAX_PLAN_SOURCE_REQUESTS, Plan, PlanAcquisition, PlanApplication, PlanBuild, PlanClaim,
    PlanError, PlanPackage, PlanPackageId, PlanRoot, PlanSourceRequest, PlanStandardLibrary,
    PlanType, PlanValue,
};
pub use project::{
    AuthenticatedStandardLibrary, AuthenticatedStandardSource, CheckConfiguration, CheckFailure,
    CheckReport, LoadedProject, LoadedProjectInput, LoadedProjectSource, LockCleanupError,
    LockPublicationError, LockReport, MAX_DIRECTORY_DEPTH, MAX_DIRECTORY_ENTRIES,
    MAX_PROJECT_BYTES, MAX_PROJECT_WORK, ProjectLimits, ProjectOperationError,
    StandardLibraryError, ValidatedProject, check_file, check_path, check_path_with, check_project,
    check_project_lock, check_project_lock_with, check_project_with, lock_project,
    lock_project_with, plan_project, plan_project_outputs_with, plan_project_with,
    validate_project, validate_project_with,
};
#[cfg(target_os = "linux")]
pub use project::{LockedProject, ProjectEvaluation, open_locked_project_with};
#[cfg(target_os = "linux")]
pub use realize::{
    ApplicationError, ResolvedApplication, realize_application, resolve_application,
};
pub use realize::{
    BuildProgress, RealizeError, ResolvedBuild, build_exports, realize_build,
    realize_build_with_cancellation, realize_builds_with_cancellation, resolve_build,
    resolve_builds, search_build_exports,
};
#[cfg(target_os = "linux")]
pub use runtime::{
    RuntimeClosure, RuntimeError, RuntimeMount, RuntimeOutput, RuntimeRequest, run_runtime,
    run_runtime_with_cancellation, verify_runtime, verify_runtime_with_cancellation,
};
pub use source_archive::{
    ArchiveError, ArchiveFormat, ArchiveInventory, ArchiveLimits,
    DEFAULT_LARGE_ARCHIVE_COMPRESSED_BYTES, DEFAULT_LARGE_ARCHIVE_EXPANDED_BYTES,
    MAX_ARCHIVE_DEPTH, MAX_ARCHIVE_ENTRIES, MAX_ARCHIVE_EXPANDED_BYTES, MAX_ARCHIVE_FILE_BYTES,
    MAX_ARCHIVE_PATH_BYTES, MAX_LARGE_ARCHIVE_ENTRIES, MAX_XZ_DECODER_MEMORY_BYTES,
    inspect_archive, inspect_archive_with_cancellation, inspect_source_archive,
};
pub use store::{
    ContentDigest, DigestParseError, GcCandidate, GcLimits, GcReport, GcRequest, MAX_GC_CANDIDATES,
    MAX_GC_OBJECT_BYTES, MAX_GC_OBJECTS, MAX_GC_REFERENCES, MAX_GC_ROOT_BYTES, MAX_GC_ROOTS,
    MAX_GC_WORK, MAX_RECOVERY_CANDIDATES, MAX_RECOVERY_ENTRIES, MAX_RECOVERY_WORK, MAX_ROOT_BYTES,
    MAX_ROOT_NAME_BYTES, MAX_ROOT_REFERENCES, MAX_STORE_BLOB_BYTES, MaintenanceFailure,
    MaintenanceFailureOperation, MaintenanceLease, OperationLease, RecoveryLimits, RecoveryReport,
    RootName, RootNameError, RootPublicationState, Store, StoreCleanupError, StoreError,
    StoreObject, StoreObjectState, StoreStaging, VerifiedBytes, VerifiedReader,
};
