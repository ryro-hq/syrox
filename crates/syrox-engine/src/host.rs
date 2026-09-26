//! Operation-specific Linux capability evidence; active cgroup probes are opt-in.
use std::path::{Path, PathBuf};

use thiserror::Error;

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
#[path = "host/linux.rs"]
mod linux;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostCapabilityStatus {
    Available(String),
    Unavailable(String),
    NeedsActiveProbe(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCapability {
    pub name: &'static str,
    pub status: HostCapabilityStatus,
}

#[derive(Debug, Clone)]
pub struct HostInspection {
    pub cgroup_parent: PathBuf,
    pub capabilities: Vec<HostCapability>,
}

#[derive(Debug, Error)]
pub enum HostInspectError {
    #[error("host inspection requires Linux")]
    UnsupportedPlatform,
    #[error("cgroup parent must resolve beneath /sys/fs/cgroup")]
    InvalidCgroupParent,
}

/// Observe this process's current cgroup or an explicitly selected candidate
/// parent. No process is migrated, cgroup created, namespace entered or policy
/// installed by this diagnostic.
pub fn inspect_host(parent: Option<&Path>) -> Result<HostInspection, HostInspectError> {
    inspect_host_with_probe(parent, false)
}

/// With `probe_cgroup`, briefly create a child, verify its cpu/memory/pids
/// controls and attempt migration of a gated copy of the running `srx` into
/// that child. This tests this caller's source cgroup only; future callers
/// elsewhere still require an executor placed within the delegated subtree.
pub fn inspect_host_with_probe(
    parent: Option<&Path>,
    probe_cgroup: bool,
) -> Result<HostInspection, HostInspectError> {
    #[cfg(target_os = "linux")]
    {
        linux::inspect(parent, probe_cgroup)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (parent, probe_cgroup);
        Err(HostInspectError::UnsupportedPlatform)
    }
}
