//! Phase 5 command implementations (binary-private).

use anyhow::{bail, Context, Result};
use ir_core::{
    discover_manifest, inspect, load_manifest, normalize_url, read_lock, to_locked,
    validate_relative_path, write_lock, Cache, Inspection, LockedRepo, Manifest, ManifestEntry,
    RepoSpec, RepoState, ResolvedRepo, Resolver, SourceKind, SyncOptions, SyncOutcome, SyncStatus,
    LOCKFILE_NAME,
};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Output format for machine-readable commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    #[default]
    Text,
    Json,
}

pub fn read_locked(dir: &Path) -> Result<Vec<LockedRepo>> {
    let lock_path = dir.join(LOCKFILE_NAME);
    if !lock_path.is_file() {
        bail!("no {} found; run `ir resolve` first", lock_path.display());
    }
    read_lock(&lock_path).map_err(anyhow::Error::from)
}

fn print_json<T: Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn short_pin(pin: &str) -> &str {
    let p = pin.strip_prefix("sha256:").unwrap_or(pin);
    &p[..p.len().min(12)]
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct StatusReport<'a> {
    repos: &'a [Inspection],
    summary: StatusSummary,
}

#[derive(Serialize, Default)]
struct StatusSummary {
    up_to_date: usize,
    modified: usize,
    outdated: usize,
    missing: usize,
}

pub fn cmd_status(dir: &Path, format: Format) -> Result<()> {
    let locked = read_locked(dir)?;
    let report = inspect(dir, &locked).map_err(anyhow::Error::from)?;
    if format == Format::Json {
        let mut summary = StatusSummary::default();
        for r in &report {
            match r.state {
                RepoState::UpToDate => summary.up_to_date += 1,
                RepoState::Modified => summary.modified += 1,
                RepoState::Outdated => summary.outdated += 1,
                RepoState::Missing => summary.missing += 1,
            }
        }
        return print_json(&StatusReport {
            repos: &report,
            summary,
        });
    }
    let (mut ok, mut modified, mut outdated, mut missing) = (0, 0, 0, 0);
    for r in &report {
        match r.state {
            RepoState::UpToDate => {
                ok += 1;
                println!("✓ {}", r.path.display());
            }
            RepoState::Modified => {
                modified += 1;
                println!("● {} (modified)", r.path.display());
            }
            RepoState::Outdated => {
                outdated += 1;
                let cur = r.current.as_deref().map(short_pin).unwrap_or("?");
                println!(
                    "○ {} (outdated: {} -> {})",
                    r.path.display(),
                    cur,
                    short_pin(&r.pin)
                );
            }
            RepoState::Missing => {
                missing += 1;
                println!("✗ {} (missing)", r.path.display());
            }
        }
    }
    println!("up-to-date {ok}, modified {modified}, outdated {outdated}, missing {missing}");
    Ok(())
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

/// vcstool-compatible `.repos` document built from the lockfile.
pub fn export_repos(locked: &[LockedRepo], exact: bool) -> Result<String> {
    use serde_yaml::{Mapping, Value};
    let mut repos = Mapping::new();
    for r in locked {
        let mut entry = Mapping::new();
        entry.insert(Value::from("type"), Value::from(r.kind.to_string()));
        entry.insert(Value::from("url"), Value::from(r.url.clone()));
        let version: Option<String> = if exact {
            match r.kind {
                SourceKind::Git | SourceKind::Hg | SourceKind::Svn | SourceKind::Bzr => {
                    Some(r.pin.clone())
                }
                SourceKind::Tar | SourceKind::Zip => {
                    r.subdir.as_ref().map(|s| s.to_string_lossy().into_owned())
                }
            }
        } else {
            match r.kind {
                SourceKind::Git | SourceKind::Hg | SourceKind::Svn | SourceKind::Bzr => {
                    (r.requested != "(default)").then(|| r.requested.clone())
                }
                SourceKind::Tar | SourceKind::Zip => {
                    r.subdir.as_ref().map(|s| s.to_string_lossy().into_owned())
                }
            }
        };
        if let Some(v) = version {
            entry.insert(Value::from("version"), Value::from(v));
        }
        repos.insert(
            Value::from(r.path.to_string_lossy().into_owned()),
            Value::from(entry),
        );
    }
    let mut doc = Mapping::new();
    doc.insert(Value::from("repositories"), Value::from(repos));
    Ok(serde_yaml::to_string(&Value::from(doc))?)
}

pub fn cmd_export(dir: &Path, output: Option<&Path>, exact: bool) -> Result<()> {
    let locked = read_locked(dir)?;
    let text = export_repos(&locked, exact)?;
    match output {
        Some(p) => {
            std::fs::write(p, &text).with_context(|| format!("cannot write {}", p.display()))?;
            println!(
                "# exported {} repositories -> {}",
                locked.len(),
                p.display()
            );
        }
        None => print!("{text}"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// resolve / update
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ResolveReport<'a> {
    repos: Vec<ResolveEntry<'a>>,
    lockfile: PathBuf,
}

#[derive(Serialize)]
struct ResolveEntry<'a> {
    path: &'a Path,
    kind: String,
    url: &'a str,
    requested: String,
    pin: String,
    depth: usize,
}

fn resolve_report<'a>(repos: &'a [ResolvedRepo], lockfile: PathBuf) -> ResolveReport<'a> {
    ResolveReport {
        repos: repos
            .iter()
            .map(|r| ResolveEntry {
                path: &r.spec.path,
                kind: r.spec.kind.to_string(),
                url: &r.spec.url,
                requested: r.spec.version.to_string(),
                pin: r.pin(),
                depth: r.depth,
            })
            .collect(),
        lockfile,
    }
}

