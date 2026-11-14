//! Current-user secret storage helpers (P4 signing, decision D-033).
//!
//! - [`protect_for_current_user`] / [`unprotect_for_current_user`] wrap
//!   DPAPI (`CryptProtectData` / `CryptUnprotectData`) without an extra
//!   entropy or UI: only the same Windows user on the same machine (or a
//!   roaming profile of that user) can decrypt the blob.
//! - [`fill_random`] fills a buffer from the system-preferred CSPRNG
//!   (`BCryptGenRandom`).
//!
//! Audit contract: every input buffer outlives its call; DPAPI output is
//! copied into an owned `Vec`, zeroed, and released with `LocalFree`; no
//! secret is formatted or logged here.

use crate::error::SandboxError;
use std::ffi::c_void;
use windows_sys::Win32::Foundation::{GetLastError, LocalFree};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom, CRYPT_INTEGER_BLOB,
    CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};

/// Run a DPAPI transform (`protect` or `unprotect`) over `data`.
fn transform(data: &[u8], protect: bool) -> Result<Vec<u8>, SandboxError> {
    let mut input_buffer = data.to_vec();
    let input = CRYPT_INTEGER_BLOB {
        cbData: input_buffer.len() as u32,
        pbData: input_buffer.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: `input` points at `input_buffer`, alive for the whole call and
    // of the stated size; the description, entropy, reserved, and prompt
    // pointers are null as documented; on success `output.pbData` is a
    // LocalAlloc'd buffer of `cbData` bytes that we copy and free below.
    let ok = unsafe {
        if protect {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    // Plaintext copies of our own input are wiped either way.
    input_buffer.iter_mut().for_each(|byte| *byte = 0);
    if ok == 0 {
        return Err(SandboxError::Windows {
            call: if protect {
                "CryptProtectData"
            } else {
                "CryptUnprotectData"
            },
            // SAFETY: reads the calling thread's last-error value.
            code: unsafe { GetLastError() },
        });
    }
    let length = output.cbData as usize;
    let mut result = vec![0u8; length];
    // SAFETY: `output.pbData` holds `length` bytes owned by the OS; we copy
    // them into `result`, zero the OS buffer, and free it exactly once.
    unsafe {
        std::ptr::copy_nonoverlapping(output.pbData, result.as_mut_ptr(), length);
        std::ptr::write_bytes(output.pbData, 0, length);
        LocalFree(output.pbData as *mut c_void);
    }
    Ok(result)
}

/// Encrypt `data` so only the current Windows user can decrypt it.
pub fn protect_for_current_user(data: &[u8]) -> Result<Vec<u8>, SandboxError> {
    transform(data, true)
}

/// Decrypt a blob produced by [`protect_for_current_user`] for this user.
pub fn unprotect_for_current_user(blob: &[u8]) -> Result<Vec<u8>, SandboxError> {
    transform(blob, false)
}

/// Fill `buffer` with cryptographically secure random bytes.
pub fn fill_random(buffer: &mut [u8]) -> Result<(), SandboxError> {
    // SAFETY: a null algorithm handle with BCRYPT_USE_SYSTEM_PREFERRED_RNG is
    // the documented way to use the system RNG; `buffer` is valid for
    // writes of its full length.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        return Err(SandboxError::Windows {
            call: "BCryptGenRandom",
            code: status as u32,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protect_round_trips_and_random_is_not_constant() {
        let secret = b"upload key password";
        let blob = protect_for_current_user(secret).expect("protect");
        assert_ne!(blob.as_slice(), secret.as_slice());
        assert_eq!(
            unprotect_for_current_user(&blob).expect("unprotect"),
            secret
        );
        assert!(unprotect_for_current_user(b"not a dpapi blob").is_err());

        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        fill_random(&mut a).expect("random");
        fill_random(&mut b).expect("random");
        assert_ne!(a, b);
        assert_ne!(a, [0u8; 32]);
    }
}
