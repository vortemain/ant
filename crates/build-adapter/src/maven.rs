//! The Java/Maven adapter (roadmap P6): builds a Maven project's artifact.
//!
//! A Maven project is a directory with a `pom.xml` at its root. The adapter
//! generates one action that runs `mvn package -DskipTests`, which compiles the
//! code, packages it into a JAR/WAR/EAR in `target/`, and skips running tests
//! (the build-tool equivalent of `cargo build`, not `cargo test`). Tests are a
//! separate concern; `--option goal=verify` includes them.
//!
//! Honest limits (decision D-010, recorded in the module tests):
//!
//! - Maven resolves dependencies from Maven Central (or a configured mirror)
//!   into `~/.m2/repository`; those sources live outside the workspace and are
//!   undeclared inputs, so generated actions are `cache_policy: never`
//!   (D-003, D-010) — the same stance the cargo, npm, Go, Python, and Docker
//!   adapters take on registry dependency sources.
//! - The output directory `target/` contains the packaged artifact (e.g.
//!   `myapp-1.0.0.jar`), compiled classes, and intermediate files. The
//!   artifact filename encodes the `artifactId` and `version` from `pom.xml`,
//!   so it is not a fixed name; `target/` is declared as a `Tree` output (like
//!   Python's `dist/`), and the adapter's `artifacts` narrows to the JAR file
//!   when it can be found.
//! - `mvn` is `mvn.cmd` (a batch shim) on Windows and a shell script elsewhere.
//!   On Windows the action runs `cmd /C mvn package -DskipTests` so
//!   `CreateProcess` can launch it (same pattern as the npm adapter); on other
//!   platforms it runs `mvn package -DskipTests` directly.
//! - Toolchain identity is captured from the real `mvn`: the version from
//!   `mvn --version` (which includes the Maven and Java versions) and the
//!   SHA-256 digest of the resolved `mvn` launcher. The `mvn` launcher is
//!   located on PATH; on Windows `locate_executable` finds `mvn.cmd`, on Unix
//!   it finds the `mvn` shell script. Digesting the launcher is truthful: it
//!   is the exact program that runs, and its identity changes when Maven is
//!   upgraded.
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

pub struct MavenAdapter;

const OPTIONS: &[OptionSpec] = &[OptionSpec {
    key: "goal",
    values: "package|verify|compile",
    description: "the Maven build goal; `package` (default) compiles and packages the \
                      artifact into target/ (skips tests), `verify` runs integration tests \
                      too, `compile` only compiles source (no packaging)",
}];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Goal {
    /// `mvn package -DskipTests` — compile, package, skip tests.
    Package,
    /// `mvn verify` — compile, package, run tests (unit + integration).
    Verify,
    /// `mvn compile` — compile source only, no packaging.
    Compile,
}

fn goal_from(options: &AdapterOptions) -> Result<Goal, AdapterError> {
    match options.get("goal") {
        None | Some("package") => Ok(Goal::Package),
        Some("verify") => Ok(Goal::Verify),
        Some("compile") => Ok(Goal::Compile),
        Some(other) => Err(AdapterError::Option(format!(
            "maven goal '{other}' is not supported (supported: package, verify, compile)"
        ))),
    }
}

impl BuildAdapter for MavenAdapter {
    fn name(&self) -> &'static str {
        "maven"
    }

    fn options(&self) -> &'static [OptionSpec] {
        OPTIONS
    }

    fn detect(&self, project_root: &Path) -> Result<Detection, AdapterError> {
        if project_root.join("pom.xml").is_file() {
            Ok(Detection::supported())
        } else {
            Ok(Detection::unsupported(format!(
                "no pom.xml at {}",
                project_root.display()
            )))
        }
    }

    /// The packaged archive(s) Maven wrote to `target/`: `*.jar`, `*.war`,
    /// `*.ear`, without the side artifacts (`-sources`, `-javadoc`,
    /// `-tests`) and the shade plugin's `original-*` copy.
    fn built_artifacts(&self, project_root: &Path, _plan: &PlanFile) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(project_root.join("target")) else {
            return Vec::new();
        };
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| {
                let archive = [".jar", ".war", ".ear"]
                    .iter()
                    .any(|extension| name.ends_with(extension));
                let side = ["-sources.", "-javadoc.", "-tests."]
                    .iter()
                    .any(|marker| name.contains(marker));
                archive && !side && !name.starts_with("original-")
            })
            .map(|name| PathBuf::from("target").join(name))
            .collect();
        found.sort();
        found
    }

    fn generate(
        &self,
        project_root: &Path,
        options: &AdapterOptions,
    ) -> Result<PlanFile, AdapterError> {
        check_option_keys(self, options)?;
        let goal = goal_from(options)?;
        let root = canonical_root(project_root)?;
        // Require a pom.xml: detect's marker check, repeated here so a
        // `--adapter maven` invocation on the wrong project fails at generation
        // with the same honest reason rather than a misleading toolchain
        // error.
        if !root.join("pom.xml").is_file() {
            return Err(AdapterError::Project(format!(
                "no pom.xml at {}",
                root.display()
            )));
        }

        let inputs = declare_inputs(&root);
        let outputs = match goal {
            Goal::Compile => Vec::new(),
            Goal::Package | Goal::Verify => vec![Output {
                path: PathBuf::from("target"),
                kind: OutputKind::Tree,
            }],
        };
        let id = match goal {
            Goal::Package => "maven-package",
            Goal::Verify => "maven-verify",
            Goal::Compile => "maven-compile",
        }
        .to_owned();
        let action = build_action(id, goal, inputs, outputs);
        let mut file = PlanFile::new();
        file.actions.push(PlanAction {
            action,
            dependencies: Vec::new(),
        });
        // Validate the plan shape before the toolchain digest and before any
        // side effect.
        file.to_graph()?;
        let toolchain = maven_toolchain(&root)?;
        for entry in &mut file.actions {
            entry.action.toolchain = Some(toolchain.clone());
        }
        file.to_graph()?;
        std::fs::create_dir_all(root.join(".ant").join("tmp"))?;
        Ok(file)
    }
}

