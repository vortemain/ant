//! The container image adapter (roadmap P6): builds a Docker image from a
//! `Dockerfile`.
//!
//! A container project is a directory with a `Dockerfile` at its root. The
//! adapter derives an image tag from the directory name and generates one
//! action that runs `docker build -t <tag> .`, producing a Docker image in the
//! local daemon.
//!
//! Honest limits (decision D-010, recorded in the module tests):
//!
//! - Base images are pulled from a registry (Docker Hub, a private registry,
//!   …); those downloads live outside the workspace and are undeclared inputs,
//!   so generated actions are `cache_policy: never` (D-003, D-010) — the same
//!   stance the cargo, npm, Go, and Python adapters take on registry
//!   dependency sources.
//! - The build context is the entire project directory; `Dockerfile` and
//!   `.dockerignore` are declared inputs (they control what enters the
//!   context), but the rest of the context is not enumerated. A `.dockerignore`
//!   can exclude anything; Ant does not interpret it.
//! - By default (`output = image`) the action produces no file output — the
//!   image lives in the Docker daemon, not on disk. `--option output=tar`
//!   switches to `docker build --output type=docker,dest=.ant/docker/image.tar …`,
//!   producing a portable image tar as a `File` output (inside `.ant/`, so
//!   `ant build` collects it into `dist/` without colliding with it). The `--output` flag
//!   requires BuildKit (the default in Docker 23+); `DOCKER_BUILDKIT=1` is set
//!   in the action environment so older daemons use it too.
//! - Toolchain identity is captured from the real `docker`: the version from
//!   `docker --version` and the SHA-256 digest of the resolved `docker`
//!   executable. `docker` is a standalone executable (`docker.exe` on
//!   Windows), not a batch shim, so no `cmd /C` wrapper is needed.
//! - Workspace-relative scratch: `TMP`/`TEMP` (plus `TMPDIR` off Windows)
//!   point at `.ant/tmp`, the staging pattern shared with the other adapters.

use build_action::{Action, CachePolicy, Input, InputKind, Output, OutputKind, ToolchainIdentity};
use build_graph::{PlanAction, PlanFile};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::{
    AdapterError, AdapterOptions, BuildAdapter, Detection, OptionSpec, canonical_root,
    check_option_keys, executable_version, locate_executable,
};

pub struct DockerAdapter;

const OPTIONS: &[OptionSpec] = &[
    OptionSpec {
        key: "tag",
        values: "<name>",
        description: "the Docker image tag; defaults to the project directory name",
    },
    OptionSpec {
        key: "output",
        values: "image|tar",
        description: "the build output: `image` (default) loads the image into the Docker \
                          daemon (no file artifact), `tar` writes a portable image tar to \
                          .ant/docker/image.tar (requires BuildKit)",
    },
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputMode {
    /// `docker build -t <tag> .` — image in the daemon, no file output.
    Image,
    /// `docker build --output type=docker,dest=.ant/docker/image.tar -t <tag> .` —
    /// portable image tar as a `File` output.
    Tar,
}

fn output_mode_from(options: &AdapterOptions) -> Result<OutputMode, AdapterError> {
    match options.get("output") {
        None | Some("image") => Ok(OutputMode::Image),
        Some("tar") => Ok(OutputMode::Tar),
        Some(other) => Err(AdapterError::Option(format!(
            "docker output '{other}' is not supported (supported: image, tar)"
        ))),
    }
}

/// Derive the default image tag from the project directory name (last segment
/// of the canonical root). Docker tags must be lowercase; the name is
/// lowercased. If the directory name has characters invalid in a tag (spaces,
/// uppercase that were already lowercased, …) the user should pass `--option
/// tag=<name>`.
fn default_tag(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_lowercase())
        .unwrap_or_else(|| "ant-build".to_owned())
}

