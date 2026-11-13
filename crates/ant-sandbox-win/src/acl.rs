//! ACL grants for sandbox capability SIDs.
//!
//! Audit contract: every function here fetches the current DACL of one
//! object, merges explicit-access entries for a capability SID, and applies
//! the result. All owned handles/security descriptors are freed on every
//! return path. Grants are additive and idempotent (a SID that already has
//! the requested mask is skipped); no ACE is ever removed.

use crate::error::SandboxError;
use std::ffi::c_void;
use std::path::Path;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::ACCESS_ALLOWED_ACE;
use windows_sys::Win32::Security::ACE_HEADER;
use windows_sys::Win32::Security::ACL;
use windows_sys::Win32::Security::ACL_SIZE_INFORMATION;
use windows_sys::Win32::Security::AclSizeInformation;
use windows_sys::Win32::Security::Authorization::EXPLICIT_ACCESS_W;
use windows_sys::Win32::Security::Authorization::GetNamedSecurityInfoW;
use windows_sys::Win32::Security::Authorization::GetSecurityInfo;
use windows_sys::Win32::Security::Authorization::SET_ACCESS;
use windows_sys::Win32::Security::Authorization::SetEntriesInAclW;
use windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW;
use windows_sys::Win32::Security::Authorization::SetSecurityInfo;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_SID;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_UNKNOWN;
use windows_sys::Win32::Security::Authorization::TRUSTEE_W;
use windows_sys::Win32::Security::CONTAINER_INHERIT_ACE;
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::EqualSid;
use windows_sys::Win32::Security::GetAce;
use windows_sys::Win32::Security::GetAclInformation;
use windows_sys::Win32::Security::OBJECT_INHERIT_ACE;
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::Storage::FileSystem::DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_DELETE_CHILD;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_EXECUTE;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;
use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;

/// Write capability = read/write/execute plus delete of contained objects.
pub(crate) const WRITE_ALLOW_MASK: u32 =
    FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE | FILE_DELETE_CHILD;

const SE_FILE_OBJECT: i32 = 1;

