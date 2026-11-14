//! `ant install`: put the Android app on a phone (roadmap P4, decision D-034).
//!
//! It builds first (through `ant build`, so an unchanged project is reused
//! in well under a second), then:
//!
//! - **USB:** when `adb` finds connected devices, `adb install -r` on each.
//! - **Wi-Fi (no device, or `--qr`):** serves the APK on this computer's
//!   local network and prints a QR code; the phone scans it and downloads
//!   the app. The server is deliberately small: plain HTTP on one interface
//!   (the LAN address, or `--host`), only `GET`, only the built APKs, under a
//!   path holding a 128-bit random token from the OS CSPRNG (anything else
//!   is a bare 404), until Ctrl+C or, with `--once`, the first complete
//!   download. Anyone on the same network who learns the URL can download
//!   the APK while it runs; nothing else on this machine is reachable
//!   through it.

use local_executor::CancellationToken;
use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const INSTALL_USAGE: &str = "usage: ant install [android] [--release] [--qr] [--once] [--host ADDRESS] [--port N] [--project PATH]";

pub fn install_command(args: impl Iterator<Item = OsString>) -> Result<u8, String> {
    let mut build_args: Vec<OsString> = Vec::new();
    let mut qr = false;
    let mut once = false;
    let mut host: Option<IpAddr> = None;
    let mut port: u16 = 0;
    let mut args = args.map(|arg| arg.to_string_lossy().into_owned());
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "android" | "--release" | "--fresh" | "--verbose" => build_args.push(arg.into()),
            "--project" => {
                build_args.push("--project".into());
                build_args.push(args.next().ok_or("--project requires a path")?.into());
            }
            "--qr" => qr = true,
            "--once" => once = true,
            "--host" => {
                let value = args.next().ok_or("--host requires an address")?;
                host = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--host '{value}' is not an IP address"))?,
                );
            }
            "--port" => {
                port = args
                    .next()
                    .and_then(|value| value.parse().ok())
                    .ok_or("--port requires a port number")?;
            }
            other => return Err(format!("unexpected argument '{other}'\n{INSTALL_USAGE}")),
        }
    }
    let (code, deliverables) = crate::front::build_and_collect(build_args.into_iter())?;
    if code != 0 {
        return Ok(code);
    }
    let apks: Vec<PathBuf> = deliverables
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "apk"))
        .collect();
    if apks.is_empty() {
        return Err(
            "this build produced no APK to install (`ant install` is for Android projects)"
                .to_owned(),
        );
    }

    if !qr {
        match find_adb() {
            Some(adb) => {
                let devices = connected_devices(&adb)?;
                if !devices.is_empty() {
                    return install_with_adb(&adb, &devices, &apks);
                }
                println!(
                    "\nNo Android device is connected over USB; sharing the app over your network instead."
                );
            }
            None => println!(
                "\nadb was not found (Android SDK platform-tools); sharing the app over your network instead."
            ),
        }
    }
    serve(&apks, host, port, once)
}

// ---------------------------------------------------------------------------
// USB
// ---------------------------------------------------------------------------

/// `adb` from the Android SDK (env, then default install locations), else
/// from `PATH`.
fn find_adb() -> Option<PathBuf> {
    let name = format!("adb{}", std::env::consts::EXE_SUFFIX);
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let mut sdks: Vec<PathBuf> = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
        .iter()
        .filter_map(|name| std::env::var_os(name).filter(|value| !value.is_empty()))
        .map(PathBuf::from)
        .collect();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        sdks.push(PathBuf::from(local).join("Android").join("Sdk"));
    }
    if let Some(home) = home {
        sdks.push(home.join("Android").join("Sdk"));
        sdks.push(home.join("Library").join("Android").join("sdk"));
    }
    sdks.into_iter()
        .map(|sdk| sdk.join("platform-tools").join(&name))
        .find(|candidate| candidate.is_file())
        .or_else(|| {
            Command::new(&name)
                .arg("version")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|_| PathBuf::from(&name))
        })
}

