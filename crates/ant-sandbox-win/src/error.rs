//! Error contract for the Windows sandbox.

use std::fmt;
use std::path::PathBuf;

#[derive(Debug)]
pub enum SandboxError {
    /// Sandbox support exists only on Windows.
    UnsupportedPlatform,
    /// The workspace could not be resolved or is not a directory.
    WorkspaceUnavailable { path: PathBuf, reason: String },
    /// Confinement cannot be provided on this host (for example a filesystem
    /// without persistent ACLs). Fail closed: the run must not proceed
    /// unsandboxed.
    ConfinementUnavailable { path: PathBuf, reason: String },
    /// A Win32 call failed. `call` names the API, `code` the last error.
    Windows { call: &'static str, code: u32 },
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                write!(f, "the sandbox is implemented for Windows only")
            }
            Self::WorkspaceUnavailable { path, reason } => {
                write!(
                    f,
                    "workspace {} unusable for sandboxing: {reason}",
                    path.display()
                )
            }
            Self::ConfinementUnavailable { path, reason } => {
                write!(f, "cannot confine writes on {}: {reason}", path.display())
            }
            Self::Windows { call, code } => write!(f, "{call} failed (Win32 error {code})"),
        }
    }
}

impl std::error::Error for SandboxError {}
