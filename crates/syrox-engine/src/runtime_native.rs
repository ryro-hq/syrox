//! Internal, single-threaded Linux runtime helper. The monitor owns namespace
//! PID 1 until wait completes; loss of the coordinator closes its control socket.
use std::ffi::{CString, OsString};
use std::io::{self, Read as _};
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, FromRawFd as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

mod process;

fn checked(result: libc::c_long) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn text(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)
}

fn mount(
    source: Option<&Path>,
    target: &Path,
    kind: Option<&std::ffi::CStr>,
    flags: libc::c_ulong,
) -> io::Result<()> {
    let source = source.map(text).transpose()?;
    let target = text(target)?;
    // SAFETY: all optional strings remain live through this synchronous syscall.
    checked(
        unsafe {
            libc::mount(
                source.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                target.as_ptr(),
                kind.map_or(std::ptr::null(), std::ffi::CStr::as_ptr),
                flags,
                std::ptr::null(),
            )
        }
        .into(),
    )
    .map_err(|error| io::Error::other(format!("mount {source:?} on {target:?}: {error}")))
}

/// Event wait shared with the coordinator. A timeout is used only by callers
/// whose cancellation source is an externally owned atomic flag.
pub(crate) fn ready(fd: BorrowedFd<'_>, timeout: i32) -> io::Result<bool> {
    let mut item = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: item is initialized and writable for the duration of poll.
    let result = unsafe { libc::poll(&raw mut item, 1, timeout) };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error);
    }
    Ok(result > 0)
}

pub(crate) fn wait_event(
    fd: BorrowedFd<'_>,
    cancellation: &crate::BuildCancellation,
) -> io::Result<()> {
    wait_event_timeout(fd, cancellation, -1)
}

pub(crate) fn wait_event_timeout(
    fd: BorrowedFd<'_>,
    cancellation: &crate::BuildCancellation,
    timeout: i32,
) -> io::Result<()> {
    let Some(wake) = cancellation.wake_fd() else {
        return ready(fd, if timeout < 0 { 25 } else { timeout.min(25) }).map(|_| ());
    };
    let mut events = [
        libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: both poll entries refer to live descriptors retained by the caller.
    let result = unsafe { libc::poll(events.as_mut_ptr(), 2, timeout) };
    if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn fd(value: &OsString) -> io::Result<i32> {
    let descriptor = value
        .to_str()
        .and_then(|s| s.parse().ok())
        .filter(|n| *n > 2)
        .ok_or_else(|| io::Error::other("invalid runtime descriptor"))?;
    // SAFETY: F_GETFD accepts an integer and fails for a descriptor not inherited.
    checked(unsafe { libc::fcntl(descriptor, libc::F_GETFD) }.into())?;
    Ok(descriptor)
}

fn status_code(status: std::process::ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(9))
}

/// Internal worker entry point, before starting threads.
///
/// # Safety
/// Call only in a dedicated single-threaded process with no active borrowed
/// process-global locks. Descriptor arguments must transfer ownership of live,
/// distinct channels to this function. The init clone allocates after clone3
/// and terminates with _exit; it never returns to the caller.
#[allow(clippy::too_many_lines)]
pub unsafe fn runtime_helper(args: &[OsString]) -> io::Result<i32> {
    if args.len() < 5
        || args.len() > 4096
        || args.iter().map(|s| s.len()).sum::<usize>() > 1024 * 1024
    {
        return Err(io::Error::other("invalid runtime request"));
    }
    if args[0] != "monitor" {
        return Err(io::Error::other("invalid runtime mode"));
    }
    let control_fd = fd(&args[1])?;
    let info_fd = fd(&args[2])?;
    let gate_fd = fd(&args[3])?;
    if control_fd == info_fd || control_fd == gate_fd || info_fd == gate_fd {
        return Err(io::Error::other("aliased runtime channels"));
    }
    // SAFETY: these distinct inherited descriptors are transferred by the caller.
    let control = unsafe { UnixStream::from_raw_fd(control_fd) };
    // SAFETY: info_fd is a distinct inherited channel, now owned here.
    let info = unsafe { std::os::fd::OwnedFd::from_raw_fd(info_fd) };
    // SAFETY: gate_fd is the third distinct channel transferred by the caller.
    let gate = unsafe { UnixStream::from_raw_fd(gate_fd) };
    // SAFETY: querying the process credentials has no memory arguments.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    // SAFETY: namespace flags are fixed; the helper is single-threaded.
    checked(unsafe { libc::unshare(libc::CLONE_NEWUSER) }.into())?;
    std::fs::write("/proc/self/uid_map", format!("0 {uid} 1"))?;
    std::fs::write("/proc/self/setgroups", "deny")?;
    std::fs::write("/proc/self/gid_map", format!("0 {gid} 1"))?;
    let root = tempfile::tempdir()?;
    let parent = crate::linux_fd::pidfd_open(std::process::id())?;
    // SAFETY: the helper's caller guarantees a fresh single-threaded process.
    // The init branch closes the monitor channels and cannot return or unwind.
    let mut child = match unsafe { process::spawn()? } {
        process::Spawn::Init => {
            drop(control);
            drop(info);
            init_process(parent.as_fd(), gate, root.path(), &args[4..]);
        }
        process::Spawn::Monitor(child) => child,
    };
    drop(gate);
    drop(parent);
    let outcome = (|| {
        crate::linux_fd::send_fd(info.as_fd(), child.pidfd())?;
        drop(info);
        // SCM_RIGHTS pins the same process object even if init exits before the
        // coordinator receives it. No numeric-PID pin acknowledgement is needed.
        let mut events = [
            libc::pollfd {
                fd: control_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: child.pidfd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            // SAFETY: events contains two initialized entries, both fds live.
            let result = unsafe { libc::poll(events.as_mut_ptr(), 2, -1) };
            if result < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(io::Error::last_os_error());
            }
            if events[0].revents != 0 {
                child.kill()?;
            }
            if events[0].revents != 0 || events[1].revents != 0 {
                break;
            }
        }
        Ok(())
    })();
    if outcome.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    outcome?;
    Ok(status_code(status))
}

/// Only the clone child enters this function. Catching unwinds prevents the
/// monitor's copied `TempDir` and process-retention owners from being dropped.
fn init_process(parent: BorrowedFd<'_>, gate: UnixStream, root: &Path, args: &[OsString]) -> ! {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: integer-only prctl in the fresh namespace init.
        checked(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) }.into())?;
        // getppid can be zero across PID namespaces. The inherited pidfd also
        // detects monitor death before installing the parent-death signal.
        require_live_monitor(parent)?;
        init(gate, root, args)
    }));
    let code = match result {
        Ok(Ok(code)) => code,
        Ok(Err(error)) => {
            eprintln!("error: {error}");
            1
        }
        Err(_) => 1,
    };
    // SAFETY: terminate the clone without running copied monitor destructors or
    // atexit handlers. PID 1 exit tears down all remaining namespace descendants.
    unsafe { libc::_exit(code) }
}

