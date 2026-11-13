//! Restricted-token creation for the workspace-write sandbox.
//!
//! Design adapted from OpenAI Codex `codex-rs/windows-sandbox-rs/src/token.rs`
//! (Apache-2.0). Semantics: `CreateRestrictedToken` with
//! `DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICT` and restricting SIDs
//! `[capability, logon, everyone]`. The restricted-SID check applies to write
//! access only, so reads and execution stay usable; writes succeed only where
//! a DACL grants one of the restricting SIDs. The default DACL is rebuilt to
//! include the capability, logon, and everyone SIDs so the confined process
//! can create pipes/IPC objects.
//!
//! Audit contract: every unsafe block here operates on token/SID handles it
//! owns or receives as valid pointers; each owned handle is closed or freed
//! on every return path.

use crate::error::SandboxError;
use std::ffi::c_void;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::LUID;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::ACL;
use windows_sys::Win32::Security::AdjustTokenPrivileges;
use windows_sys::Win32::Security::Authorization::EXPLICIT_ACCESS_W;
use windows_sys::Win32::Security::Authorization::GRANT_ACCESS;
use windows_sys::Win32::Security::Authorization::SetEntriesInAclW;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_SID;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_UNKNOWN;
use windows_sys::Win32::Security::Authorization::TRUSTEE_W;
use windows_sys::Win32::Security::CopySid;
use windows_sys::Win32::Security::CreateRestrictedToken;
use windows_sys::Win32::Security::CreateWellKnownSid;
use windows_sys::Win32::Security::GetLengthSid;
use windows_sys::Win32::Security::GetTokenInformation;
use windows_sys::Win32::Security::LookupPrivilegeValueW;
use windows_sys::Win32::Security::SID_AND_ATTRIBUTES;
use windows_sys::Win32::Security::TOKEN_ADJUST_DEFAULT;
use windows_sys::Win32::Security::TOKEN_ADJUST_PRIVILEGES;
use windows_sys::Win32::Security::TOKEN_ADJUST_SESSIONID;
use windows_sys::Win32::Security::TOKEN_ASSIGN_PRIMARY;
use windows_sys::Win32::Security::TOKEN_DUPLICATE;
use windows_sys::Win32::Security::TOKEN_PRIVILEGES;
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::Security::TokenDefaultDacl;
use windows_sys::Win32::Security::TokenGroups;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

const DISABLE_MAX_PRIVILEGE: u32 = 0x01;
const LUA_TOKEN: u32 = 0x04;
const WRITE_RESTRICTED: u32 = 0x08;
/// `SE_GROUP_LOGON_ID` (0xC0000000); windows-sys places it behind the
/// `Win32_System_SystemServices` feature, so it is kept local to avoid a
/// feature dependency for one constant.
const SE_GROUP_LOGON_ID: u32 = 0xC000_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const WIN_WORLD_SID: i32 = 1;
const SE_PRIVILEGE_ENABLED: u32 = 0x0000_0002;

/// A SID string parsed by the OS (`ConvertStringSidToSidW`), freed with
/// `LocalFree` when dropped.
pub(crate) struct LocalSid {
    psid: *mut c_void,
}

impl LocalSid {
    pub(crate) fn from_string(sid: &str) -> Result<Self, SandboxError> {
        #[link(name = "advapi32")]
        unsafe extern "system" {
            fn ConvertStringSidToSidW(StringSid: *const u16, Sid: *mut *mut c_void) -> i32;
        }
        let wide = crate::winutil::to_wide(sid);
        let mut psid: *mut c_void = std::ptr::null_mut();
        // SAFETY: `wide` outlives the call; on success `psid` is owned and
        // freed in Drop.
        let ok = unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut psid) };
        if ok == 0 || psid.is_null() {
            return Err(SandboxError::Windows {
                call: "ConvertStringSidToSidW",
                code: unsafe { GetLastError() },
            });
        }
        Ok(Self { psid })
    }

    pub(crate) fn as_ptr(&self) -> *const c_void {
        self.psid
    }
}

impl Drop for LocalSid {
    fn drop(&mut self) {
        if !self.psid.is_null() {
            // SAFETY: `psid` was allocated by ConvertStringSidToSidW.
            unsafe { LocalFree(self.psid as HLOCAL) };
        }
    }
}

/// An open process token handle, closed when dropped.
pub(crate) struct TokenHandle(pub(crate) HANDLE);

