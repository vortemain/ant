//! Stable SHA-256 content digests for declared local files and directory trees.
//!
//! Tree hashing is deterministic across traversal order and uses UTF-8 slash-
//! separated relative paths. Symlinks and non-UTF-8 names are rejected until
//! the project defines and tests cross-platform symlink semantics.

use sha2::{Digest as _, Sha256};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ContentDigest(pub [u8; 32]);

impl fmt::Display for ContentDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("sha256:")?;
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl std::str::FromStr for ContentDigest {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let hex_part = value
            .strip_prefix("sha256:")
            .ok_or_else(|| format!("digest {value} lacks sha256: prefix"))?;
        if hex_part.len() != 64 || !hex_part.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("digest {value} is not a SHA-256 hex digest"));
        }
        let mut bytes = [0u8; 32];
        for (index, chunk) in hex_part.as_bytes().chunks(2).enumerate() {
            let high = (chunk[0] as char).to_digit(16).expect("validated hex") as u8;
            let low = (chunk[1] as char).to_digit(16).expect("validated hex") as u8;
            bytes[index] = (high << 4) | low;
        }
        Ok(ContentDigest(bytes))
    }
}

#[derive(Debug)]
pub enum DigestError {
    Io { path: PathBuf, source: io::Error },
    Symlink(PathBuf),
    NonUtf8Path(PathBuf),
    InvalidTreePath(PathBuf),
    UnexpectedType(PathBuf),
}

impl fmt::Display for DigestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::Symlink(path) => write!(f, "symlinks are not supported in declared trees: {}", path.display()),
            Self::NonUtf8Path(path) => write!(f, "path is not valid UTF-8: {}", path.display()),
            Self::InvalidTreePath(path) => write!(f, "invalid path inside declared tree: {}", path.display()),
            Self::UnexpectedType(path) => write!(f, "file or directory has the wrong type: {}", path.display()),
        }
    }
}

impl std::error::Error for DigestError {}

/// Last-modification metadata that can affect action outputs even when file
/// content is unchanged (for example a build step that reads input timestamps
/// to decide whether to regenerate). Included only in the metadata-aware
/// digest mode; see `digest_file_with_metadata`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileMetadata {
    pub size: u64,
    /// Last modification time as nanoseconds since the Unix epoch, when the
    /// platform reports one. `None` is hashed distinctly from any real time.
    pub modified_unix_nanos: Option<u128>,
}

fn file_metadata(metadata: &fs::Metadata) -> FileMetadata {
    FileMetadata {
        size: metadata.len(),
        modified_unix_nanos: metadata.modified().ok().map(|time| {
            time.duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        }),
    }
}

fn hash_metadata(hasher: &mut Sha256, metadata: &FileMetadata) {
    hasher.update(metadata.size.to_be_bytes());
    match metadata.modified_unix_nanos {
        None => hasher.update([0u8]),
        Some(nanos) => {
            hasher.update([1u8]);
            hasher.update(nanos.to_be_bytes());
        }
    }
}

pub fn digest_file(path: &Path) -> Result<ContentDigest, DigestError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| DigestError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(DigestError::Symlink(path.to_path_buf()));
    }
    if !metadata.is_file() {
        return Err(DigestError::UnexpectedType(path.to_path_buf()));
    }
    let mut file = File::open(path).map_err(|source| DigestError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    hasher.update(b"ant-file-v1\0");
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|source| DigestError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(ContentDigest(hasher.finalize().into()))
}

/// Digest a file's content plus its size and last-modification time. Two files
/// with identical content but different timestamps produce different digests.
/// This is the mode an executor must use before it may truthfully declare the
/// `metadata_fingerprinting` capability.
pub fn digest_file_with_metadata(path: &Path) -> Result<ContentDigest, DigestError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| DigestError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(DigestError::Symlink(path.to_path_buf()));
    }
    if !metadata.is_file() {
        return Err(DigestError::UnexpectedType(path.to_path_buf()));
    }
    let fingerprint = file_metadata(&metadata);
    let mut file = File::open(path).map_err(|source| DigestError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    hasher.update(b"ant-file-meta-v1\0");
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|source| DigestError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    hash_metadata(&mut hasher, &fingerprint);
    Ok(ContentDigest(hasher.finalize().into()))
}

/// One entry of a declared directory tree with its relative slash-separated
/// path and, for files, the content digest. Metadata is present only when the
/// tree was enumerated in metadata-aware mode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeEntry {
    pub path: String,
    pub is_dir: bool,
    pub digest: Option<ContentDigest>,
    pub metadata: Option<FileMetadata>,
}

