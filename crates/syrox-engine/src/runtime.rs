//! Validated runtime closure and launcher for the Linux `x86_64` ELF subset.
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::Read as _;
use std::io::Write as _;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd};
use std::os::unix::net::UnixStream;
#[cfg(test)]
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
#[cfg(test)]
use std::process::Child;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::build::MAX_OUTPUT_BYTES;
use crate::build::artifact;
use crate::{
    ContentDigest, MaterializationError, MaterializedArtifact, RootName, Store, StoreError,
};

const MAX_LIBRARIES: usize = 64;

#[derive(Clone, Debug)]
pub struct RuntimeOutput {
    pub root: RootName,
    pub receipt: ContentDigest,
}

#[derive(Clone, Debug)]
pub struct RuntimeRequest {
    pub application: RuntimeOutput,
    /// The exact output containing the ELF interpreter, when `PT_INTERP` exists.
    pub loader: Option<RuntimeOutput>,
    /// Outputs supplying `DT_NEEDED` libraries, including transitive ones.
    pub libraries: Vec<RuntimeOutput>,
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("runtime cancelled")]
    Cancelled,
    #[error("invalid runtime closure: {0}")]
    Invalid(String),
    #[error(transparent)]
    Materialization(#[from] MaterializationError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Retains all verified output views for the whole lifetime of the consumer.
/// The launcher holds this value until the namespace init has exited.
#[derive(Debug)]
pub struct RuntimeClosure {
    application: MaterializedArtifact,
    loader: Option<MaterializedArtifact>,
    libraries: Vec<MaterializedArtifact>,
    utf8_locale: Option<String>,
}

#[derive(Debug)]
pub struct RuntimeMount {
    pub source: PathBuf,
    pub target: String,
}

impl RuntimeClosure {
    pub fn application(&self) -> &MaterializedArtifact {
        &self.application
    }
    pub fn loader(&self) -> Option<&MaterializedArtifact> {
        self.loader.as_ref()
    }
    pub fn libraries(&self) -> &[MaterializedArtifact] {
        &self.libraries
    }
    pub fn entry(&self) -> String {
        format!(
            "{}/{}",
            self.application.logical_prefix(),
            self.application.entry()
        )
    }
    /// Revalidate every physical source immediately before constructing the
    /// private mount namespace. The launcher must keep this closure alive until
    /// its process tree settles, and bind these targets read-only.
    pub fn mounts(&self) -> Result<Vec<RuntimeMount>, RuntimeError> {
        Ok(self
            .mount_bindings()?
            .into_iter()
            .map(|(mount, _)| mount)
            .collect())
    }

    fn mount_bindings(&self) -> Result<Vec<(RuntimeMount, BorrowedFd<'_>)>, RuntimeError> {
        let mut seen = BTreeSet::new();
        std::iter::once(&self.application)
            .chain(self.loader.iter())
            .chain(&self.libraries)
            .filter(|view| seen.insert(view.action()))
            .map(|view| {
                Ok((
                    RuntimeMount {
                        source: view.directory()?,
                        target: view.logical_prefix(),
                    },
                    view.mount_fd(),
                ))
            })
            .collect()
    }
}

/// Synchronous rootless session. The borrowed closure holds the Store lease
/// throughout namespace settlement; a private PID namespace kills all
/// remaining descendants when its init exits. No ambient host `/usr` or
/// library-search environment enters the child.
pub fn run_runtime(
    closure: &RuntimeClosure,
    argv: &[OsString],
    working_directory: &std::path::Path,
) -> Result<ExitStatus, RuntimeError> {
    run_runtime_with_cancellation(
        closure,
        argv,
        working_directory,
        &crate::BuildCancellation::default(),
    )
}

/// Run the verified closure while observing CLI cancellation. The native monitor
/// settles its private PID namespace before releasing the process retention.
#[allow(clippy::too_many_lines)]
pub fn run_runtime_with_cancellation(
    closure: &RuntimeClosure,
    argv: &[OsString],
    working_directory: &std::path::Path,
    cancellation: &crate::BuildCancellation,
) -> Result<ExitStatus, RuntimeError> {
    if cancellation.is_cancelled() {
        return Err(RuntimeError::Cancelled);
    }
    if !working_directory.is_absolute()
        || working_directory == std::path::Path::new("/")
        || working_directory.starts_with("/syrox")
    {
        return Err(invalid(
            "runtime working directory must be a non-root absolute path outside /syrox",
        ));
    }
    let cwd_descriptor = crate::linux_fd::open_top_directory(working_directory)
        .map_err(|error| invalid(&format!("cannot open runtime working directory: {error:?}")))?;
    let cwd = crate::linux_fd::directory_path(&cwd_descriptor)?;
    let mounts = closure.mount_bindings()?;
    let source = &mounts[0].0.source;
    let views = source
        .parent()
        .ok_or_else(|| invalid("invalid view source"))?;
    let private = views
        .parent()
        .ok_or_else(|| invalid("invalid view parent"))?;
    let store = private
        .parent()
        .ok_or_else(|| invalid("invalid Store path"))?;
    if views.file_name().is_none_or(|name| name != "views")
        || private
            .file_name()
            .is_none_or(|name| name != ".syrox-store")
        || mounts
            .iter()
            .any(|(mount, _)| !mount.source.starts_with(views))
        || cwd.starts_with(store)
    {
        return Err(invalid(
            "runtime working directory would expose the writable Store",
        ));
    }
    let masked_store = store.starts_with(&cwd).then(|| store.to_path_buf());
    // The monitor and sync EOF can both precede kernel teardown of descendants.
    // Block the payload until its namespace PID 1 is pinned by a pidfd, then
    // await that pidfd even on cancellation and post-spawn errors.
    let (mut settled, keepalive) = UnixStream::pair()?;
    let (info, info_writer) = crate::linux_fd::seqpacket_pair()?;
    let (mut gate, gate_reader) = UnixStream::pair()?;
    let keepalive_fd = keepalive.as_raw_fd();
    let info_fd = info_writer.as_raw_fd();
    let gate_fd = gate_reader.as_raw_fd();
    let executable = std::env::current_exe()?;
    let mut helper = executable.with_file_name("syrox-worker");
    if !helper.is_file()
        && executable
            .parent()
            .is_some_and(|path| path.ends_with("deps"))
    {
        helper = executable
            .parent()
            .and_then(std::path::Path::parent)
            .ok_or_else(|| invalid("missing runtime helper directory"))?
            .join("syrox-worker");
    }
    let process_retention = closure.application.process_retention()?;
    let mut command = crate::linux_fd::PreparedLaunch::new(helper.as_os_str())?;
    command.inherit(process_retention.as_fd());
    // The monitor owns PID 1 and watches the coordinator channel through exit.
    command.args(["__runtime", "monitor"])?;
    command.arg(keepalive_fd.to_string())?;
    command.arg(info_fd.to_string())?;
    command.arg(gate_fd.to_string())?;
    for (mount, directory) in mounts {
        let fd = directory.as_raw_fd();
        command.inherit(directory);
        command
            .arg("--ro")?
            .arg(format!("/proc/self/fd/{fd}"))?
            .arg(mount.target)?;
    }
    let cwd_fd = cwd_descriptor.fd().as_raw_fd();
    command.inherit(cwd_descriptor.fd());
    command
        .arg("--rw")?
        .arg(format!("/proc/self/fd/{cwd_fd}"))?
        .arg(&cwd)?;
    if let Some(store) = masked_store {
        // Preserve getcwd while hiding the Store under a private, empty mount.
        // Logical views were already bound by descriptor-verified sources above.
        command.arg("--mask")?.arg(store)?;
    }
    command.arg("--cwd")?.arg(&cwd)?;
    if let Some(locale) = &closure.utf8_locale {
        command.arg("--locale")?.arg(locale)?;
    }
    command.args(["--", &closure.entry()])?;
    command.args(argv)?;
    // Clear CLOEXEC only through spawn actions, so concurrent process launches
    // cannot inherit a writable endpoint and delay the settlement barrier.
    command.inherit(keepalive.as_fd());
    command.inherit(info_writer.as_fd());
    command.inherit(gate_reader.as_fd());
    let mut child = command.spawn()?;
    drop((keepalive, info_writer, gate_reader));
    let mut namespace = None;
    let prepared = (|| -> Result<(), RuntimeError> {
        namespace = Some(receive_namespace(info.as_fd(), cancellation)?);
        if cancellation.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        gate.write_all(b"x")?;
        Ok(())
    })();
    let namespace = match prepared {
        Ok(()) => namespace.expect("successful preparation pinned namespace init"),
        Err(error) => {
            // Both authorization EOF and coordinator loss abort the native init.
            drop(settled);
            drop(gate);
            child.wait()?;
            if let Some(pinned) = namespace {
                crate::linux_fd::wait_pidfd(pinned.as_fd())?;
            }
            return Err(error);
        }
    };
    drop(gate);
    let monitor = match crate::linux_fd::pidfd_open(child.id()) {
        Ok(fd) => fd,
        Err(error) => {
            drop(settled);
            child.wait()?;
            crate::linux_fd::wait_pidfd(namespace.as_fd())?;
            return Err(error.into());
        }
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Err(error) => {
                drop(settled);
                crate::linux_fd::kill_pidfd(namespace.as_fd())?;
                let _ = child.wait();
                crate::linux_fd::wait_pidfd(namespace.as_fd())?;
                return Err(error.into());
            }
            Ok(None) => {}
        }
        if cancellation.is_cancelled() {
            drop(settled);
            let status = child.wait();
            crate::linux_fd::wait_pidfd(namespace.as_fd())?;
            return Ok(status?);
        }
        if let Err(error) = crate::runtime_native::wait_event(monitor.as_fd(), cancellation) {
            drop(settled);
            child.wait()?;
            crate::linux_fd::wait_pidfd(namespace.as_fd())?;
            return Err(error.into());
        }
    };
    let mut buffer = [0_u8; 1];
    let sync = settled.read(&mut buffer);
    crate::linux_fd::wait_pidfd(namespace.as_fd())?;
    if sync? != 0 {
        return Err(invalid("runtime settlement descriptor contained data"));
    }
    Ok(status)
}

