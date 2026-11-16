//! `ant server`: put Ant on your own Linux server over SSH (roadmap P8,
//! decision D-047).
//!
//! ```text
//! ant server deploy USER@HOST [--name NAME] [--port N] [--identity FILE] [--binary PATH] [--sandbox]
//! ant server add NAME --url https://HOST [--token-file FILE]   (also: ant login URL)
//! ant server status [NAME]
//! ant server audit [NAME]
//! ant server list
//! ant server forget NAME
//! ant remote run --server NAME --plan PLAN.json
//! ```
//!
//! - Everything goes through the system `ssh`/`scp` with `BatchMode=yes`:
//!   your keys or agent, never a password typed into Ant. A host seen for
//!   the first time is accepted and remembered (`StrictHostKeyChecking=
//!   accept-new`); a *changed* host key still stops the connection.
//! - The server must be Linux with systemd, reachable as root or a user
//!   with passwordless sudo. Deploy installs `/usr/local/bin/ant` (uploaded
//!   with `--binary`, else downloaded from the latest GitHub Release on the
//!   server; either way checked against a SHA-256 before it is installed),
//!   a locked-down `ant` service account, tokens generated on the server,
//!   and two systemd services: the coordinator, listening on the server's
//!   own `127.0.0.1:8742` only, and one worker. Redeploying upgrades the
//!   binary and keeps tokens and data.
//! - Nothing listens on the network: clients reach the coordinator through
//!   an SSH tunnel Ant opens when needed, so there is no TLS to set up and
//!   no port to open. The client token travels back once over SSH and is
//!   stored like Ant's signing passwords (DPAPI on Windows, a 0600 file
//!   elsewhere).

use crate::signing::{data_dir, seal, sealed_name, unseal, write_private};
use std::ffi::OsString;
use std::io::Write as _;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The coordinator's port on the server's loopback interface.
pub const REMOTE_PORT: u16 = 8742;

const USAGE: &str = "usage: ant server deploy USER@HOST [--name NAME] [--port N] [--identity FILE] [--binary PATH] [--sandbox] [--toolchain rust] [--workers N]
       ant server add NAME --url https://HOST [--token-file FILE]
       ant server status [NAME]
       ant server audit [NAME]
       ant server list
       ant server forget NAME";

/// Runs on the server as root: install the binary, the service account,
/// tokens, and the two systemd services, then wait until both are active.
/// Arguments: MODE (upload|download), SOURCE (uploaded path | target
/// triple), SANDBOX (0|1), EXPECTED (sha256 of an upload, or -),
/// TOOLCHAINS (comma-separated, e.g. `rust`, or -), WORKERS (1-16).
const SETUP_SCRIPT: &str = r#"set -eu
MODE="$1"; SOURCE="$2"; SANDBOX="$3"; EXPECTED="$4"; TOOLCHAINS="$5"; WORKERS="${6:-1}"
PORT=8742
if [ "$MODE" = upload ]; then
  BIN="$SOURCE"
  actual=$(sha256sum "$BIN" | cut -d' ' -f1)
  if [ "$actual" != "$EXPECTED" ]; then
    rm -f "$BIN"; echo "ant server deploy: the uploaded binary's checksum does not match" >&2; exit 3
  fi
else
  BIN=$(mktemp); SUMS=$(mktemp)
  base=https://github.com/vortemain/ant/releases/latest/download
  if ! curl -fsSL -o "$BIN" "$base/ant-$SOURCE" || ! curl -fsSL -o "$SUMS" "$base/SHA256SUMS"; then
    echo "ant server deploy: could not download ant-$SOURCE from GitHub Releases (has a release been published? pass --binary PATH to upload one)" >&2; exit 4
  fi
  expected=$(awk -v n="ant-$SOURCE" '$2 == n || $2 == "*" n { print $1 }' "$SUMS")
  if [ -z "$expected" ] || [ "$(sha256sum "$BIN" | cut -d' ' -f1)" != "$expected" ]; then
    echo "ant server deploy: the downloaded binary's checksum does not match" >&2; exit 3
  fi
  rm -f "$SUMS"
