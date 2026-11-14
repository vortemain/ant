//! Content-addressed local cache for verified action outputs.
//!
//! Entries are addressed by the action-key candidate digest. A key may only
//! be stored or looked up when `action_key_candidate` reported no blockers;
//! enforcing that gate is the caller's responsibility and is not bypassable
//! through this crate's API (the run path calls `store`/`lookup` only for
//! eligible keys; see `ant-cli/src/cache_runner.rs` and decision D-003).
//!
//! Storage layout under the cache root:
//! - `blobs/<hex>`: content-addressed file blobs (deduplicated across entries).
//! - `entries/<hex>/manifest.json`: one entry per action key, listing the
//!   declared outputs with per-file digests.
//!
//! Restores verify every blob against its recorded digest before using it
//! and fail on mismatch, treating a corrupted cache as a miss rather than as
//! trusted data (a corrupted blob is deleted so the next store rewrites it).
//! When the process enabled change detection (developer commands, D-027),
//! a blob whose filesystem stamp is unchanged since it was last verified is
//! not re-read.
//!
//! Caches opened with [`Cache::with_links`] (local reuse in `ant build`,
//! only while change detection is on) restore by hard-linking outputs from
//! the blob store when the store and the workspace share a volume, falling
//! back to a copy (which is re-digested). A linked output *is* the blob, so
//! before an action runs again the caller calls [`Cache::break_links`],
//! which gives every still-linked declared output a private copy: tools
//! that rewrite outputs in place (`copy /Y`, incremental packagers) then
//! never write into the store. If something outside Ant rewrites a linked
//! output in place anyway, the blob's stamp changes, the next restore
//! re-digests it, finds the mismatch, and drops the blob. Stores always copy, because build tools
//! often hard-link their own outputs (cargo uplifts `target/<profile>/x`
//! from `target/<profile>/deps`) and a linker writing in place there must
//! never reach the store.

use build_action::OutputKind;
use build_digest::{ContentDigest, DigestCache, DigestError, digest_file, tree_entries};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