fn receive_namespace(
    info: BorrowedFd<'_>,
    cancellation: &crate::BuildCancellation,
) -> Result<std::os::fd::OwnedFd, RuntimeError> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        check_runtime(cancellation)?;
        match crate::linux_fd::receive_fd(info) {
            Ok(fd) => return Ok(fd),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error.into()),
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(invalid("runtime namespace preparation timed out"));
        }
        crate::runtime_native::wait_event_timeout(
            info,
            cancellation,
            i32::try_from(remaining.as_millis()).unwrap_or(10_000),
        )?;
    }
}

#[cfg(test)]
fn prepare_namespace(
    info: &mut UnixStream,
    gate: &mut UnixStream,
    cancellation: &crate::BuildCancellation,
    namespace: &mut Option<std::os::fd::OwnedFd>,
) -> Result<(), RuntimeError> {
    prepare_namespace_with_hook(info, gate, cancellation, namespace, || {})
}

#[cfg(test)]
fn prepare_namespace_with_hook(
    info: &mut UnixStream,
    gate: &mut UnixStream,
    cancellation: &crate::BuildCancellation,
    namespace: &mut Option<std::os::fd::OwnedFd>,
    after_pin: impl FnOnce(),
) -> Result<(), RuntimeError> {
    let bytes = read_namespace_info(info, cancellation)?;
    let pid = namespace_pid(&bytes)?;
    *namespace = Some(crate::linux_fd::pidfd_open(pid)?);
    after_pin();
    if cancellation.is_cancelled() {
        return Err(RuntimeError::Cancelled);
    }
    gate.write_all(b"x")?;
    Ok(())
}