fi
install -m 0755 "$BIN" /usr/local/bin/ant.new
rm -f "$BIN"
/usr/local/bin/ant.new --version >/dev/null
mv -f /usr/local/bin/ant.new /usr/local/bin/ant
getent group ant >/dev/null || groupadd --system ant
id -u ant >/dev/null 2>&1 || useradd --system --gid ant --home-dir /var/lib/ant --no-create-home --shell /usr/sbin/nologin ant
install -d -m 0750 -o root -g ant /etc/ant
install -d -m 0750 -o ant -g ant /var/lib/ant /var/lib/ant/coordinator /var/lib/ant/worker
for name in client worker; do
  f=/etc/ant/$name.token
  if [ ! -s "$f" ]; then (umask 077; head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$f"); fi
  chown root:ant "$f"; chmod 0640 "$f"
done
case ",$TOOLCHAINS," in
  *,rust,*)
    if ! command -v cc >/dev/null 2>&1; then
      if command -v apt-get >/dev/null 2>&1; then
        apt-get update -q >/dev/null; DEBIAN_FRONTEND=noninteractive apt-get install -y -q gcc >/dev/null
      elif command -v dnf >/dev/null 2>&1; then
        dnf install -y -q gcc >/dev/null
      else
        echo "ant server deploy: Rust needs a C linker (cc); install gcc on this server" >&2
      fi
    fi
    if [ ! -x /var/lib/ant/.cargo/bin/cargo ]; then
      command -v curl >/dev/null 2>&1 || { echo "ant server deploy: installing Rust needs curl" >&2; exit 6; }
      INIT=$(mktemp)
      curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o "$INIT"
      chmod 0755 "$INIT"
      runuser -u ant -- env HOME=/var/lib/ant sh "$INIT" -y --profile minimal --no-modify-path >/dev/null
      rm -f "$INIT"
    fi
    echo "rust: $(runuser -u ant -- env HOME=/var/lib/ant /var/lib/ant/.cargo/bin/cargo --version)"
    ;;
esac
SANDBOX_FLAG=""
if [ "$SANDBOX" = 1 ]; then SANDBOX_FLAG=" --sandbox"; fi
cat > /etc/systemd/system/ant-coordinator.service <<UNIT
[Unit]
Description=Ant coordinator (installed by ant server deploy)
After=network-online.target

[Service]
User=ant
Group=ant
ExecStart=/usr/local/bin/ant coordinator --listen 127.0.0.1:$PORT --data-dir /var/lib/ant/coordinator --client-token-file /etc/ant/client.token --worker-token-file /etc/ant/worker.token --max-body-mb 512
Restart=on-failure
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/lib/ant

[Install]
WantedBy=multi-user.target
UNIT
cat > /etc/systemd/system/ant-worker.service <<UNIT
[Unit]
Description=Ant worker (installed by ant server deploy)
After=ant-coordinator.service
Wants=ant-coordinator.service

[Service]
User=ant
Group=ant
Environment=HOME=/var/lib/ant
Environment=PATH=/var/lib/ant/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
ExecStart=/usr/local/bin/ant worker --coordinator http://127.0.0.1:$PORT --token-file /etc/ant/worker.token --work-dir /var/lib/ant/worker$SANDBOX_FLAG
Restart=always
RestartSec=2
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/lib/ant

[Install]
WantedBy=multi-user.target
UNIT
# Extra workers (D-050): ant-worker@2 ... ant-worker@N, each with its own
# work directory, so up to N independent actions build at once.
cat > /etc/systemd/system/ant-worker@.service <<UNIT
[Unit]
Description=Ant worker %i (installed by ant server deploy)
After=ant-coordinator.service
Wants=ant-coordinator.service

[Service]
User=ant
Group=ant
Environment=HOME=/var/lib/ant
Environment=PATH=/var/lib/ant/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
ExecStart=/usr/local/bin/ant worker --coordinator http://127.0.0.1:$PORT --token-file /etc/ant/worker.token --work-dir /var/lib/ant/worker-%i --id %H-%i$SANDBOX_FLAG
Restart=always
RestartSec=2
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/lib/ant

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl enable ant-coordinator.service ant-worker.service >/dev/null 2>&1
systemctl restart ant-coordinator.service
systemctl restart ant-worker.service
UNITS="ant-coordinator.service ant-worker.service"
n=2
while [ "$n" -le 16 ]; do
  if [ "$n" -le "$WORKERS" ]; then
    install -d -m 0750 -o ant -g ant "/var/lib/ant/worker-$n"
    systemctl enable "ant-worker@$n.service" >/dev/null 2>&1
    systemctl restart "ant-worker@$n.service"
    UNITS="$UNITS ant-worker@$n.service"
  else
    systemctl disable --now "ant-worker@$n.service" >/dev/null 2>&1 || true
  fi
  n=$((n + 1))
done
i=0
while :; do
  all=1
  for unit in $UNITS; do systemctl is-active --quiet "$unit" || all=0; done
  [ "$all" = 1 ] && break
  i=$((i + 1))
  if [ "$i" -ge 30 ]; then systemctl status --no-pager $UNITS >&2 || true; exit 5; fi
  sleep 1
done
echo "ANT_DEPLOY_OK $(/usr/local/bin/ant --version)"
"#;

/// How to reach one server (non-secret; the token is stored separately).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServerProfile {
    pub name: String,
    /// `user@host` as given to ssh.
    pub target: String,
    #[serde(default)]
    pub ssh_port: Option<u16>,
    #[serde(default)]
    pub identity: Option<String>,
    pub remote_port: u16,
    pub deployed_ms: u64,
    /// `uname -s` / `uname -m` of the server, recorded at deploy time.
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    /// A team coordinator reached over HTTPS directly (D-058) instead of an
    /// SSH tunnel; `target` then repeats the URL for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

