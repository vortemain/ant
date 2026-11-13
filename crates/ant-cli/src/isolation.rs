//! Network isolation via the dedicated sandbox account (decision D-013).
//!
//! - `ant sandbox setup|status|teardown` run the checked-in PowerShell scripts
//!   (`scripts/sandbox/*.ps1`), embedded at build time and passed with
//!   `-EncodedCommand` so the executed text is exactly the reviewed text.
//! - [`load_account`] gates every `--network-isolated` run: the status script
//!   must report `ready=true` (account, credential, firewall profiles, policy
//!   override, and both SID-scoped block rules verified) and the DPAPI
//!   credential must decrypt, or the run fails before any action starts.
//! - [`runner_main`] is the hidden `__sandbox-runner` entry point that runs
//!   *as the sandbox account*: it restricts its own token for the workspace
//!   and runs the action, exiting with the action's exit code.

use std::path::{Path, PathBuf};
use std::process::Command;

const SETUP_SCRIPT: &str = include_str!("../../../scripts/sandbox/setup.ps1");
const STATUS_SCRIPT: &str = include_str!("../../../scripts/sandbox/status.ps1");
const TEARDOWN_SCRIPT: &str = include_str!("../../../scripts/sandbox/teardown.ps1");

/// Base64 of the UTF-16LE script, as `powershell -EncodedCommand` expects.
fn encode_command(script: &str, dry_run: bool) -> String {
    // A `param()` block cannot receive arguments through -EncodedCommand, so
    // the dry-run switch is set by wrapping the script in a script block.
    // Progress records would otherwise leak to stderr as CLIXML.
    let wrapped = if dry_run {
        format!("$ProgressPreference = 'SilentlyContinue'\n& {{\n{script}\n}} -DryRun")
    } else {
        format!("$ProgressPreference = 'SilentlyContinue'\n& {{\n{script}\n}}")
    };
    let bytes: Vec<u8> = wrapped.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64(&bytes)
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn run_script(script: &str, dry_run: bool) -> Result<(bool, String), String> {
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-EncodedCommand",
        ])
        .arg(encode_command(script, dry_run))
        .output()
        .map_err(|error| format!("cannot run powershell: {error}"))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        text.push_str(&stderr);
    }
    Ok((output.status.success(), text))
}

/// `ant sandbox <setup|status|teardown> [--dry-run]`.
pub fn sandbox_command(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<u8, String> {
    if !cfg!(windows) {
        return Err("network isolation is implemented for Windows only".to_owned());
    }
    let sub = args
        .next()
        .ok_or("usage: ant sandbox <setup [--dry-run]|status|teardown>")?;
    let rest: Vec<_> = args.collect();
    let dry_run = rest.iter().any(|arg| arg == "--dry-run");
    if rest.iter().any(|arg| arg != "--dry-run") || (dry_run && sub != "setup") {
        return Err("usage: ant sandbox <setup [--dry-run]|status|teardown>".to_owned());
    }
    let script = match sub.to_string_lossy().as_ref() {
        "setup" => SETUP_SCRIPT,
        "status" => STATUS_SCRIPT,
        "teardown" => TEARDOWN_SCRIPT,
        other => return Err(format!("unknown sandbox subcommand '{other}'")),
    };
    let (ok, text) = run_script(script, dry_run)?;
    print!("{text}");
    Ok(if ok { 0 } else { 1 })
}

/// Facts parsed from a passing status check.
#[derive(Debug, PartialEq, Eq)]
pub struct Status {
    pub user: String,
    pub user_sid: String,
}

/// Parse status-script output; `None` unless it reported `ready=true` with a
/// user and SID and no `failed=` line.
pub fn parse_status(text: &str) -> Option<Status> {
    let mut user = None;
    let mut user_sid = None;
    let mut ready = false;
    for line in text.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("user=") {
            user = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("user_sid=") {
            user_sid = Some(value.to_owned());
        } else if line == "ready=true" {
            ready = true;
        } else if line.starts_with("failed=") {
            return None;
        }
    }
    match (ready, user, user_sid) {
        (true, Some(user), Some(user_sid))
            if !user.is_empty() && user_sid.starts_with("S-1-5-") =>
        {
            Some(Status { user, user_sid })
        }
        _ => None,
    }
}

fn credential_path() -> Result<PathBuf, String> {
    let base = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is not set")?;
    Ok(Path::new(&base)
        .join("Ant")
        .join("sandbox")
        .join("offline.cred"))
}

/// Verify the setup and load the sandbox account for this run. Fails closed:
/// any problem means no action runs.
#[cfg(windows)]
pub fn load_account() -> Result<&'static local_executor::IsolatedAccount, String> {
    let (ok, text) = run_script(STATUS_SCRIPT, false)?;
    let status = parse_status(&text).filter(|_| ok).ok_or_else(|| {
        let reasons: Vec<&str> = text.lines().filter(|l| l.starts_with("failed=") || l.starts_with("check=")).collect();
        format!(
            "network isolation is not ready ({}); run 'ant sandbox setup' from an elevated terminal, then 'ant sandbox status'",
            if reasons.is_empty() { "status check failed".to_owned() } else { reasons.join(", ") }
        )
    })?;
    let hex = std::fs::read_to_string(credential_path()?)
        .map_err(|error| format!("cannot read sandbox credential: {error}"))?;
    let password = ant_sandbox_win::unprotect_secure_string_hex(&hex)
        .map_err(|error| format!("cannot decrypt sandbox credential: {error}"))?;
    let runner = std::env::current_exe()
        .map_err(|error| format!("cannot locate ant executable: {error}"))?;
    // Lives for the rest of the process; plan runs borrow it from every worker.
    Ok(Box::leak(Box::new(local_executor::IsolatedAccount {
        user: status.user,
        user_sid: status.user_sid,
        password,
        runner,
    })))
}