/// Declare the workspace inputs Ant can honestly cover: `pom.xml` (the project
/// manifest), `src/` (the source tree), and `settings.xml` (a local Maven
/// settings file) when present. The Maven local repository
/// (`~/.m2/repository`) is outside the workspace and intentionally not declared
/// (see the module docs).
fn declare_inputs(root: &Path) -> BTreeMap<PathBuf, InputKind> {
    let mut inputs = BTreeMap::new();
    inputs.insert(PathBuf::from("pom.xml"), InputKind::File);
    if root.join("src").is_dir() {
        inputs.insert(PathBuf::from("src"), InputKind::Tree);
    }
    if root.join("settings.xml").is_file() {
        inputs.insert(PathBuf::from("settings.xml"), InputKind::File);
    }
    inputs
}

/// `mvn package -DskipTests` / `mvn verify` / `mvn compile`, wrapped in
/// `cmd /C` on Windows because `mvn` is `mvn.cmd` (a batch shim, like npm).
fn maven_argv(goal: Goal) -> Vec<String> {
    let mvn_args: Vec<String> = match goal {
        Goal::Package => vec!["package".to_owned(), "-DskipTests".to_owned()],
        Goal::Verify => vec!["verify".to_owned()],
        Goal::Compile => vec!["compile".to_owned()],
    };
    if cfg!(windows) {
        let mut argv = vec!["cmd".to_owned(), "/C".to_owned(), "mvn".to_owned()];
        argv.extend(mvn_args);
        argv
    } else {
        let mut argv = vec!["mvn".to_owned()];
        argv.extend(mvn_args);
        argv
    }
}

/// `TMP`/`TEMP` (plus `TMPDIR` off Windows) pointing at `.ant/tmp` relative to
/// the action's working directory (the shared staging pattern).
fn staging_environment() -> Vec<(String, String)> {
    let staging_value = PathBuf::from(".ant")
        .join("tmp")
        .to_string_lossy()
        .into_owned();
    let mut environment = vec![
        ("TMP".to_owned(), staging_value.clone()),
        ("TEMP".to_owned(), staging_value.clone()),
    ];
    if cfg!(not(windows)) {
        environment.push(("TMPDIR".to_owned(), staging_value));
    }
    environment.sort();
    environment
}

fn build_action(
    id: String,
    goal: Goal,
    inputs: BTreeMap<PathBuf, InputKind>,
    outputs: Vec<Output>,
) -> Action {
    Action {
        schema_version: 1,
        id,
        argv: maven_argv(goal),
        working_directory: PathBuf::from("."),
        inputs: inputs
            .into_iter()
            .map(|(path, kind)| Input { path, kind })
            .collect(),
        outputs,
        environment: staging_environment(),
        // Maven dependencies are downloaded from Maven Central (or a mirror)
        // into ~/.m2/repository — undeclared inputs outside the workspace — so
        // these actions never claim shared cache eligibility (D-003, D-010).
        cache_policy: CachePolicy::Never,
        toolchain: None,
        timeout_ms: None,
        resources: None,
        background_services: false,
    }
}

