//! Local cache layout.
//!
//! ```text
//! <cache>/ir/
//!   git/<sha256(url)>/        bare git mirrors, one per repository URL
//!   git/<sha256(url)>.fetch-stamp   last successful fetch (for --fetch-interval)
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
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use crate::error::Result;
use crate::manifest::SourceKind;

/// Parse a human duration like "30s", "10m", "1h" (a bare number means
/// seconds). Returns `None` on invalid input.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (num, unit) = match s.strip_suffix(['s', 'm', 'h']) {
        Some(n) => (n, &s[s.len() - 1..]),
        None => (s, "s"),
    };
    // "10m" -> ("10", "m"); a bare "s"/"m"/"h" leaves an empty number.
    let secs: u64 = num.parse().ok()?;
    Some(match unit {
        "s" => Duration::from_secs(secs),
        "m" => Duration::from_secs(secs * 60),
        "h" => Duration::from_secs(secs * 3600),
        _ => return None,
    })
}

/// Fetch-throttle interval from `IR_FETCH_INTERVAL` ("10m", "1h", "30s";
/// bare number = seconds). "0" or unset means every run fetches.
/// The `--fetch-interval` CLI flag sets this variable.
pub fn fetch_interval_from_env() -> Duration {
    std::env::var("IR_FETCH_INTERVAL")
        .ok()
        .and_then(|v| parse_duration(&v))
        .unwrap_or(Duration::ZERO)
}

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

    /// Marker file recording the last successful network fetch for `key`
    /// (a sibling of the mirror directory).
    pub fn fetch_stamp(&self, kind: SourceKind, key: &str) -> PathBuf {
        self.vcs_dir(kind, key).with_extension("fetch-stamp")
    }

    /// Whether a network fetch is due: true when the throttle interval is
    /// zero, no successful fetch has been recorded, or the recorded fetch
    /// is older than `interval`.
    pub fn fetch_due(&self, kind: SourceKind, key: &str, interval: Duration) -> bool {
        if interval.is_zero() {
            return true;
        }
        match fs::metadata(self.fetch_stamp(kind, key)).and_then(|m| m.modified()) {
            Ok(t) => t.elapsed().map(|e| e >= interval).unwrap_or(true),
            Err(_) => true,
        }
    }

    /// Record a successful network fetch for `key`.
    pub fn mark_fetched(&self, kind: SourceKind, key: &str) -> Result<()> {
        let stamp = self.fetch_stamp(kind, key);
        if let Some(parent) = stamp.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&stamp, b"")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("10m"), Some(Duration::from_secs(600)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("45"), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration("0"), Some(Duration::ZERO));
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("m"), None);
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("-5s"), None);
        assert_eq!(parse_duration("10x"), None);
    }

    #[test]
    fn fetch_interval_from_env_default_zero() {
        std::env::remove_var("IR_FETCH_INTERVAL");
        assert_eq!(fetch_interval_from_env(), Duration::ZERO);
    }

    #[test]
    fn fetch_interval_from_env_parses() {
        std::env::set_var("IR_FETCH_INTERVAL", "10m");
        assert_eq!(fetch_interval_from_env(), Duration::from_secs(600));
        std::env::remove_var("IR_FETCH_INTERVAL");
    }

    #[test]
    fn fetch_due_and_mark_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CACHE_HOME", dir.path());
        let cache = Cache::new().unwrap();
        let key = "abc123";
        let interval = Duration::from_secs(3600);
        // No stamp yet: due.
        assert!(cache.fetch_due(SourceKind::Git, key, interval));
        // Zero interval: always due.
        cache.mark_fetched(SourceKind::Git, key).unwrap();
        assert!(cache.fetch_due(SourceKind::Git, key, Duration::ZERO));
        // Fresh stamp: not due.
        assert!(!cache.fetch_due(SourceKind::Git, key, interval));
        std::env::remove_var("XDG_CACHE_HOME");
    }
}
