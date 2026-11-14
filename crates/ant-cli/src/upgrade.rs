//! `ant upgrade`: replace this `ant` with the latest release (D-043).
//!
//! Releases publish raw binaries named `ant-<target>[.exe]`, a `VERSION` file,
//! and `SHA256SUMS`, reachable at GitHub's stable
//! `<base>/latest/download/<asset>` (or `<base>/download/v<version>/<asset>`)
//! addresses, the same layout the one-line installers use. `ant upgrade`
//! reads `VERSION`, and when it is newer (or `--version` asks for another
//! release) downloads this platform's binary, verifies it against
//! `SHA256SUMS`, and swaps it in next to the running executable. On Windows a
//! running `.exe` cannot be overwritten but can be renamed, so the old one is
//! moved aside (`ant.exe.old`, removed by the next upgrade). Only HTTPS is
//! accepted, except plain HTTP to a loopback mirror (tests, local mirrors).
//! `ANT_DOWNLOAD_BASE` points at a mirror.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_BASE: &str = "https://github.com/vortemain/ant/releases";
const USAGE: &str = "usage: ant upgrade [--check] [--version X.Y.Z]";

/// The release target triple for this build, when releases exist for it.
fn release_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        ("windows", "aarch64") => Some("aarch64-pc-windows-msvc"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        _ => None,
    }
}

/// `1.2.3` → `[1, 2, 3]`; anything after `-` or `+` is ignored.
fn version_parts(text: &str) -> Vec<u64> {
    text.trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()
        .unwrap_or("")
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

fn newer(candidate: &str, current: &str) -> bool {
    let (mut a, mut b) = (version_parts(candidate), version_parts(current));
    let len = a.len().max(b.len());
    a.resize(len, 0);
    b.resize(len, 0);
    a > b
}

/// HTTPS, or HTTP to a loopback host only.
fn check_base(base: &str) -> Result<(), String> {
    if base.starts_with("https://") {
        return Ok(());
    }
    let rest = base
        .strip_prefix("http://")
        .ok_or_else(|| format!("download base {base} must be an https:// address"))?;
    let host = rest.split(['/', ':']).next().unwrap_or("");
    if matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
        Ok(())
    } else {
        Err(format!(
            "download base {base} uses plain HTTP; only https:// (or a loopback mirror) is allowed"
        ))
    }
}

struct Downloader {
    agent: ureq::Agent,
}

impl Downloader {
    fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(300)))
            .user_agent(concat!("ant/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }

    fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        let mut response = self
            .agent
            .get(url)
            .call()
            .map_err(|error| format!("cannot reach {url}: {error}"))?;
        let status = response.status().as_u16();
        if status == 404 {
            return Err(format!("{url} was not found (no such release?)"));
        }
        if !(200..300).contains(&status) {
            return Err(format!("{url} answered HTTP {status}"));
        }
        response
            .body_mut()
            .with_config()
            .limit(512 * 1024 * 1024)
            .read_to_vec()
            .map_err(|error| format!("cannot read {url}: {error}"))
    }
}

/// The expected hex SHA-256 of `asset` in a `sha256sum`-style listing.
fn expected_sha256(listing: &str, asset: &str) -> Option<String> {
    listing.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        (name == asset && hash.len() == 64).then(|| hash.to_ascii_lowercase())
    })
}

