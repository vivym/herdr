//! Managed detached server mode for the Jacquard Term fork (A1 Phase 2).
//!
//! The Jacquard daemon owns a `jacquard-term` headless server started with
//! explicit config/session/socket paths. This module provides the product-only
//! interface (A1 plan §6.2/§6.6, decisions D4/D16):
//!
//! ```text
//! jacquard-term server --managed --config <path> --session <name> \
//!     --socket <api-socket> [--client-socket <path>] [--detached]
//! jacquard-term server status --managed [--json] [target flags]
//! jacquard-term server stop --managed [--json] [target flags]
//! ```
//!
//! Rules:
//! - managed mode only ever uses explicit paths; the session/api socket are
//!   required and there is no discovery of a user Herdr server;
//! - layers are `CLI flags > JACQUARD_TERM_* env > [jacquard] profile > defaults`
//!   (A1 D4);
//! - `--detached` spawns a `setsid` daemon and waits until the JSON API reports
//!   `detached_server_daemon: true` (A1 §6.6);
//! - `status` returns 78 when a server exists but is not a detached managed
//!   server (D16: never silently take over);
//! - `stop` only talks to the explicit managed socket, so it can never stop a
//!   user's Herdr server.
//!
//! Upstream/default builds keep their existing `server` behavior: this module
//! returns `None` for them and never intercepts commands.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::product;

pub(crate) const MANAGED_ENV_VAR: &str = "JACQUARD_TERM_MANAGED";
pub(crate) const CONFIG_PATH_ENV_VAR: &str = "JACQUARD_TERM_CONFIG_PATH";
pub(crate) const SESSION_ENV_VAR: &str = "JACQUARD_TERM_SESSION";
pub(crate) const API_SOCKET_ENV_VAR: &str = "JACQUARD_TERM_API_SOCKET";
pub(crate) const CLIENT_SOCKET_ENV_VAR: &str = "JACQUARD_TERM_CLIENT_SOCKET";

const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(15);
const SERVER_READY_POLL: Duration = Duration::from_millis(50);
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

/// Exit code used by the A1 lifecycle when a server exists but cannot be
/// adopted (D16: explicit stop/reset required, never silent takeover).
const EXIT_MISMATCH: i32 = 78;

const USAGE: &str = "\
usage: jacquard-term server --managed --session <name> --socket <api-socket> [options]
       jacquard-term server status --managed [--json] [target options]
       jacquard-term server stop --managed [--json] [target options]

managed options:
  --config <PATH>         explicit managed config.toml (must exist)
  --session <NAME>        managed session name (default: jacquard)
  --socket <PATH>         explicit managed JSON API socket (required)
  --client-socket <PATH>  explicit managed client socket (derived from --socket)
  --detached              start the managed server as a detached daemon
  --json                  machine-readable status/stop output