/// Serials of devices `adb devices` reports as ready (`device` state).
fn connected_devices(adb: &Path) -> Result<Vec<String>, String> {
    let output = Command::new(adb)
        .arg("devices")
        .output()
        .map_err(|error| format!("cannot run {}: {error}", adb.display()))?;
    Ok(parse_devices(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_devices(listing: &str) -> Vec<String> {
    listing
        .lines()
        .skip_while(|line| !line.starts_with("List of devices"))
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let serial = fields.next()?;
            (fields.next() == Some("device")).then(|| serial.to_owned())
        })
        .collect()
}

fn install_with_adb(adb: &Path, devices: &[String], apks: &[PathBuf]) -> Result<u8, String> {
    let mut failed = false;
    for device in devices {
        for apk in apks {
            let name = file_name(apk);
            println!("\nInstalling {name} on {device}…");
            let output = Command::new(adb)
                .args(["-s", device, "install", "-r"])
                .arg(apk)
                .output()
                .map_err(|error| format!("cannot run {}: {error}", adb.display()))?;
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            if output.status.success() && text.contains("Success") {
                println!("  ✓ installed");
            } else {
                failed = true;
                println!("  ✗ {}", text.trim());
                if text.contains("INSTALL_FAILED_UPDATE_INCOMPATIBLE") {
                    println!(
                        "    The phone has this app signed with another key (for example a debug build). Uninstall it first, then run ant install again."
                    );
                }
            }
        }
    }
    Ok(if failed { 1 } else { 0 })
}

// ---------------------------------------------------------------------------
// Wi-Fi + QR
// ---------------------------------------------------------------------------

/// This computer's address on its default route, found without sending
/// anything (a UDP "connect" only picks the route).
fn lan_address() -> Option<IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    let address = socket.local_addr().ok()?.ip();
    (!address.is_unspecified() && !address.is_loopback()).then_some(address)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The QR code for `text`, drawn with half-block characters. Light modules
/// are drawn, so it scans on the usual dark terminal background.
pub fn terminal_qr(text: &str) -> Result<String, String> {
    use qrcode::render::unicode::Dense1x2;
    let code = qrcode::QrCode::new(text.as_bytes()).map_err(|error| error.to_string())?;
    Ok(code
        .render::<Dense1x2>()
        .dark_color(Dense1x2::Light)
        .light_color(Dense1x2::Dark)
        .quiet_zone(true)
        .build())
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn serve(apks: &[PathBuf], host: Option<IpAddr>, port: u16, once: bool) -> Result<u8, String> {
    let host = match host.or_else(lan_address) {
        Some(host) => host,
        None => {
            println!(
                "Could not find this computer's network address; serving on 127.0.0.1 (this computer only). Pass --host with your Wi-Fi address."
            );
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        }
    };
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
    println!("Scan with your phone's camera (same Wi-Fi as this computer), or open:\n  {url}");
    println!(
        "Android asks once to allow installs from your browser. {}",
        if once {
            "Stops after the first download."
        } else {
            "Press Ctrl+C to stop sharing."
        }
    );
    if cfg!(windows) && !host.is_loopback() {
        println!(
            "If Windows asks whether to allow Ant on private networks, allow it so the phone can connect."
        );
    }

    let cancellation = CancellationToken::new();
    crate::install_ctrl_c_handler(&cancellation);
    let code = run_server(&listener, &base, apks, once, &cancellation)?;
    if cancellation.is_cancelled() {
        println!("Stopped sharing.");
    }
    Ok(code)
}

/// Accept and answer requests until cancelled or, with `once`, until one
/// APK was sent completely.
fn run_server(
    listener: &TcpListener,
    base: &str,
    apks: &[PathBuf],
    once: bool,
    cancellation: &CancellationToken,
) -> Result<u8, String> {
    while !cancellation.is_cancelled() {
        match listener.accept() {
            Ok((stream, peer)) => {
                match handle(stream, base, apks) {
                    Ok(Some(sent)) => {
                        println!("  ✓ sent {sent} to {}", peer.ip());
                        if once {
                            return Ok(0);
                        }
                    }
                    Ok(None) => {}
                    // A dropped or malformed connection only affects that client.
                    Err(error) => println!("  · {} disconnected: {error}", peer.ip()),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(format!("network error: {error}")),
        }
    }
    Ok(0)
}

/// Answer one request; returns the APK name when one was sent completely.
fn handle(stream: TcpStream, base: &str, apks: &[PathBuf]) -> std::io::Result<Option<String>> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    let mut reader = BufReader::new(stream.take(8192));
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    // Drain the (bounded) headers; nothing in them changes the answer.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }
    let mut stream = reader.into_inner().into_inner();
    let mut parts = request_line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let respond = |stream: &mut TcpStream, status: &str, kind: &str, body: &[u8]| {
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .and_then(|_| stream.write_all(body))
    };
    if method != "GET" {
        respond(&mut stream, "405 Method Not Allowed", "text/plain", b"")?;
        return Ok(None);
    }
    if path == base {
        let mut page = String::from(
            "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><title>Install</title><body style=\"font:18px system-ui;margin:24px\"><h1>Install</h1>",
        );
        for apk in apks {
            let name = html_escape(&file_name(apk));
            page.push_str(&format!(
                "<p><a href=\"{name}\" style=\"font-size:22px\">Download {name}</a></p>"
            ));
        }
        page.push_str("<p>Built with Ant.</p>");
        respond(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            page.as_bytes(),
        )?;
        return Ok(None);
    }
    let requested = path.strip_prefix(base).unwrap_or("");
    let Some(apk) = apks
        .iter()
        .find(|apk| !requested.is_empty() && file_name(apk) == requested)
    else {
        respond(&mut stream, "404 Not Found", "text/plain", b"")?;
        return Ok(None);
    };
    let body = std::fs::read(apk)?;
    let name = file_name(apk);
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.android.package-archive\r\nContent-Length: {}\r\nContent-Disposition: attachment; filename=\"{name}\"\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    stream.flush()?;
    Ok(Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(address: SocketAddr, path: &str) -> (String, Vec<u8>) {
        let mut stream = TcpStream::connect(address).expect("connect");
        write!(stream, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").expect("request");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("response");
        let split = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("header end");
        let head = String::from_utf8_lossy(&response[..split]).into_owned();
        (head, response[split + 4..].to_vec())
    }

    #[test]
    fn serves_only_the_apk_under_the_token_and_stops_after_one_download() {
        let dir = std::env::temp_dir().join(format!("ant-install-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let apk = dir.join("app-debug.apk");
        let content: Vec<u8> = (0..20_000u32).map(|n| (n % 251) as u8).collect();
        std::fs::write(&apk, &content).expect("apk");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let address = listener.local_addr().expect("address");
        let base = "/0123456789abcdef0123456789abcdef/".to_owned();
        let apks = vec![apk.clone()];
        let server = {
            let base = base.clone();
            std::thread::spawn(move || {
                run_server(&listener, &base, &apks, true, &CancellationToken::new())
            })
        };

        let (head, _) = get(address, "/app-debug.apk");
        assert!(head.starts_with("HTTP/1.1 404"), "{head}");
        let (head, _) = get(address, "/ffffffffffffffffffffffffffffffff/app-debug.apk");
        assert!(head.starts_with("HTTP/1.1 404"), "wrong token: {head}");
        let (head, _) = get(address, &format!("{base}../../etc/passwd"));
        assert!(head.starts_with("HTTP/1.1 404"), "{head}");
        let (head, page) = get(address, &base);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(String::from_utf8_lossy(&page).contains("href=\"app-debug.apk\""));
        let (head, body) = get(address, &format!("{base}app-debug.apk"));
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(
            head.contains("application/vnd.android.package-archive"),
            "{head}"
        );
        assert_eq!(body, content);
        assert_eq!(server.join().expect("server thread"), Ok(0), "--once stops");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_ready_devices_only() {
        let listing = "* daemon started successfully\nList of devices attached\nemulator-5554\tdevice\nR58M\tunauthorized\n0123ABC\tdevice product:x model:y\n\n";
        assert_eq!(parse_devices(listing), vec!["emulator-5554", "0123ABC"]);
        assert!(parse_devices("List of devices attached\n\n").is_empty());
    }

    #[test]
    fn qr_codes_render_as_text_and_names_are_escaped() {
        let qr =
            terminal_qr("http://192.168.1.20:5000/0123456789abcdef0123456789abcdef/").expect("qr");
        assert!(qr.lines().count() > 10, "{qr}");
        assert!(qr.chars().any(|c| c == '█' || c == '▀' || c == '▄'));
        assert_eq!(html_escape("<a&\"b>"), "&lt;a&amp;&quot;b&gt;");
    }
}
