//! Sync engine: materialize every repository pinned in `ir.lock`.
//!
//! Repositories are synced in parallel with rayon. Cache fills are
//! serialized per URL (parallel `git fetch` calls into the same bare mirror
//! would fight over locks), everything else runs concurrently. Progress is
//! reported through [`SyncProgress`].
//!
//! Sync is deterministic: the lockfile pins exact revisions, so `sync`
//! never touches the network beyond refreshing the cache.

use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::cache::Cache;
use crate::error::{IrError, Result};
use crate::lock::LockedRepo;
use crate::manifest::{RepoSpec, SourceKind, VersionSpec};
use crate::progress::SyncProgress;
use crate::provider::{provider_for, Resolved};

#[derive(Debug, Clone)]
pub enum SyncStatus {
    UpToDate,
    Synced,
    Skipped(String),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct SyncOutcome {
    pub path: PathBuf,
    pub status: SyncStatus,
}

#[derive(Debug, Clone)]
pub struct SyncOptions {
    /// Parallel jobs (rayon threads).
    pub jobs: usize,
    /// Replace existing checkouts whose state differs from the lockfile.
    pub force: bool,
    /// Show the indicatif progress UI (auto-disabled when not a terminal).
    pub progress: bool,
}

/// Sync every locked repository into `dir`, in parallel.
///
/// Repositories are synced shallowest-first: a parent is always
/// materialized before the nested repositories inside it, so a child can
/// never observe (or create) a half-materialized parent. Within one depth
/// everything runs concurrently.
pub fn sync_workspace(
    dir: &Path,
    cache: &Cache,
    repos: &[LockedRepo],
    opts: &SyncOptions,
) -> Vec<SyncOutcome> {
    let progress = Arc::new(SyncProgress::new(repos.len() as u64, opts.progress));
    let key_locks: Mutex<HashMap<String, Arc<Mutex<()>>>> = Mutex::new(HashMap::new());

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(opts.jobs.max(1))
        .build()
        .expect("failed to build sync thread pool");

    // Group by path depth; BTreeMap iterates shallowest first.
    let mut by_depth: std::collections::BTreeMap<usize, Vec<&LockedRepo>> =
        std::collections::BTreeMap::new();
    for repo in repos {
        by_depth
            .entry(repo.path.components().count())
            .or_default()
            .push(repo);
    }

    let mut outcomes = Vec::with_capacity(repos.len());
    for group in by_depth.into_values() {
        let mut group_outcomes: Vec<SyncOutcome> = pool.install(|| {
            group
                .par_iter()
                .map(|locked| {
                    let outcome = sync_one(dir, cache, repos, locked, opts, &progress, &key_locks);
                    progress.inc();
                    outcome
                })
                .collect()
        });
        outcomes.append(&mut group_outcomes);
    }
    progress.finish();
    outcomes
}

fn sync_one(
    dir: &Path,
    cache: &Cache,
    repos: &[LockedRepo],
    locked: &LockedRepo,
    opts: &SyncOptions,
    progress: &SyncProgress,
    key_locks: &Mutex<HashMap<String, Arc<Mutex<()>>>>,
) -> SyncOutcome {
    let label = locked.path.display().to_string();
    let bar = progress.repo_spinner(&label);
    let status = match sync_one_inner(dir, cache, repos, locked, opts, key_locks, &bar) {
        Ok(s) => s,
        Err(e) => SyncStatus::Failed(e.to_string()),
    };
    let done_msg = match &status {
        SyncStatus::UpToDate => format!("✓ {label} (up-to-date)"),
        SyncStatus::Synced => format!("✓ {label}"),
        SyncStatus::Skipped(m) => format!("○ {label}: {m}"),
        SyncStatus::Failed(e) => format!("✗ {label}: {e}"),
    };
    bar.finish_with_message(done_msg);
    SyncOutcome {
        path: locked.path.clone(),
        status,
    }
}

fn sync_one_inner(
    dir: &Path,
    cache: &Cache,
    repos: &[LockedRepo],
    locked: &LockedRepo,
    opts: &SyncOptions,
    key_locks: &Mutex<HashMap<String, Arc<Mutex<()>>>>,
    bar: &indicatif::ProgressBar,
) -> Result<SyncStatus> {
    let spec = spec_from_locked(locked);
    let provider = provider_for(spec.kind)?;

    // Serialize the cache fill per URL so parallel workers don't run
    // `git fetch` into the same bare mirror concurrently.
    let key = cache.key(&spec.normalized_url());
    let key_lock = {
        let mut map = key_locks.lock().unwrap();
        map.entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    let _guard = key_lock.lock().unwrap();

    bar.set_message(format!("{label} fetching", label = locked.path.display()));
    let entry = provider.ensure_cached(cache, &spec)?;
    // The lockfile pins exact versions, so resolution is a pure cache-local
    // verification; for archives it also re-checks the content hash.
    let resolved = provider.resolve(&entry, &spec.version)?;
    if let Resolved::Archive { sha256, .. } = &resolved {
        let expected = locked.pin.strip_prefix("sha256:").unwrap_or("");
        if sha256 != expected {
            return Err(IrError::ChecksumMismatch {
                path: cache.archive_path(&cache.key(&spec.normalized_url()), "bin"),
                expected: expected.to_string(),
                actual: sha256.clone(),
            });
        }
    }

    let dest = dir.join(&locked.path);
    let state = provider.status(&dest)?;
    if state.present && pin_matches(&resolved, state.current.as_deref()) {
        // Changes strictly under a nested managed repository are ir's own
        // doing (it materialized them there); only other local changes
        // count as dirty for the up-to-date decision.
        let nested: Vec<PathBuf> = repos
            .iter()
            .map(|r| &r.path)
            .filter(|p| *p != &locked.path && p.starts_with(&locked.path))
            .filter_map(|p| p.strip_prefix(&locked.path).ok().map(PathBuf::from))
            .collect();
        let dirty = match provider.changed_paths(&dest)? {
            Some(paths) => paths
                .iter()
                .any(|p| !nested.iter().any(|n| p == n || p.starts_with(n))),
            None => state.dirty,
        };
        if !dirty {
            return Ok(SyncStatus::UpToDate);
        }
    }
    if state.present && !opts.force {
        return Ok(SyncStatus::Skipped(
            "exists with unexpected state; re-run with --force to replace".into(),
        ));
    }
    if state.present {
        std::fs::remove_dir_all(&dest)?;
    }
    bar.set_message(format!(
        "{label} materializing",
        label = locked.path.display()
    ));
    provider.materialize(&entry, &resolved, &spec, &dest)?;
    Ok(SyncStatus::Synced)
}

/// Rebuild a [`RepoSpec`] from a lockfile entry: the pin becomes the exact
/// requested version.
fn spec_from_locked(locked: &LockedRepo) -> RepoSpec {
    let version = match locked.kind {
        SourceKind::Git | SourceKind::Hg | SourceKind::Svn | SourceKind::Bzr => {
            VersionSpec::Revision(locked.pin.clone())
        }
        SourceKind::Tar | SourceKind::Zip => match &locked.subdir {
            Some(s) => VersionSpec::Subdir(s.to_string_lossy().into_owned()),
            None => VersionSpec::Default,
        },
    };
    RepoSpec {
        path: locked.path.clone(),
        kind: locked.kind,
        url: locked.url.clone(),
        version,
        subpaths: vec![],
    }
}

/// Does the on-disk state match the resolved pin?
fn pin_matches(resolved: &Resolved, current: Option<&str>) -> bool {
    match resolved {
        Resolved::Revision(r) => current == Some(r.as_str()),
        Resolved::Archive { sha256, .. } => current == Some(sha256.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::{to_locked, write_lock, LOCKFILE_NAME};
    use crate::manifest::{validate_relative_path, Manifest, ManifestEntry};
    use crate::resolve::{ResolvedRepo, Resolver};

    fn git_repo(dir: &Path, files: &[(&str, &str)]) -> PathBuf {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        let git = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .unwrap();
            assert!(st.success());
        };
        git(&["init", "-qb", "main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        for (name, content) in files {
            let p = dir.join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
        }
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        dir.to_path_buf()
    }

    fn repo_spec(path: &str, url: &str) -> RepoSpec {
        RepoSpec {
            path: validate_relative_path(path).unwrap(),
            kind: SourceKind::Git,
            url: url.to_string(),
            version: VersionSpec::Default,
            subpaths: vec![],
        }
    }

    /// Resolve a manifest and write ir.lock into the workspace dir.
    fn resolve_to_lock(ws: &Path, specs: Vec<RepoSpec>, cache: &Cache) -> Vec<LockedRepo> {
        let manifest = Manifest {
            source: ws.join(".repos"),
            entries: specs.into_iter().map(ManifestEntry::Repo).collect(),
        };
        let resolved: Vec<ResolvedRepo> = Resolver::new(cache).resolve(&manifest).unwrap();
        write_lock(&ws.join(LOCKFILE_NAME), &resolved).unwrap();
        resolved.iter().map(to_locked).collect()
    }

    fn opts(force: bool) -> SyncOptions {
        SyncOptions {
            jobs: 4,
            force,
            progress: false,
        }
    }

    #[test]
    fn sync_materializes_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::with_root(tmp.path().join("cache"));
        let r1 = git_repo(&tmp.path().join("r1"), &[("a.txt", "hello")]);
        let r2 = git_repo(&tmp.path().join("r2"), &[("b.txt", "world")]);
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let locked = resolve_to_lock(
            &ws,
            vec![
                repo_spec("one", r1.to_str().unwrap()),
                repo_spec("two", r2.to_str().unwrap()),
            ],
            &cache,
        );

        let outcomes = sync_workspace(&ws, &cache, &locked, &opts(false));
        assert!(outcomes
            .iter()
            .all(|o| matches!(o.status, SyncStatus::Synced)));
        assert_eq!(
            std::fs::read_to_string(ws.join("one/a.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(ws.join("two/b.txt")).unwrap(),
            "world"
        );

        // Second run: everything is up-to-date.
        let outcomes = sync_workspace(&ws, &cache, &locked, &opts(false));
        assert!(outcomes
            .iter()
            .all(|o| matches!(o.status, SyncStatus::UpToDate)));
    }

    #[test]
    fn sync_force_replaces_dirty_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::with_root(tmp.path().join("cache"));
        let r1 = git_repo(&tmp.path().join("r1"), &[("a.txt", "hello")]);
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let locked = resolve_to_lock(&ws, vec![repo_spec("one", r1.to_str().unwrap())], &cache);
        let outcomes = sync_workspace(&ws, &cache, &locked, &opts(false));
        assert!(matches!(outcomes[0].status, SyncStatus::Synced));

        // Dirty the checkout: untracked file -> status reports dirty.
        std::fs::write(ws.join("one/local.txt"), "mine").unwrap();

        let outcomes = sync_workspace(&ws, &cache, &locked, &opts(false));
        assert!(
            matches!(outcomes[0].status, SyncStatus::Skipped(_)),
            "got {:?}",
            outcomes[0].status
        );

        let outcomes = sync_workspace(&ws, &cache, &locked, &opts(true));
        assert!(matches!(outcomes[0].status, SyncStatus::Synced));
        assert!(!ws.join("one/local.txt").exists());
        assert_eq!(
            std::fs::read_to_string(ws.join("one/a.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn sync_nested_repo_does_not_dirty_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::with_root(tmp.path().join("cache"));
        let r1 = git_repo(&tmp.path().join("r1"), &[("a.txt", "hello")]);
        let r2 = git_repo(&tmp.path().join("r2"), &[("b.txt", "world")]);
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let locked = resolve_to_lock(
            &ws,
            vec![
                repo_spec("parent", r1.to_str().unwrap()),
                repo_spec("parent/child", r2.to_str().unwrap()),
            ],
            &cache,
        );

        // First sync materializes parent before child (depth order).
        let outcomes = sync_workspace(&ws, &cache, &locked, &opts(false));
        assert!(outcomes
            .iter()
            .all(|o| matches!(o.status, SyncStatus::Synced)));
        assert_eq!(
            std::fs::read_to_string(ws.join("parent/child/b.txt")).unwrap(),
            "world"
        );

        // Second sync: the nested checkout inside the parent must not
        // mark the parent dirty.
        let outcomes = sync_workspace(&ws, &cache, &locked, &opts(false));
        assert!(
            outcomes
                .iter()
                .all(|o| matches!(o.status, SyncStatus::UpToDate)),
            "got {:?}",
            outcomes
                .iter()
                .map(|o| (&o.path, &o.status))
                .collect::<Vec<_>>()
        );

        // But a genuine local change in the parent still skips it.
        std::fs::write(ws.join("parent/local.txt"), "mine").unwrap();
        let outcomes = sync_workspace(&ws, &cache, &locked, &opts(false));
        let parent = outcomes
            .iter()
            .find(|o| o.path.as_os_str() == "parent")
            .unwrap();
        assert!(
            matches!(parent.status, SyncStatus::Skipped(_)),
            "got {:?}",
            parent.status
        );
        let child = outcomes
            .iter()
            .find(|o| o.path.as_os_str() == "parent/child")
            .unwrap();
        assert!(
            matches!(child.status, SyncStatus::UpToDate),
            "got {:?}",
            child.status
        );
    }

    #[test]
    fn sync_bad_pin_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::with_root(tmp.path().join("cache"));
        let r1 = git_repo(&tmp.path().join("r1"), &[("a.txt", "hello")]);
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let mut locked = resolve_to_lock(&ws, vec![repo_spec("one", r1.to_str().unwrap())], &cache);
        locked[0].pin = "0000000000000000000000000000000000000000".into();

        let outcomes = sync_workspace(&ws, &cache, &locked, &opts(false));
        assert!(
            matches!(outcomes[0].status, SyncStatus::Failed(_)),
            "got {:?}",
            outcomes[0].status
        );
        assert!(!ws.join("one").exists());
    }

    #[test]
    fn sync_parallel_archives() {
        use crate::manifest::VersionSpec as VS;
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::with_root(tmp.path().join("cache"));

        // Build two small tar.gz archives, each with a nested .repos.
        let mk = |name: &str, inner_file: &str| {
            let ar_path = tmp.path().join(format!("{name}.tar.gz"));
            let file = std::fs::File::create(&ar_path).unwrap();
            let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut b = tar::Builder::new(enc);
            let name_in_archive = format!("{name}/{inner_file}");
            let mut h = tar::Header::new_gnu();
            h.set_size(4);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, &name_in_archive, b"data".as_slice())
                .unwrap();
            b.into_inner().unwrap().finish().unwrap();
            ar_path
        };
        let a1 = mk("aaa", "x.txt");
        let a2 = mk("bbb", "y.txt");

        let spec = |path: &str, url: &Path, subdir: &str| RepoSpec {
            path: validate_relative_path(path).unwrap(),
            kind: SourceKind::Tar,
            url: url.to_str().unwrap().to_string(),
            version: VS::Subdir(subdir.to_string()),
            subpaths: vec![],
        };
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let locked = resolve_to_lock(
            &ws,
            vec![spec("arc1", &a1, "aaa"), spec("arc2", &a2, "bbb")],
            &cache,
        );
        let outcomes = sync_workspace(&ws, &cache, &locked, &opts(false));
        assert!(outcomes
            .iter()
            .all(|o| matches!(o.status, SyncStatus::Synced)));
        assert!(ws.join("arc1/aaa/x.txt").is_file());
        assert!(ws.join("arc2/bbb/y.txt").is_file());
        // markers recorded the content hashes
        let outcomes = sync_workspace(&ws, &cache, &locked, &opts(false));
        assert!(outcomes
            .iter()
            .all(|o| matches!(o.status, SyncStatus::UpToDate)));
    }
}
