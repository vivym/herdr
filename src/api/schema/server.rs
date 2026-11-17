use serde::{Deserialize, Serialize};

/// Handoff-capable baseline implemented by this build (V-07a, additive).
///
/// Baseline 1 means the source server can drive a pane-preserving handoff with
/// the product-managed environment (P1), a node-local handoff control socket
/// (P2), and expected build identity gates written into the manifest by the
/// source and validated by the import server (P3). Absent on pre-V-07a
/// servers, which callers must treat as restart-only.
pub const HANDOFF_BASELINE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct PingParams {}

/// Build identity advertised by a running server (V-07a).
///
/// Every field is optional: a developer build without compile-time identity
/// reports an empty object rather than fabricated values. Field names match
/// the `--version --json` side channel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BuildIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServerLiveHandoffParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_exe: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_protocol: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<String>,
    /// V-07a: expected compile-time build identity of the import binary.
    /// Additive optional fields; older sources silently drop them, and older
    /// imports decode them as absent. The manifest version is not bumped for
    /// these gates (`HANDOFF_VERSION` stays 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_build_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_product_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_fork_commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServerSshAgentRegisterParams {
    /// Absolute remote-host agent socket. Registration lasts until this API connection closes.
    pub socket_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServerCapabilities {
    pub live_handoff: bool,
    #[serde(default)]
    pub detached_server_daemon: bool,
    /// Stable client-owned endpoint generation supported by this server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_protocol_generation: Option<u32>,
    /// Whether this server supports explicit client-shell surface interest.
    #[serde(default)]
    pub surface_interest: bool,
    /// Whether this server supports endpoint health probes.
    #[serde(default)]
    pub health_check: bool,
    /// Supports connection-scoped `server.ssh_agent.register` on the local JSON API.
    #[serde(default)]
    pub ssh_agent_registration: bool,
    /// Managed session name reported by a product managed server.
    ///
    /// A1 M6: `server status/start/stop --managed` compare this with the
    /// explicit `--session` target and refuse to adopt/stop a server whose
    /// identity cannot be verified (unknown/mismatched session => 78).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_session: Option<String>,
    /// Compile-time build identity of the running server (V-07a, additive).
    ///
    /// Absent on pre-V-07a servers, which are restart-only for pane-preserving
    /// upgrades (a pre-V-07a source silently drops the expected-build handoff
    /// gates, so callers cannot claim build-gated handoff).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildIdentity>,
    /// Handoff-capable baseline implemented by this server (V-07a, additive).
    /// `Some(1)` = P1/P2/P3 present; absent on pre-V-07a servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_baseline: Option<u32>,
}
