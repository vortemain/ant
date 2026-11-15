//! Android release signing (roadmap P4, decision D-033).
//!
//! `ant build --release` in an Android project signs the release APK and
//! AAB with the project's upload key. On first use Ant generates one
//! (`keytool`, RSA 4096, PKCS#12) with a random password; `ant signing
//! import` brings an existing key instead. Keys live in the per-user *data*
//! directory (never the cache directory, which users may wipe, and never
//! the project):
//!
//! ```text
//! <data>/signing/android/<application id>/
//!   key.json            alias, keystore file name, certificate SHA-256
//!   upload.p12          the keystore (itself encrypted with its password)
//!   store-password.*    the password, sealed (see below)
//!   key-password.*      only when an imported key's password differs
//! ```
//!
//! Sealing: on Windows passwords are encrypted with DPAPI for the current
//! user (`.dpapi`). Elsewhere they are written with mode 0600 and are **not
//! encrypted at rest yet** (`.txt`); `ant signing status` says which.
//!
//! Secrets never enter an action, a plan file, a cache key, an event, or a
//! log: the front door hands them to the direct local executor as a
//! [`SecretEnvironment`] (Gradle reads `ORG_GRADLE_PROJECT_*` variables as
//! project properties, and the Android Gradle Plugin's
//! `android.injected.signing.*` properties configure signing). Only the
//! certificate's public SHA-256 fingerprint becomes key material
//! (`ANT_ANDROID_SIGNING_CERT_SHA256`), so a result signed with one key is
//! never reused for another.

use local_executor::SecretEnvironment;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Environment variable carrying the signing certificate fingerprint into
/// the action (public key material, not a secret).
pub const FINGERPRINT_VARIABLE: &str = "ANT_ANDROID_SIGNING_CERT_SHA256";
const KEY_FILE: &str = "key.json";
const KEYTOOL_PASSWORD_VARIABLE: &str = "ANT_KEYTOOL_PASSWORD";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyRecord {
    alias: String,
    keystore: String,
    cert_sha256: String,
    separate_key_password: bool,
}

/// An Android upload key held by Ant for one application id.
pub struct AndroidKey {
    pub dir: PathBuf,
    pub keystore: PathBuf,
    pub alias: String,
    pub cert_sha256: String,
    separate_key_password: bool,
}

/// A secret string wiped on drop and never formatted.
struct Password(String);

impl Drop for Password {
    fn drop(&mut self) {
        let wiped = "0".repeat(self.0.len());
        self.0.replace_range(.., &wiped);
    }
}

/// The per-user data directory (`ANT_DATA_HOME` overrides; tests use it).
pub fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("ANT_DATA_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let home = || {
        std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("Ant"))
    } else if cfg!(target_os = "macos") {
        home().map(|home| home.join("Library").join("Application Support").join("Ant"))
    } else if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(PathBuf::from(xdg).join("ant"))
    } else {
        home().map(|home| home.join(".local").join("share").join("ant"))
    }
}

fn key_dir(app_id: &str) -> Result<PathBuf, String> {
    let valid = !app_id.is_empty()
        && app_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_');
    if !valid {
        return Err(format!("'{app_id}' is not a valid Android application id"));
    }
    let data = data_dir().ok_or("cannot find a per-user data directory for signing keys")?;
    Ok(data.join("signing").join("android").join(app_id))
}

pub(crate) fn sealed_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.dpapi")
    } else {
        format!("{base}.txt")
    }
}

/// How passwords are protected at rest on this platform, in words.
pub fn protection_description() -> &'static str {
    if cfg!(windows) {
        "encrypted with Windows DPAPI for your user account"
    } else {
        "readable only by your user (file mode 0600); not encrypted at rest yet"
    }
}

#[cfg(windows)]
pub(crate) fn seal(secret: &[u8]) -> Result<Vec<u8>, String> {
    ant_sandbox_win::secrets::protect_for_current_user(secret).map_err(|error| error.to_string())
}