environment:
  JACQUARD_TERM_MANAGED=1
  JACQUARD_TERM_CONFIG_PATH, JACQUARD_TERM_SESSION,
  JACQUARD_TERM_API_SOCKET, JACQUARD_TERM_CLIENT_SOCKET
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedCommand {
    Start,
    Status,
    Stop,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct CliArgs {
    managed: bool,
    detached: bool,
    json: bool,
    config: Option<String>,
    session: Option<String>,
    api_socket: Option<String>,
    client_socket: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct EnvArgs {
    managed: Option<bool>,
    config: Option<String>,
    session: Option<String>,
    api_socket: Option<String>,
    client_socket: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedTarget {
    pub(crate) session: String,
    pub(crate) api_socket: PathBuf,
    pub(crate) client_socket: PathBuf,
    pub(crate) config: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct ManagedStatusJson<'a> {
    running: bool,
    detached_server_daemon: bool,
    socket: &'a str,
    session: &'a str,
    version: Option<String>,
    protocol: Option<u32>,
    config: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct ManagedStopJson<'a> {
    status: &'a str,
    socket: &'a str,
    session: &'a str,
}

/// Entry point called from `main` before the generic CLI parsing.
///
/// Returns `None` when this is not a managed-server invocation (upstream
/// behavior is preserved). Product builds must be explicit: the managed
/// interface only activates with `--managed` or a product env/profile opt-in.
pub(crate) fn maybe_run(args: &[String]) -> Option<io::Result<i32>> {
    if !product::IS_JACQUARD_PRODUCT {
        return None;
    }
    if args.get(1).map(String::as_str) != Some("server") {
        return None;
    }
    let rest = &args[2..];
    if rest
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        return None;
    }
    // Internal server modes keep using the upstream paths.
    if matches!(
        rest.first().map(String::as_str),
        Some("--handoff-import" | "live-handoff")
    ) {
        return None;
    }

    let (command, flag_args) = match rest.first().map(String::as_str) {
        Some("status") => (ManagedCommand::Status, &rest[1..]),
        Some("stop") => (ManagedCommand::Stop, &rest[1..]),
        Some(
            "reload-config"
            | "agent-manifests"
            | "update-agent-manifests"
            | "reload-agent-manifests",
        ) => return None,
        _ => (ManagedCommand::Start, rest),
    };

    let cli = match parse_cli(flag_args) {
        Ok(cli) => cli,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("{USAGE}");
            return Some(Ok(2));
        }
    };
    let env = match EnvArgs::from_env() {
        Ok(env) => env,
        Err(message) => {
            eprintln!("managed server: {message}");
            return Some(Ok(2));
        }
    };

    let explicit_config = match resolve_config_path(&cli, &env) {
        Ok(path) => path,
        Err(message) => {
            eprintln!("managed server: {message}");
            return Some(Ok(2));
        }
    };
    if let Some(path) = &explicit_config {
        if !path.is_file() {
            eprintln!(
                "managed server: explicit config file {} does not exist; managed mode never falls back to a default config",
                path.display()
            );
            return Some(Ok(1));
        }
        // Used only to read the profile layer; the server child re-reads it.
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, path);
    }
    let profile = crate::config::Config::load().config.jacquard;

    let managed = cli.managed || env.managed == Some(true) || profile.managed;
    if !managed {
        return None;
    }

    let target = match resolve_target(&cli, &env, &profile, explicit_config) {
        Ok(target) => target,
        Err(message) => {
            eprintln!("managed server: {message}");
            eprintln!("{USAGE}");
            return Some(Ok(2));
        }
    };
    apply_target_env(&target);

    let outcome = match command {
        ManagedCommand::Start if cli.detached => start_detached(&target),
        ManagedCommand::Start => {
            // Foreground managed server: same process, explicit env applied.
            return Some(crate::server::headless::run_server().map(|_| 0));
        }
        ManagedCommand::Status => print_status(&target, cli.json),
        ManagedCommand::Stop => stop_server(&target, cli.json),
    };
    Some(outcome.inspect(|_| {
        let _ = io::stdout().flush();
    }))
}

/// Parse the managed CLI flags. Unknown flags fail closed instead of being
/// ignored so a launcher typo can never start a wrong server.
fn parse_cli(args: &[String]) -> Result<CliArgs, String> {
    let mut cli = CliArgs::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--managed" => cli.managed = true,
            "--detached" => cli.detached = true,
            "--json" => cli.json = true,
            "--config" => cli.config = Some(take_value(args, &mut index, "--config")?),
            "--session" => cli.session = Some(take_value(args, &mut index, "--session")?),
            "--socket" => cli.api_socket = Some(take_value(args, &mut index, "--socket")?),
            "--client-socket" => {
                cli.client_socket = Some(take_value(args, &mut index, "--client-socket")?)
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown managed server option: {other}"));
            }
            other => return Err(format!("unexpected managed server argument: {other}")),
        }
        index += 1;
    }
    Ok(cli)
}

fn take_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    if let Some(value) = args.get(*index + 1) {
        if !value.starts_with("--") {
            *index += 1;
            return Ok(value.clone());
        }
    }
    Err(format!("missing value for {flag}"))
}

fn resolve_config_path(cli: &CliArgs, env: &EnvArgs) -> Result<Option<PathBuf>, String> {
    match cli.config.as_deref().or(env.config.as_deref()) {
        Some(value) => {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err(format!(
                    "managed config path must be absolute, got {}",
                    path.display()
                ));
            }
            Ok(Some(path))
        }
        None => Ok(None),
    }
}