#[derive(Debug)]
pub enum CacheError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Digest {
        source: DigestError,
    },
    InvalidEntry {
        path: PathBuf,
        reason: String,
    },
    /// A restored output did not match its recorded digest. Partial outputs
    /// are removed before this is returned.
    Integrity {
        path: PathBuf,
    },
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "cache IO error on {}: {source}", path.display())
            }
            Self::Digest { source } => write!(f, "cache digest error: {source}"),
            Self::InvalidEntry { path, reason } => {
                write!(f, "invalid cache entry {}: {reason}", path.display())
            }
            Self::Integrity { path } => {
                write!(
                    f,
                    "restored output does not match its recorded digest: {}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for CacheError {}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    action_key: String,
    outputs: Vec<ManifestOutput>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestOutput {
    path: String,
    kind: OutputKind,
    /// Directory entries preserved so empty directories round-trip.
    #[serde(default)]
    dirs: Vec<String>,
    #[serde(default)]
    files: Vec<ManifestFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    path: String,
    digest: String,
}

/// A stored entry ready to be restored into a workspace.
#[derive(Clone, Debug)]
pub struct CachedEntry {
    manifest: Manifest,
}

impl CachedEntry {
    pub fn output_paths(&self) -> Vec<String> {
        self.manifest
            .outputs
            .iter()
            .map(|output| output.path.clone())
            .collect()
    }
}

pub struct Cache {
    root: PathBuf,
    links: bool,
}

impl Cache {
    /// Open (creating if needed) a cache rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, CacheError> {
        let root = root.into();
        for directory in ["", "blobs", "entries", "tmp"] {
            let path = root.join(directory);
            fs::create_dir_all(&path).map_err(|source| CacheError::Io { path, source })?;
        }
        Ok(Self { root, links: false })
    }

    /// Restore by hard link where possible (see the crate docs). Links are
    /// used only while the process has change detection enabled, which is
    /// what lets [`Cache::break_links`] recognize a linked output cheaply.
    pub fn with_links(mut self, links: bool) -> Self {
        self.links = links;
        self
    }

    /// Give every declared output that is still a hard link to a blob of
    /// this store a private copy, so the action about to run cannot write
    /// into the store through it. Outputs Ant did not link are untouched.
    pub fn break_links(
        &self,
        workspace: &Path,
        outputs: &[(PathBuf, OutputKind)],
    ) -> Result<(), CacheError> {
        if !self.links {
            return Ok(());
        }
        let Some(workspace_cache) = change_cache(workspace) else {
            return Ok(());
        };
        let mut files = Vec::new();
        for (path, kind) in outputs {
            let absolute = workspace.join(path);
            match kind {
                OutputKind::File => files.push(absolute),
                OutputKind::Tree => collect_files(&absolute, &mut files),
            }
        }
        for file in files {
            let Ok(metadata) = fs::symlink_metadata(&file) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            // Restores record each linked output's digest, which names the
            // blob it may be linked to.
            let Some(digest) = workspace_cache.recorded(&file, &metadata) else {
                continue;
            };
            let blob = self.blob_path(&digest);
            let Ok(blob_metadata) = fs::symlink_metadata(&blob) else {
                continue;
            };
            if !same_file(&metadata, &blob_metadata) {
                continue;
            }
            let private = file.with_file_name(format!(
                ".{}.ant-unlink-{}",
                file.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                std::process::id()
            ));
            fs::copy(&blob, &private).map_err(|source| CacheError::Io {
                path: private.clone(),
                source,
            })?;
            fs::rename(&private, &file).map_err(|source| {
                let _ = fs::remove_file(&private);
                CacheError::Io {
                    path: file.clone(),
                    source,
                }
            })?;
        }
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn entry_dir(&self, key: &ContentDigest) -> PathBuf {
        self.root.join("entries").join(hex(key))
    }

    fn blob_path(&self, digest: &ContentDigest) -> PathBuf {
        self.root.join("blobs").join(hex(digest))
    }

    /// Whether a manifest for `key` exists. A malformed manifest counts as
    /// absent; corruption must not fail inspection.
    pub fn entry_exists(&self, key: &ContentDigest) -> bool {
        let path = self.entry_dir(key).join("manifest.json");
        match fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str::<Manifest>(&content).is_ok(),
            Err(_) => false,
        }
    }

    /// Store the current on-disk content of `outputs` under `key`.
    ///
    /// Every file is copied into the blob store and its digest recorded; the
    /// caller must already have verified these outputs (the executor does).
    /// Storing an existing key is a no-op: entries are immutable.
    pub fn store(
        &self,
        key: &ContentDigest,
        workspace: &Path,
        outputs: &[(PathBuf, OutputKind)],
    ) -> Result<(), CacheError> {
        // Entries are immutable, but an entry whose blob was dropped (found
        // corrupted on restore) is rewritten so it can be reused again.
        if matches!(self.lookup(key), Ok(Some(_))) {
            return Ok(());
        }
        let mut manifest_outputs = Vec::with_capacity(outputs.len());
        for (path, kind) in outputs {
            let relative = validate_relative(path).map_err(|reason| CacheError::InvalidEntry {
                path: path.clone(),
                reason,
            })?;
            let absolute = workspace.join(path);
            let mut manifest_output = ManifestOutput {
                path: relative.clone(),
                kind: *kind,
                dirs: Vec::new(),
                files: Vec::new(),
            };
            match kind {
                OutputKind::File => {
                    let digest = self.ensure_blob(&absolute)?;
                    manifest_output.files.push(ManifestFile {
                        path: relative,
                        digest: digest.to_string(),
                    });
                }
                OutputKind::Tree => {
                    for entry in
                        tree_entries(&absolute).map_err(|source| CacheError::Digest { source })?
                    {
                        if entry.is_dir {
                            manifest_output
                                .dirs
                                .push(format!("{relative}/{}", entry.path));
                        } else {
                            let digest = entry.digest.ok_or_else(|| CacheError::InvalidEntry {
                                path: absolute.clone(),
                                reason: "tree file entry without digest".to_owned(),
                            })?;
                            let file_relative = format!("{relative}/{}", entry.path);
                            manifest_output.files.push(ManifestFile {
                                path: file_relative,
                                digest: digest.to_string(),
                            });
                            // The blob was already hashed while walking the
                            // tree; copy the file content into the store.
                            self.copy_blob(&absolute.join(&entry.path), &digest)?;
                        }
                    }
                }
            }
            manifest_outputs.push(manifest_output);
        }
        let manifest = Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            action_key: key.to_string(),
            outputs: manifest_outputs,
        };
        self.write_manifest(key, &manifest)
    }

    /// Return the stored entry for `key`, or `None` on any miss (absent,
    /// unreadable, unparsable manifest, or missing blobs).
    pub fn lookup(&self, key: &ContentDigest) -> Result<Option<CachedEntry>, CacheError> {
        let manifest_path = self.entry_dir(key).join("manifest.json");
        let content = match fs::read_to_string(&manifest_path) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(CacheError::Io {
                    path: manifest_path,
                    source,
                });
            }
        };
        let manifest: Manifest = match serde_json::from_str(&content) {
            Ok(manifest) => manifest,
            Err(_) => return Ok(None),
        };
        if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
            return Ok(None);
        }
        if manifest.action_key != key.to_string() {
            return Ok(None);
        }
        if manifest.outputs.is_empty() {
            return Ok(None);
        }
        for output in &manifest.outputs {
            if validate_relative(Path::new(&output.path)).is_err() {
                return Ok(None);
            }
            for file in &output.files {
                if validate_relative(Path::new(&file.path)).is_err() {
                    return Ok(None);
                }
                let Ok(digest) = file.digest.parse::<ContentDigest>() else {
                    return Ok(None);
                };
                if !self.blob_path(&digest).is_file() {
                    return Ok(None);
                }
            }
        }
        Ok(Some(CachedEntry { manifest }))
    }

    /// Restore a looked-up entry into `workspace`, re-digesting every
    /// restored file against the manifest. On integrity failure the partially
    /// restored files are removed and an error is returned; the caller must
    /// treat the key as a miss and rebuild.
    pub fn restore(
        &self,
        entry: &CachedEntry,
        workspace: &Path,
        declared_outputs: &[(PathBuf, OutputKind)],
    ) -> Result<(), CacheError> {
        // The entry must cover exactly the outputs the action declares.
        let declared: Vec<String> = declared_outputs
            .iter()
            .map(|(path, _)| {
                validate_relative(path).map_err(|reason| CacheError::InvalidEntry {
                    path: path.clone(),
                    reason,
                })
            })
            .collect::<Result<_, _>>()?;
        let mut stored: Vec<String> = entry.output_paths();
        stored.sort();
        let mut expected = declared.clone();
        expected.sort();
        if stored != expected {
            return Err(CacheError::InvalidEntry {
                path: self.entry_dir(&parse_key(&entry.manifest)?),
                reason: format!(
                    "entry outputs {:?} do not match declared outputs {:?}",
                    stored, expected
                ),
            });
        }
        let mut restored: Vec<PathBuf> = Vec::new();
        let result = self.restore_outputs(entry, workspace, declared_outputs, &mut restored);
        if result.is_err() {
            for path in restored {
                let _ = fs::remove_file(&path);
            }
        }
        result
    }

    fn restore_outputs(
        &self,
        entry: &CachedEntry,
        workspace: &Path,
        declared_outputs: &[(PathBuf, OutputKind)],
        restored: &mut Vec<PathBuf>,
    ) -> Result<(), CacheError> {
        let workspace_cache = change_cache(workspace);
        let blob_cache = change_cache(&self.root.join("blobs"));
        for (manifest_output, (declared_path, declared_kind)) in
            entry.manifest.outputs.iter().zip(declared_outputs)
        {
            if manifest_output.path
                != validate_relative(declared_path).map_err(|reason| CacheError::InvalidEntry {
                    path: declared_path.clone(),
                    reason,
                })?
            {
                return Err(CacheError::InvalidEntry {
                    path: declared_path.clone(),
                    reason: "output order mismatch".to_owned(),
                });
            }
            if manifest_output.kind != *declared_kind {
                return Err(CacheError::InvalidEntry {
                    path: declared_path.clone(),
                    reason: "output kind mismatch".to_owned(),
                });
            }
            for directory in &manifest_output.dirs {
                fs::create_dir_all(workspace.join(directory)).map_err(|source| CacheError::Io {
                    path: workspace.join(directory),
                    source,
                })?;
            }
            for file in &manifest_output.files {
                let Ok(blob_digest) = file.digest.parse::<ContentDigest>() else {
                    return Err(CacheError::InvalidEntry {
                        path: self.root.clone(),
                        reason: format!("unparsable digest {}", file.digest),
                    });
                };
                let target = workspace.join(&file.path);
                // Already in place with the right content: nothing to write
                // (digest through the change-detection cache when enabled,
                // so an unchanged output is not even re-read; D-028).
                if current_digest(&target, workspace_cache.as_deref())
                    .is_some_and(|digest| digest == blob_digest)
                {
                    continue;
                }
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(|source| CacheError::Io {
                        path: parent.to_path_buf(),
                        source,
                    })?;
                }
                let blob = self.blob_path(&blob_digest);
                if current_digest(&blob, blob_cache.as_deref()) != Some(blob_digest) {
                    // Corrupted (or vanished) store content: drop it so the
                    // next store of this key writes a good copy.
                    let _ = fs::remove_file(&blob);
                    return Err(CacheError::Integrity { path: blob });
                }
                remove_existing_file(&target)?;
                let linked = self.links
                    && workspace_cache.is_some()
                    && fs::hard_link(&blob, &target).is_ok();
                if !linked {
                    fs::copy(&blob, &target).map_err(|source| CacheError::Io {
                        path: target.clone(),
                        source,
                    })?;
                }
                restored.push(target.clone());
                if linked {
                    // The target is the blob just verified. Re-record both
                    // stamps: creating a link changes ctime on Unix.
                    for (path, cache) in [(&blob, &blob_cache), (&target, &workspace_cache)] {
                        if let (Some(cache), Ok(metadata)) = (cache, fs::metadata(path)) {
                            cache.record(path, &metadata, blob_digest);
                        }
                    }
                } else {
                    let actual =
                        digest_file(&target).map_err(|source| CacheError::Digest { source })?;
                    if actual != blob_digest {
                        return Err(CacheError::Integrity { path: target });
                    }
                    if let (Some(cache), Ok(metadata)) = (&workspace_cache, fs::metadata(&target)) {
                        cache.record(&target, &metadata, blob_digest);
                    }
                }
            }
        }
        Ok(())
    }

    fn ensure_blob(&self, source: &Path) -> Result<ContentDigest, CacheError> {
        let digest = digest_file(source).map_err(|source| CacheError::Digest { source })?;
        self.copy_blob(source, &digest)?;
        Ok(digest)
    }

    fn copy_blob(&self, source: &Path, digest: &ContentDigest) -> Result<(), CacheError> {
        let blob = self.blob_path(digest);
        if blob.is_file() {
            return Ok(());
        }
        // Copy to a temporary name first so concurrent stores never expose a
        // partially written blob under its final content-addressed name.
        let tmp =
            self.root
                .join("tmp")
                .join(format!("blob-{}-{}", std::process::id(), hex(digest)));
        fs::copy(source, &tmp).map_err(|source| CacheError::Io {
            path: tmp.clone(),
            source,
        })?;
        match fs::rename(&tmp, &blob) {
            Ok(()) => Ok(()),
            Err(_) if blob.is_file() => {
                // Another store won the race; drop our temporary copy.
                let _ = fs::remove_file(&tmp);
                Ok(())
            }
            Err(source) => Err(CacheError::Io { path: blob, source }),
        }
    }

    fn write_manifest(&self, key: &ContentDigest, manifest: &Manifest) -> Result<(), CacheError> {
        let entry_dir = self.entry_dir(key);
        fs::create_dir_all(&entry_dir).map_err(|source| CacheError::Io {
            path: entry_dir.clone(),
            source,
        })?;
        let final_path = entry_dir.join("manifest.json");
        let tmp =
            self.root
                .join("tmp")
                .join(format!("manifest-{}-{}", std::process::id(), hex(key)));
        let content =
            serde_json::to_string_pretty(manifest).map_err(|error| CacheError::InvalidEntry {
                path: final_path.clone(),
                reason: error.to_string(),
            })?;
        fs::write(&tmp, content).map_err(|source| CacheError::Io {
            path: tmp.clone(),
            source,
        })?;
        match fs::rename(&tmp, &final_path) {
            Ok(()) => Ok(()),
            Err(source) => Err(CacheError::Io {
                path: final_path,
                source,
            }),
        }
    }
}