/// Re-resolve the whole workspace, rewrite the lock, and report pin changes.
pub fn cmd_update(dir: &Path, cache: &Cache, format: Format) -> Result<()> {
    let manifest = load_cli_manifest(dir, None)?;
    let old: Vec<LockedRepo> = read_locked(dir).unwrap_or_default();
    let repos = Resolver::new(cache)
        .resolve(&manifest)
        .map_err(anyhow::Error::from)?;
    let new: Vec<LockedRepo> = repos.iter().map(to_locked).collect();
    let diffs = diff_locks(&old, &new);
    let lock_path = dir.join(LOCKFILE_NAME);
    write_lock(&lock_path, &repos).map_err(anyhow::Error::from)?;
    report_update(&diffs, &lock_path, format, repos.len())
}

/// Re-resolve the subtree rooted at `path` (a manifest-declared repo or one
/// of its nested children, which updates the declaring ancestor's subtree).
pub fn cmd_update_one(dir: &Path, cache: &Cache, path: &str, format: Format) -> Result<()> {
    let manifest = load_cli_manifest(dir, None)?;
    let target = validate_relative_path(path).map_err(anyhow::Error::from)?;
    // Find the manifest-declared ancestor (the path itself or the closest
    // ancestor present in the manifest).
    let mut ancestor: Option<&RepoSpec> = None;
    let mut probe: Option<&Path> = Some(target.as_path());
    while let Some(p) = probe {
        if let Some(spec) = manifest_spec(&manifest, p) {
            ancestor = Some(spec);
            break;
        }
        probe = p.parent().filter(|pp| !pp.as_os_str().is_empty());
    }
    let spec = ancestor
        .ok_or_else(|| anyhow::anyhow!("{path} is not declared in the manifest"))?
        .clone();
    let root = spec.path.clone();

    let sub_manifest = Manifest {
        source: manifest.source.clone(),
        entries: vec![ManifestEntry::Repo(spec)],
    };
    let repos = Resolver::new(cache)
        .resolve(&sub_manifest)
        .map_err(anyhow::Error::from)?;
    // Paths are already workspace-relative (rebased onto `root` by the resolver).
    let subtree: Vec<LockedRepo> = repos.iter().map(to_locked).collect();

    let old = read_locked(dir)?;
    let mut merged: Vec<LockedRepo> = Vec::with_capacity(old.len());
    let mut spliced = false;
    for e in &old {
        let in_subtree = e.path == root || e.path.starts_with(&root);
        if in_subtree {
            if !spliced {
                merged.extend(subtree.iter().cloned());
                spliced = true;
            }
        } else {
            merged.push(e.clone());
        }
    }
    if !spliced {
        // Target had no locked entries yet (e.g. newly added to the manifest).
        merged.extend(subtree.iter().cloned());
    }
    let diffs: Vec<PinDiff> = {
        let old_map: std::collections::HashMap<&Path, &str> = old
            .iter()
            .map(|e| (e.path.as_path(), e.pin.as_str()))
            .collect();
        subtree
            .iter()
            .map(|e| PinDiff {
                path: e.path.clone(),
                old: old_map.get(e.path.as_path()).map(|s| s.to_string()),
                new: e.pin.clone(),
            })
            .filter(|d| d.old.as_deref() != Some(d.new.as_str()))
            .collect()
    };
    let lock_path = dir.join(LOCKFILE_NAME);
    // write_lock takes ResolvedRepo; rebuild via a tiny shim is overkill, so
    // serialize the merged LockedRepo list directly.
    write_merged_lock(&lock_path, &merged)?;
    report_update(&diffs, &lock_path, format, merged.len())
}