/// Plain SHA-256 as `sha256sum` prints it. (Not `build_digest::digest_bytes`,
/// which is domain-separated for Ant's own content addressing.)
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn upgrade_command(args: impl Iterator<Item = OsString>) -> Result<u8, String> {
    let mut check_only = false;
    let mut pinned: Option<String> = None;
    let mut args = args.map(|arg| arg.to_string_lossy().into_owned());
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--check" => check_only = true,
            "--version" => {
                pinned = Some(
                    args.next()
                        .ok_or("--version requires a version such as 0.2.0")?
                        .trim_start_matches('v')
                        .to_owned(),
                )
            }
            other => return Err(format!("unexpected argument '{other}'\n{USAGE}")),
        }
    }
    let current = env!("CARGO_PKG_VERSION");
    let target = release_target().ok_or_else(|| {
        format!(
            "no Ant releases are published for {}-{}; build from source instead",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let base = std::env::var("ANT_DOWNLOAD_BASE")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE.to_owned());
    let base = base.trim_end_matches('/').to_owned();
    check_base(&base)?;
    let folder = match &pinned {
        Some(version) => format!("{base}/download/v{version}"),
        None => format!("{base}/latest/download"),
    };
    let downloader = Downloader::new();

    let available = match &pinned {
        Some(version) => version.clone(),
        None => String::from_utf8_lossy(&downloader.get(&format!("{folder}/VERSION"))?)
            .trim()
            .to_owned(),
    };
    if available.is_empty() || version_parts(&available).iter().all(|part| *part == 0) {
        return Err(format!("{folder}/VERSION does not hold a version"));
    }
    if pinned.is_none() && !newer(&available, current) {
        println!("Ant {current} is up to date (latest release: {available}).");
        return Ok(0);
    }
    if check_only {
        println!(
            "Ant {available} is available (you have {current}). Run `ant upgrade` to install it."
        );
        return Ok(0);
    }

    let exe_suffix = std::env::consts::EXE_SUFFIX;
    let asset = format!("ant-{target}{exe_suffix}");
    println!("Downloading Ant {available} for {target}…");
    let listing =
        String::from_utf8_lossy(&downloader.get(&format!("{folder}/SHA256SUMS"))?).into_owned();
    let expected = expected_sha256(&listing, &asset)
        .ok_or_else(|| format!("SHA256SUMS has no entry for {asset}"))?;
    let binary = downloader.get(&format!("{folder}/{asset}"))?;
    let actual = sha256_hex(&binary);
    if actual != expected {
        return Err(format!(
            "checksum mismatch for {asset} (expected {expected}, got {actual}); nothing was changed"
        ));
    }
    let current_exe = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|error| format!("cannot find the running ant: {error}"))?;
    replace_executable(&current_exe, &binary)?;
    let reported = std::process::Command::new(&current_exe)
        .arg("--version")
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| format!("Ant {available}"));
    println!(
        "Upgraded {current} → {reported} ({}).",
        display(&current_exe)
    );
    Ok(0)
}

fn display(path: &Path) -> String {
    let text = path.display().to_string();
    text.strip_prefix(r"\\?\")
        .map(str::to_owned)
        .unwrap_or(text)
}

/// Write `binary` next to `exe` and swap it in; the old executable is kept
/// as `<exe>.old` until the next upgrade (a running Windows `.exe` cannot be
/// deleted, only renamed).
fn replace_executable(exe: &Path, binary: &[u8]) -> Result<(), String> {
    let with_suffix = |suffix: &str| -> PathBuf {
        let mut name = exe.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    let staged = with_suffix(".new");
    let old = with_suffix(".old");
    let _ = std::fs::remove_file(&old);
    std::fs::write(&staged, binary)
        .map_err(|error| format!("cannot write {}: {error}", display(&staged)))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("cannot mark {} executable: {error}", display(&staged)))?;
    }
    std::fs::rename(exe, &old).map_err(|error| {
        let _ = std::fs::remove_file(&staged);
        format!(
            "cannot move the running ant aside ({error}); is it installed somewhere you cannot write?"
        )
    })?;
    if let Err(error) = std::fs::rename(&staged, exe) {
        // Put the old executable back so `ant` keeps working.
        let _ = std::fs::rename(&old, exe);
        let _ = std::fs::remove_file(&staged);
        return Err(format!("cannot install the new ant: {error}"));
    }
    #[cfg(unix)]
    let _ = std::fs::remove_file(&old);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_standard_tool() {
        // FIPS 180-2 test vector, as `sha256sum` prints it.
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(newer("0.10.0", "0.9.9"));
        assert!(newer("1.0.0", "0.99.0"));
        assert!(newer("v0.2.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0-rc.1", "0.1.0"));
        assert!(newer("0.1.1", "0.1"));
    }

    #[test]
    fn checksums_parse_and_bases_must_be_safe() {
        let hash = "ab".repeat(32);
        let listing = format!(
            "{hash}  ant-x86_64-pc-windows-msvc.exe\n{}  VERSION\n",
            "cd".repeat(32)
        );
        assert_eq!(
            expected_sha256(&listing, "ant-x86_64-pc-windows-msvc.exe"),
            Some(hash.clone())
        );
        assert_eq!(
            expected_sha256(&format!("{hash} *ant-x"), "ant-x"),
            Some(hash)
        );
        assert_eq!(expected_sha256(&listing, "ant-other"), None);
        assert!(check_base("https://github.com/vortemain/ant/releases").is_ok());
        assert!(check_base("http://127.0.0.1:8765").is_ok());
        assert!(check_base("http://localhost:8765/x").is_ok());
        assert!(check_base("http://example.com/releases").is_err());
        assert!(check_base("ftp://example.com").is_err());
        assert!(release_target().is_some(), "CI platforms have releases");
    }

    #[test]
    fn replacing_keeps_a_working_executable() {
        let dir = std::env::temp_dir().join(format!("ant-upgrade-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let exe = dir.join("ant-test-binary");
        std::fs::write(&exe, b"old").expect("old");
        replace_executable(&exe, b"new").expect("replace");
        assert_eq!(std::fs::read(&exe).expect("read"), b"new");
        assert!(!dir.join("ant-test-binary.new").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
