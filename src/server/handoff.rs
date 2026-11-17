#[cfg(unix)]
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Child, Command};
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use tracing::{info, warn};

#[cfg(unix)]
const HANDOFF_VERSION: u32 = 1;
#[cfg(unix)]
const READY_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(unix)]
const OWNED_ACK_TIMEOUT: Duration = Duration::from_millis(500);
// Descriptors are transferred in batches of this size. A single SCM_RIGHTS
// control message caps out at 253 descriptors on Linux and 254 on macOS, so the
// batch stays well below both limits and the number of panes stays unbounded.
#[cfg(unix)]
const FDS_PER_MESSAGE: usize = 64;
#[cfg(unix)]
pub(crate) const MAX_REPLAY_BYTES_PER_PANE: usize = 8 * 1024;
#[cfg(unix)]
pub(crate) const COMMIT_TIMEOUT: Duration = READY_TIMEOUT;

#[cfg(unix)]
#[derive(Serialize, Deserialize)]
pub(crate) struct HandoffManifest {
    pub version: u32,
    pub source_version: String,
    pub source_protocol: u32,
    pub expected_version: Option<String>,
    pub expected_protocol: Option<u32>,
    /// V-07a expected build identity gates. The source server writes the
    /// expected values from `server.live_handoff` params into the manifest; the
    /// import server validates them against its own compile-time identity and
    /// fails closed before receiving pane descriptors. The fields are additive
    /// and default to absent, so manifests written by older servers still
    /// decode and pre-V-07a sources simply do not write them (callers must not
    /// claim build-gated handoff from such a source). `HANDOFF_VERSION` stays 1.
    #[serde(default)]
    pub expected_build_id: Option<String>,
    #[serde(default)]
    pub expected_product_revision: Option<String>,
    #[serde(default)]
    pub expected_fork_commit: Option<String>,
    pub snapshot: crate::persist::SessionSnapshot,
    pub panes: Vec<crate::handoff_runtime::HandoffRuntimeState>,
    /// An outer window title set over the API outlives the server that took the
    /// call, so a handoff carries it rather than falling back to the config.
    /// Absent from manifests written before this field existed.
    #[serde(default)]
    pub api_window_title: Option<String>,
    /// A host restart or shutdown announced to the old server but not yet
    /// committed. Absent from manifests written before this field existed.
    #[serde(default)]
    pub host_shutdown_intent: Option<crate::platform::HostShutdownIntent>,
}

#[cfg(unix)]
pub(crate) struct ReceivedHandoff {
    pub manifest: HandoffManifest,
    pub fds: Vec<RawFd>,
    pub stream: UnixStream,
}

/// Expected build identity gate values carried by a handoff manifest (V-07a P3).
#[cfg(unix)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ExpectedBuildIdentity<'a> {
    build_id: Option<&'a str>,
    product_revision: Option<&'a str>,
    fork_commit: Option<&'a str>,
}

#[cfg(unix)]
impl<'a> ExpectedBuildIdentity<'a> {
    fn from_manifest(manifest: &'a HandoffManifest) -> Self {
        Self {
            build_id: manifest.expected_build_id.as_deref(),
            product_revision: manifest.expected_product_revision.as_deref(),
            fork_commit: manifest.expected_fork_commit.as_deref(),
        }
    }
}

/// Validate any expected build identity against the import server's own
/// compile-time identity (V-07a P3).
///
/// Fails closed with a clear message before the manifest is acknowledged, so the
/// source server rolls back and keeps serving panes. A missing local value for
/// an expected field is a mismatch, not a pass: a build that cannot prove its
/// identity must not receive a handoff that was gated on it. All-absent
/// expectations are allowed — this is the additive/pre-V-07a-source path.
#[cfg(unix)]
fn validate_expected_build_identity(
    expected: ExpectedBuildIdentity<'_>,
    local: crate::build_info::BuildIdentity,
) -> Result<(), String> {
    for (label, expected_value, local_value) in [
        ("build id", expected.build_id, local.build_id),
        (
            "product revision",
            expected.product_revision,
            local.product_revision,
        ),
        ("fork commit", expected.fork_commit, local.fork_commit),
    ] {
        let Some(expected_value) = expected_value else {
            continue;
        };
        match local_value {
            Some(local_value) if local_value == expected_value => {}
            Some(local_value) => {
                return Err(format!(
                    "handoff expected {label} {expected_value:?}, but this server was built with {label} {local_value:?}"
                ));
            }
            None => {
                return Err(format!(
                    "handoff expected {label} {expected_value:?}, but this server was built without a {label} value"
                ));
            }
        }
    }
    Ok(())
}

/// Directory for the handoff control socket (V-07a P2).
///
/// Product managed servers export an explicit `HERDR_SOCKET_PATH` pointing at a
/// node-local runtime directory (the managed JSON API socket). The handoff
/// control socket must live beside it: binding under `session::data_dir()`
/// (`$XDG_CONFIG_HOME/...`, possibly a shared HOME/NAS) is unreliable for
/// AF_UNIX sockets, and a pid-only name can collide across login nodes. When no
/// explicit API socket override is set, keep the upstream location.
#[cfg(unix)]
pub(crate) fn handoff_socket_path() -> PathBuf {
    handoff_socket_path_from(
        std::env::var_os(crate::api::SOCKET_PATH_ENV_VAR).map(PathBuf::from),
        &crate::session::data_dir(),
        std::process::id(),
    )
}

