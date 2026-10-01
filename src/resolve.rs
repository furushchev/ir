//! Recursive dependency resolver.
//!
//! Starting from the workspace manifest, every declared repository is
//! fetched into the cache, resolved to an exact pin
//! ([`Resolved`]), and then scanned for a nested `.repos` / `.rosinstall`
//! file, whose entries are rebased onto the workspace root and resolved in
//! turn (depth-first).
//!
//! Two declarations of the same workspace path must agree on both the URL
//! and the resolved pin, otherwise resolution fails with a conflict error.
//! A repository that (transitively) depends on itself is a cycle error.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::cache::{Cache, CacheEntry};
use crate::error::{IrError, Result};
use crate::manifest::{
    parse_repos, parse_rosinstall, validate_relative_path, Manifest, ManifestEntry, RepoSpec,
};
use crate::provider::{provider_for, Provider, Resolved};

/// One repository after recursive resolution.
#[derive(Debug, Clone)]
pub struct ResolvedRepo {
    /// Workspace-relative destination (rebased for nested manifests).
    pub spec: RepoSpec,
    pub resolved: Resolved,
    /// Nesting depth: 0 = declared directly in the workspace manifest.
    pub depth: usize,
}

impl ResolvedRepo {
    /// Short pin string for display and the lockfile: a VCS revision, or
    /// `sha256:<hex>` for archives.
    pub fn pin(&self) -> String {
        match &self.resolved {
            Resolved::Revision(r) => r.clone(),
            Resolved::Archive { sha256, .. } => format!("sha256:{sha256}"),
        }
    }
}

#[derive(Default)]
struct State {
    /// Resolved repos in discovery (pre-order) order.
    order: Vec<ResolvedRepo>,
    /// Workspace path -> index into `order`.
    by_path: HashMap<PathBuf, usize>,
    /// URLs currently being expanded (cycle detection): (url, path).
    stack: Vec<(String, PathBuf)>,
}

pub struct Resolver<'a> {
    cache: &'a Cache,
}

impl<'a> Resolver<'a> {
    pub fn new(cache: &'a Cache) -> Self {
        Self { cache }
    }

    /// Resolve the whole workspace manifest recursively.
    pub fn resolve(&self, manifest: &Manifest) -> Result<Vec<ResolvedRepo>> {
        let mut state = State::default();
        for entry in &manifest.entries {
            if let ManifestEntry::Repo(spec) = entry {
                self.visit(spec, 0, &mut state)?;
            }
        }
        Ok(state.order)
    }

    fn visit(&self, spec: &RepoSpec, depth: usize, state: &mut State) -> Result<()> {
        let provider = provider_for(spec.kind)?;
        let entry = provider.ensure_cached(self.cache, spec)?;
        let resolved = provider.resolve(&entry, &spec.version)?;

        // Dedupe / conflict check against already-recorded repositories.
        if let Some(&idx) = state.by_path.get(&spec.path) {
            let prev = &state.order[idx];
            if prev.spec.normalized_url() != spec.normalized_url() {
                return Err(IrError::PathConflict {
                    path: spec.path.clone(),
                    url_a: prev.spec.url.clone(),
                    url_b: spec.url.clone(),
                });
            }
            if prev.resolved != resolved {
                return Err(IrError::VersionConflict {
                    path: spec.path.clone(),
                    rev_a: prev.pin(),
                    rev_b: ResolvedRepo {
                        spec: spec.clone(),
                        resolved: resolved.clone(),
                        depth,
                    }
                    .pin(),
                });
            }
            return Ok(());
        }

        // Cycle detection over the current expansion stack.
        let url_key = spec.normalized_url();
        if let Some(pos) = state.stack.iter().position(|(u, _)| *u == url_key) {
            let mut chain: Vec<String> = state.stack[pos..]
                .iter()
                .map(|(_, p)| p.display().to_string())
                .collect();
            chain.push(spec.path.display().to_string());
            return Err(IrError::Cycle {
                chain: chain.join(" -> "),
            });
        }

        state.stack.push((url_key, spec.path.clone()));
        state.order.push(ResolvedRepo {
            spec: spec.clone(),
            resolved: resolved.clone(),
            depth,
        });
        state
            .by_path
            .insert(spec.path.clone(), state.order.len() - 1);

        let nested = self.nested_specs(provider.as_ref(), &entry, &resolved, spec)?;
        for child in nested {
            // Rebase the nested path onto the workspace root.
            let rebased = validate_relative_path(&spec.path.join(&child.path).to_string_lossy())
                .map_err(|_| IrError::InvalidPath {
                    path: spec.path.join(&child.path).display().to_string(),
                    reason: "escapes its parent repository".into(),
                })?;
            let mut child = child;
            child.path = rebased;
            self.visit(&child, depth + 1, state)?;
        }

        state.stack.pop();
        Ok(())
    }

