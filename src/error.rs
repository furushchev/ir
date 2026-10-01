use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;

/// Errors produced by ir-core.
#[derive(Debug, Error)]
pub enum IrError {
    #[error("manifest parse error in {path}: {msg}")]
    ManifestParse { path: PathBuf, msg: String },

    #[error("invalid repository path '{path}': {reason}")]
    InvalidPath { path: String, reason: String },

    #[error("unknown source kind '{0}' (expected git, hg, svn, bzr, tar or zip)")]
    UnknownKind(String),

    #[error("no manifest found in {0} (looked for .repos and .rosinstall)")]
    ManifestNotFound(PathBuf),

    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error("failed to spawn '{cmd}': {source}")]
    SpawnFailed { cmd: String, source: std::io::Error },

    #[error("command failed ({status}): {cmd}\n{stderr}")]
    CommandFailed {
        cmd: String,
        status: String,
        stderr: String,
    },

    #[error("command timed out after {1:?}: {0}")]
    CommandTimeout(String, Duration),

    #[error("destination already exists: {0}")]
    DestExists(PathBuf),

    #[error("revision not found: {0}")]
    RevisionNotFound(String),

    #[error("cannot determine default branch of {0}")]
    NoDefaultBranch(String),

    #[error("archive error: {0}")]
    Archive(String),

    #[error("checksum mismatch for {path}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    #[error("http error: {0}")]
    Http(String),

    #[error("dependency cycle detected: {chain}")]
    Cycle { chain: String },

    #[error(
        "path conflict: '{path}' is declared with two different URLs ('{url_a}' vs '{url_b}')"
    )]
    PathConflict {
        path: PathBuf,
        url_a: String,
        url_b: String,
    },

    #[error(
        "version conflict: '{path}' is required at two different revisions ({rev_a} vs {rev_b})"
    )]
    VersionConflict {
        path: PathBuf,
        rev_a: String,
        rev_b: String,
    },

    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),

    #[error(transparent)]
    TomlSer(#[from] toml::ser::Error),

    #[error(transparent)]
    TomlDe(#[from] toml::de::Error),
}

pub type Result<T> = std::result::Result<T, IrError>;

impl IrError {
    pub fn manifest_parse(path: &Path, msg: impl Into<String>) -> Self {
        Self::ManifestParse {
            path: path.to_path_buf(),
            msg: msg.into(),
        }
    }
}
