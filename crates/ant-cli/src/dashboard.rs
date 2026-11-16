//! `ant dashboard`: the local product dashboard (roadmap P7, decision D-042).
//!
//! Serves a small web app on `127.0.0.1` showing this machine's builds
//! (from the local build history), per project, with the deliverables each
//! build collected available for download. It refreshes itself every few
//! seconds, so a build started in a terminal shows up while you watch.
//!
//! Security model (same rules as the coordinator dashboard, D-015):
//! - loopback only, under a path holding a 128-bit random token from the OS
//!   CSPRNG (other local users and web pages cannot guess it);
//! - the `Host` header must be this loopback address and port, which defeats
//!   DNS-rebinding pages that try to read it through another name;
//! - only `GET`; a strict Content-Security-Policy (own script and style
//!   only, no inline code), `nosniff`, `no-referrer`, `no-store`, and no
//!   framing; the page renders data with `textContent` only;
//! - files are served only when a recorded build collected them and they
//!   still resolve inside that build's project.

use crate::history::{self, BuildRecord};
use crate::install::ActiveShare;
use local_executor::CancellationToken;
use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

const INDEX_HTML: &str = include_str!("../dashboard/index.html");
const APP_CSS: &str = include_str!("../dashboard/app.css");
const APP_JS: &str = include_str!("../dashboard/app.js");
/// The brand mark, straight from the repository's assets.
const LOGO_SVG: &str = include_str!("../../../docs/assets/logo-mark.svg");
/// The fine grain behind the glass surfaces (D-061), served same-origin
/// because the CSP allows no `data:` images.
const TEXTURE_SVG: &str = include_str!("../dashboard/texture.svg");

/// Builds the dashboard lists.
const SHOWN: usize = 200;

const USAGE: &str = "usage: ant dashboard [--port N] [--open]";

const SECURITY_HEADERS: &str = "Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nCache-Control: no-store\r\n";