/// Pure resolution of the handoff control socket path.
///
/// Prefers the parent directory of the explicit API socket override (node-local
/// in product managed mode) and falls back to the persistent session data dir.
#[cfg(unix)]
fn handoff_socket_path_from(
    api_socket_override: Option<PathBuf>,
    fallback_dir: &Path,
    pid: u32,
) -> PathBuf {
    let directory = api_socket_override
        .as_deref()
        .and_then(Path::parent)
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| fallback_dir.to_path_buf(), Path::to_path_buf);
    directory.join(format!("herdr-handoff-{pid}.sock"))
}

/// The explicit managed-server environment values a handoff import child must
/// keep (V-07a P1).
///
/// Product builds sanitize inherited `HERDR_*` overrides at startup unless the
/// internal-child marker is set (`product::sanitize_inherited_herdr_env`). The
/// import child re-runs that startup path, so without the marker it would drop
/// the explicit managed socket/session/config values and bind the default
/// app-dir sockets: the managed launcher then cannot adopt the replacement and
/// M6 reports a foreign/unknown server (78).
#[cfg(unix)]
pub(crate) const HANDOFF_IMPORT_ENV_VARS: [&str; 4] = [
    crate::session::SESSION_ENV_VAR,
    crate::api::SOCKET_PATH_ENV_VAR,
    crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR,
    crate::config::CONFIG_PATH_ENV_VAR,
];

/// Pure helper: pair the explicit handoff-import env values with their variable
/// names, skipping values that are not set in the source server environment.
#[cfg(unix)]
fn handoff_import_env(
    session: Option<std::ffi::OsString>,
    api_socket: Option<std::ffi::OsString>,
    client_socket: Option<std::ffi::OsString>,
    config: Option<std::ffi::OsString>,
) -> Vec<(&'static str, std::ffi::OsString)> {
    [
        (HANDOFF_IMPORT_ENV_VARS[0], session),
        (HANDOFF_IMPORT_ENV_VARS[1], api_socket),
        (HANDOFF_IMPORT_ENV_VARS[2], client_socket),
        (HANDOFF_IMPORT_ENV_VARS[3], config),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.map(|value| (name, value)))
    .collect()
}

/// Configure the import child environment: mark it as an internal child and
/// re-inject the explicit managed environment from the source server process so
/// the replacement binds exactly the sockets/config/session this server uses
/// (V-07a P1).
///
/// When the source server was started with an explicit `--session` argument,
/// the upstream contract still removes leaked socket overrides: the import child
/// no longer has that argument, so a leaked `HERDR_SOCKET_PATH` (for example
/// from a nested Herdr pane) would otherwise mask the named session. Product
/// managed servers are unaffected by that branch because managed mode applies
/// its explicit target environment *before* startup without going through the
/// `--session` CLI normalization, so `explicit_session_requested()` is false
/// there and all explicit managed values are kept.
#[cfg(unix)]
fn configure_handoff_import_child(
    command: &mut Command,
    explicit_session: bool,
    session: Option<std::ffi::OsString>,
    api_socket: Option<std::ffi::OsString>,
    client_socket: Option<std::ffi::OsString>,
    config: Option<std::ffi::OsString>,
) {
    // The marker only suppresses inherited-env sanitization for a child we
    // spawned ourselves; it grants no privilege.
    let marker = std::ffi::OsString::from("1");
    command.env(crate::product::INTERNAL_CHILD_ENV_VAR, marker);
    if explicit_session {
        command
            .env_remove(crate::api::SOCKET_PATH_ENV_VAR)
            .env_remove(crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR);
    }
    let environment = if explicit_session {
        handoff_import_env(session, None, None, config)
    } else {
        handoff_import_env(session, api_socket, client_socket, config)
    };
    for (name, value) in environment {
        command.env(name, value);
    }
}

#[cfg(unix)]
pub(crate) fn spawn_handoff_import(
    import_exe: Option<&Path>,
    socket_path: &Path,
    token: &str,
) -> io::Result<Child> {
    let fallback_exe;
    let exe = if let Some(import_exe) = import_exe {
        import_exe
    } else {
        fallback_exe = std::env::current_exe().map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("failed to determine herdr executable path: {err}"),
            )
        })?;
        &fallback_exe
    };
    let mut command = Command::new(exe);
    command
        .arg("server")
        .arg("--handoff-import")
        .arg(socket_path)
        .arg(token)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // V-07a P1: keep the explicit managed environment instead of letting the
    // import child re-derive defaults after product env sanitization. The four
    // values are re-injected *from this server's environment*, which is exactly
    // what this server serves on: product managed servers are launched with
    // their explicit socket/session/config env and never run the `--session` CLI
    // normalization, so nothing is stripped here. Removing the old
    // unconditional `env_remove` is the fix: it could strip the managed API and
    // client sockets and make the replacement bind default app-dir paths, so M6
    // would then refuse to adopt the replacement (78).
    configure_handoff_import_child(
        &mut command,
        crate::session::explicit_session_requested(),
        std::env::var_os(crate::session::SESSION_ENV_VAR),
        std::env::var_os(crate::api::SOCKET_PATH_ENV_VAR),
        std::env::var_os(crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR),
        std::env::var_os(crate::config::CONFIG_PATH_ENV_VAR),
    );
    crate::platform::detach_server_daemon_command(&mut command);
    command.spawn().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to spawn handoff import server at {}: {err}",
                exe.display()
            ),
        )
    })
}

