//! Confinement integration tests for the Windows sandbox.
//!
//! These tests spawn real confined processes on Windows. They are ignored
//! on non-Windows (the sandbox API fails closed there).

#![cfg(windows)]

use ant_sandbox_win::Sandbox;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

/// A workspace directory owned for the duration of one test.
struct Workspace {
    dir: PathBuf,
}

impl Workspace {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "ant-sandbox-it-{}-{}",
            label,
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp workspace");
        Self { dir }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Poll a confined child up to a few seconds, terminating on timeout.
fn wait_for_exit(child: &mut ant_sandbox_win::Child) -> u32 {
    for _ in 0..100 {
        match child.try_wait().expect("try_wait") {
            Some(code) => return code,
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    child.terminate();
    panic!("confined child did not exit within the poll window");
}

/// Minimal environment that the confined process can use: just enough for
/// `cmd` to find itself on Windows.
fn minimal_env() -> Vec<(String, String)> {
    let mut env = Vec::new();
    for key in &["SystemRoot", "PATH", "PATHEXT", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(key) {
            env.push((
                (*key).to_owned(),
                value.to_string_lossy().into_owned(),
            ));
        }
    }
    env
}

#[test]
fn confined_write_inside_workspace_succeeds() {
    let ws = Workspace::new("inside");
    let sandbox = Sandbox::prepare(&ws.dir).expect("prepare");
    let out = ws.dir.join("out.txt");
    let argv = vec![
        "cmd".to_owned(),
        "/C".to_owned(),
        format!("echo hi > {}", out.display()),
    ];
    let mut child = sandbox
        .spawn(&argv, &ws.dir, &minimal_env())
        .expect("spawn");
    let code = wait_for_exit(&mut child);
    assert_eq!(code, 0, "confined write inside workspace should succeed");
    assert!(out.exists(), "output file should exist");
    assert_eq!(
        std::fs::read_to_string(&out).unwrap().trim(),
        "hi",
        "output file contents"
    );
}

#[test]
fn confined_write_outside_workspace_is_blocked() {
    let ws = Workspace::new("outside");
    let outside = std::env::temp_dir().join(format!(
        "ant-sandbox-escape-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    // Sanity: the escape target must NOT be inside the workspace.
    assert!(!outside.starts_with(&ws.dir), "escape target must be outside");

    let sandbox = Sandbox::prepare(&ws.dir).expect("prepare");
    let argv = vec![
        "cmd".to_owned(),
        "/C".to_owned(),
        format!("echo escaped > {}", outside.display()),
    ];
    let mut child = sandbox
        .spawn(&argv, &ws.dir, &minimal_env())
        .expect("spawn");
    let code = wait_for_exit(&mut child);
    // The confined process must fail (non-zero) because the restricted
    // token cannot write where the capability SID has no grant.
    assert_ne!(
        code, 0,
        "confined write outside workspace should fail, got exit {code}"
    );
    assert!(
        !Path::new(&outside).exists(),
        "no escape file should be created outside the workspace"
    );
    let _ = std::fs::remove_file(&outside);
}

#[test]
fn confined_exit_code_propagates() {
    let ws = Workspace::new("exitcode");
    let sandbox = Sandbox::prepare(&ws.dir).expect("prepare");
    let argv = vec!["cmd".to_owned(), "/C".to_owned(), "exit 3".to_owned()];
    let mut child = sandbox
        .spawn(&argv, &ws.dir, &minimal_env())
        .expect("spawn");
    let code = wait_for_exit(&mut child);
    assert_eq!(code, 3, "confined process exit code should propagate");
}
