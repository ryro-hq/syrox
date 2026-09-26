use super::BuildError;
use crate::{ContentDigest, Store};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildIndexReport {
    pub indexed: usize,
    pub skipped: usize,
    pub removed_stale: usize,
    pub conflicts: Vec<ContentDigest>,
}

/// Reconstruct the local action index from managed build roots. Referenced bytes
/// and receipt semantics are verified and retention durability is re-confirmed.
/// Journals and unrooted CAS objects never supply reconstruction authority.
pub fn rebuild_build_index(store: &Store) -> Result<BuildIndexReport, BuildError> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        super::cache::rebuild(store)
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = store;
        Err(BuildError::UnsupportedPlatform)
    }
}
