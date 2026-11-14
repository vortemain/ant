//! `ant upgrade` end to end (D-043): a loopback mirror laid out like GitHub's
//! `releases/latest/download/` serves VERSION, SHA256SUMS, and this
//! platform's binary; a *copy* of the test binary upgrades itself from it,
//! and a tampered checksum leaves it untouched.

use sha2::Digest as _;
use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

fn temp_dir(name: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ant-upgrade-{name}-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        other => panic!("no release target for {other:?}"),
    }
}

type Files = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// A minimal static file server on 127.0.0.1 (one request per connection).
fn serve(files: Files) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                    break;
                }
            }
            let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
            let body = files.lock().expect("files").get(&path).cloned();
            let _ = match body {
                Some(body) => write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .and_then(|_| stream.write_all(&body)),
                None => stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                ),
            };
        }
    });
    format!("http://{address}")
}

fn sums(entries: &[(&str, &[u8])]) -> Vec<u8> {
    entries
        .iter()
        .map(|(name, bytes)| {
            let hash: String = sha2::Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            format!("{hash}  {name}\n")
        })
        .collect::<String>()
        .into_bytes()
}

fn run(exe: &Path, base: &str, args: &[&str]) -> (i32, String) {
    let output = Command::new(exe)
        .env("ANT_DOWNLOAD_BASE", base)
        .args(args)
        .output()
        .expect("run ant");
    (
        output.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

#[test]
fn upgrade_checks_downloads_verifies_and_swaps() {
    let dir = temp_dir("swap");
    let exe = dir.join(format!("ant{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(env!("CARGO_BIN_EXE_ant"), &exe).expect("copy ant");
    // The "new release" is this same binary (so it still runs after the
    // swap), marked with a trailing byte so the swap is observable.
    let mut release = std::fs::read(&exe).expect("read ant");
    release.push(0);
    let asset = format!("ant-{}{}", target(), std::env::consts::EXE_SUFFIX);
    let files: Files = Arc::new(Mutex::new(HashMap::new()));
    {
        let mut files = files.lock().expect("files");
        files.insert("/latest/download/VERSION".into(), b"99.0.0\n".to_vec());
        files.insert(
            "/latest/download/SHA256SUMS".into(),
            sums(&[(&asset, &release)]),
        );
        files.insert(format!("/latest/download/{asset}"), release.clone());
    }
    let base = serve(Arc::clone(&files));

    let (code, text) = run(&exe, &base, &["upgrade", "--check"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("99.0.0 is available"), "{text}");

    // Tampered checksum: refused, nothing changed.
    files.lock().expect("files").insert(
        "/latest/download/SHA256SUMS".into(),
        sums(&[(&asset, b"something else")]),
    );
    let before = std::fs::read(&exe).expect("read");
    let (code, text) = run(&exe, &base, &["upgrade"]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("checksum mismatch"), "{text}");
    assert_eq!(
        std::fs::read(&exe).expect("read"),
        before,
        "binary untouched"
    );

    // Correct checksum: swapped in, and the new binary runs.
    files.lock().expect("files").insert(
        "/latest/download/SHA256SUMS".into(),
        sums(&[(&asset, &release)]),
    );
    let (code, text) = run(&exe, &base, &["upgrade"]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Upgraded"), "{text}");
    assert_eq!(
        std::fs::read(&exe).expect("read"),
        release,
        "new binary in place"
    );
    let ran = Command::new(&exe)
        .arg("--version")
        .output()
        .expect("run new ant");
    assert!(ran.status.success());

    // A missing release is a clear error.
    let (code, text) = run(&exe, &base, &["upgrade", "--version", "1.2.3"]);
    assert_ne!(code, 0);
    assert!(text.contains("not found"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}
