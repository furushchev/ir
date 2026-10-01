//! Git provider: everything through the `git` CLI.
//!
//! * Cache: one bare mirror per URL (`git clone --bare`), refreshed with
//!   `git fetch`.
//! * Version resolution: branch/tag names are resolved against the mirror's
//!   refs; raw hashes are verified with `cat-file -e` (with a best-effort
//!   direct fetch first).
//! * Materialization: `git clone --no-checkout <bare> <dest>` (fast local
//!   clone) followed by `git checkout --detach <rev>`.
//!
//! Authentication is delegated to git itself (credential helpers, ssh-agent,
//! ...). `GIT_TERMINAL_PROMPT=0` is set so non-interactive runs fail fast
//! instead of hanging on a password prompt.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cache::{fetch_interval_from_env, Cache, CacheEntry};
use crate::error::{IrError, Result};
use crate::manifest::{RepoSpec, SourceKind, VersionSpec};
use crate::process::{self, RetryPolicy, DEFAULT_TIMEOUT, LOCAL_TIMEOUT};
use crate::provider::{Provider, Resolved, WorktreeState};

pub struct GitProvider;

impl GitProvider {
    fn git_cmd() -> Command {
        let mut cmd = Command::new("git");
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        cmd
    }

    /// The mirror's recorded origin URL, if any.
    fn origin_url(bare: &Path) -> Option<String> {
        let mut cmd = Self::git_cmd();
        cmd.arg("-C")
            .arg(bare)
            .args(["config", "--get", "remote.origin.url"]);
        process::run(&mut cmd, LOCAL_TIMEOUT)
            .ok()
            .map(|o| o.stdout_trimmed())
            .filter(|s| !s.is_empty())
    }

    fn bare_of(entry: &CacheEntry) -> Result<&Path> {
        match entry {
            CacheEntry::BareRepo(p) => Ok(p),
            CacheEntry::Archive(_) | CacheEntry::Remote(_) => Err(IrError::Unsupported(
                "GitProvider got a non-mirror cache entry".into(),
            )),
        }
    }