#[cfg(windows)]
pub(crate) fn unseal(blob: &[u8]) -> Result<Vec<u8>, String> {
    ant_sandbox_win::secrets::unprotect_for_current_user(blob).map_err(|error| error.to_string())
}

#[cfg(not(windows))]
pub(crate) fn seal(secret: &[u8]) -> Result<Vec<u8>, String> {
    Ok(secret.to_vec())
}

#[cfg(not(windows))]
pub(crate) fn unseal(blob: &[u8]) -> Result<Vec<u8>, String> {
    Ok(blob.to_vec())
}

/// Write a file only the current user can read (mode 0600 on Unix; the
/// per-user profile directory's ACL on Windows). Fails if it exists.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

fn random_password() -> Result<Password, String> {
    random_hex(24).map(Password)
}

/// `count` bytes from the OS CSPRNG, hex-encoded (used for passwords and
/// for `ant install`'s unguessable download URL).
pub fn random_hex(count: usize) -> Result<String, String> {
    let mut bytes = vec![0u8; count];
    #[cfg(windows)]
    ant_sandbox_win::secrets::fill_random(&mut bytes).map_err(|error| error.to_string())?;
    #[cfg(not(windows))]
    {
        use std::io::Read as _;
        fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut bytes))
            .map_err(|error| format!("cannot read /dev/urandom: {error}"))?;
    }
    let text: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    bytes.iter_mut().for_each(|byte| *byte = 0);
    Ok(text)
}

fn keytool() -> OsString {
    let name = format!("keytool{}", std::env::consts::EXE_SUFFIX);
    if let Some(java_home) = std::env::var_os("JAVA_HOME").filter(|v| !v.is_empty()) {
        let candidate = PathBuf::from(java_home).join("bin").join(&name);
        if candidate.is_file() {
            return candidate.into_os_string();
        }
    }
    name.into()
}

