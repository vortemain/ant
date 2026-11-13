//! Change detection for large workspaces (P3, D-027): reuse a file's digest
//! while its filesystem stamp is unchanged, so a no-change build reads
//! almost nothing from disk, even on a slow laptop.
//!
//! An entry is reused only when the file's size, modification time, and a
//! platform change marker (inode + ctime on Unix, creation time on Windows)
//! all match what was recorded, **and** the recording happened at least
//! [`RACY_WINDOW_NS`] after the modification time. The second rule is git's
//! "racily clean" guard: a file written within the same timestamp tick as a
//! recording could change again without its stamp changing, so such entries
//! are re-hashed until they are old enough. Trust model (same as make and
//! git): a file rewritten with identical size, modification time, and change
//! marker is not re-read. Caching is opt-in per process
//! ([`enable_workspace_caches`]); remote workers never enable it.
//!
//! Caches live outside the workspace (per-user cache directory, one file per
//! workspace), so they can never become build inputs. A corrupt or
//! unreadable cache file is ignored and rebuilt.

use crate::{ContentDigest, DigestError, digest_file, digest_file_with_metadata};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Entries are trusted only when recorded at least this long after the
/// file's last modification (2 s covers coarse filesystem timestamps).
pub const RACY_WINDOW_NS: u128 = 2_000_000_000;

const HEADER: &str = "ant-digest-cache v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    size: u64,
    modified_ns: u128,
    change_marker: u128,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    stamp: Stamp,
    digest: ContentDigest,
    recorded_ns: u128,
}

fn now_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn stamp(metadata: &fs::Metadata) -> Option<Stamp> {
    let modified_ns = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    #[cfg(unix)]
    let change_marker = {
        use std::os::unix::fs::MetadataExt as _;
        (u128::from(metadata.ino()) << 64)
            ^ (metadata.ctime() as u128).wrapping_mul(1_000_000_000)
            ^ metadata.ctime_nsec() as u128
    };
    #[cfg(not(unix))]
    let change_marker = metadata
        .created()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos());
    Some(Stamp {
        size: metadata.len(),
        modified_ns,
        change_marker,
    })
}

/// A persistent, thread-safe map from (file, digest mode) to its digest.
pub struct DigestCache {
    file: PathBuf,
    entries: Mutex<HashMap<(PathBuf, bool), Entry>>,
    dirty: Mutex<bool>,
}

impl DigestCache {
    /// Load the cache stored at `file`; a missing or unreadable file gives
    /// an empty cache.
    pub fn load(file: PathBuf) -> Self {
        let mut entries = HashMap::new();
        if let Ok(text) = fs::read_to_string(&file) {
            let mut lines = text.lines();
            if lines.next() == Some(HEADER) {
                for line in lines {
                    if let Some((key, entry)) = parse_line(line) {
                        entries.insert(key, entry);
                    }
                }
            }
        }
        Self {
            file,
            entries: Mutex::new(entries),
            dirty: Mutex::new(false),
        }
    }

    /// Digest `path` (content-only, or content + metadata when
    /// `with_metadata`), reusing the recorded digest when the file's stamp
    /// is unchanged and the entry is not racy.
    pub fn digest(
        &self,
        path: &Path,
        metadata: &fs::Metadata,
        with_metadata: bool,
    ) -> Result<ContentDigest, DigestError> {
        let compute = || {
            if with_metadata {
                digest_file_with_metadata(path)
            } else {
                digest_file(path)
            }
        };
        let Some(current) = stamp(metadata) else {
            return compute();
        };
        let key = (path.to_path_buf(), with_metadata);
        if let Some(entry) = self.entries.lock().expect("digest cache").get(&key)
            && entry.stamp == current
            && entry.recorded_ns >= current.modified_ns.saturating_add(RACY_WINDOW_NS)
        {
            return Ok(entry.digest);
        }
        let recorded_ns = now_ns();
        let digest = compute()?;
        self.entries.lock().expect("digest cache").insert(
            key,
            Entry {
                stamp: current,
                digest,
                recorded_ns,
            },
        );
        *self.dirty.lock().expect("digest cache") = true;
        Ok(digest)
    }

    /// Write the cache back if anything changed (atomic replace). Entries
    /// whose files no longer exist are dropped.
    pub fn save(&self) -> std::io::Result<()> {
        if !*self.dirty.lock().expect("digest cache") {
            return Ok(());
        }
        let entries = self.entries.lock().expect("digest cache");
        let mut text = String::with_capacity(entries.len() * 128 + 32);
        text.push_str(HEADER);
        text.push('\n');
        let mut keys: Vec<_> = entries.keys().collect();
        keys.sort();
        for key in keys {
            let entry = &entries[key];
            let Some(path) = key.0.to_str() else {
                continue;
            };
            if path.contains('\n') || !key.0.is_file() {
                continue;
            }
            text.push_str(&format!(
                "{} {} {} {} {} {} {}\n",
                if key.1 { 'm' } else { 'c' },
                entry.digest,
                entry.stamp.size,
                entry.stamp.modified_ns,
                entry.stamp.change_marker,
                entry.recorded_ns,
                path
            ));
        }
        let dir = self.file.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(dir)?;
        let temp = dir.join(format!(".digests-{}.tmp", std::process::id()));
        fs::write(&temp, text)?;
        fs::rename(&temp, &self.file).inspect_err(|_| {
            let _ = fs::remove_file(&temp);
        })?;
        *self.dirty.lock().expect("digest cache") = false;
        Ok(())
    }
}