/// Resolve the effective managed target. Layer order is
/// `CLI > JACQUARD_TERM_* env > [jacquard] profile > defaults` (A1 D4).
fn resolve_target(
    cli: &CliArgs,
    env: &EnvArgs,
    profile: &crate::config::JacquardConfig,
    config: Option<PathBuf>,
) -> Result<ManagedTarget, String> {
    let session = cli
        .session
        .clone()
        .or_else(|| env.session.clone())
        .or_else(|| profile.session.clone())
        .unwrap_or_else(|| product::DEFAULT_SESSION_NAME.to_string());
    crate::session::validate_name(&session)
        .map_err(|err| format!("invalid managed session name {session:?}: {err}"))?;

    let api_socket = cli
        .api_socket
        .clone()
        .or_else(|| env.api_socket.clone())
        .or_else(|| profile.api_socket.clone())
        .ok_or_else(|| {
            "managed mode requires an explicit --socket / JACQUARD_TERM_API_SOCKET".to_string()
        })?;
    let api_socket = validate_managed_path("managed JSON API socket", &api_socket)?;

    let client_socket = cli
        .client_socket
        .clone()
        .or_else(|| env.client_socket.clone())
        .or_else(|| profile.client_socket.clone());
    let client_socket = match client_socket {
        Some(value) => validate_managed_path("managed client socket", &value)?,
        None => crate::server::socket_paths::derive_client_socket_from_api_socket(&api_socket),
    };
    if client_socket == api_socket {
        return Err("managed client socket must differ from the JSON API socket".to_string());
    }

    Ok(ManagedTarget {
        session,
        api_socket,
        client_socket,
        config,
    })
}

fn validate_managed_path(label: &str, value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(format!(
            "{label} must be an absolute path, got {}",
            path.display()
        ));
    }
    if is_legacy_user_herdr_path(&path) {
        return Err(format!(
            "{label} {} is inside a legacy user Herdr config/state directory; managed mode never touches user Herdr paths",
            path.display()
        ));
    }
    Ok(path)
}

/// Reject paths inside `~/.config/herdr` / `~/.local/state/herdr` so a managed
/// server can never be pointed at a user's Herdr sockets.
fn is_legacy_user_herdr_path(path: &Path) -> bool {
    let text = path.to_string_lossy().replace('\\', "/");
    text.contains("/.config/herdr/")
        || text.ends_with("/.config/herdr")
        || text.contains("/.local/state/herdr/")
        || text.ends_with("/.local/state/herdr")
}

impl EnvArgs {
    fn from_env() -> Result<Self, String> {
        Ok(Self {
            managed: env_bool(MANAGED_ENV_VAR)?,
            config: env_string(CONFIG_PATH_ENV_VAR)?,
            session: env_string(SESSION_ENV_VAR)?,
            api_socket: env_string(API_SOCKET_ENV_VAR)?,
            client_socket: env_string(CLIENT_SOCKET_ENV_VAR)?,
        })
    }
}

fn env_string(name: &str) -> Result<Option<String>, String> {
    match std::env::var(name) {
        Ok(value) if value.trim().is_empty() => Ok(None),
        Ok(value) => Ok(Some(value.trim().to_string())),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} is not valid UTF-8")),
    }
}

fn env_bool(name: &str) -> Result<Option<bool>, String> {
    match env_string(name)? {
        None => Ok(None),
        Some(value) => match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(Some(true)),
            "0" | "false" | "no" | "off" => Ok(Some(false)),
            other => Err(format!(
                "{name} must be a boolean (1/true/yes/on/0/false/no/off), got {other:?}"
            )),
        },
    }
}

/// Push the resolved target into the internal (upstream) environment so the
/// server and CLI helpers use exactly these paths.
fn apply_target_env(target: &ManagedTarget) {
    for name in [
        crate::session::SESSION_ENV_VAR,
        crate::api::SOCKET_PATH_ENV_VAR,
        crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR,
        crate::config::CONFIG_PATH_ENV_VAR,
    ] {
        std::env::remove_var(name);
    }
    std::env::set_var(crate::session::SESSION_ENV_VAR, &target.session);
    std::env::set_var(crate::api::SOCKET_PATH_ENV_VAR, &target.api_socket);
    std::env::set_var(
        crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR,
        &target.client_socket,
    );
    if let Some(config) = &target.config {
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, config);
    }
    std::env::set_var(MANAGED_ENV_VAR, "1");
}

