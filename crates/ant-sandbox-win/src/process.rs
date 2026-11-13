//! Confined process launch and lifecycle.
//!
//! Audit contract: handles returned by CreateProcessAsUserW are owned by
//! `Child` and closed exactly once in Drop; std handles are inherited from
//! the caller; the environment block and command line buffers outlive the
//! call they feed.

use crate::error::SandboxError;
use std::ffi::c_void;
use std::path::Path;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::SetHandleInformation;
use windows_sys::Win32::System::Console::GetStdHandle;
use windows_sys::Win32::System::Console::STD_ERROR_HANDLE;
use windows_sys::Win32::System::Console::STD_INPUT_HANDLE;
use windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE;
use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
use windows_sys::Win32::System::Threading::CREATE_UNICODE_ENVIRONMENT;
use windows_sys::Win32::System::Threading::ResumeThread;
use windows_sys::Win32::System::Threading::CreateProcessAsUserW;
use windows_sys::Win32::System::Threading::GetExitCodeProcess;
use windows_sys::Win32::System::Threading::PROCESS_INFORMATION;
use windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES;
use windows_sys::Win32::System::Threading::STARTUPINFOW;
use windows_sys::Win32::System::Threading::TerminateProcess;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

pub(crate) const WAIT_OBJECT_0: u32 = 0;
pub(crate) const WAIT_TIMEOUT: u32 = 0x0000_0102;

/// The interactive default desktop. Some processes (for example PowerShell)
/// fail with STATUS_DLL_INIT_FAILED when launched with a restricted token
/// unless an explicit desktop is set (observed by the reference design).
const DEFAULT_DESKTOP: &str = "winsta0\\default";

/// A confined child process. Handles are closed on drop; the kill-on-close
/// job (assigned at spawn) terminates any surviving descendants.
pub struct Child {
    process: PROCESS_INFORMATION,
    /// The captured exit code, set once `try_wait` observes the child exited.
    exit_code: Option<u32>,
    /// Whether the child could be attached to a kill-on-close job object.
    pub descendants_tracked: bool,
    /// Held so its `Drop` runs the kill-on-close policy for descendants.
    /// Not read directly; the lint is suppressed because the field's
    /// existence is the mechanism that enforces cleanup.
    #[allow(dead_code)]
    job: Option<win32job::Job>,
}

impl Child {
    /// Non-blocking status poll: `Some(exit_code)` once the child exited.
    pub fn try_wait(&mut self) -> Result<Option<u32>, SandboxError> {
        // SAFETY: `self.process.hProcess` is an owned, open handle.
        let state = unsafe { WaitForSingleObject(self.process.hProcess, 0) };
        if state == WAIT_TIMEOUT {
            return Ok(None);
        }
        if state != WAIT_OBJECT_0 {
            return Err(SandboxError::Windows {
                call: "WaitForSingleObject",
                code: unsafe { GetLastError() },
            });
        }
        let mut code: u32 = 0;
        // SAFETY: handle is open; `code` is written by the call.
        let ok = unsafe { GetExitCodeProcess(self.process.hProcess, &mut code) };
        if ok == 0 {
            return Err(SandboxError::Windows {
                call: "GetExitCodeProcess",
                code: unsafe { GetLastError() },
            });
        }
        self.exit_code = Some(code);
        Ok(Some(code))
    }

    /// Terminate the child and its job-tracked descendants. Idempotent;
    /// errors are ignored because a concurrently exiting child is not a
    /// failure of cleanup. Descendant cleanup happens via the kill-on-close
    /// job when `Child` is dropped (win32job 2.x has no explicit terminate).
    pub fn terminate(&mut self) {
        // SAFETY: handle is open; terminating an exiting process is benign.
        unsafe { TerminateProcess(self.process.hProcess, 1) };
    }

    pub fn exit_code(&self) -> u32 {
        self.exit_code.unwrap_or(0)
    }

    /// Peak committed memory of the job (child plus descendants), when the
    /// child is job-tracked.
    pub fn peak_memory_bytes(&self) -> Option<u64> {
        self.job.as_ref().and_then(|job| crate::job::peak_job_memory(job).ok())
    }

    #[allow(dead_code)]
    pub fn process_id(&self) -> u32 {
        self.process.dwProcessId
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        // SAFETY: each handle is owned and closed exactly once.
        unsafe {
            CloseHandle(self.process.hProcess);
            CloseHandle(self.process.hThread);
        }
    }
}