fn parse_line(line: &str) -> Option<((PathBuf, bool), Entry)> {
    let mut parts = line.splitn(7, ' ');
    let mode = match parts.next()? {
        "m" => true,
        "c" => false,
        _ => return None,
    };
    let digest: ContentDigest = parts.next()?.parse().ok()?;
    let size = parts.next()?.parse().ok()?;
    let modified_ns = parts.next()?.parse().ok()?;
    let change_marker = parts.next()?.parse().ok()?;
    let recorded_ns = parts.next()?.parse().ok()?;
    let path = PathBuf::from(parts.next()?);
    Some((
        (path, mode),
        Entry {
            stamp: Stamp {
                size,
                modified_ns,
                change_marker,
            },
            digest,
            recorded_ns,
        },
    ))
}

struct Registry {
    dir: PathBuf,
    caches: HashMap<PathBuf, Arc<DigestCache>>,
}

static REGISTRY: OnceLock<Mutex<Option<Registry>>> = OnceLock::new();

fn registry() -> &'static Mutex<Option<Registry>> {
    REGISTRY.get_or_init(|| Mutex::new(None))
}

/// Turn on digest caching for this process, storing one cache file per
/// workspace under `dir`. Developer-facing commands call this; workers do
/// not.
pub fn enable_workspace_caches(dir: PathBuf) {
    *registry().lock().expect("digest registry") = Some(Registry {
        dir,
        caches: HashMap::new(),
    });
}

/// The cache for `workspace` (canonical path), when caching is enabled.
pub fn workspace_cache(workspace: &Path) -> Option<Arc<DigestCache>> {
    let mut guard = registry().lock().expect("digest registry");
    let registry = guard.as_mut()?;
    if let Some(cache) = registry.caches.get(workspace) {
        return Some(Arc::clone(cache));
    }
    let name = crate::digest_bytes(workspace.to_string_lossy().as_bytes()).to_string();
    let file = registry
        .dir
        .join(format!("{}.txt", name.trim_start_matches("sha256:")));
    let cache = Arc::new(DigestCache::load(file));
    registry
        .caches
        .insert(workspace.to_path_buf(), Arc::clone(&cache));
    Some(cache)
}

/// Persist every loaded workspace cache. Failures are reported, not fatal:
/// the next run simply re-hashes.
pub fn save_workspace_caches() {
    let caches: Vec<Arc<DigestCache>> = match registry().lock().expect("digest registry").as_ref() {
        Some(registry) => registry.caches.values().cloned().collect(),
        None => return,
    };
    for cache in caches {
        if let Err(error) = cache.save() {
            eprintln!("ant: could not save the change-detection cache: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ant-digest-cache-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn set_old_mtime(path: &Path) {
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(path)
            .expect("open")
            .set_modified(old)
            .expect("set mtime");
    }

    #[test]
    fn unchanged_files_reuse_and_changed_files_rehash_across_reloads() {
        let dir = temp_dir("reuse");
        let file = dir.join("a.txt");
        fs::write(&file, "one").expect("write");
        set_old_mtime(&file);
        let store = dir.join("cache.txt");

        let cache = DigestCache::load(store.clone());
        let meta = fs::symlink_metadata(&file).expect("meta");
        let first = cache.digest(&file, &meta, false).expect("digest");
        assert_eq!(first, digest_file(&file).expect("direct"));
        cache.save().expect("save");

        // A reloaded cache reuses the entry: prove it by planting a wrong
        // digest for the unchanged stamp and seeing it returned.
        let text = fs::read_to_string(&store).expect("read");
        let planted = text.replace(&first.to_string(), &crate::digest_bytes(b"x").to_string());
        fs::write(&store, planted).expect("plant");
        let reloaded = DigestCache::load(store.clone());
        let meta = fs::symlink_metadata(&file).expect("meta");
        assert_eq!(
            reloaded.digest(&file, &meta, false).expect("digest"),
            crate::digest_bytes(b"x"),
            "an unchanged stamp is trusted"
        );

        // Changing content (and so size/mtime) re-hashes.
        fs::write(&file, "two!").expect("rewrite");
        let meta = fs::symlink_metadata(&file).expect("meta");
        assert_eq!(
            reloaded.digest(&file, &meta, false).expect("digest"),
            digest_file(&file).expect("direct")
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn racily_recent_files_are_rehashed() {
        let dir = temp_dir("racy");
        let file = dir.join("a.txt");
        fs::write(&file, "one").expect("write");
        let store = dir.join("cache.txt");
        let cache = DigestCache::load(store.clone());
        let meta = fs::symlink_metadata(&file).expect("meta");
        cache.digest(&file, &meta, false).expect("digest");
        cache.save().expect("save");
        // Plant a wrong digest: a just-written file must not trust it.
        let text = fs::read_to_string(&store).expect("read");
        let real = digest_file(&file).expect("direct");
        fs::write(
            &store,
            text.replace(&real.to_string(), &crate::digest_bytes(b"x").to_string()),
        )
        .expect("plant");
        let reloaded = DigestCache::load(store);
        assert_eq!(reloaded.digest(&file, &meta, false).expect("digest"), real);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_cache_files_are_ignored() {
        let dir = temp_dir("corrupt");
        let store = dir.join("cache.txt");
        fs::write(&store, "garbage\nmore garbage").expect("write");
        let cache = DigestCache::load(store);
        let file = dir.join("a.txt");
        fs::write(&file, "one").expect("write");
        let meta = fs::symlink_metadata(&file).expect("meta");
        assert_eq!(
            cache.digest(&file, &meta, true).expect("digest"),
            digest_file_with_metadata(&file).expect("direct")
        );
        let _ = fs::remove_dir_all(dir);
    }
}
