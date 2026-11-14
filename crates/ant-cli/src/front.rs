//! The product front door (roadmap phase P1, decision D-016): `ant init` and
//! `ant build`. Everything else in the CLI is engine plumbing; these two
//! commands are what a developer uses day to day:
//!
//! - `ant build` needs no configuration: it detects the project, generates
//!   the plan in memory (saved to `.ant/plan.json` for transparency), runs it
//!   on all cores with human-friendly progress, and collects the project's
//!   deliverables into `dist/`. Release builds by default; `--dev` for debug.
//! - `ant init` records the detected setup in a small, commented `ant.toml`
//!   so builds are reproducible across machines, and keeps Ant's working
//!   directories out of git.

use crate::scheduler::{self, Capacity, EventStream, RunOptions};
use crate::serve;
use crate::signing;
use build_adapter::{AdapterOptions, BuildAdapter};
use build_events::{ActionStatus, Event, EventKind, EventSink};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

pub const CONFIG_FILE: &str = "ant.toml";
const DEFAULT_OUT_DIR: &str = "dist";
const ARTIFACT_MARKER: &str = ".ant-artifacts";

/// `ant.toml`. Every field is optional; unknown keys are rejected so typos
/// fail loudly instead of being ignored.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// Adapter name (`cargo`, `npm`, `cmake`, `gradle`, `flutter`, `xcode`, `go`, `python`); auto-detected when omitted.
    pub adapter: Option<String>,
    /// Where `ant build` collects deliverables (workspace-relative).
    pub out_dir: Option<String>,
    /// Adapter options (see `ant init` output for what each adapter accepts).
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

/// A path as users expect to read it: without the `\\?\` verbatim prefix
/// Windows canonicalization adds.
fn shown(path: &Path) -> String {
    let text = path.display().to_string();
    text.strip_prefix(r"\\?\")
        .map(str::to_owned)
        .unwrap_or(text)
}

pub fn load_config(root: &Path) -> Result<Option<ProjectConfig>, String> {
    let path = root.join(CONFIG_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read {CONFIG_FILE}: {error}")),
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|error| format!("invalid {CONFIG_FILE}: {error}"))
}

fn project_root(arg: Option<&str>) -> Result<PathBuf, String> {
    let path = match arg {
        Some(path) => PathBuf::from(path),
        None => std::env::current_dir().map_err(|error| error.to_string())?,
    };
    path.canonicalize()
        .map_err(|error| format!("cannot access project {}: {error}", path.display()))
}

fn choose_adapter(
    root: &Path,
    configured: Option<&str>,
) -> Result<&'static dyn BuildAdapter, String> {
    if let Some(name) = configured {
        return build_adapter::adapter(name).ok_or_else(|| {
            format!(
                "unknown adapter '{name}' in {CONFIG_FILE}; available: {}",
                adapter_names()
            )
        });
    }
    build_adapter::select_adapter(root).map_err(|error| match error {
        build_adapter::SelectionError::Ambiguous(names) => format!(
            "this project matches several build systems ({}). Pick one with: ant init --adapter {}",
            names.join(", "),
            names[0]
        ),
        build_adapter::SelectionError::NoMatch(_) => format!(
            "no supported project found in {} (Ant currently builds Cargo, npm, CMake, \
             Gradle, Flutter, Xcode, Go, and Python projects; see docs/vision.md for what is coming)",
            shown(root)
        ),
        other => other.to_string(),
    })
}