impl Drop for TokenHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is an owned token handle.
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn open_current_process_token() -> Result<HANDLE, SandboxError> {
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(
            ProcessHandle: HANDLE,
            DesiredAccess: u32,
            TokenHandle: *mut HANDLE,
        ) -> i32;
    }
    let desired = TOKEN_DUPLICATE
        | TOKEN_QUERY
        | TOKEN_ASSIGN_PRIMARY
        | TOKEN_ADJUST_DEFAULT
        | TOKEN_ADJUST_SESSIONID
        | TOKEN_ADJUST_PRIVILEGES;
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a pseudo-handle; `token` is owned on
    // success and wrapped in TokenHandle by the caller.
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), desired, &mut token) };
    if ok == 0 {
        return Err(SandboxError::Windows {
            call: "OpenProcessToken",
            code: unsafe { GetLastError() },
        });
    }
    Ok(token)
}

/// Copy the logon SID out of the token's groups. The logon SID is needed as a
/// restricting SID so ACLs that grant the logon session keep working.
fn logon_sid_bytes(token: HANDLE) -> Result<Vec<u8>, SandboxError> {
    let mut needed: u32 = 0;
    // SAFETY: size query with null buffer is the documented probe.
    unsafe { GetTokenInformation(token, TokenGroups, std::ptr::null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(SandboxError::ConfinementUnavailable {
            path: std::path::PathBuf::from("<token>"),
            reason: "cannot query token groups for the logon SID".to_owned(),
        });
    }
    let mut buffer: Vec<u8> = vec![0u8; needed as usize];
    // SAFETY: `buffer` is sized by the preceding probe.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenGroups,
            buffer.as_mut_ptr() as *mut c_void,
            needed,
            &mut needed,
        )
    };
    if ok == 0 || (needed as usize) < std::mem::size_of::<u32>() {
        return Err(SandboxError::Windows {
            call: "GetTokenInformation (TokenGroups)",
            code: unsafe { GetLastError() },
        });
    }
    // TOKEN_GROUPS layout: u32 count, then (pointer-aligned) SID_AND_ATTRIBUTES.
    let count = u32::from_ne_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
    let after_count = buffer.as_ptr() as usize + std::mem::size_of::<u32>();
    let align = std::mem::align_of::<SID_AND_ATTRIBUTES>();
    let aligned = (after_count + (align - 1)) & !(align - 1);
    for index in 0..count {
        // SAFETY: entries live inside `buffer`, which stays alive here.
        let entry: SID_AND_ATTRIBUTES = unsafe {
            std::ptr::read_unaligned(
                (aligned + index * std::mem::size_of::<SID_AND_ATTRIBUTES>())
                    as *const SID_AND_ATTRIBUTES,
            )
        };
        if entry.Attributes & SE_GROUP_LOGON_ID == SE_GROUP_LOGON_ID {
            // SAFETY: `entry.Sid` points into `buffer`, valid here.
            let length = unsafe { GetLengthSid(entry.Sid) };
            if length == 0 {
                break;
            }
            let mut out = vec![0u8; length as usize];
            // SAFETY: `out` is sized by GetLengthSid.
            if unsafe { CopySid(length, out.as_mut_ptr() as *mut c_void, entry.Sid) } == 0 {
                break;
            }
            return Ok(out);
        }
    }
    Err(SandboxError::ConfinementUnavailable {
        path: std::path::PathBuf::from("<token>"),
        reason: "the token carries no logon SID".to_owned(),
    })
}

