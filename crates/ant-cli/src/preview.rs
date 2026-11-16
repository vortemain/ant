//! `ant preview` (roadmap P9, decision D-064): an Expo Go-style live preview
//! over the local network, with over-the-air updates to every phone and
//! browser that opened it.
//!
//! It builds once, serves the result on this computer's Wi-Fi address behind
//! an unguessable link (and a terminal QR code), then watches the project and
//! rebuilds on every saved change:
//!
//! - Web apps (a deliverable folder or `dist/` with an `index.html`: Vite,
//!   Flutter web, ...) are served as they are, with one small script added
//!   to HTML pages that reloads them when a new build is ready.
//! - Android apps get a page with the current APK; when a new build is ready
//!   it shows "Update ready" with the new install link, and phones connected
//!   over USB are updated with `adb install -r` at once.
//! - Anything else is offered for download, newest first.
//!
//! Only this computer's builds are served, only under the random path, only
//! to the local network, and nothing is accepted from the phones.

use crate::install::{
    connected_devices, file_name, find_adb, html_escape, install_with_adb, lan_address, terminal_qr,
};
use local_executor::CancellationToken;
use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const USAGE: &str =
    "usage: ant preview [--dev|--release] [--host ADDRESS] [--port N] [--project PATH]";

/// What the latest successful build offers.
#[derive(Clone, Default)]
struct Served {
    version: u64,
    building: bool,
    last_ok: bool,
    /// Folder of a web app (has an index.html).
    web_root: Option<PathBuf>,
    apks: Vec<PathBuf>,
    files: Vec<PathBuf>,
}