fn parse_key(manifest: &Manifest) -> Result<ContentDigest, CacheError> {
    manifest
        .action_key
        .parse::<ContentDigest>()
        .map_err(|_| CacheError::InvalidEntry {
            path: PathBuf::from("<manifest>"),
            reason: format!("unparsable action key {}", manifest.action_key),
        })
}

/// Reject absolute paths, parent traversal, and empty paths in entry
/// metadata. Manifests live on disk and are treated as untrusted input.
fn validate_relative(path: &Path) -> Result<String, String> {
    if path.as_os_str().is_empty() {
        return Err("empty path".to_owned());
    }
    let mut normalized = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                let name = name.to_str().ok_or_else(|| "non-UTF-8 path".to_owned())?;
                normalized.push(name);
            }
            _ => return Err(format!("path component is not allowed: {component:?}")),
        }
    }
    Ok(normalized.join("/"))
}

fn hex(digest: &ContentDigest) -> String {
    digest.0.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The change-detection cache for `scope` (a workspace or the blob
/// directory), when this process enabled change detection.
fn change_cache(scope: &Path) -> Option<Arc<DigestCache>> {
    scope
        .canonicalize()
        .ok()
        .and_then(|root| build_digest::workspace_cache(&root))
}

/// The content digest of an existing regular file, through `cache` when
/// given; `None` when the file is missing, not a regular file, or unreadable.
fn current_digest(target: &Path, cache: Option<&DigestCache>) -> Option<ContentDigest> {
    let metadata = fs::symlink_metadata(target).ok()?;
    if !metadata.is_file() {
        return None;
    }
    match cache {
        Some(cache) => cache.digest(target, &metadata, false).ok(),
        None => digest_file(target).ok(),
    }
}

/// Every regular file below `dir` (missing directories give nothing).
fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => collect_files(&entry.path(), files),
            Ok(kind) if kind.is_file() => files.push(entry.path()),
            _ => {}
        }
    }
}

