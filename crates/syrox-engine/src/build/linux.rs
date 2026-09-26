use std::collections::BTreeMap;
use std::fs;
use std::io::Read as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::time::Duration;

use sha2::{Digest as _, Sha256};

use super::{
    BuildCancellation, BuildError, BuildExecution, BuildProtocol, BuildResult, BuildSpecification,
    MAX_LARGE_SOURCE_BYTES, MAX_OUTPUT_BYTES, MAX_SOURCE_BYTES, artifact, cache, group,
    journal::Operation,
    records::{Action, ProviderIdentity, Receipt},
    sandbox, shared,
};
use crate::{ContentDigest, Plan, RootName, Store};

const MAX_TOOLCHAIN_FILES: usize = 262_144;
const MAX_TOOLCHAIN_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_INVENTORY_BYTES: usize = 32 * 1024 * 1024;
const INVENTORY_BATCH: usize = 1024;
const INVENTORY_WORKERS: usize = 4;
const INVENTORY_TASKS_PER_WORKER: usize = 8;

static INVENTORY_CAPACITY: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();

struct InventoryPermits {
    capacity: &'static (Mutex<usize>, Condvar),
    count: usize,
}

impl InventoryPermits {
    fn acquire(requested: usize, cancellation: &BuildCancellation) -> Result<Self, BuildError> {
        let capacity =
            INVENTORY_CAPACITY.get_or_init(|| (Mutex::new(INVENTORY_WORKERS), Condvar::new()));
        let mut available = capacity.0.lock().expect("inventory capacity poisoned");
        while *available == 0 {
            cancellation.check()?;
            available = capacity
                .1
                .wait_timeout(available, Duration::from_millis(50))
                .expect("inventory capacity poisoned")
                .0;
        }
        cancellation.check()?;
        let count = (*available).min(requested);
        *available -= count;
        Ok(Self { capacity, count })
    }
}

impl Drop for InventoryPermits {
    fn drop(&mut self) {
        let mut available = self.capacity.0.lock().expect("inventory capacity poisoned");
        *available += self.count;
        self.capacity.1.notify_all();
    }
}

pub(crate) fn retained_application(
    plan: &Plan,
    build: &crate::PlanBuild,
    provider: &crate::PlanPackageId,
    store: &Store,
    toolchain: &Path,
    worker: &Path,
    cancellation: &BuildCancellation,
) -> Result<Option<(BuildResult, ContentDigest)>, BuildError> {
    let mut phases = super::profile::PhaseProfiler::new();
    let source = plan
        .acquisitions()
        .find(|acquisition| acquisition.package() == build.package())
        .and_then(|acquisition| acquisition.sources().next())
        .ok_or(BuildError::UnsupportedInputs)?;
    let provider_source = plan
        .acquisitions()
        .find(|acquisition| acquisition.package() == provider)
        .and_then(|acquisition| acquisition.sources().next())
        .ok_or(BuildError::UnsupportedInputs)?;
    let provider_build = plan
        .builds()
        .find(|candidate| candidate.package() == provider)
        .ok_or(BuildError::UnsupportedInputs)?;
    let lease = store.operation()?;
    if !cache::has_retained_application(
        &lease,
        plan.lock_digest(),
        build.package().as_str(),
        cancellation,
    )? {
        phases.mark("application-retention-miss");
        return Ok(None);
    }
    phases.mark("application-retention-scan");
    let (inventory, _) = toolchain_inventory(toolchain, cancellation)?;
    phases.mark("application-toolchain-inventory");
    let worker = read_worker(worker, cancellation)?;
    phases.mark("application-worker-identity");
    let query = cache::RetainedQuery {
        lock: plan.lock_digest(),
        package: build.package().as_str(),
        provider: provider.as_str(),
        provider_source: provider_source.digest(),
        source: source.digest(),
        worker: ContentDigest::sha256(&worker),
        toolchain: ContentDigest::sha256(&inventory),
        entry: build.entry(),
        directory: build.source_directory(),
        deadline: build.timeout_seconds(),
        needs_development: build.development().is_some(),
        provider_protocol: if provider_build.is_glibc() {
            BuildProtocol::Glibc
        } else {
            BuildProtocol::Autotools
        },
        provider_directory: provider_build.source_directory(),
        provider_entry: provider_build.entry(),
        provider_deadline: provider_build.timeout_seconds(),
    };
    let result = cache::retained_application(&lease, &query, cancellation);
    phases.mark("application-lookup");
    result
}

/// Owned identity survives source acquisition; the worker bytes and toolchain
/// inventory used to compute this Action are the same inputs used on a miss.
pub(crate) struct PreparedBuild {
    description: Action,
    inventory: Vec<u8>,
    blocked: Vec<PathBuf>,
    worker_bytes: Vec<u8>,
    runtime: Option<ProviderResult>,
    retained_inputs: Vec<ContentDigest>,
    source: ContentDigest,
    maximum_source: u64,
}

impl PreparedBuild {
    /// An HTTPS/local acquisition can outlive a mutation of the host `/usr`.
    /// Do not accept a result for the old Action or launch with stale mounts.
    pub(crate) fn revalidate_after_acquisition(
        &self,
        toolchain: &Path,
        cancellation: &BuildCancellation,
    ) -> Result<(), BuildError> {
        let (inventory, blocked) = toolchain_inventory(toolchain, cancellation)?;
        if inventory != self.inventory || blocked != self.blocked {
            return Err(BuildError::InputChanged);
        }
        Ok(())
    }
}

pub(crate) enum PreparedLookup {
    Hit(BuildResult),
    Miss(Box<PreparedBuild>),
}

/// Pure with respect to source acquisition: validate the Plan and current
/// build identities, then ask the admitted action index before reading source
/// bytes. A miss retains the exact inputs for the subsequent producer.
pub(crate) fn prepare_managed(
    plan: &Plan,
    request: &BuildSpecification,
    store: &Store,
    toolchain: &Path,
    worker: &Path,
    cancellation: &BuildCancellation,
) -> Result<PreparedLookup, BuildError> {
    request.validate()?;
    prepare_action(plan, request, store, None, toolchain, worker, cancellation)
}