fn servers_dir() -> Result<PathBuf, String> {
    Ok(data_dir()
        .ok_or("cannot find a per-user data directory for server profiles")?
        .join("servers"))
}

fn valid_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if ok && !name.starts_with('.') {
        Ok(())
    } else {
        Err(format!(
            "server name '{name}' must use letters, digits, '-', '_' or '.'"
        ))
    }
}

/// The default name for a target: its host, made into a valid name.
fn default_name(target: &str) -> String {
    let host = target.rsplit('@').next().unwrap_or(target);
    let name: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    name.trim_matches(['-', '.']).to_owned()
}

/// Accept `user@host` or `host`; refuse anything ssh would read as an
/// option or that carries shell metacharacters.
fn valid_target(target: &str) -> Result<(), String> {
    let ok = !target.is_empty()
        && !target.starts_with('-')
        && target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@._-:[]".contains(c));
    if ok {
        Ok(())
    } else {
        Err(format!("'{target}' is not a valid USER@HOST"))
    }
}

impl ServerProfile {
    fn ssh(&self) -> Command {
        let mut command = Command::new("ssh");
        command.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=15",
            "-o",
            "StrictHostKeyChecking=accept-new",
        ]);
        if let Some(port) = self.ssh_port {
            command.args(["-p", &port.to_string()]);
        }
        if let Some(identity) = &self.identity {
            command.args(["-i", identity]);
        }
        command
    }

    fn scp(&self) -> Command {
        let mut command = Command::new("scp");
        command.args([
            "-q",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=15",
            "-o",
            "StrictHostKeyChecking=accept-new",
        ]);
        if let Some(port) = self.ssh_port {
            command.args(["-P", &port.to_string()]);
        }
        if let Some(identity) = &self.identity {
            command.args(["-i", identity]);
        }
        command
    }

    /// Run `remote` on the server; returns stdout, or an error with stderr.
    fn run(&self, remote: &str, stdin: Option<&str>) -> Result<String, String> {
        let mut command = self.ssh();
        command
            .arg(&self.target)
            .arg(remote)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| format!("cannot run ssh (is OpenSSH installed?): {error}"))?;
        if let Some(input) = stdin
            && let Some(mut pipe) = child.stdin.take()
        {
            pipe.write_all(input.as_bytes())
                .map_err(|error| format!("cannot send the setup script: {error}"))?;
        }
        let output = child
            .wait_with_output()
            .map_err(|error| error.to_string())?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if output.status.success() {
            Ok(stdout)
        } else {
            Err(format!(
                "{}{}",
                String::from_utf8_lossy(&output.stderr).trim(),
                if stdout.trim().is_empty() {
                    String::new()
                } else {
                    format!("\n{}", stdout.trim())
                }
            ))
        }
    }

    fn save(&self, token: &str) -> Result<(), String> {
        let dir = servers_dir()?;
        std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        let profile = dir.join(format!("{}.json", self.name));
        let sealed = dir.join(sealed_name(&format!("{}.token", self.name)));
        if let Ok(old) = std::fs::read(&sealed) {
            crate::signing::forget_sealed(&old);
        }
        for path in [&profile, &sealed] {
            let _ = std::fs::remove_file(path);
        }
        let text = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        write_private(&profile, text.as_bytes())?;
        write_private(&sealed, &seal(token.as_bytes())?)
    }

    pub fn load(name: &str) -> Result<Self, String> {
        valid_name(name)?;
        let path = servers_dir()?.join(format!("{name}.json"));
        let text = std::fs::read_to_string(&path).map_err(|_| {
            format!("no server named '{name}' (see `ant server list`, or deploy one with `ant server deploy USER@HOST`)")
        })?;
        serde_json::from_str(&text)
            .map_err(|error| format!("server profile {name} is damaged: {error}"))
    }

    fn token(&self) -> Result<String, String> {
        let path = servers_dir()?.join(sealed_name(&format!("{}.token", self.name)));
        let blob = std::fs::read(&path)
            .map_err(|error| format!("cannot read the token for {}: {error}", self.name))?;
        let bytes = unseal(&blob)?;
        String::from_utf8(bytes).map_err(|_| "the stored token is not text".to_owned())
    }
}

