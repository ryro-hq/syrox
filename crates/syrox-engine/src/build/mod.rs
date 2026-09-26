//! The first concrete build consumer. The package graph itself carries no build semantics.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::{ContentDigest, Plan, RootName, Store, StoreError};

#[cfg(target_os = "linux")]
pub(crate) mod artifact;
#[cfg(target_os = "linux")]
pub(crate) mod cache;
mod cancellation;
#[cfg(target_os = "linux")]
mod capture;
#[cfg(target_os = "linux")]
mod group;
#[cfg(target_os = "linux")]
mod journal;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::retained_application;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) use linux::{PreparedLookup, build_prepared, prepare_managed};
#[cfg(target_os = "linux")]
pub(crate) mod materialize;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)] // getrusage writes one rusage per checked syscall in the opt-in profiler.
pub(crate) mod profile;
#[cfg(target_os = "linux")]
pub(crate) mod records;
mod recovery;
mod reindex;
#[cfg(target_os = "linux")]
mod sandbox;
#[cfg(target_os = "linux")]
mod shared;
#[cfg(target_os = "linux")]
mod worker;

pub use cancellation::BuildCancellation;
pub(crate) use cancellation::{cancellation_io, is_cancellation_io};
#[cfg(target_os = "linux")]
pub use materialize::{
    MaterializationError, MaterializedArtifact, clear_materializations, materialize_artifact,
    materialize_named_artifact, recover_materializations,
};
pub use recovery::{BuildRecoveryEntry, BuildRecoveryReport, BuildRecoveryState, recover_builds};
pub use reindex::{BuildIndexReport, rebuild_build_index};

pub(crate) const MAX_OUTPUT_BYTES: u64 = 128 * 1024 * 1024;
pub(crate) const MAX_AUTOTOOLS_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_SOURCE_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_LARGE_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
pub(crate) const PROFILE: &str = "syrox-host-autotools-x86_64";
pub(crate) const GLIBC_PROFILE: &str = "syrox-host-glibc-x86_64";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildProtocol {
    Autotools,
    Glibc,
}

#[derive(Clone, Copy, Debug)]
struct ResourceEnvelope {
    memory: u64,
    tasks: u32,
    cpu_percent: u32,
    nofile: u32,
    output_bytes: u64,
    work_tmpfs: u64,
    output_tmpfs: u64,
}

impl BuildProtocol {
    const fn envelope(self) -> ResourceEnvelope {
        match self {
            Self::Autotools => ResourceEnvelope {
                memory: 805_306_368,
                tasks: 64,
                cpu_percent: 100,
                nofile: 256,
                output_bytes: MAX_AUTOTOOLS_OUTPUT_BYTES,
                work_tmpfs: 402_653_184,
                output_tmpfs: 67_108_864,
            },
            Self::Glibc => ResourceEnvelope {
                memory: 3_221_225_472,
                tasks: 256,
                cpu_percent: 200,
                nofile: 4096,
                output_bytes: MAX_OUTPUT_BYTES,
                work_tmpfs: 1_610_612_736,
                output_tmpfs: 402_653_184,
            },
        }
    }
}

/// A selected build with its declared inputs. Caller selections are checked
/// against the locked Plan before any effects.
#[derive(Clone, Debug)]
pub struct BuildSpecification {
    pub package: String,
    pub source_directory: String,
    pub entry: String,
    pub timeout_seconds: u32,
    pub mode: BuildMode,
}

#[derive(Clone, Debug)]
pub enum BuildMode {
    Glibc,
    Autotools { provider: Option<BuildProvider> },
}

#[derive(Clone, Debug)]
pub struct BuildProvider {
    pub root: RootName,
    pub receipt: ContentDigest,
    /// Whether the consumer explicitly selects the provider's `dev` output.
    pub development: bool,
}

