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

fn open_in_browser(url: &str) {
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

fn serve(
    listener: &TcpListener,
    address: SocketAddr,
    token: &str,
    cancellation: &CancellationToken,
) {
    while !cancellation.is_cancelled() {
        match listener.accept() {
            Ok((stream, _)) => {
                // One request per connection; a bad client only hurts itself.
                let _ = handle(stream, address, token, &history::recent, &history::running);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

struct Request {
    method: String,
    path: String,
    host: String,
}

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
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header == "\r\n" || header == "\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.trim().eq_ignore_ascii_case("host")
        {
            host = value.trim().to_owned();
        }
    }
    Ok(Request { method, path, host })
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
    if request.method != "GET" {
        return respond(&mut stream, "405 Method Not Allowed", "text/plain", "", b"");
    }
    let base = format!("/{token}/");
    let Some(route) = request.path.strip_prefix(&base) else {
        return respond(&mut stream, "404 Not Found", "text/plain", "", b"");
    };
    match route {
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
        "app.js" => respond(
            &mut stream,
            "200 OK",
            "text/javascript; charset=utf-8",
            "",
            APP_JS.as_bytes(),
        ),
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
                    let metadata = std::fs::metadata(build.project.join(relative)).ok();
                    let replaced = !collected.insert((build.project.clone(), relative.clone()));
                    serde_json::json!({
                        "replaced": replaced,
                        "name": relative.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                        "path": relative.to_string_lossy(),
                        "exists": metadata.as_ref().is_some_and(|m| m.is_file()),
                        "bytes": metadata.map(|m| m.len()).unwrap_or(0),
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
            for _ in 0..10 {
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
        let (head, _) = get(address, &format!("/{token}/files/nope/0"), &host);
        assert!(head.starts_with("HTTP/1.1 404"), "{head}");
        server.join().expect("server");
        let _ = std::fs::remove_dir_all(&project);
    }
}