pub(super) fn build(
    plan: &Plan,
    request: &BuildSpecification,
    store: &Store,
    root: Option<&RootName>,
    toolchain: &Path,
    worker: &Path,
    cancellation: &BuildCancellation,
) -> Result<BuildResult, BuildError> {
    match prepare_action(plan, request, store, root, toolchain, worker, cancellation)? {
        PreparedLookup::Hit(result) => Ok(result),
        PreparedLookup::Miss(prepared) => {
            build_prepared_with_root(*prepared, request, store, root, toolchain, cancellation)
        }
    }
}

#[allow(clippy::too_many_lines)]
fn prepare_action(
    plan: &Plan,
    request: &BuildSpecification,
    store: &Store,
    root: Option<&RootName>,
    toolchain: &Path,
    worker: &Path,
    cancellation: &BuildCancellation,
) -> Result<PreparedLookup, BuildError> {
    let mut phases = super::profile::PhaseProfiler::new();
    cancellation.check()?;
    if (toolchain != Path::new("/usr") && !cfg!(test)) || cfg!(not(target_arch = "x86_64")) {
        return Err(BuildError::Toolchain);
    }
    let package = plan
        .packages()
        .find(|p| p.id().as_str() == request.package)
        .ok_or(BuildError::UnsupportedInputs)?;
    let declared = plan
        .builds()
        .find(|build| build.package().as_str() == request.package)
        .ok_or(BuildError::UnsupportedInputs)?;
    let provider = plan
        .applications()
        .find(|app| app.package() == declared.package())
        .and_then(|app| app.loader());
    if declared.source_directory() != request.source_directory
        || declared.entry() != request.entry
        || declared.timeout_seconds() != request.timeout_seconds
        || (declared.is_glibc() != (request.protocol() == BuildProtocol::Glibc))
        || provider.is_some() != request.provider().is_some()
        || declared.development().is_some()
            != request.provider().is_some_and(|input| input.development)
        || declared.development() != provider.filter(|_| declared.development().is_some())
    {
        return Err(BuildError::UnsupportedInputs);
    }
    let acquisition = plan
        .acquisitions()
        .find(|a| a.package().as_str() == request.package)
        .ok_or(BuildError::UnsupportedInputs)?;
    if package.dependencies().len() != 0 || acquisition.sources().len() != 1 {
        return Err(BuildError::UnsupportedInputs);
    }
    let source = acquisition.sources().next().unwrap();
    let source_digest = source.digest();
    let lease = store.operation()?;
    let maximum_source = if request.protocol() == BuildProtocol::Glibc {
        MAX_LARGE_SOURCE_BYTES
    } else {
        MAX_SOURCE_BYTES
    };
    // The Action is determined by the locked source digest, not by opening
    // source bytes. A retained hit validates the source through its receipt;
    // only a miss has to read the source for inspection and the payload.
    let (inventory, blocked) = toolchain_inventory(toolchain, cancellation)?;
    phases.mark("toolchain-inventory");
    let toolchain_digest = ContentDigest::sha256(&inventory);
    let worker_bytes = read_worker(worker, cancellation)?;
    let worker_digest = ContentDigest::sha256(&worker_bytes);
    phases.mark("worker-identity");
    let runtime = request
        .provider()
        .map(|input| {
            runtime_input(
                plan,
                request,
                &lease,
                input,
                worker_digest,
                toolchain_digest,
                cancellation,
            )
        })
        .transpose()?;
    let retained_inputs = runtime
        .as_ref()
        .map_or_else(Vec::new, |provider| provider.references.clone());
    phases.mark("provider-identity");
    let description = Action::new(
        plan.lock_digest(),
        request,
        source_digest,
        toolchain_digest,
        worker_digest,
        runtime.as_ref().and_then(|provider| {
            provider.development.map(|artifact| {
                (
                    request.provider().expect("provider exists").receipt,
                    artifact,
                )
            })
        }),
    );
    let hit = cache::lookup(&lease, &description, root, cancellation)?;
    phases.mark("action-lookup");
    if let Some(result) = hit {
        return Ok(PreparedLookup::Hit(result));
    }
    Ok(PreparedLookup::Miss(Box::new(PreparedBuild {
        description,
        inventory,
        blocked,
        worker_bytes,
        runtime,
        retained_inputs,
        source: source_digest,
        maximum_source: source.maximum_bytes().min(maximum_source),
    })))
}

pub(crate) fn build_prepared(
    prepared: PreparedBuild,
    request: &BuildSpecification,
    store: &Store,
    toolchain: &Path,
    cancellation: &BuildCancellation,
) -> Result<BuildResult, BuildError> {
    build_prepared_with_root(prepared, request, store, None, toolchain, cancellation)
}

