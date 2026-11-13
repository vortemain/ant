//! Canonical action contract boundary.
//!
//! This first slice uses a conservative model and serde-compatible data types.
//! Canonical encoding, action-key hashing, remote mappings, and execution are
//! not owned by this crate yet; see `docs/architecture.md` before extending it.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Whether the action may be considered for cache reuse.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CachePolicy {
    /// Safe default until complete inputs and determinism are established.
    Never,
    /// The planner asserts all output-affecting inputs are declared.
    Eligible,
    /// Reusable only on the machine that built it, never shared (D-028):
    /// every output-affecting input is declared or pinned (for example
    /// registry dependencies pinned by a lock file), but some pinned content
    /// lives outside the project, so the result cannot be proven elsewhere.
    Local,
}

/// A declared workspace-relative file or tree input.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Input {
    pub path: PathBuf,
    pub kind: InputKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    File,
    Tree,
}

/// A declared workspace-relative output path.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Output {
    pub path: PathBuf,
    pub kind: OutputKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    File,
    Tree,
}

/// One process invocation with explicit inputs and outputs.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub schema_version: u32,
    /// Stable caller-provided identifier; not a content digest.
    pub id: String,
    /// Executable and arguments, without implicit shell interpretation.
    pub argv: Vec<String>,
    /// Workspace-relative directory.
    pub working_directory: PathBuf,
    pub inputs: Vec<Input>,
    pub outputs: Vec<Output>,
    /// Non-secret environment only. Secret references belong to a separate contract.
    pub environment: Vec<(String, String)>,
    pub cache_policy: CachePolicy,
    /// Adapter-provided compiler/runtime identity, required for cache reuse.
    #[serde(default)]
    pub toolchain: Option<ToolchainIdentity>,
    /// Maximum process runtime; `None` allows an unbounded run.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Scheduling weight requested by the action. `None` means the default
    /// request (one CPU slot, no declared memory). This is an admission hint
    /// for the scheduler only: it does not limit the process at runtime and
    /// is deliberately excluded from the action key because it cannot change
    /// the action's outputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequest>,
}

/// Resources an action asks the scheduler to reserve while it runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceRequest {
    /// CPU slots (compared against `--jobs`). Must be at least 1.
    #[serde(default = "default_cpu")]
    pub cpu: u32,
    /// Expected peak memory in MiB, compared against an optional memory
    /// budget. `None` declares nothing; zero is invalid.
    #[serde(default)]
    pub memory_mb: Option<u64>,
}

fn default_cpu() -> u32 {
    1
}

impl Default for ResourceRequest {
    fn default() -> Self {
        Self {
            cpu: 1,
            memory_mb: None,
        }
    }
}