fn adapter_names() -> String {
    build_adapter::available()
        .iter()
        .map(|adapter| adapter.name())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Release-by-default build options: `profile` (cargo) or `config` (CMake),
/// then `ant.toml` options, then `--dev`.
fn build_options(
    adapter: &dyn BuildAdapter,
    configured: &BTreeMap<String, String>,
    dev: bool,
    release: bool,
) -> Result<(AdapterOptions, &'static str), String> {
    let accepts = |key: &str| adapter.options().iter().any(|spec| spec.key == key);
    let mut chosen: BTreeMap<String, String> = BTreeMap::new();
    if accepts("profile") {
        chosen.insert("profile".into(), "release".into());
    }
    if accepts("config") {
        chosen.insert("config".into(), "Release".into());
    }
    // One `cargo build --workspace` instead of one invocation per member:
    // cargo schedules crates in parallel itself, and a no-change build pays
    // cargo's startup once, not once per crate (D-027).
    if accepts("granularity") {
        chosen.insert("granularity".into(), "workspace".into());
    }
    // Local builds keep the adapter's build daemon (Gradle's) warm between
    // invocations: the daemon changes no declared output, only the time the
    // next build spends starting it. `ant.toml` can override this to "0" for
    // a hermetic one-shot daemon; `ant adapt` plans (remote and sandbox runs)
    // always default to the hermetic variant.
    if accepts("daemon") {
        chosen.insert("daemon".into(), "1".into());
    }
    // Android (gradle): debug by default, the explicit --dev/--release flags
    // beat ant.toml's `task`, which itself beats the adapter default.
    if accepts("task") {
        chosen.insert("task".into(), "assembleDebug".into());
    }
    // Flutter: release by default; --dev selects debug, --release restores
    // release, and ant.toml's `mode` sits between (same precedence as task).
    if accepts("mode") {
        chosen.insert("mode".into(), "release".into());
    }
    // Xcode (iOS): Release by default (like cargo/cmake); --dev selects Debug,
    // --release restores Release, ant.toml's `configuration` sits between.
    if accepts("configuration") {
        chosen.insert("configuration".into(), "Release".into());
    }
    for (key, value) in configured {
        chosen.insert(key.clone(), value.clone());
    }
    if dev {
        if accepts("profile") {
            chosen.insert("profile".into(), "dev".into());
        }
        if accepts("config") {
            chosen.insert("config".into(), "Debug".into());
        }
        if accepts("task") {
            chosen.insert("task".into(), "assembleDebug".into());
        }
        if accepts("mode") {
            chosen.insert("mode".into(), "debug".into());
        }
        if accepts("configuration") {
            chosen.insert("configuration".into(), "Debug".into());
        }
    }
    if release {
        if accepts("profile") {
            chosen.insert("profile".into(), "release".into());
        }
        if accepts("config") {
            chosen.insert("config".into(), "Release".into());
        }
        if accepts("task") {
            chosen.insert("task".into(), "assembleRelease".into());
        }
        if accepts("mode") {
            chosen.insert("mode".into(), "release".into());
        }
        if accepts("configuration") {
            chosen.insert("configuration".into(), "Release".into());
        }
    }
    let flavor = match (
        chosen.get("profile"),
        chosen.get("config"),
        chosen.get("task"),
        chosen.get("mode"),
        chosen.get("configuration"),
    ) {
        (Some(profile), _, _, _, _) if profile == "dev" => "debug",
        (_, Some(config), _, _, _) if config == "Debug" => "debug",
        (_, _, Some(task), _, _) if build_adapter::gradle::release_task(task) => "release",
        // `assembleDebug` is Gradle's default build; like the plain npm build it
        // shows no flavor label, so the summary stays `ant build · gradle ·
        // 1 step`. Only the release task (assembleRelease/bundleRelease) labels
        // the build, so a release run is obvious at a glance.
        (_, _, Some(task), _, _) if task == "assembleDebug" => "default",
        // Flutter's build mode is a real choice (like cargo's profile), so it is
        // always labeled: release (the default), debug, or profile.
        (_, _, _, Some(mode), _) if mode == "debug" => "debug",
        (_, _, _, Some(mode), _) if mode == "profile" => "profile",
        (_, _, _, Some(mode), _) if mode == "release" => "release",
        // Xcode's configuration is a real choice (Debug/Release), labeled like
        // the others; Release is the default.
        (_, _, _, _, Some(config)) if config == "Debug" => "debug",
        (_, _, _, _, Some(config)) if config == "Release" => "release",
        (Some(_), _, _, _, _) | (_, Some(_), _, _, _) => "release",
        _ => "default",
    };
    let mut options = AdapterOptions::new();
    for (key, value) in &chosen {
        options
            .insert(key, value)
            .map_err(|error| error.to_string())?;
    }
    Ok((options, flavor))
}

/// Accept only a plain relative directory (no `..`, no absolute paths).
fn safe_relative_dir(value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if value.trim().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(format!(
            "out_dir '{value}' must be a relative path inside the project"
        ));
    }
    Ok(path)
}