impl BuildSpecification {
    pub const fn protocol(&self) -> BuildProtocol {
        match self.mode {
            BuildMode::Glibc => BuildProtocol::Glibc,
            BuildMode::Autotools { .. } => BuildProtocol::Autotools,
        }
    }

    pub const fn provider(&self) -> Option<&BuildProvider> {
        match &self.mode {
            BuildMode::Glibc => None,
            BuildMode::Autotools { provider } => provider.as_ref(),
        }
    }

    fn validate(&self) -> Result<(), BuildError> {
        if !relative_path(&self.source_directory)
            || self.source_directory.contains('/')
            || !relative_path(&self.entry)
            || !(1..=if self.protocol() == BuildProtocol::Glibc {
                1800
            } else {
                300
            })
                .contains(&self.timeout_seconds)
            || (self.protocol() == BuildProtocol::Glibc
                && self.entry != "usr/lib/ld-linux-x86-64.so.2")
        {
            return Err(BuildError::InvalidRequest);
        }
        Ok(())
    }
}

pub(crate) fn relative_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 255
        && path.split('/').count() <= 32
        && path.split('/').all(|part| {
            !part.is_empty()
                && !matches!(part, "." | "..")
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._+-@,".contains(&b))
        })
}

#[derive(Clone, Debug)]
pub enum BuildExecution {
    Built { operation: String },
    Shared { operation: String },
    Cached,
}

#[derive(Clone, Debug)]
pub struct BuildResult {
    /// Cache hits have no new operation. Shared consumers name their producer.
    pub execution: BuildExecution,
    pub action: ContentDigest,
    pub artifact: ContentDigest,
    pub receipt: ContentDigest,
    pub toolchain: ContentDigest,
    pub files: usize,
    pub root: RootName,
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error(
        "build requires Linux x86_64 with systemd user services, Bubblewrap and setpriv Landlock support"
    )]
    UnsupportedPlatform,
    #[error(
        "invalid build request: canonical relative paths and a deadline within the selected protocol are required"
    )]
    InvalidRequest,
    #[error("host build requires an existing package with no dependencies and exactly one source")]
    UnsupportedInputs,
    #[error("build source is absent or exceeds the selected protocol's compressed source limit")]
    MissingSource,
    #[error("host build requires the explicitly selected /usr toolchain")]
    Toolchain,
    #[error("build host capability unavailable: {0}")]
    Capability(&'static str),
    #[error("toolchain inventory refused: {0}")]
    Inventory(String),
    #[error("build input changed during the operation")]
    InputChanged,
    #[error("build deadline elapsed")]
    Timeout,
    #[error("build cancelled")]
    Cancelled,
    #[error("build payload or sandbox failed: {0}")]
    Execution(String),
    #[error("build resource settlement is unconfirmed ({reason}); private staging retained at {}{}", stage.display(), primary.as_ref().map_or_else(String::new, |error| format!("; original failure: {error}")))]
    Settlement {
        stage: PathBuf,
        reason: &'static str,
        #[source]
        primary: Option<Box<BuildError>>,
    },
    #[error("invalid, missing, or oversized build output")]
    Output,
    #[error("build cleanup failed: {0}")]
    Cleanup(std::io::Error),
    #[error("build failed ({primary}) and private staging cleanup also failed: {cleanup}")]
    FailureAndCleanup {
        #[source]
        primary: Box<BuildError>,
        cleanup: std::io::Error,
    },
    #[error("build operation journal failed: {0}")]
    Journal(String),
    #[error("build failed ({primary}); operation journal also failed: {journal}")]
    FailureAndJournal {
        #[source]
        primary: Box<BuildError>,
        journal: String,
    },
    #[error(
        "build result was published under {root}, but its operation journal update failed: {journal}"
    )]
    PublishedAndJournal { root: RootName, journal: String },
    #[error("build action index refused: {0}")]
    Cache(String),
    #[error("divergent build results for action {action}: receipts {first} and {second}")]
    DivergentResults {
        action: ContentDigest,
        first: ContentDigest,
        second: ContentDigest,
    },
    #[error("build result was published under {root}, but action indexing failed: {source}")]
    PublishedAndCache {
        root: RootName,
        #[source]
        source: Box<BuildError>,
    },
    #[error("shared build producer {operation}: {reason}")]
    SharedProducer { operation: String, reason: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Archive(#[from] crate::ArchiveError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Builds with a caller-authorized host toolchain. `worker` must be the trusted
/// srx executable implementing the internal worker dispatch. Publication is last,
/// after payload quiescence, unit finalization, artifact validation, input recheck
/// and staging cleanup.
pub fn build_autotools(
    plan: &Plan,
    request: &BuildSpecification,
    store: &Store,
    root: &RootName,
    toolchain: &Path,
    worker: &Path,
) -> Result<BuildResult, BuildError> {
    build_autotools_with_cancellation(
        plan,
        request,
        store,
        root,
        toolchain,
        worker,
        &BuildCancellation::default(),
    )
}

/// Cancellable form of [`build_autotools`]. Concurrent consumers share a producer
/// with independent interests. A cancelled caller gets no new custom root, while
/// the shared managed result may still be published for another caller. The first
/// caller continues coordinating that producer until completion. Cancellation of
/// the last interest before commit prevents publication; a commit already in
/// progress finishes with its actual durable/uncertain outcome.
pub fn build_autotools_with_cancellation(
    plan: &Plan,
    request: &BuildSpecification,
    store: &Store,
    root: &RootName,
    toolchain: &Path,
    worker: &Path,
    cancellation: &BuildCancellation,
) -> Result<BuildResult, BuildError> {
    request.validate()?;
    cancellation.check()?;
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        linux::build(
            plan,
            request,
            store,
            Some(root),
            toolchain,
            worker,
            cancellation,
        )
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = (plan, request, store, root, toolchain, worker);
        Err(BuildError::UnsupportedPlatform)
    }
}

pub(crate) fn preflight(toolchain: &Path) -> Result<(), BuildError> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        if toolchain != Path::new("/usr") {
            return Err(BuildError::Toolchain);
        }
        crate::linux_fd::require_build_landlock()?;
        sandbox::preflight()
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = toolchain;
        Err(BuildError::UnsupportedPlatform)
    }
}

