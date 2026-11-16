//! `ant publish android` (D-062) against a local fake of Google's token
//! endpoint and the Android Publisher API. The fake verifies the RS256
//! signature of the service account's JWT with the matching public key, so
//! the real signing path is exercised. A throwaway RSA key is generated per
//! run with `openssl` (no key is stored in the repository).

use base64::Engine as _;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ant-publish-{name}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Seen {
    requests: Vec<(String, String)>,
    uploaded: Vec<u8>,
    track_body: String,
    jwt_verified: bool,
}

/// Serve `count` requests like Google would, recording what arrived.
fn fake_google(public_key_der: Vec<u8>, seen: Arc<Mutex<Seen>>, fail_commit: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line.is_empty() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or("").to_owned();
            let path = parts.next().unwrap_or("").to_owned();
            let mut length = 0usize;
            let mut authorized = false;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).expect("header");
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                let lower = header.to_ascii_lowercase();
                if let Some(value) = lower.strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap_or(0);
                }
                if lower == "authorization: bearer fake-access-token" {
                    authorized = true;
                }
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).expect("body");
            let mut seen_now = seen.lock().expect("lock");
            seen_now.requests.push((method.clone(), path.clone()));
            let (status, answer) = if path == "/token" {
                let form = String::from_utf8_lossy(&body).into_owned();
                let assertion = form
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("assertion="))
                    .unwrap_or("")
                    .to_owned();
                let pieces: Vec<&str> = assertion.split('.').collect();
                let decode = |text: &str| {
                    base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(text)
                        .unwrap_or_default()
                };
                let verified = pieces.len() == 3
                    && ring::signature::UnparsedPublicKey::new(
                        &ring::signature::RSA_PKCS1_2048_8192_SHA256,
                        &public_key_der,
                    )
                    .verify(
                        format!("{}.{}", pieces[0], pieces[1]).as_bytes(),
                        &decode(pieces[2]),
                    )
                    .is_ok();
                let claims: serde_json::Value =
                    serde_json::from_slice(&decode(pieces.get(1).copied().unwrap_or("")))
                        .unwrap_or_default();
                let good = verified
                    && claims["iss"] == "ci-publisher@example.iam.gserviceaccount.com"
                    && claims["scope"] == "https://www.googleapis.com/auth/androidpublisher"
                    && form.contains(
                        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer",
                    );
                seen_now.jwt_verified |= good;
                if good {
                    (
                        200,
                        r#"{"access_token":"fake-access-token","expires_in":3599}"#.to_owned(),
                    )
                } else {
                    (
                        400,
                        r#"{"error":"invalid_grant","error_description":"bad signature"}"#
                            .to_owned(),
                    )
                }
            } else if !authorized {
                (401, r#"{"error":{"message":"no token"}}"#.to_owned())
            } else if method == "POST" && path.ends_with("/edits") {
                (200, r#"{"id":"edit-42"}"#.to_owned())
            } else if method == "POST" && path.contains("/edits/edit-42/bundles?uploadType=media") {
                seen_now.uploaded = body.clone();
                (200, r#"{"versionCode":7,"sha256":"x"}"#.to_owned())
            } else if method == "PUT" && path.ends_with("/edits/edit-42/tracks/internal") {
                seen_now.track_body = String::from_utf8_lossy(&body).into_owned();
                (200, "{}".to_owned())
            } else if method == "POST" && path.ends_with("/edits/edit-42:commit") {
                if fail_commit {
                    (
                        403,
                        r#"{"error":{"message":"The caller does not have permission"}}"#.to_owned(),
                    )
                } else {
                    (200, r#"{"id":"edit-42"}"#.to_owned())
                }
            } else {
                (
                    404,
                    r#"{"error":{"message":"unexpected request"}}"#.to_owned(),
                )
            };
            drop(seen_now);
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                answer.len()
            );
        }
    });
    format!("http://{address}")
}

fn openssl(args: &[&str]) -> bool {
    Command::new("openssl")
        .args(args)
        .output()
        .is_ok_and(|output| output.status.success())
}

