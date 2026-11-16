pub(crate) const REMOTE_PREFERENCES_ENV_VAR: &str = "HERDR_REMOTE_PREFERENCES_IDENTITY";
pub(crate) const REATTACH_COMMAND_ENV_VAR: &str = "HERDR_REATTACH_COMMAND";
pub(crate) const REMOTE_KEYBINDINGS_ENV_VAR: &str = "HERDR_REMOTE_KEYBINDINGS";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteKeybindings {
    Local,
    Server,
}

impl RemoteKeybindings {
    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "local" => Ok(Self::Local),
            "server" => Ok(Self::Server),
            _ => Err("--remote-keybindings must be 'local' or 'server'".to_string()),
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Server => "server",
        }
    }
}

/// Explicit owner-side remote attach options (A1 Phase 3, D5/D6/D17).
///
/// These flags are only valid together with `--remote`; they pin the owner
/// binary and sockets instead of letting the client discover, install or
/// restart a Herdr binary on the remote host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RemoteExplicitOptions {
    /// Owner-side absolute path of the already-present bundled binary.
    pub(crate) binary: Option<String>,
    /// Owner-side absolute JSON API socket path.
    pub(crate) api_socket: Option<String>,
    /// Owner-side absolute client-protocol socket path.
    pub(crate) client_socket: Option<String>,
    /// Optional owner-side absolute config path (injected as HERDR_CONFIG_PATH).
    pub(crate) config: Option<String>,
    /// Never install/update the remote binary.
    pub(crate) no_install: bool,
    /// Never stop/restart an existing remote server.
    pub(crate) no_restart: bool,
}

impl RemoteExplicitOptions {
    /// True when any explicit owner-side option is present.
    pub(crate) fn has_any_path(&self) -> bool {
        self.binary.is_some()
            || self.api_socket.is_some()
            || self.client_socket.is_some()
            || self.config.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteLaunch {
    pub(crate) target: String,
    pub(crate) keybindings: RemoteKeybindings,
    pub(crate) live_handoff: bool,
    pub(crate) explicit: RemoteExplicitOptions,
}

pub(crate) fn extract_remote_args(
    args: &[String],
) -> Result<(Vec<String>, Option<RemoteLaunch>), String> {
    let mut cleaned = Vec::with_capacity(args.len());
    if let Some(program) = args.first() {
        cleaned.push(program.clone());
    }

    let mut remote_target = None;
    let mut keybindings = RemoteKeybindings::Local;
    let mut keybindings_seen = false;
    let mut live_handoff = false;
    let mut explicit = RemoteExplicitOptions::default();
    // First explicit option seen without --remote, used for an actionable error.
    let mut explicit_without_remote: Option<&'static str> = None;
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            cleaned.extend_from_slice(&args[index..]);
            break;
        }
        if arg == "--handoff" {
            live_handoff = true;
            index += 1;
            continue;
        }
        if arg == "--remote" {
            if remote_target.is_some() {
                return Err("--remote can only be specified once".to_string());
            }
            let Some(value) = args.get(index + 1) else {
                return Err("missing value for --remote".to_string());
            };
            remote_target = Some(validate_remote_target(value)?.to_owned());
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--remote=") {
            if remote_target.is_some() {
                return Err("--remote can only be specified once".to_string());
            }
            remote_target = Some(validate_remote_target(value)?.to_owned());
            index += 1;
            continue;
        }
        if arg == "--remote-keybindings" {
            if keybindings_seen {
                return Err("--remote-keybindings can only be specified once".to_string());
            }
            let Some(value) = args.get(index + 1) else {
                return Err("missing value for --remote-keybindings".to_string());
            };
            keybindings = RemoteKeybindings::parse(value)?;
            keybindings_seen = true;
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--remote-keybindings=") {
            if keybindings_seen {
                return Err("--remote-keybindings can only be specified once".to_string());
            }
            keybindings = RemoteKeybindings::parse(value)?;
            keybindings_seen = true;
            index += 1;
            continue;
        }

        // A1 Phase 3 explicit owner-side flags. They are parsed here and
        // removed from the cleaned argv so the generic CLI never sees them.
        if let Some((flag, value)) = explicit_inline_flag(arg) {
            set_explicit_value(flag, value, &mut explicit)?;
            if remote_target.is_none() {
                explicit_without_remote.get_or_insert(flag);
            }
            index += 1;
            continue;
        }
        if let Some(flag) = explicit_value_flag(arg) {
            let (value, consumed) = take_remote_value(args, index, flag)?;
            set_explicit_value(flag, value, &mut explicit)?;
            if remote_target.is_none() {
                explicit_without_remote.get_or_insert(flag);
            }
            index += consumed;
            continue;
        }
        if arg == "--remote-no-install" {
            explicit.no_install = true;
            if remote_target.is_none() {
                explicit_without_remote.get_or_insert("--remote-no-install");
            }
            index += 1;
            continue;
        }
        if arg == "--remote-no-restart" {
            explicit.no_restart = true;
            if remote_target.is_none() {
                explicit_without_remote.get_or_insert("--remote-no-restart");
            }
            index += 1;
            continue;
        }

        cleaned.push(arg.clone());
        index += 1;
    }