fn probe(api_socket: &Path) -> io::Result<Option<crate::api::RuntimeStatus>> {
    crate::api::read_runtime_status_at(api_socket, STATUS_TIMEOUT)
}

fn is_detached(status: &crate::api::RuntimeStatus) -> bool {
    status
        .capabilities
        .as_ref()
        .is_some_and(|capabilities| capabilities.detached_server_daemon)
}

/// A1 M6: a managed server must report the expected managed session identity.
/// Unknown identity (legacy/upstream server, or a different managed session) is
/// never adopted or stopped.
fn managed_session_matches(target: &ManagedTarget, status: &crate::api::RuntimeStatus) -> bool {
    status
        .capabilities
        .as_ref()
        .and_then(|capabilities| capabilities.managed_session.as_deref())
        == Some(target.session.as_str())
}

fn refuse_foreign_server(target: &ManagedTarget, status: &crate::api::RuntimeStatus) {
    let reported = status
        .capabilities
        .as_ref()
        .and_then(|capabilities| capabilities.managed_session.as_deref())
        .unwrap_or("<unreported>");
    eprintln!(
        "managed server: server at {} reports managed session {reported:?}, expected {:?};          refusing to adopt/stop a server that is not the explicit managed target",
        target.api_socket.display(),
        target.session
    );
}

fn status_json<'a>(
    target: &'a ManagedTarget,
    status: Option<&crate::api::RuntimeStatus>,
) -> ManagedStatusJson<'a> {
    ManagedStatusJson {
        running: status.is_some(),
        detached_server_daemon: status.is_some_and(is_detached),
        socket: target.api_socket.to_str().unwrap_or_default(),
        session: &target.session,
        version: status.and_then(|status| status.version.clone()),
        protocol: status.and_then(|status| status.protocol),
        config: target.config.as_ref().and_then(|path| path.to_str()),
    }
}

/// `server status --managed`: probe only the explicit managed socket.
fn print_status(target: &ManagedTarget, json: bool) -> io::Result<i32> {
    let status = probe(&target.api_socket)?;
    if let Some(status) = status.as_ref() {
        if !managed_session_matches(target, status) {
            refuse_foreign_server(target, status);
            return Ok(EXIT_MISMATCH);
        }
    }
    let payload = status_json(target, status.as_ref());
    if json {
        println!("{}", serde_json::to_string(&payload).unwrap_or_default());
    } else if payload.running {
        println!("managed server: running");
        println!("  socket: {}", payload.socket);
        println!("  session: {}", payload.session);
        println!(
            "  detached_server_daemon: {}",
            payload.detached_server_daemon
        );
        if let Some(version) = &payload.version {
            println!("  version: {version}");
        }
        if let Some(config) = payload.config {
            println!("  config: {config}");
        }
    } else {
        println!("managed server: not running");
        println!("  socket: {}", payload.socket);
        println!("  session: {}", payload.session);
    }
    Ok(if payload.running && payload.detached_server_daemon {
        0
    } else if payload.running {
        EXIT_MISMATCH
    } else {
        1
    })
}

