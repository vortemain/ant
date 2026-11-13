//! Launching a process as the dedicated sandbox account (decision D-013).
//!
//! A non-elevated process cannot hand another user's logon token to
//! `CreateProcessAsUserW` (that needs `SeAssignPrimaryTokenPrivilege`), so the
//! sandbox account is reached through the Secondary Logon service
//! (`CreateProcessWithLogonW`). The process started that way is Ant's own
//! runner, which then restricts *its own* token (allowed for any caller) for
//! the real action. Firewall rules scoped to the sandbox account's SID apply
//! to the runner and every descendant.
//!
//! Audit contract: the password is held only in a [`Secret`] whose buffer is
//! zeroed on drop and is never formatted, logged, or copied into an owned
//! `String`; DPAPI output is freed with `LocalFree` after copying; the process
//! is always created suspended and resumed only after it joined its
//! kill-on-close job (see `process::finish_spawn`).

use crate::error::SandboxError;
use crate::process::{Child, attach_std_handles, finish_spawn};
use std::ffi::c_void;
use std::path::Path;
use windows_sys::Win32::Foundation::{GetLastError, LocalFree};
use windows_sys::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessWithLogonW, LOGON_WITH_PROFILE,
    PROCESS_INFORMATION, STARTUPINFOW,
};

/// A UTF-16 secret (NUL-terminated) zeroed on drop. Deliberately has no
/// `Debug`/`Display`/`Clone`.
pub struct Secret(Vec<u16>);

impl Secret {
    fn as_ptr(&self) -> *const u16 {
        self.0.as_ptr()
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        for unit in self.0.iter_mut() {
            // SAFETY: writing through a valid `&mut u16`; volatile so the
            // zeroing is not optimized away.
            unsafe { std::ptr::write_volatile(unit, 0) };
        }
    }
}

/// Decrypt a hex-encoded DPAPI blob as written by PowerShell's
/// `ConvertFrom-SecureString` (no key: current-user DPAPI) into the UTF-16
/// secret it protects. Only the same Windows user who protected it can
/// decrypt it.
pub fn unprotect_secure_string_hex(hex: &str) -> Result<Secret, SandboxError> {
    let hex = hex.trim();
    if !hex.len().is_multiple_of(2) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(SandboxError::ConfinementUnavailable {
            path: std::path::PathBuf::from("<credential>"),
            reason: "stored sandbox credential is not a hex DPAPI blob".to_owned(),
        });
    }
    let mut blob: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("validated hex"))
        .collect();
    let input = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: `input` points at a live buffer of the stated size; on success
    // `output.pbData` is a LocalAlloc'd buffer we free below.
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(SandboxError::Windows {
            call: "CryptUnprotectData",
            code: unsafe { GetLastError() },
        });
    }
    let units = output.cbData as usize / 2;
    let mut secret = Vec::with_capacity(units + 1);
    // SAFETY: `output.pbData` holds `cbData` bytes; we read whole UTF-16 units
    // with unaligned reads, then zero and free the OS buffer.
    unsafe {
        let words = output.pbData as *const u16;
        for i in 0..units {
            secret.push(std::ptr::read_unaligned(words.add(i)));
        }
        std::ptr::write_bytes(output.pbData, 0, output.cbData as usize);
        LocalFree(output.pbData as *mut c_void);
    }
    secret.push(0);
    Ok(Secret(secret))
}

/// Start `command_line` as local account `user` (profile loaded), suspended,
/// joined to a kill-on-close job (plus an optional memory cap), then resumed.
/// Standard handles are this process's own. Fails closed on any error.
pub(crate) fn spawn_as_user(
    user: &str,
    password: &Secret,
    command_line: &str,
    cwd: &Path,
    environment: &[(String, String)],
    memory_limit_bytes: Option<u64>,
) -> Result<Child, SandboxError> {
    let user_wide = crate::winutil::to_wide(user);
    let domain_wide = crate::winutil::to_wide(".");
    let mut command_line_wide = crate::winutil::to_wide(command_line);
    let environment_block = crate::winutil::make_env_block(environment);
    let cwd_wide = crate::winutil::to_wide(&cwd.as_os_str().to_string_lossy());

    // SAFETY: plain-old-data struct; zero is a valid initial state.
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    attach_std_handles(&mut startup)?;

    // SAFETY: plain-old-data struct written by the call.
    let mut process_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: every buffer outlives the call; on success both handles in
    // `process_info` are owned by the returned `Child`.
    let ok = unsafe {
        CreateProcessWithLogonW(
            user_wide.as_ptr(),
            domain_wide.as_ptr(),
            password.as_ptr(),
            LOGON_WITH_PROFILE,
            std::ptr::null(),
            command_line_wide.as_mut_ptr(),
            CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
            environment_block.as_ptr() as *const c_void,
            cwd_wide.as_ptr(),
            &startup,
            &mut process_info,
        )
    };
    if ok == 0 {
        return Err(SandboxError::Windows {
            call: "CreateProcessWithLogonW",
            code: unsafe { GetLastError() },
        });
    }
    finish_spawn(process_info, cwd, memory_limit_bytes, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_hex_credentials_without_calling_dpapi() {
        assert!(unprotect_secure_string_hex("not hex").is_err());
        assert!(unprotect_secure_string_hex("abc").is_err(), "odd length");
    }

    #[test]
    fn decrypts_a_powershell_secure_string_for_the_current_user() {
        // Produce a real current-user DPAPI blob the way setup.ps1 does.
        // Same PSModulePath isolation as production (see ant-cli isolation.rs).
        let output = std::process::Command::new("powershell")
            .env_remove("PSModulePath")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "ConvertTo-SecureString 'ant-test-secret' -AsPlainText -Force | ConvertFrom-SecureString",
            ])
            .output()
            .expect("powershell");
        assert!(
            output.status.success(),
            "powershell failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let hex = String::from_utf8(output.stdout).expect("utf8");
        let secret = unprotect_secure_string_hex(&hex).expect("decrypt");
        let expected: Vec<u16> = "ant-test-secret".encode_utf16().chain([0]).collect();
        assert_eq!(secret.0, expected);
    }
}
