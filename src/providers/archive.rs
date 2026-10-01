//! Archive providers: download + extract for `tar` and `zip` sources.
//!
//! * Cache: the downloaded file, keyed by sha256(normalized URL).
//! * Version resolution: there is no branch/tag concept, so [`Resolved`] is
//!   the sha256 of the archive content (the pin written to the lockfile).
//!   A manifest `version` on an archive names a subdirectory inside the
//!   archive root (rosinstall `tar` semantics).
//! * Materialization: extract into `dest` plus a `.wspm-archive` marker file
//!   recording the content hash, used by [`Provider::status`].
//!
//! Zip Slip protection: archive entries with absolute paths or `..`
//! components are rejected instead of extracted.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::cache::{Cache, CacheEntry};
use crate::error::{Result, WspmError};
use crate::manifest::{validate_relative_path, RepoSpec, SourceKind, VersionSpec};
use crate::provider::{Provider, Resolved, WorktreeState};

/// Marker file written at the root of an extracted archive.
const MARKER: &str = ".wspm-archive";

#[derive(Debug, Serialize, Deserialize)]
struct ArchiveMarker {
    sha256: String,
    url: String,
    subdir: Option<String>,
}

pub struct TarProvider;
pub struct ZipProvider;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Detect the archive extension from a URL (`tar.gz`, `tar.bz2`, `tar.xz`,
/// `tar` or `zip`). Query strings / fragments are ignored.
fn archive_ext(url: &str) -> Result<&'static str> {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    for (suffix, ext) in [
        (".tar.gz", "tar.gz"),
        (".tgz", "tar.gz"),
        (".tar.bz2", "tar.bz2"),
        (".tbz2", "tar.bz2"),
        (".tar.xz", "tar.xz"),
        (".txz", "tar.xz"),
        (".tar", "tar"),
        (".zip", "zip"),
    ] {
        if path.ends_with(suffix) {
            return Ok(ext);
        }
    }
    Err(WspmError::Archive(format!(
        "cannot determine archive type of '{url}'"
    )))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    io::copy(&mut f, &mut h)?;
    Ok(hex::encode(h.finalize()))
}

/// Download `url` to `dest`. Supports `https://`, `file://` and plain local
/// paths (rosinstall allows local URIs).
fn download(url: &str, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(local) = url.strip_prefix("file://") {
        fs::copy(local, dest)?;
        return Ok(());
    }
    if Path::new(url).exists() {
        fs::copy(url, dest)?;
        return Ok(());
    }
    let resp = reqwest::blocking::get(url)?;
    let resp = resp
        .error_for_status()
        .map_err(|e| WspmError::Http(e.to_string()))?;
    let mut file = fs::File::create(dest)?;
    let mut resp = resp;
    io::copy(&mut resp, &mut file)?;
    Ok(())
}

fn ensure_downloaded(cache: &Cache, spec: &RepoSpec) -> Result<PathBuf> {
    let key = cache.key(&spec.normalized_url());
    let ext = archive_ext(&spec.url)?;
    let dest = cache.archive_path(&key, ext);
    if !dest.exists() {
        download(&spec.url, &dest)?;
    }
    Ok(dest)
}

fn subdir_of(version: &VersionSpec) -> Result<Option<PathBuf>> {
    match version {
        VersionSpec::Subdir(d) => Ok(Some(validate_relative_path(d)?)),
        VersionSpec::Default => Ok(None),
        VersionSpec::Ref(r) | VersionSpec::Revision(r) => Err(WspmError::Unsupported(format!(
            "archive sources take a subdirectory as version, got '{r}'"
        ))),
    }
}

fn open_tar_reader(path: &Path, ext: &str) -> Result<Box<dyn io::Read>> {
    let file = fs::File::open(path)?;
    Ok(match ext {
        "tar.gz" => Box::new(flate2::read::GzDecoder::new(file)),
        "tar.bz2" => Box::new(bzip2::read::BzDecoder::new(file)),
        "tar.xz" => Box::new(xz2::read::XzDecoder::new(file)),
        "tar" => Box::new(file),
        _ => {
            return Err(WspmError::Archive(format!(
                "unsupported tar extension: {ext}"
            )));
        }
    })
}

/// Reject absolute paths and `..` components (Zip Slip).
fn sanitize_entry(raw: &Path) -> Result<PathBuf> {
    let s = raw
        .to_str()
        .ok_or_else(|| WspmError::Archive(format!("non-UTF8 entry path: {}", raw.display())))?;
    validate_relative_path(s)
        .map_err(|_| WspmError::Archive(format!("unsafe archive entry path: {}", raw.display())))
}

