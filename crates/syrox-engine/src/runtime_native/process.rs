//! Fork-like clone3, used only by the dedicated single-threaded runtime helper.
use std::io;
use std::os::fd::{AsFd as _, BorrowedFd, FromRawFd as _, OwnedFd};
use std::os::unix::process::ExitStatusExt as _;
use std::process::ExitStatus;

#[repr(C)]
#[derive(Default)]
struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

pub(super) enum Spawn {
    Init,
    Monitor(NamespaceChild),
}

/// The pidfd and the right to reap this direct child have the same owner.
/// Early returns stop and settle the namespace before releasing its resources.
pub(super) struct NamespaceChild {
    pid: libc::pid_t,
    pinned: OwnedFd,
    status: Option<ExitStatus>,
}

impl NamespaceChild {
    pub(super) fn pidfd(&self) -> BorrowedFd<'_> {
        self.pinned.as_fd()
    }

    pub(super) fn kill(&self) -> io::Result<()> {
        crate::linux_fd::kill_pidfd(self.pidfd())
    }

    pub(super) fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        loop {
            let mut status = 0;
            // SAFETY: pid is our unreaped direct child; status is a writable int.
            let result = unsafe { libc::waitpid(self.pid, &raw mut status, 0) };
            if result == self.pid {
                let status = ExitStatus::from_raw(status);
                self.status = Some(status);
                return Ok(status);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

impl Drop for NamespaceChild {
    fn drop(&mut self) {
        if self.status.is_none() {
            let _ = self.kill();
            let _ = self.wait();
        }
    }
}

/// # Safety
/// The process must be single-threaded, with no active borrowed process-global
/// locks. The init branch must terminate with _exit rather than return/unwind
/// into the monitor's owners. No VM, stack or file-descriptor table is shared.
pub(super) unsafe fn spawn() -> io::Result<Spawn> {
    const CLONE_PIDFD: u64 = 0x1000;
    let mut pidfd: libc::c_int = -1;
    let args = CloneArgs {
        flags: CLONE_PIDFD
            | u64::from(libc::CLONE_NEWNS.cast_unsigned() | libc::CLONE_NEWPID.cast_unsigned()),
        pidfd: (&raw mut pidfd).addr() as u64,
        exit_signal: u64::from(libc::SIGCHLD.cast_unsigned()),
        ..CloneArgs::default()
    };
    // SAFETY: the caller establishes fork safety. With CLONE_VM unset, a null
    // stack requests a copy of the caller's stack; all ABI fields are initialized.
    // The kernel writes the newly owned pidfd into the parent's live int.
    let result =
        unsafe { libc::syscall(libc::SYS_clone3, &raw const args, size_of::<CloneArgs>()) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if result == 0 {
        return Ok(Spawn::Init);
    }
    let pid = libc::pid_t::try_from(result).expect("clone3 returns a kernel pid_t");
    // SAFETY: successful CLONE_PIDFD transfers one fresh descriptor to the parent.
    let pinned = unsafe { OwnedFd::from_raw_fd(pidfd) };
    Ok(Spawn::Monitor(NamespaceChild {
        pid,
        pinned,
        status: None,
    }))
}
