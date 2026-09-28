//! Public reference resolution and the first recipe-owned realization flow.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use thiserror::Error;

use crate::{
    BuildCancellation, BuildError, BuildResult, CheckConfiguration, ContentDigest, HttpsError,
    HttpsSourceRequest, HttpsTransportPolicy, Plan, PlanBuild, ProjectOperationError, RootName,
    Store, StoreError, UserConfiguration, acquire_https_with_cancellation, plan_project_with,
};

#[cfg(target_os = "linux")]
mod apps;
#[cfg(target_os = "linux")]
pub use apps::{ApplicationError, ResolvedApplication, realize_application, resolve_application};

#[derive(Debug)]
pub struct ResolvedBuild {
    project: PathBuf,
    export: String,
    lock_digest: String,
    plan: Arc<Plan>,
    build: PlanBuild,
}

impl ResolvedBuild {
    pub fn project(&self) -> &Path {
        &self.project
    }
    pub fn export(&self) -> &str {
        &self.export
    }
    pub fn lock_digest(&self) -> &str {
        &self.lock_digest
    }
    pub const fn description(&self) -> &PlanBuild {
        &self.build
    }
    pub fn backend(&self) -> &'static str {
        if self.build.is_glibc() {
            crate::build::GLIBC_PROFILE
        } else {
            crate::build::PROFILE
        }
    }

    /// A provider is part of the selected build only when an application
    /// explicitly declares it. The host protocol currently binds one shared
    /// loader/libc output; other layouts fail before any effects.
    pub fn runtime_provider(&self) -> Option<&str> {
        self.plan
            .applications()
            .find(|app| app.package() == self.build.package())
            .and_then(|app| app.loader().map(crate::PlanPackageId::as_str))
    }

    #[cfg(target_os = "linux")]
    fn for_package(&self, id: &crate::PlanPackageId) -> Self {
        let package = self
            .plan
            .packages()
            .find(|package| package.id() == id)
            .expect("validated application provider package");
        let build = self
            .plan
            .builds()
            .find(|build| build.package() == id)
            .expect("validated application provider build");
        Self {
            project: self.project.clone(),
            export: package.export().unwrap_or(id.as_str()).to_owned(),
            lock_digest: self.lock_digest.clone(),
            plan: self.plan.clone(),
            build: build.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum BuildProgress {
    PreparingStore,
    CheckingBackend,
    SourceCacheHit,
    AcquiringSource,
    Building,
}

#[derive(Debug, Error)]
pub enum RealizeError {
    #[error(
        "invalid build reference; use a catalog export, .#export, ./path#export or a project path"
    )]
    InvalidReference,
    #[error(
        "no default catalog configured; set [catalog] path and lock-sha256 in the user configuration, or use .#export"
    )]
    MissingCatalog,
    #[error(
        "configured catalog lock has changed; inspect the checkout and explicitly update catalog.lock-sha256"
    )]
    CatalogDrift,
    #[error("project has no std::DefaultBuild; select an export explicitly")]
    MissingDefault,
    #[error("no public buildable package export `{0}` in this project")]
    MissingExport(String),
    #[error(
        "host bootstrap is not configured; set [build] host-toolchain = \"/usr\" in the user configuration"
    )]
    MissingToolchain,
    #[error("source {0} is absent in offline mode")]
    OfflineMissing(ContentDigest),
    #[error("source URL has no exact grant in network.allow-https: {0}")]
    SourceDenied(String),
    #[error("automatic acquisition supports HTTPS and pinned project-local sources only")]
    UnsupportedSource,
    #[error(
        "host Autotools runtime build requires no providers or one output supplying both loader and libc"
    )]
    UnsupportedRuntime,
    #[error(transparent)]
    Project(#[from] ProjectOperationError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Https(#[from] HttpsError),
    #[error(transparent)]
    Local(#[from] crate::LocalSourceError),
    #[error(transparent)]
    Build(#[from] BuildError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Resolve a public export against verified recipe bytes without touching the
/// Store, network or build host. A simple name always uses the pinned catalog.
pub fn resolve_build(
    reference: &str,
    user: &UserConfiguration,
    checks: &CheckConfiguration,
) -> Result<ResolvedBuild, RealizeError> {
    let (project, export, pin) = reference_parts(reference, user)?;
    let project = std::path::absolute(project)?;
    let plan = plan_project_with(&project, checks)?;
    if pin.is_some_and(|pin| pin.as_bytes() != plan.lock_digest()) {
        return Err(RealizeError::CatalogDrift);
    }
    let package = if let Some(export) = export {
        plan.packages()
            .find(|package| package.export() == Some(export))
            .ok_or_else(|| RealizeError::MissingExport(export.to_owned()))?
    } else {
        let id = plan.default_build().ok_or(RealizeError::MissingDefault)?;
        plan.packages()
            .find(|package| package.id() == id)
            .expect("validated default package")
    };
    let export = package
        .export()
        .expect("selected root package export")
        .to_owned();
    let build = plan
        .builds()
        .find(|build| build.package() == package.id())
        .ok_or_else(|| RealizeError::MissingExport(export.clone()))?
        .clone();
    let lock_digest = plan
        .lock_digest()
        .iter()
        .fold(String::new(), |mut text, byte| {
            write!(&mut text, "{byte:02x}").expect("String write is infallible");
            text
        });
    Ok(ResolvedBuild {
        project,
        export,
        lock_digest,
        plan: Arc::new(plan),
        build,
    })
}

/// Enumerate public buildable exports of one locked project or pinned catalog.
/// Like `resolve_build`, this is pure with respect to Store and build host.
pub fn build_exports(
    reference: &str,
    user: &UserConfiguration,
    checks: &CheckConfiguration,
) -> Result<Vec<String>, RealizeError> {
    Ok(resolve_builds(reference, user, checks)?
        .into_iter()
        .map(|build| build.export)
        .collect())
}

/// Resolve all public builds from one verified project snapshot. In particular,
/// `build -A` must not reload and reevaluate the project for every export.
pub fn resolve_builds(
    reference: &str,
    user: &UserConfiguration,
    checks: &CheckConfiguration,
) -> Result<Vec<ResolvedBuild>, RealizeError> {
    let (project, export, pin) = reference_parts(reference, user)?;
    if export.is_some() {
        return Err(RealizeError::InvalidReference);
    }
    let project = std::path::absolute(project)?;
    let plan = plan_project_with(&project, checks)?;
    if pin.is_some_and(|pin| pin.as_bytes() != plan.lock_digest()) {
        return Err(RealizeError::CatalogDrift);
    }
    let lock_digest = plan
        .lock_digest()
        .iter()
        .fold(String::new(), |mut text, byte| {
            write!(&mut text, "{byte:02x}").expect("String write is infallible");
            text
        });
    let plan = Arc::new(plan);
    let mut builds = plan
        .packages()
        .filter_map(|package| {
            let export = package.export()?;
            let build = plan
                .builds()
                .find(|build| build.package() == package.id())?;
            Some(ResolvedBuild {
                project: project.clone(),
                export: export.to_owned(),
                lock_digest: lock_digest.clone(),
                plan: plan.clone(),
                build: build.clone(),
            })
        })
        .collect::<Vec<_>>();
    builds.sort_by(|left, right| left.export.cmp(&right.export));
    Ok(builds)
}

/// Search only the configured, pinned local catalog. No registry or network lookup.
pub fn search_build_exports(
    query: &str,
    user: &UserConfiguration,
    checks: &CheckConfiguration,
) -> Result<Vec<String>, RealizeError> {
    if query.len() > 255 || !query.is_ascii() {
        return Err(RealizeError::InvalidReference);
    }
    let catalog = user.catalog().ok_or(RealizeError::MissingCatalog)?;
    let plan = plan_project_with(catalog.path(), checks)?;
    if catalog.lock_digest().as_bytes() != plan.lock_digest() {
        return Err(RealizeError::CatalogDrift);
    }
    let query = query.to_ascii_lowercase();
    let mut names = plan
        .packages()
        .filter(|package| plan.builds().any(|build| build.package() == package.id()))
        .filter_map(|package| {
            package
                .export()
                .filter(|export| export.to_ascii_lowercase().contains(&query))
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    names.sort();
    Ok(names)
}

type ReferenceParts<'a> = (&'a Path, Option<&'a str>, Option<ContentDigest>);

fn reference_parts<'a>(
    reference: &'a str,
    user: &'a UserConfiguration,
) -> Result<ReferenceParts<'a>, RealizeError> {
    if reference.is_empty() || reference.len() > 4096 {
        return Err(RealizeError::InvalidReference);
    }
    if let Some((path, export)) = reference.split_once('#') {
        if !is_project_path(path) || !valid_export(export) {
            return Err(RealizeError::InvalidReference);
        }
        return Ok((Path::new(path), Some(export), None));
    }
    if is_project_path(reference) {
        return Ok((Path::new(reference), None, None));
    }
    if !valid_export(reference) {
        return Err(RealizeError::InvalidReference);
    }
    let catalog = user.catalog().ok_or(RealizeError::MissingCatalog)?;
    Ok((catalog.path(), Some(reference), Some(catalog.lock_digest())))
}

fn is_project_path(text: &str) -> bool {
    matches!(text, "." | "..") || text.contains('/')
}
fn valid_export(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 255
        && text
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
}

/// Realize the already selected recipe. One operation lease spans source cache
/// lookup, acquisition and build publication; managed result roots are immutable
/// and content-addressed by receipt, not a caller-invented GC name.
pub fn realize_build(
    resolved: &ResolvedBuild,
    user: &UserConfiguration,
    worker: &Path,
    offline: bool,
    progress: impl FnMut(BuildProgress),
) -> Result<BuildResult, RealizeError> {
    realize_build_with_cancellation(
        resolved,
        user,
        worker,
        offline,
        &BuildCancellation::default(),
        progress,
    )
}

/// Cancellable realization. Source transport observes cancellation during its
/// bounded deadline. After admission, requests have
/// independent interests in a shared producer. The last cancellation stops its
/// payload through the service manager before cleanup or releasing retention.
pub fn realize_build_with_cancellation(
    resolved: &ResolvedBuild,
    user: &UserConfiguration,
    worker: &Path,
    offline: bool,
    cancellation: &BuildCancellation,
    mut progress: impl FnMut(BuildProgress),
) -> Result<BuildResult, RealizeError> {
    #[cfg(target_os = "linux")]
    if resolved
        .plan
        .applications()
        .any(|app| app.package() == resolved.build.package())
    {
        return Ok(realize_declared_application(
            resolved,
            user,
            worker,
            offline,
            cancellation,
            |_, stage| progress(stage),
        )?
        .result);
    }
    realize_build_with_runtime(
        resolved,
        user,
        worker,
        offline,
        cancellation,
        None,
        progress,
    )
}

/// Realize several exports with bounded invocation-wide admission. A completed
/// task immediately releases capacity to the next export; results and failures
/// are selected in the original export order after every admitted task settles.
pub fn realize_builds_with_cancellation(
    builds: &[ResolvedBuild],
    user: &UserConfiguration,
    worker: &Path,
    offline: bool,
    jobs: usize,
    cancellation: &BuildCancellation,
) -> Result<Vec<BuildResult>, RealizeError> {
    run_bounded(builds.len(), jobs.min(8), |index| {
        realize_build_with_cancellation(&builds[index], user, worker, offline, cancellation, |_| {})
    })
}

fn run_bounded<T: Send, E: Send + From<std::io::Error>>(
    count: usize,
    jobs: usize,
    work: impl Fn(usize) -> Result<T, E> + Sync,
) -> Result<Vec<T>, E> {
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let completed = Mutex::new(Vec::with_capacity(count));
    std::thread::scope(|scope| -> Result<(), E> {
        let mut handles = Vec::new();
        for _ in 0..count.min(jobs.max(1)) {
            let handle = std::thread::Builder::new().spawn_scoped(scope, || {
                while !failed.load(Ordering::Acquire) {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= count {
                        break;
                    }
                    let result = work(index);
                    if result.is_err() {
                        failed.store(true, Ordering::Release);
                    }
                    completed
                        .lock()
                        .expect("build result queue poisoned")
                        .push((index, result));
                }
            });
            match handle {
                Ok(handle) => handles.push(handle),
                Err(error) => {
                    failed.store(true, Ordering::Release);
                    for handle in handles {
                        let _ = handle.join();
                    }
                    return Err(E::from(error));
                }
            }
        }
        let mut panicked = false;
        for handle in handles {
            panicked |= handle.join().is_err();
        }
        if panicked {
            return Err(E::from(std::io::Error::other("build worker panicked")));
        }
        Ok(())
    })?;
    let mut results = completed.into_inner().expect("build result queue poisoned");
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, result)| result).collect()
}

#[cfg(target_os = "linux")]
pub(crate) struct RealizedApplication {
    pub result: BuildResult,
    pub loader: Option<crate::RuntimeOutput>,
    pub libraries: Vec<crate::RuntimeOutput>,
}

#[cfg(target_os = "linux")]
pub(crate) fn realize_declared_application(
    resolved: &ResolvedBuild,
    user: &UserConfiguration,
    worker: &Path,
    offline: bool,
    cancellation: &BuildCancellation,
    mut progress: impl FnMut(&crate::PlanPackageId, BuildProgress),
) -> Result<RealizedApplication, RealizeError> {
    let application = resolved
        .plan
        .applications()
        .find(|app| app.package() == resolved.build.package())
        .expect("selected declared application");
    // Validate the supported shape before acquiring or building any output.
    let provider = match (
        application.loader(),
        application.libraries().collect::<Vec<_>>().as_slice(),
    ) {
        (None, []) => None,
        (Some(loader), [library]) if loader == *library => Some(loader),
        _ => return Err(RealizeError::UnsupportedRuntime),
    };
    if let Some(id) = provider {
        cancellation.check()?;
        let toolchain = user
            .host_toolchain()
            .ok_or(RealizeError::MissingToolchain)?;
        let store = Store::initialize(user.store())?;
        if let Some((result, provider_receipt)) = crate::build::retained_application(
            &resolved.plan,
            &resolved.build,
            id,
            &store,
            toolchain,
            worker,
            cancellation,
        )? {
            progress(application.package(), BuildProgress::SourceCacheHit);
            let provider = crate::RuntimeOutput {
                root: RootName::new(format!("build_{provider_receipt}"))
                    .expect("digest-derived root"),
                receipt: provider_receipt,
            };
            return Ok(RealizedApplication {
                result,
                loader: Some(provider.clone()),
                libraries: vec![provider],
            });
        }
    }
    let output = if let Some(id) = provider {
        let provider_build = resolved.for_package(id);
        let result = realize_build_with_runtime(
            &provider_build,
            user,
            worker,
            offline,
            cancellation,
            None,
            |stage| progress(id, stage),
        )?;
        Some(crate::RuntimeOutput {
            root: result.root,
            receipt: result.receipt,
        })
    } else {
        None
    };
    let development = match (resolved.build.development(), output.as_ref()) {
        (None, _) => false,
        (Some(provider), Some(_)) if Some(provider) == application.loader() => true,
        _ => return Err(RealizeError::UnsupportedRuntime),
    };
    let provider = output.as_ref().map(|output| crate::BuildProvider {
        root: output.root.clone(),
        receipt: output.receipt,
        development,
    });
    let result = realize_build_with_runtime(
        resolved,
        user,
        worker,
        offline,
        cancellation,
        provider,
        |stage| progress(application.package(), stage),
    )?;
    Ok(RealizedApplication {
        result,
        loader: output.clone(),
        libraries: output.into_iter().collect(),
    })
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub(crate) fn realize_build_with_runtime(
    resolved: &ResolvedBuild,
    user: &UserConfiguration,
    worker: &Path,
    offline: bool,
    cancellation: &BuildCancellation,
    provider: Option<crate::BuildProvider>,
    mut progress: impl FnMut(BuildProgress),
) -> Result<BuildResult, RealizeError> {
    #[cfg(target_os = "linux")]
    let mut phases = crate::build::profile::PhaseProfiler::new();
    cancellation.check()?;
    let toolchain = user
        .host_toolchain()
        .ok_or(RealizeError::MissingToolchain)?;
    progress(BuildProgress::PreparingStore);
    cancellation.check()?;
    let store = Store::initialize(user.store())?;
    let lease = store.operation()?;
    #[cfg(target_os = "linux")]
    phases.mark("store-open");
    let specification = crate::BuildSpecification {
        mode: if resolved.build.is_glibc() {
            crate::BuildMode::Glibc
        } else {
            crate::BuildMode::Autotools { provider }
        },
        ..resolved.build.request()
    };
    // An admitted result already retains and verifies the locked source. On a
    // miss, keep the prepared Action (including worker and inventory bytes)
    // across acquisition so the backend never reinvents a different identity.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    let prepared = if lease.managed_build_roots()?.is_empty() {
        // A new Store cannot contain an admitted build result. Keep the cold
        // acquisition's previous early offline/permission checks and inventory
        // the toolchain only once, after the source has arrived.
        phases.mark("pre-source-empty-store");
        None
    } else {
        match crate::build::prepare_managed(
            &resolved.plan,
            &specification,
            &store,
            toolchain,
            worker,
            cancellation,
        )? {
            crate::build::PreparedLookup::Hit(result) => {
                phases.mark("pre-source-action-hit");
                progress(BuildProgress::Building);
                return Ok(result);
            }
            crate::build::PreparedLookup::Miss(prepared) => {
                phases.mark("pre-source-action-miss");
                Some(prepared)
            }
        }
    };
    let source = resolved
        .plan
        .acquisitions()
        .find(|acquisition| acquisition.package() == resolved.build.package())
        .and_then(|acquisition| acquisition.sources().next())
        .expect("validated build source");
    let maximum_source = if resolved.build.is_glibc() {
        crate::build::MAX_LARGE_SOURCE_BYTES
    } else {
        crate::build::MAX_SOURCE_BYTES
    };
    // Integrity failures are errors, never a trigger for silent replacement.
    #[cfg(target_os = "linux")]
    let cached = source_cached(
        &lease,
        source.digest(),
        source.maximum_bytes().min(maximum_source),
        cancellation,
    )?;
    #[cfg(not(target_os = "linux"))]
    let cached = lease
        .read_verified(source.digest(), source.maximum_bytes().min(maximum_source))?
        .is_some();
    #[cfg(target_os = "linux")]
    phases.mark("source-lookup");
    if !cached {
        // Offline forbids transport, not reading a digest-pinned file beneath
        // this project's explicitly authorized directory.
        if offline && source.url().starts_with("https://") {
            return Err(RealizeError::OfflineMissing(source.digest()));
        }
        if !source.url().starts_with("https://") && !source.url().starts_with("project:") {
            return Err(RealizeError::UnsupportedSource);
        }
        if source.url().starts_with("https://") && !user.permits_https(source.url()) {
            return Err(RealizeError::SourceDenied(source.url().to_owned()));
        }
    }
    if cached {
        progress(BuildProgress::SourceCacheHit);
    } else {
        // Keep the pre-download capability check. A fully local result may be
        // reused without a live service manager; misses check again before launch.
        progress(BuildProgress::CheckingBackend);
        cancellation.check()?;
        if source.url().starts_with("https://") {
            crate::build::preflight(toolchain)?;
        }
        cancellation.check()?;
        progress(BuildProgress::AcquiringSource);
        cancellation.check()?;
        let root = RootName::new(format!("source_{}", source.digest()))
            .expect("digest makes a bounded root");
        if let Some(relative) = source.url().strip_prefix("project:") {
            let origin = source
                .owner()
                .and_then(|owner| resolved.plan.project_root(owner))
                .ok_or(RealizeError::UnsupportedSource)?;
            acquire_build_local(
                &store,
                origin,
                relative,
                source.digest(),
                source.maximum_bytes().min(maximum_source),
                &root,
                cancellation,
            )?;
        } else {
            acquire_build_https(
                &store,
                source.url(),
                source.digest(),
                source.maximum_bytes().min(maximum_source),
                &root,
                cancellation,
            )?;
        }
    }
    #[cfg(target_os = "linux")]
    phases.mark("source-acquisition");
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if !cached && let Some(ref prepared) = prepared {
        prepared.revalidate_after_acquisition(toolchain, cancellation)?;
        phases.mark("post-acquisition-inventory-check");
    }
    progress(BuildProgress::Building);
    cancellation.check()?;
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    let result = if let Some(prepared) = prepared {
        crate::build::build_prepared(*prepared, &specification, &store, toolchain, cancellation)?
    } else {
        crate::build::build_managed(
            &resolved.plan,
            &specification,
            &store,
            toolchain,
            worker,
            cancellation,
        )?
    };
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    let result = crate::build::build_managed(
        &resolved.plan,
        &specification,
        &store,
        toolchain,
        worker,
        cancellation,
    )?;
    #[cfg(target_os = "linux")]
    phases.mark("build-realization");
    drop(lease);
    Ok(result)
}

pub(crate) fn acquire_build_local(
    store: &Store,
    project_path: &Path,
    relative: &str,
    digest: ContentDigest,
    maximum_bytes: u64,
    root: &RootName,
    cancellation: &BuildCancellation,
) -> Result<(), RealizeError> {
    let absolute = std::path::absolute(project_path)?;
    // The loader already admitted every component without following symlinks.
    // Remove lexical `..` left by a sibling locator before constructing the
    // absolute file URL; canonicalize() here would follow a newly added symlink.
    let mut project_path = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::ParentDir => {
                project_path.pop();
            }
            std::path::Component::CurDir => {}
            other => project_path.push(other.as_os_str()),
        }
    }
    let project = crate::AuthorizedLocalDirectory::open(&project_path)?;
    let path = project_path.join(relative);
    let url = url::Url::from_file_path(&path)
        .map_err(|()| RealizeError::UnsupportedSource)?
        .to_string();
    let request = crate::LocalSourceRequest::new(&url, digest, maximum_bytes)?;
    crate::acquire_local_with_cancellation(store, &project, &request, root, cancellation).map_err(
        |error| match error {
            crate::LocalSourceError::Cancelled => RealizeError::Build(BuildError::Cancelled),
            error => RealizeError::Local(error),
        },
    )?;
    Ok(())
}