fn extract_tar(archive: &Path, ext: &str, dest: &Path) -> Result<()> {
    let reader = open_tar_reader(archive, ext)?;
    let mut ar = tar::Archive::new(reader);
    let entries = ar
        .entries()
        .map_err(|e| WspmError::Archive(e.to_string()))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| WspmError::Archive(e.to_string()))?;
        let rel = sanitize_entry(
            &entry
                .path()
                .map_err(|e| WspmError::Archive(e.to_string()))?,
        )?;
        let target = dest.join(&rel);
        let ftype = entry.header().entry_type();
        if ftype.is_dir() {
            fs::create_dir_all(&target)?;
        } else if ftype.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            entry
                .unpack(&target)
                .map_err(|e| WspmError::Archive(e.to_string()))?;
        }
        // Symlinks and other special entries are skipped in Phase 1.
    }
    Ok(())
}

fn extract_zip(archive: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(archive)?;
    let mut ar = zip::ZipArchive::new(file).map_err(|e| WspmError::Archive(e.to_string()))?;
    for i in 0..ar.len() {
        let mut entry = ar
            .by_index(i)
            .map_err(|e| WspmError::Archive(e.to_string()))?;
        let rel = entry
            .enclosed_name()
            .ok_or_else(|| WspmError::Archive(format!("unsafe zip entry at index {i}")))?;
        // enclosed_name already rejects absolute paths and `..`; re-validate
        // for a single code path.
        let rel = sanitize_entry(&rel)?;
        let target = dest.join(&rel);
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out = fs::File::create(&target)?;
            io::copy(&mut entry, &mut out)?;
        }
    }
    Ok(())
}

fn extract(kind: SourceKind, archive: &Path, dest: &Path) -> Result<()> {
    match kind {
        SourceKind::Tar => extract_tar(archive, archive_ext_from_path(archive)?, dest),
        SourceKind::Zip => extract_zip(archive, dest),
        _ => Err(WspmError::Unsupported(format!(
            "{kind} is not an archive kind"
        ))),
    }
}

/// Recover the extension from a cached archive file name (`<key>.<ext>`).
fn archive_ext_from_path(path: &Path) -> Result<&'static str> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| WspmError::Archive("cannot determine cached archive type".into()))?;
    // name is `<hexkey>.<ext>`; match the longest known suffix.
    for ext in ["tar.gz", "tar.bz2", "tar.xz", "tar", "zip"] {
        if name.ends_with(&format!(".{ext}")) {
            return Ok(ext);
        }
    }
    Err(WspmError::Archive(format!(
        "cannot determine cached archive type of '{name}'"
    )))
}

fn write_marker(dest: &Path, marker: &ArchiveMarker) -> Result<()> {
    let json =
        serde_json::to_string_pretty(marker).map_err(|e| WspmError::Archive(e.to_string()))?;
    fs::write(dest.join(MARKER), json)?;
    Ok(())
}

fn read_marker(dest: &Path) -> Result<Option<ArchiveMarker>> {
    let path = dest.join(MARKER);
    if !path.is_file() {
        return Ok(None);
    }
    let content = fs::read_to_string(&path)?;
    serde_json::from_str(&content)
        .map(Some)
        .map_err(|e| WspmError::Archive(format!("corrupt marker file: {e}")))
}