    /// `git rev-parse --verify <rev>^{commit}`, returning `None` when the
    /// revision does not exist locally. Other failures (timeout, spawn)
    /// propagate.
    fn rev_parse(bare: &Path, rev: &str) -> Result<Option<String>> {
        let mut cmd = Self::git_cmd();
        cmd.arg("-C")
            .arg(bare)
            .args(["rev-parse", "--verify", &format!("{rev}^{{commit}}")]);
        match process::run(&mut cmd, LOCAL_TIMEOUT) {
            Ok(out) => Ok(Some(out.stdout_trimmed())),
            Err(IrError::CommandFailed { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn default_branch_rev(&self, bare: &Path, url: &str) -> Result<String> {
        // refs/remotes/origin/HEAD -> refs/remotes/origin/<branch>
        let mut cmd = Self::git_cmd();
        cmd.arg("-C")
            .arg(bare)
            .args(["symbolic-ref", "refs/remotes/origin/HEAD"]);
        if let Ok(out) = process::run(&mut cmd, LOCAL_TIMEOUT) {
            let target = out.stdout_trimmed();
            if let Some(branch) = target.strip_prefix("refs/remotes/origin/") {
                if let Some(hash) = Self::rev_parse(bare, &format!("refs/remotes/origin/{branch}"))?
                {
                    return Ok(hash);
                }
            }
        }
        for branch in ["main", "master"] {
            if let Some(hash) = Self::rev_parse(bare, &format!("refs/remotes/origin/{branch}"))? {
                return Ok(hash);
            }
        }
        Err(IrError::NoDefaultBranch(url.to_string()))
    }

    fn ref_rev(&self, bare: &Path, name: &str) -> Result<String> {
        // Best-effort refresh; if offline we still try the local refs.
        let mut fetch = Self::git_cmd();
        fetch
            .arg("-C")
            .arg(bare)
            .args(["fetch", "origin", "--prune"]);
        let _ = process::run(&mut fetch, DEFAULT_TIMEOUT);

        // Branches win over tags, mirroring `git checkout <name>` DWIM order.
        for r in [
            format!("refs/remotes/origin/{name}"),
            format!("refs/tags/{name}"),
        ] {
            if let Some(hash) = Self::rev_parse(bare, &r)? {
                return Ok(hash);
            }
        }
        Err(IrError::RevisionNotFound(name.to_string()))
    }

    fn commit_rev(&self, bare: &Path, hash: &str) -> Result<String> {
        if let Some(h) = Self::rev_parse(bare, hash)? {
            return Ok(h);
        }
        // Some servers allow fetching an arbitrary sha1 directly.
        let mut fetch = Self::git_cmd();
        fetch
            .arg("-C")
            .arg(bare)
            .args(["fetch", "--depth", "1", "origin", hash]);
        let _ = process::run(&mut fetch, DEFAULT_TIMEOUT);
        Self::rev_parse(bare, hash)?.ok_or_else(|| IrError::RevisionNotFound(hash.to_string()))
    }
}

impl Provider for GitProvider {
    fn kind(&self) -> SourceKind {
        SourceKind::Git
    }

    fn ensure_cached(&self, cache: &Cache, spec: &RepoSpec) -> Result<CacheEntry> {
        let key = cache.key(&spec.normalized_url());
        let bare = cache.bare_dir(&key);
        // A previous failed run can leave a bare repo behind (e.g. `init`
        // succeeded but `fetch` failed), or the raw URL may have changed
        // while the normalized key stayed the same. Verify the origin
        // before reuse; otherwise a poisoned mirror fails forever.
        if !bare.join("HEAD").exists()
            || Self::origin_url(&bare).as_deref() != Some(spec.url.as_str())
        {
            if bare.exists() {
                fs::remove_dir_all(&bare)?;
            }
            if let Some(parent) = bare.parent() {
                fs::create_dir_all(parent)?;
            }
            // NOTE: `git clone --bare` from a local path copies refs/heads/*
            // directly instead of setting up remote-tracking refs, so we
            // init + remote add explicitly for a uniform ref layout.
            let bare_s = bare.to_string_lossy().into_owned();
            let mut init = Self::git_cmd();
            init.args(["init", "--bare", &bare_s]);
            process::run(&mut init, LOCAL_TIMEOUT)?;
            let mut remote = Self::git_cmd();
            remote
                .arg("-C")
                .arg(&bare)
                .args(["remote", "add", "origin", &spec.url]);
            process::run(&mut remote, LOCAL_TIMEOUT)?;
        }
        // Throttle network fetches: with --fetch-interval, a recent mirror
        // is reused without hitting the network (still self-repairing above).
        let interval = fetch_interval_from_env();
        if !cache.fetch_due(SourceKind::Git, &key, interval) {
            return Ok(CacheEntry::BareRepo(bare));
        }
        let mut fetch = Self::git_cmd();
        fetch
            .arg("-C")
            .arg(&bare)
            .args(["fetch", "origin", "--prune", "--tags"]);
        process::run_retry(&mut fetch, DEFAULT_TIMEOUT, &RetryPolicy::from_env())?;
        let _ = cache.mark_fetched(SourceKind::Git, &key);
        // Best-effort: point refs/remotes/origin/HEAD at the default branch
        // (only `git clone` sets it up automatically).
        let mut set_head = Self::git_cmd();
        set_head
            .arg("-C")
            .arg(&bare)
            .args(["remote", "set-head", "origin", "--auto"]);
        let _ = process::run(&mut set_head, LOCAL_TIMEOUT);
        Ok(CacheEntry::BareRepo(bare))
    }

    fn resolve(&self, entry: &CacheEntry, version: &VersionSpec) -> Result<Resolved> {
        let bare = Self::bare_of(entry)?;
        let hash = match version {
            VersionSpec::Default => self.default_branch_rev(bare, "<repo>")?,
            VersionSpec::Ref(name) => self.ref_rev(bare, name)?,
            VersionSpec::Revision(hash) => self.commit_rev(bare, hash)?,
            VersionSpec::Subdir(d) => {
                return Err(IrError::Unsupported(format!(
                    "in-archive subdirectory '{d}' is only valid for tar/zip sources"
                )));
            }
        };
        Ok(Resolved::Revision(hash))
    }

    fn read_file(
        &self,
        entry: &CacheEntry,
        resolved: &Resolved,
        rel: &str,
    ) -> Result<Option<Vec<u8>>> {
        let bare = Self::bare_of(entry)?;
        let rev = resolved.revision()?;
        let object = format!("{rev}:{rel}");
        let mut check = Self::git_cmd();
        check.arg("-C").arg(bare).args(["cat-file", "-e", &object]);
        if process::run(&mut check, LOCAL_TIMEOUT).is_err() {
            return Ok(None);
        }
        let mut show = Self::git_cmd();
        show.arg("-C").arg(bare).args(["show", &object]);
        let out = process::run(&mut show, LOCAL_TIMEOUT)?;
        Ok(Some(out.stdout))
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
        let bare = Self::bare_of(entry)?;
        let rev = resolved.revision()?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let bare_s = bare.to_string_lossy().into_owned();
        let dest_s = dest.to_string_lossy().into_owned();
        let mut clone = Self::git_cmd();
        clone.args(["clone", "--no-checkout", "--", &bare_s, &dest_s]);
        process::run(&mut clone, DEFAULT_TIMEOUT)?;

        let mut checkout = Self::git_cmd();
        checkout
            .arg("-C")
            .arg(dest)
            .args(["checkout", "--detach", "--quiet", rev]);
        if let Err(e) = process::run(&mut checkout, DEFAULT_TIMEOUT) {
            let _ = fs::remove_dir_all(dest);
            return Err(e);
        }
        if !spec.subpaths.is_empty() {
            let mut sparse = Self::git_cmd();
            sparse.arg("-C").arg(dest).arg("sparse-checkout").arg("set");
            for s in &spec.subpaths {
                sparse.arg(s);
            }
            process::run(&mut sparse, LOCAL_TIMEOUT)?;
        }
        Ok(())
    }

    fn status(&self, dest: &Path) -> Result<WorktreeState> {
        if !dest.join(".git").exists() {
            return Ok(WorktreeState {
                present: dest.exists(),
                dirty: false,
                current: None,
            });
        }
        let mut rp = Self::git_cmd();
        rp.arg("-C").arg(dest).args(["rev-parse", "HEAD"]);
        let current = process::run(&mut rp, LOCAL_TIMEOUT)
            .ok()
            .map(|o| o.stdout_trimmed());
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
        if !dest.join(".git").exists() {
            return Ok(None);
        }
        // NUL-separated porcelain: `XY path\0` per entry, no quoting issues.
        // --no-renames avoids the `old -> new` two-path rename entries.
        let mut st = Self::git_cmd();
        st.arg("-C").arg(dest).args([
            "status",
            "--porcelain=v1",
            "--no-renames",
            "-z",
            "--untracked-files=all",
        ]);
        let out = process::run(&mut st, LOCAL_TIMEOUT)?;
        let mut paths = Vec::new();
        for entry in out.stdout.split(|b| *b == 0) {
            if entry.is_empty() {
                continue;
            }
            // Unexpected shape: fail safe to "unknown" rather than misread.
            let Some(raw) = entry.get(3..).filter(|p| !p.is_empty()) else {
                return Ok(None);
            };
            paths.push(PathBuf::from(String::from_utf8_lossy(raw).into_owned()));
        }
        Ok(Some(paths))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::validate_relative_path;
    use tempfile::TempDir;

    /// Build a small git repo with two commits and a tag; return the tempdir
    /// (kept alive by the caller) and the HEAD hash.
    fn fixture_repo() -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        let run = |args: &[&str]| {
            let mut c = GitProvider::git_cmd();
            c.arg("-C").arg(p);
            for a in args {
                c.arg(a);
            }
            process::run(&mut c, LOCAL_TIMEOUT).unwrap()
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "test@ir"]);
        run(&["config", "user.name", "ir-test"]);
        fs::write(p.join("a.txt"), "hello\n").unwrap();
        fs::write(
            p.join(".repos"),
            "repositories:\n  child:\n    type: git\n    url: https://example.com/child.git\n",
        )
        .unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "first"]);
        fs::write(p.join("b.txt"), "world\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "second"]);
        run(&["tag", "v1.0"]);
        let head = run(&["rev-parse", "HEAD"]).stdout_trimmed();
        (dir, head)
    }

    fn spec_for(url: &str, version: VersionSpec) -> RepoSpec {
        RepoSpec {
            path: validate_relative_path("src/demo").unwrap(),
            kind: SourceKind::Git,
            url: url.to_string(),
            version,
            subpaths: vec![],
        }
    }

    #[test]
    fn ensure_cached_and_resolve() {
        let (repo_dir, head) = fixture_repo();
        let cache_dir = TempDir::new().unwrap();
        let cache = Cache::with_root(cache_dir.path().to_path_buf());
        let provider = GitProvider;
        let url = repo_dir.path().to_string_lossy().into_owned();

        let entry = provider
            .ensure_cached(&cache, &spec_for(&url, VersionSpec::Default))
            .unwrap();
        assert!(matches!(entry, CacheEntry::BareRepo(_)));

        // Default branch -> HEAD
        let r = provider.resolve(&entry, &VersionSpec::Default).unwrap();
        assert_eq!(r, Resolved::Revision(head.clone()));
        // Branch ref
        let r = provider
            .resolve(&entry, &VersionSpec::Ref("main".into()))
            .unwrap();
        assert_eq!(r, Resolved::Revision(head.clone()));
        // Tag ref
        let r = provider
            .resolve(&entry, &VersionSpec::Ref("v1.0".into()))
            .unwrap();
        assert_eq!(r, Resolved::Revision(head.clone()));
        // Raw hash
        let r = provider
            .resolve(&entry, &VersionSpec::Revision(head.clone()))
            .unwrap();
        assert_eq!(r, Resolved::Revision(head));
        // Missing ref
        assert!(provider
            .resolve(&entry, &VersionSpec::Ref("nope".into()))
            .is_err());
    }

    #[test]
    fn read_file_present_and_missing() {
        let (repo_dir, head) = fixture_repo();
        let cache_dir = TempDir::new().unwrap();
        let cache = Cache::with_root(cache_dir.path().to_path_buf());
        let provider = GitProvider;
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
        let (repo_dir, head) = fixture_repo();
        let cache_dir = TempDir::new().unwrap();
        let cache = Cache::with_root(cache_dir.path().to_path_buf());
        let ws_dir = TempDir::new().unwrap();
        let provider = GitProvider;
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

        // dirty after local modification
        fs::write(dest.join("a.txt"), "changed\n").unwrap();
        assert!(provider.status(&dest).unwrap().dirty);

        // materialize refuses an existing destination
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
