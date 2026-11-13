//! Shared Gradle task cache through the coordinator (D-025).
//!
//! Gradle's own build cache stores each task's outputs under a key Gradle
//! computes from the task's inputs. Serving that cache from the coordinator
//! lets a worker that misses Ant's whole-action cache reuse every unchanged
//! task (one edited file recompiles one module, not the app).
//!
//! The sandbox has no network, so the channel is narrow and token-free:
//!
//! - On the host, the worker serves HTTP on a Unix socket in a per-job
//!   directory and forwards only `GET`/`PUT /cache/{key}` to the
//!   coordinator's `/v1/task-cache/gradle/{key}` with the *worker's* token.
//!   The token never enters the sandbox.
//! - That directory is bound into the sandbox at [`BRIDGE_DIR`]. Inside,
//!   `ant __gradle-cache-bridge` listens on the sandbox's own loopback,
//!   pipes each connection to the socket, writes a Gradle init script that
//!   points the remote build cache at it (push enabled), and runs the
//!   action.
//!
//! Only sandboxed Linux workers started with `--task-cache` do this, and only
//! for actions that declare [`MARKER`]. Entries are written only by workers
//! (the coordinator refuses client writes), but any build that runs on a
//! worker can write any key: the task cache trusts every project the
//! workers build. That fits one owner's own servers, not a shared
//! multi-tenant service (documented in D-025).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Environment variable an action declares to ask for the task cache.
pub const MARKER: &str = "ANT_GRADLE_TASK_CACHE";
/// Where the bridge directory appears inside the sandbox.
pub const BRIDGE_DIR: &str = "/tmp/ant/bridge";
/// Socket file name inside the bridge directory.
pub const SOCKET_NAME: &str = "cache.sock";
/// Environment variable carrying the in-sandbox cache URL to Gradle.
pub const URL_VARIABLE: &str = "ANT_GRADLE_CACHE_URL";

/// Gradle init script: use the bridge as the remote build cache, pushing.
pub const INIT_SCRIPT: &str = r#"// Written by Ant (D-025): remote build cache through the worker bridge.
gradle.settingsEvaluated { settings ->
    def url = System.getenv('ANT_GRADLE_CACHE_URL')
    if (url) {
        settings.buildCache {
            remote(HttpBuildCache) {
                setUrl(url)
                setPush(true)
                setAllowInsecureProtocol(true)
            }
        }
    }
}
"#;

/// Whether the action asks for the Gradle task cache.
pub fn wants_task_cache(action: &build_action::Action) -> bool {
    action
        .environment
        .iter()
        .any(|(key, value)| key == MARKER && value == "1")
}

/// Wrap an action's argv so it runs under the in-sandbox bridge.
pub fn bridged_argv(ant: &Path, argv: &[String]) -> Vec<String> {
    let mut wrapped = vec![
        ant.to_string_lossy().into_owned(),
        "__gradle-cache-bridge".to_owned(),
        "--socket".to_owned(),
        format!("{BRIDGE_DIR}/{SOCKET_NAME}"),
        "--".to_owned(),
    ];
    wrapped.extend(argv.iter().cloned());
    wrapped
}

/// Map an HTTP request path from Gradle (`.../cache/{key}`) to its key.
#[cfg(any(unix, test))]
fn key_from_path(path: &str) -> Option<&str> {
    let key = path.split('?').next()?.rsplit('/').next()?;
    (!key.is_empty()).then_some(key)
}

/// Host side: serve the bridge socket, forwarding to the coordinator, for
/// as long as `body` runs. Requests are handled concurrently.
#[cfg(unix)]
pub fn serve_while<T>(
    coordinator: &crate::http::Coordinator,
    bridge_dir: &Path,
    body: impl FnOnce() -> T,
) -> Result<T, String> {
    let _ = std::fs::remove_dir_all(bridge_dir);
    std::fs::create_dir_all(bridge_dir).map_err(|error| error.to_string())?;
    let socket = bridge_dir.join(SOCKET_NAME);
    let server = tiny_http::Server::http_unix(&socket)
        .map_err(|error| format!("task cache socket: {error}"))?;
    let result = std::thread::scope(|scope| {
        let server = &server;
        scope.spawn(move || {
            for request in server.incoming_requests() {
                scope.spawn(move || handle(coordinator, request));
            }
        });
        let result = body();
        server.unblock();
        result
    });
    let _ = std::fs::remove_dir_all(bridge_dir);
    Ok(result)
}