/// Spawn `argv` under the sandbox token with the caller's standard handles
/// inherited, in `cwd`, with the given (small, non-secret) environment.
pub(crate) fn spawn(
    token: HANDLE,
    argv: &[String],
    cwd: &Path,
    environment: &[(String, String)],
    memory_limit_bytes: Option<u64>,
) -> Result<Child, SandboxError> {
    let command_line = crate::winutil::argv_to_command_line(argv);
    let mut command_line_wide = crate::winutil::to_wide(&command_line);
    let environment_block = crate::winutil::make_env_block(environment);
    let cwd_wide = crate::winutil::to_wide(&cwd.as_os_str().to_string_lossy());
    let mut desktop_wide = crate::winutil::to_wide(DEFAULT_DESKTOP);

    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    startup.lpDesktop = desktop_wide.as_mut_ptr();
    startup.dwFlags |= STARTF_USESTDHANDLES;

    // Inherit the caller's standard handles so output stays visible in real
    // time and is captured when the caller's own stdout is a pipe.
    let mut std_handles: [(u32, *mut HANDLE); 3] = [
        (STD_INPUT_HANDLE, &mut startup.hStdInput),
        (STD_OUTPUT_HANDLE, &mut startup.hStdOutput),
        (STD_ERROR_HANDLE, &mut startup.hStdError),
    ];
    for (kind, target) in std_handles.iter_mut() {
        // SAFETY: `kind` is a valid std-handle id; `target` is a valid field.
        let handle = unsafe { GetStdHandle(*kind) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(SandboxError::Windows {
                call: "GetStdHandle",
                code: unsafe { GetLastError() },
            });
        }
        // SAFETY: `handle` is owned by this process.
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            return Err(SandboxError::Windows {
                call: "SetHandleInformation",
                code: unsafe { GetLastError() },
            });
        }
        // SAFETY: `target` is a valid `*mut HANDLE` field pointer inside
        // the live `startup` struct; the assignment does not invalidate any
        // outstanding reference.
        unsafe { **target = handle };
    }

    let mut process_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: all buffers outlive the call; on success the two handles in
    // `process_info` are owned by Child.
    let ok = unsafe {
        CreateProcessAsUserW(
            token,
            std::ptr::null(),
            command_line_wide.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // inherit handles
            // With a memory cap the process starts suspended so the cap is
            // in force before its first instruction runs.
            CREATE_UNICODE_ENVIRONMENT
                | if memory_limit_bytes.is_some() { CREATE_SUSPENDED } else { 0 },
            environment_block.as_ptr() as *const c_void,
            cwd_wide.as_ptr(),
            &startup,
            &mut process_info,
        )
    };
    if ok == 0 {
        return Err(SandboxError::Windows {
            call: "CreateProcessAsUserW",
            code: unsafe { GetLastError() },
        });
    }

    // Kill-on-close job: closing our ends terminates every process in the
    // tree, including grandchildren. Assigned right after spawn; if the
    // process already exited, assignment fails and cleanup is best-effort.
    let mut job = None;
    let mut descendants_tracked = false;
    let mut info = win32job::ExtendedLimitInfo::new();
    info.limit_kill_on_job_close();
    // SAFETY: `process_info.hProcess` is an owned open handle; win32job 2.x
    // takes the raw handle as an `isize`, which is the standard Win32 HANDLE
    // representation under that crate's API.
    if let Ok(open_job) = win32job::Job::create_with_limit_info(&info)
        && open_job.assign_process(process_info.hProcess as isize).is_ok()
    {
        descendants_tracked = true;
        job = Some(open_job);
    }

    let child = Child {
        process: process_info,
        exit_code: None,
        descendants_tracked,
        job,
    };
    if let Some(limit) = memory_limit_bytes {
        // Fail closed: a requested cap that cannot be applied means the
        // (still suspended) process is terminated, never run uncapped.
        let applied = match child.job.as_ref() {
            Some(job) => crate::job::set_job_memory_limit(job, limit),
            None => Err(SandboxError::ConfinementUnavailable {
                path: cwd.to_path_buf(),
                reason: "memory limit requested but the process could not join a job object".to_owned(),
            }),
        };
        if let Err(error) = applied {
            // SAFETY: handle is open; the process never ran user code.
            unsafe { TerminateProcess(child.process.hProcess, 1) };
            return Err(error);
        }
        // SAFETY: `hThread` is the owned primary-thread handle of the
        // suspended process.
        if unsafe { ResumeThread(child.process.hThread) } == u32::MAX {
            let code = unsafe { GetLastError() };
            unsafe { TerminateProcess(child.process.hProcess, 1) };
            return Err(SandboxError::Windows { call: "ResumeThread", code });
        }
    }
    Ok(child)
}
