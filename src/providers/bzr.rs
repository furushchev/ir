//! Bzr (Bazaar) provider: everything through the `bzr` CLI.
//!
//! * Cache: one local branch per URL (`bzr branch`), refreshed with a
//!   best-effort `bzr pull`.
//! * Version resolution: integer revnos; default = latest (`bzr revno`),
//!   refs = tag names (`bzr revno -r tag:<name>`), revisions verified and
//!   normalized with `bzr revno -r <rev>`.
//! * Materialization: `bzr branch -r <revno> <mirror> <dest>` (keeps `.bzr`
//!   metadata for `status`).
//! * Status: `bzr revno` for the current revision, `bzr status --short`
//!   for dirtiness.
//!
//! Breezy's `brz` accepts the same arguments; symlink or alias it to `bzr`
//! if that is what you have installed. Sparse subpaths are not supported
//! for bzr. Authentication is delegated to bzr itself (ssh-agent, ...).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cache::{fetch_interval_from_env, Cache, CacheEntry};
use crate::error::{IrError, Result};
use crate::manifest::{RepoSpec, SourceKind, VersionSpec};
use crate::process::{self, RetryPolicy, DEFAULT_TIMEOUT, LOCAL_TIMEOUT};
use crate::provider::{Provider, Resolved, WorktreeState};

pub struct BzrProvider;

impl BzrProvider {
    /// Breezy (the maintained Bazaar fork) installs as `brz`; classic
    /// Bazaar installs as `bzr`. Prefer `bzr` when both exist.
    fn bzr_cmd() -> Command {
        static BIN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let bin = BIN.get_or_init(|| {
            std::env::var_os("PATH")
                .and_then(|paths| {
                    std::env::split_paths(&paths)
                        .flat_map(|dir| ["bzr", "brz"].iter().map(move |n| (dir.clone(), n)))
                        .find(|(dir, name)| dir.join(name).is_file())
                        .map(|(_, name)| name.to_string())
                })
                .unwrap_or_else(|| "bzr".to_string())
        });
        Command::new(bin)
    }

    /// Run `args` with the working directory set to `dir`.
    fn run_in(
        dir: &Path,
        args: &[&str],
        timeout: std::time::Duration,
    ) -> Result<process::CmdOutput> {
        let mut cmd = Self::bzr_cmd();
        cmd.current_dir(dir);
        for a in args {
            cmd.arg(a);
        }
        process::run(&mut cmd, timeout)
    }

    fn run_in_retry(
        dir: &Path,
        args: &[&str],
        timeout: std::time::Duration,
    ) -> Result<process::CmdOutput> {
        let mut cmd = Self::bzr_cmd();
        cmd.current_dir(dir);
        for a in args {
            cmd.arg(a);
        }
        process::run_retry(&mut cmd, timeout, &RetryPolicy::from_env())
    }

    fn mirror_of(entry: &CacheEntry) -> Result<&Path> {
        match entry {
            CacheEntry::BareRepo(p) => Ok(p),
            _ => Err(IrError::Unsupported(
                "BzrProvider got a non-mirror cache entry".into(),
            )),
        }
    }

