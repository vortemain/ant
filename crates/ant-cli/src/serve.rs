//! Serve collected release APKs on the LAN for install-by-QR (P4, D-037).
//!
//! After `ant build --release` collects signed APKs into `dist/`, the front
//! door binds a short-lived HTTP server on all interfaces at an OS-assigned
//! port, prints a QR code of the download URL, and serves exactly the
//! collected APK files — nothing else — until Ctrl+C. The QR carries only
//! `http://<lan-ip>:<port>/<file>`; no token, no path traversal, no
//! directory listing.

use local_executor::CancellationToken;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::Path;
use std::time::Duration;

/// Content type for Android packages (RFC 3778 registration: `application/vnd.android.package-archive`).
const APK_CONTENT_TYPE: &str = "application/vnd.android.package-archive";

/// Serve the collected `.apk` files in `installed` until cancelled.
/// Prints the QR code and the URL before blocking. APKs only; any other
/// collected artifacts are ignored (there is nothing to install by QR yet).
pub fn serve_installed(
    root: &Path,
    installed: &[std::path::PathBuf],
    cancellation: &CancellationToken,
) -> Result<u8, String> {
    let apks: Vec<std::path::PathBuf> = installed
        .iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("apk"))
        })
        .cloned()
        .collect();
    if apks.is_empty() {
        return Ok(0);
    }
    for path in &apks {
        if !path.is_file() {
            return Err(format!(
                "serve: {} disappeared; re-run ant build",
                path.display()
            ));
        }
    }
    let ip = lan_ip().ok_or("serve: cannot determine a LAN address to advertise")?;
    let server = tiny_http::Server::http(SocketAddr::from(([0, 0, 0, 0], 0)))
        .map_err(|error| format!("serve: cannot listen: {error}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .ok_or("serve: listener has no IP address")?
        .port();
    let first_name = apks[0]
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "app.apk".to_owned());
    let url = format!("http://{ip}:{port}/{first_name}");

    println!();
    if apks.len() == 1 {
        println!("Install on a device on this network: scan, open, install.");
    } else {
        println!(
            "Install on a device on this network: scan, open, install ({} APKs).",
            apks.len()
        );
    }
    println!();
    let code = crate::qr::QrCode::encode(&url)?;
    println!("{}", code.render_terminal());
    println!("  {url}");
    println!();
    println!(
        "Serving from {} ({}). Ctrl+C stops the server and exits.",
        root.display(),
        if ip.is_loopback() {
            "this machine only"
        } else {
            "this network"
        }
    );

    // The exact file names that may be fetched, by content: a request for
    // anything else — including `../` tricks — is a 404.
    let names: Vec<String> = apks
        .iter()
        .map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect();
    let files = apks.to_vec();
    serve_loop(&server, &names, &files, cancellation)
}

/// How to answer one request, decided before any file is read.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    /// The exact file name of a collected APK, fetched with GET or HEAD.
    Serve,
    /// Any other name — including `..` tricks: never looked up on disk.
    NotFound,
    /// A collected APK requested with another method.
    MethodNotAllowed,
}

fn route(method: &str, name: &str, allowed: &[String]) -> Route {
    if !allowed.iter().any(|candidate| candidate == name) {
        return Route::NotFound;
    }
    match method {
        "GET" | "HEAD" => Route::Serve,
        _ => Route::MethodNotAllowed,
    }
}

/// Answer requests until cancelled. The exact file names that may be
/// fetched, by content: a request for anything else is a 404.
fn serve_loop(
    server: &tiny_http::Server,
    names: &[String],
    files: &[std::path::PathBuf],
    cancellation: &CancellationToken,
) -> Result<u8, String> {
    loop {
        if cancellation.is_cancelled() {
            println!("\nStopped serving.");
            return Ok(0);
        }
        let request = match server.recv_timeout(Duration::from_millis(250)) {
            Ok(Some(request)) => request,
            // No request inside the poll window: check Ctrl+C again.
            Ok(None) => continue,
            Err(error) => {
                if matches!(
                    error.kind(),
                    ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted
                ) {
                    continue;
                }
                return Err(format!("serve: {error}"));
            }
        };
        let method = request.method().as_str().to_owned();
        let url = request.url().to_owned();
        let peer = request
            .remote_addr()
            .map(|addr| addr.ip().to_string())
            .unwrap_or_else(|| "peer".to_owned());
        let name = url.trim_start_matches('/');
        match route(&method, name, names) {
            Route::Serve => {
                let path = files[names.iter().position(|c| c == name).expect("checked")].clone();
                let bytes = std::fs::read(&path)
                    .map_err(|error| format!("serve: cannot read {}: {error}", path.display()))?;
                let head = method == "HEAD";
                let mut response =
                    tiny_http::Response::from_data(if head { Vec::new() } else { bytes.clone() })
                        .with_status_code(200);
                for (header, value) in [
                    ("Content-Type", APK_CONTENT_TYPE),
                    ("Content-Length", &bytes.len().to_string()),
                    ("X-Content-Type-Options", "nosniff"),
                    (
                        "Content-Disposition",
                        &format!("attachment; filename=\"{name}\""),
                    ),
                ] {
                    if let Ok(header) =
                        tiny_http::Header::from_bytes(header.as_bytes(), value.as_bytes())
                    {
                        response = response.with_header(header);
                    }
                }
                let _ = request.respond(response);
                println!("serve: sent {name} ({}) to {peer}", bytes.len());
            }
            Route::NotFound => {
                let _ = request
                    .respond(tiny_http::Response::from_data(Vec::new()).with_status_code(404));
                println!("serve: refused {method} {url} from {peer}");
            }
            Route::MethodNotAllowed => {
                let _ = request
                    .respond(tiny_http::Response::from_data(Vec::new()).with_status_code(405));
                println!("serve: refused {method} {url} from {peer}");
            }
        }
    }
}

