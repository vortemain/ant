//! Windows restricted-token filesystem sandbox for Ant's local executor.
//!
//! # What it confines
//!
//! A run inside the sandbox executes under a restricted token derived from
//! the caller's own token: privileges are dropped
//! (`DISABLE_MAX_PRIVILEGE | LUA_TOKEN`) and write access is restricted
//! (`WRITE_RESTRICT`) to a capability SID that exists only inside this
//! workspace's DACL. Reads and execution stay unrestricted so real
//! toolchains work; writes succeed only where a DACL grants the capability
//! SID (the workspace root, granted at prepare time) or one of the
//! inherently allowed restricting SIDs (logon/everyone — see limitations).
//!
//! The capability SID is derived deterministically from the canonical
//! workspace path (`sid::capability_sid_for_workspace`), so the workspace
//! DACL gains at most one ACE per workspace, the grant is idempotent, and
//! the ACE is inert for every process whose token does not contain the SID.
//!
//! # What it does NOT confine (honest limitations)
//!
//! - **Network**: restricted tokens cannot prevent network access for
//!   same-user code. Ant therefore never declares the network-isolation
//!   capability and cache reuse stays gated (decision D-003).
//! - **World-writable locations**: because the everyone SID participates in
//!   the restricted write check, locations whose DACL grants write to
//!   Everyone remain writable.
//! - **Filesystems without persistent ACLs** (FAT, exFAT, network shares):
//!   detection fails closed (`ConfinementUnavailable`) and the run is
//!   refused rather than executed unsandboxed.
//!
//! # Unsafe-code audit contract
//!
//! This crate deliberately does not inherit the workspace-wide
//! `unsafe_code = "forbid"`; it owns a small, reviewed FFI boundary. Every
//! `unsafe` block: (1) operates on handles/pointers this crate owns or
//! validates, (2) keeps every buffer alive for the duration of the call it
//! feeds, (3) frees or closes every owned resource on every return path
//! (success, error, panic-free), and (4) is documented with a `SAFETY`
//! comment. The public API exposed to the workspace is safe Rust. The
//! design is adapted from OpenAI Codex `codex-rs/windows-sandbox-rs`
//! (Apache-2.0) and documented in `docs/sandbox/windows-design.md`.
//!
//! # Security disclosure
//!
//! This is a first-line write-confinement mechanism, not a hardened
//! security boundary. It is defense against accidental escape paths (build
//! scripts writing outside the workspace) and matches the reference
//! implementations shipped for coding agents; adversarial escape analysis
//! (job breakaway, WMI, ShellExecute) remains future work before the
//! capability is used to gate cache reuse end-to-end. See
//! `docs/sandbox/windows-design.md`.

pub use error::SandboxError;

#[cfg(windows)]
mod acl;
#[cfg(windows)]
mod error;
#[cfg(windows)]
mod process;
#[cfg(windows)]
mod sid;
#[cfg(windows)]
mod token;
#[cfg(windows)]
mod winutil;

#[cfg(not(windows))]
mod stub;

/// A prepared sandbox: a restricted primary token plus a granted workspace
/// DACL. Owns the token; dropping it closes the handle.
pub struct Sandbox {
    #[cfg(windows)]
    token: token::TokenHandle,
    #[cfg(not(windows))]
    token: stub::TokenHandle,
    /// The canonical workspace the grants and token are scoped to.
    pub workspace: std::path::PathBuf,
    /// The capability SID string (diagnostics only).
    pub capability_sid: String,
}