/// Whether two metadata records describe the same file (hard links share
/// them): device + inode on Unix; size, modification, and creation time
/// elsewhere (a copy gets a new creation time).
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    #[cfg(not(unix))]
    {
        a.len() == b.len()
            && a.modified().ok() == b.modified().ok()
            && a.created().ok().is_some()
            && a.created().ok() == b.created().ok()
    }
}

/// Remove a file or symlink at `target` so a link can take its place; a
/// missing target is fine.
fn remove_existing_file(target: &Path) -> Result<(), CacheError> {
    match fs::symlink_metadata(target) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(CacheError::Io {
            path: target.to_path_buf(),
            source,
        }),
        Ok(metadata) if metadata.is_dir() => Err(CacheError::InvalidEntry {
            path: target.to_path_buf(),
            reason: "a directory is in the way of a restored file".to_owned(),
        }),
        Ok(_) => fs::remove_file(target).map_err(|source| CacheError::Io {
            path: target.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use build_action::OutputKind;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("ant-cache-{name}-{}-{unique}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, relative: &str, contents: &[u8]) -> PathBuf {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent");
            }
            fs::write(&path, contents).expect("write file");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn key(digest: ContentDigest) -> ContentDigest {
        digest
    }

    #[test]
    fn store_lookup_restore_round_trip_file_and_tree() {
        let workspace = TempDir::new("ws");
        let restored_ws = TempDir::new("restored");
        let cache_dir = TempDir::new("cache");
        workspace.write("out.txt", b"artifact");
        workspace.write("tree/nested/deep.bin", b"deep");
        workspace.write("tree/file.txt", b"leaf");
        fs::create_dir_all(workspace.path().join("tree/empty")).expect("empty dir");

        let outputs = vec![
            (PathBuf::from("out.txt"), OutputKind::File),
            (PathBuf::from("tree"), OutputKind::Tree),
        ];
        let cache = Cache::open(cache_dir.path()).expect("open cache");
        let action_key = key(digest_file(&workspace.path().join("out.txt")).unwrap());
        cache
            .store(&action_key, workspace.path(), &outputs)
            .expect("store");

        let entry = cache.lookup(&action_key).expect("lookup").expect("hit");
        cache
            .restore(&entry, restored_ws.path(), &outputs)
            .expect("restore");
        assert_eq!(
            fs::read(restored_ws.path().join("out.txt")).expect("restored file"),
            b"artifact"
        );
        assert_eq!(
            build_digest::digest_tree(&restored_ws.path().join("tree")).expect("tree digest"),
            build_digest::digest_tree(&workspace.path().join("tree")).expect("original digest"),
            "restored tree must be byte-identical, including the empty directory"
        );
    }

    #[test]
    fn unknown_key_is_a_miss() {
        let cache_dir = TempDir::new("miss");
        let cache = Cache::open(cache_dir.path()).expect("open cache");
        let unknown = key(ContentDigest([7u8; 32]));
        assert!(cache.lookup(&unknown).expect("lookup").is_none());
        assert!(!cache.entry_exists(&unknown));
    }

    #[test]
    fn storing_the_same_key_twice_is_a_no_op() {
        let workspace = TempDir::new("noop-ws");
        let cache_dir = TempDir::new("noop-cache");
        workspace.write("out.txt", b"one");
        let cache = Cache::open(cache_dir.path()).expect("open cache");
        let action_key = key(digest_file(&workspace.path().join("out.txt")).unwrap());
        let outputs = vec![(PathBuf::from("out.txt"), OutputKind::File)];
        cache
            .store(&action_key, workspace.path(), &outputs)
            .expect("first store");
        // Change the file; the stored entry must remain immutable.
        workspace.write("out.txt", b"two");
        cache
            .store(&action_key, workspace.path(), &outputs)
            .expect("second store");
        let restored = TempDir::new("noop-restore");
        let entry = cache.lookup(&action_key).expect("lookup").expect("hit");
        cache
            .restore(&entry, restored.path(), &outputs)
            .expect("restore");
        assert_eq!(
            fs::read(restored.path().join("out.txt")).expect("restored"),
            b"one"
        );
    }

    #[test]
    fn corrupted_blob_is_detected_on_restore() {
        let workspace = TempDir::new("corrupt-ws");
        let cache_dir = TempDir::new("corrupt-cache");
        workspace.write("out.txt", b"original");
        let cache = Cache::open(cache_dir.path()).expect("open cache");
        let action_key = key(digest_file(&workspace.path().join("out.txt")).unwrap());
        let outputs = vec![(PathBuf::from("out.txt"), OutputKind::File)];
        cache
            .store(&action_key, workspace.path(), &outputs)
            .expect("store");

        // Corrupt the single stored blob in place.
        let blob = cache.root().join("blobs");
        let blob_path = fs::read_dir(&blob)
            .expect("read blobs")
            .next()
            .expect("one blob")
            .unwrap()
            .path();
        fs::write(&blob_path, b"tampered").expect("corrupt blob");

        let entry = cache.lookup(&action_key).expect("lookup").expect("hit");
        let restored = TempDir::new("corrupt-restore");
        let error = cache
            .restore(&entry, restored.path(), &outputs)
            .unwrap_err();
        assert!(
            matches!(error, CacheError::Integrity { .. }),
            "got {error:?}"
        );
        // The bad output must not be left behind for the build to consume.
        assert!(
            !restored.path().join("out.txt").exists(),
            "partially restored output must be removed on integrity failure"
        );
    }

    #[test]
    fn restore_links_and_recovers_from_in_place_rewrites() {
        let workspace = TempDir::new("link-ws");
        let cache_dir = TempDir::new("link-cache");
        workspace.write("bin/app", b"program v1");
        let cache = Cache::open(cache_dir.path()).expect("open cache");
        let action_key = key(digest_file(&workspace.path().join("bin/app")).unwrap());
        let outputs = vec![(PathBuf::from("bin/app"), OutputKind::File)];
        cache
            .store(&action_key, workspace.path(), &outputs)
            .expect("store");

        // The store copies: rewriting the original output in place must not
        // reach the stored blob.
        fs::write(workspace.path().join("bin/app"), b"edited in place").expect("rewrite");
        let entry = cache.lookup(&action_key).expect("lookup").expect("hit");
        cache
            .restore(&entry, workspace.path(), &outputs)
            .expect("restore after the original was rewritten");
        assert_eq!(
            fs::read(workspace.path().join("bin/app")).unwrap(),
            b"program v1"
        );

        // A tool rewriting the (possibly linked) restored output in place
        // may change the blob; the next restore must notice, drop the blob,
        // and report a miss instead of restoring the wrong bytes.
        fs::write(workspace.path().join("bin/app"), b"tampered").expect("rewrite");
        let other = TempDir::new("link-other");
        match cache.restore(&entry, other.path(), &outputs) {
            Ok(()) => assert_eq!(
                fs::read(other.path().join("bin/app")).unwrap(),
                b"program v1",
                "a restore that succeeds must produce the stored content"
            ),
            Err(error) => {
                assert!(
                    matches!(error, CacheError::Integrity { .. }),
                    "got {error:?}"
                );
                assert!(!other.path().join("bin/app").exists());
                // The entry is now a miss, and storing the key again (after
                // a rebuild) makes it reusable again.
                assert!(cache.lookup(&action_key).expect("lookup").is_none());
                fs::write(workspace.path().join("bin/app"), b"program v1").expect("rebuild");
                cache
                    .store(&action_key, workspace.path(), &outputs)
                    .expect("store again");
                let entry = cache.lookup(&action_key).expect("lookup").expect("hit");
                cache
                    .restore(&entry, other.path(), &outputs)
                    .expect("restore after re-store");
                assert_eq!(
                    fs::read(other.path().join("bin/app")).unwrap(),
                    b"program v1"
                );
            }
        }
    }

    #[test]
    fn linked_restores_are_detached_before_a_tool_rewrites_them() {
        // Links need change detection (process-wide; other tests stay
        // correct with it on, since tampering re-stamps files).
        let digests = TempDir::new("link-digests");
        build_digest::enable_workspace_caches(digests.path().to_path_buf());
        let workspace = TempDir::new("detach-ws");
        let cache_dir = TempDir::new("detach-cache");
        workspace.write("out/app", b"program v1");
        let cache = Cache::open(cache_dir.path())
            .expect("open cache")
            .with_links(true);
        let action_key = key(digest_file(&workspace.path().join("out/app")).unwrap());
        let outputs = vec![(PathBuf::from("out/app"), OutputKind::File)];
        cache
            .store(&action_key, workspace.path(), &outputs)
            .expect("store");
        let blob = cache.blob_path(&digest_file(&workspace.path().join("out/app")).unwrap());

        let restored = TempDir::new("detach-restored");
        let entry = cache.lookup(&action_key).expect("lookup").expect("hit");
        cache
            .restore(&entry, restored.path(), &outputs)
            .expect("restore");
        let target = restored.path().join("out/app");
        assert_eq!(fs::read(&target).unwrap(), b"program v1");
        // Store and workspace share the temp volume, so this was a link.
        assert!(
            same_file(
                &fs::metadata(&target).unwrap(),
                &fs::metadata(&blob).unwrap()
            ),
            "same-volume restores hard-link from the store"
        );

        cache
            .break_links(restored.path(), &outputs)
            .expect("detach");
        assert!(
            !same_file(
                &fs::metadata(&target).unwrap(),
                &fs::metadata(&blob).unwrap()
            ),
            "after detaching, the output is a private file"
        );
        assert_eq!(fs::read(&target).unwrap(), b"program v1");

        // An in-place rewrite now leaves the store intact.
        fs::write(&target, b"rewritten in place").expect("rewrite");
        assert_eq!(fs::read(&blob).unwrap(), b"program v1");
        let other = TempDir::new("detach-other");
        cache
            .restore(&entry, other.path(), &outputs)
            .expect("restore again");
        assert_eq!(
            fs::read(other.path().join("out/app")).unwrap(),
            b"program v1"
        );
    }

    #[test]
    fn missing_blob_is_a_miss_not_an_error() {
        let workspace = TempDir::new("noblob-ws");
        let cache_dir = TempDir::new("noblob-cache");
        workspace.write("out.txt", b"content");
        let cache = Cache::open(cache_dir.path()).expect("open cache");
        let action_key = key(digest_file(&workspace.path().join("out.txt")).unwrap());
        let outputs = vec![(PathBuf::from("out.txt"), OutputKind::File)];
        cache
            .store(&action_key, workspace.path(), &outputs)
            .expect("store");
        let blob = cache.root().join("blobs");
        let blob_path = fs::read_dir(&blob)
            .expect("read blobs")
            .next()
            .expect("one blob")
            .unwrap()
            .path();
        fs::remove_file(blob_path).expect("remove blob");
        assert!(cache.lookup(&action_key).expect("lookup").is_none());
    }

    #[test]
    fn manifest_paths_are_treated_as_untrusted() {
        let cache_dir = TempDir::new("traversal");
        let cache = Cache::open(cache_dir.path()).expect("open cache");
        let action_key = key(ContentDigest([3u8; 32]));
        let entry_dir = cache.root().join("entries").join(hex(&action_key));
        fs::create_dir_all(&entry_dir).expect("entry dir");
        let manifest = r#"{
            "schema_version": 1,
            "action_key": "KEY",
            "outputs": [{
                "path": "ok.txt",
                "kind": "file",
                "files": [{"path": "../escape.txt", "digest": "sha256:0000000000000000000000000000000000000000000000000000000000000000"}]
            }]
        }"#.replace("KEY", &format!("sha256:{}", "0".repeat(64)));
        fs::write(entry_dir.join("manifest.json"), manifest).expect("write manifest");
        assert!(
            cache.lookup(&action_key).expect("lookup").is_none(),
            "traversal paths must make the entry a miss"
        );
    }

    #[test]
    fn identical_content_is_stored_once_across_entries() {
        let workspace = TempDir::new("dedup-ws");
        let cache_dir = TempDir::new("dedup-cache");
        workspace.write("a.txt", b"same content");
        workspace.write("b.txt", b"same content");
        let cache = Cache::open(cache_dir.path()).expect("open cache");
        let outputs_a = vec![(PathBuf::from("a.txt"), OutputKind::File)];
        let outputs_b = vec![(PathBuf::from("b.txt"), OutputKind::File)];
        let key_a = key(digest_file(&workspace.path().join("a.txt")).unwrap());
        let key_b = key(digest_file(&workspace.path().join("b.txt")).unwrap());
        cache
            .store(&key_a, workspace.path(), &outputs_a)
            .expect("store a");
        cache
            .store(&key_b, workspace.path(), &outputs_b)
            .expect("store b");
        let blobs: Vec<_> = fs::read_dir(cache.root().join("blobs"))
            .expect("read blobs")
            .collect();
        assert_eq!(
            blobs.len(),
            1,
            "identical content must deduplicate to one blob"
        );
    }

    #[test]
    fn digest_parses_round_trip() {
        let dir = TempDir::new("parse");
        let original = digest_file(&dir.write("f", b"x")).expect("digest");
        let parsed: ContentDigest = original.to_string().parse().expect("parse");
        assert_eq!(parsed, original);
        assert!("deadbeef".parse::<ContentDigest>().is_err());
        assert!("sha256:zz".parse::<ContentDigest>().is_err());
    }
}