pub fn dashboard_command(args: impl Iterator<Item = OsString>) -> Result<u8, String> {
    let mut port: u16 = 0;
    let mut open = false;
    let mut args = args.map(|arg| arg.to_string_lossy().into_owned());
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                port = args
                    .next()
                    .and_then(|value| value.parse().ok())
                    .ok_or("--port requires a port number")?;
            }
            "--open" => open = true,
            other => return Err(format!("unexpected argument '{other}'\n{USAGE}")),
        }
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .map_err(|error| format!("cannot listen on 127.0.0.1:{port}: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let address = listener.local_addr().map_err(|error| error.to_string())?;
    let token = crate::signing::random_hex(16)?;
    let url = format!("http://{address}/{token}/");
    println!("Ant dashboard: {url}");
    println!("Only this computer can open it. Press Ctrl+C to stop.");
    if open {
        open_in_browser(&url);
    }
    let cancellation = CancellationToken::new();
    crate::install_ctrl_c_handler(&cancellation);
    serve(&listener, address, &token, &cancellation);
    println!("Dashboard stopped.");
    Ok(0)
}

pub(crate) fn open_in_browser(url: &str) {
    // The URL is ours (address + hex token), so no shell quoting concerns.
    let result = if cfg!(windows) {
        std::process::Command::new("explorer").arg(url).spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    if result.is_err() {
        println!("Could not open a browser; open the address above yourself.");
    }
}

/// The dashboard's one piece of state that changes: the LAN share started
/// from "Install on phone" (D-046). `start` is injectable for tests.
pub(crate) struct ShareControl {
    slot: std::sync::Mutex<Option<ActiveShare>>,
    start: Box<StartShare>,
}

type StartShare = dyn Fn(PathBuf) -> Result<ActiveShare, String> + Send + Sync;

/// How long a share started from the dashboard stays up.
const SHARE_LIFETIME: Duration = Duration::from_secs(15 * 60);

impl ShareControl {
    fn new(start: Box<StartShare>) -> Self {
        Self {
            slot: std::sync::Mutex::new(None),
            start,
        }
    }

    /// The share still running, if any (expired ones are cleared).
    fn current_json(&self) -> serde_json::Value {
        let mut slot = self.slot.lock().expect("share slot");
        if slot
            .as_ref()
            .is_some_and(|share| share.cancel.is_cancelled())
        {
            *slot = None;
        }
        match slot.as_ref() {
            Some(share) => serde_json::json!({
                "file": share.file,
                "url": share.url,
                "qr": share.qr,
                "expires_ms": share.expires_ms,
            }),
            None => serde_json::Value::Null,
        }
    }

    fn stop(&self) {
        if let Some(share) = self.slot.lock().expect("share slot").take() {
            share.cancel.cancel();
        }
    }

    fn start(&self, apk: PathBuf) -> Result<(), String> {
        self.stop();
        let share = (self.start)(apk)?;
        *self.slot.lock().expect("share slot") = Some(share);
        Ok(())
    }
}

fn serve(
    listener: &TcpListener,
    address: SocketAddr,
    token: &str,
    cancellation: &CancellationToken,
) {
    // `ANT_SHARE_HOST` picks the network address to share on (machines
    // with several networks; tests use 127.0.0.1).
    let share_host = std::env::var("ANT_SHARE_HOST")
        .ok()
        .and_then(|value| value.parse().ok());
    let shares = ShareControl::new(Box::new(move |apk| {
        crate::install::start_share(apk, share_host, SHARE_LIFETIME)
    }));
    while !cancellation.is_cancelled() {
        match listener.accept() {
            Ok((stream, _)) => {
                // One request per connection; a bad client only hurts itself.
                let _ = handle(
                    stream,
                    address,
                    token,
                    &history::recent,
                    &history::running,
                    &|id, from| crate::build_log::read(id, from, LOG_CHUNK),
                    &shares,
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
    shares.stop();
}

struct Request {
    method: String,
    path: String,
    host: String,
    /// The dashboard's own scripts send `X-Ant-Dashboard: 1` on requests
    /// that change something. Other web pages cannot send a custom header
    /// to this origin (it needs a CORS preflight this server never
    /// approves), so its presence blocks cross-site request forgery.
    from_dashboard: bool,
    body: Vec<u8>,
}

/// Largest request body accepted (share requests are tiny JSON).
const MAX_BODY: usize = 4096;

fn read_request(stream: &TcpStream) -> std::io::Result<Request> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    let mut reader = BufReader::new(stream.take(16 * 1024));
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("").to_owned();
    let mut host = String::new();
    let mut from_dashboard = false;
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header == "\r\n" || header == "\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            let name = name.trim();
            let value = value.trim();
            if name.eq_ignore_ascii_case("host") {
                host = value.to_owned();
            } else if name.eq_ignore_ascii_case("x-ant-dashboard") {
                from_dashboard = value == "1";
            } else if name.eq_ignore_ascii_case("content-length") {
                length = value.parse().unwrap_or(usize::MAX);
            }
        }
    }
    let mut body = Vec::new();
    if length > 0 && length <= MAX_BODY {
        body.resize(length, 0);
        reader.read_exact(&mut body)?;
    }
    Ok(Request {
        method,
        path,
        host,
        from_dashboard,
        body,
    })
}

fn respond(
    stream: &mut TcpStream,
    status: &str,
    kind: &str,
    extra: &str,
    body: &[u8],
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n{SECURITY_HEADERS}{extra}Connection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

/// Answer one request. `builds` supplies the history (tests inject it).
fn handle(
    mut stream: TcpStream,
    address: SocketAddr,
    token: &str,
    builds: &dyn Fn(usize) -> Vec<BuildRecord>,
    running: &dyn Fn() -> Vec<history::RunningBuild>,
    logs: &LogReader,
    shares: &ShareControl,
) -> std::io::Result<()> {
    let request = read_request(&stream)?;
    let allowed_hosts = [
        format!("127.0.0.1:{}", address.port()),
        format!("localhost:{}", address.port()),
    ];
    if !allowed_hosts
        .iter()
        .any(|host| host.eq_ignore_ascii_case(&request.host))
    {
        return respond(&mut stream, "403 Forbidden", "text/plain", "", b"");
    }
    let base = format!("/{token}/");
    let Some(route) = request.path.strip_prefix(&base) else {
        return respond(&mut stream, "404 Not Found", "text/plain", "", b"");
    };
    // State changes (D-046): POST to the share routes, from the dashboard's
    // own scripts only.
    if request.method == "POST" {
        if !request.from_dashboard {
            return respond(&mut stream, "403 Forbidden", "text/plain", "", b"");
        }
        let reply = match route {
            "api/share" => start_share_request(&request.body, &builds(SHOWN), shares),
            "api/share/stop" => {
                shares.stop();
                Ok(())
            }
            _ => return respond(&mut stream, "404 Not Found", "text/plain", "", b""),
        };
        let body = match reply {
            Ok(()) => serde_json::json!({ "share": shares.current_json() }),
            Err(message) => serde_json::json!({ "share": shares.current_json(), "error": message }),
        }
        .to_string();
        return respond(
            &mut stream,
            "200 OK",
            "application/json",
            "",
            body.as_bytes(),
        );
    }
    if request.method != "GET" {
        return respond(&mut stream, "405 Method Not Allowed", "text/plain", "", b"");
    }
    match route {
        "api/share" => {
            let body = serde_json::json!({ "share": shares.current_json() }).to_string();
            respond(
                &mut stream,
                "200 OK",
                "application/json",
                "",
                body.as_bytes(),
            )
        }
        "" => respond(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            "",
            INDEX_HTML.as_bytes(),
        ),
        "app.css" => respond(
            &mut stream,
            "200 OK",
            "text/css; charset=utf-8",
            "",
            APP_CSS.as_bytes(),
        ),
        "logo.svg" => respond(
            &mut stream,
            "200 OK",
            "image/svg+xml",
            "",
            LOGO_SVG.as_bytes(),
        ),
        "texture.svg" => respond(
            &mut stream,
            "200 OK",
            "image/svg+xml",
            "",
            TEXTURE_SVG.as_bytes(),
        ),
        "app.js" => respond(
            &mut stream,
            "200 OK",
            "text/javascript; charset=utf-8",
            "",
            APP_JS.as_bytes(),
        ),
        "api/servers" => {
            let body = servers_json();
            respond(
                &mut stream,
                "200 OK",
                "application/json",
                "",
                body.as_bytes(),
            )
        }
        _ if route.starts_with("api/servers/") && route.ends_with("/status") => {
            let name = route["api/servers/".len()..route.len() - "/status".len()].to_owned();
            // Reaching a server takes an SSH round trip (seconds); answer
            // on a thread so the dashboard keeps serving meanwhile.
            std::thread::spawn(move || {
                let body = match crate::server::status_line(&name) {
                    Ok(line) => serde_json::json!({ "ok": true, "summary": line }),
                    Err(error) => serde_json::json!({ "ok": false, "error": error }),
                }
                .to_string();
                let _ = respond(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    "",
                    body.as_bytes(),
                );
            });
            Ok(())
        }
        "api/builds" => {
            let body = builds_json(&builds(SHOWN), &running());
            respond(
                &mut stream,
                "200 OK",
                "application/json",
                "",
                body.as_bytes(),
            )
        }
        _ if route.starts_with("api/log/") => {
            let rest = &route["api/log/".len()..];
            let (id, query) = rest.split_once('?').unwrap_or((rest, ""));
            let from = query
                .split('&')
                .find_map(|pair| pair.strip_prefix("from="))
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0);
            match logs(id, from) {
                Some((bytes, size)) => {
                    let body = log_json(&bytes, from.min(size), size);
                    respond(
                        &mut stream,
                        "200 OK",
                        "application/json",
                        "",
                        body.as_bytes(),
                    )
                }
                None => respond(&mut stream, "404 Not Found", "text/plain", "", b""),
            }
        }
        _ => match route
            .strip_prefix("files/")
            .and_then(|rest| file_for(rest, &builds(SHOWN)))
        {
            Some((path, name)) => {
                let body = std::fs::read(&path)?;
                let disposition = format!(
                    "Content-Disposition: attachment; filename=\"{}\"\r\n",
                    name.replace(['"', '\r', '\n'], "_")
                );
                respond(
                    &mut stream,
                    "200 OK",
                    "application/octet-stream",
                    &disposition,
                    &body,
                )
            }
            None => respond(&mut stream, "404 Not Found", "text/plain", "", b""),
        },
    }
}

/// Start sharing the APK a recorded build collected (`{"build", "index"}`),
/// under the same rules as downloads (recorded, still inside the project,
/// not replaced by a newer build).
fn start_share_request(
    body: &[u8],
    builds: &[BuildRecord],
    shares: &ShareControl,
) -> Result<(), String> {
    let request: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| "expected {\"build\": id, \"index\": n}".to_owned())?;
    let build = request["build"].as_str().ok_or("missing build")?;
    let index = request["index"].as_u64().ok_or("missing index")?;
    let (path, name) = file_for(&format!("{build}/{index}"), builds)
        .ok_or("that file is not available any more")?;
    if !name.to_ascii_lowercase().ends_with(".apk") {
        return Err("only Android apps (.apk) can be installed on a phone".to_owned());
    }
    shares.start(path)
}

/// Reads a build's log from an offset: `(bytes, current size)`.
type LogReader = dyn Fn(&str, u64) -> Option<(Vec<u8>, u64)>;

/// Bytes per log request; the page asks again from `next` for more.
const LOG_CHUNK: usize = 256 * 1024;

/// A log chunk as JSON: the text (never cut inside a UTF-8 character: an
/// incomplete trailing sequence waits for the next request), the offset to
/// ask from next, and the log's current size.
fn log_json(bytes: &[u8], start: u64, size: u64) -> String {
    let usable = match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(_) => bytes.len(),
    };
    let text = String::from_utf8_lossy(&bytes[..usable]);
    serde_json::json!({
        "text": text,
        "next": start + usable as u64,
        "size": size,
    })
    .to_string()
}

/// The JSON the page renders: builds newest first, each deliverable with
/// whether it still exists, its size, and its download path. A deliverable
/// a newer build of the same project collected again is `replaced`: the
/// file on disk is the newer build's, so it is not offered as this one's.
fn builds_json(builds: &[BuildRecord], running: &[history::RunningBuild]) -> String {
    let mut collected: std::collections::HashSet<(PathBuf, PathBuf)> =
        std::collections::HashSet::new();
    let items: Vec<serde_json::Value> = builds
        .iter()
        .map(|build| {
            let downloads: Vec<serde_json::Value> = build
                .deliverables
                .iter()
                .enumerate()
                .map(|(index, relative)| {
                    let full = build.project.join(relative);
                    let metadata = std::fs::metadata(&full).ok();
                    let replaced = !collected.insert((build.project.clone(), relative.clone()));
                    // A deliverable may be a whole folder (a web app, a
                    // published .NET app, an iOS .app bundle; D-051).
                    let folder = metadata.as_ref().is_some_and(|m| m.is_dir());
                    let size = if folder {
                        folder_size(&full)
                    } else {
                        metadata.as_ref().map(|m| m.len()).unwrap_or(0)
                    };
                    serde_json::json!({
                        "replaced": replaced,
                        "name": relative.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                        "path": relative.to_string_lossy(),
                        "exists": metadata.as_ref().is_some_and(|m| m.is_file() || m.is_dir()),
                        "folder": folder,
                        "bytes": size,
                        "url": format!("files/{}/{index}", build.id),
                    })
                })
                .collect();
            serde_json::json!({
                "id": build.id,
                "started_ms": build.started_ms,
                "project": build.project.to_string_lossy(),
                "adapter": build.adapter,
                "flavor": build.flavor,
                "succeeded": build.succeeded,
                "millis": build.millis,
                "steps": build.steps,
                "downloads": downloads,
            })
        })
        .collect();
    serde_json::json!({ "now_ms": history::now_ms(), "running": running, "builds": items })
        .to_string()
}

/// Total size of the files in a folder deliverable (bounded walk).
fn folder_size(dir: &std::path::Path) -> u64 {
    let mut total = 0;
    let mut pending = vec![dir.to_path_buf()];
    let mut visited = 0;
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > 100_000 {
                return total;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(entry.path()),
                Ok(kind) if kind.is_file() => {
                    total += entry.metadata().map(|m| m.len()).unwrap_or(0);
                }
                _ => {}
            }
        }
    }
    total
}