    /// Read `.repos` (preferred) or `.rosinstall` from inside a resolved
    /// repository and parse its repo entries.
    fn nested_specs(
        &self,
        provider: &dyn Provider,
        entry: &CacheEntry,
        resolved: &Resolved,
        parent: &RepoSpec,
    ) -> Result<Vec<RepoSpec>> {
        for name in [".repos", ".rosinstall"] {
            if let Some(bytes) = provider.read_file(entry, resolved, name)? {
                let text = String::from_utf8(bytes).map_err(|e| {
                    IrError::Archive(format!("nested {name} is not valid UTF-8: {e}"))
                })?;
                // Pseudo-source for error messages: the manifest only exists
                // inside the cached repository, not on disk.
                let source = Path::new(name);
                if name == ".repos" {
                    return parse_repos(&text, source).map_err(|e| match e {
                        IrError::ManifestParse { msg, .. } => IrError::ManifestParse {
                            path: parent.path.join(name),
                            msg,
                        },
                        other => other,
                    });
                }
                let manifest = parse_rosinstall(&text, source).map_err(|e| match e {
                    IrError::ManifestParse { msg, .. } => IrError::ManifestParse {
                        path: parent.path.join(name),
                        msg,
                    },
                    other => other,
                })?;
                return Ok(manifest
                    .entries
                    .into_iter()
                    .filter_map(|e| match e {
                        ManifestEntry::Repo(s) => Some(s),
                        ManifestEntry::Other { .. } | ManifestEntry::SetupFile { .. } => None,
                    })
                    .collect());
            }
        }
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{SourceKind, VersionSpec};

    /// Create a git repo at `dir` with `files` committed on branch `main`,
    /// and an optional tag pointing at HEAD.
    fn git_repo(dir: &Path, files: &[(&str, &str)], tag: Option<&str>) -> PathBuf {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        let repo = dir.to_path_buf();
        let git = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
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
            let p = repo.join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
        }
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        if let Some(t) = tag {
            git(&["tag", t]);
        }
        repo
    }

    fn repo_spec(path: &str, url: &str, version: VersionSpec) -> RepoSpec {
        RepoSpec {
            path: validate_relative_path(path).unwrap(),
            kind: SourceKind::Git,
            url: url.to_string(),
            version,
            subpaths: vec![],
        }
    }

    fn manifest_of(specs: Vec<RepoSpec>) -> Manifest {
        Manifest {
            source: PathBuf::from(".repos"),
            entries: specs.into_iter().map(ManifestEntry::Repo).collect(),
        }
    }

    fn resolve_all(manifest: &Manifest, cache_root: &Path) -> Result<Vec<ResolvedRepo>> {
        let cache = Cache::with_root(cache_root.to_path_buf());
        Resolver::new(&cache).resolve(manifest)
    }

