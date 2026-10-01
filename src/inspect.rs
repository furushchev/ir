//! Workspace inspection: compare `ir.lock` pins against materialized worktrees.
//!
//! Used by `ir status` and by the sync engine's up-to-date decision. The
//! dirty check is nested-repo aware: changes strictly under a managed
//! nested repository don't mark the parent dirty.

use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::lock::LockedRepo;
use crate::manifest::SourceKind;
use crate::provider::{provider_for, Provider};

/// Sync state of one locked repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoState {
    /// Present, clean, and at the pinned revision.
    UpToDate,
    /// Present but has local modifications (beyond nested managed repos).
    Modified,
    /// Present and clean, but checked out at a different revision than pinned.
    Outdated,
    /// Not materialized in the workspace.
    Missing,
}

/// Inspection result for one locked repository.
#[derive(Debug, Clone, Serialize)]
pub struct Inspection {
    pub path: PathBuf,
    pub kind: SourceKind,
    pub state: RepoState,
    /// Currently checked-out revision, if the worktree reports one.
    pub current: Option<String>,
    /// Pinned revision from `ir.lock`.
    pub pin: String,
}

/// Does the on-disk revision match the lockfile pin?
pub fn pin_current_matches(kind: SourceKind, pin: &str, current: Option<&str>) -> bool {
    match kind {
        SourceKind::Git | SourceKind::Hg | SourceKind::Svn | SourceKind::Bzr => {
            current == Some(pin)
        }
        // Archive pins look like `sha256:<hex>`; worktrees report the raw hex.
        SourceKind::Tar | SourceKind::Zip => {
            current == Some(pin.strip_prefix("sha256:").unwrap_or(pin))
        }
    }
}

/// Nested-aware dirty check: changes strictly under a managed nested
/// repository path are ir's own doing and don't count. Falls back to the
/// provider's plain dirty flag when it can't enumerate changed paths.
pub fn effective_dirty(
    provider: &dyn Provider,
    dest: &Path,
    repos: &[LockedRepo],
    locked: &LockedRepo,
    fallback_dirty: bool,
) -> Result<bool> {
    let nested: Vec<PathBuf> = repos
        .iter()
        .map(|r| &r.path)
        .filter(|p| *p != &locked.path && p.starts_with(&locked.path))
        .filter_map(|p| p.strip_prefix(&locked.path).ok().map(PathBuf::from))
        .collect();
    match provider.changed_paths(dest)? {
        Some(paths) => Ok(paths
            .iter()
            .any(|p| !nested.iter().any(|n| p == n || p.starts_with(n)))),
        None => Ok(fallback_dirty),
    }
}

/// Inspect every locked repository. No network access: pins are compared
/// against the worktrees as they are.
pub fn inspect(dir: &Path, repos: &[LockedRepo]) -> Result<Vec<Inspection>> {
    let mut out = Vec::with_capacity(repos.len());
    for locked in repos {
        let provider = provider_for(locked.kind)?;
        let dest = dir.join(&locked.path);
        let state = provider.status(&dest)?;
        let repo_state = if !state.present {
            RepoState::Missing
        } else if effective_dirty(provider.as_ref(), &dest, repos, locked, state.dirty)? {
            RepoState::Modified
        } else if !pin_current_matches(locked.kind, &locked.pin, state.current.as_deref()) {
            RepoState::Outdated
        } else {
            RepoState::UpToDate
        };
        out.push(Inspection {
            path: locked.path.clone(),
            kind: locked.kind,
            state: repo_state,
            current: state.current,
            pin: locked.pin.clone(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_match_vcs_and_archive() {
        use SourceKind as K;
        assert!(pin_current_matches(K::Git, "abc123", Some("abc123")));
        assert!(!pin_current_matches(K::Git, "abc123", Some("def456")));
        assert!(!pin_current_matches(K::Git, "abc123", None));
        assert!(pin_current_matches(K::Svn, "42", Some("42")));
        assert!(pin_current_matches(
            K::Tar,
            "sha256:deadbeef",
            Some("deadbeef")
        ));
        assert!(!pin_current_matches(
            K::Zip,
            "sha256:deadbeef",
            Some("other")
        ));
    }
}