#[test]
fn publish_android_uploads_the_bundle_to_a_play_track() {
    let dir = TempDir::new("play");
    let key = dir.0.join("key.pem");
    let public = dir.0.join("key.der");
    if !openssl(&[
        "genpkey",
        "-algorithm",
        "RSA",
        "-pkeyopt",
        "rsa_keygen_bits:2048",
        "-out",
        &key.to_string_lossy(),
    ]) || !openssl(&[
        "rsa",
        "-in",
        &key.to_string_lossy(),
        "-RSAPublicKey_out",
        "-outform",
        "DER",
        "-out",
        &public.to_string_lossy(),
    ]) {
        assert!(std::env::var_os("CI").is_none(), "openssl is needed in CI");
        eprintln!("skipping: openssl not available");
        return;
    }
    let seen = Arc::new(Mutex::new(Seen::default()));
    let base = fake_google(
        std::fs::read(&public).expect("public key"),
        seen.clone(),
        false,
    );
    let account = serde_json::json!({
        "type": "service_account",
        "client_email": "ci-publisher@example.iam.gserviceaccount.com",
        "private_key": std::fs::read_to_string(&key).expect("key"),
        "token_uri": format!("{base}/token"),
    });
    let account_file = dir.0.join("service-account.json");
    std::fs::write(&account_file, account.to_string()).expect("account");
    let bundle = dir.0.join("app-release.aab");
    std::fs::write(
        &bundle,
        b"PK\x03\x04 not really a bundle, but bytes to upload",
    )
    .expect("aab");
    let data = dir.0.join("data");
    let ant = |args: &[&str], base: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_ant"))
            .env("ANT_DATA_HOME", &data)
            .env("ANT_PLAY_API", base)
            .args(args)
            .output()
            .expect("spawn ant");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let bundle_arg = bundle.to_string_lossy().into_owned();
    let (code, stdout, stderr) = ant(
        &[
            "publish",
            "android",
            "--aab",
            &bundle_arg,
            "--package",
            "com.example.app",
            "--key",
            &account_file.to_string_lossy(),
        ],
        &base,
    );
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(
        stdout.contains("Published versionCode 7 of com.example.app to internal"),
        "{stdout}"
    );
    {
        let seen = seen.lock().expect("lock");
        assert!(
            seen.jwt_verified,
            "the JWT was signed with the service account's key"
        );
        assert_eq!(seen.uploaded, std::fs::read(&bundle).expect("aab"));
        let track: serde_json::Value = serde_json::from_str(&seen.track_body).expect("track body");
        assert_eq!(track["releases"][0]["versionCodes"][0], "7");
        assert_eq!(track["releases"][0]["status"], "completed");
        let paths: Vec<&str> = seen
            .requests
            .iter()
            .map(|(_, path)| path.as_str())
            .collect();
        assert_eq!(paths.len(), 5, "{paths:?}");
        assert!(paths[4].ends_with(":commit"));
    }
    // The key is remembered sealed (DPAPI on Windows, the keyring or a
    // 0600 file elsewhere, D-052) and the copy passed with --key is not needed.
    #[cfg(windows)]
    for entry in std::fs::read_dir(data.join("play"))
        .expect("play dir")
        .flatten()
    {
        let bytes = std::fs::read(entry.path()).expect("stored");
        assert!(!String::from_utf8_lossy(&bytes).contains("BEGIN PRIVATE KEY"));
    }
    // A second run needs no --key; a refused commit is reported, not hidden.
    let failing = Arc::new(Mutex::new(Seen::default()));
    let base = fake_google(std::fs::read(&public).expect("public key"), failing, true);
    let rewritten = serde_json::json!({
        "type": "service_account",
        "client_email": "ci-publisher@example.iam.gserviceaccount.com",
        "private_key": std::fs::read_to_string(&key).expect("key"),
        "token_uri": format!("{base}/token"),
    });
    std::fs::write(&account_file, rewritten.to_string()).expect("account");
    let (code, _, _) = ant(
        &[
            "publish",
            "android",
            "--aab",
            &bundle_arg,
            "--package",
            "com.example.app",
            "--key",
            &account_file.to_string_lossy(),
        ],
        &base,
    );
    assert_ne!(code, 0);
    let (code, stdout, stderr) = ant(
        &[
            "publish",
            "android",
            "--aab",
            &bundle_arg,
            "--package",
            "com.example.app",
        ],
        &base,
    );
    assert_ne!(code, 0, "{stdout}");
    assert!(
        stderr.contains("committing the Play edit failed (HTTP 403)")
            && stderr.contains("does not have permission"),
        "{stderr}"
    );
}