#[cfg(unix)]
pub(crate) fn cleanup_failed_import_child(child: &mut Child) {
    let pid = child.id();
    match child.try_wait() {
        Ok(Some(status)) => {
            info!(pid, status = %status, "handoff import server exited during rollback");
            return;
        }
        Ok(None) => {}
        Err(err) => {
            warn!(pid, err = %err, "failed to inspect handoff import server before rollback");
        }
    }

    if let Err(err) = child.kill() {
        warn!(pid, err = %err, "failed to kill handoff import server during rollback");
    }
    match child.wait() {
        Ok(status) => {
            info!(pid, status = %status, "handoff import server reaped during rollback");
        }
        Err(err) => {
            warn!(pid, err = %err, "failed to reap handoff import server during rollback");
        }
    }
}

#[cfg(unix)]
pub(crate) fn bind_listener(socket_path: &Path) -> io::Result<UnixListener> {
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
    restrict_socket_permissions(socket_path)?;
    Ok(listener)
}

#[cfg(unix)]
pub(crate) fn accept_and_validate_on(
    listener: UnixListener,
    socket_path: &Path,
    token: &str,
    manifest: &HandoffManifest,
) -> io::Result<UnixStream> {
    let (mut stream, _) = accept_with_timeout(&listener, READY_TIMEOUT)?;
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    stream.set_write_timeout(Some(READY_TIMEOUT))?;
    let token_line = read_line_unbuffered(&mut stream)?;
    if token_line.trim_end() != token {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "handoff import token mismatch",
        ));
    }

    serde_json::to_writer(&mut stream, manifest).map_err(io::Error::other)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    let validated = read_line_unbuffered(&mut stream)?;
    if validated.trim_end() != "validated" {
        return Err(io::Error::other("handoff import did not validate manifest"));
    }
    let _ = std::fs::remove_file(socket_path);
    Ok(stream)
}

