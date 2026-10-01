//! ir-core: workspace package manager core library.
//!
//! Manifest parsing (.repos / .rosinstall) and normalization into
//! [`RepoSpec`], the [`Provider`] trait over VCS tools and archives,
//! the recursive [`Resolver`], and the `ir.lock` lockfile.

pub mod cache;
pub mod error;
pub mod lock;
pub mod manifest;
pub mod process;
pub mod progress;
pub mod provider;
pub mod providers;
pub mod resolve;
pub mod sync;

pub use cache::{Cache, CacheEntry};
pub use error::{IrError, Result};
pub use lock::{read_lock, write_lock, LockedRepo, LOCKFILE_NAME};
pub use manifest::{
    discover_manifest, load_manifest, normalize_url, parse_repos, parse_rosinstall,
    validate_relative_path, Manifest, ManifestEntry, RepoSpec, SourceKind, VersionSpec,
};
pub use provider::{provider_for, Provider, Resolved, WorktreeState};
pub use resolve::{ResolvedRepo, Resolver};
pub use sync::{sync_workspace, SyncOptions, SyncOutcome, SyncStatus};