#[cfg(not(windows))]
pub fn load_account() -> Result<&'static local_executor::IsolatedAccount, String> {
    Err("network isolation is implemented for Windows only".to_owned())
}

/// Hidden `ant __sandbox-runner --workspace W -- PROGRAM [ARG...]`, started
/// as the sandbox account by the executor. Returns the action's exit code.
pub fn runner_main(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<u8, String> {
    let mut workspace = None;
    let mut argv = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--workspace" {
            workspace = Some(PathBuf::from(
                args.next().ok_or("--workspace requires a path")?,
            ));
        } else if arg == "--" {
            argv.extend(args.by_ref().map(|a| a.to_string_lossy().into_owned()));
        } else {
            return Err(format!(
                "unexpected runner argument '{}'",
                arg.to_string_lossy()
            ));
        }
    }
    let workspace = workspace.ok_or("runner requires --workspace")?;
    if argv.is_empty() {
        return Err("runner requires a program after --".to_owned());
    }
    let sandbox = ant_sandbox_win::Sandbox::from_existing_grant(&workspace)
        .map_err(|error| format!("sandbox runner cannot confine the action: {error}"))?;
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    // The executor already reduced the environment to Ant's allowlist plus
    // the action's declared values; pass it through unchanged.
    let environment: Vec<(String, String)> = std::env::vars().collect();
    let mut child = sandbox
        .spawn(&argv, &cwd, &environment)
        .map_err(|error| format!("sandbox runner cannot start the action: {error}"))?;
    loop {
        if let Some(code) = child.try_wait().map_err(|error| error.to_string())? {
            // Exit codes above 255 (NTSTATUS values) are clamped to 1 rather
            // than truncated into a misleading success-looking value.
            return Ok(u8::try_from(code).unwrap_or(1));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn status_parsing_fails_closed() {
        let good = "user=AntSandboxOffline\nuser_sid=S-1-5-21-1-2-3-1001\nready=true\n";
        assert_eq!(
            parse_status(good),
            Some(Status {
                user: "AntSandboxOffline".to_owned(),
                user_sid: "S-1-5-21-1-2-3-1001".to_owned()
            })
        );
        assert_eq!(
            parse_status("user=a\nuser_sid=S-1-5-21-1\nfailed=rule-missing:x\nready=true"),
            None
        );
        assert_eq!(
            parse_status("user=a\nuser_sid=S-1-5-21-1\nready=false"),
            None
        );
        assert_eq!(parse_status("ready=true"), None, "identity is required");
        assert_eq!(parse_status("user=a\nuser_sid=bogus\nready=true"), None);
    }
}
