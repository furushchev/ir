//! wspm-core: workspace package manager core library.
//!
//! Phase 0 covers manifest parsing (.repos / .rosinstall) and normalization
//! into [`RepoSpec`]. Later phases add the resolver, lockfile, cache,
//! VCS providers and the sync UI.

pub mod error;
pub mod manifest;

pub use error::{Result, WspmError};
pub use manifest::{
    Manifest, ManifestEntry, RepoSpec, SourceKind, VersionSpec, discover_manifest, load_manifest,
    normalize_url, parse_repos, parse_rosinstall, validate_relative_path,
};
