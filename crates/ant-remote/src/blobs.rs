//! Content-addressed blob store used by the coordinator. A blob's name is
//! Ant's canonical file digest of its bytes (`build_digest::digest_bytes`,
//! identical to `digest_file`), so remote and local digests always agree;
//! `put` re-hashes and rejects mismatches, so a stored blob is always what
//! its name says.

use build_digest::ContentDigest;
pub use build_digest::digest_bytes;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
pub struct BlobStore {
    root: PathBuf,
    counter: AtomicU64,
}

impl BlobStore {
    pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(root.join("tmp"))?;
        Ok(Self {
            root,
            counter: AtomicU64::new(0),
        })
    }

    fn path_for(&self, digest: &ContentDigest) -> PathBuf {
        let hex = digest.to_string();
        let hex = hex.trim_start_matches("sha256:");
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    pub fn contains(&self, digest: &ContentDigest) -> bool {
        self.path_for(digest).is_file()
    }

    /// Store `bytes` under `expected` after verifying the hash. Idempotent;
    /// written to a temp file and renamed so readers never see partial data.
    pub fn put(&self, expected: &ContentDigest, bytes: &[u8]) -> Result<(), String> {
        let actual = digest_bytes(bytes);
        if &actual != expected {
            return Err(format!("content hashes to {actual}, not {expected}"));
        }
        let target = self.path_for(expected);
        if target.is_file() {
            return Ok(());
        }
        let parent = target.parent().expect("blob path has a parent");
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let temp = self.root.join("tmp").join(format!(
            "{}-{}",
            std::process::id(),
            self.counter.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = std::fs::File::create(&temp).map_err(|error| error.to_string())?;
        file.write_all(bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        match std::fs::rename(&temp, &target) {
            Ok(()) => Ok(()),
            // A concurrent writer stored the same content first.
            Err(_) if target.is_file() => {
                let _ = std::fs::remove_file(&temp);
                Ok(())
            }
            Err(error) => {
                let _ = std::fs::remove_file(&temp);
                Err(error.to_string())
            }
        }
    }

    /// Read a blob and re-verify it (detects on-disk corruption).
    pub fn get(&self, digest: &ContentDigest) -> Result<Option<Vec<u8>>, String> {
        let path = self.path_for(digest);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        if &digest_bytes(&bytes) != digest {
            return Err(format!("stored blob {digest} is corrupt"));
        }
        Ok(Some(bytes))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_verifies_and_get_detects_corruption() {
        let dir = std::env::temp_dir().join(format!("ant-blobs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = BlobStore::open(&dir).expect("open");
        let digest = digest_bytes(b"hello");
        assert!(
            store.put(&digest_bytes(b"other"), b"hello").is_err(),
            "mismatch rejected"
        );
        assert!(!store.contains(&digest));
        store.put(&digest, b"hello").expect("put");
        store.put(&digest, b"hello").expect("idempotent");
        assert_eq!(store.get(&digest).expect("get"), Some(b"hello".to_vec()));
        std::fs::write(store.path_for(&digest), b"tampered").expect("tamper");
        assert!(store.get(&digest).is_err(), "corruption detected on read");
        assert_eq!(store.get(&digest_bytes(b"absent")).expect("get"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