#[derive(Debug, Clone, Serialize)]
struct PinDiff {
    path: PathBuf,
    old: Option<String>,
    new: String,
}

fn diff_locks(old: &[LockedRepo], new: &[LockedRepo]) -> Vec<PinDiff> {
    let old_map: std::collections::HashMap<&Path, &str> = old
        .iter()
        .map(|e| (e.path.as_path(), e.pin.as_str()))
        .collect();
    let new_map: std::collections::HashMap<&Path, &str> = new
        .iter()
        .map(|e| (e.path.as_path(), e.pin.as_str()))
        .collect();
    let mut diffs: Vec<PinDiff> = new
        .iter()
        .filter(|e| old_map.get(e.path.as_path()) != Some(&e.pin.as_str()))
        .map(|e| PinDiff {
            path: e.path.clone(),
            old: old_map.get(e.path.as_path()).map(|s| s.to_string()),
            new: e.pin.clone(),
        })
        .collect();
    for e in old {
        if !new_map.contains_key(e.path.as_path()) {
            diffs.push(PinDiff {
                path: e.path.clone(),
                old: Some(e.pin.clone()),
                new: String::new(),
            });
        }
    }
    diffs.sort_by(|a, b| a.path.cmp(&b.path));
    diffs
}

#[derive(Serialize)]
struct UpdateReport<'a> {
    changes: &'a [PinDiff],
    lockfile: &'a Path,
    repos: usize,
}

fn report_update(diffs: &[PinDiff], lock_path: &Path, format: Format, total: usize) -> Result<()> {
    if format == Format::Json {
        return print_json(&UpdateReport {
            changes: diffs,
            lockfile: lock_path,
            repos: total,
        });
    }
    if diffs.is_empty() {
        println!("# already up-to-date; {} repositories", total);
    } else {
        for d in diffs {
            match (&d.old, d.new.is_empty()) {
                (Some(old), false) => println!(
                    "~ {}: {} -> {}",
                    d.path.display(),
                    short_pin(old),
                    short_pin(&d.new)
                ),
                (None, false) => println!("+ {} @ {}", d.path.display(), short_pin(&d.new)),
                (Some(old), true) => {
                    println!("- {} (was {})", d.path.display(), short_pin(old))
                }
                (None, true) => {}
            }
        }
        println!("# {} changed -> {}", diffs.len(), lock_path.display());
    }
    Ok(())
}