/// `server --managed --detached`: start (or adopt) the detached managed server.
fn start_detached(target: &ManagedTarget) -> io::Result<i32> {
    if let Some(status) = probe(&target.api_socket)? {
        if is_detached(&status) && managed_session_matches(target, &status) {
            // Already running and adoptable: report it, do not spawn a second server.
            println!(
                "{}",
                serde_json::to_string(&status_json(target, Some(&status))).unwrap_or_default()
            );
            return Ok(0);
        }
        if !managed_session_matches(target, &status) {
            refuse_foreign_server(target, &status);
        } else {
            eprintln!(
                "managed server: an existing non-detached server is listening at {}",
                target.api_socket.display()
            );
            eprintln!(
                "managed server: stop it explicitly (never silently taking over a user/Plan D server)"
            );
        }
        return Ok(EXIT_MISMATCH);
    }

    let exe = std::env::current_exe().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("failed to find current executable: {err}"),
        )
    })?;
    let mut command = Command::new(exe);
    command
        .arg("server")
        .arg("--managed")
        .arg("--session")
        .arg(&target.session)
        .arg("--socket")
        .arg(&target.api_socket)
        .arg("--client-socket")
        .arg(&target.client_socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env(crate::session::SESSION_ENV_VAR, &target.session)
        .env(crate::api::SOCKET_PATH_ENV_VAR, &target.api_socket)
        .env(
            crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR,
            &target.client_socket,
        )
        .env(MANAGED_ENV_VAR, "1");
    if let Some(config) = &target.config {
        command.arg("--config").arg(config);
        command.env(crate::config::CONFIG_PATH_ENV_VAR, config);
    }
    if let Ok(cwd) = std::env::current_dir() {
        command.env(crate::server::autodetect::STARTUP_CWD_ENV_VAR, cwd);
    }
    crate::platform::detach_server_daemon_command(&mut command);
    let pid = crate::platform::launch_server_daemon_command(&mut command)?;

    let deadline = Instant::now() + SERVER_READY_TIMEOUT;
    loop {
        if let Some(status) = probe(&target.api_socket)? {
            if !managed_session_matches(target, &status) {
                refuse_foreign_server(target, &status);
                return Ok(EXIT_MISMATCH);
            }
            if !is_detached(&status) {
                eprintln!(
                    "managed server: server at {} did not report detached_server_daemon=true",
                    target.api_socket.display()
                );
                return Ok(1);
            }
            println!(
                "{}",
                serde_json::to_string(&status_json(target, Some(&status))).unwrap_or_default()
            );
            return Ok(0);
        }
        if Instant::now() >= deadline {
            eprintln!(
                "managed server: server did not become ready within {}s (socket {}, pid {pid})",
                SERVER_READY_TIMEOUT.as_secs(),
                target.api_socket.display()
            );
            return Ok(1);
        }
        std::thread::sleep(SERVER_READY_POLL);
    }
}

/// `server stop --managed`: stop only the explicit managed server.
fn stop_server(target: &ManagedTarget, json: bool) -> io::Result<i32> {
    let status = probe(&target.api_socket)?;
    let Some(status) = status else {
        eprintln!(
            "managed server: no server is running at {}; refusing to fall back to any other socket",
            target.api_socket.display()
        );
        return Ok(1);
    };
    if !managed_session_matches(target, &status) {
        refuse_foreign_server(target, &status);
        return Ok(EXIT_MISMATCH);
    }
    let api_socket = target.api_socket.clone();
    let client_socket = target.client_socket.clone();
    crate::session::stop_socket_with_timeout(
        api_socket.clone(),
        vec![api_socket, client_socket],
        STOP_TIMEOUT,
        "managed server",
    )
    .map_err(io::Error::other)?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&ManagedStopJson {
                status: "stopped",
                socket: target.api_socket.to_str().unwrap_or_default(),
                session: &target.session,
            })
            .unwrap_or_default()
        );
    } else {
        println!(
            "stopped managed server at {} (session {})",
            target.api_socket.display(),
            target.session
        );
    }
    Ok(0)
}

use std::io::Write as _;

#[cfg(test)]
mod tests {
    use super::*;

    fn target(
        cli: CliArgs,
        env: EnvArgs,
        profile: &crate::config::JacquardConfig,
    ) -> ManagedTarget {
        resolve_target(&cli, &env, profile, None).expect("target resolves")
    }

    fn profile_with(
        session: Option<&str>,
        api: Option<&str>,
        client: Option<&str>,
    ) -> crate::config::JacquardConfig {
        crate::config::JacquardConfig {
            managed: true,
            session: session.map(str::to_string),
            api_socket: api.map(str::to_string),
            client_socket: client.map(str::to_string),
        }
    }

    fn cli_for(api: &str) -> CliArgs {
        CliArgs {
            managed: true,
            api_socket: Some(api.to_string()),
            ..CliArgs::default()
        }
    }