impl Action {
    /// The effective resource request (the default when none is declared).
    pub fn resource_request(&self) -> ResourceRequest {
        self.resources.unwrap_or_default()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolchainIdentity {
    pub name: String,
    pub version: String,
    /// SHA-256 digest of the selected compiler/runtime executable.
    pub executable_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValidationError {
    UnsupportedSchemaVersion(u32),
    EmptyId,
    EmptyCommand,
    InvalidArgument,
    InvalidPath(PathBuf),
    DuplicateInput(PathBuf),
    DuplicateOutput(PathBuf),
    EmptyEnvironmentKey,
    InvalidEnvironmentKey(String),
    DuplicateEnvironmentKey(String),
    InvalidEnvironmentValue(String),
    InvalidTimeout,
    InvalidToolchainIdentity,
    InvalidResources,
}

impl Action {
    /// Validate basic structural invariants. Filesystem containment, symlink
    /// resolution, canonical encoding, and cache completeness are later layers.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.schema_version != 1 {
            return Err(ValidationError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        if self.id.trim().is_empty() {
            return Err(ValidationError::EmptyId);
        }
        if self
            .argv
            .first()
            .is_none_or(|program| program.trim().is_empty())
        {
            return Err(ValidationError::EmptyCommand);
        }
        if self.argv.iter().any(|arg| arg.contains('\0')) {
            return Err(ValidationError::InvalidArgument);
        }
        if self.timeout_ms == Some(0) {
            return Err(ValidationError::InvalidTimeout);
        }
        if let Some(resources) = &self.resources
            && (resources.cpu == 0 || resources.memory_mb == Some(0))
        {
            return Err(ValidationError::InvalidResources);
        }
        if let Some(toolchain) = &self.toolchain
            && (toolchain.name.trim().is_empty()
                || toolchain.version.trim().is_empty()
                || !is_sha256_digest(&toolchain.executable_digest))
        {
            return Err(ValidationError::InvalidToolchainIdentity);
        }
        validate_relative_path(&self.working_directory)?;
        let mut inputs = std::collections::HashSet::new();
        for input in &self.inputs {
            validate_relative_path(&input.path)?;
            if !inputs.insert(&input.path) {
                return Err(ValidationError::DuplicateInput(input.path.clone()));
            }
        }
        let mut outputs = std::collections::HashSet::new();
        for output in &self.outputs {
            validate_relative_path(&output.path)?;
            if !outputs.insert(&output.path) {
                return Err(ValidationError::DuplicateOutput(output.path.clone()));
            }
        }
        let mut environment_keys = std::collections::HashSet::new();
        for (key, value) in &self.environment {
            if key.trim().is_empty() {
                return Err(ValidationError::EmptyEnvironmentKey);
            }
            if key.contains('=') || key.contains('\0') {
                return Err(ValidationError::InvalidEnvironmentKey(key.clone()));
            }
            if value.contains('\0') {
                return Err(ValidationError::InvalidEnvironmentValue(key.clone()));
            }
            if !environment_keys.insert(key) {
                return Err(ValidationError::DuplicateEnvironmentKey(key.clone()));
            }
        }
        Ok(())
    }
}

fn is_sha256_digest(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Workspace-relative paths only, judged the same way on every platform so a
/// plan valid on one OS is not an escape on another:
/// - no `..` and no root (`/x`, `\x`): on Windows a rooted path without a
///   drive is not `is_absolute()`, yet `workspace.join("/x")` resolves to the
///   drive root — outside the workspace;
/// - no drive-style prefix (`C:\x`, `C:x`): absolute or drive-relative on
///   Windows, and never portable, even where (Unix) it is a legal file name.
fn validate_relative_path(path: &Path) -> Result<(), ValidationError> {
    use std::path::Component;
    let text = path.to_string_lossy();
    let bytes = text.as_bytes();
    let drive_style = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || drive_style
        || text.starts_with(['/', '\\'])
        || path.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        return Err(ValidationError::InvalidPath(path.to_path_buf()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Action {
        Action {
            schema_version: 1,
            id: "compile".to_owned(),
            argv: vec!["cc".to_owned(), "-c".to_owned(), "main.c".to_owned()],
            working_directory: PathBuf::from("src"),
            inputs: vec![Input {
                path: PathBuf::from("src/main.c"),
                kind: InputKind::File,
            }],
            outputs: vec![Output {
                path: PathBuf::from("build/main.o"),
                kind: OutputKind::File,
            }],
            environment: vec![("OPT".to_owned(), "2".to_owned())],
            cache_policy: CachePolicy::Never,
            toolchain: None,
            timeout_ms: None,
            resources: None,
        }
    }

    fn assert_error(action: &Action, expected: ValidationError) {
        assert_eq!(
            action.validate(),
            Err(expected),
            "unexpected validation outcome"
        );
    }

    #[test]
    fn accepts_well_formed_action() {
        assert_eq!(sample().validate(), Ok(()));
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let mut action = sample();
        action.schema_version = 2;
        assert_error(&action, ValidationError::UnsupportedSchemaVersion(2));
    }

    #[test]
    fn rejects_blank_id() {
        let mut action = sample();
        action.id = "   ".to_owned();
        assert_error(&action, ValidationError::EmptyId);
    }

    #[test]
    fn rejects_empty_command() {
        let mut action = sample();
        action.argv = Vec::new();
        assert_error(&action, ValidationError::EmptyCommand);
    }

    #[test]
    fn rejects_whitespace_command() {
        let mut action = sample();
        action.argv = vec!["  ".to_owned()];
        assert_error(&action, ValidationError::EmptyCommand);
    }

    #[test]
    fn rejects_argument_with_nul_byte() {
        let mut action = sample();
        action.argv[1] = "-DFOO\0".to_owned();
        assert_error(&action, ValidationError::InvalidArgument);
    }

    #[test]
    fn rejects_zero_timeout() {
        let mut action = sample();
        action.timeout_ms = Some(0);
        assert_error(&action, ValidationError::InvalidTimeout);
    }

    #[test]
    fn validates_resource_requests_and_defaults() {
        let mut action = sample();
        assert_eq!(
            action.resource_request(),
            ResourceRequest {
                cpu: 1,
                memory_mb: None
            }
        );
        action.resources = Some(ResourceRequest {
            cpu: 4,
            memory_mb: Some(512),
        });
        assert_eq!(action.validate(), Ok(()));
        action.resources = Some(ResourceRequest {
            cpu: 0,
            memory_mb: None,
        });
        assert_error(&action, ValidationError::InvalidResources);
        action.resources = Some(ResourceRequest {
            cpu: 1,
            memory_mb: Some(0),
        });
        assert_error(&action, ValidationError::InvalidResources);

        // `cpu` defaults to 1 when only memory is declared; unknown fields are
        // rejected; an action without `resources` serializes without the key
        // (existing plans and adapter output stay byte-identical).
        let parsed: ResourceRequest = serde_json::from_str(r#"{"memory_mb":64}"#).expect("parse");
        assert_eq!(
            parsed,
            ResourceRequest {
                cpu: 1,
                memory_mb: Some(64)
            }
        );
        assert!(serde_json::from_str::<ResourceRequest>(r#"{"gpu":1}"#).is_err());
        let json = serde_json::to_string(&sample()).expect("serialize");
        assert!(!json.contains("resources"), "{json}");
    }

    #[test]
    fn rejects_absolute_working_directory() {
        // Every one of these is rejected on every platform: absolute, rooted
        // (escapes to the drive root on Windows), or drive-style.
        for path in ["C:\\Windows", "C:Windows", "c:/x", "/etc", "\\etc", "/"] {
            let mut action = sample();
            action.working_directory = PathBuf::from(path);
            assert_error(&action, ValidationError::InvalidPath(PathBuf::from(path)));
        }
        // Ordinary relative paths still pass.
        for path in ["src", "src/nested", ".", "src/x.y", ".ant/tmp"] {
            let mut action = sample();
            action.working_directory = PathBuf::from(path);
            assert_eq!(action.validate(), Ok(()), "{path}");
        }
    }

    #[test]
    fn rejects_parent_directory_traversal() {
        let mut action = sample();
        action.inputs[0].path = PathBuf::from("../secrets.txt");
        assert_error(
            &action,
            ValidationError::InvalidPath(PathBuf::from("../secrets.txt")),
        );
    }

    #[test]
    fn rejects_empty_path() {
        let mut action = sample();
        action.outputs[0].path = PathBuf::from("");
        assert_error(&action, ValidationError::InvalidPath(PathBuf::from("")));
    }

    #[test]
    fn rejects_duplicate_inputs() {
        let mut action = sample();
        action.inputs.push(Input {
            path: PathBuf::from("src/main.c"),
            kind: InputKind::File,
        });
        assert_error(
            &action,
            ValidationError::DuplicateInput(PathBuf::from("src/main.c")),
        );
    }

    #[test]
    fn rejects_duplicate_outputs() {
        let mut action = sample();
        action.outputs.push(Output {
            path: PathBuf::from("build/main.o"),
            kind: OutputKind::Tree,
        });
        assert_error(
            &action,
            ValidationError::DuplicateOutput(PathBuf::from("build/main.o")),
        );
    }

    #[test]
    fn rejects_empty_environment_key() {
        let mut action = sample();
        action.environment = vec![("".to_owned(), "1".to_owned())];
        assert_error(&action, ValidationError::EmptyEnvironmentKey);
    }

    #[test]
    fn rejects_environment_key_with_equals() {
        let mut action = sample();
        action.environment = vec![("A=B".to_owned(), "1".to_owned())];
        assert_error(
            &action,
            ValidationError::InvalidEnvironmentKey("A=B".to_owned()),
        );
    }

    #[test]
    fn rejects_environment_value_with_nul_byte() {
        let mut action = sample();
        action.environment = vec![("A".to_owned(), "\0".to_owned())];
        assert_error(
            &action,
            ValidationError::InvalidEnvironmentValue("A".to_owned()),
        );
    }

    #[test]
    fn rejects_duplicate_environment_keys() {
        let mut action = sample();
        action.environment = vec![
            ("A".to_owned(), "1".to_owned()),
            ("A".to_owned(), "2".to_owned()),
        ];
        assert_error(
            &action,
            ValidationError::DuplicateEnvironmentKey("A".to_owned()),
        );
    }

    #[test]
    fn rejects_malformed_toolchain_digest() {
        let mut action = sample();
        action.toolchain = Some(ToolchainIdentity {
            name: "clang".to_owned(),
            version: "17".to_owned(),
            executable_digest: "not-a-digest".to_owned(),
        });
        assert_error(&action, ValidationError::InvalidToolchainIdentity);
    }

    #[test]
    fn accepts_well_formed_toolchain_digest() {
        let mut action = sample();
        action.toolchain = Some(ToolchainIdentity {
            name: "clang".to_owned(),
            version: "17".to_owned(),
            executable_digest: format!("sha256:{}", "a".repeat(64)),
        });
        assert_eq!(action.validate(), Ok(()));
    }

    #[test]
    fn serializes_and_round_trips() {
        let action = sample();
        let json = serde_json::to_string(&action).expect("serialize");
        let parsed: Action = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, action);
    }

    #[test]
    fn rejects_unknown_fields_in_serialization() {
        let json = r#"{
            "schema_version": 1,
            "id": "x",
            "argv": ["cmd"],
            "working_directory": ".",
            "inputs": [],
            "outputs": [],
            "environment": [],
            "cache_policy": "never",
            "surprise": true
        }"#;
        let parsed: Result<Action, _> = serde_json::from_str(json);
        assert!(parsed.is_err(), "unknown fields must be rejected");
    }
}
