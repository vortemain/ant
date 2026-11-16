//! `ant preview` (D-064) end to end: a real npm web project (its build
//! script writes `dist/`, like Vite), served live; a saved edit rebuilds and
//! the served page changes, and the version endpoint the page polls moves.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ant-preview-{name}-{}-{unique}",
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

struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(path, text).expect("write");
}

/// GET `path` from `host:port`; (status line, content type, body).
fn get(address: &str, path: &str) -> (String, String, String) {
    let mut stream = TcpStream::connect(address).expect("connect");
    write!(stream, "GET {path} HTTP/1.1\r\nHost: {address}\r\n\r\n").expect("request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("response");
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    let status = head.lines().next().unwrap_or("").to_owned();
    let kind = head
        .lines()
        .find_map(|line| line.strip_prefix("Content-Type: "))
        .unwrap_or("")
        .to_owned();
    (status, kind, body.to_owned())
}

#[test]
fn preview_serves_a_web_app_and_follows_edits() {
    let node = Command::new(if cfg!(windows) { "node.exe" } else { "node" })
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !node {
        assert!(std::env::var_os("CI").is_none(), "node is needed in CI");
        eprintln!("skipping: node not available");
        return;
    }
    let dir = TempDir::new("web");
    let project = dir.0.join("site");
    write(
        &project.join("package.json"),
        r#"{"name":"site","private":true,"scripts":{"build":"node build.js"},"devDependencies":{"vite":"*"}}"#,
    );
    write(
        &project.join("build.js"),
        "const fs = require('fs');\nconst msg = fs.readFileSync('src/msg.txt', 'utf8').trim();\nfs.mkdirSync('dist/assets', { recursive: true });\nfs.writeFileSync('dist/index.html', `<!doctype html><html><body><h1>${msg}</h1><script src=\"assets/app.js\"></script></body></html>`);\nfs.writeFileSync('dist/assets/app.js', 'console.log(1);');\n",
    );
    write(&project.join("src/msg.txt"), "Hello one\n");

    let mut child = Command::new(env!("CARGO_BIN_EXE_ant"))
        .args(["preview", "--port", "0"])
        .current_dir(&project)
        .env("ANT_SHARE_HOST", "127.0.0.1")
        .env("ANT_CACHE_HOME", dir.0.join("cache"))
        .env("ANT_DATA_HOME", dir.0.join("data"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn preview");
    let stdout = child.stdout.take().expect("stdout");
    let _guard = Guard(child);
    let mut lines = BufReader::new(stdout).lines();
    let url = loop {
        let line = lines.next().expect("preview exited early").expect("line");
        if let Some(url) = line.trim().strip_prefix("http://") {
            break url.to_owned();
        }
    };
    // Keep draining output so the child never blocks on a full pipe.
    std::thread::spawn(move || for _ in lines {});
    let (address, base) = url.split_once('/').expect("url");
    let base = format!("/{base}");

    let (status, kind, page) = get(address, &base);
    assert!(status.contains("200"), "{status}");
    assert!(kind.starts_with("text/html"), "{kind}");
    assert!(page.contains("<h1>Hello one</h1>"), "{page}");
    assert!(
        page.contains(&format!("{base}__ant/version")),
        "the live script is added: {page}"
    );
    let (_, _, version) = get(address, &format!("{base}__ant/version"));
    let version: serde_json::Value = serde_json::from_str(&version).expect("version json");
    assert_eq!(version["version"], 1);
    let (status, kind, script) = get(address, &format!("{base}assets/app.js"));
    assert!(
        status.contains("200") && kind.starts_with("text/javascript"),
        "{status} {kind}"
    );
    assert_eq!(script, "console.log(1);");
    // Client-side routes fall back to the app; nothing outside it is served.
    let (status, _, routed) = get(address, &format!("{base}settings/profile"));
    assert!(
        status.contains("200") && routed.contains("Hello one"),
        "{status}"
    );
    for bad in [
        format!("{base}../package.json"),
        format!("{base}%2e%2e/package.json"),
        "/wrong-token/".to_owned(),
    ] {
        let (status, _, _) = get(address, &bad);
        assert!(status.contains("404"), "{bad}: {status}");
    }

    // Save an edit: the preview rebuilds and every open page sees version 2.
    std::thread::sleep(Duration::from_millis(1100));
    write(&project.join("src/msg.txt"), "Hello two\n");
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let (_, _, version) = get(address, &format!("{base}__ant/version"));
        let version: serde_json::Value = serde_json::from_str(&version).expect("version json");
        if version["version"] == 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no rebuild after the edit: {version}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    let (_, _, page) = get(address, &base);
    assert!(page.contains("<h1>Hello two</h1>"), "{page}");
}
