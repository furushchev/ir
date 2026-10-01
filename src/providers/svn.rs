//! Svn (Subversion) provider: everything through the `svn` CLI.
//!
//! Subversion has no offline mirror primitive, so there is no local cache:
//! `ensure_cached` only validates that the URL is reachable (`svn info`)
//! and returns a [`CacheEntry::Remote`]. Resolution and materialization talk
//! to the remote directly.
//!
//! * Default -> HEAD revision (`svn info`).
//! * Revision -> numeric revision (an optional leading `r` is accepted),
//!   verified with `svn info -r <rev>`.
//! * Ref -> unsupported: svn has no named refs. Point the URL at the
//!   branch/tag path instead (e.g. `.../branches/foo`).
//! * Materialization: `svn checkout -r <rev> <url> <dest>` (keeps `.svn`
//!   metadata so `status` can report the revision and dirtiness).
//!
//! `svn` runs with `--non-interactive` so it fails fast instead of hanging
//! on a password prompt; authentication is delegated to svn itself
//! (cached credentials, client certificates, ...).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cache::{Cache, CacheEntry};
use crate::error::{IrError, Result};
use crate::manifest::{RepoSpec, SourceKind, VersionSpec};
use crate::process::{self, RetryPolicy, DEFAULT_TIMEOUT, LOCAL_TIMEOUT};
use crate::provider::{Provider, Resolved, WorktreeState};

pub struct SvnProvider;

impl SvnProvider {
    fn svn_cmd() -> Command {
        let mut cmd = Command::new("svn");
        cmd.arg("--non-interactive");
        cmd
    }

    fn remote_of(entry: &CacheEntry) -> Result<&str> {
        match entry {
            CacheEntry::Remote(url) => Ok(url),
            _ => Err(IrError::Unsupported(
                "SvnProvider got a cached cache entry".into(),
            )),
        }
    }

    /// Parse the `Revision: N` line out of `svn info` output.
    fn parse_revision(output: &str) -> Option<String> {
        output
            .lines()
            .find_map(|l| l.strip_prefix("Revision: ").map(|n| n.trim().to_string()))
    }