fn require_live_monitor(parent: BorrowedFd<'_>) -> io::Result<()> {
    let mut event = libc::pollfd {
        fd: parent.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: parent is a retained pidfd and event is initialized. Unlike
        // the coordinator's event loop, an interrupted probe cannot mean alive:
        // monitor death may have preceded PR_SET_PDEATHSIG in this child.
        let result = unsafe { libc::poll(&raw mut event, 1, 0) };
        if result == 0 {
            return Ok(());
        }
        if result > 0 {
            return Err(io::Error::other("runtime monitor exited"));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[allow(clippy::too_many_lines)]
fn init(mut gate: UnixStream, root: &Path, args: &[OsString]) -> io::Result<i32> {
    // clone3 created a private mount namespace, separate from the monitor.
    mount(None, Path::new("/"), None, libc::MS_REC | libc::MS_PRIVATE)?;
    mount(
        Some(Path::new("tmpfs")),
        root,
        Some(c"tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV,
    )?;
    let mut at = 0;
    let mut cwd = None;
    let mut locale = None;
    while at < args.len() && args[at] != "--" {
        let option = &args[at];
        let value = args
            .get(at + 1)
            .ok_or_else(|| io::Error::other("truncated runtime request"))?;
        if option == "--cwd" {
            cwd = Some(PathBuf::from(value));
            at += 2;
            continue;
        }
        if option == "--locale" {
            locale = Some(value.clone());
            at += 2;
            continue;
        }
        let target = if option == "--mask" {
            value
        } else {
            args.get(at + 2)
                .ok_or_else(|| io::Error::other("missing mount target"))?
        };
        let target = Path::new(target);
        if !target.is_absolute()
            || target
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(io::Error::other("invalid runtime mount"));
        }
        let destination = root.join(target.strip_prefix("/").map_err(io::Error::other)?);
        std::fs::create_dir_all(&destination)?;
        if option == "--mask" {
            mount(
                Some(Path::new("tmpfs")),
                &destination,
                Some(c"tmpfs"),
                libc::MS_NOSUID | libc::MS_NODEV,
            )?;
            at += 2;
        } else if option == "--ro" || option == "--rw" {
            bind_tree(Path::new(value), &destination)?;
            if option == "--ro" {
                let target = text(&destination)?;
                let attrs = libc::mount_attr {
                    attr_set: libc::MOUNT_ATTR_RDONLY,
                    attr_clr: 0,
                    propagation: 0,
                    userns_fd: 0,
                };
                // SAFETY: pathname and mount_attr are live and initialized.
                checked(unsafe {
                    libc::syscall(
                        libc::SYS_mount_setattr,
                        libc::AT_FDCWD,
                        target.as_ptr(),
                        libc::AT_RECURSIVE,
                        &raw const attrs,
                        size_of_val(&attrs),
                    )
                })?;
            }
            at += 3;
        } else {
            return Err(io::Error::other("invalid runtime option"));
        }
    }
    let program = args
        .get(at + 1)
        .ok_or_else(|| io::Error::other("missing runtime program"))?;
    std::fs::create_dir(root.join("oldroot"))?;
    let old = text(&root.join("oldroot"))?;
    let root = text(root)?;
    // SAFETY: both directories exist in the private mount namespace.
    checked(unsafe { libc::syscall(libc::SYS_pivot_root, root.as_ptr(), old.as_ptr()) })?;
    std::env::set_current_dir("/")?;
    // SAFETY: oldroot is the detached host mount; this affects only our namespace.
    checked(unsafe { libc::umount2(c"/oldroot".as_ptr(), libc::MNT_DETACH) }.into())?;
    std::fs::remove_dir("/oldroot")?;
    drop_privileges()?;
    std::env::set_current_dir(cwd.ok_or_else(|| io::Error::other("missing runtime cwd"))?)?;
    let mut authorization = [0];
    gate.read_exact(&mut authorization)?;
    if authorization != *b"x" {
        return Err(io::Error::other("runtime launch refused"));
    }
    drop(gate);
    // SAFETY: all inherited host/channel descriptors are no longer needed.
    checked(unsafe { libc::syscall(libc::SYS_close_range, 3_u32, u32::MAX, 0_u32) })?;
    let mut payload = Command::new(program);
    payload.args(&args[at + 2..]).env_clear();
    if let Some(locale) = locale {
        payload.env("LC_ALL", "C.utf8").env("LOCPATH", locale);
    }
    // Exiting PID 1 terminates all remaining namespace descendants. The outer
    // monitor waits for PID 1, including the kernel's namespace teardown.
    Ok(status_code(payload.status()?))
}

fn bind_tree(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    // Inherited f_paths refer to the original mount namespace. Reopen in this
    // namespace and compare inode identity before cloning the mount by fd.
    let original = std::fs::metadata(source)?;
    let local_path = if source.starts_with("/proc/self/fd") {
        std::fs::read_link(source)?
    } else {
        source.to_path_buf()
    };
    let local = crate::linux_fd::open_top_directory(&local_path)
        .map_err(|error| io::Error::other(format!("runtime mount reopen: {error:?}")))?;
    let actual = local.metadata().identity();
    if (original.dev(), original.ino()) != (actual.device, actual.inode) {
        return Err(io::Error::other("runtime mount identity changed"));
    }
    let destination = text(destination)?;
    // SAFETY: AT_EMPTY_PATH clones the live, reopened directory directly. Its
    // existing metadata already established identity; no /proc path or new stat
    // is needed. The descriptor belongs to this mount namespace.
    let raw = unsafe {
        libc::syscall(
            libc::SYS_open_tree,
            local.fd().as_raw_fd(),
            c"".as_ptr(),
            1 | libc::O_CLOEXEC | libc::AT_RECURSIVE | libc::AT_EMPTY_PATH,
        )
    };
    if raw < 0 {
        return Err(io::Error::other(format!(
            "open_tree: {}",
            io::Error::last_os_error()
        )));
    }
    let raw = i32::try_from(raw).map_err(io::Error::other)?;
    // SAFETY: successful open_tree transfers a fresh descriptor.
    let tree = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
    // SAFETY: MOVE_MOUNT_F_EMPTY_PATH attaches the live detached mount at target.
    checked(unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            tree.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            4_u32,
        )
    })
}

fn drop_privileges() -> io::Result<()> {
    // SAFETY: integer-only prctl operations on this single-threaded init.
    checked(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) }.into())?;
    // SECBIT_NOROOT | SECBIT_NOROOT_LOCKED prevents uid 0 regaining caps on exec.
    checked(unsafe { libc::prctl(libc::PR_SET_SECUREBITS, 3, 0, 0, 0) }.into())?;
    for cap in 0..1024 {
        // SAFETY: dropping a numeric bounding capability has no pointers.
        // DROP succeeds even if the bit is already clear; EINVAL marks the end
        // of the kernel's capability range, so a separate READ is redundant.
        let result = unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) };
        if result < 0 {
            if cap > 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
                return clear_capabilities();
            }
            return Err(io::Error::last_os_error());
        }
    }
    Err(io::Error::other(
        "kernel capability range exceeds runtime support",
    ))
}

fn clear_capabilities() -> io::Result<()> {
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Caps {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let header = Header {
        version: 0x2008_0522,
        pid: 0,
    };
    let caps = [Caps {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    // SAFETY: capset v3 expects one header and two initialized capability words.
    checked(unsafe { libc::syscall(libc::SYS_capset, &raw const header, caps.as_ptr()) })
}