impl BuildAdapter for DockerAdapter {
    /// A Dockerfile packages a project another adapter builds; Docker is
    /// chosen only when nothing else matches or `ant.toml` names it (D-036).
    fn packaging_only(&self) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "docker"
    }

    fn options(&self) -> &'static [OptionSpec] {
        OPTIONS
    }

    fn detect(&self, project_root: &Path) -> Result<Detection, AdapterError> {
        if project_root.join("Dockerfile").is_file() {
            Ok(Detection::supported())
        } else {
            Ok(Detection::unsupported(format!(
                "no Dockerfile at {}",
                project_root.display()
            )))
        }
    }

    fn generate(
        &self,
        project_root: &Path,
        options: &AdapterOptions,
    ) -> Result<PlanFile, AdapterError> {
        check_option_keys(self, options)?;
        let mode = output_mode_from(options)?;
        let root = canonical_root(project_root)?;
        // Require a Dockerfile: detect's marker check, repeated here so a
        // `--adapter docker` invocation on the wrong project fails at
        // generation with the same honest reason rather than a misleading
        // toolchain error.
        if !root.join("Dockerfile").is_file() {
            return Err(AdapterError::Project(format!(
                "no Dockerfile at {}",
                root.display()
            )));
        }

        let tag = options
            .get("tag")
            .map(|t| t.to_owned())
            .unwrap_or_else(|| default_tag(&root));
        let inputs = declare_inputs(&root);
        let outputs = match mode {
            OutputMode::Image => Vec::new(),
            OutputMode::Tar => vec![Output {
                path: PathBuf::from(".ant").join("docker").join("image.tar"),
                kind: OutputKind::File,
            }],
        };
        let id = "docker-build".to_owned();
        let action = build_action(id, &tag, mode, inputs, outputs);
        let mut file = PlanFile::new();
        file.actions.push(PlanAction {
            action,
            dependencies: Vec::new(),
        });
        // Validate the plan shape before the toolchain digest and before any
        // side effect.
        file.to_graph()?;
        let toolchain = docker_toolchain(&root)?;
        for entry in &mut file.actions {
            entry.action.toolchain = Some(toolchain.clone());
        }
        file.to_graph()?;
        std::fs::create_dir_all(root.join(".ant").join("tmp"))?;
        std::fs::create_dir_all(root.join(".ant").join("docker"))?;
        Ok(file)
    }
}

/// Declare the workspace inputs Ant can honestly cover: the `Dockerfile` (the
/// build recipe) and `.dockerignore` (controls the build context) when
/// present. The rest of the build context is not enumerated — a `.dockerignore`
/// can exclude anything, and Ant does not interpret it. Base images pulled
/// from a registry are outside the workspace and intentionally not declared
/// (see the module docs).
fn declare_inputs(root: &Path) -> BTreeMap<PathBuf, InputKind> {
    let mut inputs = BTreeMap::new();
    inputs.insert(PathBuf::from("Dockerfile"), InputKind::File);
    if root.join(".dockerignore").is_file() {
        inputs.insert(PathBuf::from(".dockerignore"), InputKind::File);
    }
    inputs
}

/// `docker build -t <tag> .` or
/// `docker build --output type=docker,dest=.ant/docker/image.tar -t <tag> .`.
fn docker_build_argv(tag: &str, mode: OutputMode) -> Vec<String> {
    let mut argv = vec![
        "docker".to_owned(),
        "build".to_owned(),
        "-t".to_owned(),
        tag.to_owned(),
    ];
    if mode == OutputMode::Tar {
        argv.push("--output".to_owned());
        argv.push("type=docker,dest=.ant/docker/image.tar".to_owned());
    }
    argv.push(".".to_owned());
    argv
}

/// `TMP`/`TEMP` (plus `TMPDIR` off Windows) pointing at `.ant/tmp` relative to
/// the action's working directory (the shared staging pattern), plus
/// `DOCKER_BUILDKIT=1` so the `--output` flag is available even on older
/// daemons.
fn staging_environment() -> Vec<(String, String)> {
    let staging_value = PathBuf::from(".ant")
        .join("tmp")
        .to_string_lossy()
        .into_owned();
    let mut environment = vec![
        ("TMP".to_owned(), staging_value.clone()),
        ("TEMP".to_owned(), staging_value.clone()),
        ("DOCKER_BUILDKIT".to_owned(), "1".to_owned()),
    ];
    if cfg!(not(windows)) {
        environment.push(("TMPDIR".to_owned(), staging_value));
    }
    environment.sort();
    environment
}