    /// `svn info [-r <rev>] <target>`, returning the revision number, or
    /// `None` when the target/revision does not exist.
    fn info_revision(target: &str, rev: Option<&str>) -> Result<Option<String>> {
        let mut cmd = Self::svn_cmd();
        cmd.arg("info");
        if let Some(r) = rev {
            cmd.args(["-r", r]);
        }
        cmd.arg(target);
        match process::run_retry(&mut cmd, DEFAULT_TIMEOUT, &RetryPolicy::from_env()) {
            Ok(out) => Ok(Self::parse_revision(&out.stdout_trimmed())),
            Err(IrError::CommandFailed { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn unsupported_subdir(d: &str) -> IrError {
        IrError::Unsupported(format!(
            "in-archive subdirectory '{d}' is only valid for tar/zip sources"
        ))
    }
}

impl Provider for SvnProvider {
    fn kind(&self) -> SourceKind {
        SourceKind::Svn
    }

    fn ensure_cached(&self, cache: &Cache, spec: &RepoSpec) -> Result<CacheEntry> {
        // No local mirror for svn; just validate reachability. The cache key
        // still identifies the URL for the sync engine's per-URL locking.
        let _ = cache.key(&spec.normalized_url());
        let mut info = Self::svn_cmd();
        info.args(["info", &spec.url]);
        process::run_retry(&mut info, DEFAULT_TIMEOUT, &RetryPolicy::from_env()).map_err(|e| {
            match e {
                IrError::CommandFailed { .. } => {
                    IrError::Unsupported(format!("cannot reach svn repository '{}'", spec.url))
                }
                other => other,
            }
        })?;
        Ok(CacheEntry::Remote(spec.url.clone()))
    }

    fn resolve(&self, entry: &CacheEntry, version: &VersionSpec) -> Result<Resolved> {
        let url = Self::remote_of(entry)?;
        let rev = match version {
            VersionSpec::Default => Self::info_revision(url, None)?
                .ok_or_else(|| IrError::Unsupported(format!("cannot read HEAD of '{url}'")))?,
            VersionSpec::Revision(r) => {
                let digits = r
                    .strip_prefix('r')
                    .or_else(|| r.strip_prefix('R'))
                    .unwrap_or(r);
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(IrError::RevisionNotFound(r.clone()));
                }
                Self::info_revision(url, Some(digits))?
                    .ok_or_else(|| IrError::RevisionNotFound(r.clone()))?
            }
            VersionSpec::Ref(name) => {
                return Err(IrError::Unsupported(format!(
                    "svn has no named refs (got '{name}'); point the URL at the branch/tag path instead"
                )));
            }
            VersionSpec::Subdir(d) => return Err(Self::unsupported_subdir(d)),
        };
        Ok(Resolved::Revision(rev))
    }

    fn read_file(
        &self,
        entry: &CacheEntry,
        resolved: &Resolved,
        rel: &str,
    ) -> Result<Option<Vec<u8>>> {
        let url = Self::remote_of(entry)?;
        let rev = resolved.revision()?;
        let file_url = format!(
            "{}/{}",
            url.trim_end_matches('/'),
            rel.trim_start_matches('/')
        );
        let mut cat = Self::svn_cmd();
        cat.args(["cat", "-r", rev, &file_url]);
        match process::run_retry(&mut cat, DEFAULT_TIMEOUT, &RetryPolicy::from_env()) {
            Ok(out) => Ok(Some(out.stdout)),
            Err(IrError::CommandFailed { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn materialize(
        &self,
        entry: &CacheEntry,
        resolved: &Resolved,
        spec: &RepoSpec,
        dest: &Path,
    ) -> Result<()> {
        if dest.exists() {
            return Err(IrError::DestExists(dest.to_path_buf()));
        }
        if !spec.subpaths.is_empty() {
            return Err(IrError::Unsupported(
                "sparse subpaths are not supported for svn".into(),
            ));
        }
        let url = Self::remote_of(entry)?;
        let rev = resolved.revision()?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut co = Self::svn_cmd();
        co.args(["checkout", "-q", "-r", rev, url]).arg(dest);
        if let Err(e) = process::run_retry(&mut co, DEFAULT_TIMEOUT, &RetryPolicy::from_env()) {
            let _ = fs::remove_dir_all(dest);
            return Err(e);
        }
        Ok(())
    }

    fn status(&self, dest: &Path) -> Result<WorktreeState> {
        if !dest.join(".svn").exists() {
            return Ok(WorktreeState {
                present: dest.exists(),
                dirty: false,
                current: None,
            });
        }
        let current = Self::info_revision(&dest.to_string_lossy(), None)?;
        let dirty = self
            .changed_paths(dest)?
            .map(|p| !p.is_empty())
            .unwrap_or(true);
        Ok(WorktreeState {
            present: true,
            dirty,
            current,
        })
    }

    fn changed_paths(&self, dest: &Path) -> Result<Option<Vec<PathBuf>>> {
        if !dest.join(".svn").exists() {
            return Ok(None);
        }
        // NOTE: no `-q`: quiet mode hides unversioned (`?`) entries, and
        // untracked files count as dirty. Lines are `<7 status cols><space>
        // <path>`; run inside the checkout so paths are relative to it.
        let mut st = Self::svn_cmd();
        st.current_dir(dest).arg("status");
        let out = process::run(&mut st, LOCAL_TIMEOUT)?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut paths = Vec::new();
        for line in text.lines() {
            let bytes = line.as_bytes();
            // Unexpected shape: fail safe to "unknown" rather than misread.
            if bytes.len() <= 8 || bytes[7] != b' ' {
                return Ok(None);
            }
            paths.push(PathBuf::from(&line[8..]));
        }
        Ok(Some(paths))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::validate_relative_path;
    use std::process::Stdio;
    use tempfile::TempDir;

    fn svn_available() -> bool {
        Command::new("svn")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Build a local svn repo (file://) with two revisions.
    fn fixture_repo() -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        let wc = dir.path().join("wc");
        let run = |cmd: &str, args: &[&str]| {
            let mut c = Command::new(cmd);
            for a in args {
                c.arg(a);
            }
            c.env("SVN_EDITOR", "true");
            process::run(&mut c, LOCAL_TIMEOUT).unwrap()
        };
        run("svnadmin", &["create", &repo.to_string_lossy()]);
        let url = format!("file://{}", repo.to_string_lossy());
        run(
            "svn",
            &[
                "--non-interactive",
                "checkout",
                "-q",
                &url,
                &wc.to_string_lossy(),
            ],
        );
        fs::write(wc.join("a.txt"), "hello\n").unwrap();
        fs::write(
            wc.join(".repos"),
            "repositories:\n  child:\n    type: svn\n    url: https://example.com/child\n",
        )
        .unwrap();
        let wc_s = wc.to_string_lossy().into_owned();
        run(
            "svn",
            &[
                "--non-interactive",
                "add",
                "-q",
                &format!("{wc_s}/a.txt"),
                &format!("{wc_s}/.repos"),
            ],
        );
        run(
            "svn",
            &[
                "--non-interactive",
                "commit",
                "-q",
                "-m",
                "first",
                &wc.to_string_lossy(),
            ],
        );
        fs::write(wc.join("b.txt"), "world\n").unwrap();
        run(
            "svn",
            &[
                "--non-interactive",
                "add",
                "-q",
                &wc.join("b.txt").to_string_lossy(),
            ],
        );
        run(
            "svn",
            &[
                "--non-interactive",
                "commit",
                "-q",
                "-m",
                "second",
                &wc.to_string_lossy(),
            ],
        );
        (dir, url)
    }

    fn spec_for(url: &str, version: VersionSpec) -> RepoSpec {
        RepoSpec {
            path: validate_relative_path("src/demo").unwrap(),
            kind: SourceKind::Svn,
            url: url.to_string(),
            version,
            subpaths: vec![],
        }
    }

    #[test]
    fn ensure_cached_and_resolve() {
        if !svn_available() {
            eprintln!("skipping: svn is not installed");
            return;
        }
        let (_dir, url) = fixture_repo();
        let cache_dir = TempDir::new().unwrap();
        let cache = Cache::with_root(cache_dir.path().to_path_buf());
        let provider = SvnProvider;

        let entry = provider
            .ensure_cached(&cache, &spec_for(&url, VersionSpec::Default))
            .unwrap();
        assert!(matches!(entry, CacheEntry::Remote(_)));

        // Default -> HEAD (r2)
        let r = provider.resolve(&entry, &VersionSpec::Default).unwrap();
        assert_eq!(r, Resolved::Revision("2".into()));
        // Numeric revision
        let r = provider
            .resolve(&entry, &VersionSpec::Revision("1".into()))
            .unwrap();
        assert_eq!(r, Resolved::Revision("1".into()));
        // Leading 'r' accepted
        let r = provider
            .resolve(&entry, &VersionSpec::Revision("r2".into()))
            .unwrap();
        assert_eq!(r, Resolved::Revision("2".into()));
        // Nonexistent revision
        assert!(provider
            .resolve(&entry, &VersionSpec::Revision("99".into()))
            .is_err());
        // Non-numeric revision
        assert!(provider
            .resolve(&entry, &VersionSpec::Revision("abc".into()))
            .is_err());
        // Named refs unsupported
        assert!(provider
            .resolve(&entry, &VersionSpec::Ref("trunk".into()))
            .is_err());
    }

    #[test]
    fn read_file_present_and_missing() {
        if !svn_available() {
            eprintln!("skipping: svn is not installed");
            return;
        }
        let (_dir, url) = fixture_repo();
        let cache_dir = TempDir::new().unwrap();
        let cache = Cache::with_root(cache_dir.path().to_path_buf());
        let provider = SvnProvider;
        let entry = provider
            .ensure_cached(&cache, &spec_for(&url, VersionSpec::Default))
            .unwrap();

        let content = provider
            .read_file(&entry, &Resolved::Revision("2".into()), ".repos")
            .unwrap()
            .unwrap();
        assert!(String::from_utf8(content).unwrap().contains("child:"));

        // r1 has no .repos? No - .repos was added in r1. Use a missing name.
        assert!(provider
            .read_file(
                &entry,
                &Resolved::Revision("2".into()),
                "does-not-exist.repos"
            )
            .unwrap()
            .is_none());
    }

    #[test]
    fn materialize_and_status() {
        if !svn_available() {
            eprintln!("skipping: svn is not installed");
            return;
        }
        let (_dir, url) = fixture_repo();
        let cache_dir = TempDir::new().unwrap();
        let cache = Cache::with_root(cache_dir.path().to_path_buf());
        let ws_dir = TempDir::new().unwrap();
        let provider = SvnProvider;
        let entry = provider
            .ensure_cached(&cache, &spec_for(&url, VersionSpec::Default))
            .unwrap();
        // Pin r1: only a.txt exists there.
        let resolved = Resolved::Revision("1".into());

        let dest = ws_dir.path().join("src/demo");
        provider
            .materialize(
                &entry,
                &resolved,
                &spec_for(&url, VersionSpec::Default),
                &dest,
            )
            .unwrap();
        assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "hello\n");
        assert!(!dest.join("b.txt").exists());

        let st = provider.status(&dest).unwrap();
        assert!(st.present && !st.dirty);
        assert_eq!(st.current.as_deref(), Some("1"));

        fs::write(dest.join("a.txt"), "changed\n").unwrap();
        assert!(provider.status(&dest).unwrap().dirty);

        assert!(provider
            .materialize(
                &entry,
                &resolved,
                &spec_for(&url, VersionSpec::Default),
                &dest
            )
            .is_err());
    }
}