/// Enumerate a directory tree deterministically: sorted by relative path,
/// rejecting symlinks and non-UTF-8 names. Shared by tree digesting and the
/// content store so both observe identical file sets.
pub fn tree_entries(root: &Path) -> Result<Vec<TreeEntry>, DigestError> {
    let metadata = fs::symlink_metadata(root).map_err(|source| DigestError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(DigestError::Symlink(root.to_path_buf()));
    }
    if !metadata.is_dir() {
        return Err(DigestError::UnexpectedType(root.to_path_buf()));
    }
    let mut entries = Vec::new();
    collect_tree(root, root, &mut entries, false)?;
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

/// Same as `tree_entries`, but file entries also carry size and
/// last-modification metadata.
pub fn tree_entries_with_metadata(root: &Path) -> Result<Vec<TreeEntry>, DigestError> {
    let metadata = fs::symlink_metadata(root).map_err(|source| DigestError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(DigestError::Symlink(root.to_path_buf()));
    }
    if !metadata.is_dir() {
        return Err(DigestError::UnexpectedType(root.to_path_buf()));
    }
    let mut entries = Vec::new();
    collect_tree(root, root, &mut entries, true)?;
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

pub fn digest_tree(root: &Path) -> Result<ContentDigest, DigestError> {
    let metadata = fs::symlink_metadata(root).map_err(|source| DigestError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(DigestError::Symlink(root.to_path_buf()));
    }
    if !metadata.is_dir() {
        return Err(DigestError::UnexpectedType(root.to_path_buf()));
    }
    let entries = tree_entries(root)?;
    digest_entries(b"ant-tree-v1", &entries)
}

/// Digest a tree including per-file size and last-modification metadata. Two
/// trees with identical content but different timestamps produce different
/// digests. This is the mode an executor must use before it may truthfully
/// declare the `metadata_fingerprinting` capability.
pub fn digest_tree_with_metadata(root: &Path) -> Result<ContentDigest, DigestError> {
    let metadata = fs::symlink_metadata(root).map_err(|source| DigestError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(DigestError::Symlink(root.to_path_buf()));
    }
    if !metadata.is_dir() {
        return Err(DigestError::UnexpectedType(root.to_path_buf()));
    }
    let entries = tree_entries_with_metadata(root)?;
    digest_entries(b"ant-tree-meta-v1", &entries)
}

fn digest_entries(tag: &[u8], entries: &[TreeEntry]) -> Result<ContentDigest, DigestError> {
    let mut hasher = Sha256::new();
    hasher.update(tag);
    hasher.update(b"\0");
    hasher.update((entries.len() as u64).to_be_bytes());
    for entry in entries {
        let path_bytes = entry.path.as_bytes();
        hasher.update((path_bytes.len() as u64).to_be_bytes());
        hasher.update(path_bytes);
        hasher.update([if entry.is_dir { b'D' } else { b'F' }]);
        if let Some(digest) = entry.digest {
            hasher.update(digest.0);
        }
        if let Some(metadata) = entry.metadata {
            hash_metadata(&mut hasher, &metadata);
        }
    }
    Ok(ContentDigest(hasher.finalize().into()))
}

fn collect_tree(
    root: &Path,
    current: &Path,
    entries: &mut Vec<TreeEntry>,
    with_metadata: bool,
) -> Result<(), DigestError> {
    let mut children = fs::read_dir(current)
        .map_err(|source| DigestError::Io { path: current.to_path_buf(), source })?
        .map(|item| item.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| DigestError::Io { path: current.to_path_buf(), source })?;
    children.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

    for child in children {
        let metadata = fs::symlink_metadata(&child)
            .map_err(|source| DigestError::Io { path: child.clone(), source })?;
        if metadata.file_type().is_symlink() {
            return Err(DigestError::Symlink(child));
        }
        let relative = child.strip_prefix(root)
            .map_err(|_| DigestError::InvalidTreePath(child.clone()))?;
        let normalized = normalize_relative(relative)?;
        if metadata.is_dir() {
            entries.push(TreeEntry { path: normalized, is_dir: true, digest: None, metadata: None });
            collect_tree(root, &child, entries, with_metadata)?;
        } else if metadata.is_file() {
            let digest = if with_metadata { digest_file_with_metadata(&child)? } else { digest_file(&child)? };
            let entry_metadata = if with_metadata { Some(file_metadata(&metadata)) } else { None };
            entries.push(TreeEntry { path: normalized, is_dir: false, digest: Some(digest), metadata: entry_metadata });
        } else {
            return Err(DigestError::UnexpectedType(child));
        }
    }
    Ok(())
}

fn normalize_relative(path: &Path) -> Result<String, DigestError> {
    let mut names = Vec::new();
    for component in path.components() {
        match component {
            // `.` components carry no meaning in a relative path; action
            // validation accepts them (e.g. a `.` working directory).
            Component::Normal(name) => names.push(name.to_str().ok_or_else(|| DigestError::NonUtf8Path(path.to_path_buf()))?),
            Component::CurDir => {}
            _ => return Err(DigestError::NonUtf8Path(path.to_path_buf())),
        }
    }
    Ok(names.join("/"))
}



/// The local process properties inherited by an action and included in its key.
#[derive(Clone, Debug)]
pub struct ExecutionFingerprint {
    pub os: String,
    pub architecture: String,
    pub platform_version: Option<String>,
    pub workspace: PathBuf,
    pub inherited_environment: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    /// Isolation properties the executor truthfully declares about this run.
    /// Cache eligibility requires all of them; the direct local executor
    /// reports none, which keeps its candidates non-cacheable.
    pub capabilities: ExecutorCapabilities,
}

/// Executor-declared isolation capabilities. Callers must not set these
/// unless the executor actually enforces them; there is no CLI override.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExecutorCapabilities {
    /// The action ran isolated from the host filesystem and OS identity.
    pub sandboxed: bool,
    /// Network access was prevented (or fully recorded) for the action.
    pub network_isolation: bool,
    /// Declared input digests include output-affecting filesystem metadata.
    pub metadata_fingerprinting: bool,
}

impl ExecutionFingerprint {
    /// Fingerprint of the direct local executor in content-only digest mode:
    /// no metadata is hashed into input digests, so the metadata capability
    /// must remain undeclared.
    pub fn local(workspace: &Path) -> Self {
        Self {
            os: std::env::consts::OS.to_owned(),
            architecture: std::env::consts::ARCH.to_owned(),
            platform_version: Some(current_platform_version()),
            workspace: workspace.to_path_buf(),
            inherited_environment: local_runtime_environment(),
            capabilities: ExecutorCapabilities::default(),
        }
    }

    /// Fingerprint of an executor run that digests declared inputs with
    /// `digest_file_with_metadata`/`digest_tree_with_metadata`. Declaring the
    /// capability here is truthful only for that digest mode; never use it for
    /// digests produced by `digest_file`/`digest_tree`.
    pub fn local_metadata_fingerprinting(workspace: &Path) -> Self {
        let mut fingerprint = Self::local(workspace);
        fingerprint.capabilities.metadata_fingerprinting = true;
        fingerprint
    }

    /// Fingerprint of an executor run inside the Windows restricted-token
    /// sandbox (decision D-007). The sandbox truthfully declares the
    /// `sandboxed` and `metadata_fingerprinting` capabilities; it does NOT
    /// declare `network_isolation` — restricted tokens cannot prevent network
    /// access for same-user code — so cache reuse stays gated by decision
    /// D-003 even when the sandbox is in use.
    pub fn local_sandboxed(workspace: &Path) -> Self {
        let mut fingerprint = Self::local_metadata_fingerprinting(workspace);
        fingerprint.capabilities.sandboxed = true;
        fingerprint
    }

    /// Fingerprint for the network-isolated sandbox (decision D-013): the
    /// action runs write-confined *as the dedicated sandbox account*, whose
    /// SID is the only scope of block-all firewall rules verified (by
    /// `ant sandbox status`) immediately before the run. Only that executor
    /// may use this constructor; it is the one that truthfully declares
    /// `network_isolation`. Known residual channel, documented in
    /// `docs/sandbox/network-isolation.md`: name resolution through the
    /// system DNS client service runs as another account and is not blocked.
    pub fn local_network_isolated(workspace: &Path) -> Self {
        let mut fingerprint = Self::local_sandboxed(workspace);
        fingerprint.capabilities.network_isolation = true;
        fingerprint
    }
}

/// The actual OS version reported by the running system. Captured so the key
/// material records the platform version truthfully instead of leaving the
/// `platform-version-unspecified` blocker permanent.
fn current_platform_version() -> String {
    os_info::get().version().to_string()
}

/// Runtime environment values Ant deliberately passes through after clearing
/// the child environment. This list is shared with the local executor so its
/// action key reflects the exact inherited values.
///
/// Beyond path/temp basics, the Windows list includes the well-known machine
/// root variables that toolchains consult when locating their own linkers at
/// spawn time: verified empirically, `rustc` on an `x86_64-pc-windows-msvc`
/// host cannot resolve `link.exe` under a cleared environment unless
/// `ProgramFiles(x86)` (used to locate `vswhere.exe`) and `SystemDrive` are
/// present; both are required, either alone is insufficient. `ProgramFiles`
/// and `ProgramW6432` are standard machine roots of the same kind and are
/// included for toolchain detection on non-standard installs. User-specific
/// variables (profile, app data, credentials) stay excluded.
pub fn local_runtime_environment() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    #[cfg(windows)]
    let keys: &[&str] = &[
        "PATH",
        "SYSTEMROOT",
        "WINDIR",
        "TEMP",
        "TMP",
        "PATHEXT",
        "SYSTEMDRIVE",
        "PROGRAMFILES",
        "PROGRAMFILES(X86)",
        "PROGRAMW6432",
    ];
    #[cfg(not(windows))]
    let keys: &[&str] = &["PATH", "HOME", "TMPDIR"];

    keys.iter()
        .filter_map(|key| std::env::var_os(key).map(|value| ((*key).into(), value)))
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CacheabilityReason {
    PolicyNever,
    MissingToolchainIdentity,
    MissingInputDigest(PathBuf),
    UnexpectedInputDigest(PathBuf),
    DuplicateInputDigest(PathBuf),
    PlatformVersionUnspecified,
    NetworkIsolationUnavailable,
    LocalExecutorUnsandboxed,
    FilesystemMetadataNotFingerprintable,
}

/// Stable, human-readable labels for blocker reporting.
impl fmt::Display for CacheabilityReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::PolicyNever => "policy-never".to_owned(),
            Self::MissingToolchainIdentity => "missing-toolchain-identity".to_owned(),
            Self::MissingInputDigest(path) => format!("missing-input-digest={}", path.display()),
            Self::UnexpectedInputDigest(path) => {
                format!("unexpected-input-digest={}", path.display())
            }
            Self::DuplicateInputDigest(path) => format!("duplicate-input-digest={}", path.display()),
            Self::PlatformVersionUnspecified => "platform-version-unspecified".to_owned(),
            Self::NetworkIsolationUnavailable => "network-isolation-unavailable".to_owned(),
            Self::LocalExecutorUnsandboxed => "local-executor-unsandboxed".to_owned(),
            Self::FilesystemMetadataNotFingerprintable => {
                "filesystem-metadata-not-fingerprintable".to_owned()
            }
        };
        f.write_str(&text)
    }
}