fn build_action(
    id: String,
    tag: &str,
    mode: OutputMode,
    inputs: BTreeMap<PathBuf, InputKind>,
    outputs: Vec<Output>,
) -> Action {
    Action {
        schema_version: 1,
        id,
        argv: docker_build_argv(tag, mode),
        working_directory: PathBuf::from("."),
        inputs: inputs
            .into_iter()
            .map(|(path, kind)| Input { path, kind })
            .collect(),
        outputs,
        environment: staging_environment(),
        // Base images are pulled from a registry (undeclared inputs outside
        // the workspace), so these actions never claim shared cache
        // eligibility (D-003, D-010).
        cache_policy: CachePolicy::Never,
        toolchain: None,
        timeout_ms: None,
        resources: None,
        background_services: false,
    }
}

/// Truthful Docker toolchain identity: the version from `docker --version`
/// and the SHA-256 digest of the resolved `docker` executable.
fn docker_toolchain(root: &Path) -> Result<ToolchainIdentity, AdapterError> {
    let docker = locate_executable(&format!("docker{}", std::env::consts::EXE_SUFFIX))?;
    let digest_target = docker.canonicalize().map_err(|error| {
        AdapterError::Toolchain(format!(
            "cannot resolve docker executable {}: {error}",
            docker.display()
        ))
    })?;
    let version = executable_version(&docker, root)?;
    // `docker --version` prints e.g. `Docker version 29.9.1, build dfc520d`;
    // the full line is the version.
    let version = version.lines().next().unwrap_or_default().trim().to_owned();
    let executable_digest = crate::toolchain_digest(&digest_target)
        .map_err(|error| {
            AdapterError::Toolchain(format!(
                "cannot digest docker executable {}: {error}",
                digest_target.display()
            ))
        })?
        .to_string();
    Ok(ToolchainIdentity {
        name: "docker".to_owned(),
        version,
        executable_digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "ant-docker-adapter-{name}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdirs");
        }
        fs::write(path, contents).expect("write");
    }

    fn options(pairs: &[(&str, &str)]) -> AdapterOptions {
        let mut options = AdapterOptions::new();
        for (key, value) in pairs {
            options.insert(key, value).expect("insert option");
        }
        options
    }

    fn dockerfile() -> &'static str {
        "FROM alpine:latest\nCMD [\"echo\", \"hello\"]\n"
    }

    #[test]
    fn detect_recognizes_dockerfile_and_rejects_others() {
        let adapter = DockerAdapter;
        let dir = TempDir::new("detect");
        write(&dir.path().join("Dockerfile"), dockerfile());
        assert!(adapter.detect(dir.path()).expect("detect").supported);

        let empty = TempDir::new("empty");
        let detection = adapter.detect(empty.path()).expect("detect");
        assert!(!detection.supported);
        assert!(
            detection.reason.contains("no Dockerfile"),
            "{}",
            detection.reason
        );
    }

    #[test]
    fn output_rejects_unknown_values() {
        let adapter = DockerAdapter;
        let dir = TempDir::new("output");
        write(&dir.path().join("Dockerfile"), dockerfile());
        let error = adapter
            .generate(dir.path(), &options(&[("output", "oci")]))
            .expect_err("bad output must fail");
        assert!(
            error.to_string().contains("supported: image, tar"),
            "{}",
            error
        );
    }

    #[test]
    fn generate_rejects_non_docker_project() {
        let adapter = DockerAdapter;
        let dir = TempDir::new("not-docker");
        write(&dir.path().join("README.md"), "not a docker project");
        let error = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect_err("must fail");
        assert!(error.to_string().contains("no Dockerfile"), "{}", error);
    }

    #[test]
    fn unknown_option_rejected() {
        let adapter = DockerAdapter;
        let dir = TempDir::new("unknown");
        write(&dir.path().join("Dockerfile"), dockerfile());
        let error = adapter
            .generate(dir.path(), &options(&[("platform", "linux/amd64")]))
            .expect_err("unknown option must fail");
        assert!(
            error
                .to_string()
                .contains("does not accept option 'platform'"),
            "{}",
            error
        );
    }

    #[test]
    fn default_tag_from_directory_name() {
        let root = Path::new("/tmp/my-Cool-App");
        let tag = default_tag(root);
        assert_eq!(tag, "my-cool-app");
    }

    #[test]
    fn custom_tag_used_when_provided() {
        let adapter = DockerAdapter;
        let dir = TempDir::new("custom-tag");
        write(&dir.path().join("Dockerfile"), dockerfile());
        let plan = adapter
            .generate(dir.path(), &options(&[("tag", "myregistry/app:v2")]))
            .expect("plan");
        let action = &plan.actions[0].action;
        assert!(action.argv.contains(&"myregistry/app:v2".to_owned()));
    }

    #[test]
    fn image_mode_has_no_file_outputs() {
        let adapter = DockerAdapter;
        let dir = TempDir::new("image");
        write(&dir.path().join("Dockerfile"), dockerfile());
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert!(action.outputs.is_empty());
    }

    #[test]
    fn tar_mode_declares_image_tar_output() {
        let adapter = DockerAdapter;
        let dir = TempDir::new("tar");
        write(&dir.path().join("Dockerfile"), dockerfile());
        let plan = adapter
            .generate(dir.path(), &options(&[("output", "tar")]))
            .expect("plan");
        let action = &plan.actions[0].action;
        assert_eq!(action.outputs.len(), 1);
        assert_eq!(
            action.outputs[0].path,
            PathBuf::from(".ant/docker/image.tar")
        );
        assert_eq!(action.outputs[0].kind, OutputKind::File);
    }

    #[test]
    fn dockerfile_and_dockerignore_declared_as_inputs() {
        let adapter = DockerAdapter;
        let dir = TempDir::new("inputs");
        write(&dir.path().join("Dockerfile"), dockerfile());
        write(&dir.path().join(".dockerignore"), "target/\n.git\n");
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        let input_paths: Vec<&Path> = action.inputs.iter().map(|i| i.path.as_path()).collect();
        assert!(input_paths.contains(&Path::new("Dockerfile")));
        assert!(input_paths.contains(&Path::new(".dockerignore")));
    }

    #[test]
    fn dockerignore_absent_not_declared() {
        let adapter = DockerAdapter;
        let dir = TempDir::new("no-ignore");
        write(&dir.path().join("Dockerfile"), dockerfile());
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        let has_dockerignore = action
            .inputs
            .iter()
            .any(|i| i.path.as_path() == Path::new(".dockerignore"));
        assert!(!has_dockerignore);
    }

    #[test]
    fn image_mode_argv_is_docker_build_tag_dot() {
        let adapter = DockerAdapter;
        let dir = TempDir::new("argv-image");
        write(&dir.path().join("Dockerfile"), dockerfile());
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert_eq!(action.argv[0], "docker");
        assert_eq!(action.argv[1], "build");
        assert_eq!(action.argv[2], "-t");
        // argv[3] is the tag (directory name, lowercased)
        assert_eq!(action.argv[4], ".");
    }

    #[test]
    fn tar_mode_argv_includes_output_flag() {
        let adapter = DockerAdapter;
        let dir = TempDir::new("argv-tar");
        write(&dir.path().join("Dockerfile"), dockerfile());
        let plan = adapter
            .generate(dir.path(), &options(&[("output", "tar")]))
            .expect("plan");
        let action = &plan.actions[0].action;
        assert_eq!(action.argv[0], "docker");
        assert_eq!(action.argv[1], "build");
        let output_idx = action
            .argv
            .iter()
            .position(|a| a == "--output")
            .expect("--output flag present");
        assert!(action.argv[output_idx + 1].starts_with("type=docker,dest="));
    }

    #[test]
    fn cache_policy_is_never() {
        let adapter = DockerAdapter;
        let dir = TempDir::new("cache");
        write(&dir.path().join("Dockerfile"), dockerfile());
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert_eq!(action.cache_policy, CachePolicy::Never);
    }

    #[test]
    fn docker_buildkit_env_set() {
        let adapter = DockerAdapter;
        let dir = TempDir::new("buildkit");
        write(&dir.path().join("Dockerfile"), dockerfile());
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert!(
            action
                .environment
                .iter()
                .any(|(k, v)| k == "DOCKER_BUILDKIT" && v == "1")
        );
    }

    #[test]
    fn plan_has_exactly_one_action() {
        let adapter = DockerAdapter;
        let dir = TempDir::new("one-action");
        write(&dir.path().join("Dockerfile"), dockerfile());
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.actions[0].action.id, "docker-build");
        assert!(plan.actions[0].dependencies.is_empty());
    }
}