/// Check whether this host can start a new build with the configured bootstrap
/// toolchain. Existing cached results can remain usable when this check fails.
pub fn check_build_host(toolchain: &Path) -> Result<(), BuildError> {
    preflight(toolchain)
}

#[cfg_attr(all(target_os = "linux", target_arch = "x86_64"), allow(dead_code))]
pub(crate) fn build_managed(
    plan: &Plan,
    request: &BuildSpecification,
    store: &Store,
    toolchain: &Path,
    worker: &Path,
    cancellation: &BuildCancellation,
) -> Result<BuildResult, BuildError> {
    request.validate()?;
    cancellation.check()?;
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        linux::build(plan, request, store, None, toolchain, worker, cancellation)
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = (plan, request, store, toolchain, worker);
        Err(BuildError::UnsupportedPlatform)
    }
}

/// Internal subprocess entry; requires the sandbox's private mount layout.
/// Calling this function does not confer build or Store publication authority.
pub fn run_build_worker(source_directory: &str, action: &str) -> Result<(), BuildError> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        worker::run(source_directory, action)
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = (source_directory, action);
        Err(BuildError::UnsupportedPlatform)
    }
}

/// Internal trusted service entry. The persistent launch authorization is checked
/// under its own lock before any payload is started. This is not a recipe API.
pub fn run_build_gate(
    operation: &Path,
    source_directory: &str,
    blocked: &[PathBuf],
) -> Result<(), BuildError> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        sandbox::run_gate(operation, source_directory, blocked)
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = (operation, source_directory, blocked);
        Err(BuildError::UnsupportedPlatform)
    }
}