    /// `bzr revno [-r <rev>]`, returning `None` when the revision does not
    /// exist. Other failures (timeout, spawn) propagate.
    fn revno(dir: &Path, rev: Option<&str>) -> Result<Option<String>> {
        let mut args = vec!["revno"];
        if let Some(r) = rev {
            args.extend(["-r", r]);
        }
        match Self::run_in(dir, &args, LOCAL_TIMEOUT) {
            Ok(out) => {
                let n = out.stdout_trimmed();
                if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                    Ok(None)
                } else {
                    Ok(Some(n))
                }
            }
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

impl Provider for BzrProvider {
    fn kind(&self) -> SourceKind {
        SourceKind::Bzr
    }

    fn ensure_cached(&self, cache: &Cache, spec: &RepoSpec) -> Result<CacheEntry> {
        let key = cache.key(&spec.normalized_url());
        let mirror = cache.vcs_dir(SourceKind::Bzr, &key);
        if !mirror.join(".bzr").exists() {
            if mirror.exists() {
                fs::remove_dir_all(&mirror)?;
            }
            if let Some(parent) = mirror.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut branch = Self::bzr_cmd();
            branch.arg("branch").arg(&spec.url).arg(&mirror);
            process::run_retry(&mut branch, DEFAULT_TIMEOUT, &RetryPolicy::from_env())?;
            let _ = cache.mark_fetched(SourceKind::Bzr, &key);
        }
        let interval = fetch_interval_from_env();
        if cache.fetch_due(SourceKind::Bzr, &key, interval) {
            let _ = Self::run_in_retry(&mirror, &["pull"], DEFAULT_TIMEOUT);
            let _ = cache.mark_fetched(SourceKind::Bzr, &key);
        }
        Ok(CacheEntry::BareRepo(mirror))
    }

    fn resolve(&self, entry: &CacheEntry, version: &VersionSpec) -> Result<Resolved> {
        let mirror = Self::mirror_of(entry)?;
        let revno = match version {
            VersionSpec::Default => Self::revno(mirror, None)?
                .ok_or_else(|| IrError::NoDefaultBranch("<repo>".into()))?,
            // Normalized to the canonical revno (accepts revids too).
            VersionSpec::Revision(r) => {
                Self::revno(mirror, Some(r))?.ok_or_else(|| IrError::RevisionNotFound(r.clone()))?
            }
            // Refs are tag names.
            VersionSpec::Ref(name) => Self::revno(mirror, Some(&format!("tag:{name}")))?
                .ok_or_else(|| IrError::RevisionNotFound(name.clone()))?,
            VersionSpec::Subdir(d) => return Err(Self::unsupported_subdir(d)),
        };
        Ok(Resolved::Revision(revno))
    }

    fn read_file(
        &self,
        entry: &CacheEntry,
        resolved: &Resolved,
        rel: &str,
    ) -> Result<Option<Vec<u8>>> {
        let mirror = Self::mirror_of(entry)?;
        let rev = resolved.revision()?;
        match Self::run_in(mirror, &["cat", "-r", rev, rel], LOCAL_TIMEOUT) {
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
                "sparse subpaths are not supported for bzr".into(),
            ));
        }
        let mirror = Self::mirror_of(entry)?;
        let rev = resolved.revision()?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut branch = Self::bzr_cmd();
        branch.args(["branch", "-r", rev]).arg(mirror).arg(dest);
        if let Err(e) = process::run(&mut branch, DEFAULT_TIMEOUT) {
            let _ = fs::remove_dir_all(dest);
            return Err(e);
        }
        Ok(())
    }