// ---------------------------------------------------------------------------
// ant build
// ---------------------------------------------------------------------------

const BUILD_USAGE: &str = "usage: ant build [--dev|--release] [--fresh] [--no-serve] \
    [android] [ios] [--project PATH] [--jobs N] [--verbose]";

pub fn build_command(args: impl Iterator<Item = OsString>) -> Result<u8, String> {
    let mut project = None;
    let mut dev = false;
    let mut release = false;
    let mut verbose = false;
    let mut fresh = false;
    let mut no_serve = false;
    let mut android = false;
    let mut ios = false;
    let mut jobs = None;
    let mut args = args.map(|arg| arg.to_string_lossy().into_owned());
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dev" => dev = true,
            "--release" => release = true,
            "--no-serve" => no_serve = true,
            "--verbose" => verbose = true,
            "--fresh" => fresh = true,
            "android" => android = true,
            "ios" => ios = true,
            "--project" => project = Some(args.next().ok_or("--project requires a path")?),
            "--jobs" => {
                jobs = Some(
                    args.next()
                        .and_then(|v| v.parse::<usize>().ok())
                        .filter(|n| *n > 0)
                        .ok_or("--jobs requires a positive integer")?,
                )
            }
            target if !target.starts_with('-') => {
                return Err(format!(
                    "build target '{target}' is not supported yet: today 'android' and 'ios' are the \
                     named targets (ios needs Xcode on a Mac, docs/plan.md)"
                ));
            }
            other => return Err(format!("unexpected argument '{other}'\n{BUILD_USAGE}")),
        }
    }
    if dev && release {
        return Err("--dev and --release are exclusive".to_owned());
    }
    let root = project_root(project.as_deref())?;
    let config = load_config(&root)?.unwrap_or_default();
    // The `android` target is Gradle-only and shipped in roadmap phase P4
    // (docs/plan.md). Asking for it on a project Gradle did not detect —
    // including an ambiguous cargo/npm one, which would otherwise report a
    // generic adapter pick — says so before adapter selection, so the user
    // learns where Android lives instead of a misleading adapter prompt.
    if android {
        let gradle_match = build_adapter::detect_all(&root).into_iter().any(|report| {
            report.adapter == "gradle" && report.outcome.as_ref().is_ok_and(|d| d.supported)
        });
        if !gradle_match {
            return Err(
                "build target 'android' needs a Gradle/Android project; Ant's Android support \
                 shipped in roadmap phase P4 (docs/plan.md), but Gradle did not detect this \
                 project. Run `ant build` for the detected project, or `ant init --adapter gradle` \
                 only on an Android project."
                    .to_string(),
            );
        }
    }
    // The `ios` target is Xcode-only and ships in roadmap phase P5
    // (docs/plan.md). Like `android`, asking for it on a project Xcode did not
    // detect says so before adapter selection, so the user learns where iOS
    // lives instead of a misleading adapter prompt.
    if ios {
        let xcode_match = build_adapter::detect_all(&root).into_iter().any(|report| {
            report.adapter == "xcode" && report.outcome.as_ref().is_ok_and(|d| d.supported)
        });
        if !xcode_match {
            return Err(
                "build target 'ios' needs an Xcode project (a *.xcodeproj or *.xcworkspace); \
                 Ant's iOS support shipped in roadmap phase P5 (docs/plan.md), but Xcode did not \
                 detect this project. iOS builds also need Xcode on a Mac. Run `ant build` for the \
                 detected project, or `ant init --adapter xcode` only on an iOS project."
                    .to_owned(),
            );
        }
    }
    let adapter = choose_adapter(&root, config.adapter.as_deref())?;
    if android && adapter.name() != "gradle" {
        return Err(format!(
            "build target 'android': {} detected this project, and it is not an Android one",
            adapter.name()
        ));
    }
    if ios && adapter.name() != "xcode" {
        return Err(format!(
            "build target 'ios': {} detected this project, and it is not an iOS (Xcode) one",
            adapter.name()
        ));
    }
    let (mut options, flavor) = build_options(adapter, &config.options, dev, release)?;
    // Release signing (P4, D-036): on a Gradle release task with a
    // registered keystore, the plan references the keystore by digest (the
    // `sign` option) and the front door resolves the secrets now. Without a
    // keystore the build is simply unsigned — Android debug builds need no
    // signing, and release builds fail at Gradle's own unsigned check.
    let sign_entry = if adapter.name() == "gradle"
        && build_adapter::gradle::release_task(options.get("task").unwrap_or("assembleDebug"))
    {
        signing::registered(&root)
    } else {
        None
    };
    if let Some(entry) = &sign_entry {
        options
            .insert("sign", entry.keystore_digest.as_str())
            .map_err(|error| error.to_string())?;
    }
    let out_dir = safe_relative_dir(config.out_dir.as_deref().unwrap_or(DEFAULT_OUT_DIR))?;
    let jobs = jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
    });

    let started = Instant::now();
    let plan = adapter
        .generate(&root, &options)
        .map_err(|error| format!("{} could not plan this project: {error}", adapter.name()))?;
    let graph = plan.to_graph().map_err(|error| error.to_string())?;
    let plan_json = serde_json::to_string_pretty(&plan).map_err(|error| error.to_string())?;
    let ant_dir = root.join(".ant");
    std::fs::create_dir_all(&ant_dir).map_err(|error| error.to_string())?;
    std::fs::write(ant_dir.join("plan.json"), plan_json).map_err(|error| error.to_string())?;

    let count = plan.actions.len();
    // Adapters without a release/debug notion (npm) show no flavor at all.
    let flavor = if flavor == "default" {
        String::new()
    } else {
        format!(" · {flavor}")
    };
    println!(
        "ant build · {}{flavor} · {count} step{} · {jobs} parallel",
        adapter.name(),
        if count == 1 { "" } else { "s" }
    );
    scheduler::set_machine_output(verbose);
    let cancellation = local_executor::CancellationToken::new();
    crate::install_ctrl_c_handler(&cancellation);
    let events = std::sync::Mutex::new(EventStream::with_sink(
        Box::new(HumanReporter::default()),
        crate::new_run_id(),
    ));
    let run_options = RunOptions {
        capacity: Capacity {
            cpu: jobs as u64,
            memory_mb: None,
        },
        keep_going: false,
        exec: crate::cache_runner::ExecConfig {
            sandbox: false,
            enforce_memory: false,
            isolated: None,
            // Reuse this machine's own earlier results (D-028); `--fresh`
            // rebuilds and refreshes them.
            local_reuse: true,
            fresh,
        },
        // Build-time signing secrets (D-036), resolved before the run:
        // staging happens even for a cached build (the plan is generated
        // first), but the values only ever reach the spawned process.
        secrets: match &sign_entry {
            Some(entry) => {
                signing::ensure_init_script(&root)?;
                Some(Arc::new(signing::unlock(&root, entry)?))
            }
            None => None,
        },
    };
    let results = build_adapter::user_cache_dir().map(|dir| dir.join("local-results"));
    let code = scheduler::run_plan(
        &graph,
        &root,
        &cancellation,
        run_options,
        results.as_deref(),
        &events,
    )?;
    let elapsed = started.elapsed().as_secs_f64();
    if code != 0 {
        println!("\nBuild failed after {elapsed:.1}s. Re-run with --verbose for engine details.");
        return Ok(code);
    }
    let installed = install_artifacts(&root, &out_dir, &adapter.artifacts(&plan))?;
    if installed.is_empty() {
        println!(
            "\nBuilt in {elapsed:.1}s. {}",
            match adapter.name() {
                "npm" =>
                    "Your build script wrote its output in place (set `outputs = \"entry-points\"` under [options] in ant.toml to have Ant verify and collect it).",
                "cmake" =>
                    "Build products are in build/ (Ant does not yet know CMake's target list, so nothing was collected).",
                _ => "This project declares no deliverables to collect.",
            }
        );
    } else {
        let mut list = String::new();
        for path in &installed {
            let _ = write!(list, "\n  {}", path.display());
        }
        println!("\nBuilt in {elapsed:.1}s →{list}");
    }
    // Install-by-QR (D-037): a release build that collected APKs serves
    // them on the LAN until Ctrl+C. Piped stdout cannot show a QR and
    // --no-serve opts out; both simply skip serving and exit.
    let task = options.get("task").unwrap_or("assembleDebug").to_owned();
    if !no_serve
        && adapter.name() == "gradle"
        && build_adapter::gradle::release_task(&task)
        && std::io::IsTerminal::is_terminal(&std::io::stdout())
        && !installed.is_empty()
    {
        let absolute: Vec<PathBuf> = installed.iter().map(|path| root.join(path)).collect();
        return serve::serve_installed(&root, &absolute, &cancellation);
    }
    Ok(0)
}

