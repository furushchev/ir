//! `ir.lock`: TOML lockfile with exact pins for every resolved repository.
//!
//! Written by `ir resolve`, consumed by `ir sync` (Phase 3). Each entry pins
//! a workspace path to one exact revision: a VCS commit hash, or
//! `sha256:<hex>` of the archive content.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::error::{IrError, Result};
use crate::manifest::SourceKind;
use crate::provider::Resolved;
use crate::resolve::ResolvedRepo;

/// Name of the lockfile inside the workspace root.
pub const LOCKFILE_NAME: &str = "ir.lock";

const LOCK_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedRepo {
    /// Workspace-relative destination.
    pub path: PathBuf,
    pub kind: SourceKind,
    pub url: String,
    /// Version string as written in the manifest (informational).
    pub requested: String,
    /// Exact pin: VCS revision, or `sha256:<hex>` for archives.
    pub pin: String,
    /// In-archive subdirectory (archives only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LockfileDoc {
    version: u32,
    #[serde(default)]
    repo: Vec<LockedRepo>,
}

pub fn to_locked(r: &ResolvedRepo) -> LockedRepo {
    let (pin, subdir) = match &r.resolved {
        Resolved::Revision(rev) => (rev.clone(), None),
        Resolved::Archive { sha256, subdir } => (format!("sha256:{sha256}"), subdir.clone()),
    };
    LockedRepo {
        path: r.spec.path.clone(),
        kind: r.spec.kind,
        url: r.spec.url.clone(),
        requested: r.spec.version.to_string(),
        pin,
        subdir,
    }
}

pub fn write_lock(path: &Path, repos: &[ResolvedRepo]) -> Result<()> {
    let doc = LockfileDoc {
        version: LOCK_VERSION,
        repo: repos.iter().map(to_locked).collect(),
    };
    let text = toml::to_string_pretty(&doc)?;
    std::fs::write(path, text)?;
    Ok(())
}

pub fn read_lock(path: &Path) -> Result<Vec<LockedRepo>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| IrError::manifest_parse(path, format!("cannot read lockfile: {e}")))?;
    let doc: LockfileDoc =
        toml::from_str(&text).map_err(|e| IrError::manifest_parse(path, e.to_string()))?;
    if doc.version != LOCK_VERSION {
        return Err(IrError::Unsupported(format!(
            "unsupported {LOCKFILE_NAME} version {}",
            doc.version
        )));
    }
    Ok(doc.repo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{validate_relative_path, RepoSpec, VersionSpec};

    fn sample() -> Vec<ResolvedRepo> {
        vec![
            ResolvedRepo {
                spec: RepoSpec {
                    path: validate_relative_path("vendor/foo").unwrap(),
                    kind: SourceKind::Git,
                    url: "https://example.com/foo.git".into(),
                    version: VersionSpec::Ref("main".into()),
                    subpaths: vec![],
                },
                resolved: Resolved::Revision("6e049c56bc815c086d1d0cb44bc19436399f5e88".into()),
                depth: 0,
            },
            ResolvedRepo {
                spec: RepoSpec {
                    path: validate_relative_path("vendor/bar").unwrap(),
                    kind: SourceKind::Tar,
                    url: "https://example.com/bar.tar.gz".into(),
                    version: VersionSpec::Subdir("bar-1.0".into()),
                    subpaths: vec![],
                },
                resolved: Resolved::Archive {
                    sha256: "9f8e7d6c5b4a3948271605f4e3d2c1b0a".into(),
                    subdir: Some(PathBuf::from("bar-1.0")),
                },
                depth: 1,
            },
        ]
    }

    #[test]
    fn lockfile_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(LOCKFILE_NAME);
        let repos = sample();
        write_lock(&path, &repos).unwrap();
        let back = read_lock(&path).unwrap();
        assert_eq!(back, repos.iter().map(to_locked).collect::<Vec<_>>());
        // spot-check the TOML shape
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("version = 1"));
        assert!(text.contains("[[repo]]"));
        assert!(text.contains("kind = \"git\""));
        assert!(text.contains("sha256:9f8e7d6c5b4a3948271605f4e3d2c1b0a"));
    }

    #[test]
    fn lockfile_version_mismatch_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(LOCKFILE_NAME);
        std::fs::write(&path, "version = 99\n").unwrap();
        let err = read_lock(&path).unwrap_err();
        assert!(matches!(err, IrError::Unsupported(_)));
    }
}