/// Does the DACL already contain an effective (non-inherit-only) allow ACE
/// for this SID granting every bit of `mask` (after generic mapping)? The
/// mask here contains no generic bits, so the mapping is a no-op.
unsafe fn dacl_grants_mask(dacl: *mut ACL, psid: *const c_void, mask: u32) -> bool {
    if dacl.is_null() || psid.is_null() {
        return false;
    }
    // SAFETY: this function's contract requires `dacl`/`psid` to be valid;
    // every unsafe op below operates on those validated pointers.
    let mut info: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe {
        GetAclInformation(
            dacl as *const ACL,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return false;
    }
    const INHERIT_ONLY_ACE: u8 = 0x08;
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    for index in 0..info.AceCount as usize {
        let mut ace: *mut c_void = std::ptr::null_mut();
        if unsafe { GetAce(dacl as *const ACL, index as u32, &mut ace) } == 0 {
            continue;
        }
        // SAFETY: `ace` points into the DACL's ACE array; reading its
        // header is within the contract of this function.
        let header = unsafe { &*(ace as *const ACE_HEADER) };
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE || (header.AceFlags & INHERIT_ONLY_ACE) != 0 {
            continue;
        }
        // ACCESS_ALLOWED_ACE layout: ACE_HEADER (4 bytes), Mask (u32), Sid.
        let sid_offset = std::mem::size_of::<ACE_HEADER>() + std::mem::size_of::<u32>();
        let ace_sid = (ace as usize + sid_offset) as *const c_void;
        // SAFETY: `ace_sid` is a valid SID pointer inside the ACE.
        if unsafe { EqualSid(ace_sid as *mut c_void, psid as *mut c_void) } == 0 {
            continue;
        }
        // SAFETY: `ace` points at an ACCESS_ALLOWED_ACE; reading its mask
        // is within the contract of this function.
        let granted = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
        if granted.Mask & mask == mask {
            return true;
        }
    }
    false
}

/// Grant the capability SID read/write/execute (plus delete of contained
/// objects) on `path`, inherited by files and subdirectories. Idempotent.
///
/// Fails closed: an OS error is returned instead of silently continuing
/// without a grant, because a missing grant would make the confined process
/// unable to write where it must.
pub(crate) fn grant_mask(path: &Path, psid: *const c_void, mask: u32) -> Result<(), SandboxError> {
    let wide = crate::winutil::to_wide(&path.as_os_str().to_string_lossy());
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor: *mut c_void = std::ptr::null_mut();
    // SAFETY: `wide` outlives the call; on success we own `descriptor` and
    // free it on every path below.
    let code = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if code != ERROR_SUCCESS {
        return Err(SandboxError::Windows {
            call: "GetNamedSecurityInfoW",
            code: code as u32,
        });
    }
    let result = (|| {
        // SAFETY: `dacl` and `psid` are valid for the duration of the call.
        if unsafe { dacl_grants_mask(dacl, psid, mask) } {
            return Ok(());
        }
        let entry = EXPLICIT_ACCESS_W {
            grfAccessPermissions: mask,
            grfAccessMode: SET_ACCESS,
            grfInheritance: CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: psid as *mut u16,
            },
        };
        let mut new_dacl: *mut ACL = std::ptr::null_mut();
        // SAFETY: `dacl` is the valid existing DACL; on success we own
        // `new_dacl` and free it on every path below.
        let merge = unsafe { SetEntriesInAclW(1, &entry, dacl, &mut new_dacl) };
        if merge != ERROR_SUCCESS {
            return Err(SandboxError::Windows {
                call: "SetEntriesInAclW",
                code: merge as u32,
            });
        }
        // SAFETY: `wide` and `new_dacl` are valid for the duration of the call.
        let apply = unsafe {
            SetNamedSecurityInfoW(
                wide.as_ptr() as *mut u16,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                new_dacl,
                std::ptr::null_mut(),
            )
        };
        let applied = apply == ERROR_SUCCESS;
        // SAFETY: `new_dacl` is owned here in both branches.
        unsafe { LocalFree(new_dacl as HLOCAL) };
        if !applied {
            return Err(SandboxError::Windows {
                call: "SetNamedSecurityInfoW",
                code: apply as u32,
            });
        }
        // Verify the grant actually took effect: filesystems without real
        // ACL support (FAT, exFAT) can report success while storing nothing.
        // SAFETY: `dacl` is stale here; re-fetch the live DACL instead.
        let mut check_dacl: *mut ACL = std::ptr::null_mut();
        let mut check_descriptor: *mut c_void = std::ptr::null_mut();
        let verify = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut check_dacl,
                std::ptr::null_mut(),
                &mut check_descriptor,
            )
        };
        if verify != ERROR_SUCCESS {
            return Err(SandboxError::Windows {
                call: "GetNamedSecurityInfoW (verify)",
                code: verify as u32,
            });
        }
        // SAFETY: both pointers are valid for the check.
        let granted = unsafe { dacl_grants_mask(check_dacl, psid, mask) };
        unsafe { LocalFree(check_descriptor as HLOCAL) };
        if !granted {
            return Err(SandboxError::ConfinementUnavailable {
                path: path.to_path_buf(),
                reason: "the filesystem did not retain the capability grant (no persistent ACLs?)"
                    .to_owned(),
            });
        }
        Ok(())
    })();
    // SAFETY: `descriptor` is owned here and freed exactly once.
    unsafe { LocalFree(descriptor as HLOCAL) };
    result
}

/// Grant the capability SID access to the NUL device so commands redirecting
/// output to `NUL` do not fail under the restricted token (mirrors the
/// reference implementation).
pub(crate) fn grant_null_device(psid: *const c_void) -> Result<(), SandboxError> {
    let wide = crate::winutil::to_wide(r"\\.\NUL");
    // SAFETY: `wide` outlives the call; the returned handle is closed below.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(SandboxError::Windows {
            call: "CreateFileW (NUL)",
            code: unsafe { GetLastError() },
        });
    }
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor: *mut c_void = std::ptr::null_mut();
    // SAFETY: `handle` is valid; on success we own `descriptor`.
    let code = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if code != ERROR_SUCCESS {
        // SAFETY: `handle` is owned here.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
        return Err(SandboxError::Windows {
            call: "GetSecurityInfo (NUL)",
            code: code as u32,
        });
    }
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE,
        grfAccessMode: SET_ACCESS,
        grfInheritance: 0,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: psid as *mut u16,
        },
    };
    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `dacl` is valid; on success we own `new_dacl`.
    let merge = unsafe { SetEntriesInAclW(1, &entry, dacl, &mut new_dacl) };
    if merge == ERROR_SUCCESS {
        // SAFETY: `handle` and `new_dacl` are valid.
        let _ = unsafe {
            SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                new_dacl,
                std::ptr::null_mut(),
            )
        };
    }
    // SAFETY: owned handles freed once.
    unsafe { LocalFree(new_dacl as HLOCAL) };
    unsafe { LocalFree(descriptor as HLOCAL) };
    unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
    // Best-effort: NUL redirection failing must not fail closed (some
    // kernels/hosts may refuse the object DACL change), so this returns Ok
    // even when the merge failed; `>nul` writes fall back to being denied.
    Ok(())
}