#[allow(clippy::too_many_lines)]
fn build_prepared_with_root(
    prepared: PreparedBuild,
    request: &BuildSpecification,
    store: &Store,
    root: Option<&RootName>,
    toolchain: &Path,
    cancellation: &BuildCancellation,
) -> Result<BuildResult, BuildError> {
    let PreparedBuild {
        description,
        inventory,
        blocked,
        worker_bytes,
        runtime,
        retained_inputs,
        source,
        maximum_source,
    } = prepared;
    let mut phases = super::profile::PhaseProfiler::new();
    cancellation.check()?;
    let lease = store.operation()?;
    // A concurrent caller may have published this Action while the source was
    // acquired. Check the index again before any source read or build preflight.
    if let Some(result) = cache::lookup(&lease, &description, root, cancellation)? {
        phases.mark("post-acquisition-lookup-hit");
        return Ok(result);
    }
    phases.mark("post-acquisition-lookup-miss");
    let source_bytes = lease
        .read_verified(source, maximum_source)?
        .ok_or(BuildError::MissingSource)?;
    phases.mark("source-open");
    // A retained, fully verified result already attests to source inspection.
    // Only a miss needs to expand the pinned archive again.
    if request.protocol() == BuildProtocol::Glibc {
        crate::source_archive::inspect_archive_with_cancellation(
            store,
            source,
            crate::ArchiveFormat::XzTar,
            crate::ArchiveLimits::large_xz(),
            cancellation,
        )
        .map_err(|error| match error {
            crate::ArchiveError::Cancelled => BuildError::Cancelled,
            error => BuildError::Archive(error),
        })?;
    } else {
        crate::source_archive::inspect_gzip_tar_with_cancellation(
            source_bytes.as_bytes(),
            cancellation,
        )
        .map_err(|error| match error {
            crate::ArchiveError::Cancelled => BuildError::Cancelled,
            error => BuildError::Archive(error),
        })?;
    }
    phases.mark("source-inspection");
    let result = shared::realize(
        &lease,
        &description,
        root,
        cancellation,
        |operation, cancellation| {
            let action_bytes = description.encode();
            let action = ContentDigest::sha256(&action_bytes);
            let execution = (|| {
                super::preflight(toolchain)?;
                cancellation.check()?;
                let views = match (request.provider(), runtime.as_ref()) {
                    (Some(input), Some(provider)) => {
                        Some(stage_provider(store, input, provider, cancellation)?)
                    }
                    (None, None) => None,
                    _ => return Err(BuildError::UnsupportedInputs),
                };
                prepare(
                    operation,
                    &worker_bytes,
                    source_bytes.as_bytes(),
                    views
                        .as_ref()
                        .filter(|_| request.provider().is_some_and(|p| !p.development)),
                    cancellation,
                )?;
                operation.authorize_protocol(
                    action,
                    runtime.as_ref().map(|input| input.action),
                    request.protocol() == BuildProtocol::Glibc,
                    runtime.as_ref().and_then(|provider| {
                        provider.development.map(|dev| (provider.output, dev))
                    }),
                )?;
                sandbox::execute(operation, request, &blocked, cancellation)
            })();
            let mut output = finish_stage(operation, execution)?;
            cancellation.check()?;
            let (after_inventory, after_blocked) = toolchain_inventory(toolchain, cancellation)?;
            if after_inventory != inventory || after_blocked != blocked {
                return Err(BuildError::InputChanged);
            }
            publish_outputs(
                &lease,
                operation,
                &description,
                &inventory,
                &mut output.file,
                &retained_inputs,
                cancellation,
            )
        },
    );
    phases.mark("build-and-publication");
    result
}