/// An SSH tunnel from a free local port to the coordinator on the server.
/// Dropping it closes the tunnel.
pub struct Tunnel {
    /// `None` for HTTPS servers (D-058): nothing to close.
    child: Option<Child>,
    pub local_port: u16,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Tunnel {
    fn open(profile: &ServerProfile) -> Result<Self, String> {
        let local_port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .and_then(|listener| listener.local_addr())
            .map(|address| address.port())
            .map_err(|error| format!("cannot pick a local port: {error}"))?;
        let mut command = profile.ssh();
        command
            .args([
                "-N",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ServerAliveInterval=15",
                "-L",
                &format!("127.0.0.1:{local_port}:127.0.0.1:{}", profile.remote_port),
            ])
            .arg(&profile.target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| format!("cannot run ssh (is OpenSSH installed?): {error}"))?;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if TcpStream::connect((Ipv4Addr::LOCALHOST, local_port)).is_ok() {
                return Ok(Self {
                    child: Some(child),
                    local_port,
                });
            }
            if let Ok(Some(_)) = child.try_wait() {
                let mut message = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    use std::io::Read as _;
                    let _ = stderr.read_to_string(&mut message);
                }
                return Err(format!(
                    "the SSH tunnel to {} failed: {}",
                    profile.target,
                    message.trim()
                ));
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                return Err(format!(
                    "timed out opening an SSH tunnel to {}",
                    profile.target
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// A coordinator client for server `name`, through a fresh tunnel (keep the
/// tunnel alive while using the client).
pub fn connect(name: &str) -> Result<(ant_remote::http::Coordinator, Tunnel), String> {
    let profile = ServerProfile::load(name)?;
    let token = ant_remote::auth::Token::new(&bearer(&profile)?)?;
    if let Some(url) = &profile.url {
        let coordinator = ant_remote::http::Coordinator::new(url, token, false)?;
        let tunnel = Tunnel {
            child: None,
            local_port: 0,
        };
        return Ok((coordinator, tunnel));
    }
    let tunnel = Tunnel::open(&profile)?;
    let coordinator = ant_remote::http::Coordinator::new(
        &format!("http://127.0.0.1:{}", tunnel.local_port),
        token,
        false,
    )?;
    Ok((coordinator, tunnel))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn deploy(mut args: impl Iterator<Item = String>) -> Result<u8, String> {
    let mut target = None;
    let mut name = None;
    let mut ssh_port = None;
    let mut identity = None;
    let mut binary: Option<PathBuf> = None;
    let mut sandbox = false;
    let mut toolchains: Vec<String> = Vec::new();
    let mut workers: u8 = 1;
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| args.next().ok_or(format!("{flag} requires a value"));
        match arg.as_str() {
            "--name" => name = Some(value("--name")?),
            "--port" => {
                ssh_port = Some(
                    value("--port")?
                        .parse::<u16>()
                        .map_err(|_| "--port requires a port number")?,
                )
            }
            "--identity" => identity = Some(value("--identity")?),
            "--binary" => binary = Some(PathBuf::from(value("--binary")?)),
            "--sandbox" => sandbox = true,
            "--workers" => {
                workers = value("--workers")?
                    .parse::<u8>()
                    .ok()
                    .filter(|n| (1..=16).contains(n))
                    .ok_or("--workers takes a number from 1 to 16")?;
            }
            "--toolchain" => {
                let toolchain = value("--toolchain")?;
                if toolchain != "rust" {
                    return Err(format!(
                        "toolchain '{toolchain}' cannot be installed by Ant yet (supported: rust); install it on the server so the `ant` user finds it on PATH"
                    ));
                }
                toolchains.push(toolchain);
            }
            other if !other.starts_with('-') && target.is_none() => target = Some(other.to_owned()),
            other => return Err(format!("unexpected argument '{other}'\n{USAGE}")),
        }
    }
    let target = target.ok_or(USAGE)?;
    valid_target(&target)?;
    let name = name.unwrap_or_else(|| default_name(&target));
    valid_name(&name)?;
    let mut profile = ServerProfile {
        name: name.clone(),
        target: target.clone(),
        ssh_port,
        identity,
        remote_port: REMOTE_PORT,
        deployed_ms: crate::history::now_ms(),
        os: None,
        arch: None,
        url: None,
    };

    println!("Checking {target}…");
    let facts = profile.run(
        "uname -sm; id -u; command -v systemctl >/dev/null && echo systemd || echo no-systemd; \
         sudo -n true >/dev/null 2>&1 && echo sudo || echo no-sudo",
        None,
    )?;
    let lines: Vec<&str> = facts.lines().map(str::trim).collect();
    let (system, uid, init, sudo) = match lines.as_slice() {
        [system, uid, init, sudo, ..] => (*system, *uid, *init, *sudo),
        _ => return Err(format!("unexpected answer from {target}: {facts}")),
    };
    let mut parts = system.split_whitespace();
    let (os, arch) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if os != "Linux" {
        return Err(format!(
            "{target} runs {os}; ant server deploy supports Linux servers"
        ));
    }
    if init != "systemd" {
        return Err(format!(
            "{target} has no systemd; ant server deploy installs systemd services"
        ));
    }
    let root = uid == "0";
    if !root && sudo != "sudo" {
        return Err(format!(
            "{target} needs root, or passwordless sudo for this user (Ant never types passwords)"
        ));
    }
    profile.os = Some(os.to_owned());
    profile.arch = Some(arch.to_owned());
    let remote_target = match arch {
        "x86_64" | "amd64" => "x86_64-unknown-linux-musl",
        "aarch64" | "arm64" => "aarch64-unknown-linux-musl",
        other => return Err(format!("no Ant build for {other} servers")),
    };
    println!(
        "  Linux {arch}, systemd, {}",
        if root { "root" } else { "passwordless sudo" }
    );

    let (mode, source, expected) = match &binary {
        Some(path) => {
            let bytes = std::fs::read(path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            let expected = sha256_hex(&bytes);
            let remote_path = format!("/tmp/ant-deploy-{}", crate::signing::random_hex(8)?);
            println!(
                "Uploading {} ({:.1} MB)…",
                path.display(),
                bytes.len() as f64 / 1_048_576.0
            );
            let status = profile
                .scp()
                .arg(path)
                .arg(format!("{target}:{remote_path}"))
                .status()
                .map_err(|error| format!("cannot run scp: {error}"))?;
            if !status.success() {
                return Err(format!("uploading to {target} failed"));
            }
            ("upload", remote_path, expected)
        }
        None => ("download", remote_target.to_owned(), "-".to_owned()),
    };
    println!("Installing Ant, its service account, and the coordinator and worker services…");
    if !toolchains.is_empty() {
        println!(
            "  and toolchains: {} (this can take a minute the first time)",
            toolchains.join(", ")
        );
    }
    let sudo_prefix = if root { "" } else { "sudo -n " };
    let output = profile.run(
        &format!(
            "{sudo_prefix}sh -s -- {mode} {source} {} {expected} {} {workers}",
            if sandbox { 1 } else { 0 },
            if toolchains.is_empty() {
                "-".to_owned()
            } else {
                toolchains.join(",")
            }
        ),
        Some(SETUP_SCRIPT),
    )?;
    for line in output.lines().filter(|line| line.starts_with("rust: ")) {
        println!("  {line}");
    }
    let version = output
        .lines()
        .find_map(|line| line.strip_prefix("ANT_DEPLOY_OK "))
        .ok_or_else(|| format!("the setup on {target} did not finish:\n{output}"))?;
    let token = profile.run(&format!("{sudo_prefix}cat /etc/ant/client.token"), None)?;
    let token = token.trim();
    if token.len() < 32 {
        return Err("the server's client token looks wrong; nothing was saved".to_owned());
    }
    profile.save(token)?;

    let (coordinator, _tunnel) = connect(&name)?;
    let health = health_and_workers(&coordinator)?;
    println!("\n✓ {name} is ready: {version} on {target}");
    println!("  {health}");
    println!("  The coordinator listens only on the server itself; Ant reaches it through SSH.");
    println!("  Build there:  ant build --on {name}   (or server = \"{name}\" in ant.toml)");
    println!("  Check it:     ant server status {name}");
    Ok(0)
}

/// Connect to a deployed server and describe it in one line (the
/// dashboard's Servers page, D-053).
pub(crate) fn status_line(name: &str) -> Result<String, String> {
    let (coordinator, _tunnel) = connect(name)?;
    health_and_workers(&coordinator)
}

/// `ant server add NAME --url URL [--token-file FILE]` and `ant login URL
/// [--name NAME] [--token-file FILE]` (D-058): remember a team coordinator
/// served over HTTPS (behind a TLS reverse proxy; plain HTTP only on
/// loopback), check it answers, and record its workers' OS for `ant build
/// --on`. The client token comes from a file or stdin, never the command
/// line, and is sealed like a deployed server's (D-052).
fn add(mut args: impl Iterator<Item = String>) -> Result<u8, String> {
    let name = args.next().ok_or(ADD_USAGE)?;
    let mut url = None;
    let mut token_file = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--url" => url = Some(args.next().ok_or(ADD_USAGE)?),
            "--token-file" => token_file = Some(PathBuf::from(args.next().ok_or(ADD_USAGE)?)),
            other => return Err(format!("unexpected argument '{other}'\n{ADD_USAGE}")),
        }
    }
    let url = url.ok_or(ADD_USAGE)?;
    register(&name, &url, token_file.as_deref())
}

const ADD_USAGE: &str = "usage: ant server add NAME --url https://HOST [--token-file FILE]   (the token is read from stdin without --token-file)";
const LOGIN_USAGE: &str = "usage: ant login https://HOST [--name NAME] [--sso | --token-file FILE]   (--sso signs in with your team's identity provider; otherwise the token is read from stdin without --token-file)";

/// A single-sign-on credential (D-063), stored sealed in place of a token.
#[derive(serde::Serialize, serde::Deserialize)]
struct SsoCredential {
    id_token: String,
    refresh_token: Option<String>,
    token_endpoint: String,
    client_id: String,
    client_secret: Option<String>,
}

/// The bearer to present: the stored token, or for single sign-on a
/// current ID token (refreshed and re-sealed when it is about to expire).
fn bearer(profile: &ServerProfile) -> Result<String, String> {
    let text = profile.token()?;
    let text = text.trim();
    if !text.starts_with('{') {
        return Ok(text.to_owned());
    }
    let mut credential: SsoCredential =
        serde_json::from_str(text).map_err(|_| "the stored sign-in is damaged".to_owned())?;
    let now = crate::history::now_ms() / 1000;
    let expiry = ant_remote::oidc::unverified_expiry(&credential.id_token).unwrap_or(0);
    if expiry > now + 120 {
        return Ok(credential.id_token);
    }
    let refresh = credential.refresh_token.clone().ok_or_else(|| {
        format!(
            "your sign-in to {} expired; sign in again with: ant login {} --sso",
            profile.name, profile.target
        )
    })?;
    let mut form = vec![
        ("grant_type", "refresh_token".to_owned()),
        ("refresh_token", refresh),
        ("client_id", credential.client_id.clone()),
    ];
    if let Some(secret) = &credential.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    let answer = post_form(&credential.token_endpoint, &form)?;
    credential.id_token = answer["id_token"]
        .as_str()
        .ok_or_else(|| {
            format!(
                "your sign-in to {} could not be renewed; sign in again with: ant login {} --sso",
                profile.name, profile.target
            )
        })?
        .to_owned();
    if let Some(rotated) = answer["refresh_token"].as_str() {
        credential.refresh_token = Some(rotated.to_owned());
    }
    let text = serde_json::to_string(&credential).map_err(|error| error.to_string())?;
    profile.save(&text)?;
    Ok(credential.id_token)
}

fn form_encode(pairs: &[(&str, String)]) -> String {
    let encode = |text: &str| {
        text.bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    (byte as char).to_string()
                }
                _ => format!("%{byte:02X}"),
            })
            .collect::<String>()
    };
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn http_agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)))
            .build(),
    )
}

