//! Local cache layout.
//!
//! ```text
//! <cache>/ir/
//!   git/<sha256(url)>/        bare git mirrors, one per repository URL
//!   hg/<sha256(url)>/         local hg clones (--noupdate), one per URL
//!   bzr/<sha256(url)>/        local bzr branches, one per URL
//!   archives/<sha256(url)>.<ext>   downloaded tar/zip files
//! ```
//!
//! Subversion has no offline mirror primitive, so svn sources are resolved
//! and checked out from the remote directly ([`CacheEntry::Remote`]).
//!
//! The cache root defaults to `$XDG_CACHE_HOME/ir` (or `~/.cache/ir`).

use sha2::{Digest, Sha256};
use std::path::PathBuf;

use crate::error::Result;
use crate::manifest::SourceKind;

/// A local cache entry produced by [`Provider::ensure_cached`].
#[derive(Debug, Clone)]
pub enum CacheEntry {
    /// A local repository mirror (bare git mirror, hg clone, bzr branch).
    BareRepo(PathBuf),
    /// A downloaded archive file.
    Archive(PathBuf),
    /// No local mirror: operations go to the remote directly (svn).
    /// Holds the repository URL to use.
    Remote(String),
}

#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    /// Open the cache at its default location, creating directories lazily.
    pub fn new() -> Result<Self> {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".cache")))
            .ok_or_else(|| {
                crate::error::IrError::Unsupported(
                    "cannot determine cache directory: set XDG_CACHE_HOME or HOME".into(),
                )
            })?;
        Ok(Self::with_root(base.join("ir")))
    }

    /// Open a cache at an explicit root (used by tests).
    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    /// Stable cache key for a normalized URL: hex(sha256(url)).
    pub fn key(&self, normalized_url: &str) -> String {
        let mut h = Sha256::new();
        h.update(normalized_url.as_bytes());
        hex::encode(h.finalize())
    }

    /// Directory holding the bare mirror for `key`.
    pub fn bare_dir(&self, key: &str) -> PathBuf {
        self.vcs_dir(SourceKind::Git, key)
    }

    /// Directory holding the local VCS mirror for `key` (`git/`, `hg/`,
    /// `bzr/`; svn keeps no local mirror).
    pub fn vcs_dir(&self, kind: SourceKind, key: &str) -> PathBuf {
        let name = match kind {
            SourceKind::Git => "git",
            SourceKind::Hg => "hg",
            SourceKind::Svn => "svn",
            SourceKind::Bzr => "bzr",
            SourceKind::Tar | SourceKind::Zip => "archives",
        };
        self.root.join(name).join(key)
    }

    /// File path for a downloaded archive.
    pub fn archive_path(&self, key: &str, ext: &str) -> PathBuf {
        self.root.join("archives").join(format!("{key}.{ext}"))
    }
}