#[cfg(unix)]
pub(crate) fn send_fds_and_wait_restored(stream: &mut UnixStream, fds: &[RawFd]) -> io::Result<()> {
    send_fds(stream, fds)?;

    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    let restored = read_line_unbuffered(&mut *stream)?;
    if restored.trim_end() != "restored" {
        return Err(io::Error::other(
            "handoff import did not report restored runtimes",
        ));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn wait_ready(stream: &mut UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    let ready = read_line_unbuffered(&mut *stream)?;
    if ready.trim_end() != "ready" {
        return Err(io::Error::other("handoff import did not report ready"));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn report_committed(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"committed\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn wait_owned_ack(stream: &mut UnixStream) {
    if let Err(err) = stream.set_read_timeout(Some(OWNED_ACK_TIMEOUT)) {
        warn!(err = %err, "failed to set handoff ownership ack timeout");
        return;
    }
    match read_line_unbuffered(&mut *stream) {
        Ok(owned) if owned.trim_end() == "owned" => {}
        Ok(other) => {
            warn!(
                response = %other.trim_end(),
                "handoff import sent unexpected ownership ack after commit"
            );
        }
        Err(err) => {
            warn!(err = %err, "handoff import ownership ack was not received after commit");
        }
    }
}

#[cfg(unix)]
pub(crate) fn receive(socket_path: &Path, token: &str) -> io::Result<ReceivedHandoff> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.write_all(token.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let manifest_line = read_line_unbuffered(&mut stream)?;
    let manifest: HandoffManifest =
        serde_json::from_str(&manifest_line).map_err(io::Error::other)?;
    if manifest.version != HANDOFF_VERSION {
        return Err(io::Error::other(format!(
            "unsupported handoff version {}",
            manifest.version
        )));
    }
    if manifest
        .expected_protocol
        .is_some_and(|protocol| protocol != crate::protocol::PROTOCOL_VERSION)
    {
        return Err(io::Error::other(format!(
            "handoff expected protocol {}, but this server speaks protocol {}",
            manifest.expected_protocol.unwrap_or_default(),
            crate::protocol::PROTOCOL_VERSION
        )));
    }
    if manifest
        .expected_version
        .as_deref()
        .is_some_and(|version| version != crate::build_info::version())
    {
        return Err(io::Error::other(format!(
            "handoff expected herdr v{}, but this server is v{}",
            manifest.expected_version.as_deref().unwrap_or("unknown"),
            crate::build_info::version()
        )));
    }
    // V-07a P3: gate on the source server's expected compile-time identity
    // before acknowledging the manifest, so a failed gate rolls back on the
    // source and the panes keep running there.
    if let Err(message) = validate_expected_build_identity(
        ExpectedBuildIdentity::from_manifest(&manifest),
        crate::build_info::identity(),
    ) {
        return Err(io::Error::other(message));
    }
    stream.write_all(b"validated\n")?;
    stream.flush()?;
    let fds = recv_fds(&stream, manifest.panes.len())?;
    Ok(ReceivedHandoff {
        manifest,
        fds,
        stream,
    })
}

#[cfg(unix)]
pub(crate) fn report_restored(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"restored\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn report_ready(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"ready\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn wait_committed(stream: &mut UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(READY_TIMEOUT))?;
    let committed = read_line_unbuffered(&mut *stream)?;
    if committed.trim_end() != "committed" {
        return Err(io::Error::other("handoff source did not commit"));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn report_owned(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"owned\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn manifest_for(
    snapshot: crate::persist::SessionSnapshot,
    panes: Vec<crate::handoff_runtime::HandoffRuntimeState>,
    params: &crate::api::schema::ServerLiveHandoffParams,
    api_window_title: Option<String>,
    host_shutdown_intent: Option<crate::platform::HostShutdownIntent>,
) -> HandoffManifest {
    HandoffManifest {
        version: HANDOFF_VERSION,
        source_version: crate::build_info::version(),
        source_protocol: crate::protocol::PROTOCOL_VERSION,
        expected_version: params.expected_version.clone(),
        expected_protocol: params.expected_protocol,
        expected_build_id: params.expected_build_id.clone(),
        expected_product_revision: params.expected_product_revision.clone(),
        expected_fork_commit: params.expected_fork_commit.clone(),
        snapshot,
        panes,
        api_window_title,
        host_shutdown_intent,
    }
}

#[cfg(unix)]
fn restrict_socket_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(unix)]
fn accept_with_timeout(
    listener: &UnixListener,
    timeout: Duration,
) -> io::Result<(UnixStream, std::os::unix::net::SocketAddr)> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok(accepted) => return Ok(accepted),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out waiting for handoff import connection",
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
}

#[cfg(unix)]
fn read_line_unbuffered(stream: &mut UnixStream) -> io::Result<String> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = stream.read(&mut byte)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "handoff stream closed while reading line",
            ));
        }
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            return String::from_utf8(bytes)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err));
        }
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handoff line exceeded maximum size",
            ));
        }
    }
}

#[cfg(unix)]
fn send_fds(stream: &UnixStream, fds: &[RawFd]) -> io::Result<()> {
    for batch in fds.chunks(FDS_PER_MESSAGE) {
        send_fd_batch(stream, batch)?;
    }
    Ok(())
}

#[cfg(unix)]
fn send_fd_batch(stream: &UnixStream, fds: &[RawFd]) -> io::Result<()> {
    if fds.is_empty() {
        return Ok(());
    }
    let byte = [b'F'];
    let iov = [libc::iovec {
        iov_base: byte.as_ptr() as *mut libc::c_void,
        iov_len: byte.len(),
    }];
    let fd_bytes = std::mem::size_of_val(fds);
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(fd_bytes as u32) as usize }];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = iov.as_ptr() as *mut libc::iovec;
    msg.msg_iovlen = iov.len() as _;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = control.len() as _;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("failed to allocate fd control message"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_bytes as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, libc::CMSG_DATA(cmsg), fd_bytes);
        if libc::sendmsg(stream.as_raw_fd(), &msg, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn close_raw_fds(fds: &[RawFd]) {
    for fd in fds {
        let _ = unsafe { libc::close(*fd) };
    }
}

#[cfg(unix)]
fn recv_fds(stream: &UnixStream, expected: usize) -> io::Result<Vec<RawFd>> {
    let mut out: Vec<RawFd> = Vec::with_capacity(expected);
    while out.len() < expected {
        let wanted = (expected - out.len()).min(FDS_PER_MESSAGE);
        let batch = match recv_fd_batch(stream, wanted) {
            Ok(batch) => batch,
            Err(err) => {
                close_raw_fds(&out);
                return Err(err);
            }
        };
        if batch.is_empty() {
            let received = out.len();
            close_raw_fds(&out);
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "handoff stream closed after {received} of {expected} pane file descriptors"
                ),
            ));
        }
        out.extend(batch);
    }
    Ok(out)
}

