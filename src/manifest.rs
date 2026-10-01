//! Manifest parsing and normalization.
//!
//! Two input formats are supported, both describing a set of repositories
//! to materialize into a workspace:
//!
//! * `.repos` (vcstool): a YAML mapping with a top-level `repositories` key,
//!   where each entry maps a workspace-relative path to
//!   `{type, url, version}`. The optional git-only `subpaths` key (sparse
//!   checkout) is preserved.
//! * `.rosinstall`: a YAML list whose elements each carry exactly one VCS
//!   key (`git`, `hg`, `svn`, `bzr`) with `{local-name, uri, version}`.
//!   As an extension we also accept `tar` and `zip` keys here, and as
//!   `type:` values in `.repos` files. `other` / `setup-file` entries are
//!   parsed and retained but carry no VCS action.
//!
//! Both formats are normalized into [`RepoSpec`]. Paths declared in a
//! manifest are interpreted relative to the directory containing that
//! manifest file (for a top-level manifest that is the workspace root).

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use crate::error::{Result, WspmError};

/// Where a repository's content comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    Git,
    Hg,
    Svn,
    Bzr,
    Tar,
    Zip,
}

impl SourceKind {
    /// True for archive kinds, which have no branch/tag/hash semantics.
    pub fn is_archive(self) -> bool {
        matches!(self, SourceKind::Tar | SourceKind::Zip)
    }
}

impl FromStr for SourceKind {
    type Err = WspmError;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "git" => Ok(SourceKind::Git),
            "hg" | "mercurial" => Ok(SourceKind::Hg),
            "svn" | "subversion" => Ok(SourceKind::Svn),
            "bzr" | "bazaar" => Ok(SourceKind::Bzr),
            "tar" => Ok(SourceKind::Tar),
            "zip" => Ok(SourceKind::Zip),
            other => Err(WspmError::UnknownKind(other.to_string())),
        }
    }
}

impl std::fmt::Display for SourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            SourceKind::Git => "git",
            SourceKind::Hg => "hg",
            SourceKind::Svn => "svn",
            SourceKind::Bzr => "bzr",
            SourceKind::Tar => "tar",
            SourceKind::Zip => "zip",
        };
        write!(f, "{s}")
    }
}

/// How the desired version of a repository is expressed in a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionSpec {
    /// No version given: use the VCS default branch (or archive root).
    Default,
    /// A branch or tag name; resolved to an exact revision later.
    Ref(String),
    /// An exact revision (commit hash etc.); needs no resolution.
    Revision(String),
    /// For archives: a folder inside the archive root (rosinstall `tar`
    /// semantics: `version` must refer to a folder inside the tar root).
    Subdir(String),
}

impl VersionSpec {
    fn classify(raw: Option<&str>, is_archive: bool) -> Self {
        match raw.map(str::trim).filter(|s| !s.is_empty()) {
            None => VersionSpec::Default,
            Some(s) if is_archive => VersionSpec::Subdir(s.to_string()),
            Some(s) if is_hex_hash(s) => VersionSpec::Revision(s.to_string()),
            Some(s) => VersionSpec::Ref(s.to_string()),
        }
    }
}

impl std::fmt::Display for VersionSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VersionSpec::Default => write!(f, "(default)"),
            VersionSpec::Ref(r) => write!(f, "{r}"),
            VersionSpec::Revision(r) => write!(f, "{r}"),
            VersionSpec::Subdir(d) => write!(f, "{d}/"),
        }
    }
}

