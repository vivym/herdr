//! Exercised A1 Phase 3 explicit remote attach paths with a fake SSH transport.
//!
//! These tests run the real `jacquard-product` binary against a `ssh` shim that
//! executes the remote command locally. That captures the exact owner-side
//! command strings (explicit binary, allowlisted env, no install/download
//! probes) without needing a real second host. A full remote data-path
//! loopback belongs to the A1 client/bundle POC (A1-08/A1-04).

#![cfg(all(feature = "jacquard-product", unix))]

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const COMPATIBLE_SERVER_STATUS: &str = r#"{"status":"running","running":true,"version":"0.9.5","protocol":22,"capabilities":{"live_handoff":true,"detached_server_daemon":true,"endpoint_protocol_generation":1,"surface_interest":true,"health_check":true}}"#;
const NON_DETACHED_SERVER_STATUS: &str = r#"{"status":"running","running":true,"version":"0.9.5","protocol":22,"capabilities":{"live_handoff":true,"detached_server_daemon":false,"endpoint_protocol_generation":1,"surface_interest":true,"health_check":true}}"#;

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "jacquard-remote-explicit-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        for dir in ["bin", "owner", "home", "config", "state"] {
            fs::create_dir_all(root.join(dir)).expect("create sandbox dir");
        }
        let sandbox = Self { root };
        sandbox.write_fake_ssh();
        sandbox.write_fake_owner_binary();
        sandbox
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn ssh_log(&self) -> PathBuf {
        self.path("ssh.log")
    }

    fn binary_log(&self) -> PathBuf {
        self.path("binary.log")
    }

    fn owner_binary(&self) -> PathBuf {
        self.path("owner/jacquard-term")
    }

    fn config_path(&self) -> PathBuf {
        self.path("owner-config.toml")
    }

    fn write_fake_ssh(&self) {
        let script = r#"#!/bin/sh
log="${FAKE_SSH_LOG:?}"
last=""
for arg in "$@"; do
    last="$arg"
done
if [ "$last" = "/bin/sh -s" ]; then
    script=$(cat)
    printf 'STDIN:%s\n' "$script" >> "$log"
    case "$script" in
        *"uname -s"*)
            printf 'login banner\nherdr-remote-output-ready:1\nLinux\nx86_64\n'
            exit 0
            ;;
    esac
    printf '%s' "$script" | /bin/sh
    exit $?
fi
printf 'CMD:%s\n' "$last" >> "$log"
/bin/sh -c "$last"
exit $?
"#;
        let path = self.path("bin/ssh");
        fs::write(&path, script).expect("write fake ssh");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod fake ssh");
    }

    fn write_fake_owner_binary(&self) {
        let script = format!(
            r#"#!/bin/sh
log="${{FAKE_BINARY_LOG:?}}"
printf 'ENV:HERDR_SESSION=%s|HERDR_SOCKET_PATH=%s|HERDR_CLIENT_SOCKET_PATH=%s|HERDR_CONFIG_PATH=%s|XDG_CONFIG_HOME=%s\n' \
    "${{HERDR_SESSION-}}" "${{HERDR_SOCKET_PATH-}}" "${{HERDR_CLIENT_SOCKET_PATH-}}" \
    "${{HERDR_CONFIG_PATH-}}" "${{XDG_CONFIG_HOME-}}" >> "$log"
printf 'ARGS:%s\n' "$*" >> "$log"
case "$1" in
    --version)
        printf '%s\n' "${{FAKE_VERSION_OUTPUT:-jacquard-term 0.9.5}}"
        exit 0
        ;;
    status)
        default_status='{default_status}'
        printf '%s\n' "${{FAKE_SERVER_STATUS_JSON:-$default_status}}"
        exit 0
        ;;
    remote-client-bridge)
        printf 'fake bridge: cannot connect\n' >&2
        exit 1
        ;;
    *)
        exit 0
        ;;