/// The address to advertise: the LAN-facing local address the OS would route
/// through (a UDP connect picks an interface without sending a packet), or
/// loopback when offline. Loopback keeps the feature usable on the same
/// machine; the printout says so.
fn lan_ip() -> Option<IpAddr> {
    let probe = UdpSocket::bind("0.0.0.0:0").ok()?;
    // No packet is sent for a connected UDP socket; this only selects a route.
    probe.connect("8.8.8.8:80").ok()?;
    let address = probe.local_addr().ok()?;
    let ip = address.ip();
    if ip.is_unspecified() || ip.is_multicast() {
        return Some(IpAddr::from([127, 0, 0, 1]));
    }
    Some(ip)
}

#[cfg(test)]
mod tests {
    use super::{APK_CONTENT_TYPE, Route, lan_ip, route, serve_loop};
    use local_executor::CancellationToken;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn routing_serves_only_exact_collected_names() {
        let allowed = vec!["app-release.apk".to_owned()];
        assert_eq!(route("GET", "app-release.apk", &allowed), Route::Serve);
        assert_eq!(route("HEAD", "app-release.apk", &allowed), Route::Serve);
        // A name is a name: traversal strings never match the allowlist.
        assert_eq!(route("GET", "../Cargo.toml", &allowed), Route::NotFound);
        assert_eq!(route("GET", "..%2fCargo.toml", &allowed), Route::NotFound);
        assert_eq!(route("GET", "app-debug.apk", &allowed), Route::NotFound);
        // Other methods on a collected file are refused, not served.
        assert_eq!(
            route("POST", "app-release.apk", &allowed),
            Route::MethodNotAllowed
        );
        assert_eq!(
            route("PUT", "app-release.apk", &allowed),
            Route::MethodNotAllowed
        );
    }

    #[test]
    fn lan_ip_reports_an_address_even_offline() {
        // Offline the UDP route trick fails and the fallback is loopback;
        // online it is a real LAN address. Both are valid advertisements.
        let ip = lan_ip().expect("an address to advertise");
        assert!(ip.is_loopback() || ip.is_ipv4(), "{ip:?}");
    }

    /// One raw HTTP request over loopback; returns (status, headers, body).
    fn http(addr: &std::net::SocketAddr, request: &str) -> (String, String, Vec<u8>) {
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        stream.write_all(request.as_bytes()).expect("write request");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read response");
        let text = String::from_utf8_lossy(&raw).into_owned();
        let (head, body) = text.split_once("\r\n\r").unwrap_or((text.as_str(), ""));
        let (status, headers) = head.split_once("\r\n").unwrap_or((head, ""));
        (
            status.to_owned(),
            headers.to_owned(),
            body.trim_start_matches('\n').as_bytes().to_vec(),
        )
    }

    #[test]
    fn serves_the_apk_and_refuses_everything_else() {
        let tag = std::process::id();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ant-serve-{tag}-{unique}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let apk = dir.join("app-release.apk");
        let payload = b"fake signed apk bytes".to_vec();
        std::fs::write(&apk, &payload).expect("write apk");

        let server = tiny_http::Server::http("127.0.0.1:0").expect("listen");
        let addr = server
            .server_addr()
            .to_ip()
            .expect("ip listener")
            .to_owned();
        let names = vec!["app-release.apk".to_owned()];
        let files = vec![PathBuf::from(&apk)];
        let cancellation = CancellationToken::new();
        let worker = std::thread::spawn({
            let names = names.clone();
            let files = files.clone();
            let cancellation = cancellation.clone();
            move || serve_loop(&server, &names, &files, &cancellation)
        });

        // The collected APK: 200, the Android content type, and the bytes.
        let (status, headers, body) = http(
            &addr,
            "GET /app-release.apk HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        );
        assert!(status.contains(" 200"), "{status}");
        assert!(headers.contains(APK_CONTENT_TYPE), "{headers}");
        assert_eq!(body, payload);

        // Traversal and unknown names: refused before any disk lookup.
        let (status, _, _) = http(
            &addr,
            "GET /../Cargo.toml HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        );
        assert!(status.contains(" 404"), "{status}");
        let (status, _, _) = http(
            &addr,
            "GET /other.apk HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        );
        assert!(status.contains(" 404"), "{status}");

        // HEAD is allowed (headers only, empty body).
        let (status, _, body) = http(
            &addr,
            "HEAD /app-release.apk HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        );
        assert!(status.contains(" 200"), "{status}");
        assert!(body.is_empty(), "{body:?}");

        // Other methods on the same file: refused with 405.
        let (status, _, _) = http(
            &addr,
            "POST /app-release.apk HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        assert!(status.contains(" 405"), "{status}");

        cancellation.cancel();
        let code = worker.join().expect("serve thread").expect("serve ok");
        assert_eq!(code, 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