/// Looks like an abbreviated or full hex object id (git/hg style).
fn is_hex_hash(s: &str) -> bool {
    (4..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A single repository dependency, normalized from either manifest format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSpec {
    /// Workspace-relative destination path (validated, no `..`, no absolutes).
    pub path: PathBuf,
    pub kind: SourceKind,
    /// The URL/URI as written in the manifest.
    pub url: String,
    pub version: VersionSpec,
    /// `.repos` git-only sparse-checkout directories.
    pub subpaths: Vec<String>,
}

impl RepoSpec {
    /// URL normalized for cache keys and equality: trims whitespace and
    /// trailing slashes, strips a `.git` suffix, lowercases the host of
    /// parseable URLs. The original URL is still used for actual fetching.
    pub fn normalized_url(&self) -> String {
        normalize_url(&self.url)
    }
}

/// Normalize a repository URL for cache keys and comparison.
pub fn normalize_url(raw: &str) -> String {
    let mut u = raw.trim().to_string();
    while u.ends_with('/') && u.len() > 1 {
        u.pop();
    }
    if let Some(stripped) = u.strip_suffix(".git") {
        // Don't strip ".git" from something like "https://host/.git".
        if !stripped.ends_with('/') && !stripped.is_empty() {
            u = stripped.to_string();
        }
    }
    if let Ok(mut parsed) = url::Url::parse(&u) {
        if let Some(host) = parsed.host_str() {
            let lower = host.to_lowercase();
            let _ = parsed.set_host(Some(&lower));
        }
        let s = parsed.to_string();
        return s.trim_end_matches('/').to_string();
    }
    // Non-URL forms (scp-like `git@host:org/repo`, plain paths): keep as-is.
    u
}

/// Validate that a declared path is a safe workspace-relative path.
pub fn validate_relative_path(raw: &str) -> Result<PathBuf> {
    let p = Path::new(raw);
    if p.is_absolute() {
        return Err(WspmError::InvalidPath {
            path: raw.to_string(),
            reason: "absolute paths are not allowed; use a workspace-relative path".into(),
        });
    }
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => {
                return Err(WspmError::InvalidPath {
                    path: raw.to_string(),
                    reason: "absolute paths are not allowed".into(),
                });
            }
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(WspmError::InvalidPath {
                    path: raw.to_string(),
                    reason: "`..` would escape the workspace".into(),
                });
            }
            Component::Normal(c) => out.push(c),
        }
    }
    if out.as_os_str().is_empty() {
        return Err(WspmError::InvalidPath {
            path: raw.to_string(),
            reason: "empty path".into(),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// .repos format
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ReposFile {
    #[serde(default)]
    repositories: HashMap<String, ReposEntry>,
}

#[derive(Debug, Deserialize)]
struct ReposEntry {
    #[serde(rename = "type", default = "default_kind")]
    kind: String,
    url: String,
    #[serde(default)]
    version: Option<serde_yaml::Value>,
    #[serde(default)]
    subpaths: Vec<String>,
}

fn default_kind() -> String {
    "git".to_string()
}

/// Parse a `.repos` (vcstool) document into normalized specs.
pub fn parse_repos(content: &str, source: &Path) -> Result<Vec<RepoSpec>> {
    let file: ReposFile = serde_yaml::from_str(content)
        .map_err(|e| WspmError::manifest_parse(&source.to_path_buf(), e.to_string()))?;
    let mut specs = Vec::with_capacity(file.repositories.len());
    for (path, entry) in file.repositories {
        let kind = SourceKind::from_str(&entry.kind)?;
        let version = VersionSpec::classify(
            entry.version.as_ref().and_then(yaml_to_string).as_deref(),
            kind.is_archive(),
        );
        specs.push(RepoSpec {
            path: validate_relative_path(&path)?,
            kind,
            url: entry.url,
            version,
            subpaths: entry.subpaths,
        });
    }
    // Deterministic order regardless of YAML mapping order.
    specs.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(specs)
}

// ---------------------------------------------------------------------------
// .rosinstall format
// ---------------------------------------------------------------------------

/// A parsed manifest: the file it came from plus all its entries.
#[derive(Debug)]
pub struct Manifest {
    pub source: PathBuf,
    pub entries: Vec<ManifestEntry>,
}

/// One element of a manifest file.
#[derive(Debug, Clone)]
pub enum ManifestEntry {
    Repo(RepoSpec),
    /// `other:` element: a workspace path with no VCS attached.
    Other { local_name: String },
    /// `setup-file:` element: a shell snippet path, no VCS action.
    SetupFile { local_name: String },
}

impl Manifest {
    pub fn repos(&self) -> impl Iterator<Item = &RepoSpec> {
        self.entries.iter().filter_map(|e| match e {
            ManifestEntry::Repo(spec) => Some(spec),
            _ => None,
        })
    }
}

fn yaml_to_string(v: &serde_yaml::Value) -> Option<String> {
    match v {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn mapping_get_string(
    map: &serde_yaml::Mapping,
    key: &str,
    source: &Path,
) -> Result<String> {
    map.get(&serde_yaml::Value::String(key.to_string()))
        .and_then(yaml_to_string)
        .ok_or_else(|| {
            WspmError::manifest_parse(
                &source.to_path_buf(),
                format!("missing required key '{key}'"),
            )
        })
}

/// Parse a `.rosinstall` document.
pub fn parse_rosinstall(content: &str, source: &Path) -> Result<Manifest> {
    let docs: Vec<HashMap<String, serde_yaml::Value>> = serde_yaml::from_str(content)
        .map_err(|e| WspmError::manifest_parse(&source.to_path_buf(), e.to_string()))?;
    let mut entries = Vec::with_capacity(docs.len());
    for (idx, mut doc) in docs.into_iter().enumerate() {
        if doc.len() != 1 {
            return Err(WspmError::manifest_parse(
                &source.to_path_buf(),
                format!("element #{idx}: expected exactly one top-level key"),
            ));
        }
        let (kind_key, value) = doc.drain().next().expect("len == 1");
        let mapping = value.as_mapping().ok_or_else(|| {
            WspmError::manifest_parse(
                &source.to_path_buf(),
                format!("element #{idx} ('{kind_key}'): expected a mapping"),
            )
        })?;
        let local_name = mapping_get_string(mapping, "local-name", source)?;
        match kind_key.as_str() {
            "git" | "hg" | "svn" | "bzr" | "tar" | "zip" => {
                let kind = SourceKind::from_str(&kind_key)?;
                let uri = mapping_get_string(mapping, "uri", source).or_else(|_| {
                    // Accept `url` as an alias for `uri`.
                    mapping_get_string(mapping, "url", source)
                })?;
                let version = mapping
                    .get(&serde_yaml::Value::String("version".to_string()))
                    .and_then(yaml_to_string);
                entries.push(ManifestEntry::Repo(RepoSpec {
                    path: validate_relative_path(&local_name)?,
                    kind,
                    url: uri,
                    version: VersionSpec::classify(version.as_deref(), kind.is_archive()),
                    subpaths: Vec::new(),
                }));
            }
            "other" => entries.push(ManifestEntry::Other { local_name }),
            "setup-file" => entries.push(ManifestEntry::SetupFile { local_name }),
            other => {
                return Err(WspmError::manifest_parse(
                    &source.to_path_buf(),
                    format!("element #{idx}: unknown key '{other}'"),
                ));
            }
        }
    }
    Ok(Manifest {
        source: source.to_path_buf(),
        entries,
    })
}

// ---------------------------------------------------------------------------
// Loading & discovery
// ---------------------------------------------------------------------------

/// Load a manifest file, choosing the parser by file name.
/// Unknown extensions fall back to trying `.repos` first, then `.rosinstall`.
pub fn load_manifest(path: &Path) -> Result<Manifest> {
    let content =
        std::fs::read_to_string(path).map_err(|e| WspmError::manifest_parse(
            &path.to_path_buf(),
            format!("cannot read file: {e}"),
        ))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if name == ".repos" || name.ends_with(".repos") {
        let specs = parse_repos(&content, path)?;
        return Ok(Manifest {
            source: path.to_path_buf(),
            entries: specs.into_iter().map(ManifestEntry::Repo).collect(),
        });
    }
    if name.ends_with(".rosinstall") {
        return parse_rosinstall(&content, path);
    }
    // Unknown name: try .repos, fall back to .rosinstall.
    match parse_repos(&content, path) {
        Ok(specs) => Ok(Manifest {
            source: path.to_path_buf(),
            entries: specs.into_iter().map(ManifestEntry::Repo).collect(),
        }),
        Err(_) => parse_rosinstall(&content, path),
    }
}

/// Find the workspace manifest: `.repos` first, then `.rosinstall`.
pub fn discover_manifest(dir: &Path) -> Result<PathBuf> {
    for name in [".repos", ".rosinstall"] {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(WspmError::ManifestNotFound(dir.to_path_buf()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_REPOS: &str = r#"
repositories:
  navigation:
    type: git
    url: https://github.com/ros-navigation/navigation2.git
    version: main
  old_tools/rosinstall:
    type: svn
    url: https://github.com/vcstools/rosinstall/trunk
    version: 748
  vendor/sparse:
    type: git
    url: git@github.com:example/sparse.git
    subpaths:
      - sub/a
      - sub/b
  pinned:
    type: git
    url: https://github.com/example/pinned
    version: a3f9c1d2e4b5a3f9c1d2e4b5a3f9c1d2e4b5a3f9
  vendored:
    type: tar
    url: https://example.com/foo-1.2.0.tar.gz
    version: foo-1.2.0
"#;

    #[test]
    fn parse_repos_basic() {
        let specs = parse_repos(SAMPLE_REPOS, Path::new(".repos")).unwrap();
        assert_eq!(specs.len(), 5);
        // sorted by path
        assert_eq!(specs[0].path, Path::new("navigation"));
        assert_eq!(specs[0].kind, SourceKind::Git);
        assert_eq!(specs[0].version, VersionSpec::Ref("main".into()));

        let svn = &specs[1];
        assert_eq!(svn.path, Path::new("old_tools/rosinstall"));
        assert_eq!(svn.kind, SourceKind::Svn);
        // numeric YAML version becomes a string ref
        assert_eq!(svn.version, VersionSpec::Ref("748".into()));

        let sparse = specs.iter().find(|s| s.path == Path::new("vendor/sparse")).unwrap();
        assert_eq!(sparse.subpaths, vec!["sub/a".to_string(), "sub/b".to_string()]);
        assert_eq!(sparse.version, VersionSpec::Default);

        let pinned = specs.iter().find(|s| s.path == Path::new("pinned")).unwrap();
        assert!(matches!(pinned.version, VersionSpec::Revision(_)));

        let tar = specs.iter().find(|s| s.path == Path::new("vendored")).unwrap();
        assert_eq!(tar.kind, SourceKind::Tar);
        assert_eq!(tar.version, VersionSpec::Subdir("foo-1.2.0".into()));
    }

    const SAMPLE_ROSINSTALL: &str = r#"
- git: {local-name: src/nav, uri: 'https://github.com/ros-navigation/navigation2.git', version: main}
- hg:
    local-name: src/legacy
    uri: https://example.com/legacy
    version: 123
- tar:
    local-name: vendor/foo.tar.gz
    uri: https://example.com/foo-1.2.0.tar.gz
    version: foo-1.2.0
- other: {local-name: /opt/ros/humble}
- setup-file: {local-name: /opt/ros/humble/setup.sh}
"#;

    #[test]
    fn parse_rosinstall_basic() {
        let m = parse_rosinstall(SAMPLE_ROSINSTALL, Path::new(".rosinstall")).unwrap();
        assert_eq!(m.entries.len(), 5);
        let repos: Vec<_> = m.repos().collect();
        assert_eq!(repos.len(), 3);
        assert_eq!(repos[0].path, Path::new("src/nav"));
        assert_eq!(repos[0].version, VersionSpec::Ref("main".into()));
        // numeric version stays a ref (branch/tag semantics for hg)
        assert_eq!(repos[1].version, VersionSpec::Ref("123".into()));
        assert_eq!(repos[2].kind, SourceKind::Tar);
        assert!(matches!(
            m.entries[3],
            ManifestEntry::Other { .. }
        ));
        assert!(matches!(
            m.entries[4],
            ManifestEntry::SetupFile { .. }
        ));
    }

    #[test]
    fn reject_absolute_and_parent_paths() {
        assert!(validate_relative_path("/abs/path").is_err());
        assert!(validate_relative_path("../escape").is_err());
        assert!(validate_relative_path("a/../../b").is_err());
        assert!(validate_relative_path("").is_err());
        assert_eq!(
            validate_relative_path("a/./b").unwrap(),
            Path::new("a/b")
        );
    }

    #[test]
    fn normalize_url_cases() {
        assert_eq!(
            normalize_url("https://github.com/org/repo.git"),
            "https://github.com/org/repo"
        );
        assert_eq!(
            normalize_url("https://GitHub.com/org/repo/"),
            "https://github.com/org/repo"
        );
        // scp-like syntax is preserved (minus .git suffix)
        assert_eq!(
            normalize_url("git@github.com:org/repo.git"),
            "git@github.com:org/repo"
        );
    }

    #[test]
    fn version_classification() {
        assert_eq!(VersionSpec::classify(None, false), VersionSpec::Default);
        assert_eq!(VersionSpec::classify(Some("  "), false), VersionSpec::Default);
        assert_eq!(
            VersionSpec::classify(Some("main"), false),
            VersionSpec::Ref("main".into())
        );
        assert!(matches!(
            VersionSpec::classify(Some("a3f9c1d2"), false),
            VersionSpec::Revision(_)
        ));
        assert_eq!(
            VersionSpec::classify(Some("foo-1.2.0"), true),
            VersionSpec::Subdir("foo-1.2.0".into())
        );
    }

    #[test]
    fn unknown_kind_errors() {
        let bad = "repositories:\n  x:\n    type: cvs\n    url: https://example.com\n";
        assert!(parse_repos(bad, Path::new(".repos")).is_err());
    }
}
