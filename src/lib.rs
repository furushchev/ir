//! wspm-core: workspace package manager core library.
//!
//! Phase 0 covers manifest parsing (.repos / .rosinstall) and normalization
//! into [`RepoSpec`]. Later phases add the resolver, lockfile, cache,
//! VCS providers and the sync UI.

pub mod cache;
pub mod error;
pub mod manifest;
pub mod process;
pub mod provider;
pub mod providers;

pub use cache::{Cache, CacheEntry};
pub use error::{Result, WspmError};
pub use manifest::{
    discover_manifest, load_manifest, normalize_url, parse_repos, parse_rosinstall,
    validate_relative_path, Manifest, ManifestEntry, RepoSpec, SourceKind, VersionSpec,
};
pub use provider::{provider_for, Provider, Resolved, WorktreeState};