/// Run keytool with the password in the child's environment only (never in
/// argv, where other processes could read it).
fn run_keytool(args: &[&str], password: &Password) -> Result<String, String> {
    let output = Command::new(keytool())
        .args(args)
        .env(KEYTOOL_PASSWORD_VARIABLE, &password.0)
        .output()
        .map_err(|error| {
            format!(
                "cannot run keytool (install a JDK and put it on PATH or set JAVA_HOME): {error}"
            )
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        return Err(format!(
            "keytool failed: {}{}",
            stdout.trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(stdout)
}

fn fingerprint(keystore: &Path, alias: &str, password: &Password) -> Result<String, String> {
    let keystore = keystore.to_string_lossy();
    let listing = run_keytool(
        &[
            "-list",
            "-v",
            "-keystore",
            &keystore,
            "-alias",
            alias,
            "-storepass:env",
            KEYTOOL_PASSWORD_VARIABLE,
        ],
        password,
    )?;
    listing
        .lines()
        .find_map(|line| line.trim().strip_prefix("SHA256:"))
        .map(|hex| hex.trim().replace(':', "").to_ascii_lowercase())
        .filter(|hex| hex.len() == 64)
        .ok_or_else(|| "keytool did not report a SHA-256 certificate fingerprint".to_owned())
}

/// The application id of the project's app module(s), read from the build
/// file next to the first declared output (`applicationId`, else
/// `namespace`).
pub fn android_app_id(root: &Path, outputs: &[PathBuf]) -> Result<String, String> {
    for output in outputs {
        let parts: Vec<_> = output.components().collect();
        let Some(build_index) = parts.iter().position(|part| part.as_os_str() == "build") else {
            continue;
        };
        let module: PathBuf = parts[..build_index].iter().collect();
        for name in ["build.gradle.kts", "build.gradle"] {
            let Ok(text) = fs::read_to_string(root.join(&module).join(name)) else {
                continue;
            };
            for key in ["applicationId", "namespace"] {
                if let Some(id) = quoted_value(&text, key) {
                    return Ok(id);
                }
            }
        }
    }
    Err(
        "cannot find the app's applicationId (or namespace) in its build file; Ant names the signing key after it"
            .to_owned(),
    )
}

/// The first quoted string on a line that starts with `key`.
fn quoted_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix(key)?;
        if rest.starts_with(|c: char| c.is_ascii_alphanumeric()) {
            return None;
        }
        let start = rest.find(['"', '\''])?;
        let quote = rest[start..].chars().next()?;
        let value = &rest[start + 1..];
        let end = value.find(quote)?;
        Some(value[..end].to_owned())
    })
}

impl AndroidKey {
    /// The stored key for `app_id`, if any.
    pub fn load(app_id: &str) -> Result<Option<Self>, String> {
        let dir = key_dir(app_id)?;
        let text = match fs::read_to_string(dir.join(KEY_FILE)) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("cannot read signing key record: {error}")),
        };
        let record: KeyRecord = serde_json::from_str(&text)
            .map_err(|error| format!("signing key record {} is damaged: {error}", dir.display()))?;
        Ok(Some(Self {
            keystore: dir.join(&record.keystore),
            dir,
            alias: record.alias,
            cert_sha256: record.cert_sha256,
            separate_key_password: record.separate_key_password,
        }))
    }

    /// The stored key for `app_id`, generating a new upload key on first
    /// use. The flag says whether it was just created.
    pub fn load_or_create(app_id: &str) -> Result<(Self, bool), String> {
        if let Some(key) = Self::load(app_id)? {
            return Ok((key, false));
        }
        let dir = key_dir(app_id)?;
        fs::create_dir_all(&dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        let keystore = dir.join("upload.p12");
        if keystore.exists() {
            return Err(format!(
                "{} exists without a key record; move it away or import it with `ant signing import`",
                keystore.display()
            ));
        }
        let password = random_password()?;
        let distinguished_name = format!("CN={app_id}");
        run_keytool(
            &[
                "-genkeypair",
                "-keystore",
                &keystore.to_string_lossy(),
                "-storetype",
                "PKCS12",
                "-alias",
                "upload",
                "-keyalg",
                "RSA",
                "-keysize",
                "4096",
                "-validity",
                "10000",
                "-dname",
                &distinguished_name,
                "-storepass:env",
                KEYTOOL_PASSWORD_VARIABLE,
                "-keypass:env",
                KEYTOOL_PASSWORD_VARIABLE,
                "-noprompt",
            ],
            &password,
        )?;
        let cert_sha256 = fingerprint(&keystore, "upload", &password)?;
        Self::record(&dir, "upload.p12", "upload", &cert_sha256, &password, None)?;
        Ok((
            Self {
                keystore,
                dir,
                alias: "upload".to_owned(),
                cert_sha256,
                separate_key_password: false,
            },
            true,
        ))
    }

    fn record(
        dir: &Path,
        keystore: &str,
        alias: &str,
        cert_sha256: &str,
        store_password: &Password,
        key_password: Option<&Password>,
    ) -> Result<(), String> {
        write_private(
            &dir.join(sealed_name("store-password")),
            &seal(store_password.0.as_bytes())?,
        )?;
        if let Some(key_password) = key_password {
            write_private(
                &dir.join(sealed_name("key-password")),
                &seal(key_password.0.as_bytes())?,
            )?;
        }
        let record = KeyRecord {
            alias: alias.to_owned(),
            keystore: keystore.to_owned(),
            cert_sha256: cert_sha256.to_owned(),
            separate_key_password: key_password.is_some(),
        };
        let text = serde_json::to_string_pretty(&record).map_err(|error| error.to_string())?;
        write_private(&dir.join(KEY_FILE), text.as_bytes())
    }

    fn password(&self, base: &str) -> Result<Password, String> {
        let path = self.dir.join(sealed_name(base));
        let blob =
            fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let mut bytes = unseal(&blob).map_err(|error| {
            format!(
                "cannot unlock the signing password (stored for another user or machine?): {error}"
            )
        })?;
        let text = String::from_utf8(bytes.clone())
            .map_err(|_| "signing password is not UTF-8".to_owned());
        bytes.iter_mut().for_each(|byte| *byte = 0);
        Ok(Password(text?))
    }

    fn passwords(&self) -> Result<(Password, Password), String> {
        let store = self.password("store-password")?;
        let key = if self.separate_key_password {
            self.password("key-password")?
        } else {
            Password(store.0.clone())
        };
        Ok((store, key))
    }

    /// Spawn-time secrets that make the Android Gradle Plugin sign with
    /// this key.
    pub fn gradle_secrets(&self) -> Result<SecretEnvironment, String> {
        let (store, key) = self.passwords()?;
        let property = |name: &str| format!("ORG_GRADLE_PROJECT_android.injected.signing.{name}");
        Ok(SecretEnvironment::new(vec![
            (
                property("store.file"),
                self.keystore.to_string_lossy().into_owned(),
            ),
            (property("store.password"), store.0.clone()),
            (property("key.alias"), self.alias.clone()),
            (property("key.password"), key.0.clone()),
        ]))
    }

    /// The fingerprint shortened for messages.
    pub fn short_fingerprint(&self) -> String {
        self.cert_sha256.chars().take(16).collect()
    }
}

