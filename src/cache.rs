//! Local cache layout.
//!
//! ```text
//! <cache>/ir/
//!   git/<sha256(url)>/        bare mirrors, one per repository URL
//!   archives/<sha256(url)>.<ext>   downloaded tar/zip files
//! ```
//!
//! The cache root defaults to `$XDG_CACHE_HOME/ir` (or `~/.cache/ir`).

use sha2::{Digest, Sha256};
use std::path::PathBuf;

use crate::error::Result;

/// A local cache entry produced by [`Provider::ensure_cached`].
#[derive(Debug, Clone)]
pub enum CacheEntry {
    /// A bare repository mirror.
    BareRepo(PathBuf),
    /// A downloaded archive file.
    Archive(PathBuf),
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
        self.root.join("git").join(key)
    }

    /// File path for a downloaded archive.
    pub fn archive_path(&self, key: &str, ext: &str) -> PathBuf {
        self.root.join("archives").join(format!("{key}.{ext}"))
    }
}