    fn status(&self, dest: &Path) -> Result<WorktreeState> {
        if !dest.join(".bzr").exists() {
            return Ok(WorktreeState {
                present: dest.exists(),
                dirty: false,
                current: None,
            });
        }
        let current = Self::revno(dest, None)?;
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
        if !dest.join(".bzr").exists() {
            return Ok(None);
        }
        // Short status lines look like ` M  a.txt`, `?   sub/`, or
        // `RM  old => new`: a 4-char status prefix, then the path(s).
        // A trailing `+` marks a nested bzr tree (a subdirectory that is
        // itself a branch); strip it so the path matches the directory,
        // but only when it really is a nested tree -- a plain file can
        // legitimately end in `+`.
        // Run inside the branch so the reported paths are relative to it.
        let out = Self::run_in(dest, &["status", "--short"], LOCAL_TIMEOUT)?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut paths = Vec::new();
        for line in text.lines() {
            // Unexpected shape: fail safe to "unknown" rather than misread.
            if line.len() < 5 {
                return Ok(None);
            }
            let rest = &line[4..];
            let mut push = |p: &str| {
                let stripped = p.strip_suffix('+').unwrap_or(p);
                let path = if stripped != p && dest.join(stripped).join(".bzr").is_dir() {
                    stripped
                } else {
                    p
                };
                paths.push(PathBuf::from(path));
            };
            match rest.split_once(" => ") {
                Some((old, new)) => {
                    push(old);
                    push(new);
                }
                None => push(rest),
            }
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

    fn bzr_available() -> bool {
        BzrProvider::bzr_cmd()
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Build a small bzr branch with two commits and a tag.
    fn fixture_repo() -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        let run = |args: &[&str]| {
            let mut c = BzrProvider::bzr_cmd();
            c.current_dir(p);
            for a in args {
                c.arg(a);
            }
            process::run(&mut c, LOCAL_TIMEOUT).unwrap()
        };
        run(&["init", "."]);
        run(&["whoami", "ir-test <ir@test>"]);
        fs::write(p.join("a.txt"), "hello\n").unwrap();
        fs::write(
            p.join(".repos"),
            "repositories:\n  child:\n    type: bzr\n    url: https://example.com/child\n",
        )
        .unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "first"]);
        fs::write(p.join("b.txt"), "world\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "second"]);
        run(&["tag", "v1.0"]);
        let url = p.to_string_lossy().into_owned();
        (dir, url)
    }

    fn spec_for(url: &str, version: VersionSpec) -> RepoSpec {
        RepoSpec {
            path: validate_relative_path("src/demo").unwrap(),
            kind: SourceKind::Bzr,
            url: url.to_string(),
            version,
            subpaths: vec![],
        }
    }

    #[test]
    fn ensure_cached_and_resolve() {
        if !bzr_available() {
            eprintln!("skipping: bzr is not installed");
            return;
        }
        let (repo_dir, url) = fixture_repo();
        let _ = repo_dir;
        let cache_dir = TempDir::new().unwrap();
        let cache = Cache::with_root(cache_dir.path().to_path_buf());
        let provider = BzrProvider;

        let entry = provider
            .ensure_cached(&cache, &spec_for(&url, VersionSpec::Default))
            .unwrap();

        // Default -> latest revno (2)
        let r = provider.resolve(&entry, &VersionSpec::Default).unwrap();
        assert_eq!(r, Resolved::Revision("2".into()));
        // Tag ref
        let r = provider
            .resolve(&entry, &VersionSpec::Ref("v1.0".into()))
            .unwrap();
        assert_eq!(r, Resolved::Revision("2".into()));
        // Numeric revision
        let r = provider
            .resolve(&entry, &VersionSpec::Revision("1".into()))
            .unwrap();
        assert_eq!(r, Resolved::Revision("1".into()));
        // Missing revision / tag
        assert!(provider
            .resolve(&entry, &VersionSpec::Revision("99".into()))
            .is_err());
        assert!(provider
            .resolve(&entry, &VersionSpec::Ref("nope".into()))
            .is_err());
    }

    #[test]
    fn read_file_present_and_missing() {
        if !bzr_available() {
            eprintln!("skipping: bzr is not installed");
            return;
        }
        let (repo_dir, url) = fixture_repo();
        let _ = repo_dir;
        let cache_dir = TempDir::new().unwrap();
        let cache = Cache::with_root(cache_dir.path().to_path_buf());
        let provider = BzrProvider;
        let entry = provider
            .ensure_cached(&cache, &spec_for(&url, VersionSpec::Default))
            .unwrap();
        let resolved = Resolved::Revision("2".into());

        let content = provider
            .read_file(&entry, &resolved, ".repos")
            .unwrap()
            .unwrap();
        assert!(String::from_utf8(content).unwrap().contains("child:"));

        assert!(provider
            .read_file(&entry, &resolved, "does-not-exist.repos")
            .unwrap()
            .is_none());
    }

    #[test]
    fn materialize_and_status() {
        if !bzr_available() {
            eprintln!("skipping: bzr is not installed");
            return;
        }
        let (repo_dir, url) = fixture_repo();
        let _ = repo_dir;
        let cache_dir = TempDir::new().unwrap();
        let cache = Cache::with_root(cache_dir.path().to_path_buf());
        let ws_dir = TempDir::new().unwrap();
        let provider = BzrProvider;
        let entry = provider
            .ensure_cached(&cache, &spec_for(&url, VersionSpec::Default))
            .unwrap();
        // Pin revno 1: only a.txt exists there.
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