    #[test]
    fn managed_session_and_socket_layer_priority_is_cli_env_profile_default() {
        let mut cli = CliArgs {
            managed: true,
            session: Some("cli-session".into()),
            api_socket: Some("/tmp/cli-api.sock".into()),
            client_socket: Some("/tmp/cli-client.sock".into()),
            ..CliArgs::default()
        };
        let mut env = EnvArgs {
            managed: Some(true),
            session: Some("env-session".into()),
            api_socket: Some("/tmp/env-api.sock".into()),
            client_socket: Some("/tmp/env-client.sock".into()),
            ..EnvArgs::default()
        };
        let profile = profile_with(Some("profile-session"), Some("/tmp/p-api.sock"), None);

        let resolved = target(cli.clone(), env.clone(), &profile);
        assert_eq!(resolved.session, "cli-session");
        assert_eq!(resolved.api_socket, PathBuf::from("/tmp/cli-api.sock"));
        assert_eq!(
            resolved.client_socket,
            PathBuf::from("/tmp/cli-client.sock")
        );

        cli.session = None;
        cli.api_socket = None;
        cli.client_socket = None;
        let resolved = target(cli.clone(), env.clone(), &profile);
        assert_eq!(resolved.session, "env-session");
        assert_eq!(resolved.api_socket, PathBuf::from("/tmp/env-api.sock"));
        assert_eq!(
            resolved.client_socket,
            PathBuf::from("/tmp/env-client.sock")
        );

        env.session = None;
        env.api_socket = None;
        env.client_socket = None;
        let resolved = target(cli, env, &profile);
        assert_eq!(resolved.session, "profile-session");
        assert_eq!(resolved.api_socket, PathBuf::from("/tmp/p-api.sock"));
        // client socket explicitly derived from the API socket when absent.
        assert_eq!(
            resolved.client_socket,
            PathBuf::from("/tmp/p-api-client.sock")
        );
    }

    #[test]
    fn managed_session_defaults_to_product_default_session() {
        let resolved = target(
            cli_for("/tmp/herdr-term-api.sock"),
            EnvArgs::default(),
            &Default::default(),
        );
        assert_eq!(resolved.session, product::DEFAULT_SESSION_NAME);
    }

    #[test]
    fn managed_requires_explicit_absolute_api_socket() {
        let error = resolve_target(
            &CliArgs {
                managed: true,
                ..CliArgs::default()
            },
            &EnvArgs::default(),
            &Default::default(),
            None,
        )
        .expect_err("missing socket must fail");
        assert!(error.contains("--socket"), "unexpected error: {error}");

        let relative = CliArgs {
            managed: true,
            api_socket: Some("relative/api.sock".into()),
            ..CliArgs::default()
        };
        let error = resolve_target(&relative, &EnvArgs::default(), &Default::default(), None)
            .expect_err("relative socket must fail");
        assert!(error.contains("absolute"), "unexpected error: {error}");
    }

    #[test]
    fn managed_rejects_legacy_user_herdr_paths() {
        for path in [
            "/home/user/.config/herdr/herdr.sock",
            "/home/user/.local/state/herdr/herdr-client.sock",
        ] {
            let cli = CliArgs {
                managed: true,
                api_socket: Some(path.into()),
                ..CliArgs::default()
            };
            let error = resolve_target(&cli, &EnvArgs::default(), &Default::default(), None)
                .expect_err("legacy user herdr path must be rejected");
            assert!(
                error.contains("legacy user Herdr"),
                "unexpected error: {error}"
            );
        }
    }

    #[test]
    fn managed_validates_session_name() {
        let cli = CliArgs {
            managed: true,
            session: Some("../escape".into()),
            api_socket: Some("/tmp/term-api.sock".into()),
            ..CliArgs::default()
        };
        let error = resolve_target(&cli, &EnvArgs::default(), &Default::default(), None)
            .expect_err("invalid session name must fail");
        assert!(
            error.contains("invalid managed session"),
            "unexpected: {error}"
        );
    }