fn everyone_sid_bytes() -> Result<Vec<u8>, SandboxError> {
    let mut size: u32 = 0;
    // SAFETY: first call with null buffer is the documented size probe.
    unsafe {
        CreateWellKnownSid(
            WIN_WORLD_SID,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if size == 0 {
        return Err(SandboxError::Windows {
            call: "CreateWellKnownSid (probe)",
            code: unsafe { GetLastError() },
        });
    }
    let mut buffer: Vec<u8> = vec![0u8; size as usize];
    // SAFETY: `buffer` is sized by the probe.
    let ok = unsafe {
        CreateWellKnownSid(
            WIN_WORLD_SID,
            std::ptr::null_mut(),
            buffer.as_mut_ptr() as *mut c_void,
            &mut size,
        )
    };
    if ok == 0 {
        return Err(SandboxError::Windows {
            call: "CreateWellKnownSid",
            code: unsafe { GetLastError() },
        });
    }
    Ok(buffer)
}

/// Set a permissive default DACL on the restricted token so confined
/// processes can create pipes/IPC objects (pipes are protected by the
/// token's default DACL).
fn set_default_dacl(token: HANDLE, sids: &[*const c_void]) -> Result<(), SandboxError> {
    #[repr(C)]
    struct TokenDefaultDaclInfo {
        default_dacl: *mut ACL,
    }
    let entries: Vec<EXPLICIT_ACCESS_W> = sids
        .iter()
        .map(|sid| EXPLICIT_ACCESS_W {
            grfAccessPermissions: GENERIC_ALL,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: 0,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: *sid as *mut u16,
            },
        })
        .collect();
    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `entries` outlives the call; on success `new_dacl` is owned.
    let result = unsafe {
        SetEntriesInAclW(
            entries.len() as u32,
            entries.as_ptr(),
            std::ptr::null_mut(),
            &mut new_dacl,
        )
    };
    if result != ERROR_SUCCESS {
        return Err(SandboxError::Windows {
            call: "SetEntriesInAclW (default DACL)",
            code: result as u32,
        });
    }
    let info = TokenDefaultDaclInfo {
        default_dacl: new_dacl,
    };
    // SAFETY: `info`/`new_dacl` are valid for the call.
    let ok = unsafe {
        windows_sys::Win32::Security::SetTokenInformation(
            token,
            TokenDefaultDacl,
            &info as *const _ as *const c_void,
            std::mem::size_of::<TokenDefaultDaclInfo>() as u32,
        )
    };
    let applied = ok != 0;
    // SAFETY: `new_dacl` is owned here.
    unsafe { LocalFree(new_dacl as HLOCAL) };
    if !applied {
        return Err(SandboxError::Windows {
            call: "SetTokenInformation (TokenDefaultDacl)",
            code: unsafe { GetLastError() },
        });
    }
    Ok(())
}

/// Enable a single privilege on the token (used for bypass-traverse-checking
/// so the confined process can walk directories it can read).
fn enable_privilege(token: HANDLE, name: &str) -> Result<(), SandboxError> {
    let mut luid = LUID {
        LowPart: 0,
        HighPart: 0,
    };
    let wide = crate::winutil::to_wide(name);
    // SAFETY: `wide` outlives the call.
    let ok = unsafe { LookupPrivilegeValueW(std::ptr::null(), wide.as_ptr(), &mut luid) };
    if ok == 0 {
        return Err(SandboxError::Windows {
            call: "LookupPrivilegeValueW",
            code: unsafe { GetLastError() },
        });
    }
    let mut privileges: TOKEN_PRIVILEGES = unsafe { std::mem::zeroed() };
    privileges.PrivilegeCount = 1;
    privileges.Privileges[0].Luid = luid;
    privileges.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
    // SAFETY: `privileges` outlives the call.
    let ok = unsafe {
        AdjustTokenPrivileges(
            token,
            0,
            &privileges,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(SandboxError::Windows {
            call: "AdjustTokenPrivileges",
            code: unsafe { GetLastError() },
        });
    }
    // AdjustTokenPrivileges reports partial failure through GetLastError.
    let remaining = unsafe { GetLastError() };
    if remaining != 0 {
        return Err(SandboxError::ConfinementUnavailable {
            path: std::path::PathBuf::from("<token>"),
            reason: format!("privilege {name} could not be enabled (error {remaining})"),
        });
    }
    Ok(())
}

/// Create the workspace-write restricted token:
/// privileges dropped, LUA-filtered, write-restricted to the restricting
/// SIDs `[capability, logon, everyone]`.
pub(crate) fn create_workspace_write_restricted_token(
    capability: &LocalSid,
) -> Result<TokenHandle, SandboxError> {
    // Wrap the base token so it is closed on every return path (including
    // `?` early-returns below); the restricted token we hand back is a
    // separate handle that the caller owns via the returned `TokenHandle`.
    let base = TokenHandle(open_current_process_token()?);
    let logon = logon_sid_bytes(base.0)?;
    let everyone = everyone_sid_bytes()?;
    // SAFETY: both byte buffers stay alive through `CreateRestrictedToken`
    // below (they are local bindings kept alive for the duration of the call).
    let psid_logon = logon.as_ptr() as *const c_void;
    let psid_everyone = everyone.as_ptr() as *const c_void;

    // Order matters for the reference design: capability, logon, everyone.
    let entries: [SID_AND_ATTRIBUTES; 3] = [
        SID_AND_ATTRIBUTES {
            Sid: capability.as_ptr() as *mut _,
            Attributes: 0,
        },
        SID_AND_ATTRIBUTES {
            Sid: psid_logon as *mut _,
            Attributes: 0,
        },
        SID_AND_ATTRIBUTES {
            Sid: psid_everyone as *mut _,
            Attributes: 0,
        },
    ];
    let flags = DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED;
    let mut restricted: HANDLE = std::ptr::null_mut();
    // SAFETY: `base.0` is an open primary token; entries point to live SIDs;
    // on success `restricted` is owned and returned in `TokenHandle`.
    let ok = unsafe {
        CreateRestrictedToken(
            base.0,
            flags,
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            entries.len() as u32,
            entries.as_ptr(),
            &mut restricted,
        )
    };
    if ok == 0 {
        return Err(SandboxError::Windows {
            call: "CreateRestrictedToken",
            code: unsafe { GetLastError() },
        });
    }
    set_default_dacl(
        restricted,
        &[psid_logon, psid_everyone, capability.as_ptr()],
    )?;
    enable_privilege(restricted, "SeChangeNotifyPrivilege")?;
    Ok(TokenHandle(restricted))
}
