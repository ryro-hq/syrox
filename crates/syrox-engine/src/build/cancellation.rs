use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::{fmt, io};

use super::BuildError;

#[derive(Debug)]
struct CancelledIo;

impl fmt::Display for CancelledIo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("operation cancelled")
    }
}

impl std::error::Error for CancelledIo {}

/// Use `Other` because the standard readers retry `Interrupted` unconditionally.
pub(crate) fn cancellation_io() -> io::Error {
    io::Error::other(CancelledIo)
}

pub(crate) fn is_cancellation_io(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<CancelledIo>)
}

/// Cooperative cancellation, scoped to one caller operation. Library entry
/// points never install process-global signal handlers. Cleanup/settlement must
/// run even after this flag is set.
#[derive(Clone, Debug)]
pub struct BuildCancellation {
    flag: Arc<AtomicBool>,
    #[cfg(target_os = "linux")]
    interest: Option<Arc<super::shared::Interest>>,
    #[cfg(target_os = "linux")]
    wake: Option<
        Arc<(
            std::os::unix::net::UnixStream,
            std::os::unix::net::UnixStream,
        )>,
    >,
}

#[cfg_attr(not(target_os = "linux"), allow(clippy::derivable_impls))]
impl Default for BuildCancellation {
    fn default() -> Self {
        Self {
            flag: Arc::default(),
            #[cfg(target_os = "linux")]
            interest: None,
            #[cfg(target_os = "linux")]
            wake: Self::wake_pair().ok().map(Arc::new),
        }
    }
}

impl BuildCancellation {
    /// Use a caller-owned flag, for example one registered with a signal handler.
    #[cfg_attr(not(target_os = "linux"), allow(clippy::needless_update))]
    pub fn from_flag(flag: Arc<AtomicBool>) -> Self {
        Self {
            flag,
            #[cfg(target_os = "linux")]
            wake: None,
            ..Self::default()
        }
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
        #[cfg(target_os = "linux")]
        if let Some(wake) = &self.wake {
            use std::io::Write as _;
            let _ = (&wake.1).write(&[1]);
        }
    }

    #[cfg(target_os = "linux")]
    fn wake_pair() -> io::Result<(
        std::os::unix::net::UnixStream,
        std::os::unix::net::UnixStream,
    )> {
        let pair = std::os::unix::net::UnixStream::pair()?;
        pair.0.set_nonblocking(true)?;
        pair.1.set_nonblocking(true)?;
        Ok(pair)
    }

    /// The caller must write to the returned channel whenever it sets the flag.
    /// Suitable for signal-hook's async-signal-safe pipe registration.
    #[cfg(target_os = "linux")]
    pub fn from_flag_with_wakeup(
        flag: Arc<AtomicBool>,
    ) -> io::Result<(Self, std::os::unix::net::UnixStream)> {
        let wake = Self::wake_pair()?;
        let writer = wake.1.try_clone()?;
        Ok((
            Self {
                flag,
                interest: None,
                wake: Some(Arc::new(wake)),
            },
            writer,
        ))
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn wake_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        use std::os::fd::AsFd as _;
        self.wake
            .as_ref()
            .filter(|_| self.interest.is_none())
            .map(|wake| wake.0.as_fd())
    }

    pub fn is_cancelled(&self) -> bool {
        self.check().is_err()
    }

    pub(crate) fn check(&self) -> Result<(), BuildError> {
        if self.flag.load(Ordering::Acquire) {
            return Err(BuildError::Cancelled);
        }
        #[cfg(target_os = "linux")]
        if let Some(interest) = &self.interest {
            return interest.check();
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub(super) fn check_publication(&self) -> Result<(), BuildError> {
        self.check()?;
        if let Some(interest) = &self.interest {
            interest.check_publication()?;
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub(super) fn producer(interest: Arc<super::shared::Interest>) -> Self {
        Self {
            interest: Some(interest),
            ..Self::default()
        }
    }
}