/// The deployed servers this computer knows (D-053): names and addresses
/// only; no tokens.
fn servers_json() -> String {
    let servers: Vec<serde_json::Value> = crate::server::all_profiles()
        .into_iter()
        .map(|profile| {
            serde_json::json!({
                "name": profile.name,
                "target": profile.target,
                "os": profile.os,
                "arch": profile.arch,
                "deployed_ms": profile.deployed_ms,
            })
        })
        .collect();
    serde_json::json!({ "servers": servers }).to_string()
}

/// `<build id>/<index>` → the deliverable's absolute path and file name,
/// only when it still resolves inside that build's project.
fn file_for(rest: &str, builds: &[BuildRecord]) -> Option<(PathBuf, String)> {
    let (id, index) = rest.split_once('/')?;
    let index: usize = index.parse().ok()?;
    let position = builds.iter().position(|build| build.id == id)?;
    let build = &builds[position];
    let relative = build.deliverables.get(index)?;
    // Only the newest build that collected this path owns the file now.
    if builds[..position]
        .iter()
        .any(|newer| newer.project == build.project && newer.deliverables.contains(relative))
    {
        return None;
    }
    let project = build.project.canonicalize().ok()?;
    let path = project.join(relative).canonicalize().ok()?;
    if !path.starts_with(&project) || !path.is_file() {
        return None;
    }
    let name = path.file_name()?.to_string_lossy().into_owned();
    Some((path, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::StepRecord;

    fn get(address: SocketAddr, path: &str, host: &str) -> (String, Vec<u8>) {
        let mut stream = TcpStream::connect(address).expect("connect");
        write!(stream, "GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n").expect("request");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("response");
        let split = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("header end");
        (
            String::from_utf8_lossy(&response[..split]).into_owned(),
            response[split + 4..].to_vec(),
        )
    }

    fn post(
        address: SocketAddr,
        path: &str,
        host: &str,
        body: &str,
        from_dashboard: bool,
    ) -> (String, Vec<u8>) {
        let mut stream = TcpStream::connect(address).expect("connect");
        let marker = if from_dashboard {
            "X-Ant-Dashboard: 1\r\n"
        } else {
            ""
        };
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: {host}\r\n{marker}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .expect("request");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("response");
        let split = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("header end");
        (
            String::from_utf8_lossy(&response[..split]).into_owned(),
            response[split + 4..].to_vec(),
        )
    }

    /// D-046: sharing an APK to a phone needs the dashboard's own header,
    /// only shares recorded APKs, serves the bytes, and stops on request.
    #[test]
    fn shares_recorded_apks_to_phones_and_stops() {
        let project = std::env::temp_dir().join(format!("ant-share-{}", std::process::id()));
        std::fs::create_dir_all(project.join("dist")).expect("dist");
        std::fs::write(project.join("dist").join("app-debug.apk"), b"PK-apk-bytes").expect("apk");
        std::fs::write(project.join("dist").join("tool.exe"), b"MZ").expect("exe");
        let record = BuildRecord {
            id: "300-5".to_owned(),
            started_ms: 300,
            project: project.clone(),
            adapter: "gradle".to_owned(),
            flavor: "debug".to_owned(),
            succeeded: true,
            millis: 10,
            steps: Vec::new(),
            deliverables: vec![
                PathBuf::from("dist/app-debug.apk"),
                PathBuf::from("dist/tool.exe"),
            ],
        };
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let address = listener.local_addr().expect("address");
        let token = "0123456789abcdef0123456789abcdef";
        let host = format!("127.0.0.1:{}", address.port());
        let server = std::thread::spawn(move || {
            let shares = ShareControl::new(Box::new(|apk| {
                crate::install::start_share(
                    apk,
                    Some(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST)),
                    Duration::from_secs(60),
                )
            }));
            for _ in 0..6 {
                let (stream, _) = listener.accept().expect("accept");
                let records = vec![record.clone()];
                handle(
                    stream,
                    address,
                    token,
                    &move |_| records.clone(),
                    &Vec::new,
                    &|_, _| None,
                    &shares,
                )
                .expect("handle");
            }
        });
        let start = format!("/{token}/api/share");
        let (head, _) = post(
            address,
            &start,
            &host,
            r#"{"build":"300-5","index":0}"#,
            false,
        );
        assert!(
            head.starts_with("HTTP/1.1 403"),
            "forged request without the header: {head}"
        );
        let (_, body) = post(
            address,
            &start,
            &host,
            r#"{"build":"300-5","index":1}"#,
            true,
        );
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert!(
            json["error"].as_str().unwrap_or("").contains(".apk"),
            "{json}"
        );
        let (head, body) = post(
            address,
            &start,
            &host,
            r#"{"build":"300-5","index":0}"#,
            true,
        );
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        let share = &json["share"];
        assert_eq!(share["file"], "app-debug.apk");
        let url = share["url"].as_str().expect("url").to_owned();
        assert!(share["qr"].as_array().expect("qr").len() >= 21, "{share}");
        let (_, body) = get(address, &format!("/{token}/api/share"), &host);
        assert!(String::from_utf8_lossy(&body).contains("app-debug.apk"));
        // A phone downloads the APK from the share.
        let share_address: SocketAddr = url
            .trim_start_matches("http://")
            .split('/')
            .next()
            .expect("address")
            .parse()
            .expect("socket address");
        let share_path = format!("/{}", url.splitn(4, '/').nth(3).expect("path"));
        let (head, apk) = get(
            share_address,
            &format!("{share_path}app-debug.apk"),
            "phone",
        );
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(apk, b"PK-apk-bytes");
        let (_, body) = post(
            address,
            &format!("/{token}/api/share/stop"),
            &host,
            "",
            true,
        );
        assert!(
            String::from_utf8_lossy(&body).contains("\"share\":null"),
            "{}",
            String::from_utf8_lossy(&body)
        );
        let (_, body) = get(address, &format!("/{token}/api/share"), &host);
        assert!(String::from_utf8_lossy(&body).contains("\"share\":null"));
        server.join().expect("server");
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            TcpStream::connect(share_address).is_err(),
            "the share stops listening once stopped"
        );
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn log_chunks_never_split_a_character() {
        let text = "café";
        let bytes = text.as_bytes();
        // Cut inside the two-byte "é": the partial byte waits for later.
        let cut = &bytes[..bytes.len() - 1];
        let json: serde_json::Value = serde_json::from_str(&log_json(cut, 10, 99)).expect("json");
        assert_eq!(json["text"], "caf");
        assert_eq!(json["next"], 13);
    }

    #[test]
    fn serves_the_app_history_and_only_recorded_files() {
        let project = std::env::temp_dir().join(format!("ant-dashboard-{}", std::process::id()));
        std::fs::create_dir_all(project.join("dist")).expect("dist");
        std::fs::write(project.join("dist").join("app.exe"), b"binary").expect("artifact");
        std::fs::write(project.join("secret.txt"), b"not a deliverable").expect("other file");
        let record = BuildRecord {
            id: "123-9".to_owned(),
            started_ms: 123,
            project: project.clone(),
            adapter: "cargo".to_owned(),
            flavor: "release".to_owned(),
            succeeded: true,
            millis: 1500,
            steps: vec![StepRecord {
                id: "workspace".to_owned(),
                status: "reused".to_owned(),
                millis: 3,
            }],
            deliverables: vec![
                PathBuf::from("dist/app.exe"),
                PathBuf::from("../escape.txt"),
            ],
        };
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let address = listener.local_addr().expect("address");
        let token = "0123456789abcdef0123456789abcdef";
        let host = format!("127.0.0.1:{}", address.port());
        let server = std::thread::spawn(move || {
            let mut older = record.clone();
            older.id = "100-9".to_owned();
            older.started_ms = 100;
            older.deliverables = vec![PathBuf::from("dist/app.exe")];
            for _ in 0..13 {
                let (stream, _) = listener.accept().expect("accept");
                let records = vec![record.clone(), older.clone()];
                let live = vec![history::RunningBuild {
                    id: "200-1".to_owned(),
                    adapter: "go".to_owned(),
                    total_steps: 1,
                    current: vec!["go-build".to_owned()],
                    ..history::RunningBuild::default()
                }];
                handle(
                    stream,
                    address,
                    token,
                    &move |_| records.clone(),
                    &move || live.clone(),
                    &|id, from| {
                        let log =
                            "Compiling app\n\u{1b}[32mFinished\u{1b}[0m caf\u{e9}\n".as_bytes();
                        (id == "123-9").then(|| {
                            let start = (from as usize).min(log.len());
                            (log[start..].to_vec(), log.len() as u64)
                        })
                    },
                    &ShareControl::new(Box::new(|_| Err("no sharing in this test".to_owned()))),
                )
                .expect("handle");
            }
        });

        let (head, body) = get(address, &format!("/{token}/"), &host);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(
            head.contains("Content-Security-Policy: default-src 'none'"),
            "{head}"
        );
        assert!(String::from_utf8_lossy(&body).contains("app.js"));
        let (head, logo) = get(address, &format!("/{token}/logo.svg"), &host);
        assert!(head.contains("image/svg+xml"), "{head}");
        assert!(String::from_utf8_lossy(&logo).contains("<svg"));
        // The glass surfaces' grain (D-061) is same-origin: the CSP has no
        // data: images, and the page loads nothing from elsewhere.
        let (head, texture) = get(address, &format!("/{token}/texture.svg"), &host);
        assert!(head.contains("image/svg+xml"), "{head}");
        assert!(String::from_utf8_lossy(&texture).contains("feTurbulence"));
        assert!(!APP_CSS.contains("http") && !APP_JS.contains("https://"));
        let (head, _) = get(address, "/", &host);
        assert!(head.starts_with("HTTP/1.1 404"), "no token: {head}");
        let (head, _) = get(address, &format!("/{token}/"), "evil.example:80");
        assert!(
            head.starts_with("HTTP/1.1 403"),
            "DNS rebinding host: {head}"
        );
        let (head, body) = get(address, &format!("/{token}/api/builds"), &host);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(json["builds"][0]["adapter"], "cargo");
        assert_eq!(json["running"][0]["current"][0], "go-build");
        assert_eq!(json["builds"][0]["downloads"][0]["exists"], true);
        assert_eq!(json["builds"][0]["downloads"][0]["bytes"], 6);
        assert_eq!(json["builds"][0]["downloads"][0]["replaced"], false);
        assert_eq!(
            json["builds"][1]["downloads"][0]["replaced"], true,
            "the newer build owns dist/app.exe now"
        );
        let (head, body) = get(address, &format!("/{token}/files/123-9/0"), &host);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(body, b"binary");
        let (head, _) = get(address, &format!("/{token}/files/123-9/1"), &host);
        assert!(
            head.starts_with("HTTP/1.1 404"),
            "outside the project: {head}"
        );
        let (head, _) = get(address, &format!("/{token}/files/123-9/7"), &host);
        assert!(head.starts_with("HTTP/1.1 404"), "{head}");
        let (head, _) = get(address, &format!("/{token}/files/100-9/0"), &host);
        assert!(head.starts_with("HTTP/1.1 404"), "replaced: {head}");
        let (head, body) = get(address, &format!("/{token}/api/log/123-9?from=0"), &host);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let chunk: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert!(
            chunk["text"].as_str().unwrap().contains("Finished"),
            "{chunk}"
        );
        assert_eq!(chunk["next"], chunk["size"]);
        let (head, _) = get(address, &format!("/{token}/api/log/nope"), &host);
        assert!(head.starts_with("HTTP/1.1 404"), "{head}");
        let (head, _) = get(address, &format!("/{token}/files/nope/0"), &host);
        assert!(head.starts_with("HTTP/1.1 404"), "{head}");
        server.join().expect("server");
        let _ = std::fs::remove_dir_all(&project);
    }
}