/// POST a form; the JSON answer whatever the status (OAuth errors are JSON).
fn post_form(url: &str, pairs: &[(&str, String)]) -> Result<serde_json::Value, String> {
    let mut response = http_agent()
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(form_encode(pairs).as_bytes())
        .map_err(|error| format!("cannot reach {url}: {error}"))?;
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|error| format!("cannot read {url}: {error}"))?;
    serde_json::from_str(&text).map_err(|_| format!("{url} did not answer with JSON"))
}

/// `ant login URL --sso` (D-063): sign in with the team's identity provider
/// using the device authorization grant (RFC 8628): open a link anywhere,
/// enter the code shown here, and Ant receives an ID token.
fn sso_register(name: &str, url: &str) -> Result<u8, String> {
    valid_name(name)?;
    let url = url.trim_end_matches('/').to_owned();
    ant_remote::http::check_transport(&url, false)?;
    let mut response = http_agent()
        .get(&format!("{url}/v1/sso"))
        .call()
        .map_err(|error| format!("cannot reach {url}: {error}"))?;
    if response.status().as_u16() != 200 {
        return Err(format!(
            "{url} has no single sign-on configured (ask for a client token instead)"
        ));
    }
    let info: serde_json::Value = serde_json::from_str(
        &response
            .body_mut()
            .read_to_string()
            .map_err(|error| error.to_string())?,
    )
    .map_err(|_| "the server's sign-in settings are not JSON".to_owned())?;
    let text = |key: &str| info[key].as_str().map(str::to_owned);
    let client_id = text("client_id").ok_or("the server names no client id")?;
    let client_secret = text("client_secret");
    let token_endpoint = text("token_endpoint").ok_or("the server names no token endpoint")?;
    let device_endpoint = text("device_authorization_endpoint")
        .ok_or("the identity provider offers no device sign-in (device_authorization_endpoint)")?;
    let mut request = vec![
        ("client_id", client_id.clone()),
        ("scope", "openid email".to_owned()),
    ];
    if let Some(secret) = &client_secret {
        request.push(("client_secret", secret.clone()));
    }
    let device = post_form(&device_endpoint, &request)?;
    let device_code = device["device_code"]
        .as_str()
        .ok_or_else(|| {
            format!(
                "the identity provider refused device sign-in: {}",
                device["error"]
            )
        })?
        .to_owned();
    let user_code = device["user_code"].as_str().unwrap_or("").to_owned();
    let verify_at = device["verification_uri_complete"]
        .as_str()
        .or_else(|| device["verification_uri"].as_str())
        .or_else(|| device["verification_url"].as_str())
        .unwrap_or("")
        .to_owned();
    let mut interval = device["interval"].as_u64().unwrap_or(5).max(1);
    let deadline =
        Instant::now() + Duration::from_secs(device["expires_in"].as_u64().unwrap_or(900));
    println!("To sign in to {name}, open {verify_at}");
    println!("and enter the code:  {user_code}");
    println!("Waiting for you to finish signing in…");
    let tokens = loop {
        if Instant::now() > deadline {
            return Err("the sign-in code expired; run the command again".to_owned());
        }
        std::thread::sleep(Duration::from_secs(interval));
        let mut poll = vec![
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:device_code".to_owned(),
            ),
            ("device_code", device_code.clone()),
            ("client_id", client_id.clone()),
        ];
        if let Some(secret) = &client_secret {
            poll.push(("client_secret", secret.clone()));
        }
        let answer = post_form(&token_endpoint, &poll)?;
        match answer["error"].as_str() {
            None => break answer,
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some(other) => return Err(format!("sign-in failed: {other}")),
        }
    };
    let credential = SsoCredential {
        id_token: tokens["id_token"]
            .as_str()
            .ok_or("the identity provider returned no ID token (is the openid scope allowed?)")?
            .to_owned(),
        refresh_token: tokens["refresh_token"].as_str().map(str::to_owned),
        token_endpoint,
        client_id,
        client_secret,
    };
    let coordinator = ant_remote::http::Coordinator::new(
        &url,
        ant_remote::auth::Token::new(&credential.id_token)?,
        false,
    )?;
    let summary = health_and_workers(&coordinator)
        .map_err(|error| format!("{url} did not accept your sign-in: {error}"))?;
    let workers: ant_remote::protocol::WorkerList =
        coordinator.get_json("/v1/workers", "worker list")?;
    let profile = ServerProfile {
        name: name.to_owned(),
        target: url.clone(),
        ssh_port: None,
        identity: None,
        remote_port: 0,
        deployed_ms: crate::history::now_ms(),
        os: workers.workers.first().map(|worker| worker.os.clone()),
        arch: None,
        url: Some(url.clone()),
    };
    profile.save(&serde_json::to_string(&credential).map_err(|error| error.to_string())?)?;
    println!("Signed in to {name} ({url}): {summary}");
    println!("  Build there:  ant build --on {name}");
    Ok(0)
}