    if remote_target.is_none() {
        if let Some(flag) = explicit_without_remote {
            return Err(format!("{flag} requires --remote"));
        }
    }
    let remote = remote_target.map(|target| RemoteLaunch {
        target,
        keybindings,
        live_handoff,
        explicit,
    });
    if remote.is_none() && keybindings_seen {
        return Err("--remote-keybindings requires --remote".to_string());
    }
    if remote.is_none() && live_handoff {
        cleaned.push("--handoff".to_string());
    }

    Ok((cleaned, remote))
}

fn explicit_inline_flag(arg: &str) -> Option<(&'static str, &str)> {
    for flag in [
        "--remote-binary",
        "--remote-socket",
        "--remote-client-socket",
        "--remote-config",
    ] {
        if let Some(value) = arg
            .strip_prefix(flag)
            .and_then(|rest| rest.strip_prefix('='))
        {
            return Some((flag, value));
        }
    }
    None
}

fn explicit_value_flag(arg: &str) -> Option<&'static str> {
    match arg {
        "--remote-binary" => Some("--remote-binary"),
        "--remote-socket" => Some("--remote-socket"),
        "--remote-client-socket" => Some("--remote-client-socket"),
        "--remote-config" => Some("--remote-config"),
        _ => None,
    }
}

fn take_remote_value<'a>(
    args: &'a [String],
    index: usize,
    flag: &str,
) -> Result<(&'a str, usize), String> {
    match args.get(index + 1) {
        Some(value) if !value.starts_with('-') => Ok((value.as_str(), 2)),
        _ => Err(format!("missing value for {flag}")),
    }
}

fn set_explicit_value(
    flag: &'static str,
    value: &str,
    explicit: &mut RemoteExplicitOptions,
) -> Result<(), String> {
    let value = validate_owner_absolute_path(flag, value)?;
    let slot = match flag {
        "--remote-binary" => &mut explicit.binary,
        "--remote-socket" => &mut explicit.api_socket,
        "--remote-client-socket" => &mut explicit.client_socket,
        "--remote-config" => &mut explicit.config,
        other => return Err(format!("unsupported remote option {other}")),
    };
    if slot.is_some() {
        return Err(format!("{flag} can only be specified once"));
    }
    *slot = Some(value);
    Ok(())
}