#[derive(Clone, Debug)]
pub struct ActionKeyCandidate {
    pub digest: ContentDigest,
    /// A cache may consider this key only when this is true. Ant wires cache
    /// lookup and storage behind exactly this gate (decision D-003): no
    /// executor can report an eligible key yet, so nothing is reused today.
    pub cache_eligible: bool,
    pub reasons: Vec<CacheabilityReason>,
}

/// Compute a stable candidate key without performing cache lookup or reuse.
pub fn action_key_candidate(
    action: &build_action::Action,
    input_digests: &[(PathBuf, ContentDigest)],
    executor: &ExecutionFingerprint,
) -> Result<ActionKeyCandidate, DigestError> {
    use build_action::CachePolicy;
    let mut reasons = Vec::new();
    if action.cache_policy == CachePolicy::Never {
        reasons.push(CacheabilityReason::PolicyNever);
    }
    if action.toolchain.is_none() {
        reasons.push(CacheabilityReason::MissingToolchainIdentity);
    }
    if executor.platform_version.is_none() {
        reasons.push(CacheabilityReason::PlatformVersionUnspecified);
    }
    // The canonical workspace path is hashed into the key below, so outputs
    // that embed the absolute workspace path are covered by the key itself.
    // Hard blockers until the executor truthfully declares the corresponding
    // capability. There is no CLI override for these.
    if !executor.capabilities.network_isolation {
        reasons.push(CacheabilityReason::NetworkIsolationUnavailable);
    }
    if !executor.capabilities.sandboxed {
        reasons.push(CacheabilityReason::LocalExecutorUnsandboxed);
    }
    if !executor.capabilities.metadata_fingerprinting {
        reasons.push(CacheabilityReason::FilesystemMetadataNotFingerprintable);
    }

    let mut declared = Vec::new();
    for input in &action.inputs {
        declared.push(normalize_relative(&input.path)?);
    }
    declared.sort();
    let mut provided = Vec::new();
    for (path, digest) in input_digests {
        provided.push((normalize_relative(path)?, *digest));
    }
    provided.sort_by(|left, right| left.0.cmp(&right.0));
    for pair in provided.windows(2) {
        if pair[0].0 == pair[1].0 {
            reasons.push(CacheabilityReason::DuplicateInputDigest(PathBuf::from(&pair[0].0)));
        }
    }
    let provided_paths: Vec<_> = provided.iter().map(|entry| entry.0.as_str()).collect();
    for path in &declared {
        if !provided_paths.contains(&path.as_str()) {
            reasons.push(CacheabilityReason::MissingInputDigest(PathBuf::from(path)));
        }
    }
    for (path, _) in &provided {
        if !declared.contains(path) {
            reasons.push(CacheabilityReason::UnexpectedInputDigest(PathBuf::from(path)));
        }
    }

    let mut hasher = Sha256::new();
    hasher.update(b"ant-action-key-v1\0");
    hasher.update(action.schema_version.to_be_bytes());
    hash_text(&mut hasher, &action.argv[0]);
    hasher.update((action.argv.len() as u64).to_be_bytes());
    for arg in &action.argv[1..] {
        hash_text(&mut hasher, arg);
    }
    hash_text(&mut hasher, &normalize_relative(&action.working_directory)?);
    match action.timeout_ms {
        Some(timeout) => {
            hasher.update([1]);
            hasher.update(timeout.to_be_bytes());
        }
        None => hasher.update([0]),
    }

    let mut environment: Vec<_> = action.environment.iter().collect();
    environment.sort_by(|left, right| left.0.cmp(&right.0));
    hasher.update((environment.len() as u64).to_be_bytes());
    for (key, value) in environment {
        hash_text(&mut hasher, key);
        hash_text(&mut hasher, value);
    }

    if let Some(toolchain) = &action.toolchain {
        hash_text(&mut hasher, &toolchain.name);
        hash_text(&mut hasher, &toolchain.version);
        hash_text(&mut hasher, &toolchain.executable_digest);
    } else {
        hash_text(&mut hasher, "<missing-toolchain>");
    }
    hash_text(&mut hasher, &executor.os);
    hash_text(&mut hasher, &executor.architecture);
    hash_text(&mut hasher, executor.platform_version.as_deref().unwrap_or("<unknown-platform-version>"));
    hash_bytes(&mut hasher, &os_string_bytes(executor.workspace.as_os_str()));
    let mut inherited: Vec<_> = executor.inherited_environment.iter().collect();
    inherited.sort_by(|left, right| left.0.cmp(&right.0));
    hasher.update((inherited.len() as u64).to_be_bytes());
    for (key, value) in inherited {
        hash_bytes(&mut hasher, &os_string_bytes(key));
        hash_bytes(&mut hasher, &os_string_bytes(value));
    }

    let mut outputs = Vec::new();
    for output in &action.outputs {
        outputs.push((normalize_relative(&output.path)?, output.kind));
    }
    outputs.sort_by(|left, right| left.0.cmp(&right.0));
    hasher.update((outputs.len() as u64).to_be_bytes());
    for (path, kind) in outputs {
        hash_text(&mut hasher, &path);
        hasher.update([match kind {
            build_action::OutputKind::File => b'F',
            build_action::OutputKind::Tree => b'T',
        }]);
    }

    hasher.update((provided.len() as u64).to_be_bytes());
    for (path, digest) in provided {
        hash_text(&mut hasher, &path);
        hasher.update(digest.0);
    }

    let digest = ContentDigest(hasher.finalize().into());
    Ok(ActionKeyCandidate {
        digest,
        cache_eligible: reasons.is_empty(),
        reasons,
    })
}

