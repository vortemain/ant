//! CLI entry points for remote execution (`ant coordinator`, `ant worker`,
//! `ant remote run`). The logic lives in the `ant-remote` crate.

use ant_remote::auth::Token;
use ant_remote::coordinator::{CoordinatorConfig, serve};
use ant_remote::http::Coordinator;
use ant_remote::worker::WorkerConfig;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

const COORDINATOR_USAGE: &str = "usage: ant coordinator --data-dir DIR --client-token-file FILE --worker-token-file FILE [--viewer-token-file FILE] [--listen ADDR] [--allow-non-loopback] [--max-body-mb N]";
const WORKER_USAGE: &str = "usage: ant worker --coordinator URL --token-file FILE --work-dir DIR [--id NAME] [--sandbox] [--once] [--allow-insecure-http]";
const REMOTE_USAGE: &str = "usage: ant remote run --coordinator URL --token-file FILE --plan PLAN.json [--workspace PATH] [--allow-insecure-http]";

/// Minimal flag parser: `--flag VALUE` pairs and boolean `--flag`s.
struct Flags {
    values: Vec<(String, String)>,
    switches: Vec<String>,
}

impl Flags {
    fn parse(
        args: impl Iterator<Item = OsString>,
        value_flags: &[&str],
        switch_flags: &[&str],
        usage: &str,
    ) -> Result<Self, String> {
        let mut values = Vec::new();
        let mut switches = Vec::new();
        let mut args = args.map(|arg| arg.to_string_lossy().into_owned());
        while let Some(arg) = args.next() {
            if value_flags.contains(&arg.as_str()) {
                if values.iter().any(|(name, _)| *name == arg) {
                    return Err(format!("{arg} may only be specified once"));
                }
                let value = args
                    .next()
                    .ok_or_else(|| format!("{arg} requires a value\n{usage}"))?;
                values.push((arg, value));
            } else if switch_flags.contains(&arg.as_str()) {
                if switches.contains(&arg) {
                    return Err(format!("{arg} may only be specified once"));
                }
                switches.push(arg);
            } else {
                return Err(format!("unexpected argument '{arg}'\n{usage}"));
            }
        }
        Ok(Self { values, switches })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(flag, _)| flag == name)
            .map(|(_, value)| value.as_str())
    }

    fn require(&self, name: &str, usage: &str) -> Result<&str, String> {
        self.get(name)
            .ok_or_else(|| format!("{name} is required\n{usage}"))
    }

    fn has(&self, name: &str) -> bool {
        self.switches.iter().any(|flag| flag == name)
    }
}

pub fn coordinator_command(args: impl Iterator<Item = OsString>) -> Result<u8, String> {
    let flags = Flags::parse(
        args,
        &[
            "--listen",
            "--data-dir",
            "--client-token-file",
            "--worker-token-file",
            "--viewer-token-file",
            "--max-body-mb",
        ],
        &["--allow-non-loopback"],
        COORDINATOR_USAGE,
    )?;
    let listen = flags
        .get("--listen")
        .unwrap_or("127.0.0.1:8742")
        .parse()
        .map_err(|error| format!("--listen must be IP:PORT: {error}"))?;
    let max_body_mb: u64 = flags
        .get("--max-body-mb")
        .unwrap_or("512")
        .parse()
        .ok()
        .filter(|mb| *mb > 0)
        .ok_or("--max-body-mb requires a positive integer")?;
    let client_token = Token::from_file(&PathBuf::from(
        flags.require("--client-token-file", COORDINATOR_USAGE)?,
    ))?;
    let worker_token = Token::from_file(&PathBuf::from(
        flags.require("--worker-token-file", COORDINATOR_USAGE)?,
    ))?;
    let viewer_token = flags
        .get("--viewer-token-file")
        .map(|path| Token::from_file(&PathBuf::from(path)))
        .transpose()?;
    let config = CoordinatorConfig {
        listen,
        data_dir: PathBuf::from(flags.require("--data-dir", COORDINATOR_USAGE)?),
        client_token,
        worker_token,
        viewer_token,
        max_body_bytes: max_body_mb * 1024 * 1024,
        allow_non_loopback: flags.has("--allow-non-loopback"),
        threads: 8,
    };
    serve(config, |bound| {
        use std::io::Write as _;
        println!(
            "coordinator listening={bound} protocol_version={} dashboard=http://{bound}/ui",
            ant_remote::protocol::PROTOCOL_VERSION
        );
        let _ = std::io::stdout().flush();
    })?;
    Ok(0)
}

fn connect(flags: &Flags, usage: &str) -> Result<Coordinator, String> {
    let token = Token::from_file(&PathBuf::from(flags.require("--token-file", usage)?))?;
    Coordinator::new(
        flags.require("--coordinator", usage)?,
        token,
        flags.has("--allow-insecure-http"),
    )
}

pub fn worker_command(args: impl Iterator<Item = OsString>) -> Result<u8, String> {
    let flags = Flags::parse(
        args,
        &["--coordinator", "--token-file", "--work-dir", "--id"],
        &["--sandbox", "--once", "--allow-insecure-http"],
        WORKER_USAGE,
    )?;
    let coordinator = connect(&flags, WORKER_USAGE)?;
    let default_id = format!(
        "{}-{}",
        std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "worker".to_owned()),
        std::process::id()
    );
    let config = WorkerConfig {
        worker_id: flags.get("--id").map_or(default_id, str::to_owned),
        work_dir: PathBuf::from(flags.require("--work-dir", WORKER_USAGE)?),
        sandbox: flags.has("--sandbox"),
        once: flags.has("--once"),
        poll_interval: Duration::from_millis(250),
    };
    if config.sandbox && !cfg!(any(windows, target_os = "linux")) {
        return Err("--sandbox is implemented for Windows and Linux only".to_owned());
    }
    ant_remote::worker::run(&coordinator, &config)?;
    Ok(0)
}

pub fn remote_command(mut args: impl Iterator<Item = OsString>) -> Result<u8, String> {
    match args.next() {
        Some(sub) if sub == "run" => {}
        _ => return Err(REMOTE_USAGE.to_owned()),
    }
    let flags = Flags::parse(
        args,
        &["--coordinator", "--token-file", "--plan", "--workspace"],
        &["--allow-insecure-http"],
        REMOTE_USAGE,
    )?;
    let coordinator = connect(&flags, REMOTE_USAGE)?;
    let graph = crate::load_graph(&PathBuf::from(flags.require("--plan", REMOTE_USAGE)?))?;
    let workspace = match flags.get("--workspace") {
        Some(path) => PathBuf::from(path),
        None => std::env::current_dir().map_err(|error| error.to_string())?,
    }
    .canonicalize()
    .map_err(|error| format!("cannot access workspace: {error}"))?;
    ant_remote::client::run_plan(&coordinator, &graph, &workspace, Duration::from_millis(200))
}