/// Copy deliverables into `out_dir` (flat), replacing what the previous
/// `ant build` put there. Refuses to touch a non-empty directory Ant did not
/// create, so user files are never overwritten.
fn install_artifacts(
    root: &Path,
    out_dir: &Path,
    artifacts: &[PathBuf],
) -> Result<Vec<PathBuf>, String> {
    if artifacts.is_empty() {
        return Ok(Vec::new());
    }
    let out = root.join(out_dir);
    let marker = out.join(ARTIFACT_MARKER);
    if out.exists() && !marker.exists() {
        let non_empty = std::fs::read_dir(&out)
            .map_err(|error| error.to_string())?
            .next()
            .is_some();
        if non_empty {
            return Err(format!(
                "{} already exists and was not created by Ant; set out_dir in {CONFIG_FILE} to another directory",
                out_dir.display()
            ));
        }
    }
    std::fs::create_dir_all(&out).map_err(|error| error.to_string())?;
    if let Ok(previous) = std::fs::read_to_string(&marker) {
        for name in previous.lines().filter(|line| !line.is_empty()) {
            let stale = Path::new(name);
            if stale.components().count() == 1 {
                let _ = std::fs::remove_file(out.join(stale));
            }
        }
    }
    let mut seen = HashMap::new();
    let mut installed = Vec::new();
    for artifact in artifacts {
        let name = artifact
            .file_name()
            .ok_or_else(|| format!("artifact {} has no file name", artifact.display()))?;
        if let Some(previous) = seen.insert(name.to_owned(), artifact.clone()) {
            return Err(format!(
                "artifacts {} and {} would collide in {}",
                previous.display(),
                artifact.display(),
                out_dir.display()
            ));
        }
        std::fs::copy(root.join(artifact), out.join(name))
            .map_err(|error| format!("cannot collect {}: {error}", artifact.display()))?;
        installed.push(out_dir.join(name));
    }
    let listing: String = seen
        .keys()
        .map(|name| format!("{}\n", name.to_string_lossy()))
        .collect();
    std::fs::write(&marker, listing).map_err(|error| error.to_string())?;
    Ok(installed)
}