#[cfg(unix)]
fn recv_fd_batch(stream: &UnixStream, wanted: usize) -> io::Result<Vec<RawFd>> {
    let mut byte = [0u8; 1];
    let mut iov = [libc::iovec {
        iov_base: byte.as_mut_ptr() as *mut libc::c_void,
        iov_len: byte.len(),
    }];
    let fd_bytes = wanted * std::mem::size_of::<RawFd>();
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(fd_bytes as u32) as usize }];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = iov.as_mut_ptr();
    msg.msg_iovlen = iov.len() as _;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = control.len() as _;

    let read = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut out = Vec::new();
    unsafe {
        let control_end = control.as_ptr() as usize + msg.msg_controllen as usize;
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg);
                // Bound the payload by both the header's own length and the
                // bytes the kernel wrote into `control`, so the read below can
                // never run past the buffer.
                let available = control_end.saturating_sub(data as usize);
                let data_len = ((*cmsg).cmsg_len as usize)
                    .saturating_sub(libc::CMSG_LEN(0) as usize)
                    .min(available);
                let count = data_len / std::mem::size_of::<RawFd>();
                let data = data as *const RawFd;
                for idx in 0..count {
                    out.push(*data.add(idx));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }

    // Truncation means the kernel closed the descriptors that did not fit, so
    // the batch is unrecoverable rather than merely short.
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        close_raw_fds(&out);
        return Err(io::Error::other("handoff fd control message was truncated"));
    }
    if read == 0 {
        close_raw_fds(&out);
        return Ok(Vec::new());
    }
    if out.len() > wanted {
        let received = out.len();
        close_raw_fds(&out);
        return Err(io::Error::other(format!(
            "handoff fd message carried {received} descriptors, expected at most {wanted}"
        )));
    }
    if out.is_empty() {
        return Err(io::Error::other("handoff fd message missing SCM_RIGHTS"));
    }
    Ok(out)
}