fn publish_outputs(
    lease: &crate::OperationLease,
    operation: &Operation,
    action: &Action,
    inventory: &[u8],
    framed: &mut fs::File,
    retained_inputs: &[ContentDigest],
    cancellation: &BuildCancellation,
) -> Result<BuildResult, BuildError> {
    use std::collections::BTreeMap;
    let entries = action
        .output_names
        .iter()
        .map(|name| {
            (
                name.clone(),
                if name == "out" {
                    action.entry.clone()
                } else {
                    String::new()
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut outputs = group::unpack(framed, &entries, lease, cancellation)?;
    let primary = outputs.remove("out").ok_or(BuildError::Output)?;
    if primary.size > action.protocol.envelope().output_bytes {
        return Err(BuildError::Output);
    }
    let mut receipt_description = Receipt::new(action, primary.digest, primary.files);
    if !outputs.is_empty() {
        let named = outputs
            .iter()
            .map(|(name, output)| (name.clone(), group::named_output(output, "")))
            .collect();
        receipt_description = receipt_description.with_outputs(action, named)?;
    }
    if !retained_inputs.is_empty() {
        receipt_description = receipt_description.with_inputs(action, retained_inputs.to_vec())?;
    }
    let action_bytes = action.encode();
    let receipt_bytes = receipt_description.encode();
    let root = receipt_description.root();
    let objects = [
        (inventory, action.toolchain),
        (action_bytes.as_slice(), action.digest()),
        (receipt_bytes.as_slice(), receipt_description.digest()),
    ];
    let artifacts = std::iter::once(primary)
        .chain(outputs.into_values())
        .collect::<Vec<_>>();
    publish_result(
        lease,
        &root,
        &receipt_description.references(),
        &objects,
        artifacts,
        cancellation,
    )?;
    Ok(BuildResult {
        execution: BuildExecution::Built {
            operation: operation.name.clone(),
        },
        action: action.digest(),
        artifact: receipt_description
            .output("out")
            .expect("primary output")
            .artifact,
        receipt: receipt_description.digest(),
        toolchain: action.toolchain,
        files: receipt_description
            .output("out")
            .expect("primary output")
            .files,
        root,
    })
}

fn prepare(
    operation: &Operation,
    worker: &[u8],
    source: &[u8],
    runtime: Option<&RuntimeStage>,
    cancellation: &BuildCancellation,
) -> Result<(), BuildError> {
    cancellation.check()?;
    operation.write_stage("source", source, false)?;
    if let Some(runtime) = runtime {
        operation.write_stage("runtime-loader", &runtime.loader, true)?;
        operation.write_stage("runtime-libc", &runtime.libc, true)?;
    }
    cancellation.check()?;
    operation.write_stage("worker", worker, true)
}

struct RuntimeStage {
    loader: Vec<u8>,
    libc: Vec<u8>,
    _output_view: crate::MaterializedArtifact,
    _development_view: Option<crate::MaterializedArtifact>,
}

struct ProviderResult {
    action: ContentDigest,
    output: ContentDigest,
    development: Option<ContentDigest>,
    references: Vec<ContentDigest>,
}

fn runtime_input(
    plan: &Plan,
    request: &BuildSpecification,
    lease: &crate::OperationLease,
    input: &super::BuildProvider,
    worker: ContentDigest,
    toolchain: ContentDigest,
    cancellation: &BuildCancellation,
) -> Result<ProviderResult, BuildError> {
    let application = plan
        .applications()
        .find(|app| app.package().as_str() == request.package)
        .ok_or(BuildError::UnsupportedInputs)?;
    let provider = application.loader().ok_or(BuildError::UnsupportedInputs)?;
    if application.libraries().len() != 1 || application.libraries().next() != Some(provider) {
        return Err(BuildError::UnsupportedInputs);
    }
    let provider_build = plan
        .builds()
        .find(|build| build.package() == provider)
        .ok_or(BuildError::UnsupportedInputs)?;
    let provider_source = plan
        .acquisitions()
        .find(|acquisition| acquisition.package() == provider)
        .and_then(|acquisition| acquisition.sources().next())
        .ok_or(BuildError::UnsupportedInputs)?;
    let (action, receipt) =
        cache::retained_result(lease, &input.root, input.receipt, cancellation)?;
    if !(ProviderIdentity {
        lock: plan.lock_digest(),
        package: provider.as_str(),
        source: provider_source.digest(),
        worker,
        toolchain,
        protocol: if provider_build.is_glibc() {
            BuildProtocol::Glibc
        } else {
            BuildProtocol::Autotools
        },
        directory: provider_build.source_directory(),
        entry: provider_build.entry(),
        deadline: provider_build.timeout_seconds(),
    })
    .matches(&action)
    {
        return Err(BuildError::UnsupportedInputs);
    }
    let output = receipt.output("out").ok_or(BuildError::UnsupportedInputs)?;
    if output.entry != "usr/lib/ld-linux-x86-64.so.2" {
        return Err(BuildError::UnsupportedInputs);
    }
    let development = if input.development {
        Some(
            receipt
                .output("dev")
                .ok_or(BuildError::UnsupportedInputs)?
                .artifact,
        )
    } else {
        None
    };
    Ok(ProviderResult {
        action: action.digest(),
        output: output.artifact,
        development,
        references: receipt.references(),
    })
}

fn stage_provider(
    store: &Store,
    input: &super::BuildProvider,
    provider: &ProviderResult,
    cancellation: &BuildCancellation,
) -> Result<RuntimeStage, BuildError> {
    cancellation.check()?;
    let output_view = crate::build::materialize::materialize_with_cancellation(
        store,
        &input.root,
        input.receipt,
        cancellation,
    )
    .map_err(|error| materialization_error(&error))?;
    if output_view.action() != provider.action || output_view.artifact() != provider.output {
        return Err(BuildError::UnsupportedInputs);
    }
    let development_view = provider
        .development
        .map(|digest| {
            let view = crate::build::materialize::materialize_named_with_cancellation(
                store,
                &input.root,
                input.receipt,
                "dev",
                cancellation,
            )
            .map_err(|error| materialization_error(&error))?;
            if view.action() != provider.action || view.artifact() != digest {
                return Err(BuildError::UnsupportedInputs);
            }
            Ok(view)
        })
        .transpose()?;
    let (loader, libc) = if development_view.is_none() {
        let lease = store.operation()?;
        let source = lease
            .open_verified_checked(output_view.artifact(), MAX_OUTPUT_BYTES, || {
                if cancellation.is_cancelled() {
                    Err(super::cancellation_io())
                } else {
                    Ok(())
                }
            })
            .map_err(|error| {
                if cancellation.is_cancelled() {
                    BuildError::Cancelled
                } else {
                    error.into()
                }
            })?
            .ok_or(BuildError::UnsupportedInputs)?;
        let mut indexed =
            artifact::IndexedArtifact::open_checked(source, output_view.entry(), cancellation)?;
        let mut selected = Vec::new();
        let (files, input) = indexed.parts();
        for path in ["usr/lib/ld-linux-x86-64.so.2", "usr/lib/libc.so.6"] {
            cancellation.check()?;
            let file = files
                .find(path)
                .filter(|file| file.executable)
                .ok_or(BuildError::UnsupportedInputs)?;
            let bytes = artifact::IndexedArtifact::range_from(
                input,
                &file,
                0,
                usize::try_from(file.size).map_err(|_| BuildError::Output)?,
                cancellation,
            )
            .map_err(|error| {
                if cancellation.is_cancelled() {
                    BuildError::Cancelled
                } else {
                    BuildError::Io(error)
                }
            })?;
            selected.push(bytes);
        }
        (selected.remove(0), selected.remove(0))
    } else {
        (Vec::new(), Vec::new())
    };
    Ok(RuntimeStage {
        loader,
        libc,
        _output_view: output_view,
        _development_view: development_view,
    })
}

fn materialization_error(error: &crate::MaterializationError) -> BuildError {
    if matches!(error, crate::MaterializationError::Cancelled) {
        BuildError::Cancelled
    } else {
        BuildError::Cache(error.to_string())
    }
}

fn read_worker(worker: &Path, cancellation: &BuildCancellation) -> Result<Vec<u8>, BuildError> {
    // Capture exactly the trusted bytes whose digest participates in the action;
    // a miss stages these same owned bytes, not a later path reopen.
    cancellation.check()?;
    let mut bytes = Vec::new();
    fs::File::open(worker)?
        .take(128 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 128 * 1024 * 1024 {
        return Err(BuildError::InvalidRequest);
    }
    cancellation.check()?;
    Ok(bytes)
}

fn finish_stage<T>(operation: &Operation, outcome: Result<T, BuildError>) -> Result<T, BuildError> {
    if let Err(BuildError::Settlement {
        reason, primary, ..
    }) = outcome
    {
        return Err(BuildError::Settlement {
            stage: operation.stage_path(),
            reason,
            primary,
        });
    }
    // No processes can retain staging at this point. Even failures must perform
    // explicit cleanup; never turn a cleanup error into a successful build.
    if let Err(cleanup) = operation.clean_stage() {
        let cleanup = std::io::Error::other(format!("{}: {cleanup}", operation.path.display()));
        return Err(match outcome {
            Err(primary) => BuildError::FailureAndCleanup {
                primary: Box::new(primary),
                cleanup,
            },
            Ok(_) => BuildError::Cleanup(cleanup),
        });
    }
    outcome
}

fn publish_result(
    lease: &crate::OperationLease,
    root: &RootName,
    references: &[ContentDigest],
    objects: &[(&[u8], ContentDigest)],
    artifacts: Vec<group::GroupOutput>,
    cancellation: &BuildCancellation,
) -> Result<(), BuildError> {
    // Store objects may be orphaned on an intermediate publication failure;
    // only the final durable root denotes a complete retained result.
    for (bytes, digest) in objects {
        cancellation.check()?;
        lease.ingest(*bytes, *digest, bytes.len() as u64)?;
    }
    for output in artifacts {
        cancellation.check()?;
        output.admission.finish()?;
    }
    cancellation.check_publication()?;
    // Commit boundary: report the Store's actual publication outcome even if a
    // cancellation arrives while the durable root publication is in progress.
    lease.publish_root(root, references)?;
    Ok(())
}

/// Host-admin controlled /usr is observed before and after execution. This is
/// not an immutable snapshot or a bit-for-bit reproducibility claim. Every file,
/// link target and mode is recorded, not only the top-level compiler binary.
fn toolchain_inventory(
    root: &Path,
    cancellation: &BuildCancellation,
) -> Result<(Vec<u8>, Vec<PathBuf>), BuildError> {
    let available_workers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(INVENTORY_WORKERS);
    let permits = InventoryPermits::acquire(available_workers, cancellation)?;
    toolchain_inventory_with_workers(root, cancellation, permits.count)
}

fn toolchain_inventory_with_workers(
    root: &Path,
    cancellation: &BuildCancellation,
    workers: usize,
) -> Result<(Vec<u8>, Vec<PathBuf>), BuildError> {
    let mut paths = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            cancellation.check()?;
            if paths.len() >= MAX_TOOLCHAIN_FILES {
                return Err(BuildError::Toolchain);
            }
            let path = entry?.path();
            if path.components().count() > 64 {
                return Err(BuildError::Toolchain);
            }
            if fs::symlink_metadata(&path)?.is_dir() {
                pending.push(path.clone());
            }
            paths.push(path);
        }
    }
    paths.sort();
    let opened = crate::linux_fd::open_top_directory(root).map_err(|_| BuildError::Toolchain)?;
    let workers = workers.clamp(1, INVENTORY_WORKERS).min(paths.len().max(1));
    let opened = &opened;
    let paths = &paths;
    std::thread::scope(|scope| {
        let (jobs, incoming) = mpsc::sync_channel(workers * INVENTORY_TASKS_PER_WORKER);
        let incoming = Arc::new(Mutex::new(incoming));
        let (finished, replies) = mpsc::channel();
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let incoming = Arc::clone(&incoming);
            let finished = finished.clone();
            match std::thread::Builder::new().spawn_scoped(scope, move || {
                inventory_worker(root, opened, paths, cancellation, &incoming, &finished);
            }) {
                Ok(handle) => handles.push(handle),
                Err(error) => {
                    drop(jobs);
                    for handle in handles {
                        let _ = handle.join();
                    }
                    return Err(BuildError::Io(error));
                }
            }
        }
        drop(finished);
        // Only workers retain the receiving end. If all of them exit, a full
        // bounded queue must fail the send instead of blocking forever.
        drop(incoming);
        let result = serialize_inventory(root, paths, cancellation, workers, &jobs, &replies);
        drop(jobs);
        let mut panicked = false;
        for handle in handles {
            panicked |= handle.join().is_err();
        }
        if panicked {
            return Err(BuildError::Inventory("inventory worker panicked".into()));
        }
        result
    })
}

fn serialize_inventory(
    root: &Path,
    paths: &[PathBuf],
    cancellation: &BuildCancellation,
    workers: usize,
    jobs: &mpsc::SyncSender<InventoryJob>,
    replies: &mpsc::Receiver<InventoryReply>,
) -> Result<(Vec<u8>, Vec<PathBuf>), BuildError> {
    let mut bytes = b"syrox-host-toolchain\n".to_vec();
    let mut total = 0_u64;
    let mut blocked = Vec::new();
    let mut queue = InventoryQueue {
        workers,
        jobs,
        replies,
        hashes: Vec::with_capacity(INVENTORY_BATCH),
        errors: BTreeMap::new(),
    };
    let mut metadata = Vec::with_capacity(INVENTORY_BATCH);
    for (number, batch) in paths.chunks(INVENTORY_BATCH).enumerate() {
        let mut budget = total;
        metadata.clear();
        for path in batch {
            cancellation.check()?;
            let item = fs::symlink_metadata(path)?;
            if item.is_file() {
                budget = budget
                    .checked_add(item.len())
                    .filter(|size| *size <= MAX_TOOLCHAIN_BYTES)
                    .ok_or(BuildError::Toolchain)?;
            }
            metadata.push(item);
        }
        queue.hash_batch(number * INVENTORY_BATCH, &metadata, cancellation)?;
        for (index, (path, metadata)) in batch.iter().zip(&metadata).enumerate() {
            cancellation.check()?;
            let relative = path.strip_prefix(root).map_err(|_| BuildError::Toolchain)?;
            let name = relative.to_str().ok_or(BuildError::Toolchain)?;
            if name.len() > 4096 || bytes.len() + name.len() + 8192 > MAX_INVENTORY_BYTES {
                return Err(BuildError::Toolchain);
            }
            let link;
            let digest;
            let content = if metadata.is_symlink() {
                bytes.push(b'l');
                link = fs::read_link(path)?
                    .to_str()
                    .ok_or(BuildError::Toolchain)?
                    .as_bytes()
                    .to_vec();
                link.as_slice()
            } else if metadata.is_dir() {
                bytes.push(b'd');
                &[][..]
            } else if metadata.is_file() {
                bytes.push(b'f');
                total = total
                    .checked_add(metadata.len())
                    .filter(|n| *n <= MAX_TOOLCHAIN_BYTES)
                    .ok_or(BuildError::Toolchain)?;
                if let Some(error) = queue.errors.remove(&index) {
                    return Err(error);
                }
                let HashOutcome::Digest(value) =
                    queue.hashes[index].take().expect("file has a hash result")
                else {
                    // Excluded helpers cannot execute: the supervisor masks them.
                    blocked.push(path.clone());
                    bytes.extend_from_slice(b"blocked\0");
                    bytes.extend_from_slice(name.as_bytes());
                    bytes.push(0);
                    continue;
                };
                digest = value;
                digest.as_slice()
            } else {
                return Err(BuildError::Toolchain);
            };
            if name.len() > 4096 || content.len() > 4096 {
                return Err(BuildError::Toolchain);
            }
            for field in [
                name.as_bytes(),
                &metadata.mode().to_le_bytes(),
                &metadata.len().to_le_bytes(),
                content,
            ] {
                if bytes.len() + 8 + field.len() > MAX_INVENTORY_BYTES {
                    return Err(BuildError::Toolchain);
                }
                bytes.extend_from_slice(&(field.len() as u64).to_le_bytes());
                bytes.extend_from_slice(field);
            }
        }
    }
    Ok((bytes, blocked))
}

type InventoryHash = Result<Option<[u8; 32]>, BuildError>;

struct InventoryJob {
    files: Vec<(usize, u64)>,
}

struct InventoryReply {
    digests: Vec<(usize, HashOutcome)>,
    errors: Vec<(usize, BuildError)>,
}

enum HashOutcome {
    Digest([u8; 32]),
    Blocked,
}

struct InventoryQueue<'a> {
    workers: usize,
    jobs: &'a mpsc::SyncSender<InventoryJob>,
    replies: &'a mpsc::Receiver<InventoryReply>,
    hashes: Vec<Option<HashOutcome>>,
    errors: BTreeMap<usize, BuildError>,
}

fn inventory_worker(
    root: &Path,
    opened: &crate::linux_fd::OpenedPath,
    paths: &[PathBuf],
    cancellation: &BuildCancellation,
    incoming: &Mutex<mpsc::Receiver<InventoryJob>>,
    finished: &mpsc::Sender<InventoryReply>,
) {
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let job = incoming.lock().expect("inventory queue poisoned").recv();
        let Ok(job) = job else { break };
        let mut digests = Vec::with_capacity(job.files.len());
        let mut errors = Vec::new();
        for (index, size) in job.files {
            match hash_inventory_file(root, opened, &paths[index], size, &mut buffer, cancellation)
            {
                Ok(Some(digest)) => digests.push((index, HashOutcome::Digest(digest))),
                Ok(None) => digests.push((index, HashOutcome::Blocked)),
                Err(error) => errors.push((index, error)),
            }
        }
        if finished.send(InventoryReply { digests, errors }).is_err() {
            break;
        }
    }
}