fn hash_text(hasher: &mut Sha256, value: &str) {
    hash_bytes(hasher, value.as_bytes());
}

fn hash_bytes(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

#[cfg(windows)]
fn os_string_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().flat_map(u16::to_le_bytes).collect()
}

#[cfg(unix)]
fn os_string_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(not(any(windows, unix)))]
fn os_string_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    value.to_string_lossy().as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use build_action::{CachePolicy, Input, InputKind, Output, OutputKind};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let mut unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
                .to_string();
            unique.push_str(std::process::id().to_string().as_str());
            let path = std::env::temp_dir().join(format!("ant-digest-{name}-{unique}"));
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

    #[test]
    fn file_digest_is_deterministic_and_content_sensitive() {
        let dir = TempDir::new("file-deterministic");
        let path = dir.write("data.bin", b"payload");
        let first = digest_file(&path).expect("digest");
        let second = digest_file(&path).expect("digest");
        assert_eq!(first, second);
        fs::write(&path, b"changed").expect("rewrite");
        assert_ne!(digest_file(&path).expect("digest"), first);
    }

    #[test]
    fn file_digest_display_uses_sha256_prefix() {
        let dir = TempDir::new("display");
        let path = dir.write("empty", b"");
        let digest = digest_file(&path).expect("digest");
        let text = digest.to_string();
        assert!(text.starts_with("sha256:"));
        assert_eq!(text.len(), "sha256:".len() + 64);
        assert!(text[7..].bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn metadata_digest_is_deterministic_and_metadata_sensitive() {
        let dir = TempDir::new("meta-file");
        let path = dir.write("data.bin", b"payload");
        let first = digest_file_with_metadata(&path).expect("digest");
        let second = digest_file_with_metadata(&path).expect("digest");
        assert_eq!(first, second, "same content and mtime must give the same digest");
        // A pure mtime change must alter the metadata digest.
        set_file_mtime(&path, std::time::SystemTime::now() + std::time::Duration::from_secs(60));
        assert_ne!(
            digest_file_with_metadata(&path).expect("digest"),
            first,
            "mtime-only change must change the metadata digest"
        );
        // …and must NOT alter the plain content digest.
        assert_ne!(
            digest_file(&path).expect("digest"),
            first,
            "the plain digest must not collide with the metadata digest"
        );
    }

    #[test]
    fn tree_metadata_digest_changes_on_mtime_only_change() {
        let dir = TempDir::new("meta-tree");
        let tree = dir.path().join("tree");
        fs::create_dir_all(tree.join("nested")).expect("tree dirs");
        fs::write(tree.join("a.txt"), b"a").expect("a");
        fs::write(tree.join("nested/b.txt"), b"b").expect("b");
        let plain = digest_tree(&tree).expect("plain");
        let first = digest_tree_with_metadata(&tree).expect("meta");
        let second = digest_tree_with_metadata(&tree).expect("meta again");
        assert_eq!(first, second, "unchanged tree must be stable");
        assert_ne!(first, plain, "metadata digest must differ from content digest");
        set_file_mtime(&tree.join("a.txt"), std::time::SystemTime::now() + std::time::Duration::from_secs(120));
        assert_ne!(
            digest_tree_with_metadata(&tree).expect("meta after touch"),
            first,
            "mtime-only change inside the tree must change the metadata digest"
        );
        assert_eq!(
            digest_tree(&tree).expect("plain after touch"),
            plain,
            "mtime-only change must not change the content digest"
        );
    }

    /// Set a file's modification time through the standard library so tests
    /// never need platform APIs.
    fn set_file_mtime(path: &Path, time: std::time::SystemTime) {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for times");
        file.set_times(std::fs::FileTimes::new().set_modified(time))
            .expect("set times");
    }

    #[test]
    fn local_fingerprint_captures_platform_version() {
        let dir = TempDir::new("fingerprint-version");
        let plain = ExecutionFingerprint::local(dir.path());
        assert!(
            plain.platform_version.as_deref().is_some_and(|version| !version.is_empty()),
            "platform version must be a captured fact, got {plain:?}"
        );
        assert!(!plain.capabilities.metadata_fingerprinting);
        let meta = ExecutionFingerprint::local_metadata_fingerprinting(dir.path());
        assert!(meta.capabilities.metadata_fingerprinting);
        assert!(!meta.capabilities.sandboxed);
        assert!(!meta.capabilities.network_isolation);
        assert_eq!(plain.platform_version, meta.platform_version);
    }

    #[test]
    fn missing_file_reports_io_error() {
        let dir = TempDir::new("missing");
        let error = digest_file(&dir.path().join("absent.txt")).unwrap_err();
        assert!(matches!(error, DigestError::Io { .. }), "got {error:?}");
    }

    #[test]
    fn digesting_a_directory_as_file_fails() {
        let dir = TempDir::new("type-mismatch");
        let error = digest_file(dir.path()).unwrap_err();
        assert!(matches!(error, DigestError::UnexpectedType(_)), "got {error:?}");
    }

    #[test]
    fn tree_digest_is_deterministic_and_traversal_order_independent() {
        let left = TempDir::new("tree-a");
        let right = TempDir::new("tree-b");
        // Identical content, created in opposite orders.
        left.write("z.txt", b"z");
        left.write("nested/deep/f.txt", b"f");
        left.write("a.txt", b"a");
        left.write("nested/g.txt", b"g");
        right.write("a.txt", b"a");
        right.write("nested/g.txt", b"g");
        right.write("nested/deep/f.txt", b"f");
        right.write("z.txt", b"z");
        let first = digest_tree(left.path()).expect("digest");
        let second = digest_tree(right.path()).expect("digest");
        assert_eq!(first, second);
        // Changing one file changes the whole-tree digest.
        right.write("nested/g.txt", b"g-modified");
        assert_ne!(digest_tree(right.path()).expect("digest"), first);
    }

    #[test]
    fn tree_digest_reflects_names_and_structure() {
        let left = TempDir::new("names-a");
        let right = TempDir::new("names-b");
        left.write("dir/file", b"x");
        right.write("file/dir", b"x");
        assert_ne!(
            digest_tree(left.path()).expect("digest"),
            digest_tree(right.path()).expect("digest"),
            "a file named 'dir/file' and a dir 'file' with 'dir' must differ"
        );
    }

    #[test]
    fn empty_tree_digest_is_stable() {
        let left = TempDir::new("empty-a");
        let right = TempDir::new("empty-b");
        assert_eq!(
            digest_tree(left.path()).expect("digest"),
            digest_tree(right.path()).expect("digest")
        );
    }

    #[test]
    fn digesting_file_as_tree_fails() {
        let dir = TempDir::new("file-as-tree");
        let path = dir.write("not-a-dir", b"x");
        let error = digest_tree(&path).unwrap_err();
        assert!(matches!(error, DigestError::UnexpectedType(_)), "got {error:?}");
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn symlink_in_tree_is_rejected() {
        let dir = TempDir::new("symlink");
        dir.write("real.txt", b"x");
        #[cfg(unix)]
        let create = std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("link.txt"));
        #[cfg(windows)]
        let create = std::os::windows::fs::symlink_file(
            dir.path().join("real.txt"),
            dir.path().join("link.txt"),
        );
        // Creating symlinks can require elevated privileges; skip when unsupported.
        if create.is_err() {
            eprintln!("skipping symlink test: cannot create symlink on this host");
            return;
        }
        let error = digest_tree(dir.path()).unwrap_err();
        assert!(matches!(error, DigestError::Symlink(_)), "got {error:?}");
        assert!(matches!(
            digest_file(&dir.path().join("link.txt")).unwrap_err(),
            DigestError::Symlink(_)
        ));
    }

    fn sample_action() -> build_action::Action {
        build_action::Action {
            schema_version: 1,
            id: "key".to_owned(),
            argv: vec!["tool".to_owned(), "run".to_owned()],
            working_directory: PathBuf::from("."),
            inputs: vec![Input { path: PathBuf::from("in.txt"), kind: InputKind::File }],
            outputs: vec![Output { path: PathBuf::from("out.txt"), kind: OutputKind::File }],
            environment: vec![("FLAG".to_owned(), "on".to_owned())],
            cache_policy: CachePolicy::Never,
            toolchain: None,
            timeout_ms: None,
            resources: None,
        }
    }

    fn local_fingerprint(workspace: &Path) -> ExecutionFingerprint {
        ExecutionFingerprint {
            os: std::env::consts::OS.to_owned(),
            architecture: std::env::consts::ARCH.to_owned(),
            platform_version: None,
            workspace: workspace.to_path_buf(),
            inherited_environment: local_runtime_environment(),
            capabilities: ExecutorCapabilities::default(),
        }
    }

    #[test]
    fn action_key_candidate_is_deterministic() {
        let dir = TempDir::new("key");
        let input = dir.write("in.txt", b"content");
        let digest = digest_file(&input).expect("digest");
        let action = sample_action();
        let executor = local_fingerprint(dir.path());
        let first = action_key_candidate(&action, &[(PathBuf::from("in.txt"), digest)], &executor)
            .expect("candidate");
        let second = action_key_candidate(&action, &[(PathBuf::from("in.txt"), digest)], &executor)
            .expect("candidate");
        assert_eq!(first.digest, second.digest);
        // A different input content must change the key.
        let changed = dir.write("in.txt", b"different");
        let different = action_key_candidate(
            &action,
            &[(PathBuf::from("in.txt"), digest_file(&changed).expect("digest"))],
            &executor,
        )
        .expect("candidate");
        assert_ne!(first.digest, different.digest);
    }

    #[test]
    fn action_key_ignores_scheduling_resources() {
        // Resource requests are admission weights that cannot change outputs,
        // so they must not change the action key.
        let dir = TempDir::new("key-resources");
        let executor = local_fingerprint(dir.path());
        let action = sample_action();
        let mut weighted = action.clone();
        weighted.resources = Some(build_action::ResourceRequest { cpu: 8, memory_mb: Some(4096) });
        let plain = action_key_candidate(&action, &[], &executor).expect("candidate");
        let heavy = action_key_candidate(&weighted, &[], &executor).expect("candidate");
        assert_eq!(plain.digest, heavy.digest);
    }

    #[test]
    fn action_key_changes_when_arguments_change() {
        let dir = TempDir::new("key-argv");
        let executor = local_fingerprint(dir.path());
        let action = sample_action();
        let key = action_key_candidate(&action, &[], &executor).expect("candidate");
        let mut other = action.clone();
        other.argv.push("-O2".to_owned());
        let other_key = action_key_candidate(&other, &[], &executor).expect("candidate");
        assert_ne!(key.digest, other_key.digest);
    }

    #[test]
    fn default_policy_reports_conservative_blockers() {
        let dir = TempDir::new("key-blockers");
        let action = sample_action();
        let candidate = action_key_candidate(&action, &[], &local_fingerprint(dir.path()))
            .expect("candidate");
        assert!(!candidate.cache_eligible);
        for expected in [
            CacheabilityReason::PolicyNever,
            CacheabilityReason::MissingToolchainIdentity,
            CacheabilityReason::PlatformVersionUnspecified,
            CacheabilityReason::NetworkIsolationUnavailable,
            CacheabilityReason::LocalExecutorUnsandboxed,
            CacheabilityReason::FilesystemMetadataNotFingerprintable,
        ] {
            assert!(candidate.reasons.contains(&expected), "missing {expected:?} in {:?}", candidate.reasons);
        }
        assert!(candidate.reasons.iter().any(|reason| matches!(
            reason,
            CacheabilityReason::MissingInputDigest(path) if path == &PathBuf::from("in.txt")
        )));
    }

    #[test]
    fn provided_digests_not_declared_are_flagged() {
        let dir = TempDir::new("key-extra");
        let action = sample_action();
        let candidate = action_key_candidate(
            &action,
            &[(PathBuf::from("undeclared.bin"), ContentDigest([0u8; 32]))],
            &local_fingerprint(dir.path()),
        )
        .expect("candidate");
        assert!(candidate.reasons.iter().any(|reason| matches!(
            reason,
            CacheabilityReason::UnexpectedInputDigest(path) if path == &PathBuf::from("undeclared.bin")
        )));
    }

    #[test]
    fn fully_declared_action_with_capable_executor_has_no_blockers() {
        let dir = TempDir::new("key-capable");
        let input = dir.write("in.txt", b"content");
        let digest = digest_file(&input).expect("digest");
        let mut action = sample_action();
        action.cache_policy = CachePolicy::Eligible;
        action.toolchain = Some(build_action::ToolchainIdentity {
            name: "tool".to_owned(),
            version: "1".to_owned(),
            executable_digest: format!("sha256:{}", "a".repeat(64)),
        });
        let executor = ExecutionFingerprint {
            os: "windows".to_owned(),
            architecture: "x86_64".to_owned(),
            platform_version: Some("11".to_owned()),
            workspace: dir.path().to_path_buf(),
            inherited_environment: local_runtime_environment(),
            capabilities: ExecutorCapabilities {
                sandboxed: true,
                network_isolation: true,
                metadata_fingerprinting: true,
            },
        };
        let candidate = action_key_candidate(&action, &[(PathBuf::from("in.txt"), digest)], &executor)
            .expect("candidate");
        assert!(candidate.cache_eligible, "unexpected blockers: {:?}", candidate.reasons);
        assert!(candidate.reasons.is_empty());
    }
}