pub fn preview_command(args: impl Iterator<Item = OsString>) -> Result<u8, String> {
    let mut build_args: Vec<OsString> = Vec::new();
    let mut host: Option<IpAddr> = std::env::var("ANT_SHARE_HOST")
        .ok()
        .and_then(|value| value.parse().ok());
    let mut port: u16 = 0;
    let mut project: Option<PathBuf> = None;
    let mut args = args.map(|arg| arg.to_string_lossy().into_owned());
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dev" | "--release" => build_args.push(arg.into()),
            "--host" => {
                host = Some(
                    args.next()
                        .and_then(|value| value.parse().ok())
                        .ok_or("--host needs an IP address")?,
                )
            }
            "--port" => {
                port = args
                    .next()
                    .and_then(|value| value.parse().ok())
                    .ok_or("--port needs a number")?
            }
            "--project" => {
                let path = args.next().ok_or("--project needs a path")?;
                build_args.extend(["--project".into(), path.clone().into()]);
                project = Some(PathBuf::from(path));
            }
            other => return Err(format!("unexpected argument '{other}'\n{USAGE}")),
        }
    }
    let root = match project {
        Some(path) => path,
        None => std::env::current_dir().map_err(|error| error.to_string())?,
    }
    .canonicalize()
    .map_err(|error| format!("cannot access the project: {error}"))?;

    let state = Arc::new(Mutex::new(Served::default()));
    rebuild(&root, &build_args, &state);
    if !state.lock().map_err(|_| "state poisoned")?.last_ok {
        println!(
            "\nThe first build failed; fix it and save: the preview starts as soon as a build succeeds."
        );
    }

    let host = host
        .or_else(lan_address)
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let listener = TcpListener::bind(SocketAddr::new(host, port))
        .map_err(|error| format!("cannot listen on {host}:{port}: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let bound = listener.local_addr().map_err(|error| error.to_string())?;
    let token = crate::signing::random_hex(16)?;
    let base = format!("/{token}/");
    let url = format!("http://{bound}{base}");
    println!("\n{}", terminal_qr(&url)?);
    println!("Live preview: scan with your phone (same Wi-Fi), or open\n  {url}");
    println!("Every save rebuilds; open pages update by themselves. Press Ctrl+C to stop.");
    if cfg!(windows) && !host.is_loopback() {
        println!(
            "If Windows asks whether to allow Ant on private networks, allow it so phones can connect."
        );
    }
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    let cancellation = CancellationToken::new();
    crate::install_ctrl_c_handler(&cancellation);
    {
        let state = state.clone();
        let base = base.clone();
        std::thread::spawn(move || serve(&listener, &base, &state));
    }
    let mut seen = signature(&root);
    while !cancellation.is_cancelled() {
        std::thread::sleep(Duration::from_millis(500));
        let now = signature(&root);
        if now == seen {
            continue;
        }
        // Let a burst of saves settle before building.
        std::thread::sleep(Duration::from_millis(300));
        seen = signature(&root);
        println!("\nChange detected; rebuilding…");
        rebuild(&root, &build_args, &state);
    }
    Ok(0)
}

/// Build, then publish the result to the preview (and to USB phones).
fn rebuild(root: &Path, build_args: &[OsString], state: &Arc<Mutex<Served>>) {
    if let Ok(mut served) = state.lock() {
        served.building = true;
    }
    let started = Instant::now();
    let built = crate::front::build_and_collect(build_args.iter().cloned());
    let mut served = match state.lock() {
        Ok(served) => served,
        Err(_) => return,
    };
    served.building = false;
    let deliverables = match built {
        Ok((0, deliverables)) => deliverables,
        Ok(_) | Err(_) => {
            if let Err(error) = &built {
                println!("ant: {error}");
            }
            served.last_ok = false;
            return;
        }
    };
    served.last_ok = true;
    served.version += 1;
    served.apks = deliverables
        .iter()
        .filter(|path| path.extension().is_some_and(|extension| extension == "apk"))
        .cloned()
        .collect();
    served.web_root = deliverables
        .iter()
        .find(|path| path.join("index.html").is_file())
        .cloned()
        .or_else(|| {
            let dist = root.join("dist");
            dist.join("index.html").is_file().then_some(dist)
        });
    served.files = deliverables
        .into_iter()
        .filter(|path| path.is_file())
        .collect();
    let version = served.version;
    let apks = served.apks.clone();
    drop(served);
    println!(
        "Preview updated (version {version}, {:.1}s).",
        started.elapsed().as_secs_f64()
    );
    // Phones on USB get the new APK right away.
    if !apks.is_empty()
        && let Some(adb) = find_adb()
        && let Ok(devices) = connected_devices(&adb)
        && !devices.is_empty()
    {
        let _ = install_with_adb(&adb, &devices, &apks);
    }
}

/// Folders that never hold sources (tool outputs, dependencies, VCS).
const IGNORED: &[&str] = &[
    ".ant",
    ".git",
    "node_modules",
    "target",
    "build",
    "dist",
    ".gradle",
    ".dart_tool",
    ".idea",
    ".vscode",
    "out",
    ".next",
    ".turbo",
    "coverage",
    "__pycache__",
    ".venv",
    "bin",
    "obj",
];

/// A cheap fingerprint of the project's files (paths, sizes, times).
fn signature(root: &Path) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut pending = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            visited += 1;
            if visited > 200_000 {
                return hasher.finish();
            }
            let name = entry.file_name();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if !IGNORED.contains(&name.to_string_lossy().as_ref()) {
                    pending.push(entry.path());
                }
            } else if let Ok(metadata) = entry.metadata() {
                entry.path().hash(&mut hasher);
                metadata.len().hash(&mut hasher);
                metadata.modified().ok().hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

fn serve(listener: &TcpListener, base: &str, state: &Arc<Mutex<Served>>) {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let state = state.clone();
                let base = base.to_owned();
                std::thread::spawn(move || {
                    let _ = handle(stream, &base, &state);
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

/// The script added to served HTML pages and to the APK page: when a newer
/// build is ready, reload (or reveal the update link). Polling keeps it
/// working through any proxy and on every browser.
fn live_script(version: u64, base: &str) -> String {
    format!(
        "(function(){{var v={version};function t(){{fetch('{base}__ant/version',{{cache:'no-store'}}).then(function(r){{return r.json()}}).then(function(s){{if(s.version>v){{var u=document.getElementById('ant-update');if(u){{u.hidden=false;v=s.version;var a=document.getElementById('ant-latest');if(a){{a.href=s.apk||a.href}}}}else{{location.reload()}}}}}}).catch(function(){{}}).finally(function(){{setTimeout(t,1000)}})}}setTimeout(t,1000)}})();"
    )
}

fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        "apk" => "application/vnd.android.package-archive",
        _ => "application/octet-stream",
    }
}

fn respond(stream: &mut TcpStream, status: &str, kind: &str, body: &[u8]) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)
}

fn handle(stream: TcpStream, base: &str, state: &Arc<Mutex<Served>>) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(120)))?;
    let mut reader = BufReader::new(stream.take(16 * 1024));
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }
    let mut stream = reader.into_inner().into_inner();
    let mut parts = request_line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if method != "GET" {
        return respond(&mut stream, "405 Method Not Allowed", "text/plain", b"");
    }
    let path = target.split(['?', '#']).next().unwrap_or("");
    let Some(rest) = path.strip_prefix(base) else {
        return respond(&mut stream, "404 Not Found", "text/plain", b"");
    };
    let served = state
        .lock()
        .map(|served| served.clone())
        .unwrap_or_default();
    if rest == "__ant/version" {
        let apk = (!served.apks.is_empty()).then(|| format!("__ant/apk/{}/0", served.version));
        let body = serde_json::json!({
            "version": served.version, "building": served.building, "ok": served.last_ok, "apk": apk,
        })
        .to_string();
        return respond(&mut stream, "200 OK", "application/json", body.as_bytes());
    }
    if let Some(index) = rest.strip_prefix("__ant/apk/") {
        // `__ant/apk/<version>/<index>`: the version only defeats caches.
        let index: Option<usize> = index
            .rsplit('/')
            .next()
            .and_then(|value| value.parse().ok());
        return match index.and_then(|index| served.apks.get(index)) {
            Some(apk) => {
                let body = std::fs::read(apk)?;
                respond(&mut stream, "200 OK", content_type(apk), &body)
            }
            None => respond(&mut stream, "404 Not Found", "text/plain", b""),
        };
    }
    if let Some(index) = rest.strip_prefix("__ant/file/") {
        let index: Option<usize> = index
            .rsplit('/')
            .next()
            .and_then(|value| value.parse().ok());
        return match index.and_then(|index| served.files.get(index)) {
            Some(file) => {
                let body = std::fs::read(file)?;
                respond(&mut stream, "200 OK", "application/octet-stream", &body)
            }
            None => respond(&mut stream, "404 Not Found", "text/plain", b""),
        };
    }
    if let Some(root) = &served.web_root {
        return serve_web(&mut stream, root, rest, served.version, base);
    }
    if rest.is_empty() {
        let page = app_page(&served, base);
        return respond(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            page.as_bytes(),
        );
    }
    respond(&mut stream, "404 Not Found", "text/plain", b"")
}

