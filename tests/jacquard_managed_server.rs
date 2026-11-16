//! Managed detached-server integration tests for `--features jacquard-product`
//! (A1 Phase 2).
//!
//! These tests exercise the real product binary: explicit config/session/socket
//! paths, detached daemon startup, status/probe reporting, 78 on non-detached
//! servers, and "stop only the managed server".

#![cfg(feature = "jacquard-product")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const START_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "jacquard-managed-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        for dir in ["config", "state", "home"] {
            fs::create_dir_all(root.join(dir)).expect("create sandbox dir");
        }
        let sandbox = Self { root };
        fs::write(sandbox.config_path(), "").expect("write managed config");
        sandbox
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn config_path(&self) -> PathBuf {
        self.path("managed-config.toml")
    }

    fn api_socket(&self) -> PathBuf {
        self.path("managed-api.sock")
    }

    fn client_socket(&self) -> PathBuf {
        self.path("managed-client.sock")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
        command
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env_remove("JACQUARD_TERM_MANAGED")
            .env_remove("JACQUARD_TERM_CONFIG_PATH")
            .env_remove("JACQUARD_TERM_SESSION")
            .env_remove("JACQUARD_TERM_API_SOCKET")
            .env_remove("JACQUARD_TERM_CLIENT_SOCKET")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("HERDR_SESSION")
            .env_remove("HERDR_CONFIG_PATH")
            .stdin(Stdio::null());
        command
    }

    fn start_args(&self) -> Vec<String> {
        vec![
            "server".into(),
            "--managed".into(),
            "--config".into(),
            self.config_path().display().to_string(),
            "--session".into(),
            "jacquard".into(),
            "--socket".into(),
            self.api_socket().display().to_string(),
            "--client-socket".into(),
            self.client_socket().display().to_string(),
        ]
    }

    fn target_args(&self) -> Vec<String> {
        vec![
            "--managed".into(),
            "--session".into(),
            "jacquard".into(),
            "--socket".into(),
            self.api_socket().display().to_string(),
            "--client-socket".into(),
            self.client_socket().display().to_string(),
        ]
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Best effort: never leave a detached managed server behind.
        let _ = self
            .command()
            .arg("server")
            .arg("stop")
            .args(self.target_args())
            .output();
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Kills a foreground test server if the test fails before stopping it.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run(command: &mut Command) -> Output {
    command.output().expect("command runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn start_detached(sandbox: &Sandbox) -> serde_json::Value {
    let output = run(sandbox
        .command()
        .arg("server")
        .args(sandbox.start_args().iter().skip(1))
        .arg("--detached"));
    assert!(
        output.status.success(),
        "managed detached start failed: status={:?} stdout={} stderr={}",
        output.status.code(),
        stdout(&output),
        stderr(&output)
    );
    serde_json::from_str(stdout(&output).trim()).unwrap_or_else(|err| {
        panic!(
            "managed start must print a JSON handshake ({err}): {}",
            stdout(&output)
        )
    })
}

fn managed_status(sandbox: &Sandbox, json: bool) -> Output {
    let mut command = sandbox.command();
    command
        .arg("server")
        .arg("status")
        .args(sandbox.target_args());
    if json {
        command.arg("--json");
    }
    run(&mut command)
}

fn wait_until<F>(mut condition: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    false
}

fn socket_exists(path: &Path) -> bool {
    path.exists()
}

#[test]
fn managed_detached_server_reports_status_and_stops() {
    let sandbox = Sandbox::new("lifecycle");
    let start = start_detached(&sandbox);
    assert_eq!(start["running"], true);
    assert_eq!(start["detached_server_daemon"], true);
    assert_eq!(start["session"], "jacquard");
    assert_eq!(
        start["socket"].as_str().unwrap(),
        sandbox.api_socket().display().to_string()
    );
    assert!(socket_exists(&sandbox.api_socket()), "API socket missing");

    let status = managed_status(&sandbox, true);
    assert_eq!(
        status.status.code(),
        Some(0),
        "managed status must pass: stdout={} stderr={}",
        stdout(&status),
        stderr(&status)
    );
    let status: serde_json::Value = serde_json::from_str(stdout(&status).trim()).unwrap();
    assert_eq!(status["running"], true);
    assert_eq!(status["detached_server_daemon"], true);
    assert_eq!(status["socket"], sandbox.api_socket().display().to_string());
    assert_eq!(status["session"], "jacquard");
    assert_eq!(status["version"], env!("CARGO_PKG_VERSION"));

    // Re-running a detached start adopts the existing server instead of
    // spawning a second one (idempotent ensure).
    let again = start_detached(&sandbox);
    assert_eq!(again["detached_server_daemon"], true);

    let stop = run(sandbox
        .command()
        .arg("server")
        .arg("stop")
        .args(sandbox.target_args())
        .arg("--json"));
    assert_eq!(
        stop.status.code(),
        Some(0),
        "managed stop failed: stdout={} stderr={}",
        stdout(&stop),
        stderr(&stop)
    );
    assert!(wait_until(|| !socket_exists(&sandbox.api_socket())));

    let status = managed_status(&sandbox, true);
    assert_eq!(status.status.code(), Some(1), "server should be stopped");
    let status: serde_json::Value = serde_json::from_str(stdout(&status).trim()).unwrap();
    assert_eq!(status["running"], false);
}

#[test]
fn managed_stop_only_stops_its_own_server() {
    let first = Sandbox::new("own-first");
    let second = Sandbox::new("own-second");
    let first_start = start_detached(&first);
    let second_start = start_detached(&second);
    assert_eq!(first_start["detached_server_daemon"], true);
    assert_eq!(second_start["detached_server_daemon"], true);

    let stop = run(first
        .command()
        .arg("server")
        .arg("stop")
        .args(first.target_args()));
    assert_eq!(stop.status.code(), Some(0), "stdout={}", stdout(&stop));

    let first_status = managed_status(&first, true);
    assert_eq!(first_status.status.code(), Some(1), "first must be stopped");
    let second_status = managed_status(&second, true);
    assert_eq!(
        second_status.status.code(),
        Some(0),
        "second managed server must keep running: {}",
        stdout(&second_status)
    );
}

#[test]
fn managed_detached_start_refuses_non_detached_server() {
    let sandbox = Sandbox::new("non-detached");
    let foreground = sandbox
        .command()
        .arg("server")
        .args(sandbox.start_args().iter().skip(1))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn foreground managed server");
    let mut foreground = ChildGuard(foreground);

    let ready = wait_until(|| managed_status(&sandbox, true).status.code() == Some(78));
    assert!(ready, "foreground managed server did not become ready");

    let refused = start_detached_refused(&sandbox);
    assert_eq!(
        refused.status.code(),
        Some(78),
        "detached start must refuse a non-detached server: stdout={} stderr={}",
        stdout(&refused),
        stderr(&refused)
    );
    let stderr = stderr(&refused);
    assert!(
        stderr.contains("non-detached") && stderr.contains("never silently taking over"),
        "unexpected refusal message: {stderr}"
    );

    let stop = run(sandbox
        .command()
        .arg("server")
        .arg("stop")
        .args(sandbox.target_args()));
    assert_eq!(stop.status.code(), Some(0), "stdout={}", stdout(&stop));
    let waited = wait_until(|| matches!(foreground.0.try_wait(), Ok(Some(_))));
    assert!(
        waited,
        "foreground managed server did not exit after managed stop"
    );
}

fn start_detached_refused(sandbox: &Sandbox) -> Output {
    run(sandbox
        .command()
        .arg("server")
        .args(sandbox.start_args().iter().skip(1))
        .arg("--detached"))
}

#[test]
fn managed_env_layer_can_start_and_stop() {
    let sandbox = Sandbox::new("env-layer");
    let start = run(sandbox
        .command()
        .env("JACQUARD_TERM_MANAGED", "1")
        .env("JACQUARD_TERM_CONFIG_PATH", sandbox.config_path())
        .env("JACQUARD_TERM_SESSION", "jacquard")
        .env("JACQUARD_TERM_API_SOCKET", sandbox.api_socket())
        .env("JACQUARD_TERM_CLIENT_SOCKET", sandbox.client_socket())
        .arg("server")
        .arg("--detached"));
    assert_eq!(
        start.status.code(),
        Some(0),
        "env-layer start failed: stdout={} stderr={}",
        stdout(&start),
        stderr(&start)
    );
    let start: serde_json::Value = serde_json::from_str(stdout(&start).trim()).unwrap();
    assert_eq!(start["detached_server_daemon"], true);
    assert_eq!(start["session"], "jacquard");

    let stop = run(sandbox
        .command()
        .env("JACQUARD_TERM_MANAGED", "1")
        .env("JACQUARD_TERM_API_SOCKET", sandbox.api_socket())
        .env("JACQUARD_TERM_CLIENT_SOCKET", sandbox.client_socket())
        .arg("server")
        .arg("stop")
        .arg("--json"));
    assert_eq!(
        stop.status.code(),
        Some(0),
        "env-layer stop failed: stdout={} stderr={}",
        stdout(&stop),
        stderr(&stop)
    );
}

#[test]
fn managed_never_uses_inherited_herdr_socket_or_config() {
    let sandbox = Sandbox::new("inherited");
    let inherited_socket = sandbox.path("user-herdr.sock");
    let inherited_config = sandbox.path("user-herdr-config.toml");
    let start = run(sandbox
        .command()
        .env("HERDR_SOCKET_PATH", &inherited_socket)
        .env(
            "HERDR_CLIENT_SOCKET_PATH",
            sandbox.path("user-herdr-client.sock"),
        )
        .env("HERDR_CONFIG_PATH", &inherited_config)
        .env("HERDR_ENV", "1")
        .arg("server")
        .args(sandbox.start_args().iter().skip(1))
        .arg("--detached"));
    assert_eq!(
        start.status.code(),
        Some(0),
        "managed start must ignore inherited HERDR_*: stdout={} stderr={}",
        stdout(&start),
        stderr(&start)
    );
    assert!(
        !socket_exists(&inherited_socket),
        "inherited socket was used"
    );
    assert!(!inherited_config.exists(), "inherited config was read");

    // Managed mode requires an explicit API socket; an inherited
    // HERDR_SOCKET_PATH is never a fallback.
    let missing_explicit = run(sandbox
        .command()
        .env("HERDR_SOCKET_PATH", &inherited_socket)
        .env("JACQUARD_TERM_MANAGED", "1")
        .arg("server")
        .arg("--detached"));
    assert_eq!(
        missing_explicit.status.code(),
        Some(2),
        "managed mode without --socket must fail closed: stdout={} stderr={}",
        stdout(&missing_explicit),
        stderr(&missing_explicit)
    );
    let stderr = stderr(&missing_explicit);
    assert!(
        stderr.contains("explicit --socket"),
        "unexpected error message: {stderr}"
    );
}