// ---------------------------------------------------------------------------
// ant signing
// ---------------------------------------------------------------------------

const SIGNING_USAGE: &str = "usage: ant signing status [--project PATH]
       ant signing export --out DIR [--project PATH]
       ant signing import --keystore FILE --alias ALIAS [--project PATH]
         (import reads ANT_SIGNING_STORE_PASSWORD and, if different,
          ANT_SIGNING_KEY_PASSWORD from the environment)";

/// The application id of the Android project at `project` (or the current
/// directory), from its release plan's outputs.
fn project_app_id(project: Option<&str>) -> Result<String, String> {
    let root = match project {
        Some(path) => PathBuf::from(path),
        None => std::env::current_dir().map_err(|error| error.to_string())?,
    }
    .canonicalize()
    .map_err(|error| format!("cannot access the project: {error}"))?;
    let adapter = build_adapter::adapter("gradle").ok_or("the gradle adapter is not available")?;
    match adapter.detect(&root) {
        Ok(detection) if detection.supported => {}
        _ => {
            return Err(
                "signing keys are for Android (Gradle wrapper) projects; this is not one"
                    .to_owned(),
            );
        }
    }
    let mut options = build_adapter::AdapterOptions::new();
    options
        .insert("task", "release")
        .map_err(|error| error.to_string())?;
    let plan = adapter
        .generate(&root, &options)
        .map_err(|error| format!("cannot read the Android project: {error}"))?;
    let outputs = build_adapter::declared_file_outputs(&plan);
    android_app_id(&root, &outputs)
}

