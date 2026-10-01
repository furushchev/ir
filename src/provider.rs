//! The [`Provider`] trait: a uniform interface over VCS tools and archives.
//!
//! Every source kind (git/hg/svn/bzr via their native CLIs, tar/zip via
//! download+extract) implements this trait. The resolver and the sync engine
//! only speak to providers through it.

use std::path::{Path, PathBuf};

use crate::cache::{Cache, CacheEntry};
use crate::error::{IrError, Result};
use crate::manifest::{RepoSpec, SourceKind, VersionSpec};

/// An exactly-pinned version, ready to be written to the lockfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// A VCS revision (commit hash, ...).
    Revision(String),
    /// An archive: content hash plus the optional in-archive subdirectory.
    Archive {
        sha256: String,
        subdir: Option<PathBuf>,
    },
}

impl Resolved {
    /// The revision string for VCS kinds; errors for archives.
    pub fn revision(&self) -> Result<&str> {
        match self {
            Resolved::Revision(r) => Ok(r),
            Resolved::Archive { .. } => Err(IrError::Unsupported(
                "expected a VCS revision, got an archive".into(),
            )),
        }
    }
}

/// Observed state of a materialized working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeState {
    /// The destination directory exists.
    pub present: bool,
    /// Local modifications (or unknown provenance) detected.
    pub dirty: bool,
    /// Currently checked-out revision / archive sha256, if known.
    pub current: Option<String>,
}

pub trait Provider: Send + Sync {
    fn kind(&self) -> SourceKind;

    /// Make the remote content available locally (bare mirror or downloaded
    /// archive), fetching/updating as needed.
    fn ensure_cached(&self, cache: &Cache, spec: &RepoSpec) -> Result<CacheEntry>;

    /// Pin a [`VersionSpec`] to an exact [`Resolved`] version.
    fn resolve(&self, entry: &CacheEntry, version: &VersionSpec) -> Result<Resolved>;

    /// Read a file (e.g. `.repos` / `.rosinstall`) at the resolved version
    /// without a full checkout. Returns `None` when the file does not exist.
    fn read_file(
        &self,
        entry: &CacheEntry,
        resolved: &Resolved,
        rel: &str,
    ) -> Result<Option<Vec<u8>>>;

    /// Materialize the resolved version into `dest` (must not exist yet).
    fn materialize(
        &self,
        entry: &CacheEntry,
        resolved: &Resolved,
        spec: &RepoSpec,
        dest: &Path,
    ) -> Result<()>;

    /// Inspect the current state of `dest`.
    fn status(&self, dest: &Path) -> Result<WorktreeState>;

    /// Relative paths (to `dest`) with local changes: modified, added,
    /// removed, or untracked files. The sync engine uses this to tell
    /// ir-managed nested repositories apart from real local modifications:
    /// changes strictly under a nested repository path do not mark the
    /// parent dirty. Returns `None` when the provider cannot enumerate
    /// changes; the engine then falls back to [`WorktreeState::dirty`].
    fn changed_paths(&self, dest: &Path) -> Result<Option<Vec<PathBuf>>> {
        let _ = dest;
        Ok(None)
    }
}

/// Build the provider for a source kind.
pub fn provider_for(kind: SourceKind) -> Result<Box<dyn Provider>> {
    match kind {
        SourceKind::Git => Ok(Box::new(crate::providers::GitProvider)),
        SourceKind::Hg => Ok(Box::new(crate::providers::HgProvider)),
        SourceKind::Svn => Ok(Box::new(crate::providers::SvnProvider)),
        SourceKind::Bzr => Ok(Box::new(crate::providers::BzrProvider)),
        SourceKind::Tar => Ok(Box::new(crate::providers::TarProvider)),
        SourceKind::Zip => Ok(Box::new(crate::providers::ZipProvider)),
    }
}