/// Validate an owner-side absolute POSIX path from `--remote-*` flags.
///
/// The product release is Linux-only (2026-11-11 platform decision), so owner
/// paths are absolute POSIX paths. Values are later shell-quoted; control
/// characters are rejected here so a flag value can never smuggle a newline
/// into the remote command.
fn validate_owner_absolute_path(flag: &str, value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err(format!("{flag} requires a non-empty absolute path"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{flag} must not contain control characters"));
    }
    if !value.starts_with('/') {
        return Err(format!(
            "{flag} must be an absolute owner-side path starting with '/', got {value:?}"
        ));
    }
    Ok(value.to_string())
}

pub(crate) fn validate_remote_target(target: &str) -> Result<&str, String> {
    if target.is_empty() {
        return Err("missing value for --remote".to_string());
    }
    if target.starts_with('-') {
        return Err("--remote target must not start with '-'".to_string());
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        std::iter::once("herdr")
            .chain(values.iter().copied())
            .map(str::to_string)
            .collect()
    }

    fn parsed(values: &[&str]) -> RemoteLaunch {
        let (cleaned, remote) = extract_remote_args(&args(values)).expect("parses");
        assert_eq!(
            cleaned,
            vec!["herdr".to_string()],
            "explicit flags are stripped"
        );
        remote.expect("remote launch present")
    }

    #[test]
    fn remote_explicit_flags_parse_and_are_stripped_from_argv() {
        let launch = parsed(&[
            "--remote",
            "owner@login01",
            "--remote-binary",
            "/home/u/.local/libexec/jacquard/jacquard-term",
            "--remote-socket",
            "/run/user/1000/jacquard-term/api.sock",
            "--remote-client-socket",
            "/run/user/1000/jacquard-term/client.sock",
            "--remote-config",
            "/run/user/1000/jacquard-term/config.toml",
            "--remote-no-install",
            "--remote-no-restart",
        ]);
        assert_eq!(launch.target, "owner@login01");
        assert_eq!(
            launch.explicit.binary.as_deref(),
            Some("/home/u/.local/libexec/jacquard/jacquard-term")
        );
        assert_eq!(
            launch.explicit.api_socket.as_deref(),
            Some("/run/user/1000/jacquard-term/api.sock")
        );
        assert_eq!(
            launch.explicit.client_socket.as_deref(),
            Some("/run/user/1000/jacquard-term/client.sock")
        );
        assert_eq!(
            launch.explicit.config.as_deref(),
            Some("/run/user/1000/jacquard-term/config.toml")
        );
        assert!(launch.explicit.no_install);
        assert!(launch.explicit.no_restart);
        assert!(launch.explicit.has_any_path());
    }

    #[test]
    fn remote_explicit_flags_support_equals_form() {
        let launch = parsed(&[
            "--remote-binary=/opt/jacquard-term",
            "--remote-socket=/tmp/api.sock",
            "--remote",
            "host",
        ]);
        assert_eq!(
            launch.explicit.binary.as_deref(),
            Some("/opt/jacquard-term")
        );
        assert_eq!(launch.explicit.api_socket.as_deref(), Some("/tmp/api.sock"));
    }

    #[test]
    fn remote_explicit_flags_require_remote() {
        for values in [
            &["--remote-binary", "/opt/jacquard-term"][..],
            &["--remote-socket", "/tmp/api.sock"][..],
            &["--remote-client-socket", "/tmp/client.sock"][..],
            &["--remote-config", "/tmp/config.toml"][..],
            &["--remote-no-install"][..],
            &["--remote-no-restart"][..],
        ] {
            let error = extract_remote_args(&args(values)).expect_err("must require --remote");
            assert!(error.contains("requires --remote"), "unexpected: {error}");
        }
    }

    #[test]
    fn remote_explicit_paths_must_be_absolute_posix_paths() {
        for flag in [
            "--remote-binary",
            "--remote-socket",
            "--remote-client-socket",
            "--remote-config",
        ] {
            let error = extract_remote_args(&args(&["--remote", "host", flag, "relative/path"]))
                .expect_err("relative owner path must fail");
            assert!(
                error.contains("absolute owner-side path"),
                "{flag}: {error}"
            );
            let error =
                extract_remote_args(&args(&["--remote", "host", flag, "/tmp/with\nnewline"]))
                    .expect_err("control characters must fail");
            assert!(error.contains("control characters"), "{flag}: {error}");
            let error = extract_remote_args(&args(&["--remote", "host", flag]))
                .expect_err("missing value must fail");
            assert!(error.contains("missing value"), "{flag}: {error}");
        }
    }

    #[test]
    fn remote_explicit_flags_cannot_repeat() {
        let error = extract_remote_args(&args(&[
            "--remote",
            "host",
            "--remote-binary",
            "/opt/a",
            "--remote-binary",
            "/opt/b",
        ]))
        .expect_err("duplicate flag must fail");
        assert!(error.contains("only be specified once"), "{error}");
    }

    #[test]
    fn remote_explicit_options_default_to_empty() {
        let launch = parsed(&["--remote", "host"]);
        assert!(!launch.explicit.has_any_path());
        assert!(!launch.explicit.no_install);
        assert!(!launch.explicit.no_restart);
    }
}