/// Extract to a temp dir and return the content of `rel`, looked up under
/// `subdir` (if any) first, then the archive root. The temp dir is removed
/// before returning, so the file must be read while it is alive.
fn read_from_archive(
    kind: SourceKind,
    archive: &Path,
    subdir: Option<&Path>,
    rel: &str,
) -> Result<Option<Vec<u8>>> {
    let rel = validate_relative_path(rel)
        .map_err(|_| WspmError::Archive(format!("unsafe relative path requested: {rel}")))?;
    let tmp = tempfile::tempdir()?;
    extract(kind, archive, tmp.path())?;
    let mut candidates = Vec::new();
    if let Some(sd) = subdir {
        candidates.push(tmp.path().join(sd).join(&rel));
    }
    candidates.push(tmp.path().join(&rel));
    for candidate in candidates {
        if candidate.is_file() {
            return Ok(Some(fs::read(candidate)?));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Provider impls (thin wrappers over the shared helpers)
// ---------------------------------------------------------------------------

macro_rules! impl_archive_provider {
    ($name:ident, $kind:expr) => {
        impl Provider for $name {
            fn kind(&self) -> SourceKind {
                $kind
            }

            fn ensure_cached(&self, cache: &Cache, spec: &RepoSpec) -> Result<CacheEntry> {
                Ok(CacheEntry::Archive(ensure_downloaded(cache, spec)?))
            }

            fn resolve(&self, entry: &CacheEntry, version: &VersionSpec) -> Result<Resolved> {
                let CacheEntry::Archive(path) = entry else {
                    return Err(WspmError::Unsupported(format!(
                        "{} got a non-archive cache entry",
                        stringify!($name)
                    )));
                };
                Ok(Resolved::Archive {
                    sha256: sha256_file(path)?,
                    subdir: subdir_of(version)?,
                })
            }

            fn read_file(
                &self,
                entry: &CacheEntry,
                resolved: &Resolved,
                rel: &str,
            ) -> Result<Option<Vec<u8>>> {
                let CacheEntry::Archive(path) = entry else {
                    return Err(WspmError::Unsupported(
                        "expected archive cache entry".into(),
                    ));
                };
                let Resolved::Archive { subdir, .. } = resolved else {
                    return Err(WspmError::Unsupported("expected archive resolution".into()));
                };
                read_from_archive($kind, path, subdir.as_deref(), rel)
            }

            fn materialize(
                &self,
                entry: &CacheEntry,
                resolved: &Resolved,
                spec: &RepoSpec,
                dest: &Path,
            ) -> Result<()> {
                if dest.exists() {
                    return Err(WspmError::DestExists(dest.to_path_buf()));
                }
                let CacheEntry::Archive(path) = entry else {
                    return Err(WspmError::Unsupported(
                        "expected archive cache entry".into(),
                    ));
                };
                let Resolved::Archive { sha256, subdir } = resolved else {
                    return Err(WspmError::Unsupported("expected archive resolution".into()));
                };
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::create_dir_all(dest)?;
                if let Err(e) = extract($kind, path, dest) {
                    let _ = fs::remove_dir_all(dest);
                    return Err(e);
                }
                write_marker(
                    dest,
                    &ArchiveMarker {
                        sha256: sha256.clone(),
                        url: spec.url.clone(),
                        subdir: subdir.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    },
                )?;
                Ok(())
            }

            fn status(&self, dest: &Path) -> Result<WorktreeState> {
                if !dest.exists() {
                    return Ok(WorktreeState {
                        present: false,
                        dirty: false,
                        current: None,
                    });
                }
                match read_marker(dest)? {
                    Some(marker) => Ok(WorktreeState {
                        present: true,
                        dirty: false,
                        current: Some(marker.sha256),
                    }),
                    // Exists but not placed by us: don't touch without --force.
                    None => Ok(WorktreeState {
                        present: true,
                        dirty: true,
                        current: None,
                    }),
                }
            }
        }
    };
}

impl_archive_provider!(TarProvider, SourceKind::Tar);
impl_archive_provider!(ZipProvider, SourceKind::Zip);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::validate_relative_path;
    use std::io::Write as _;
    use tempfile::TempDir;

    fn make_tar_gz(path: &Path, entries: &[(&str, &[u8])]) {
        let file = fs::File::create(path).unwrap();
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(enc);
        for (name, content) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, *content).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
    }

    fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = fs::File::create(path).unwrap();
        let mut w = zip::ZipWriter::new(file);
        for (name, content) in entries {
            w.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(content).unwrap();
        }
        w.finish().unwrap();
    }

    fn tar_spec(url: &str, version: VersionSpec) -> RepoSpec {
        RepoSpec {
            path: validate_relative_path("vendor/foo").unwrap(),
            kind: SourceKind::Tar,
            url: url.to_string(),
            version,
            subpaths: vec![],
        }
    }

    #[test]
    fn tar_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("foo-1.2.0.tar.gz");
        make_tar_gz(
            &archive,
            &[
                (
                    "foo-1.2.0/.repos",
                    b"repositories:\n  a: {type: git, url: x}\n",
                ),
                ("foo-1.2.0/a.txt", b"data\n"),
            ],
        );

        let cache = Cache::with_root(tmp.path().join("cache"));
        let provider = TarProvider;
        let spec = tar_spec(
            archive.to_str().unwrap(),
            VersionSpec::Subdir("foo-1.2.0".into()),
        );
        let entry = provider.ensure_cached(&cache, &spec).unwrap();
        let resolved = provider
            .resolve(&entry, &VersionSpec::Subdir("foo-1.2.0".into()))
            .unwrap();
        let Resolved::Archive { sha256, subdir } = &resolved else {
            panic!("expected archive resolution");
        };
        assert_eq!(*subdir, Some(PathBuf::from("foo-1.2.0")));
        assert_eq!(sha256.len(), 64);

        // read_file finds .repos under the subdir
        let content = provider
            .read_file(&entry, &resolved, ".repos")
            .unwrap()
            .unwrap();
        assert!(String::from_utf8(content)
            .unwrap()
            .contains("repositories:"));
        assert!(provider
            .read_file(&entry, &resolved, "missing")
            .unwrap()
            .is_none());

        // materialize + status
        let dest = tmp.path().join("ws/vendor/foo");
        provider
            .materialize(&entry, &resolved, &spec, &dest)
            .unwrap();
        assert!(dest.join(MARKER).is_file());
        // whole archive is extracted (subdir stays as-is on disk)
        assert!(dest.join("foo-1.2.0/a.txt").is_file());
        let st = provider.status(&dest).unwrap();
        assert!(st.present && !st.dirty);
        assert_eq!(st.current.as_deref(), Some(sha256.as_str()));
    }

    /// Build a gzip'd tar containing a `../evil.txt` entry by hand:
    /// `tar::Builder` refuses to create such entries, which is exactly the
    /// point (we must also refuse to *extract* them).
    fn make_evil_tar_gz(path: &Path) {
        fn header(name: &str, size: u64) -> [u8; 512] {
            let mut h = [0u8; 512];
            h[..name.len()].copy_from_slice(name.as_bytes());
            h[100..108].copy_from_slice(b"0000644\0");
            let size_s = format!("{size:011o}\0");
            h[124..136].copy_from_slice(size_s.as_bytes());
            h[156] = b'0'; // regular file
            h[257..263].copy_from_slice(b"ustar\0");
            h[263..265].copy_from_slice(b"00");
            h[148..156].copy_from_slice(b"        ");
            let sum: u32 = h.iter().map(|&b| b as u32).sum();
            let sum_s = format!("{sum:06o}\0 ");
            h[148..156].copy_from_slice(sum_s.as_bytes());
            h
        }
        let data = b"pwned";
        let mut buf = Vec::new();
        buf.extend_from_slice(&header("../evil.txt", data.len() as u64));
        buf.extend_from_slice(data);
        buf.resize(buf.len() + (512 - data.len() % 512) % 512, 0);
        buf.extend_from_slice(&[0u8; 1024]); // end-of-archive markers
        let file = fs::File::create(path).unwrap();
        let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        enc.write_all(&buf).unwrap();
        enc.finish().unwrap();
    }

    #[test]
    fn tar_zip_slip_rejected() {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("evil.tar.gz");
        make_evil_tar_gz(&archive);

        let cache = Cache::with_root(tmp.path().join("cache"));
        let provider = TarProvider;
        let spec = tar_spec(archive.to_str().unwrap(), VersionSpec::Default);
        let entry = provider.ensure_cached(&cache, &spec).unwrap();
        let resolved = provider.resolve(&entry, &VersionSpec::Default).unwrap();
        let dest = tmp.path().join("ws/out");
        assert!(provider
            .materialize(&entry, &resolved, &spec, &dest)
            .is_err());
        assert!(!tmp.path().join("evil.txt").exists());
        assert!(!tmp.path().join("ws/evil.txt").exists());
    }

    #[test]
    fn zip_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("foo.zip");
        make_zip(
            &archive,
            &[
                ("foo-1.2.0/.repos", b"repositories: {}\n"),
                ("foo-1.2.0/a.txt", b"data\n"),
            ],
        );

        let cache = Cache::with_root(tmp.path().join("cache"));
        let provider = ZipProvider;
        let spec = RepoSpec {
            kind: SourceKind::Zip,
            ..tar_spec(archive.to_str().unwrap(), VersionSpec::Default)
        };
        let entry = provider.ensure_cached(&cache, &spec).unwrap();
        let resolved = provider.resolve(&entry, &VersionSpec::Default).unwrap();
        let dest = tmp.path().join("ws/vendor/foo");
        provider
            .materialize(&entry, &resolved, &spec, &dest)
            .unwrap();
        assert!(dest.join("foo-1.2.0/a.txt").is_file());
        let st = provider.status(&dest).unwrap();
        assert!(st.present && !st.dirty);
    }

    #[test]
    fn unknown_extension_errors() {
        assert!(archive_ext("https://example.com/foo.rar").is_err());
        assert_eq!(archive_ext("https://example.com/a.tgz").unwrap(), "tar.gz");
        assert_eq!(
            archive_ext("https://example.com/a.TAR.XZ").unwrap(),
            "tar.xz"
        );
    }
}