#[cfg(unix)]
fn handle(coordinator: &crate::http::Coordinator, mut request: tiny_http::Request) {
    let reply = |request: tiny_http::Request, code: u16, body: Vec<u8>| {
        let _ = request.respond(tiny_http::Response::from_data(body).with_status_code(code));
    };
    let Some(key) = key_from_path(request.url()).map(str::to_owned) else {
        return reply(request, 404, Vec::new());
    };
    let method = request.method().clone();
    match method {
        tiny_http::Method::Get => match coordinator.task_cache_get("gradle", &key) {
            Ok(Some(bytes)) => reply(request, 200, bytes),
            Ok(None) => reply(request, 404, Vec::new()),
            Err(error) => {
                eprintln!("ant worker: task cache lookup failed: {error}");
                reply(request, 500, Vec::new());
            }
        },
        tiny_http::Method::Put => {
            let mut bytes = Vec::new();
            if std::io::Read::read_to_end(request.as_reader(), &mut bytes).is_err() {
                return reply(request, 400, Vec::new());
            }
            match coordinator.task_cache_put("gradle", &key, &bytes) {
                Ok(()) => reply(request, 201, Vec::new()),
                Err(error) => {
                    eprintln!("ant worker: task cache store failed: {error}");
                    reply(request, 500, Vec::new());
                }
            }
        }
        _ => reply(request, 405, Vec::new()),
    }
}

/// `ant __gradle-cache-bridge --socket PATH -- PROGRAM [ARGS...]`, run
/// inside the sandbox: listen on loopback, pipe connections to the socket,
/// install the init script, run the program, exit with its status.
pub fn bridge_main(args: Vec<OsString>) -> Result<i32, String> {
    let mut args = args.into_iter();
    let (Some(flag), Some(socket), Some(separator)) = (args.next(), args.next(), args.next())
    else {
        return Err(BRIDGE_USAGE.to_owned());
    };
    if flag != "--socket" || separator != "--" {
        return Err(BRIDGE_USAGE.to_owned());
    }
    let program: Vec<OsString> = args.collect();
    let Some((first, rest)) = program.split_first() else {
        return Err(BRIDGE_USAGE.to_owned());
    };
    bridge(PathBuf::from(socket), first, rest)
}

const BRIDGE_USAGE: &str = "usage: ant __gradle-cache-bridge --socket PATH -- PROGRAM [ARGS...]";

#[cfg(unix)]
fn bridge(socket: PathBuf, program: &OsString, args: &[OsString]) -> Result<i32, String> {
    use std::net::{Shutdown, TcpListener};
    use std::os::unix::net::UnixStream;

    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|error| format!("bridge listen: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    std::thread::spawn(move || {
        for tcp in listener.incoming().flatten() {
            let socket = socket.clone();
            std::thread::spawn(move || {
                let Ok(unix) = UnixStream::connect(&socket) else {
                    return;
                };
                let (Ok(mut tcp_read), Ok(mut unix_write)) = (tcp.try_clone(), unix.try_clone())
                else {
                    return;
                };
                let upstream = std::thread::spawn(move || {
                    let _ = std::io::copy(&mut tcp_read, &mut unix_write);
                    let _ = unix_write.shutdown(Shutdown::Write);
                });
                let (mut unix_read, mut tcp_write) = (unix, tcp);
                let _ = std::io::copy(&mut unix_read, &mut tcp_write);
                let _ = tcp_write.shutdown(Shutdown::Write);
                let _ = upstream.join();
            });
        }
    });

    // Gradle applies every script in `$GRADLE_USER_HOME/init.d`.
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let home = std::env::var_os("GRADLE_USER_HOME")
        .map(PathBuf::from)
        .map(|home| {
            if home.is_absolute() {
                home
            } else {
                cwd.join(home)
            }
        })
        .ok_or("GRADLE_USER_HOME must be set for the Gradle task cache")?;
    let init = home.join("init.d");
    std::fs::create_dir_all(&init).map_err(|error| error.to_string())?;
    std::fs::write(init.join("ant-task-cache.gradle"), INIT_SCRIPT)
        .map_err(|error| error.to_string())?;

    let status = std::process::Command::new(program)
        .args(args)
        .env(URL_VARIABLE, format!("http://127.0.0.1:{port}/cache/"))
        .status()
        .map_err(|error| format!("cannot start {}: {error}", program.to_string_lossy()))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(not(unix))]
fn bridge(_socket: PathBuf, _program: &OsString, _args: &[OsString]) -> Result<i32, String> {
    Err("the Gradle task-cache bridge runs only inside the Linux sandbox".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_come_from_the_last_path_segment() {
        assert_eq!(key_from_path("/cache/abc123"), Some("abc123"));
        assert_eq!(key_from_path("/cache/abc123?x=1"), Some("abc123"));
        assert_eq!(key_from_path("/cache/"), None);
    }

    #[test]
    fn bridged_argv_runs_the_action_after_the_separator() {
        let argv = bridged_argv(Path::new("/opt/ant"), &["sh".into(), "-c".into()]);
        assert_eq!(
            argv,
            vec![
                "/opt/ant",
                "__gradle-cache-bridge",
                "--socket",
                "/tmp/ant/bridge/cache.sock",
                "--",
                "sh",
                "-c"
            ]
        );
    }
}