impl Sandbox {
    /// Prepare a write-confined sandbox for `workspace`:
    /// canonicalize, derive the capability SID, grant it write access on the
    /// workspace root, and create the restricted token. Fail closed: any
    /// error means the caller must not execute the action unsandboxed.
    pub fn prepare(workspace: &std::path::Path) -> Result<Self, SandboxError> {
        #[cfg(not(windows))]
        {
            let _ = workspace;
            return Err(SandboxError::UnsupportedPlatform);
        }
        #[cfg(windows)]
        {
            let workspace = workspace.canonicalize().map_err(|error| SandboxError::WorkspaceUnavailable {
                path: workspace.to_path_buf(),
                reason: error.to_string(),
            })?;
            let sid_string = sid::capability_sid_for_workspace(&workspace);
            let capability = token::LocalSid::from_string(&sid_string)?;
            acl::grant_mask(
                &workspace,
                capability.as_ptr(),
                acl::WRITE_ALLOW_MASK,
            )?;
            acl::grant_null_device(capability.as_ptr())?;
            let token = token::create_workspace_write_restricted_token(&capability)?;
            Ok(Self {
                token,
                workspace,
                capability_sid: sid_string,
            })
        }
    }

    /// Spawn `argv` inside the sandbox in `cwd` with the given environment
    /// (already reduced to Ant's allowlist plus declared values by the
    /// caller). Standard handles are inherited from this process.
    pub fn spawn(
        &self,
        argv: &[String],
        cwd: &std::path::Path,
        environment: &[(String, String)],
    ) -> Result<Child, SandboxError> {
        #[cfg(not(windows))]
        {
            let _ = (argv, cwd, environment);
            Err(SandboxError::UnsupportedPlatform)
        }
        #[cfg(windows)]
        {
            if argv.is_empty() {
                return Err(SandboxError::ConfinementUnavailable {
                    path: cwd.to_path_buf(),
                    reason: "empty argv".to_owned(),
                });
            }
            process::spawn(self.token.0, argv, cwd, environment).map(|inner| Child { inner })
        }
    }
}

/// A confined child process.
pub struct Child {
    #[cfg(windows)]
    inner: process::Child,
    #[cfg(not(windows))]
    _private: (),
}

impl Child {
    /// Non-blocking poll: `Some(exit_code)` once the child exited.
    pub fn try_wait(&mut self) -> Result<Option<u32>, SandboxError> {
        #[cfg(not(windows))]
        {
            Err(SandboxError::UnsupportedPlatform)
        }
        #[cfg(windows)]
        {
            self.inner.try_wait()
        }
    }

    /// Terminate the child and its tracked descendants (job object).
    pub fn terminate(&mut self) {
        #[cfg(windows)]
        {
            self.inner.terminate();
        }
    }

    /// Whether descendant cleanup is actually enforced by a job object.
    pub fn descendants_tracked(&self) -> bool {
        #[cfg(windows)]
        {
            self.inner.descendants_tracked
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// The raw exit code; only meaningful after `try_wait` returned `Some`.
    pub fn exit_code(&self) -> u32 {
        #[cfg(windows)]
        {
            self.inner.exit_code()
        }
        #[cfg(not(windows))]
        {
            0
        }
    }
}

#[cfg(not(windows))]
mod stub {
    /// Non-Windows builds carry no FFI; the API fails closed with
    /// `SandboxError::UnsupportedPlatform` and these types keep the surface
    /// compiling without any Windows dependency.
    pub(crate) struct TokenHandle;

    impl TokenHandle {
        pub(crate) fn _private() -> Self {
            Self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_fails_closed_on_non_windows() {
        // On Windows this test verifies a real workspace can be prepared;
        // on other platforms it verifies the honest unsupported error.
        let dir = std::env::temp_dir().join(format!("ant-sandbox-prep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        match Sandbox::prepare(&dir) {
            #[cfg(windows)]
            Ok(sandbox) => {
                assert!(sandbox.capability_sid.starts_with("S-1-5-21-"));
                assert!(sandbox.workspace.ends_with(dir.file_name().unwrap()));
            }
            #[cfg(not(windows))]
            Err(SandboxError::UnsupportedPlatform) => {}
            #[cfg(not(windows))]
            other => panic!("unexpected result: {other:?}"),
            #[cfg(windows)]
            Err(error) => panic!("sandbox prepare failed on Windows: {error}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