esac
"#,
            default_status = COMPATIBLE_SERVER_STATUS
        );
        let path = self.owner_binary();
        fs::write(&path, script).expect("write fake owner binary");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .expect("chmod fake owner binary");
        let _ = fs::remove_file(self.config_path());
    }

    fn command(&self) -> Command {
        let inherited_path = std::env::var("PATH").unwrap_or_default();
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
        command
            .env(
                "PATH",
                format!("{}:{inherited_path}", self.path("bin").display()),
            )
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("FAKE_SSH_LOG", self.ssh_log())
            .env("FAKE_BINARY_LOG", self.binary_log())
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("HERDR_SESSION")
            .env_remove("HERDR_CONFIG_PATH")
            .env_remove("HERDR_REMOTE_BINARY")
            .stdin(Stdio::null());
        command
    }

    fn write_config(&self) {
        fs::write(self.config_path(), "").expect("write owner config");
    }

    fn read_log(&self, path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn run_with_timeout(mut command: Command) -> Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn herdr");
    let stdout = child.stdout.take().expect("stdout pipe");
    let stderr = child.stderr.take().expect("stderr pipe");
    let stdout_reader = thread::spawn(move || {
        let mut buffer = Vec::new();
        let mut stdout = stdout;
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let stderr_reader = thread::spawn(move || {
        let mut buffer = Vec::new();
        let mut stderr = stderr;
        let _ = stderr.read_to_end(&mut buffer);
        buffer
    });
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().expect("wait herdr") {
            let stdout = stdout_reader.join().expect("stdout reader");
            let stderr = stderr_reader.join().expect("stderr reader");
            return Output {
                status,
                stdout,
                stderr,
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let stdout = stdout_reader.join().expect("stdout reader");
            let stderr = stderr_reader.join().expect("stderr reader");
            panic!(
                "herdr remote command timed out after {COMMAND_TIMEOUT:?}; stdout={} stderr={}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn assert_no_install_or_download(log: &str) {
    for forbidden in [
        "install",
        "scp",
        "curl",
        "wget",
        "latest.json",
        "preview.json",
    ] {
        assert!(
            !log.contains(forbidden),
            "explicit remote path must not {forbidden}: {log}"
        );
    }
}

#[test]
fn product_remote_without_explicit_binary_fails_before_ssh() {
    let sandbox = Sandbox::new("no-binary");
    let output = run_with_timeout({
        let mut command = sandbox.command();
        command.args(["--remote", "fake-host"]);
        command
    });
    assert!(!output.status.success());
    let stderr = stderr(&output);
    assert!(
        stderr.contains("requires an explicit --remote-binary"),
        "unexpected error: {stderr}"
    );
    assert!(
        !sandbox.ssh_log().exists(),
        "fail-closed remote attach must not send SSH commands"
    );
}

#[test]
fn explicit_remote_binary_attaches_with_allowlisted_env_and_no_install() {
    let sandbox = Sandbox::new("explicit-ok");
    sandbox.write_config();
    let api_socket = sandbox.path("owner/api.sock");
    let client_socket = sandbox.path("owner/client.sock");
    let output = run_with_timeout({
        let mut command = sandbox.command();
        command.args([
            "--remote",
            "fake-host",
            "--remote-binary",
            sandbox.owner_binary().to_str().unwrap(),
            "--remote-socket",
            api_socket.to_str().unwrap(),
            "--remote-client-socket",
            client_socket.to_str().unwrap(),
            "--remote-config",
            sandbox.config_path().to_str().unwrap(),
            "--remote-no-install",
        ]);
        command
    });
    // The fake bridge refuses to connect, so the attach itself fails; what
    // matters is that the owner-side flow used the explicit binary/env and
    // never attempted discovery, install or download.
    assert!(!output.status.success());
    let ssh_log = sandbox.read_log(&sandbox.ssh_log());
    assert!(
        ssh_log.contains("--version"),
        "must probe the explicit owner binary version: {ssh_log}"
    );
    assert!(
        ssh_log.contains("status server --json"),
        "must probe the owner server status: {ssh_log}"
    );
    // The thin client needs a real terminal; nextest runs without one, so the
    // attach stops at client startup. Reaching that point proves the explicit
    // prepare/status path succeeded without any install/discovery step. Bridge
    // command env injection is covered by the posix command unit tests.
    assert!(
        stderr(&output).contains("remote client exited"),
        "explicit attach must reach the thin client stage: stderr={} ssh={ssh_log}",
        stderr(&output)
    );
    assert!(
        ssh_log.contains(sandbox.owner_binary().to_str().unwrap()),
        "must execute the explicit owner binary path: {ssh_log}"
    );
    let binary_log = sandbox.read_log(&sandbox.binary_log());
    assert!(
        binary_log.contains("HERDR_SESSION=jacquard"),
        "bridge/status must inject HERDR_SESSION: {binary_log}"
    );
    assert!(
        binary_log.contains(&format!("HERDR_SOCKET_PATH={}", api_socket.display())),
        "bridge/status must inject the explicit API socket: {binary_log}"
    );
    assert!(
        binary_log.contains(&format!(
            "HERDR_CLIENT_SOCKET_PATH={}",
            client_socket.display()
        )),
        "bridge/status must inject the explicit client socket: {binary_log}"
    );
    assert!(
        binary_log.contains(&format!(
            "HERDR_CONFIG_PATH={}",
            sandbox.config_path().display()
        )),
        "bridge/status must inject the explicit config: {binary_log}"
    );
    for forbidden in [
        "XDG_CONFIG_HOME=",
        " PATH=/",
        " HOME=",
        "install",
        "scp ",
        "curl ",
        "wget ",
    ] {
        assert!(
            !ssh_log.contains(forbidden),
            "explicit remote command must not carry {forbidden:?}: {ssh_log}"
        );
    }
    assert_no_install_or_download(&ssh_log);
}

#[test]
fn explicit_remote_binary_missing_fails_closed_without_install() {
    let sandbox = Sandbox::new("missing-binary");
    let missing = sandbox.path("owner/does-not-exist");
    let output = run_with_timeout({
        let mut command = sandbox.command();
        command.args([
            "--remote",
            "fake-host",
            "--remote-binary",
            missing.to_str().unwrap(),
        ]);
        command
    });
    assert!(!output.status.success());
    let stderr = stderr(&output);
    assert!(
        stderr.contains("is not executable on the owner host"),
        "unexpected error: {stderr}"
    );
    assert!(
        stderr.contains("no PATH fallback and no install was attempted"),
        "unexpected error: {stderr}"
    );
    let ssh_log = sandbox.read_log(&sandbox.ssh_log());
    assert!(ssh_log.contains("test -x"), "must only probe -x: {ssh_log}");
    assert_no_install_or_download(&ssh_log);
}

#[test]
fn explicit_remote_binary_version_mismatch_fails_closed() {
    let sandbox = Sandbox::new("version-mismatch");
    let output = run_with_timeout({
        let mut command = sandbox.command();
        command
            .env("FAKE_VERSION_OUTPUT", "jacquard-term 0.1.0")
            .args([
                "--remote",
                "fake-host",
                "--remote-binary",
                sandbox.owner_binary().to_str().unwrap(),
            ]);
        command
    });
    assert!(!output.status.success());
    let stderr = stderr(&output);
    assert!(
        stderr.contains("reports version 0.1.0, expected 0.9.5"),
        "unexpected error: {stderr}"
    );
    let ssh_log = sandbox.read_log(&sandbox.ssh_log());
    assert!(
        !ssh_log.contains("remote-client-bridge"),
        "version mismatch must fail before attaching the bridge: {ssh_log}"
    );
    assert_no_install_or_download(&ssh_log);
}

#[test]
fn remote_no_restart_refuses_incompatible_server_without_stop() {
    let sandbox = Sandbox::new("no-restart");
    let output = run_with_timeout({
        let mut command = sandbox.command();
        command
            .env("FAKE_SERVER_STATUS_JSON", NON_DETACHED_SERVER_STATUS)
            .args([
                "--remote",
                "fake-host",
                "--remote-binary",
                sandbox.owner_binary().to_str().unwrap(),
                "--remote-no-restart",
            ]);
        command
    });
    assert!(!output.status.success());
    let stderr = stderr(&output);
    assert!(
        stderr.contains("needs a restart") && stderr.contains("--remote-no-restart"),
        "unexpected error: {stderr}"
    );
    let ssh_log = sandbox.read_log(&sandbox.ssh_log());
    assert!(
        !ssh_log.contains("server stop") && !ssh_log.contains("live-handoff"),
        "--remote-no-restart must not stop/restart the server: {ssh_log}"
    );
    assert_no_install_or_download(&ssh_log);
}

#[test]
fn explicit_remote_flags_without_remote_fail_closed() {
    let sandbox = Sandbox::new("flags-without-remote");
    let output = run_with_timeout({
        let mut command = sandbox.command();
        command.args(["--remote-binary", "/opt/jacquard-term"]);
        command
    });
    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr(&output);
    assert!(
        stderr.contains("--remote-binary requires --remote"),
        "unexpected error: {stderr}"
    );
    assert!(
        !sandbox.ssh_log().exists(),
        "invalid flags must not send SSH commands"
    );
}
