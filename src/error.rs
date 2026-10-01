use std::path::PathBuf;
use thiserror::Error;

/// Errors produced by wspm-core.
#[derive(Debug, Error)]
pub enum WspmError {
    #[error("manifest parse error in {path}: {msg}")]
    ManifestParse { path: PathBuf, msg: String },

    #[error("invalid repository path '{path}': {reason}")]
    InvalidPath { path: String, reason: String },

    #[error("unknown source kind '{0}' (expected git, hg, svn, bzr, tar or zip)")]
    UnknownKind(String),

    #[error("no manifest found in {0} (looked for .repos and .rosinstall)")]
    ManifestNotFound(PathBuf),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
}

pub type Result<T> = std::result::Result<T, WspmError>;

impl WspmError {
    pub fn manifest_parse(path: &PathBuf, msg: impl Into<String>) -> Self {
        Self::ManifestParse {
            path: path.clone(),
            msg: msg.into(),
        }
    }
}