/// `ant login URL`: [`add`] with the name taken from the host.
pub fn login_command(args: impl Iterator<Item = OsString>) -> Result<u8, String> {
    let mut url = None;
    let mut name = None;
    let mut token_file = None;
    let mut sso = false;
    let mut args = args.map(|arg| arg.to_string_lossy().into_owned());
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sso" => sso = true,
            "--name" => name = Some(args.next().ok_or(LOGIN_USAGE)?),
            "--token-file" => token_file = Some(PathBuf::from(args.next().ok_or(LOGIN_USAGE)?)),
            other if url.is_none() && !other.starts_with('-') => url = Some(other.to_owned()),
            other => return Err(format!("unexpected argument '{other}'\n{LOGIN_USAGE}")),
        }
    }
    let url = url.ok_or(LOGIN_USAGE)?;
    let name = match name {
        Some(name) => name,
        None => {
            let host = url
                .split("://")
                .nth(1)
                .and_then(|rest| rest.split(['/', ':']).next())
                .unwrap_or("team");
            host.split('.').next().unwrap_or("team").to_owned()
        }
    };
    if sso {
        return sso_register(&name, &url);
    }
    register(&name, &url, token_file.as_deref())
}

fn register(name: &str, url: &str, token_file: Option<&std::path::Path>) -> Result<u8, String> {
    valid_name(name)?;
    let url = url.trim_end_matches('/').to_owned();
    ant_remote::http::check_transport(&url, false)?;
    let token = match token_file {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?,
        None => {
            use std::io::Read as _;
            eprintln!(
                "Paste the client token for {url} and press Enter (Ctrl+D/Ctrl+Z to finish):"
            );
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|error| format!("cannot read the token: {error}"))?;
            text
        }
    };
    let token = token.trim().to_owned();
    let coordinator =
        ant_remote::http::Coordinator::new(&url, ant_remote::auth::Token::new(&token)?, false)?;
    let summary = health_and_workers(&coordinator)
        .map_err(|error| format!("{url} did not answer as an Ant coordinator: {error}"))?;
    let workers: ant_remote::protocol::WorkerList =
        coordinator.get_json("/v1/workers", "worker list")?;
    let os = workers.workers.first().map(|worker| worker.os.clone());
    let profile = ServerProfile {
        name: name.to_owned(),
        target: url.clone(),
        ssh_port: None,
        identity: None,
        remote_port: 0,
        deployed_ms: crate::history::now_ms(),
        os,
        arch: None,
        url: Some(url.clone()),
    };
    profile.save(&token)?;
    println!("Added {name} ({url}): {summary}");
    println!("  Build there:  ant build --on {name}   (or server = \"{name}\" in ant.toml)");
    Ok(0)
}