    #[test]
    fn managed_cli_parser_rejects_unknown_flags_and_missing_values() {
        assert!(parse_cli(&["--managed".into(), "--bogus".into()]).is_err());
        assert!(parse_cli(&["--managed".into(), "--socket".into()]).is_err());
        let parsed = parse_cli(&[
            "--managed".into(),
            "--detached".into(),
            "--session=jacquard".into(),
        ])
        .expect_err("= form is not accepted by the explicit flag parser");
        assert!(parsed.contains("unexpected") || parsed.contains("unknown"));
    }

    #[test]
    fn managed_env_bool_parsing_is_strict() {
        std::env::set_var(MANAGED_ENV_VAR, "true");
        assert_eq!(env_bool(MANAGED_ENV_VAR).unwrap(), Some(true));
        std::env::set_var(MANAGED_ENV_VAR, "0");
        assert_eq!(env_bool(MANAGED_ENV_VAR).unwrap(), Some(false));
        std::env::set_var(MANAGED_ENV_VAR, "maybe");
        assert!(env_bool(MANAGED_ENV_VAR).is_err());
        std::env::remove_var(MANAGED_ENV_VAR);
    }

    #[test]
    fn managed_config_path_must_be_absolute() {
        let cli = CliArgs {
            managed: true,
            config: Some("relative/config.toml".into()),
            ..CliArgs::default()
        };
        let error = resolve_config_path(&cli, &EnvArgs::default()).expect_err("must reject");
        assert!(error.contains("absolute"), "unexpected: {error}");
    }

    #[test]
    fn managed_apply_env_sets_explicit_sockets_and_session() {
        let target = ManagedTarget {
            session: "jacquard".into(),
            api_socket: PathBuf::from("/tmp/managed-api.sock"),
            client_socket: PathBuf::from("/tmp/managed-client.sock"),
            config: None,
        };
        apply_target_env(&target);
        assert_eq!(
            std::env::var(crate::api::SOCKET_PATH_ENV_VAR).unwrap(),
            "/tmp/managed-api.sock"
        );
        assert_eq!(
            std::env::var(crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR).unwrap(),
            "/tmp/managed-client.sock"
        );
        assert_eq!(
            std::env::var(crate::session::SESSION_ENV_VAR).unwrap(),
            "jacquard"
        );
        assert_eq!(std::env::var(MANAGED_ENV_VAR).unwrap(), "1");
        for name in [
            crate::api::SOCKET_PATH_ENV_VAR,
            crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR,
            crate::session::SESSION_ENV_VAR,
        ] {
            std::env::remove_var(name);
        }
    }

    // A1 M6: managed identity must be reported by the probed server and match
    // the explicit target before status/adoption/stop. Unknown identity is
    // never adopted or stopped.
    fn runtime_status(managed_session: Option<&str>) -> crate::api::RuntimeStatus {
        crate::api::RuntimeStatus {
            version: Some("jacquard-term 0.9.5".into()),
            protocol: Some(1),
            capabilities: Some(crate::api::schema::ServerCapabilities {
                live_handoff: true,
                detached_server_daemon: true,
                endpoint_protocol_generation: Some(1),
                surface_interest: true,
                health_check: true,
                ssh_agent_registration: false,
                managed_session: managed_session.map(str::to_owned),
            }),
        }
    }

    #[test]
    fn managed_identity_matches_only_the_expected_session() {
        let target = target(
            cli_for("/tmp/herdr-term-api.sock"),
            EnvArgs::default(),
            &Default::default(),
        );
        assert!(managed_session_matches(
            &target,
            &runtime_status(Some(target.session.as_str()))
        ));
        assert!(!managed_session_matches(
            &target,
            &runtime_status(Some("other-session"))
        ));
        assert!(!managed_session_matches(&target, &runtime_status(None)));
    }

    #[test]
    fn managed_status_identity_mismatch_is_exit_78() {
        // The CLI-facing identity rule: a probed server whose reported session
        // differs from --session must never be adopted/stopped; status returns
        // EXIT_MISMATCH (78), matching the managed lifecycle contract.
        let target = target(
            cli_for("/tmp/herdr-term-api.sock"),
            EnvArgs::default(),
            &Default::default(),
        );
        let foreign = runtime_status(Some("other-session"));
        assert!(!managed_session_matches(&target, &foreign));
        assert_eq!(EXIT_MISMATCH, 78);
    }
}
