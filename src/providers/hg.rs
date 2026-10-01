//! Hg (Mercurial) provider: everything through the `hg` CLI.
//!
//! * Cache: one local clone per URL (`hg clone --noupdate`), refreshed with
//!   a best-effort `hg pull`.
//! * Version resolution: branch/tag/bookmark names resolve natively via
//!   `hg log -r <name>`; hashes are verified and expanded to full node ids;
//!   default = newest commit on the `default` branch (falling back to `tip`).
//! * Materialization: `hg clone --noupdate <mirror> <dest>` followed by
//!   `hg update -r <node>` (keeps `.hg` metadata for `status`).
//! * Status: `hg log -r .` for the current node, `hg status` for dirtiness
//!   (untracked files count as dirty, like `git status --porcelain`).
//!
//! Sparse subpaths are not supported for hg. Authentication is delegated to
//! hg itself (auth config, ssh-agent, ...).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cache::{fetch_interval_from_env, Cache, CacheEntry};
use crate::error::{IrError, Result};
use crate::manifest::{RepoSpec, SourceKind, VersionSpec};
use crate::process::{self, RetryPolicy, DEFAULT_TIMEOUT, LOCAL_TIMEOUT};
use crate::provider::{Provider, Resolved, WorktreeState};

pub struct HgProvider;

impl HgProvider {
    fn hg_cmd() -> Command {
        Command::new("hg")
    }

    fn mirror_of(entry: &CacheEntry) -> Result<&Path> {
        match entry {
            CacheEntry::BareRepo(p) => Ok(p),
            _ => Err(IrError::Unsupported(
                "HgProvider got a non-mirror cache entry".into(),
            )),
        }
    }