#[cfg(unix)]
pub(crate) fn log_import_result(panes: usize) {
    info!(panes, "handoff import ready");
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn empty_snapshot() -> crate::persist::SessionSnapshot {
        crate::persist::SessionSnapshot {
            version: 0,
            workspaces: Vec::new(),
            active: None,
            selected: 0,
            sidebar_width: None,
            sidebar_section_split: None,
            collapsed_space_keys: Default::default(),
        }
    }

    fn handoff_params() -> crate::api::schema::ServerLiveHandoffParams {
        crate::api::schema::ServerLiveHandoffParams::default()
    }

    fn params_with_expected_build_identity() -> crate::api::schema::ServerLiveHandoffParams {
        crate::api::schema::ServerLiveHandoffParams {
            expected_protocol: Some(crate::protocol::PROTOCOL_VERSION),
            expected_version: Some(crate::build_info::version()),
            expected_build_id: Some("jq.7".to_string()),
            expected_product_revision: Some("jq.7".to_string()),
            expected_fork_commit: Some("68846ca".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn a_handoff_carries_an_api_set_window_title() {
        let manifest = manifest_for(
            empty_snapshot(),
            Vec::new(),
            &handoff_params(),
            Some("deploying".to_string()),
            None,
        );

        assert_eq!(manifest.api_window_title.as_deref(), Some("deploying"));
    }

    #[test]
    fn a_handoff_carries_an_announced_host_shutdown() {
        let intent: crate::platform::HostShutdownIntent =
            serde_json::from_value(serde_json::json!({ "announced_at_ns": 42 }))
                .expect("intent should deserialize");
        let manifest = manifest_for(
            empty_snapshot(),
            Vec::new(),
            &handoff_params(),
            None,
            Some(intent),
        );
        let encoded = serde_json::to_vec(&manifest).expect("manifest should serialize");
        let decoded: HandoffManifest =
            serde_json::from_slice(&encoded).expect("manifest should deserialize");

        assert_eq!(decoded.host_shutdown_intent, Some(intent));
    }

    #[test]
    fn a_manifest_written_before_the_host_shutdown_field_still_loads() {
        let manifest = manifest_for(empty_snapshot(), Vec::new(), &handoff_params(), None, None);
        let mut value = serde_json::to_value(&manifest).expect("manifest should serialize");
        value
            .as_object_mut()
            .expect("manifest should be a json object")
            .remove("host_shutdown_intent");

        let older: HandoffManifest =
            serde_json::from_value(value).expect("an older manifest should still load");

        assert!(older.host_shutdown_intent.is_none());
    }

    #[test]
    fn an_older_server_imports_a_manifest_with_an_announced_host_shutdown() {
        /// The manifest as servers before `host_shutdown_intent` decode it,
        /// frozen here so the new field stays ignorable by them.
        #[derive(Deserialize)]
        #[allow(dead_code)] // Only decoding is under test.
        struct PreHostShutdownManifest {
            version: u32,
            source_version: String,
            source_protocol: u32,
            expected_version: Option<String>,
            expected_protocol: Option<u32>,
            snapshot: crate::persist::SessionSnapshot,
            panes: Vec<crate::handoff_runtime::HandoffRuntimeState>,
            #[serde(default)]
            api_window_title: Option<String>,
        }

        let intent: crate::platform::HostShutdownIntent =
            serde_json::from_value(serde_json::json!({ "announced_at_ns": 42 }))
                .expect("intent should deserialize");
        let manifest = manifest_for(
            empty_snapshot(),
            Vec::new(),
            &handoff_params(),
            Some("deploying".to_string()),
            Some(intent),
        );
        let encoded = serde_json::to_vec(&manifest).expect("manifest should serialize");

        let older: PreHostShutdownManifest =
            serde_json::from_slice(&encoded).expect("an older server should still load it");

        assert_eq!(older.version, HANDOFF_VERSION);
        assert_eq!(older.api_window_title.as_deref(), Some("deploying"));
    }

    #[test]
    fn a_manifest_written_before_the_title_field_still_loads() {
        let manifest = manifest_for(
            empty_snapshot(),
            Vec::new(),
            &handoff_params(),
            Some("deploying".to_string()),
            None,
        );
        let mut value = serde_json::to_value(&manifest).expect("manifest should serialize");
        value
            .as_object_mut()
            .expect("manifest should be a json object")
            .remove("api_window_title");

        let older: HandoffManifest =
            serde_json::from_value(value).expect("an older manifest should still load");

        assert!(older.api_window_title.is_none());
    }

    #[test]
    fn a_handoff_carries_the_expected_build_identity_gates() {
        let params = params_with_expected_build_identity();
        let manifest = manifest_for(empty_snapshot(), Vec::new(), &params, None, None);
        let encoded = serde_json::to_vec(&manifest).expect("manifest should serialize");
        let decoded: HandoffManifest =
            serde_json::from_slice(&encoded).expect("manifest should deserialize");

        assert_eq!(decoded.version, HANDOFF_VERSION);
        assert_eq!(decoded.expected_protocol, params.expected_protocol);
        assert_eq!(decoded.expected_version, params.expected_version);
        assert_eq!(decoded.expected_build_id.as_deref(), Some("jq.7"));
        assert_eq!(decoded.expected_product_revision.as_deref(), Some("jq.7"));
        assert_eq!(decoded.expected_fork_commit.as_deref(), Some("68846ca"));
    }

    #[test]
    fn a_manifest_written_before_the_build_identity_fields_still_loads() {
        let manifest = manifest_for(empty_snapshot(), Vec::new(), &handoff_params(), None, None);
        let mut value = serde_json::to_value(&manifest).expect("manifest should serialize");
        let object = value
            .as_object_mut()
            .expect("manifest should be a json object");
        for field in [
            "expected_build_id",
            "expected_product_revision",
            "expected_fork_commit",
        ] {
            object.remove(field);
        }

        let older: HandoffManifest =
            serde_json::from_value(value).expect("an older manifest should still load");

        assert!(older.expected_build_id.is_none());
        assert!(older.expected_product_revision.is_none());
        assert!(older.expected_fork_commit.is_none());
    }

    #[test]
    fn an_older_server_imports_a_manifest_with_expected_build_identity() {
        /// The manifest as pre-V-07a servers decode it: unknown fields are
        /// ignored, so adding P3 gates stays additive and HANDOFF_VERSION 1.
        #[derive(Deserialize)]
        #[allow(dead_code)] // Only decoding is under test.
        struct PreBuildIdentityManifest {
            version: u32,
            source_version: String,
            source_protocol: u32,
            expected_version: Option<String>,
            expected_protocol: Option<u32>,
            snapshot: crate::persist::SessionSnapshot,
            panes: Vec<crate::handoff_runtime::HandoffRuntimeState>,
        }

        let manifest = manifest_for(
            empty_snapshot(),
            Vec::new(),
            &params_with_expected_build_identity(),
            None,
            None,
        );
        let encoded = serde_json::to_vec(&manifest).expect("manifest should serialize");

        let older: PreBuildIdentityManifest =
            serde_json::from_slice(&encoded).expect("an older server should still load it");

        assert_eq!(older.version, HANDOFF_VERSION);
    }

    fn local_identity(
        product_revision: Option<&'static str>,
        fork_commit: Option<&'static str>,
        build_id: Option<&'static str>,
    ) -> crate::build_info::BuildIdentity {
        crate::build_info::BuildIdentity {
            product_revision,
            fork_commit,
            build_id,
        }
    }

    #[test]
    fn absent_expected_build_identity_never_blocks_a_handoff() {
        assert_eq!(
            validate_expected_build_identity(
                ExpectedBuildIdentity::default(),
                local_identity(Some("jq.7"), Some("68846ca"), Some("7")),
            ),
            Ok(())
        );
        assert_eq!(
            validate_expected_build_identity(
                ExpectedBuildIdentity::default(),
                local_identity(None, None, None),
            ),
            Ok(())
        );
    }

    #[test]
    fn matching_expected_build_identity_is_accepted() {
        let expected = ExpectedBuildIdentity {
            build_id: Some("7"),
            product_revision: Some("jq.7"),
            fork_commit: Some("68846ca"),
        };
        assert_eq!(
            validate_expected_build_identity(
                expected,
                local_identity(Some("jq.7"), Some("68846ca"), Some("7")),
            ),
            Ok(())
        );
    }

    #[test]
    fn mismatched_expected_build_identity_fails_closed() {
        let expected = ExpectedBuildIdentity {
            build_id: Some("7"),
            ..Default::default()
        };
        let error = validate_expected_build_identity(
            expected,
            local_identity(Some("jq.7"), Some("68846ca"), Some("6")),
        )
        .expect_err("a different build id must be rejected");
        assert!(
            error.contains("handoff expected build id \"7\""),
            "error must name the gate and expected value: {error}"
        );
        assert!(
            error.contains("built with build id \"6\""),
            "error must report the local value: {error}"
        );

        let expected = ExpectedBuildIdentity {
            product_revision: Some("jq.7"),
            fork_commit: Some("68846ca"),
            ..Default::default()
        };
        assert!(
            validate_expected_build_identity(
                expected,
                local_identity(Some("jq.7"), Some("dcc017c"), None),
            )
            .is_err(),
            "a different fork commit must be rejected"
        );
    }

    #[test]
    fn expected_build_identity_missing_locally_fails_closed() {
        let expected = ExpectedBuildIdentity {
            product_revision: Some("jq.7"),
            ..Default::default()
        };
        let error = validate_expected_build_identity(expected, local_identity(None, None, None))
            .expect_err("an expected value this build cannot prove must be rejected");
        assert!(
            error.contains("built without a product revision value"),
            "unknown local identity must fail closed with a clear message: {error}"
        );
    }

    #[test]
    fn handoff_import_env_keeps_the_explicit_managed_environment() {
        let environment = handoff_import_env(
            Some("jacquard".into()),
            Some("/run/user/1000/jacquard/nodes/host/managed/herdr.sock".into()),
            Some("/run/user/1000/jacquard/nodes/host/managed/herdr-client.sock".into()),
            Some("/home/me/.config/jacquard-term/config.toml".into()),
        );

        assert_eq!(
            environment
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>(),
            HANDOFF_IMPORT_ENV_VARS.to_vec()
        );

        let mut command = Command::new("/bin/true");
        configure_handoff_import_child(
            &mut command,
            false,
            Some("jacquard".into()),
            Some("/run/user/1000/jacquard/nodes/host/managed/herdr.sock".into()),
            Some("/run/user/1000/jacquard/nodes/host/managed/herdr-client.sock".into()),
            Some("/home/me/.config/jacquard-term/config.toml".into()),
        );
        let envs = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();

        assert_eq!(
            envs.get(crate::product::INTERNAL_CHILD_ENV_VAR),
            Some(&Some("1".to_string()))
        );
        assert_eq!(
            envs.get(crate::session::SESSION_ENV_VAR),
            Some(&Some("jacquard".to_string()))
        );
        assert_eq!(
            envs.get(crate::api::SOCKET_PATH_ENV_VAR),
            Some(&Some(
                "/run/user/1000/jacquard/nodes/host/managed/herdr.sock".to_string()
            ))
        );
        assert_eq!(
            envs.get(crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR),
            Some(&Some(
                "/run/user/1000/jacquard/nodes/host/managed/herdr-client.sock".to_string()
            ))
        );
        assert_eq!(
            envs.get(crate::config::CONFIG_PATH_ENV_VAR),
            Some(&Some(
                "/home/me/.config/jacquard-term/config.toml".to_string()
            ))
        );
        // P1 keeps values; it never removes inherited ones.
        assert!(envs.values().all(Option::is_some));
        assert_eq!(
            envs.len(),
            HANDOFF_IMPORT_ENV_VARS.len() + 1,
            "internal-child marker plus the four explicit values"
        );
    }

    #[test]
    fn handoff_import_child_drops_leaked_socket_overrides_for_an_explicit_session() {
        // Upstream named-session contract (kept): the import child has no
        // `--session` argument, so leaked socket overrides must not mask the
        // inherited HERDR_SESSION. Product managed servers never reach this
        // branch because managed mode applies its target env before startup.
        let mut command = Command::new("/bin/true");
        configure_handoff_import_child(
            &mut command,
            true,
            Some("work".into()),
            Some("/home/me/.config/herdr-dev/herdr.sock".into()),
            Some("/home/me/.config/herdr-dev/herdr-client.sock".into()),
            None,
        );
        let envs = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();

        assert_eq!(
            envs.get(crate::product::INTERNAL_CHILD_ENV_VAR),
            Some(&Some("1".to_string()))
        );
        assert_eq!(
            envs.get(crate::session::SESSION_ENV_VAR),
            Some(&Some("work".to_string()))
        );
        assert_eq!(
            envs.get(crate::api::SOCKET_PATH_ENV_VAR),
            Some(&None),
            "leaked API socket override must still be removed for a named session"
        );
        assert_eq!(
            envs.get(crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR),
            Some(&None),
            "leaked client socket override must still be removed for a named session"
        );
    }

    #[test]
    fn handoff_import_env_skips_unset_values() {
        let environment = handoff_import_env(
            None,
            Some("/run/user/1000/jacquard/herdr.sock".into()),
            None,
            None,
        );

        assert_eq!(
            environment,
            vec![(
                crate::api::SOCKET_PATH_ENV_VAR,
                std::ffi::OsString::from("/run/user/1000/jacquard/herdr.sock")
            )]
        );
    }

    #[test]
    fn handoff_socket_path_prefers_the_explicit_api_socket_parent() {
        let path = handoff_socket_path_from(
            Some(PathBuf::from(
                "/run/user/1000/jacquard/nodes/host/managed/herdr.sock",
            )),
            Path::new("/home/me/.config/herdr-dev/sessions/jacquard"),
            4242,
        );

        assert_eq!(
            path,
            PathBuf::from("/run/user/1000/jacquard/nodes/host/managed/herdr-handoff-4242.sock")
        );
    }

    #[test]
    fn handoff_socket_path_falls_back_without_an_api_socket_override() {
        let fallback = Path::new("/home/me/.config/herdr-dev/sessions/jacquard");

        assert_eq!(
            handoff_socket_path_from(None, fallback, 7),
            fallback.join("herdr-handoff-7.sock")
        );
        // An override without a parent directory cannot provide a node-local
        // directory, so the fallback still wins.
        assert_eq!(
            handoff_socket_path_from(Some(PathBuf::from("herdr.sock")), fallback, 7),
            fallback.join("herdr-handoff-7.sock")
        );
    }

    #[test]
    fn handoff_socket_path_uses_the_explicit_environment_override() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "herdr-handoff-socket-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let managed_dir = root.join("runtime").join("managed");
        std::fs::create_dir_all(&managed_dir).unwrap();

        let previous_api = std::env::var_os(crate::api::SOCKET_PATH_ENV_VAR);
        let previous_config_home = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var(
            crate::api::SOCKET_PATH_ENV_VAR,
            managed_dir.join("herdr.sock"),
        );
        std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
        let pid = std::process::id();

        // P2: the handoff control socket lives beside the node-local managed
        // API socket instead of the (possibly shared) session data dir.
        assert_eq!(
            handoff_socket_path(),
            managed_dir.join(format!("herdr-handoff-{pid}.sock"))
        );

        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
        assert_eq!(
            handoff_socket_path(),
            crate::session::data_dir().join(format!("herdr-handoff-{pid}.sock"))
        );

        match previous_api {
            Some(value) => std::env::set_var(crate::api::SOCKET_PATH_ENV_VAR, value),
            None => std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR),
        }
        match previous_config_home {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    fn unique_handoff_socket(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "herdr-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ))
    }

    #[test]
    fn receive_accepts_a_manifest_without_build_identity_gates() {
        let socket_path = unique_handoff_socket("handoff-receive-ok").with_extension("sock");
        let listener = UnixListener::bind(&socket_path).expect("bind handoff listener");
        let token = "receive-token";
        let manifest = manifest_for(empty_snapshot(), Vec::new(), &handoff_params(), None, None);

        let server_socket = socket_path.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept handoff connection");
            let token_line = read_line_unbuffered(&mut stream).expect("read token");
            assert_eq!(token_line.trim_end(), token);
            serde_json::to_writer(&mut stream, &manifest).expect("write manifest");
            stream.write_all(b"\n").expect("terminate manifest");
            stream.flush().expect("flush manifest");
            let validated = read_line_unbuffered(&mut stream).expect("read validated");
            let _ = std::fs::remove_file(&server_socket);
            validated.trim_end() == "validated"
        });

        let received = receive(&socket_path, token).expect("an ungated manifest must be accepted");
        assert!(
            server.join().expect("server thread"),
            "import must ack validation"
        );
        assert!(received.fds.is_empty());
        assert_eq!(received.manifest.version, HANDOFF_VERSION);
    }

    #[test]
    fn receive_rejects_a_manifest_whose_expected_build_identity_does_not_match() {
        let socket_path = unique_handoff_socket("handoff-receive-mismatch").with_extension("sock");
        let listener = UnixListener::bind(&socket_path).expect("bind handoff listener");
        let token = "receive-token";
        // A value this binary can never prove: the gate must fail closed both
        // for a different known identity and for an unknown local identity.
        let unmatched = format!(
            "not-this-build-{}",
            crate::build_info::build_id().unwrap_or("unknown")
        );
        let params = crate::api::schema::ServerLiveHandoffParams {
            expected_build_id: Some(unmatched),
            ..Default::default()
        };
        let manifest = manifest_for(empty_snapshot(), Vec::new(), &params, None, None);

        let server_socket = socket_path.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept handoff connection");
            let _token_line = read_line_unbuffered(&mut stream).expect("read token");
            serde_json::to_writer(&mut stream, &manifest).expect("write manifest");
            stream.write_all(b"\n").expect("terminate manifest");
            stream.flush().expect("flush manifest");
            // The import must close the stream without acknowledging validation.
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .expect("set read timeout");
            let validated = read_line_unbuffered(&mut stream);
            let _ = std::fs::remove_file(&server_socket);
            validated.is_err()
        });

        let error = match receive(&socket_path, token) {
            Ok(_) => panic!("a mismatched expected build identity must be rejected"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("handoff expected build id"),
            "rejection must name the failed gate: {error}"
        );
        assert!(
            server.join().expect("server thread"),
            "the import must not acknowledge an invalid manifest"
        );
    }
}