/// The info channel is bounded in both bytes and time. A blocked monitor must
/// not prevent cancellation before the payload gate has been released.
#[cfg(test)]
fn read_namespace_info(
    info: &mut UnixStream,
    cancellation: &crate::BuildCancellation,
) -> Result<Vec<u8>, RuntimeError> {
    info.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 513];
    loop {
        if cancellation.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(invalid("runtime namespace preparation timed out"));
        }
        crate::runtime_native::wait_event_timeout(
            info.as_fd(),
            cancellation,
            i32::try_from(
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis(),
            )
            .unwrap_or(10_000),
        )?;
        if cancellation.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        match info.read(&mut buffer[..513 - bytes.len()]) {
            Ok(0) => return Ok(bytes),
            Ok(count) => {
                bytes.extend_from_slice(&buffer[..count]);
                if bytes.len() == 513 {
                    return Err(invalid("runtime namespace info is too large"));
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
fn abort_launch(
    child: &mut Child,
    namespace: Option<&std::os::fd::OwnedFd>,
    primary: RuntimeError,
) -> RuntimeError {
    // Keep --block-fd open while killing PID 1. Killing just the monitor can
    // release the gate by EOF and execute an otherwise unauthorized payload.
    let stopped = namespace.map(|pinned| crate::linux_fd::kill_pidfd(pinned.as_fd()));
    let killed = child.kill();
    let reaped = child.wait();
    let settled = namespace.map(|pinned| crate::linux_fd::wait_pidfd(pinned.as_fd()));
    // In particular, a failed gate write must still settle namespace PID 1
    // before the caller may release the closure's Store lease.
    if let Some(Err(error)) = settled {
        return error.into();
    }
    if let Some(Err(error)) = stopped {
        return error.into();
    }
    if let Err(error) = reaped {
        return error.into();
    }
    if let Err(error) = killed
        && error.kind() != std::io::ErrorKind::InvalidInput
    {
        return error.into();
    }
    primary
}

/// Bubblewrap may have forked PID 1 even when its info record is missing,
/// partial or unreadable. Killing only its monitor leaves a blocked init that
/// executes the payload when the gate is eventually closed. The monitor's
/// unreaped PID pins its newly-created process group until the group signal
/// has been delivered, and no payload can form a new group before the gate.
#[cfg(test)]
fn abort_unpinned_launch(
    child: &mut Child,
    settled: &mut UnixStream,
    primary: RuntimeError,
) -> RuntimeError {
    let stopped = crate::linux_fd::kill_launcher_group(child.id());
    let reaped = child.wait();
    let mut buffer = [0_u8; 1];
    let sync = settled.read(&mut buffer);
    if let Err(error) = stopped {
        return error.into();
    }
    if let Err(error) = reaped {
        return error.into();
    }
    match sync {
        Ok(0) => primary,
        Ok(_) => invalid("runtime settlement descriptor contained data"),
        Err(error) => error.into(),
    }
}

#[cfg(test)]
fn namespace_pid(bytes: &[u8]) -> Result<u32, RuntimeError> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("invalid namespace info"))?;
    let (_, tail) = text
        .split_once("\"child-pid\":")
        .ok_or_else(|| invalid("namespace PID is missing"))?;
    let digits = tail
        .trim_start()
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    let pid = tail.trim_start()[..digits]
        .parse::<u32>()
        .map_err(|_| invalid("invalid namespace PID"))?;
    if pid == 0 || !text.trim_end().ends_with('}') {
        return Err(invalid("invalid namespace info"));
    }
    Ok(pid)
}

/// Materializes only explicitly named, retained outputs. Reads ELF metadata as
/// bytes from verified artifacts; it never calls `ldd` or runs a supplied file.
#[allow(clippy::too_many_lines)]
pub fn verify_runtime(
    store: &Store,
    request: &RuntimeRequest,
) -> Result<RuntimeClosure, RuntimeError> {
    verify_runtime_with_cancellation(store, request, &crate::BuildCancellation::default())
}

#[allow(clippy::too_many_lines)]
pub fn verify_runtime_with_cancellation(
    store: &Store,
    request: &RuntimeRequest,
    cancellation: &crate::BuildCancellation,
) -> Result<RuntimeClosure, RuntimeError> {
    check_runtime(cancellation)?;
    if request.libraries.len() > MAX_LIBRARIES {
        return Err(invalid("too many library outputs"));
    }
    let application = crate::build::materialize::materialize_with_cancellation(
        store,
        &request.application.root,
        request.application.receipt,
        cancellation,
    )
    .map_err(materialization_error)?;
    let mut views: BTreeMap<(RootName, ContentDigest), MaterializedArtifact> = BTreeMap::new();
    let mut provider_view = |output: &RuntimeOutput| -> Result<MaterializedArtifact, RuntimeError> {
        check_runtime(cancellation)?;
        let key = (output.root.clone(), output.receipt);
        if let Some(view) = views.get(&key) {
            return Ok(view.share());
        }
        let lease = store.operation()?;
        check_runtime(cancellation)?;
        let independent = lease.build_references(&output.root)?.is_some();
        drop(lease);
        let view = if independent {
            crate::build::materialize::materialize_with_cancellation(
                store,
                &output.root,
                output.receipt,
                cancellation,
            )
            .map_err(materialization_error)?
        } else {
            crate::build::materialize::materialize_provider_with_cancellation(
                store,
                &request.application.root,
                request.application.receipt,
                output.receipt,
                cancellation,
            )
            .map_err(materialization_error)?
        };
        views.insert(key, view.share());
        Ok(view)
    };
    let loader = request
        .loader
        .as_ref()
        .map(&mut provider_view)
        .transpose()?;
    let libraries = request
        .libraries
        .iter()
        .map(provider_view)
        .collect::<Result<Vec<_>, _>>()?;
    let mut actions = BTreeMap::new();
    for view in std::iter::once(&application)
        .chain(loader.iter())
        .chain(&libraries)
    {
        if actions
            .insert(view.action(), view.artifact())
            .is_some_and(|digest| digest != view.artifact())
        {
            return Err(invalid(
                "an action is bound to more than one runtime output",
            ));
        }
    }
    let mut inspected: BTreeMap<(ContentDigest, ContentDigest), Arc<[RuntimeFile]>> =
        BTreeMap::new();
    let mut inspect_view =
        |view: &MaterializedArtifact| -> Result<Arc<[RuntimeFile]>, RuntimeError> {
            let key = (view.action(), view.artifact());
            if let Some(found) = inspected.get(&key) {
                return Ok(Arc::clone(found));
            }
            let found = Arc::from(files(store, view, cancellation)?);
            inspected.insert(key, Arc::clone(&found));
            Ok(found)
        };
    let program_files = inspect_view(&application)?;
    let program = program_files
        .iter()
        .find(|file| file.path == application.entry())
        .ok_or_else(|| invalid("missing application entry"))?;
    let executable = program
        .elf
        .clone()
        .ok_or_else(|| invalid("application is not ELF"))?;
    if executable.kind != 2 && executable.kind != 3 {
        return Err(invalid("application is not an ELF executable"));
    }
    if executable.interpreter.is_none() && !executable.needed.is_empty() {
        return Err(invalid(
            "application has dependencies but no declared ELF interpreter",
        ));
    }
    let mut consumers = vec![executable];
    let mut loader_soname = None;
    match (&consumers[0].interpreter, &loader) {
        (None, None) => {}
        (Some(interpreter), Some(loader))
            if *interpreter == format!("{}/{}", loader.logical_prefix(), loader.entry()) =>
        {
            let loader_files = inspect_view(loader)?;
            let loader_file = loader_files
                .iter()
                .find(|file| file.path == loader.entry())
                .ok_or_else(|| invalid("missing loader entry"))?;
            let loader_elf = loader_file
                .elf
                .clone()
                .ok_or_else(|| invalid("loader is not ELF"))?;
            if loader_elf.kind != 3 || loader_elf.interpreter.is_some() {
                return Err(invalid("loader is not a standalone ELF interpreter"));
            }
            loader_soname.clone_from(&loader_elf.soname);
            consumers.push(loader_elf);
        }
        _ => {
            return Err(invalid(
                "ELF interpreter is missing, undeclared or points outside the closure",
            ));
        }
    }
    let mut providers = BTreeMap::new();
    let mut provider_elf = BTreeMap::new();
    let mut utf8_locale = None;
    for view in &libraries {
        check_runtime(cancellation)?;
        let artifact_files = inspect_view(view)?;
        if artifact_files
            .iter()
            .any(|file| file.path == "usr/lib/locale/C.utf8/LC_CTYPE")
            && utf8_locale
                .replace(format!("{}/usr/lib/locale", view.logical_prefix()))
                .is_some()
        {
            return Err(invalid("duplicate UTF-8 locale providers"));
        }
        let mut supplied = 0;
        for file in artifact_files.iter().filter(|file| {
            file.path
                .strip_prefix("usr/lib/")
                .is_some_and(|name| !name.contains('/') && name.contains(".so"))
        }) {
            // A development output may contain a linker script named libc.so;
            // the runtime loader resolves only ELF DT_NEEDED SONAMEs. A needed
            // name backed solely by a script still fails the provider check.
            let Some(lib) = file.elf.clone() else {
                continue;
            };
            if lib.kind != 3 {
                return Err(invalid("library is not an ELF shared object"));
            }
            let soname = lib
                .soname
                .as_ref()
                .ok_or_else(|| invalid("library has no SONAME"))?;
            if file.path.rsplit('/').next() != Some(soname.as_str()) {
                if file.alias_target {
                    continue;
                }
                return Err(invalid("library filename disagrees with SONAME"));
            }
            if providers
                .insert(soname.clone(), format!("{}/usr/lib", view.logical_prefix()))
                .is_some()
            {
                return Err(invalid("duplicate library SONAME"));
            }
            provider_elf.insert(soname.clone(), lib);
            supplied += 1;
        }
        if supplied == 0 {
            return Err(invalid("library output supplies no ELF library"));
        }
    }
    // Installed NSS, tracing and math modules are legitimate members of a
    // complete glibc runtime, but their DT_NEEDED entries are not dependencies
    // of this executable unless the executable's graph actually reaches them.
    // Require every installed module's dependencies to have providers, while
    // resolving RUNPATH only along the graph rooted at the application and its
    // declared interpreter.
    if provider_elf
        .values()
        .flat_map(|elf| &elf.needed)
        .any(|name| {
            Some(name.as_str()) != loader_soname.as_deref() && !providers.contains_key(name)
        })
    {
        return Err(invalid(
            "installed library has an undeclared ELF dependency",
        ));
    }
    let mut required = BTreeSet::new();
    let mut frontier = consumers
        .iter()
        .flat_map(|elf| elf.needed.iter().cloned())
        .collect::<Vec<_>>();
    while let Some(name) = frontier.pop() {
        check_runtime(cancellation)?;
        if !required.insert(name.clone()) {
            continue;
        }
        if required.len() > MAX_NEEDED {
            return Err(invalid("too many ELF dependencies"));
        }
        if Some(name.as_str()) == loader_soname.as_deref() {
            continue;
        }
        let library = provider_elf
            .remove(&name)
            .ok_or_else(|| invalid("missing declared ELF library"))?;
        frontier.extend(library.needed.iter().cloned());
        consumers.push(library);
    }
    if let Some(soname) = loader_soname
        .as_ref()
        .filter(|name| required.contains(*name))
    {
        let expected = format!(
            "{}/usr/lib",
            loader
                .as_ref()
                .expect("loader was validated")
                .logical_prefix()
        );
        if loader
            .as_ref()
            .and_then(|view| view.entry().rsplit('/').next())
            != Some(soname.as_str())
            || providers
                .get(soname)
                .is_some_and(|actual| actual != &expected)
        {
            return Err(invalid("loader SONAME must be supplied by its own output"));
        }
        providers.insert(soname.clone(), expected);
    }
    if required.len() > MAX_NEEDED || required.iter().any(|name| !providers.contains_key(name)) {
        return Err(invalid("missing declared ELF library"));
    }
    if !required.is_empty() && loader.is_none() {
        return Err(invalid("dynamic libraries require a declared loader"));
    }
    let permitted = providers.values().cloned().collect::<BTreeSet<_>>();
    for elf in &consumers {
        check_runtime(cancellation)?;
        let directories = elf
            .needed
            .iter()
            // glibc's libc.so.6 names its interpreter as DT_NEEDED but its
            // loader is already bound exactly by PT_INTERP, without RUNPATH.
            .filter(|name| Some(name.as_str()) != loader_soname.as_deref())
            .map(|name| {
                providers
                    .get(name)
                    .cloned()
                    .ok_or_else(|| invalid("missing declared ELF library"))
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        let search = elf.runpath.as_deref().unwrap_or("");
        let declared = if search.is_empty() {
            BTreeSet::new()
        } else {
            search.split(':').map(str::to_owned).collect()
        };
        if !directories.is_subset(&declared)
            || !declared.is_subset(&permitted)
            || (!search.is_empty() && search.split(':').count() != declared.len())
        {
            return Err(invalid(
                "ELF RUNPATH does not resolve solely through declared library directories",
            ));
        }
    }
    check_runtime(cancellation)?;
    Ok(RuntimeClosure {
        application,
        loader,
        libraries,
        utf8_locale,
    })
}

#[derive(Clone)]
struct RuntimeFile {
    path: String,
    elf: Option<Elf>,
    alias_target: bool,
}

fn files(
    store: &Store,
    view: &MaterializedArtifact,
    cancellation: &crate::BuildCancellation,
) -> Result<Vec<RuntimeFile>, RuntimeError> {
    check_runtime(cancellation)?;
    let lease = store.operation()?;
    let input = lease
        .open_verified_checked(view.artifact(), MAX_OUTPUT_BYTES, || {
            if cancellation.is_cancelled() {
                Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "runtime cancelled"))
            } else {
                Ok(())
            }
        }).map_err(|error| if cancellation.is_cancelled() && matches!(error, StoreError::Io(ref source) if source.kind() == std::io::ErrorKind::Interrupted) {
            RuntimeError::Cancelled
        } else {
            error.into()
        })?
        .ok_or_else(|| invalid("runtime artifact disappeared"))?;
    let mut indexed = artifact::IndexedArtifact::open_checked(input, view.entry(), cancellation)
        .map_err(|error| {
            if matches!(error, crate::BuildError::Cancelled) {
                RuntimeError::Cancelled
            } else {
                invalid("invalid runtime artifact")
            }
        })?;
    let mut files = Vec::with_capacity(indexed.files().len());
    let (indexed_files, reader) = indexed.parts();
    for file in indexed_files.iter() {
        check_runtime(cancellation)?;
        let inspect = file.path == view.entry()
            || file
                .path
                .strip_prefix("usr/lib/")
                .is_some_and(|name| !name.contains('/') && name.contains(".so"));
        let elf = if inspect {
            let size =
                usize::try_from(file.size).map_err(|_| invalid("ELF exceeds address space"))?;
            let magic = artifact::IndexedArtifact::<crate::VerifiedReader>::range_from(
                reader,
                &file,
                0,
                size.min(4),
                cancellation,
            )
            .map_err(runtime_read_error)?;
            if magic == b"\x7fELF" {
                Some(inspect_ranges(size, |start, length| {
                    artifact::IndexedArtifact::<crate::VerifiedReader>::range_from(
                        reader,
                        &file,
                        start,
                        length,
                        cancellation,
                    )
                    .map_err(runtime_read_error)
                })?)
            } else {
                None
            }
        } else {
            None
        };
        files.push(RuntimeFile {
            path: file.path.to_owned(),
            elf,
            alias_target: file.alias_target,
        });
    }
    Ok(files)
}

fn check_runtime(cancellation: &crate::BuildCancellation) -> Result<(), RuntimeError> {
    if cancellation.is_cancelled() {
        Err(RuntimeError::Cancelled)
    } else {
        Ok(())
    }
}

fn runtime_read_error(error: std::io::Error) -> RuntimeError {
    if crate::build::is_cancellation_io(&error) {
        RuntimeError::Cancelled
    } else {
        RuntimeError::Io(error)
    }
}

fn materialization_error(error: MaterializationError) -> RuntimeError {
    if matches!(error, MaterializationError::Cancelled) {
        RuntimeError::Cancelled
    } else {
        error.into()
    }
}

fn invalid(reason: &str) -> RuntimeError {
    RuntimeError::Invalid(reason.to_owned())
}

mod elf;
use elf::{Elf, MAX_NEEDED, inspect_ranges};
#[cfg(test)]
use elf::{checked, inspect};

#[cfg(test)]
pub(crate) mod tests;