    /// `hg log -r <rev> --template "{node}"`, returning `None` when the
    /// revision does not exist. Other failures (timeout, spawn) propagate.
    fn log_node(repo: &Path, rev: &str) -> Result<Option<String>> {
        let mut cmd = Self::hg_cmd();
        cmd.arg("-R")
            .arg(repo)
            .args(["log", "-r", rev, "--template", "{node}\n"]);
        match process::run(&mut cmd, LOCAL_TIMEOUT) {
            Ok(out) => {
                let node = out.stdout_trimmed();
                if node.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(node))
                }
            }
            Err(IrError::CommandFailed { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn default_node(&self, mirror: &Path) -> Result<String> {
        // Newest commit on the `default` branch; `hg log` lists newest first.
        if let Some(node) = Self::log_node(mirror, "max(branch(default))")? {
            return Ok(node);
        }
        Self::log_node(mirror, "tip")?.ok_or_else(|| IrError::NoDefaultBranch("<repo>".into()))
    }

    fn unsupported_subdir(d: &str) -> IrError {
        IrError::Unsupported(format!(
            "in-archive subdirectory '{d}' is only valid for tar/zip sources"
        ))
    }
}

impl Provider for HgProvider {
    fn kind(&self) -> SourceKind {
        SourceKind::Hg
    }

    fn ensure_cached(&self, cache: &Cache, spec: &RepoSpec) -> Result<CacheEntry> {
        let key = cache.key(&spec.normalized_url());
        let mirror = cache.vcs_dir(SourceKind::Hg, &key);
        if !mirror.join(".hg").exists() {
            if mirror.exists() {
                fs::remove_dir_all(&mirror)?;
            }
            if let Some(parent) = mirror.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut clone = Self::hg_cmd();
            clone.args(["clone", "--noupdate", &spec.url]).arg(&mirror);
            process::run_retry(&mut clone, DEFAULT_TIMEOUT, &RetryPolicy::from_env())?;
            let _ = cache.mark_fetched(SourceKind::Hg, &key);
        }
        let interval = fetch_interval_from_env();
        if cache.fetch_due(SourceKind::Hg, &key, interval) {
            let mut pull = Self::hg_cmd();
            pull.arg("-R").arg(&mirror).arg("pull");
            let _ = process::run_retry(&mut pull, DEFAULT_TIMEOUT, &RetryPolicy::from_env());
            let _ = cache.mark_fetched(SourceKind::Hg, &key);
        }
        Ok(CacheEntry::BareRepo(mirror))
    }

    fn resolve(&self, entry: &CacheEntry, version: &VersionSpec) -> Result<Resolved> {
        let mirror = Self::mirror_of(entry)?;
        let node = match version {
            VersionSpec::Default => self.default_node(mirror)?,
            VersionSpec::Ref(name) => Self::log_node(mirror, name)?
                .ok_or_else(|| IrError::RevisionNotFound(name.clone()))?,
            // hg accepts unique hash prefixes; pin the full node id.
            VersionSpec::Revision(hash) => Self::log_node(mirror, hash)?
                .ok_or_else(|| IrError::RevisionNotFound(hash.clone()))?,
            VersionSpec::Subdir(d) => return Err(Self::unsupported_subdir(d)),
        };
        Ok(Resolved::Revision(node))
    }

    fn read_file(
        &self,
        entry: &CacheEntry,
        resolved: &Resolved,
        rel: &str,
    ) -> Result<Option<Vec<u8>>> {
        let mirror = Self::mirror_of(entry)?;
        let rev = resolved.revision()?;
        // NOTE: hg resolves file arguments against the current working
        // directory, not against `-R`, so run from the mirror root.
        let mut cat = Self::hg_cmd();
        cat.current_dir(mirror).args(["cat", "-r", rev, rel]);
        match process::run(&mut cat, LOCAL_TIMEOUT) {
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
                "sparse subpaths are not supported for hg".into(),
            ));
        }
        let mirror = Self::mirror_of(entry)?;
        let rev = resolved.revision()?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut clone = Self::hg_cmd();
        clone.args(["clone", "--noupdate"]).arg(mirror).arg(dest);
        process::run(&mut clone, DEFAULT_TIMEOUT)?;

        let mut update = Self::hg_cmd();
        update
            .arg("-R")
            .arg(dest)
            .args(["update", "-r", rev, "--clean", "--quiet"]);
        if let Err(e) = process::run(&mut update, DEFAULT_TIMEOUT) {
            let _ = fs::remove_dir_all(dest);
            return Err(e);
        }
        Ok(())
    }