/// Renders engine events as short human progress lines.
#[derive(Default)]
struct HumanReporter {
    started: HashMap<String, Instant>,
}

impl EventSink for HumanReporter {
    fn emit(&mut self, event: Event) -> std::io::Result<()> {
        match event.kind {
            EventKind::ActionStarted { action_id } => {
                self.started.insert(action_id, Instant::now());
            }
            EventKind::ActionFinished {
                action_id,
                status,
                exit_code,
                error,
                ..
            } => {
                let took = self
                    .started
                    .remove(&action_id)
                    .map(|start| format!("{:.1}s", start.elapsed().as_secs_f64()))
                    .unwrap_or_default();
                let line = match status {
                    ActionStatus::Succeeded => format!("  ✓ {action_id}  {took}"),
                    ActionStatus::Reused => format!("  ✓ {action_id}  reused from cache"),
                    ActionStatus::Failed => match (exit_code, error) {
                        (_, Some(error)) => format!("  ✗ {action_id}  {error}"),
                        (Some(code), None) => format!("  ✗ {action_id}  exited with code {code}"),
                        (None, None) => format!("  ✗ {action_id}  failed"),
                    },
                    ActionStatus::TimedOut => format!("  ✗ {action_id}  timed out after {took}"),
                    ActionStatus::Cancelled => format!("  ✗ {action_id}  cancelled"),
                };
                println!("{line}");
            }
            EventKind::ActionSkipped { action_id } => {
                println!("  – {action_id}  skipped (an earlier step failed)");
            }
            EventKind::RunStarted { .. } | EventKind::RunFinished { .. } => {}
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ant init
// ---------------------------------------------------------------------------

const INIT_USAGE: &str = "usage: ant init [--adapter NAME] [--project PATH] [--force]";

pub fn init_command(args: impl Iterator<Item = OsString>) -> Result<u8, String> {
    let mut project = None;
    let mut adapter_name = None;
    let mut force = false;
    let mut args = args.map(|arg| arg.to_string_lossy().into_owned());
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--project" => project = Some(args.next().ok_or("--project requires a path")?),
            "--adapter" => adapter_name = Some(args.next().ok_or("--adapter requires a name")?),
            "--force" => force = true,
            other => return Err(format!("unexpected argument '{other}'\n{INIT_USAGE}")),
        }
    }
    let root = project_root(project.as_deref())?;
    let config_path = root.join(CONFIG_FILE);
    if config_path.exists() && !force {
        return Err(format!(
            "{CONFIG_FILE} already exists in {} (use --force to overwrite it)",
            shown(&root)
        ));
    }
    let adapter = choose_adapter(&root, adapter_name.as_deref())?;
    if adapter_name.is_some() {
        let detection = adapter.detect(&root).map_err(|error| error.to_string())?;
        if !detection.supported {
            return Err(format!(
                "the {} adapter does not recognize this project: {}",
                adapter.name(),
                detection.reason
            ));
        }
    }
    let (_, flavor) = build_options(adapter, &BTreeMap::new(), false, false)?;
    std::fs::write(&config_path, render_config(adapter))
        .map_err(|error| format!("cannot write {}: {error}", config_path.display()))?;
    println!(
        "Created {CONFIG_FILE} for a {} project ({flavor} builds by default).",
        adapter.name()
    );
    if let Some(added) = update_gitignore(&root)? {
        println!("Added {added} to .gitignore.");
    }
    println!("Next: run `ant build`.");
    Ok(0)
}

fn render_config(adapter: &dyn BuildAdapter) -> String {
    let mut text = String::from(
        "# Ant project configuration. `ant build` also works without this file;\n\
         # it records choices so every machine builds the project the same way.\n\n",
    );
    let _ = writeln!(text, "adapter = \"{}\"", adapter.name());
    let _ = writeln!(
        text,
        "out_dir = \"{DEFAULT_OUT_DIR}\"   # where `ant build` collects deliverables"
    );
    if !adapter.options().is_empty() {
        text.push_str("\n[options]\n");
        for spec in adapter.options() {
            let default = match spec.key {
                "profile" => Some("release"),
                "config" => Some("Release"),
                _ => None,
            };
            let _ = writeln!(
                text,
                "# {} ({}): {}",
                spec.key, spec.values, spec.description
            );
            match default {
                Some(value) => {
                    let _ = writeln!(text, "{} = \"{value}\"", spec.key);
                }
                None => {
                    let first = spec.values.split('|').next().unwrap_or("");
                    let _ = writeln!(text, "# {} = \"{first}\"", spec.key);
                }
            }
        }
    }
    text
}

/// Add `.ant/` and the output directory to `.gitignore` when the project
/// uses git. Returns what was added, if anything.
fn update_gitignore(root: &Path) -> Result<Option<String>, String> {
    let path = root.join(".gitignore");
    if !root.join(".git").exists() && !path.exists() {
        return Ok(None);
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let present = |entry: &str| {
        existing.lines().map(str::trim).any(|line| {
            line == entry || line == entry.trim_end_matches('/') || line == format!("/{entry}")
        })
    };
    let wanted = [".ant/", "dist/"];
    let missing: Vec<&str> = wanted
        .iter()
        .copied()
        .filter(|entry| !present(entry))
        .collect();
    if missing.is_empty() {
        return Ok(None);
    }
    let mut text = existing.clone();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("# Ant\n");
    for entry in &missing {
        text.push_str(entry);
        text.push('\n');
    }
    std::fs::write(&path, text).map_err(|error| format!("cannot update .gitignore: {error}"))?;
    Ok(Some(missing.join(" and ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_rejects_unknown_keys_and_parses_options() {
        let config: ProjectConfig = toml::from_str(
            "adapter = \"cargo\"\nout_dir = \"out\"\n[options]\nprofile = \"dev\"\n",
        )
        .expect("parse");
        assert_eq!(config.adapter.as_deref(), Some("cargo"));
        assert_eq!(
            config.options.get("profile").map(String::as_str),
            Some("dev")
        );
        assert!(
            toml::from_str::<ProjectConfig>("adaptr = \"cargo\"").is_err(),
            "typo rejected"
        );
    }

    #[test]
    fn out_dir_must_stay_inside_the_project() {
        assert!(safe_relative_dir("dist").is_ok());
        assert!(safe_relative_dir("build/out").is_ok());
        for bad in ["", "../dist", "/tmp/x", "a/../../b"] {
            assert!(safe_relative_dir(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn release_is_default_and_dev_overrides() {
        let cargo = build_adapter::adapter("cargo").expect("cargo");
        let (options, flavor) =
            build_options(cargo, &BTreeMap::new(), false, false).expect("options");
        assert_eq!(
            (options.get("profile"), flavor),
            (Some("release"), "release")
        );
        let (options, flavor) =
            build_options(cargo, &BTreeMap::new(), true, false).expect("options");
        assert_eq!((options.get("profile"), flavor), (Some("dev"), "debug"));
        let cmake = build_adapter::adapter("cmake").expect("cmake");
        let (options, _) = build_options(cmake, &BTreeMap::new(), false, false).expect("options");
        assert_eq!(options.get("config"), Some("Release"));
        let npm = build_adapter::adapter("npm").expect("npm");
        let (options, flavor) = build_options(npm, &BTreeMap::new(), true, false).expect("options");
        assert!(options.is_empty());
        assert_eq!(flavor, "default");
    }

    #[test]
    fn local_builds_default_to_the_gradle_daemon_and_config_can_opt_out() {
        // The daemon is the local-build speed path (a warm Gradle daemon
        // between invocations); `ant.toml` can restore the hermetic one-shot
        // daemon with `daemon = "0"`.
        let gradle = build_adapter::adapter("gradle").expect("gradle");
        let (options, _) = build_options(gradle, &BTreeMap::new(), false, false).expect("options");
        assert_eq!(options.get("daemon"), Some("1"));
        let mut configured = BTreeMap::new();
        configured.insert("daemon".to_owned(), "0".to_owned());
        let (options, _) = build_options(gradle, &configured, false, false).expect("options");
        assert_eq!(options.get("daemon"), Some("0"));
    }

    #[test]
    fn android_release_flag_selects_the_release_task_and_beats_config() {
        // The front door maps --release/--dev onto the Gradle task (P4,
        // D-036); an explicit flag beats ant.toml's `task`.
        let gradle = build_adapter::adapter("gradle").expect("gradle");
        let (options, flavor) =
            build_options(gradle, &BTreeMap::new(), false, false).expect("options");
        assert_eq!(
            (options.get("task"), flavor),
            (Some("assembleDebug"), "default")
        );
        let (options, flavor) =
            build_options(gradle, &BTreeMap::new(), false, true).expect("options");
        assert_eq!(
            (options.get("task"), flavor),
            (Some("assembleRelease"), "release")
        );
        let mut configured = BTreeMap::new();
        configured.insert("task".to_owned(), "bundleRelease".to_owned());
        let (options, flavor) = build_options(gradle, &configured, false, false).expect("options");
        assert_eq!(
            (options.get("task"), flavor),
            (Some("bundleRelease"), "release")
        );
        let (options, _) = build_options(gradle, &configured, false, true).expect("options");
        assert_eq!(options.get("task"), Some("assembleRelease"));
    }

    #[test]
    fn flutter_release_default_and_dev_overrides() {
        // Flutter builds release by default; --dev selects debug, --release
        // restores release, and the mode is always shown in the summary.
        let flutter = build_adapter::adapter("flutter").expect("flutter");
        let (options, flavor) =
            build_options(flutter, &BTreeMap::new(), false, false).expect("options");
        assert_eq!((options.get("mode"), flavor), (Some("release"), "release"));
        let (options, flavor) =
            build_options(flutter, &BTreeMap::new(), true, false).expect("options");
        assert_eq!((options.get("mode"), flavor), (Some("debug"), "debug"));
        let mut configured = BTreeMap::new();
        configured.insert("mode".to_owned(), "profile".to_owned());
        let (options, flavor) = build_options(flutter, &configured, false, false).expect("options");
        assert_eq!((options.get("mode"), flavor), (Some("profile"), "profile"));
    }

    #[test]
    fn xcode_configuration_default_release_and_dev_overrides() {
        // Xcode builds Release by default (like cargo/cmake); --dev selects
        // Debug, --release restores Release, and the configuration is labeled.
        let xcode = build_adapter::adapter("xcode").expect("xcode");
        let (options, flavor) =
            build_options(xcode, &BTreeMap::new(), false, false).expect("options");
        assert_eq!(
            (options.get("configuration"), flavor),
            (Some("Release"), "release")
        );
        let (options, flavor) =
            build_options(xcode, &BTreeMap::new(), true, false).expect("options");
        assert_eq!(
            (options.get("configuration"), flavor),
            (Some("Debug"), "debug")
        );
        let mut configured = BTreeMap::new();
        configured.insert("configuration".to_owned(), "Debug".to_owned());
        let (options, flavor) = build_options(xcode, &configured, false, false).expect("options");
        assert_eq!(
            (options.get("configuration"), flavor),
            (Some("Debug"), "debug")
        );
    }

    #[test]
    fn ios_target_rejects_non_xcode_project() {
        // The `ios` target is Xcode-specific: requesting it on a project Xcode
        // did not detect fails before adapter selection with an honest reason.
        use std::ffi::OsString;
        let dir = std::env::temp_dir().join(format!(
            "ant-ios-target-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let args = ["ios", "--project"]
            .iter()
            .map(|s| OsString::from(*s))
            .chain(std::iter::once(OsString::from(dir.clone())));
        // ios on a non-Xcode project (and not any other adapter's) is refused
        // before adapter selection.
        let result = build_command(args);
        assert!(result.is_err(), "ios on a non-Xcode project must fail");
        let message = result.err().unwrap();
        assert!(message.contains("needs an Xcode project"), "{message}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
