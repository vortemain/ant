//! Deterministic per-workspace capability SIDs.
//!
//! Audit contract: builds a syntactically valid S-1-5-21 SID string from a
//! SHA-256 digest; no FFI here. The SID identifies "may write inside this
//! workspace" for one canonical workspace path: the same workspace always
//! derives the same SID, so the ACL grant added by `acl::grant_workspace_write`
//! is idempotent and the DACL stays bounded to one ACE per workspace.

use sha2::Digest as _;
use sha2::Sha256;
use std::path::Path;

/// Derive the capability SID string for a canonical workspace path.
/// Shape: `S-1-5-21-{a}-{b}-{c}-{d}` with four 32-bit components derived from
/// `SHA-256("ant-sandbox-cap-v1\0" || canonical workspace path)`.
pub(crate) fn capability_sid_for_workspace(workspace: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"ant-sandbox-cap-v1\0");
    hasher.update(workspace.as_os_str().to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let components: [u32; 4] = std::array::from_fn(|index| {
        let at = index * 4;
        u32::from_be_bytes([digest[at], digest[at + 1], digest[at + 2], digest[at + 3]])
    });
    format!(
        "S-1-5-21-{}-{}-{}-{}",
        components[0], components[1], components[2], components[3]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sid_is_deterministic_and_distinct_per_workspace() {
        let left = capability_sid_for_workspace(Path::new("C:\\work\\one"));
        let right = capability_sid_for_workspace(Path::new("C:\\work\\one"));
        assert_eq!(left, right, "same workspace must derive the same SID");
        assert_ne!(
            capability_sid_for_workspace(Path::new("C:\\work\\two")),
            left,
            "different workspaces must derive different SIDs"
        );
        assert!(left.starts_with("S-1-5-21-"), "got {left}");
        // Four subauthorities after S-1-5-21.
        assert_eq!(left.split('-').count(), 8, "got {left}");
        for part in left.split('-').skip(3) {
            // Each subauthority is a decimal u32; the SID encoding stores
            // each as a 32-bit subauthority so the parse above is exact.
            let _: u32 = part.parse().expect("decimal u32 component");
        }
    }
}