/// One line about the coordinator and its workers.
fn health_and_workers(coordinator: &ant_remote::http::Coordinator) -> Result<String, String> {
    let health: ant_remote::protocol::Health =
        coordinator.get_json("/v1/health", "health check")?;
    // The worker may still be registering right after a (re)start.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let list: ant_remote::protocol::WorkerList =
            coordinator.get_json("/v1/workers", "worker list")?;
        let online: Vec<_> = list
            .workers
            .iter()
            .filter(|worker| list.now_unix_ms.saturating_sub(worker.last_seen_unix_ms) < 30_000)
            .collect();
        if !online.is_empty() || Instant::now() > deadline {
            let busy = online
                .iter()
                .filter(|worker| worker.current_job.is_some())
                .count();
            return Ok(format!(
                "coordinator protocol v{} · {} worker{} online{}",
                health.protocol_version,
                online.len(),
                if online.len() == 1 { "" } else { "s" },
                if busy > 0 {
                    format!(" ({busy} busy)")
                } else {
                    String::new()
                }
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

pub(crate) fn all_profiles() -> Vec<ServerProfile> {
    let Ok(dir) = servers_dir() else {
        return Vec::new();
    };
    let mut profiles: Vec<ServerProfile> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| serde_json::from_str(&std::fs::read_to_string(entry.path()).ok()?).ok())
        .collect();
    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    profiles
}

fn only_profile(name: Option<String>) -> Result<String, String> {
    if let Some(name) = name {
        return Ok(name);
    }
    let profiles = all_profiles();
    match profiles.as_slice() {
        [only] => Ok(only.name.clone()),
        [] => Err("no servers yet; deploy one with `ant server deploy USER@HOST`".to_owned()),
        _ => Err(format!(
            "several servers ({}); name one: ant server status NAME",
            profiles
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

pub fn server_command(args: impl Iterator<Item = OsString>) -> Result<u8, String> {
    let mut args = args.map(|arg| arg.to_string_lossy().into_owned());
    match args.next().as_deref() {
        Some("deploy") => deploy(args),
        Some("add") => add(args),
        Some("audit") => {
            let name = only_profile(args.next())?;
            let (coordinator, _tunnel) = connect(&name)?;
            let audit: serde_json::Value = coordinator.get_json("/v1/audit", "audit log")?;
            let events = audit["events"].as_array().cloned().unwrap_or_default();
            if events.is_empty() {
                println!("{name}: no audit events yet.");
            }
            for event in events {
                let text = |key: &str| event[key].as_str().unwrap_or("").to_owned();
                let mut line = format!(
                    "{}  {:<20}",
                    event["time_ms"].as_u64().unwrap_or(0),
                    text("event")
                );
                for key in ["job", "action", "worker", "role", "from", "path"] {
                    if !text(key).is_empty() {
                        line.push_str(&format!(" {key}={}", text(key)));
                    }
                }
                if let Some(succeeded) = event["succeeded"].as_bool() {
                    line.push_str(&format!(" succeeded={succeeded}"));
                }
                println!("{line}");
            }
            Ok(0)
        }
        Some("status") => {
            let name = only_profile(args.next())?;
            let profile = ServerProfile::load(&name)?;
            println!("{name} ({})", profile.target);
            let (coordinator, _tunnel) = connect(&name)?;
            println!("  {}", health_and_workers(&coordinator)?);
            Ok(0)
        }
        Some("list") => {
            let profiles = all_profiles();
            if profiles.is_empty() {
                println!("No servers yet. Deploy one with: ant server deploy USER@HOST");
            }
            for profile in profiles {
                println!("{:<20} {}", profile.name, profile.target);
            }
            Ok(0)
        }
        Some("forget") => {
            let name = args.next().ok_or(USAGE)?;
            valid_name(&name)?;
            let dir = servers_dir()?;
            let profile = dir.join(format!("{name}.json"));
            if !profile.exists() {
                return Err(format!("no server named '{name}'"));
            }
            let _ = std::fs::remove_file(&profile);
            let token = dir.join(sealed_name(&format!("{name}.token")));
            if let Ok(blob) = std::fs::read(&token) {
                crate::signing::forget_sealed(&blob);
            }
            let _ = std::fs::remove_file(&token);
            println!(
                "Forgot {name} on this computer. The server itself still runs Ant; remove it there with: \
                 sudo systemctl disable --now 'ant-worker@*' ant-worker ant-coordinator"
            );
            Ok(0)
        }
        _ => Err(USAGE.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_targets_are_validated() {
        assert_eq!(
            default_name("deploy@build.example.com"),
            "build.example.com"
        );
        assert_eq!(default_name("root@10.0.0.5"), "10.0.0.5");
        assert!(valid_name("build-box_1").is_ok());
        assert!(valid_name("../etc").is_err());
        assert!(valid_name(".hidden").is_err());
        assert!(valid_target("runner@127.0.0.1").is_ok());
        assert!(
            valid_target("-oProxyCommand=evil").is_err(),
            "no ssh options"
        );
        assert!(
            valid_target("host;rm -rf /").is_err(),
            "no shell metacharacters"
        );
    }

    #[test]
    fn the_setup_script_is_valid_sh_and_keeps_tokens() {
        // Structure checks that hold without a server; the script itself runs
        // for real on a Linux host in the deploy workflow.
        assert!(SETUP_SCRIPT.starts_with("set -eu\n"));
        assert!(
            SETUP_SCRIPT.contains("if [ ! -s \"$f\" ]"),
            "tokens survive redeploys"
        );
        assert!(
            SETUP_SCRIPT.contains("--listen 127.0.0.1:$PORT"),
            "coordinator stays on loopback"
        );
        assert!(!SETUP_SCRIPT.contains("--allow-non-loopback"));
        assert!(
            SETUP_SCRIPT.contains("sha256sum"),
            "binaries are verified before install"
        );
    }
}
