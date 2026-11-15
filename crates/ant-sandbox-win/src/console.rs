//! Console queries (D-045): the width of the terminal this process writes
//! to, so tools whose output Ant tees through a pipe can still draw progress
//! bars that fit.
//!
//! Audit contract: one read-only Win32 query on this process's own standard
//! error handle; the returned structure is plain data.

use windows_sys::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo, GetStdHandle, STD_ERROR_HANDLE,
};

/// The visible width, in columns, of the console behind standard error, or
/// `None` when standard error is not a console.
pub fn stderr_width() -> Option<u16> {
    // SAFETY: GetStdHandle has no preconditions; the handle it returns is
    // owned by the process and must not be closed, which we do not do.
    let handle = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    if handle.is_null() || handle as isize == -1 {
        return None;
    }
    // SAFETY: plain-old-data structure; zero is a valid initial value.
    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: `handle` is this process's stderr handle and `info` is a valid,
    // writable CONSOLE_SCREEN_BUFFER_INFO for the call's duration.
    let ok = unsafe { GetConsoleScreenBufferInfo(handle, &mut info) };
    if ok == 0 {
        return None;
    }
    let width = info.srWindow.Right - info.srWindow.Left + 1;
    u16::try_from(width).ok().filter(|width| *width > 0)
}
