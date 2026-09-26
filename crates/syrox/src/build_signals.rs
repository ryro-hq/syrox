use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use signal_hook::{
    SigId,
    consts::{SIGINT, SIGTERM},
    flag, low_level,
};
use syrox_engine::BuildCancellation;

/// Handlers only set atomics; process control and cleanup stay on the normal
/// build path. A second signal also requests cancellation, never bypasses cleanup.
pub(super) struct BuildSignals {
    registrations: Vec<SigId>,
    signal: Arc<AtomicUsize>,
    pub(super) cancellation: BuildCancellation,
}

impl BuildSignals {
    pub(super) fn install() -> std::io::Result<Self> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = Arc::new(AtomicUsize::new(0));
        #[cfg(target_os = "linux")]
        let (cancellation, wake) = BuildCancellation::from_flag_with_wakeup(cancelled.clone())?;
        #[cfg(not(target_os = "linux"))]
        let cancellation = BuildCancellation::from_flag(cancelled.clone());
        let mut guard = Self {
            registrations: Vec::new(),
            signal,
            cancellation,
        };
        for number in [SIGINT, SIGTERM] {
            guard.registrations.push(flag::register_usize(
                number,
                guard.signal.clone(),
                usize::try_from(number).expect("positive signal"),
            )?);
            guard
                .registrations
                .push(flag::register(number, cancelled.clone())?);
            #[cfg(target_os = "linux")]
            guard
                .registrations
                .push(low_level::pipe::register(number, wake.try_clone()?)?);
        }
        Ok(guard)
    }

    pub(super) fn cancelled_exit_code(&self) -> u8 {
        match self.signal.load(Ordering::Acquire) {
            15 => 143,
            _ => 130,
        }
    }
}

impl Drop for BuildSignals {
    fn drop(&mut self) {
        for registration in self.registrations.drain(..) {
            low_level::unregister(registration);
        }
    }
}