    fn status(&self, dest: &Path) -> Result<WorktreeState> {
        if !dest.join(".hg").exists() {
            return Ok(WorktreeState {
                present: dest.exists(),
                dirty: false,
                current: None,
            });
        }
        let current = Self::log_node(dest, ".")?;
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
        if !dest.join(".hg").exists() {
            return Ok(None);
        }
        // Lines look like `M path`, `A path`, `? path`, ... Run inside the
        // repo so the reported paths are relative to it.
        let mut st = Self::hg_cmd();
        st.current_dir(dest).arg("status");
        let out = process::run(&mut st, LOCAL_TIMEOUT)?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut paths = Vec::new();
        for line in text.lines() {
            let bytes = line.as_bytes();
            // Unexpected shape: fail safe to "unknown" rather than misread.
            if bytes.len() < 3 || bytes[1] != b' ' {
                return Ok(None);
            }
            paths.push(PathBuf::from(&line[2..]));
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

    fn hg_available() -> bool {
        Command::new("hg")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Build a small hg repo with two commits and a tag.
    /// Returns the tempdir (kept alive by the caller), the tagged commit
    /// ("second") and the HEAD node (the `.hgtags` commit `hg tag` makes).
    fn fixture_repo() -> (TempDir, String, String) {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        // NOTE: hg resolves file arguments against the CWD, so run inside p.
        let run = |args: &[&str]| {
            let mut c = HgProvider::hg_cmd();
            c.current_dir(p);
            for a in args {
                c.arg(a);
            }
            process::run(&mut c, LOCAL_TIMEOUT).unwrap()
        };
        run(&["init", "."]);
        fs::write(p.join("a.txt"), "hello\n").unwrap();
        fs::write(
            p.join(".repos"),
            "repositories:\n  child:\n    type: hg\n    url: https://example.com/child\n",
        )
        .unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "first", "-u", "ir-test"]);
        fs::write(p.join("b.txt"), "world\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "second", "-u", "ir-test"]);
        let second = run(&["log", "-r", ".", "--template", "{node}\n"]).stdout_trimmed();
        run(&["tag", "v1.0"]);
        let head = run(&["log", "-r", ".", "--template", "{node}\n"]).stdout_trimmed();
        (dir, second, head)
    }

    fn spec_for(url: &str, version: VersionSpec) -> RepoSpec {
        RepoSpec {
            path: validate_relative_path("src/demo").unwrap(),
            kind: SourceKind::Hg,
            url: url.to_string(),
            version,
            subpaths: vec![],
        }
    }

    #[test]
    fn ensure_cached_and_resolve() {
        if !hg_available() {
            eprintln!("skipping: hg is not installed");
            return;
        }
        let (repo_dir, second, head) = fixture_repo();
        let cache_dir = TempDir::new().unwrap();
        let cache = Cache::with_root(cache_dir.path().to_path_buf());
        let provider = HgProvider;
        let url = repo_dir.path().to_string_lossy().into_owned();

        let entry = provider
            .ensure_cached(&cache, &spec_for(&url, VersionSpec::Default))
            .unwrap();

        // Default -> newest commit on the default branch (== HEAD here)
        let r = provider.resolve(&entry, &VersionSpec::Default).unwrap();
        assert_eq!(r, Resolved::Revision(head.clone()));
        // Tag ref -> the commit that was current when tagging ("second")
        let r = provider
            .resolve(&entry, &VersionSpec::Ref("v1.0".into()))
            .unwrap();
        assert_eq!(r, Resolved::Revision(second.clone()));
        // Full hash
        let r = provider
            .resolve(&entry, &VersionSpec::Revision(head.clone()))
            .unwrap();
        assert_eq!(r, Resolved::Revision(head.clone()));
        // Unique prefix resolves to the full node
        let r = provider
            .resolve(&entry, &VersionSpec::Revision(head[..12].to_string()))
            .unwrap();
        assert_eq!(r, Resolved::Revision(head));
        // Missing ref
        assert!(provider
            .resolve(&entry, &VersionSpec::Ref("nope".into()))
            .is_err());
    }

    #[test]
    fn read_file_present_and_missing() {
        if !hg_available() {
            eprintln!("skipping: hg is not installed");
            return;
        }
        let (repo_dir, _second, head) = fixture_repo();
        let cache_dir = TempDir::new().unwrap();
        let cache = Cache::with_root(cache_dir.path().to_path_buf());
        let provider = HgProvider;
        let url = repo_dir.path().to_string_lossy().into_owned();
        let entry = provider
            .ensure_cached(&cache, &spec_for(&url, VersionSpec::Default))
            .unwrap();
        let resolved = Resolved::Revision(head);

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
        if !hg_available() {
            eprintln!("skipping: hg is not installed");
            return;
        }
        let (repo_dir, _second, head) = fixture_repo();
        let cache_dir = TempDir::new().unwrap();
        let cache = Cache::with_root(cache_dir.path().to_path_buf());
        let ws_dir = TempDir::new().unwrap();
        let provider = HgProvider;
        let url = repo_dir.path().to_string_lossy().into_owned();
        let entry = provider
            .ensure_cached(&cache, &spec_for(&url, VersionSpec::Default))
            .unwrap();
        let resolved = Resolved::Revision(head.clone());

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

        let st = provider.status(&dest).unwrap();
        assert!(st.present && !st.dirty);
        assert_eq!(st.current.as_deref(), Some(head.as_str()));

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
