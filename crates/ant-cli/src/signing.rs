//! Encrypted release-signing keystores (P4, D-036).
//!
//! `ant keystore import` stores an Android signing keystore once per project,
//! in a per-user directory keyed by the project's canonical path, together
//! with its passwords sealed by AES-256-GCM under a key derived from a
//! master passphrase with PBKDF2-HMAC-SHA256 (600_000 iterations, OWASP
//! 2023). The keystore file itself stays in its own Java-encrypted form.
//!
//! Passwords never travel through argv: keytool reads them from the child
//! environment (`-storepass env:...`), and a build passes them to Gradle as
//! build-time-only environment variables that exist in the spawned process,
//! never in the plan file, the action key, the event stream, or any cache.
//! The plan references the keystore only by content digest
//! (`ANT_ANDROID_SIGNING=sha256:...`), which makes a different keystore a
//! different action key while revealing nothing.

use build_action::Action;
use serde::{Deserialize, Serialize};
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

// The signing environment contract lives with the Gradle adapter (the one
// place that knows how Android consumes it): the names must match the init
// script byte for byte, so they are imported, not redeclared.
pub use build_adapter::gradle::{
    KEY_ALIAS_ENV, KEY_PASSWORD_ENV, KEYSTORE_ENV, SIGNING_MARKER, STORE_PASSWORD_ENV,
};

/// Master passphrase for the sealed passwords, for scripts and CI.
pub const PASSPHRASE_ENV: &str = "ANT_SIGNING_PASSPHRASE";

/// PBKDF2 iterations (OWASP 2023 recommendation for PBKDF2-HMAC-SHA256).
const KDF_ITERATIONS: u32 = 600_000;

/// The registered entry's metadata file name.
const ENTRY_FILE: &str = "entry.json";
/// The stored keystore copy.
const KEYSTORE_FILE: &str = "keystore.p12";

/// Errors are strings: they are printed once to a human and never parsed.
type Result<T> = std::result::Result<T, String>;

/// Verified metadata for one project's registered keystore. This is the file
/// `entry.json`; it contains no secrets, only the sealed ones.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub schema_version: u32,
    pub alias: String,
    /// Content digest of the stored keystore (`sha256:<hex>`); doubles as the
    /// plan's signing reference.
    pub keystore_digest: String,
    /// When the entry was imported, as unix seconds (UTC).
    pub created_unix: u64,
    pub sealed: Sealed,
}

/// Encrypted passwords, with the parameters needed to re-derive the key.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sealed {
    pub kdf: KdfParams,
    pub aead: AeadParams,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KdfParams {
    pub algorithm: String,
    pub iterations: u32,
    pub salt: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AeadParams {
    pub algorithm: String,
    pub nonce: String,
    pub ciphertext: String,
}

/// The build-time secrets for one signing action, plus the reference the
/// plan must carry for them. Passed from `ant build` to the executor; never
/// serialized anywhere.
pub struct SigningSecrets {
    /// The value the plan's `ANT_ANDROID_SIGNING` marker must have: the
    /// registered keystore's digest. A plan generated against another
    /// keystore refuses these secrets instead of signing with the wrong key.
    pub reference: String,
    /// Environment pairs added to the spawned process only.
    pub values: Vec<(String, String)>,
}

// Never derived: a derived Debug would print the passwords. The manual
// impl names the reference and redacts the pairs, so an accidental `{:?}`
// in a log cannot leak a secret.
impl std::fmt::Debug for SigningSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningSecrets")
            .field("reference", &self.reference)
            .field("values", &format!("{} redacted pair(s)", self.values.len()))
            .finish()
    }
}

impl SigningSecrets {
    /// Whether `action` declares the signing marker with this entry's
    /// reference (so these are the right secrets for it).
    pub fn matches_action(&self, action: &Action) -> bool {
        action
            .environment
            .iter()
            .any(|(key, value)| key == SIGNING_MARKER && value == &self.reference)
    }
}