fn acquire_build_https(
    store: &Store,
    url: &str,
    digest: ContentDigest,
    maximum_bytes: u64,
    root: &RootName,
    cancellation: &BuildCancellation,
) -> Result<(), RealizeError> {
    let request = HttpsSourceRequest::new(url, digest, maximum_bytes)?;
    acquire_https_with_cancellation(
        store,
        &request,
        root,
        HttpsTransportPolicy {
            maximum_redirects: 0,
            ..HttpsTransportPolicy::default()
        },
        cancellation,
    )
    .map_err(|error| match error {
        HttpsError::Cancelled => RealizeError::Build(BuildError::Cancelled),
        error => RealizeError::Https(error),
    })?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn source_cached(
    lease: &crate::store::OperationLease,
    digest: ContentDigest,
    maximum_bytes: u64,
    cancellation: &BuildCancellation,
) -> Result<bool, RealizeError> {
    lease
        .open_verified_checked(digest, maximum_bytes, || {
            if cancellation.is_cancelled() {
                Err(crate::build::cancellation_io())
            } else {
                Ok(())
            }
        })
        .map(|source| source.is_some())
        .map_err(|error| {
            if cancellation.is_cancelled()
                && matches!(error, StoreError::Io(ref source) if crate::build::is_cancellation_io(source))
            {
                RealizeError::Build(BuildError::Cancelled)
            } else {
                RealizeError::Store(error)
            }
        })
}

#[cfg(test)]
mod bounded_tests {
    use super::*;
    use std::sync::{atomic::AtomicUsize, mpsc};
    use std::time::Duration;

    #[test]
    fn capacity_is_replenished_without_a_batch_barrier() {
        let (release, waiting) = mpsc::channel();
        let waiting = Mutex::new(waiting);
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let result = run_bounded::<usize, std::io::Error>(3, 2, |index| {
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(current, Ordering::SeqCst);
            if index == 0 {
                waiting
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .map_err(std::io::Error::other)?;
            } else if index == 2 {
                release.send(()).unwrap();
            }
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(index)
        })
        .unwrap();
        assert_eq!(result, [0, 1, 2]);
        assert!(peak.load(Ordering::SeqCst) <= 2);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn failures_select_the_earliest_export_after_admitted_work_settles() {
        let gate = std::sync::Barrier::new(2);
        let next_started = AtomicBool::new(false);
        let result = run_bounded::<(), std::io::Error>(3, 2, |index| {
            if index == 2 {
                next_started.store(true, Ordering::SeqCst);
            }
            gate.wait();
            if index == 0 {
                std::thread::sleep(Duration::from_millis(30));
            }
            Err(std::io::Error::other(format!("failure {index}")))
        });
        assert_eq!(result.unwrap_err().to_string(), "failure 0");
        assert!(!next_started.load(Ordering::SeqCst));
    }
}