impl InventoryQueue<'_> {
    fn hash_batch(
        &mut self,
        base: usize,
        metadata: &[fs::Metadata],
        cancellation: &BuildCancellation,
    ) -> Result<(), BuildError> {
        self.hashes.clear();
        self.hashes.resize_with(metadata.len(), || None);
        self.errors.clear();
        let chunk_size = metadata
            .len()
            .div_ceil(self.workers * INVENTORY_TASKS_PER_WORKER)
            .max(1);
        let mut submitted = 0;
        for (number, group) in metadata.chunks(chunk_size).enumerate() {
            cancellation.check()?;
            let files = group
                .iter()
                .enumerate()
                .filter(|(_, metadata)| metadata.is_file())
                .map(|(index, metadata)| (base + number * chunk_size + index, metadata.len()))
                .collect::<Vec<_>>();
            if !files.is_empty() {
                self.jobs
                    .send(InventoryJob { files })
                    .map_err(|_| BuildError::Inventory("inventory workers exited".into()))?;
                submitted += 1;
            }
        }
        for _ in 0..submitted {
            let reply = self
                .replies
                .recv()
                .map_err(|_| BuildError::Inventory("inventory workers exited".into()))?;
            for (index, digest) in reply.digests {
                self.hashes[index - base] = Some(digest);
            }
            for (index, error) in reply.errors {
                self.errors.insert(index - base, error);
            }
        }
        cancellation.check()?;
        Ok(())
    }
}