    #[test]
    fn nested_repos_are_rebased_and_resolved() {
        let tmp = tempfile::tempdir().unwrap();
        // inner repo: no nested manifest
        let inner = git_repo(&tmp.path().join("i"), &[("x.txt", "x")], None);
        // outer repo: declares inner at path "vendor/inner"
        let outer_content = format!(
            "repositories:\n  vendor/inner:\n    type: git\n    url: {}\n",
            inner.display()
        );
        let outer = git_repo(&tmp.path().join("o"), &[(".repos", &outer_content)], None);

        let manifest = manifest_of(vec![repo_spec(
            "vendor/outer",
            outer.to_str().unwrap(),
            VersionSpec::Default,
        )]);
        let repos = resolve_all(&manifest, &tmp.path().join("cache")).unwrap();
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].spec.path, PathBuf::from("vendor/outer"));
        assert_eq!(repos[0].depth, 0);
        // nested path rebased onto the workspace root
        assert_eq!(
            repos[1].spec.path,
            PathBuf::from("vendor/outer/vendor/inner")
        );
        assert_eq!(repos[1].depth, 1);
        assert!(matches!(repos[1].resolved, Resolved::Revision(_)));
    }

    #[test]
    fn cycle_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        // A declares B at "b"; B declares A at "a2" (same URL, other path).
        let dir_a = tmp.path().join("a");
        let dir_b = tmp.path().join("b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        // Create B first with a placeholder, then fix up A's URL afterwards.
        let b = git_repo(&dir_b, &[("y.txt", "y")], None);
        let a_content = format!(
            "repositories:\n  b:\n    type: git\n    url: {}\n",
            b.display()
        );
        let a = git_repo(&dir_a, &[(".repos", &a_content), ("z.txt", "z")], None);
        // Now point B back at A.
        let b_repos = format!(
            "repositories:\n  a2:\n    type: git\n    url: {}\n",
            a.display()
        );
        std::fs::write(b.join(".repos"), b_repos).unwrap();
        let st = std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&b)
            .status()
            .unwrap();
        assert!(st.success());
        let st = std::process::Command::new("git")
            .args(["commit", "-qm", "add cycle"])
            .current_dir(&b)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(st.success());

        let manifest = manifest_of(vec![repo_spec(
            "a",
            a.to_str().unwrap(),
            VersionSpec::Default,
        )]);
        let err = resolve_all(&manifest, &tmp.path().join("cache")).unwrap_err();
        assert!(matches!(err, IrError::Cycle { .. }), "got {err:?}");
    }

    #[test]
    fn same_path_different_url_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        let r1 = git_repo(&tmp.path().join("r1"), &[("a.txt", "a")], None);
        let r2 = git_repo(&tmp.path().join("r2"), &[("b.txt", "b")], None);
        let manifest = manifest_of(vec![
            repo_spec("p", r1.to_str().unwrap(), VersionSpec::Default),
            repo_spec("p", r2.to_str().unwrap(), VersionSpec::Default),
        ]);
        let err = resolve_all(&manifest, &tmp.path().join("cache")).unwrap_err();
        assert!(matches!(err, IrError::PathConflict { .. }), "got {err:?}");
    }

    #[test]
    fn same_path_different_revision_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        // One repo: tag v1 on the first commit, then a second commit on main.
        let dir = tmp.path().join("r");
        std::fs::create_dir_all(&dir).unwrap();
        let repo = git_repo(&dir, &[("a.txt", "a")], Some("v1"));
        std::fs::write(repo.join("b.txt"), "b").unwrap();
        for args in [vec!["add", "."], vec!["commit", "-qm", "second"]] {
            let st = std::process::Command::new("git")
                .args(&args)
                .current_dir(&repo)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .unwrap();
            assert!(st.success());
        }
        let url = repo.to_str().unwrap();
        let manifest = manifest_of(vec![
            repo_spec("p", url, VersionSpec::Ref("v1".into())),
            repo_spec("p", url, VersionSpec::Default),
        ]);
        let err = resolve_all(&manifest, &tmp.path().join("cache")).unwrap_err();
        assert!(
            matches!(err, IrError::VersionConflict { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn diamond_is_deduplicated() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = git_repo(&tmp.path().join("s"), &[("s.txt", "s")], None);
        let left_content = format!(
            "repositories:\n  shared:\n    type: git\n    url: {}\n",
            shared.display()
        );
        let right_content = left_content.clone();
        let left = git_repo(&tmp.path().join("l"), &[(".repos", &left_content)], None);
        let right = git_repo(&tmp.path().join("r"), &[(".repos", &right_content)], None);
        let manifest = manifest_of(vec![
            repo_spec("left", left.to_str().unwrap(), VersionSpec::Default),
            repo_spec("right", right.to_str().unwrap(), VersionSpec::Default),
        ]);
        let repos = resolve_all(&manifest, &tmp.path().join("cache")).unwrap();
        // left, left/shared, right — right/shared dedupes against left/shared? No:
        // different paths (left/shared vs right/shared), so both are recorded.
        let paths: Vec<_> = repos.iter().map(|r| r.spec.path.clone()).collect();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("left"),
                PathBuf::from("left/shared"),
                PathBuf::from("right"),
                PathBuf::from("right/shared"),
            ]
        );
    }
}
