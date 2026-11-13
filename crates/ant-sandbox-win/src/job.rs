//! Job-object resource limits and suspended-start helpers.
//!
//! `win32job` 2.x keeps its limit structure crate-private and offers only a
//! working-set range (which trims pages rather than capping commit), so the
//! hard per-job commit cap (`JOB_OBJECT_LIMIT_JOB_MEMORY`) and the peak-usage
//! query live here, behind the same audit contract as the rest of this crate.
//!
//! Audit contract: every function takes a live `win32job::Job` (whose handle
//! stays open for the duration of the borrow) or a process id; the
//! `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` buffers are stack-owned and sized
//! with `size_of`; snapshot and thread handles opened here are closed exactly
//! once before returning.

use crate::error::SandboxError;
use std::ffi::c_void;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

fn job_handle(job: &win32job::Job) -> HANDLE {
    job.handle() as HANDLE
}

fn query_extended(
    job: &win32job::Job,
) -> Result<JOBOBJECT_EXTENDED_LIMIT_INFORMATION, SandboxError> {
    // SAFETY: plain-old-data Win32 struct; all-zero is a valid initial value.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the job handle is open for the borrow; `info` is a stack buffer
    // of exactly the size passed.
    let ok = unsafe {
        QueryInformationJobObject(
            job_handle(job),
            JobObjectExtendedLimitInformation,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(SandboxError::Windows {
            call: "QueryInformationJobObject",
            code: unsafe { GetLastError() },
        });
    }
    Ok(info)
}

/// Cap the total committed memory of every process in `job` at
/// `limit_bytes`, preserving the job's existing limits (for example
/// kill-on-close). Allocations beyond the cap fail inside the process; the
/// OS does not kill it. Read back after setting so a silently ignored limit
/// fails closed.
pub fn set_job_memory_limit(job: &win32job::Job, limit_bytes: u64) -> Result<(), SandboxError> {
    let mut info = query_extended(job)?;
    info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
    info.JobMemoryLimit = usize::try_from(limit_bytes).unwrap_or(usize::MAX);
    // SAFETY: the job handle is open; `info` is a fully initialized stack
    // struct of exactly the size passed.
    let ok = unsafe {
        SetInformationJobObject(
            job_handle(job),
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return Err(SandboxError::Windows {
            call: "SetInformationJobObject",
            code: unsafe { GetLastError() },
        });
    }
    let applied = query_extended(job)?;
    if applied.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_JOB_MEMORY == 0
        || applied.JobMemoryLimit != info.JobMemoryLimit
    {
        return Err(SandboxError::ConfinementUnavailable {
            path: std::path::PathBuf::new(),
            reason: "job memory limit did not read back as applied".to_owned(),
        });
    }
    Ok(())
}

/// Peak committed memory, in bytes, used by all processes ever in `job`.
pub fn peak_job_memory(job: &win32job::Job) -> Result<u64, SandboxError> {
    Ok(query_extended(job)?.PeakJobMemoryUsed as u64)
}

/// Resume every thread of process `process_id` (used after a
/// `CREATE_SUSPENDED` spawn once the job and its limits are in place).
/// Returns the number of threads resumed; zero means nothing was found to
/// resume, which callers should treat as a failure.
pub fn resume_process_threads(process_id: u32) -> Result<usize, SandboxError> {
    // SAFETY: documented call; the returned handle is checked and closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(SandboxError::Windows {
            call: "CreateToolhelp32Snapshot",
            code: unsafe { GetLastError() },
        });
    }
    // SAFETY: plain-old-data struct; `dwSize` must be set before use.
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut resumed = 0usize;
    let mut failure = None;
    // SAFETY: `snapshot` is open; `entry` is a valid, sized buffer.
    let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == process_id {
            // SAFETY: documented call; the handle is checked and closed.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                failure = Some(SandboxError::Windows {
                    call: "OpenThread",
                    code: unsafe { GetLastError() },
                });
                break;
            }
            // SAFETY: `thread` is an open handle with suspend/resume access.
            let previous = unsafe { ResumeThread(thread) };
            // SAFETY: closed exactly once.
            unsafe { CloseHandle(thread) };
            if previous == u32::MAX {
                failure = Some(SandboxError::Windows {
                    call: "ResumeThread",
                    code: unsafe { GetLastError() },
                });
                break;
            }
            resumed += 1;
        }
        // SAFETY: as above.
        more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    // SAFETY: closed exactly once.
    unsafe { CloseHandle(snapshot) };
    match failure {
        Some(error) => Err(error),
        None => Ok(resumed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_limit_reads_back_and_preserves_existing_limits() {
        let mut info = win32job::ExtendedLimitInfo::new();
        info.limit_kill_on_job_close();
        let job = win32job::Job::create_with_limit_info(&info).expect("job");
        set_job_memory_limit(&job, 64 * 1024 * 1024).expect("limit");
        let applied = query_extended(&job).expect("query");
        assert_eq!(applied.JobMemoryLimit, 64 * 1024 * 1024);
        assert_ne!(
            applied.BasicLimitInformation.LimitFlags
                & windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            0,
            "kill-on-close must survive adding the memory cap"
        );
        assert_eq!(
            peak_job_memory(&job).expect("peak"),
            0,
            "an empty job has used nothing"
        );
    }
}