/// A web app's file, with the live script added to HTML pages. Only paths
/// inside the app's folder are served.
fn serve_web(
    stream: &mut TcpStream,
    root: &Path,
    rest: &str,
    version: u64,
    base: &str,
) -> std::io::Result<()> {
    let relative = if rest.is_empty() || rest.ends_with('/') {
        format!("{rest}index.html")
    } else {
        rest.to_owned()
    };
    let relative = percent_decode(&relative);
    let candidate = Path::new(&relative);
    if candidate
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return respond(stream, "404 Not Found", "text/plain", b"");
    }
    let mut file = root.join(candidate);
    if !file.is_file() {
        // Single-page apps route in the browser: fall back to index.html.
        if candidate.extension().is_none() {
            file = root.join("index.html");
        } else {
            return respond(stream, "404 Not Found", "text/plain", b"");
        }
    }
    let (Ok(real), Ok(real_root)) = (file.canonicalize(), root.canonicalize()) else {
        return respond(stream, "404 Not Found", "text/plain", b"");
    };
    if !real.starts_with(&real_root) {
        return respond(stream, "404 Not Found", "text/plain", b"");
    }
    let mut body = std::fs::read(&real)?;
    let kind = content_type(&real);
    if kind.starts_with("text/html") {
        let tag = format!("<script>{}</script>", live_script(version, base));
        let text = String::from_utf8_lossy(&body).into_owned();
        let text = match text.rfind("</body>") {
            Some(at) => format!("{}{tag}{}", &text[..at], &text[at..]),
            None => format!("{text}{tag}"),
        };
        body = text.into_bytes();
    }
    respond(stream, "200 OK", kind, &body)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(value) = u8::from_str_radix(&text[index + 1..index + 3], 16)
        {
            out.push(value);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The page for apps that are not web apps: the newest APK (with an update
/// banner) or the other deliverables.
fn app_page(served: &Served, base: &str) -> String {
    let mut page = String::from(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>Ant preview</title>\
         <body style=\"font:17px/1.5 system-ui,sans-serif;margin:0;background:#0b0c0f;color:#eceef2\">\
         <main style=\"max-width:520px;margin:0 auto;padding:28px 20px\"><h1 style=\"font-size:24px;margin:0 0 4px\">Live preview</h1>",
    );
    page.push_str(&format!(
        "<p style=\"color:#a3a9b4;margin:0 0 20px\">Build {}{}.</p>",
        served.version,
        if served.building {
            " · building the next one"
        } else {
            ""
        }
    ));
    page.push_str(
        "<p id=\"ant-update\" hidden style=\"background:#3a2414;color:#ffb27a;padding:12px 14px;border-radius:10px\">\
         Update ready. <a id=\"ant-latest\" style=\"color:#ffd4ad;font-weight:600\" href=\"#\">Install the new version</a></p>",
    );
    for (index, apk) in served.apks.iter().enumerate() {
        let name = html_escape(&file_name(apk));
        page.push_str(&format!(
            "<p><a href=\"__ant/apk/{}/{index}\" style=\"display:inline-block;background:#ff8a3d;color:#1a0e05;padding:12px 18px;border-radius:12px;font-weight:700;text-decoration:none\">Install {name}</a></p>",
            served.version
        ));
    }
    if served.apks.is_empty() {
        for (index, file) in served.files.iter().enumerate() {
            let name = html_escape(&file_name(file));
            page.push_str(&format!(
                "<p><a href=\"__ant/file/{}/{index}\" style=\"color:#ffb27a\">Download {name}</a></p>",
                served.version
            ));
        }
        if served.files.is_empty() {
            page.push_str("<p>Nothing to show yet: this project's build has no deliverables.</p>");
        }
    } else {
        page.push_str("<p style=\"color:#a3a9b4\">Android asks once to allow installs from your browser. This page offers each new build as soon as it is ready.</p>");
    }
    page.push_str(&format!(
        "<script>{}</script></main>",
        live_script(served.version, base)
    ));
    page
}