/// Serialize a merged lockfile (used by `update <path>` splicing).
fn write_merged_lock(path: &Path, repos: &[LockedRepo]) -> Result<()> {
    #[derive(Serialize)]
    struct Doc<'a> {
        version: u32,
        repo: &'a [LockedRepo],
    }
    let text = toml::to_string_pretty(&Doc {
        version: 1,
        repo: repos,
    })?;
    std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

fn manifest_spec<'a>(manifest: &'a Manifest, path: &Path) -> Option<&'a RepoSpec> {
    manifest.entries.iter().find_map(|e| match e {
        ManifestEntry::Repo(s) if s.path == path => Some(s),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// add / init
// ---------------------------------------------------------------------------

fn infer_kind(url: &str) -> SourceKind {
    let u = url.to_ascii_lowercase();
    if u.ends_with(".zip") {
        SourceKind::Zip
    } else if u.ends_with(".tar.gz")
        || u.ends_with(".tgz")
        || u.ends_with(".tar.bz2")
        || u.ends_with(".tbz2")
        || u.ends_with(".tar.xz")
        || u.ends_with(".txz")
    {
        SourceKind::Tar
    } else {
        SourceKind::Git
    }
}

fn infer_path(url: &str) -> Result<PathBuf> {
    let trimmed = url.trim().trim_end_matches('/');
    let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    let mut name = last.to_string();
    for suffix in [
        ".git", ".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tar.xz", ".txz", ".zip",
    ] {
        if let Some(s) = name.strip_suffix(suffix) {
            name = s.to_string();
            break;
        }
    }
    if name.is_empty() {
        bail!("cannot infer a path from url {url}; pass --path");
    }
    validate_relative_path(&name).map_err(anyhow::Error::from)
}

pub fn cmd_add(
    manifest_path: &Path,
    url: &str,
    path: Option<&str>,
    kind: Option<SourceKind>,
    version: Option<&str>,
) -> Result<()> {
    let dest = match path {
        Some(p) => validate_relative_path(p).map_err(anyhow::Error::from)?,
        None => infer_path(url)?,
    };
    let kind = kind.unwrap_or_else(|| infer_kind(url));
    let text = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("cannot read {}", manifest_path.display()))?;
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&text)?;
    let dest_s = dest.to_string_lossy().into_owned();

    if doc.get("repositories").is_some() {
        // .repos format
        let repos = doc
            .get_mut("repositories")
            .expect("checked above")
            .as_mapping_mut()
            .context("`repositories` is not a mapping")?;
        let key = serde_yaml::Value::from(dest_s.clone());
        if repos.contains_key(&key) {
            bail!("{dest_s} is already declared in the manifest");
        }
        let mut entry = serde_yaml::Mapping::new();
        entry.insert("type".into(), kind.to_string().into());
        entry.insert("url".into(), url.into());
        if let Some(v) = version.map(str::trim).filter(|s| !s.is_empty()) {
            entry.insert("version".into(), v.into());
        }
        repos.insert(key, entry.into());
    } else if doc.is_sequence() {
        // .rosinstall format
        let seq = doc
            .as_sequence_mut()
            .context("manifest is not a sequence")?;
        for item in seq.iter() {
            if let Some(m) = item.as_mapping() {
                for (_k, v) in m.iter() {
                    if v.get("local-name").and_then(|n| n.as_str()) == Some(dest_s.as_str()) {
                        bail!("{dest_s} is already declared in the manifest");
                    }
                }
            }
        }
        let mut attrs = serde_yaml::Mapping::new();
        attrs.insert("local-name".into(), dest_s.as_str().into());
        attrs.insert("uri".into(), url.into());
        if let Some(v) = version.map(str::trim).filter(|s| !s.is_empty()) {
            attrs.insert("version".into(), v.into());
        }
        let mut item = serde_yaml::Mapping::new();
        item.insert(kind.to_string().into(), attrs.into());
        seq.push(item.into());
    } else {
        bail!(
            "unrecognized manifest format in {}",
            manifest_path.display()
        );
    }
    std::fs::write(manifest_path, serde_yaml::to_string(&doc)?)
        .with_context(|| format!("cannot write {}", manifest_path.display()))?;
    println!("# added {} [{}] {}", dest.display(), kind, url);
    Ok(())
}