/// Truthful Maven toolchain identity: the version from `mvn --version` (which
/// includes the Maven and Java versions) and the SHA-256 digest of the
/// resolved `mvn` launcher. On Windows `mvn` is `mvn.cmd`; on Unix it is a
/// shell script. `executable_version` runs `mvn --version` in the project root.
fn maven_toolchain(root: &Path) -> Result<ToolchainIdentity, AdapterError> {
    // On Windows the launcher is `mvn.cmd`; on Unix it is `mvn`. The `cmd /C`
    // wrapper in the action handles Windows execution, but for toolchain
    // capture we locate and digest the launcher directly. `mvn --version`
    // works on all platforms (the launcher accepts `--version`).
    let launcher_name = if cfg!(windows) { "mvn.cmd" } else { "mvn" };
    let mvn = locate_executable(launcher_name)?;
    let digest_target = mvn.canonicalize().map_err(|error| {
        AdapterError::Toolchain(format!(
            "cannot resolve maven launcher {}: {error}",
            mvn.display()
        ))
    })?;
    let version = executable_version(&mvn, root)?;
    // `mvn --version` prints multiple lines (Maven, Java, OS, …); the full
    // trimmed output is the version string so the Java version is captured
    // too — a JDK upgrade changes the toolchain identity.
    let version = version.lines().next().unwrap_or_default().trim().to_owned();
    let executable_digest = crate::toolchain_digest(&digest_target)
        .map_err(|error| {
            AdapterError::Toolchain(format!(
                "cannot digest maven launcher {}: {error}",
                digest_target.display()
            ))
        })?
        .to_string();
    Ok(ToolchainIdentity {
        name: "maven".to_owned(),
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
                "ant-maven-adapter-{name}-{}-{unique}",
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

    fn pom_xml() -> &'static str {
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <project>\n  <modelVersion>4.0.0</modelVersion>\n  \
         <groupId>com.example</groupId>\n  <artifactId>myapp</artifactId>\n  \
         <version>1.0.0</version>\n</project>\n"
    }

    #[test]
    fn detect_recognizes_pom_and_rejects_others() {
        let adapter = MavenAdapter;
        let dir = TempDir::new("detect");
        write(&dir.path().join("pom.xml"), pom_xml());
        assert!(adapter.detect(dir.path()).expect("detect").supported);

        let empty = TempDir::new("empty");
        let detection = adapter.detect(empty.path()).expect("detect");
        assert!(!detection.supported);
        assert!(
            detection.reason.contains("no pom.xml"),
            "{}",
            detection.reason
        );
    }

    #[test]
    fn goal_rejects_unknown_values() {
        let adapter = MavenAdapter;
        let dir = TempDir::new("goal");
        write(&dir.path().join("pom.xml"), pom_xml());
        let error = adapter
            .generate(dir.path(), &options(&[("goal", "install")]))
            .expect_err("bad goal must fail");
        assert!(
            error
                .to_string()
                .contains("supported: package, verify, compile"),
            "{}",
            error
        );
    }

    #[test]
    fn generate_rejects_non_maven_project() {
        let adapter = MavenAdapter;
        let dir = TempDir::new("not-maven");
        write(&dir.path().join("README.md"), "not a maven project");
        let error = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect_err("must fail");
        assert!(error.to_string().contains("no pom.xml"), "{}", error);
    }

    #[test]
    fn unknown_option_rejected() {
        let adapter = MavenAdapter;
        let dir = TempDir::new("unknown");
        write(&dir.path().join("pom.xml"), pom_xml());
        let error = adapter
            .generate(dir.path(), &options(&[("profile", "prod")]))
            .expect_err("unknown option must fail");
        assert!(
            error
                .to_string()
                .contains("does not accept option 'profile'"),
            "{}",
            error
        );
    }

    #[test]
    fn pom_xml_and_src_declared_as_inputs() {
        let adapter = MavenAdapter;
        let dir = TempDir::new("inputs");
        write(&dir.path().join("pom.xml"), pom_xml());
        write(
            &dir.path().join("src").join("Main.java"),
            "public class Main {}\n",
        );
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        let input_paths: Vec<&Path> = action.inputs.iter().map(|i| i.path.as_path()).collect();
        assert!(input_paths.contains(&Path::new("pom.xml")));
        assert!(input_paths.contains(&Path::new("src")));
    }

    #[test]
    fn settings_xml_declared_when_present() {
        let adapter = MavenAdapter;
        let dir = TempDir::new("settings");
        write(&dir.path().join("pom.xml"), pom_xml());
        write(&dir.path().join("settings.xml"), "<settings/>\n");
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert!(
            action
                .inputs
                .iter()
                .any(|i| i.path.as_path() == Path::new("settings.xml"))
        );
    }

    #[test]
    fn src_absent_not_declared() {
        let adapter = MavenAdapter;
        let dir = TempDir::new("no-src");
        write(&dir.path().join("pom.xml"), pom_xml());
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert!(
            !action
                .inputs
                .iter()
                .any(|i| i.path.as_path() == Path::new("src"))
        );
    }

    #[test]
    fn package_goal_declares_target_tree_output() {
        let adapter = MavenAdapter;
        let dir = TempDir::new("package");
        write(&dir.path().join("pom.xml"), pom_xml());
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert_eq!(action.outputs.len(), 1);
        assert_eq!(action.outputs[0].path, PathBuf::from("target"));
        assert_eq!(action.outputs[0].kind, OutputKind::Tree);
    }

    #[test]
    fn verify_goal_declares_target_tree_output() {
        let adapter = MavenAdapter;
        let dir = TempDir::new("verify");
        write(&dir.path().join("pom.xml"), pom_xml());
        let plan = adapter
            .generate(dir.path(), &options(&[("goal", "verify")]))
            .expect("plan");
        let action = &plan.actions[0].action;
        assert_eq!(action.outputs.len(), 1);
        assert_eq!(action.outputs[0].path, PathBuf::from("target"));
    }

    #[test]
    fn compile_goal_has_no_outputs() {
        let adapter = MavenAdapter;
        let dir = TempDir::new("compile");
        write(&dir.path().join("pom.xml"), pom_xml());
        let plan = adapter
            .generate(dir.path(), &options(&[("goal", "compile")]))
            .expect("plan");
        let action = &plan.actions[0].action;
        assert!(action.outputs.is_empty());
    }

    #[test]
    fn package_argv_includes_skip_tests() {
        let adapter = MavenAdapter;
        let dir = TempDir::new("argv");
        write(&dir.path().join("pom.xml"), pom_xml());
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert!(action.argv.iter().any(|a| a == "package"));
        assert!(action.argv.iter().any(|a| a == "-DskipTests"));
    }

    #[test]
    fn verify_argv_does_not_skip_tests() {
        let adapter = MavenAdapter;
        let dir = TempDir::new("argv-verify");
        write(&dir.path().join("pom.xml"), pom_xml());
        let plan = adapter
            .generate(dir.path(), &options(&[("goal", "verify")]))
            .expect("plan");
        let action = &plan.actions[0].action;
        assert!(action.argv.iter().any(|a| a == "verify"));
        assert!(!action.argv.iter().any(|a| a == "-DskipTests"));
    }

    #[test]
    fn compile_argv_is_just_compile() {
        let adapter = MavenAdapter;
        let dir = TempDir::new("argv-compile");
        write(&dir.path().join("pom.xml"), pom_xml());
        let plan = adapter
            .generate(dir.path(), &options(&[("goal", "compile")]))
            .expect("plan");
        let action = &plan.actions[0].action;
        assert!(action.argv.iter().any(|a| a == "compile"));
        assert!(!action.argv.iter().any(|a| a == "-DskipTests"));
        assert!(!action.argv.iter().any(|a| a == "package"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_argv_uses_cmd_c() {
        let adapter = MavenAdapter;
        let dir = TempDir::new("win");
        write(&dir.path().join("pom.xml"), pom_xml());
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert_eq!(action.argv[0], "cmd");
        assert_eq!(action.argv[1], "/C");
        assert_eq!(action.argv[2], "mvn");
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_argv_runs_mvn_directly() {
        let adapter = MavenAdapter;
        let dir = TempDir::new("unix");
        write(&dir.path().join("pom.xml"), pom_xml());
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert_eq!(action.argv[0], "mvn");
    }

    #[test]
    fn cache_policy_is_never() {
        let adapter = MavenAdapter;
        let dir = TempDir::new("cache");
        write(&dir.path().join("pom.xml"), pom_xml());
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        let action = &plan.actions[0].action;
        assert_eq!(action.cache_policy, CachePolicy::Never);
    }

    #[test]
    fn plan_has_exactly_one_action() {
        let adapter = MavenAdapter;
        let dir = TempDir::new("one");
        write(&dir.path().join("pom.xml"), pom_xml());
        let plan = adapter
            .generate(dir.path(), &AdapterOptions::new())
            .expect("plan");
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.actions[0].action.id, "maven-package");
        assert!(plan.actions[0].dependencies.is_empty());
    }
}

#[cfg(test)]
mod artifact_tests {
    use super::*;

    #[test]
    fn built_artifacts_are_the_packaged_archives_only() {
        let dir = std::env::temp_dir().join(format!("ant-maven-artifacts-{}", std::process::id()));
        let target = dir.join("target");
        std::fs::create_dir_all(target.join("classes")).expect("dirs");
        for name in [
            "app-1.0.jar",
            "app-1.0-sources.jar",
            "app-1.0-javadoc.jar",
            "original-app-1.0.jar",
            "web.war",
            "notes.txt",
        ] {
            std::fs::write(target.join(name), b"x").expect("file");
        }
        let found = MavenAdapter.built_artifacts(&dir, &PlanFile::new());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            found,
            vec![
                PathBuf::from("target").join("app-1.0.jar"),
                PathBuf::from("target").join("web.war")
            ]
        );
    }
}