/// Per-project store directory: `<user cache>/signing/<scope>`, where the
/// scope is the first 16 hex digits of the sha256 of the canonical project
/// path (case-folded, so Windows capitalization differences collide to the
/// same store).
pub fn scope_dir(project_root: &Path) -> Option<PathBuf> {
    let root = project_root.canonicalize().ok()?;
    let scope_input = if cfg!(windows) {
        root.to_string_lossy().to_lowercase()
    } else {
        root.to_string_lossy().into_owned()
    };
    let digest = build_digest::digest_bytes(scope_input.as_bytes()).to_string();
    let short: String = digest
        .strip_prefix("sha256:")
        .unwrap_or(digest.as_str())
        .chars()
        .take(16)
        .collect();
    build_adapter::user_cache_dir().map(|dir| dir.join("signing").join(short))
}

/// The registered entry for this project, if any.
pub fn registered(project_root: &Path) -> Option<Entry> {
    let dir = scope_dir(project_root)?;
    let text = std::fs::read_to_string(dir.join(ENTRY_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The `ant keystore` command: import, list, remove.
pub fn keystore_command(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<u8> {
    let Some(sub) = args.next() else {
        return Err("keystore: expected a subcommand (import, list, remove)".to_owned());
    };
    match sub.to_string_lossy().as_ref() {
        "import" => keystore_import(args),
        "list" => keystore_list(args),
        "remove" => keystore_remove(args),
        other => Err(format!(
            "keystore: unknown subcommand '{other}' (supported: import, list, remove)"
        )),
    }
}

fn keystore_import(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<u8> {
    let mut source: Option<PathBuf> = None;
    let mut alias: Option<String> = None;
    let mut project: Option<PathBuf> = None;
    let mut store_pass_env: Option<String> = None;
    let mut key_pass_env: Option<String> = None;
    let mut passphrase_env: Option<String> = None;
    let argv = |item: &std::ffi::OsStr| item.to_string_lossy().into_owned();
    while let Some(arg) = args.next() {
        let arg = argv(&arg);
        match arg.as_str() {
            "--alias" => alias = Some(next_value(&mut args, "--alias")?),
            "--project" => project = Some(PathBuf::from(next_value(&mut args, "--project")?)),
            "--store-pass-env" => store_pass_env = Some(next_value(&mut args, "--store-pass-env")?),
            "--key-pass-env" => key_pass_env = Some(next_value(&mut args, "--key-pass-env")?),
            "--passphrase-env" => passphrase_env = Some(next_value(&mut args, "--passphrase-env")?),
            other if source.is_none() && !other.starts_with('-') => {
                source = Some(PathBuf::from(other));
            }
            other => return Err(format!("keystore import: unknown argument '{other}'")),
        }
    }
    let source = source.ok_or(
        "keystore import: a keystore file path is required (ant keystore import FILE --alias NAME)",
    )?;
    let alias = alias.ok_or("keystore import: --alias is required")?;
    if alias.trim().is_empty() {
        return Err("keystore import: --alias must not be empty".to_owned());
    }
    let root = project_root_or_current(project.as_deref())?;
    let bytes = std::fs::read(&source)
        .map_err(|error| format!("keystore import: cannot read {}: {error}", source.display()))?;
    if bytes.is_empty() {
        return Err("keystore import: the keystore file is empty".to_owned());
    }

    // Passwords: explicit environment (scripts, CI) or a hidden prompt.
    // Never argv, never echoed.
    let store_password = password_from(&store_pass_env, "Keystore password")?;
    let key_password = password_from(&key_pass_env, "Key password (Enter = same as keystore)")?;
    let key_password = if key_password.is_empty() {
        store_password.clone()
    } else {
        key_password
    };
    if store_password.is_empty() {
        return Err("keystore import: the keystore password must not be empty".to_owned());
    }

    // Fail closed: the passwords are proven correct by keytool before
    // anything is stored. The password travels to keytool through the child
    // environment (`env:` form), never through argv where other processes
    // could read it.
    validate_with_keytool(&source, &store_password, &alias)
        .map_err(|error| format!("keystore import: {error}"))?;

    let passphrase = passphrase_for_import(&passphrase_env)?;
    let digest = build_digest::digest_bytes(&bytes).to_string();
    let sealed = seal(&store_password, &key_password, &passphrase)
        .map_err(|error| format!("keystore import: {error}"))?;
    let entry = Entry {
        schema_version: 1,
        alias: alias.trim().to_owned(),
        keystore_digest: digest,
        created_unix: now_unix(),
        sealed,
    };

    let dir = scope_dir(&root).ok_or(
        "keystore import: cannot determine the per-user cache directory (set ANT_CACHE_HOME)",
    )?;
    write_entry(&dir, &bytes, &entry)?;
    println!(
        "keystore: registered '{}' for {} ({} bytes, {})",
        entry.alias,
        root.display(),
        bytes.len(),
        &entry.keystore_digest[..20.min(entry.keystore_digest.len())]
    );
    println!("keystore: release builds of this project sign with it (ant build --release).");
    Ok(0)
}

fn keystore_list(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<u8> {
    let mut project: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        let arg = arg.to_string_lossy().into_owned();
        if arg == "--project" {
            project = Some(PathBuf::from(next_value(&mut args, "--project")?));
        } else {
            return Err(format!("keystore list: unknown argument '{arg}'"));
        }
    }
    let root = project_root_or_current(project.as_deref())?;
    let Some(entry) = registered(&root) else {
        println!("keystore: no keystore registered for {}", root.display());
        return Ok(1);
    };
    println!("project:  {}", root.display());
    println!("alias:    {}", entry.alias);
    println!("keystore: {}", entry.keystore_digest);
    println!("imported: {} (unix seconds)", entry.created_unix);
    let present = scope_dir(&root)
        .map(|dir| dir.join(KEYSTORE_FILE).is_file())
        .unwrap_or(false);
    println!("stored:   {}", if present { "yes" } else { "MISSING" });
    Ok(0)
}

fn keystore_remove(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<u8> {
    let mut project: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        let arg = arg.to_string_lossy().into_owned();
        if arg == "--project" {
            project = Some(PathBuf::from(next_value(&mut args, "--project")?));
        } else {
            return Err(format!("keystore remove: unknown argument '{arg}'"));
        }
    }
    let root = project_root_or_current(project.as_deref())?;
    let Some(dir) = scope_dir(&root) else {
        return Err("keystore remove: cannot determine the per-user cache directory".to_owned());
    };
    if !dir.join(ENTRY_FILE).is_file() {
        println!("keystore: no keystore registered for {}", root.display());
        return Ok(1);
    }
    std::fs::remove_dir_all(&dir).map_err(|error| format!("keystore remove: {error}"))?;
    println!(
        "keystore: removed the registered keystore for {} (re-import anytime)",
        root.display()
    );
    Ok(0)
}

/// Stage the Gradle init script a signed build passes with `-I` (D-036):
/// `.ant/` is never a build input and the script holds no secrets. Called by
/// the local front door only, when it turns signing on for a release build.
pub fn ensure_init_script(project_root: &Path) -> Result<()> {
    let root = project_root.canonicalize().map_err(|error| {
        format!(
            "signing: cannot resolve {}: {error}",
            project_root.display()
        )
    })?;
    build_adapter::gradle::write_signing_init_script(&root)
        .map_err(|error| format!("signing: cannot write the init script: {error}"))
}

/// Resolve the build-time secrets for a registered entry: verify the stored
/// keystore still matches the imported digest, find the master passphrase
/// (environment first, then a hidden terminal prompt), and unlock the sealed
/// passwords. Split so each half is testable without touching the process
/// environment (the workspace forbids unsafe `set_var`/`remove_var`).
pub fn unlock(project_root: &Path, entry: &Entry) -> Result<SigningSecrets> {
    let keystore = verify_stored_keystore(project_root, entry)?;
    let passphrase = read_passphrase()?;
    unlock_with_passphrase(&keystore, entry, &passphrase)
}

/// Digest-check the stored keystore copy, returning its path. The keystore
/// itself stays Java-encrypted; a tampered copy must never reach Gradle.
fn verify_stored_keystore(project_root: &Path, entry: &Entry) -> Result<PathBuf> {
    let dir =
        scope_dir(project_root).ok_or("signing: cannot determine the per-user cache directory")?;
    let keystore = dir.join(KEYSTORE_FILE);
    let bytes = std::fs::read(&keystore)
        .map_err(|error| format!("signing: cannot read the registered keystore: {error}"))?;
    let digest = build_digest::digest_bytes(&bytes).to_string();
    if digest != entry.keystore_digest {
        return Err(format!(
            "signing: the registered keystore changed since import (stored {digest}, imported {}); re-run ant keystore import",
            entry.keystore_digest
        ));
    }
    Ok(keystore)
}

/// The master passphrase: explicit environment (scripts, CI), a hidden
/// prompt on a terminal, or a message naming the missing credential.
fn read_passphrase() -> Result<String> {
    if let Some(value) = std::env::var(PASSPHRASE_ENV)
        .ok()
        .filter(|value| !value.is_empty())
    {
        return Ok(value);
    }
    if std::io::stdin().is_terminal() {
        return rpassword::prompt_password("Signing passphrase (ant keystore): ")
            .map_err(|error| format!("signing: cannot read the passphrase: {error}"));
    }
    Err(format!(
        "signing: no passphrase available; set {PASSPHRASE_ENV} or run from a terminal"
    ))
}

/// Unlock with an already-resolved passphrase and shape the environment
/// values the spawned build process receives (build-time only).
fn unlock_with_passphrase(
    keystore: &Path,
    entry: &Entry,
    passphrase: &str,
) -> Result<SigningSecrets> {
    let passwords = open(&entry.sealed, passphrase)?;
    let keystore_display = keystore.to_string_lossy().into_owned();
    Ok(SigningSecrets {
        reference: entry.keystore_digest.clone(),
        values: vec![
            (KEYSTORE_ENV.to_owned(), keystore_display),
            (STORE_PASSWORD_ENV.to_owned(), passwords.store_password),
            (KEY_ALIAS_ENV.to_owned(), entry.alias.clone()),
            (KEY_PASSWORD_ENV.to_owned(), passwords.key_password),
        ],
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Passwords {
    store_password: String,
    key_password: String,
}

/// PBKDF2-HMAC-SHA256 → AES-256-GCM seal of the two passwords.
fn seal(store_password: &str, key_password: &str, passphrase: &str) -> Result<Sealed> {
    use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce, aead::Aead};
    let mut salt = [0u8; 16];
    fill_random(&mut salt)?;
    let mut nonce = [0u8; 12];
    fill_random(&mut nonce)?;
    let key = derive_key(passphrase.as_bytes(), &salt)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let plain = serde_json::to_vec(&Passwords {
        store_password: store_password.to_owned(),
        key_password: key_password.to_owned(),
    })
    .map_err(|error| format!("sealing: {error}"))?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), plain.as_slice())
        .map_err(|_| "sealing: AES-256-GCM encryption failed".to_owned())?;
    Ok(Sealed {
        kdf: KdfParams {
            algorithm: "pbkdf2-hmac-sha256".to_owned(),
            iterations: KDF_ITERATIONS,
            salt: hex(&salt),
        },
        aead: AeadParams {
            algorithm: "aes-256-gcm".to_owned(),
            nonce: hex(&nonce),
            ciphertext: hex(&ciphertext),
        },
    })
}

/// Open a sealed value; a wrong passphrase or tampered ciphertext fails
/// through the GCM tag, not through a guess.
fn open(sealed: &Sealed, passphrase: &str) -> Result<Passwords> {
    use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce, aead::Aead};
    if sealed.kdf.algorithm != "pbkdf2-hmac-sha256" {
        return Err(format!(
            "signing: unsupported KDF '{}' (re-import the keystore)",
            sealed.kdf.algorithm
        ));
    }
    if sealed.aead.algorithm != "aes-256-gcm" {
        return Err(format!(
            "signing: unsupported cipher '{}' (re-import the keystore)",
            sealed.aead.algorithm
        ));
    }
    let salt = unhex(&sealed.kdf.salt)?;
    let nonce = unhex(&sealed.aead.nonce)?;
    let ciphertext = unhex(&sealed.aead.ciphertext)?;
    if nonce.len() != 12 {
        return Err("signing: malformed nonce (re-import the keystore)".to_owned());
    }
    let key = derive_key(passphrase.as_bytes(), &salt)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let plain = cipher
        .decrypt(Nonce::from_slice(&nonce), ciphertext.as_slice())
        .map_err(|_| "signing: wrong passphrase or the sealed entry was modified".to_owned())?;
    serde_json::from_slice(&plain).map_err(|error| format!("signing: {error}"))
}

fn derive_key(passphrase: &[u8], salt: &[u8]) -> Result<[u8; 32]> {
    if salt.len() != 16 {
        return Err("signing: malformed salt (re-import the keystore)".to_owned());
    }
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(passphrase, salt, KDF_ITERATIONS, &mut key);
    Ok(key)
}

fn fill_random(buffer: &mut [u8]) -> Result<()> {
    getrandom::fill(buffer).map_err(|error| format!("cannot read OS randomness: {error}"))
}

fn write_entry(dir: &Path, keystore_bytes: &[u8], entry: &Entry) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|error| format!("keystore: {error}"))?;
    let entry_bytes =
        serde_json::to_vec_pretty(entry).map_err(|error| format!("keystore: {error}"))?;
    write_private(&dir.join(KEYSTORE_FILE), keystore_bytes)?;
    write_private(&dir.join(ENTRY_FILE), &entry_bytes)?;
    Ok(())
}

/// Write a file atomically (temp + rename) with owner-only permissions on
/// Unix; on Windows the directory already lives under the user's own
/// `%LOCALAPPDATA%`, protected by the account.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temp, bytes)
        .and_then(|()| restrict(&temp))
        .and_then(|()| std::fs::rename(&temp, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temp);
        })
        .map_err(|error| format!("keystore: write {}: {error}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn restrict(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Prove the passwords (and the alias) with the JDK's keytool before storing
/// anything. The password reaches keytool through the child environment,
/// never through argv.
fn validate_with_keytool(keystore: &Path, store_password: &str, alias: &str) -> Result<()> {
    let keytool = which_keytool()
        .ok_or("keytool (JDK) not found on PATH; a JDK is required to verify the keystore")?;
    let output = std::process::Command::new(&keytool)
        .arg("-list")
        .arg("-keystore")
        .arg(keystore)
        .arg("-storepass")
        .arg("env:ANT_KEYTOOL_STORE_PASS")
        .env("ANT_KEYTOOL_STORE_PASS", store_password)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("cannot run keytool: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!(
            "keytool rejected the keystore or password: {}",
            stderr.lines().last().unwrap_or("no output").trim()
        ));
    }
    if !stdout.contains(alias) {
        return Err(format!(
            "alias '{alias}' was not found in the keystore (keytool listed: {})",
            stdout
                .lines()
                .filter(|line| line.contains(',') && !line.contains("entries"))
                .map(|line| line.split(',').next().unwrap_or("").trim())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
}

fn which_keytool() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "keytool.exe"
    } else {
        "keytool"
    };
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn next_value(args: &mut impl Iterator<Item = std::ffi::OsString>, flag: &str) -> Result<String> {
    args.next()
        .map(|value| value.to_string_lossy().into_owned())
        .ok_or_else(|| format!("keystore: {flag} requires a value"))
}

fn project_root_or_current(project: Option<&Path>) -> Result<PathBuf> {
    let path = project
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    path.canonicalize()
        .map_err(|error| format!("cannot access project {}: {error}", path.display()))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// A password from an explicit environment variable name (for scripts), or a
/// hidden prompt when the terminal supports it. Refuses to read a piped
/// stdin: silent password material on a pipe is almost always an accident.
fn password_from(env_name: &Option<String>, label: &str) -> Result<String> {
    if let Some(name) = env_name {
        if name.trim().is_empty() {
            return Err(
                "keystore import: the --*-env flag needs an environment variable name".to_owned(),
            );
        }
        let value = std::env::var(name).unwrap_or_default();
        if value.is_empty() {
            return Err(format!("keystore import: {name} is empty or unset"));
        }
        return Ok(value);
    }
    if std::io::stdin().is_terminal() {
        return rpassword::prompt_password("{label}: ")
            .map_err(|error| format!("keystore import: cannot read {label}: {error}"));
    }
    Err(format!(
        "keystore import: {label} is required; pass --store-pass-env/--key-pass-env (or run from a terminal)"
    ))
}

/// The master passphrase: from an environment variable (CI), or typed twice
/// at a hidden prompt so typos cannot lock the keystore away.
fn passphrase_for_import(explicit: &Option<String>) -> Result<String> {
    if let Some(name) = explicit {
        let value = std::env::var(name).unwrap_or_default();
        if value.is_empty() {
            return Err(format!("keystore import: {name} is empty or unset"));
        }
        return Ok(value);
    }
    if std::env::var(PASSPHRASE_ENV)
        .ok()
        .is_some_and(|v| !v.is_empty())
    {
        return Ok(std::env::var(PASSPHRASE_ENV).unwrap_or_default());
    }
    if !std::io::stdin().is_terminal() {
        return Err(format!(
            "keystore import: a master passphrase is required; set {PASSPHRASE_ENV} or run from a terminal"
        ));
    }
    let first = rpassword::prompt_password("New signing passphrase: ")
        .map_err(|error| format!("keystore import: {error}"))?;
    if first.is_empty() {
        return Err("keystore import: the passphrase must not be empty".to_owned());
    }
    let second = rpassword::prompt_password("Repeat signing passphrase: ")
        .map_err(|error| format!("keystore import: {error}"))?;
    if first != second {
        return Err("keystore import: the passphrases do not match".to_owned());
    }
    Ok(first)
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn unhex(text: &str) -> Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return Err("signing: malformed hex in the sealed entry".to_owned());
    }
    let mut bytes = Vec::with_capacity(text.len() / 2);
    let chars: Vec<char> = text.chars().collect();
    for pair in chars.chunks(2) {
        let high = pair[0].to_digit(16);
        let low = pair[1].to_digit(16);
        match (high, low) {
            (Some(high), Some(low)) => bytes.push((high * 16 + low) as u8),
            _ => return Err("signing: malformed hex in the sealed entry".to_owned()),
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_dir(tag: &str) -> (PathBuf, PathBuf) {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("ant-signing-{tag}-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&root).expect("mkdir");
        (root.clone(), root)
    }

    fn test_entry(
        _root: &Path,
        store_password: &str,
        key_password: &str,
        passphrase: &str,
    ) -> Entry {
        let sealed = seal(store_password, key_password, passphrase).expect("seal");
        Entry {
            schema_version: 1,
            alias: "app".to_owned(),
            keystore_digest:
                "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
            created_unix: 0,
            sealed,
        }
    }

    #[test]
    fn sealed_entry_round_trips_and_rejects_wrong_passphrase() {
        let sealed = seal("store-pass", "key-pass", "correct horse").expect("seal");
        let opened = open(&sealed, "correct horse").expect("open");
        assert_eq!(opened.store_password, "store-pass");
        assert_eq!(opened.key_password, "key-pass");
        let wrong = open(&sealed, "wrong passphrase");
        assert!(wrong.is_err(), "a wrong passphrase must not open the entry");
    }

    #[test]
    fn sealed_entries_are_unique_per_call_and_tamper_fails() {
        let a = seal("s", "k", "p").expect("seal");
        let b = seal("s", "k", "p").expect("seal");
        assert_ne!(a.kdf.salt, b.kdf.salt, "salts must be unique");
        assert_ne!(a.aead.nonce, b.aead.nonce, "nonces must be unique");
        // Flip one ciphertext byte: the GCM tag must reject it.
        let mut tampered = b.clone();
        let mut bytes = unhex(&tampered.aead.ciphertext).expect("hex");
        bytes[0] ^= 0x01;
        tampered.aead.ciphertext = hex(&bytes);
        assert!(open(&tampered, "p").is_err(), "tampering must fail");
    }

    #[test]
    fn entry_metadata_contains_no_plaintext_secrets() {
        let (root, _) = entry_dir("meta");
        let bytes = b"fake-keystore-bytes".to_vec();
        let entry = test_entry(&root, "s3cret-store", "s3cret-key", "pass-phrase");
        let dir = scope_dir(&root).expect("scope");
        write_entry(&dir, &bytes, &entry).expect("write");
        let json = std::fs::read_to_string(dir.join(ENTRY_FILE)).expect("entry.json");
        assert!(
            !json.contains("s3cret-store"),
            "store password leaked: {json}"
        );
        assert!(!json.contains("s3cret-key"), "key password leaked: {json}");
        assert!(!json.contains("pass-phrase"), "passphrase leaked: {json}");
        // And the keystore copy is byte-identical.
        assert_eq!(
            std::fs::read(dir.join(KEYSTORE_FILE)).expect("keystore"),
            bytes
        );
    }

    fn example_action() -> build_action::Action {
        build_action::Action {
            schema_version: 1,
            id: "sign".to_owned(),
            argv: vec!["gradle".to_owned()],
            working_directory: PathBuf::from("."),
            inputs: Vec::new(),
            outputs: Vec::new(),
            environment: Vec::new(),
            cache_policy: build_action::CachePolicy::Never,
            toolchain: None,
            timeout_ms: None,
            resources: None,
        }
    }

    #[test]
    fn wrong_passphrase_message_names_the_cause() {
        let sealed = seal("store-pass", "key-pass", "right").expect("seal");
        let err = open(&sealed, "Rght").unwrap_err();
        assert!(
            err.contains("wrong passphrase"),
            "message should say why: {err}"
        );
    }

    #[test]
    fn unlock_verifies_the_keystore_digest() {
        let (root, _) = entry_dir("unlock");
        let bytes = b"keystore-v1".to_vec();
        let digest = build_digest::digest_bytes(&bytes).to_string();
        let mut entry = test_entry(&root, "s", "k", "p");
        entry.keystore_digest = digest.clone();
        let dir = scope_dir(&root).expect("scope");
        write_entry(&dir, &bytes, &entry).expect("write");

        // The stored copy matches the imported digest, so it is accepted…
        let keystore = verify_stored_keystore(&root, &entry).expect("verify");
        assert!(keystore.ends_with(KEYSTORE_FILE));
        // …and with the passphrase resolved, unlock yields the env pairs.
        let secrets = unlock_with_passphrase(&keystore, &entry, "p").expect("unlock");
        assert_eq!(secrets.reference, digest);
        let names: Vec<&str> = secrets.values.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(
            names,
            vec![
                KEYSTORE_ENV,
                STORE_PASSWORD_ENV,
                KEY_ALIAS_ENV,
                KEY_PASSWORD_ENV
            ]
        );
        assert!(
            secrets
                .values
                .iter()
                .any(|(key, value)| key == STORE_PASSWORD_ENV && value == "s"),
            "store password must reach the environment values"
        );
        assert!(
            secrets
                .values
                .iter()
                .any(|(key, value)| key == KEYSTORE_ENV && value.ends_with(KEYSTORE_FILE)),
            "the keystore path must point at the stored copy"
        );

        // Tamper with the stored keystore: the digest check must refuse.
        std::fs::write(dir.join(KEYSTORE_FILE), b"keystore-v2").expect("tamper");
        let err = verify_stored_keystore(&root, &entry).unwrap_err();
        assert!(err.contains("changed since import"), "{err}");
    }

    #[test]
    fn unlock_reports_a_missing_passphrase_not_a_wrong_one() {
        // Tests run with piped stdin, so no passphrase can be prompted: the
        // message must name the credential, not blame the sealed entry.
        let err = read_passphrase().unwrap_err();
        assert!(err.contains(PASSPHRASE_ENV), "{err}");
    }

    #[test]
    fn secrets_match_only_their_own_reference() {
        let secrets = SigningSecrets {
            reference: "sha256:aaa".to_owned(),
            values: Vec::new(),
        };
        let mut action = example_action();
        action
            .environment
            .push((SIGNING_MARKER.to_owned(), "sha256:aaa".to_owned()));
        assert!(secrets.matches_action(&action));
        action.environment.clear();
        action
            .environment
            .push((SIGNING_MARKER.to_owned(), "sha256:bbb".to_owned()));
        assert!(
            !secrets.matches_action(&action),
            "a plan for another keystore must not use these secrets"
        );
        action.environment.clear();
        assert!(!secrets.matches_action(&action));
    }

    #[test]
    fn projects_scope_to_separate_stores() {
        let (root_a, _) = entry_dir("scope-a");
        let (root_b, _) = entry_dir("scope-b");
        let a = scope_dir(&root_a).expect("a");
        let b = scope_dir(&root_b).expect("b");
        assert_ne!(a, b, "two projects must not share a keystore scope");
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0u8, 1, 0xab, 0xff, 0x10];
        let text = hex(&bytes);
        assert_eq!(text, "0001abff10");
        assert_eq!(unhex(&text).expect("hex"), bytes.to_vec());
        assert!(unhex("0").is_err());
        assert!(unhex("zz").is_err());
    }
}