fn hash_inventory_file(
    root: &Path,
    opened: &crate::linux_fd::OpenedPath,
    path: &Path,
    size: u64,
    buffer: &mut [u8; 16 * 1024],
    cancellation: &BuildCancellation,
) -> InventoryHash {
    cancellation.check()?;
    let relative = path.strip_prefix(root).map_err(|_| BuildError::Toolchain)?;
    let file = match crate::linux_fd::open_beneath(opened.fd(), relative, false) {
        Ok(file) => file,
        Err(crate::linux_fd::OpenError::Other(error))
            if error.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            return Ok(None);
        }
        Err(error) => {
            return Err(BuildError::Inventory(format!(
                "{}: {error:?}",
                path.display()
            )));
        }
    };
    if file.metadata().file_type() != crate::linux_fd::FileType::Regular {
        return Err(BuildError::Toolchain);
    }
    hash_file(file.into_file(), size, buffer, cancellation).map(Some)
}

fn hash_file(
    file: fs::File,
    size: u64,
    buffer: &mut [u8; 16 * 1024],
    cancellation: &BuildCancellation,
) -> Result<[u8; 32], BuildError> {
    let mut reader = file.take(size + 1);
    let mut hash = Sha256::new();
    let mut count = 0_u64;
    loop {
        cancellation.check()?;
        let read = reader.read(buffer)?;
        if read == 0 {
            break;
        }
        count += read as u64;
        hash.update(&buffer[..read]);
    }
    if count != size {
        return Err(BuildError::InputChanged);
    }
    Ok(hash.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_action_hits_before_source_acquisition_and_detects_toolchain_changes() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let toolchain = dir.path().join("toolchain");
        let project = dir.path().join("project");
        fs::create_dir(&toolchain).unwrap();
        fs::create_dir(&project).unwrap();
        fs::write(toolchain.join("compiler"), b"one").unwrap();
        let worker = dir.path().join("worker");
        fs::write(&worker, b"worker bytes").unwrap();
        let source = b"retained bytes need not be an archive on a hit";
        let digest = ContentDigest::sha256(source);
        fs::write(project.join("main.srx"), format!(r#"outputs {{
            hello: std::Package = std::Package {{ id = "hello"; dependencies = []; }};
            source: std::Acquisition = std::Acquisition {{ package = "hello"; sources = [std::SourceRequest {{ url = "https://example.invalid/archive"; sha256 = "{digest}"; maximum_bytes = 1024; }}]; }};
            build: std::AutotoolsBuild = std::AutotoolsBuild {{ package = "hello"; source_directory = "hello-1"; entry = "usr/bin/hello"; timeout_seconds = 120; }};
        }}"#)).unwrap();
        let standard = crate::AuthenticatedStandardSource::from_authenticated(
            "std/pkg.srx",
            include_str!("../../../../std/pkg.srx"),
        )
        .unwrap();
        let checks = crate::CheckConfiguration {
            standard_library: Some(
                crate::AuthenticatedStandardLibrary::from_authenticated(vec![standard]).unwrap(),
            ),
            ..crate::CheckConfiguration::default()
        };
        crate::lock_project_with(&project, &checks).unwrap();
        let plan = crate::plan_project_with(&project, &checks).unwrap();
        let request = plan.builds().next().unwrap().request();
        let store = Store::initialize(&dir.path().join("store")).unwrap();
        let lease = store.operation().unwrap();
        let cancellation = BuildCancellation::default();
        let (inventory, _) = toolchain_inventory(&toolchain, &cancellation).unwrap();
        let action = Action::new(
            plan.lock_digest(),
            &request,
            digest,
            ContentDigest::sha256(&inventory),
            ContentDigest::sha256(b"worker bytes"),
            None,
        );
        let output = dir.path().join("output");
        fs::create_dir_all(output.join("usr/bin")).unwrap();
        let entry = output.join("usr/bin/hello");
        fs::write(&entry, b"executable").unwrap();
        fs::set_permissions(entry, fs::Permissions::from_mode(0o755)).unwrap();
        let mut artifact = Vec::new();
        artifact::pack(&output, &mut artifact).unwrap();
        let receipt = Receipt::new(&action, ContentDigest::sha256(&artifact), 1);
        for bytes in [
            &source[..],
            &inventory,
            &action.encode(),
            &artifact,
            &receipt.encode(),
        ] {
            lease
                .ingest(bytes, ContentDigest::sha256(bytes), bytes.len() as u64)
                .unwrap();
        }
        let root = receipt.root();
        lease.publish_root(&root, &receipt.references()).unwrap();
        cache::record(
            &lease,
            &action,
            &BuildResult {
                execution: BuildExecution::Cached,
                action: action.digest(),
                artifact: ContentDigest::sha256(&artifact),
                receipt: receipt.digest(),
                toolchain: action.toolchain,
                files: 1,
                root,
            },
        )
        .unwrap();
        assert!(
            matches!(prepare_managed(&plan, &request, &store, &toolchain, &worker, &cancellation).unwrap(),
            PreparedLookup::Hit(result) if result.receipt == receipt.digest())
        );
        fs::write(toolchain.join("compiler"), b"two").unwrap();
        let PreparedLookup::Miss(prepared) =
            prepare_managed(&plan, &request, &store, &toolchain, &worker, &cancellation).unwrap()
        else {
            panic!("changed toolchain must miss");
        };
        fs::write(toolchain.join("compiler"), b"three").unwrap();
        assert!(matches!(
            prepared.revalidate_after_acquisition(&toolchain, &cancellation),
            Err(BuildError::InputChanged)
        ));
    }

    #[test]
    fn parallel_inventory_keeps_canonical_order_across_batches() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..=INVENTORY_BATCH {
            fs::write(
                root.path().join(format!("file-{index:04}")),
                index.to_le_bytes(),
            )
            .unwrap();
        }
        std::os::unix::fs::symlink("file-0000", root.path().join("alias")).unwrap();
        let mut expected = b"syrox-host-toolchain\n".to_vec();
        for name in std::iter::once("alias".to_owned())
            .chain((0..=INVENTORY_BATCH).map(|index| format!("file-{index:04}")))
        {
            let path = root.path().join(&name);
            let metadata = fs::symlink_metadata(&path).unwrap();
            let content = if metadata.is_symlink() {
                expected.push(b'l');
                b"file-0000".to_vec()
            } else {
                expected.push(b'f');
                Sha256::digest(fs::read(path).unwrap()).to_vec()
            };
            for field in [
                name.as_bytes(),
                &metadata.mode().to_le_bytes(),
                &metadata.len().to_le_bytes(),
                &content,
            ] {
                expected.extend_from_slice(&(field.len() as u64).to_le_bytes());
                expected.extend_from_slice(field);
            }
        }
        for workers in [1, 2, 4] {
            let (actual, blocked) = toolchain_inventory_with_workers(
                root.path(),
                &BuildCancellation::default(),
                workers,
            )
            .unwrap();
            assert!(blocked.is_empty());
            assert_eq!(actual, expected, "{workers} inventory workers");
        }
    }

    #[test]
    fn inventory_byte_budget_is_checked_before_hashing_a_sparse_file() {
        let root = tempfile::tempdir().unwrap();
        fs::File::create(root.path().join("oversized"))
            .unwrap()
            .set_len(MAX_TOOLCHAIN_BYTES + 1)
            .unwrap();
        assert!(matches!(
            toolchain_inventory(root.path(), &BuildCancellation::default()),
            Err(BuildError::Toolchain)
        ));
    }

    #[test]
    fn concurrent_inventory_failures_are_indexed_in_canonical_order() {
        let root = tempfile::tempdir().unwrap();
        let paths = (0..64)
            .map(|index| root.path().join(format!("file-{index:03}")))
            .collect::<Vec<_>>();
        for path in &paths {
            fs::write(path, b"data").unwrap();
        }
        let metadata = paths
            .iter()
            .map(|path| fs::metadata(path).unwrap())
            .collect::<Vec<_>>();
        let opened = crate::linux_fd::open_top_directory(root.path()).unwrap();
        fs::remove_file(&paths[0]).unwrap();
        fs::remove_file(&paths[63]).unwrap();
        let cancellation = BuildCancellation::default();
        std::thread::scope(|scope| {
            let (jobs, incoming) = mpsc::sync_channel(32);
            let incoming = Arc::new(Mutex::new(incoming));
            let (finished, replies) = mpsc::channel();
            let handles = (0..4)
                .map(|_| {
                    let sender = finished.clone();
                    let incoming = Arc::clone(&incoming);
                    let paths = &paths;
                    let opened = &opened;
                    let cancellation = &cancellation;
                    let path = root.path();
                    scope.spawn(move || {
                        inventory_worker(path, opened, paths, cancellation, &incoming, &sender);
                    })
                })
                .collect::<Vec<_>>();
            drop(finished);
            let mut queue = InventoryQueue {
                workers: 4,
                jobs: &jobs,
                replies: &replies,
                hashes: Vec::new(),
                errors: BTreeMap::new(),
            };
            queue.hash_batch(0, &metadata, &cancellation).unwrap();
            assert_eq!(queue.errors.keys().copied().collect::<Vec<_>>(), [0, 63]);
            drop(jobs);
            for handle in handles {
                handle.join().unwrap();
            }
        });
    }

    #[test]
    #[ignore = "explicit /usr inventory profiling; reads the complete mutable host toolchain"]
    fn inventory_profile_usr() {
        let (inventory, blocked) =
            toolchain_inventory(Path::new("/usr"), &BuildCancellation::default()).unwrap();
        eprintln!(
            "inventory_sha256={} inventory_bytes={} blocked={}",
            ContentDigest::sha256(&inventory),
            inventory.len(),
            blocked.len()
        );
    }

    #[test]
    #[ignore = "set SYROX_BENCH_STORE, SYROX_BENCH_RECEIPT and SYROX_BENCH_SRX for a retained application"]
    fn retained_application_costs_are_measured_separately() {
        let store = Store::open(Path::new(&std::env::var("SYROX_BENCH_STORE").unwrap())).unwrap();
        let receipt: ContentDigest = std::env::var("SYROX_BENCH_RECEIPT")
            .unwrap()
            .parse()
            .unwrap();
        let worker = std::env::var("SYROX_BENCH_SRX").unwrap();
        let cancellation = BuildCancellation::default();
        let lease = store.operation().unwrap();
        let record = lease.read_verified(receipt, 4096).unwrap().unwrap();
        let receipt = Receipt::parse(record.as_bytes()).unwrap();
        let record = lease.read_verified(receipt.action, 4096).unwrap().unwrap();
        let action = Action::parse(record.as_bytes()).unwrap();
        let start = std::time::Instant::now();
        lease
            .read_verified(action.source, MAX_LARGE_SOURCE_BYTES)
            .unwrap()
            .unwrap();
        let source_time = start.elapsed();
        let start = std::time::Instant::now();
        let (inventory, _) = toolchain_inventory(Path::new("/usr"), &cancellation).unwrap();
        let inventory_time = start.elapsed();
        let start = std::time::Instant::now();
        let worker = read_worker(Path::new(&worker), &cancellation).unwrap();
        let worker_time = start.elapsed();
        assert_eq!(ContentDigest::sha256(&inventory), action.toolchain);
        assert_eq!(ContentDigest::sha256(&worker), action.worker);
        let start = std::time::Instant::now();
        let hit = cache::lookup(&lease, &action, None, &cancellation)
            .unwrap()
            .unwrap();
        let lookup_time = start.elapsed();
        let start = std::time::Instant::now();
        let view = crate::materialize_artifact(&store, &hit.root, hit.receipt).unwrap();
        let view_time = start.elapsed();
        eprintln!(
            "source={source_time:?} inventory={inventory_time:?} worker={worker_time:?} verified_lookup={lookup_time:?} view={view_time:?} reused_view={}",
            view.reused()
        );
    }

    #[test]
    fn publication_failure_and_root_conflict_never_report_a_completed_build() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let lease = store.operation().unwrap();
        let source = ContentDigest::sha256(b"source");
        lease.ingest(&b"source"[..], source, 6).unwrap();
        let root = RootName::new("result").unwrap();
        let object = (&b"artifact"[..], ContentDigest::sha256(b"artifact"));
        let mut references = vec![source, object.1];
        references.sort_unstable();
        crate::linux_fd::fail_next_atomic(crate::linux_fd::AtomicFault::Write);
        let cancellation = BuildCancellation::default();
        assert!(
            publish_result(
                &lease,
                &root,
                &references,
                &[object],
                Vec::new(),
                &cancellation
            )
            .is_err()
        );
        assert!(!directory.path().join("roots/retained/result").exists());
        publish_result(
            &lease,
            &root,
            &references,
            &[object],
            Vec::new(),
            &cancellation,
        )
        .unwrap();
        let before = fs::read(directory.path().join("roots/retained/result")).unwrap();
        let other = (&b"other"[..], ContentDigest::sha256(b"other"));
        let mut other_references = vec![source, other.1];
        other_references.sort_unstable();
        assert!(matches!(
            publish_result(
                &lease,
                &root,
                &other_references,
                &[other],
                Vec::new(),
                &cancellation,
            ),
            Err(BuildError::Store(crate::StoreError::RootConflict { .. }))
        ));
        assert_eq!(
            fs::read(directory.path().join("roots/retained/result")).unwrap(),
            before
        );
        cancellation.cancel();
        let cancelled = RootName::new("cancelled").unwrap();
        assert!(matches!(
            publish_result(
                &lease,
                &cancelled,
                &references,
                &[object],
                Vec::new(),
                &cancellation
            ),
            Err(BuildError::Cancelled)
        ));
        assert!(!directory.path().join("roots/retained/cancelled").exists());
    }

    #[test]
    fn cancelled_stage_is_cleaned_but_uncertain_settlement_retains_evidence() {
        let parent = tempfile::tempdir().unwrap();
        let store = Store::open(parent.path()).unwrap();
        let lease = store.operation().unwrap();
        let operation = Operation::create(&lease).unwrap();
        let path = operation.stage_path();
        operation.write_stage("source", b"input", false).unwrap();
        assert!(matches!(
            finish_stage::<()>(&operation, Err(BuildError::Cancelled)),
            Err(BuildError::Cancelled)
        ));
        assert!(!path.exists());

        let operation = Operation::create(&lease).unwrap();
        let path = operation.stage_path();
        operation.write_stage("source", b"input", false).unwrap();
        let error = BuildError::Settlement {
            stage: path.clone(),
            reason: "query failed",
            primary: Some(Box::new(BuildError::Cancelled)),
        };
        let error = finish_stage::<()>(&operation, Err(error)).unwrap_err();
        assert!(
            matches!(error, BuildError::Settlement { primary: Some(ref error), .. } if matches!(error.as_ref(), BuildError::Cancelled))
        );
        assert_eq!(fs::read(path.join("source")).unwrap(), b"input");
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn cleanup_failure_cannot_report_success_or_hide_cancellation() {
        for outcome in [Ok(()), Err(BuildError::Cancelled)] {
            let cancelled = outcome.is_err();
            let parent = tempfile::tempdir().unwrap();
            let store = Store::open(parent.path()).unwrap();
            let operation = Operation::create(&store.operation().unwrap()).unwrap();
            let path = operation.stage_path();
            // Make recursive directory removal fail without relying on uid or
            // permissions. The outer guard owns both paths even on assertion failure.
            fs::rename(&path, parent.path().join("retained")).unwrap();
            fs::write(&path, b"replacement").unwrap();
            let error = finish_stage(&operation, outcome).unwrap_err();
            if cancelled {
                assert!(
                    matches!(error, BuildError::FailureAndCleanup { primary, .. }
                    if matches!(*primary, BuildError::Cancelled))
                );
            } else {
                assert!(matches!(error, BuildError::Cleanup(_)));
            }
            assert_eq!(fs::read(path).unwrap(), b"replacement");
        }
    }
}