pub fn cmd_init(dir: &Path) -> Result<()> {
    if discover_manifest(dir).is_ok() {
        bail!("a manifest already exists in {}", dir.display());
    }
    let path = dir.join(".repos");
    std::fs::write(&path, "repositories: {}\n")
        .with_context(|| format!("cannot write {}", path.display()))?;
    println!("# initialized {}", path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// prune
// ---------------------------------------------------------------------------

fn looks_like_repo(dir: &Path) -> bool {
    dir.join(".git").is_dir()
        || dir.join(".hg").is_dir()
        || dir.join(".svn").is_dir()
        || dir.join(".bzr").is_dir()
        || dir.join(".ir-archive").is_file()
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&p) else {
            continue;
        };
        for e in rd.flatten() {
            let ep = e.path();
            if let Ok(md) = std::fs::symlink_metadata(&ep) {
                if md.is_dir() && !md.file_type().is_symlink() {
                    stack.push(ep);
                } else {
                    total += md.len();
                }
            }
        }
    }
    total
}

pub fn cmd_prune(dir: &Path) -> Result<()> {
    let locked = read_locked(dir)?;
    let keep: HashSet<&Path> = locked.iter().map(|r| r.path.as_path()).collect();
    let mut removed: Vec<(PathBuf, u64)> = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(cur) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&cur) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(md) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if !md.is_dir() || md.file_type().is_symlink() {
                continue;
            }
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') {
                continue; // never touch dot-dirs (e.g. the workspace's own .git)
            }
            let rel = match p.strip_prefix(dir) {
                Ok(r) if !r.as_os_str().is_empty() => r.to_path_buf(),
                _ => continue,
            };
            if looks_like_repo(&p) && !keep.contains(rel.as_path()) {
                let bytes = dir_size(&p);
                std::fs::remove_dir_all(&p)
                    .with_context(|| format!("cannot remove {}", p.display()))?;
                removed.push((rel, bytes));
                // Don't descend into a removed tree.
            } else {
                stack.push(p);
            }
        }
    }
    removed.sort_by(|a, b| a.0.cmp(&b.0));
    let bytes: u64 = removed.iter().map(|(_, b)| b).sum();
    for (rel, _) in &removed {
        println!("- {}", rel.display());
    }
    println!(
        "# pruned {} repositories, freed {}",
        removed.len(),
        human_bytes(bytes)
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// cache
// ---------------------------------------------------------------------------

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

pub fn cmd_cache_clean(cache: &Cache) -> Result<()> {
    let bytes = dir_size(cache.root());
    std::fs::remove_dir_all(cache.root())
        .with_context(|| format!("cannot remove {}", cache.root().display()))?;
    println!("# cache cleaned, freed {}", human_bytes(bytes));
    Ok(())
}

pub fn cmd_cache_gc(cache: &Cache, dir: &Path) -> Result<()> {
    let locked = read_locked(dir)?;
    let mut keep_keys: HashSet<String> = HashSet::new();
    for r in &locked {
        keep_keys.insert(cache.key(&normalize_url(&r.url)));
    }
    let mut removed = 0usize;
    let mut freed = 0u64;
    let root = cache.root();
    let Ok(rd) = std::fs::read_dir(root) else {
        println!("# cache is empty");
        return Ok(());
    };
    for e in rd.flatten() {
        let p = e.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let keep_entry = |entry_name: &str| -> bool {
            match name {
                "git" | "hg" | "bzr" => keep_keys.contains(entry_name),
                "archives" => keep_keys
                    .iter()
                    .any(|k| entry_name.starts_with(&format!("{k}."))),
                _ => true, // don't delete what we don't understand
            }
        };
        if p.is_dir() {
            let Ok(sub) = std::fs::read_dir(&p) else {
                continue;
            };
            for se in sub.flatten() {
                let sp = se.path();
                let sname = sp.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if keep_entry(sname) {
                    continue;
                }
                freed += dir_size(&sp);
                let is_dir = sp.is_dir();
                let r = if is_dir {
                    std::fs::remove_dir_all(&sp)
                } else {
                    std::fs::remove_file(&sp)
                };
                if r.is_ok() {
                    removed += 1;
                }
            }
        }
    }
    println!(
        "# cache gc: removed {removed} entries, freed {}",
        human_bytes(freed)
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// resolve (moved here for JSON support)
// ---------------------------------------------------------------------------

pub fn cmd_resolve(
    dir: &Path,
    manifest_path: Option<&Path>,
    cache: &Cache,
    format: Format,
) -> Result<()> {
    let manifest = load_cli_manifest(dir, manifest_path)?;
    let repos = Resolver::new(cache)
        .resolve(&manifest)
        .map_err(anyhow::Error::from)?;
    let lock_path = dir.join(LOCKFILE_NAME);
    write_lock(&lock_path, &repos).map_err(anyhow::Error::from)?;
    if format == Format::Json {
        return print_json(&resolve_report(&repos, lock_path));
    }
    println!(
        "# resolved {} repositories -> {}",
        repos.len(),
        lock_path.display()
    );
    for r in &repos {
        let indent = "  ".repeat(r.depth);
        println!(
            "{indent}{} [{}] {} @ {} -> {}",
            r.spec.path.display(),
            r.spec.kind,
            r.spec.normalized_url(),
            r.spec.version,
            r.pin()
        );
    }
    Ok(())
}

/// Load the manifest honoring `--manifest`, else discover from `dir`.
pub fn load_cli_manifest(dir: &Path, manifest_path: Option<&Path>) -> Result<ir_core::Manifest> {
    match manifest_path {
        Some(p) => load_manifest(p).map_err(anyhow::Error::from),
        None => {
            let p = discover_manifest(dir).map_err(anyhow::Error::from)?;
            load_manifest(&p).map_err(anyhow::Error::from)
        }
    }
}

// ---------------------------------------------------------------------------
// sync (moved here for JSON support)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct SyncReport<'a> {
    outcomes: Vec<SyncEntry<'a>>,
    summary: SyncSummary,
}

#[derive(Serialize)]
struct SyncEntry<'a> {
    path: &'a Path,
    status: &'a str,
    detail: Option<&'a str>,
}

#[derive(Serialize, Default)]
struct SyncSummary {
    synced: usize,
    up_to_date: usize,
    skipped: usize,
    failed: usize,
}

pub fn cmd_sync(dir: &Path, cache: &Cache, jobs: usize, force: bool, format: Format) -> Result<()> {
    let locked = read_locked(dir)?;
    let progress = format == Format::Text && std::io::IsTerminal::is_terminal(&std::io::stderr());
    let opts = SyncOptions {
        jobs,
        force,
        progress,
    };
    let outcomes: Vec<SyncOutcome> = ir_core::sync_workspace(dir, cache, &locked, &opts);
    if format == Format::Json {
        let mut summary = SyncSummary::default();
        let entries: Vec<SyncEntry> = outcomes
            .iter()
            .map(|o| {
                let (status, detail) = match &o.status {
                    SyncStatus::UpToDate => {
                        summary.up_to_date += 1;
                        ("up_to_date", None)
                    }
                    SyncStatus::Synced => {
                        summary.synced += 1;
                        ("synced", None)
                    }
                    SyncStatus::Skipped(m) => {
                        summary.skipped += 1;
                        ("skipped", Some(m.as_str()))
                    }
                    SyncStatus::Failed(e) => {
                        summary.failed += 1;
                        ("failed", Some(e.as_str()))
                    }
                };
                SyncEntry {
                    path: &o.path,
                    status,
                    detail,
                }
            })
            .collect();
        print_json(&SyncReport {
            outcomes: entries,
            summary,
        })?;
    } else {
        let (mut synced, mut up_to_date, mut skipped, mut failed) = (0, 0, 0, 0);
        for o in &outcomes {
            match &o.status {
                SyncStatus::UpToDate => up_to_date += 1,
                SyncStatus::Synced => synced += 1,
                SyncStatus::Skipped(m) => {
                    skipped += 1;
                    println!("○ {}: {m}", o.path.display());
                }
                SyncStatus::Failed(e) => {
                    failed += 1;
                    println!("✗ {}: {e}", o.path.display());
                }
            }
        }
        println!("synced {synced}, up-to-date {up_to_date}, skipped {skipped}, failed {failed}");
    }
    let failed = outcomes
        .iter()
        .filter(|o| matches!(o.status, SyncStatus::Failed(_)))
        .count();
    if failed > 0 {
        bail!("{failed} repositories failed to sync");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locked_repo(path: &str, kind: SourceKind, pin: &str, requested: &str) -> LockedRepo {
        LockedRepo {
            path: PathBuf::from(path),
            kind,
            url: format!("https://example.com/{path}"),
            requested: requested.to_string(),
            pin: pin.to_string(),
            subdir: None,
        }
    }

    #[test]
    fn export_repos_basic() {
        let repos = vec![
            locked_repo("a", SourceKind::Git, "abc123", "main"),
            locked_repo("b", SourceKind::Hg, "def456", "(default)"),
        ];
        let yaml = export_repos(&repos, false).unwrap();
        assert!(yaml.contains("type: git"));
        assert!(yaml.contains("version: main"));
        assert!(yaml.contains("type: hg"));
        // default version is omitted
        let b_section = yaml.split("b:").nth(1).unwrap();
        assert!(!b_section
            .split('\n')
            .take(4)
            .any(|l| l.trim().starts_with("version:")));
    }

    #[test]
    fn export_repos_exact_uses_pins() {
        let repos = vec![locked_repo("a", SourceKind::Git, "abc123", "main")];
        let yaml = export_repos(&repos, true).unwrap();
        assert!(yaml.contains("version: abc123"));
    }

    #[test]
    fn diff_locks_detects_changes() {
        let old = vec![
            locked_repo("a", SourceKind::Git, "aaa", "main"),
            locked_repo("b", SourceKind::Git, "bbb", "main"),
            locked_repo("gone", SourceKind::Git, "ggg", "main"),
        ];
        let new = vec![
            locked_repo("a", SourceKind::Git, "aaa2", "main"),
            locked_repo("b", SourceKind::Git, "bbb", "main"),
            locked_repo("fresh", SourceKind::Git, "fff", "main"),
        ];
        let diffs = diff_locks(&old, &new);
        assert_eq!(diffs.len(), 3);
        let changed = diffs.iter().find(|d| d.path == Path::new("a")).unwrap();
        assert_eq!(changed.old.as_deref(), Some("aaa"));
        assert_eq!(changed.new, "aaa2");
        assert!(diffs
            .iter()
            .any(|d| d.path == Path::new("gone") && d.new.is_empty()));
        assert!(diffs
            .iter()
            .any(|d| d.path == Path::new("fresh") && d.old.is_none()));
    }

    #[test]
    fn infer_kind_and_path() {
        assert_eq!(infer_kind("https://x/y.git"), SourceKind::Git);
        assert_eq!(infer_kind("https://x/y.tar.gz"), SourceKind::Tar);
        assert_eq!(infer_kind("https://x/y.zip"), SourceKind::Zip);
        assert_eq!(
            infer_path("https://x/foo.git").unwrap(),
            PathBuf::from("foo")
        );
        assert_eq!(
            infer_path("https://x/releases/v1.tar.gz").unwrap(),
            PathBuf::from("v1")
        );
    }

    #[test]
    fn human_bytes_formats() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
    }
}