pub fn signing_command(args: impl Iterator<Item = OsString>) -> Result<u8, String> {
    let mut args = args.map(|arg| arg.to_string_lossy().into_owned());
    let subcommand = args.next().ok_or(SIGNING_USAGE)?;
    let mut project = None;
    let mut out = None;
    let mut keystore = None;
    let mut alias = None;
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} requires a value"));
        match arg.as_str() {
            "--project" => project = Some(value("--project")?),
            "--out" => out = Some(value("--out")?),
            "--keystore" => keystore = Some(value("--keystore")?),
            "--alias" => alias = Some(value("--alias")?),
            other => return Err(format!("unexpected argument '{other}'\n{SIGNING_USAGE}")),
        }
    }
    let app_id = project_app_id(project.as_deref())?;
    match subcommand.as_str() {
        "status" => {
            match AndroidKey::load(&app_id)? {
                None => println!(
                    "{app_id}: no signing key yet. `ant build --release` creates one, or bring yours with `ant signing import`."
                ),
                Some(key) => {
                    println!("{app_id}: upload key '{}'", key.alias);
                    println!("  certificate SHA-256  {}", key.cert_sha256);
                    println!("  stored in            {}", key.dir.display());
                    println!("  password             {}", protection_description());
                    println!(
                        "  back it up with      ant signing export --out <folder outside the project>"
                    );
                }
            }
            Ok(0)
        }
        "export" => {
            let out = PathBuf::from(out.ok_or("export requires --out DIR")?);
            let key = AndroidKey::load(&app_id)?
                .ok_or(format!("{app_id} has no signing key to export"))?;
            if out.exists()
                && fs::read_dir(&out)
                    .map(|mut d| d.next().is_some())
                    .unwrap_or(true)
            {
                return Err(format!("{} must not exist or be empty", out.display()));
            }
            fs::create_dir_all(&out).map_err(|error| error.to_string())?;
            let keystore_name = key
                .keystore
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "upload.p12".to_owned());
            fs::copy(&key.keystore, out.join(&keystore_name)).map_err(|error| error.to_string())?;
            let (store, key_password) = key.passwords()?;
            let notes = format!(
                "Android upload key for {app_id} (exported by `ant signing export`)\n\
                 keystore: {keystore_name}\nalias: {}\nstore password: {}\nkey password: {}\n\
                 certificate SHA-256: {}\n\n\
                 Keep this folder in a password manager or other safe storage. Never commit it.\n\
                 Restore on another machine with:\n  \
                 ANT_SIGNING_STORE_PASSWORD=... ant signing import --keystore {keystore_name} --alias {}\n",
                key.alias, store.0, key_password.0, key.cert_sha256, key.alias
            );
            write_private(&out.join("signing.txt"), notes.as_bytes())?;
            println!(
                "Exported the {app_id} upload key to {} (keystore + signing.txt, which contains the passwords). Store it somewhere safe and never commit it.",
                out.display()
            );
            Ok(0)
        }
        "import" => {
            let source = PathBuf::from(keystore.ok_or("import requires --keystore FILE")?);
            let alias = alias.ok_or("import requires --alias ALIAS")?;
            if AndroidKey::load(&app_id)?.is_some() {
                return Err(format!(
                    "{app_id} already has a signing key (see `ant signing status`); remove its folder first to replace it"
                ));
            }
            let store = Password(
                std::env::var("ANT_SIGNING_STORE_PASSWORD")
                    .map_err(|_| "set ANT_SIGNING_STORE_PASSWORD to the keystore password")?,
            );
            let key_password = std::env::var("ANT_SIGNING_KEY_PASSWORD")
                .ok()
                .filter(|value| !value.is_empty() && *value != store.0)
                .map(Password);
            let cert_sha256 = fingerprint(&source, &alias, &store)?;
            let dir = key_dir(&app_id)?;
            fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
            let name = "upload.keystore";
            fs::copy(&source, dir.join(name)).map_err(|error| error.to_string())?;
            AndroidKey::record(
                &dir,
                name,
                &alias,
                &cert_sha256,
                &store,
                key_password.as_ref(),
            )?;
            println!(
                "Imported '{alias}' for {app_id} (certificate SHA-256 {cert_sha256}); `ant build --release` now signs with it."
            );
            Ok(0)
        }
        other => Err(format!(
            "unknown signing command '{other}'\n{SIGNING_USAGE}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_application_ids_from_groovy_and_kotlin_build_files() {
        assert_eq!(
            quoted_value(
                "android {\n    applicationId 'dev.ant.hello'\n}",
                "applicationId"
            ),
            Some("dev.ant.hello".to_owned())
        );
        assert_eq!(
            quoted_value("  applicationId = \"com.example.app\"", "applicationId"),
            Some("com.example.app".to_owned())
        );
        assert_eq!(
            quoted_value("applicationIdSuffix = \".dev\"", "applicationId"),
            None
        );
        assert!(key_dir("../escape").is_err());
        assert!(key_dir("").is_err());
    }

    #[test]
    fn secret_environment_debug_shows_names_only() {
        let secrets = SecretEnvironment::new(vec![("NAME".to_owned(), "hunter2".to_owned())]);
        let printed = format!("{secrets:?}");
        assert!(
            printed.contains("NAME") && !printed.contains("hunter2"),
            "{printed}"
        );
    }
}
