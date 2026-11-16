//! Jacquard Term product identity for the `jacquard-product` build.
//!
//! The Jacquard desktop client bundles a patched Herdr fork as its terminal
//! workspace manager (`jacquard-term`, A1 D1–D11). The fork deliberately keeps
//! the upstream Cargo package/crate names so it stays rebasable onto Herdr
//! tags (A1 plan §4.2); product-visible identity and the network-update
//! posture change only when the `jacquard-product` feature is enabled.
//!
//! Product mode (A1 plan §6.1/§6.5, D8/D9):
//!
//! - product name / app dir / default session become `jacquard-term` /
//!   `jacquard-term` / `jacquard`;
//! - network self-update, background release checks, remote auto-install and
//!   external agent-detection manifest updates are disabled; the version is
//!   owned by the Jacquard release bundle;
//! - the plugin marketplace and plugin API stay enabled (D8);
//! - internal protocol environment variables stay `HERDR_*` (A1 plan §4.1).
//!
//! Upstream/default builds keep the Herdr identity and update behavior
//! unchanged, so the upstream test suite keeps asserting `herdr`.

/// True only in `jacquard-product` builds.
pub const IS_JACQUARD_PRODUCT: bool = cfg!(feature = "jacquard-product");

/// User-visible product/command name used in help and version output.
#[cfg(feature = "jacquard-product")]
pub const NAME: &str = "jacquard-term";
/// User-visible product/command name used in help and version output.
#[cfg(not(feature = "jacquard-product"))]
pub const NAME: &str = "herdr";

/// Product tagline printed on the root help screen.
#[cfg(feature = "jacquard-product")]
pub const TAGLINE: &str = "terminal workspace manager for AI coding agents (Jacquard Term)";
/// Product tagline printed on the root help screen.
#[cfg(not(feature = "jacquard-product"))]
pub const TAGLINE: &str = "terminal workspace manager for AI coding agents";

/// Default persistent session name (A1 plan §4.1: product session `jacquard`).
#[cfg(feature = "jacquard-product")]
pub const DEFAULT_SESSION_NAME: &str = "jacquard";
/// Default persistent session name (upstream: `default`).
#[cfg(not(feature = "jacquard-product"))]
pub const DEFAULT_SESSION_NAME: &str = "default";

/// Message used for every rejected update/install path in product builds.
#[cfg(feature = "jacquard-product")]
pub const SELF_UPDATE_DISABLED_MESSAGE: &str =
    "self-update is disabled in jacquard-term; update through the Jacquard desktop client release bundle";
/// Message kept for upstream builds; they never report it from the product gate.
#[cfg(not(feature = "jacquard-product"))]
pub const SELF_UPDATE_DISABLED_MESSAGE: &str =
    "self-update is disabled in this build; update through your installation method";

/// Platform app directory name: `~/.config/<name>` and `~/.local/state/<name>`.
///
/// Product builds use the isolated `jacquard-term` directory so the bundled
/// binary never reads or writes a user's Herdr config/state. Upstream debug
/// builds keep `herdr-dev` and upstream release builds keep `herdr`.
pub fn app_dir_name() -> &'static str {
    if IS_JACQUARD_PRODUCT {
        "jacquard-term"
    } else if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    }
}

/// Whether network update features (background release checks, `update`
/// command, update channel management) may run.
pub const NETWORK_UPDATES_ENABLED: bool = !IS_JACQUARD_PRODUCT;

/// Whether external agent-detection manifest updates may run; product builds
/// use the manifests bundled into the binary only (D9).
pub const EXTERNAL_MANIFEST_UPDATES_ENABLED: bool = !IS_JACQUARD_PRODUCT;

/// Whether attaching to a remote host may auto-install/update a remote binary.
/// Product builds require an explicit remote binary (A1 Phase 3) and never
/// download or copy one implicitly.
pub const REMOTE_AUTO_INSTALL_ENABLED: bool = !IS_JACQUARD_PRODUCT;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_identity_matches_the_build_feature() {
        let product = cfg!(feature = "jacquard-product");
        assert_eq!(NAME, if product { "jacquard-term" } else { "herdr" });
        assert_eq!(
            DEFAULT_SESSION_NAME,
            if product { "jacquard" } else { "default" }
        );
        assert_eq!(
            crate::config::app_dir_name(),
            if product {
                "jacquard-term"
            } else if cfg!(debug_assertions) {
                "herdr-dev"
            } else {
                "herdr"
            }
        );
        assert_eq!(NETWORK_UPDATES_ENABLED, !product);
        assert_eq!(EXTERNAL_MANIFEST_UPDATES_ENABLED, !product);
        assert_eq!(REMOTE_AUTO_INSTALL_ENABLED, !product);
        assert_eq!(
            TAGLINE,
            if product {
                "terminal workspace manager for AI coding agents (Jacquard Term)"
            } else {
                "terminal workspace manager for AI coding agents"
            }
        );
        assert!(SELF_UPDATE_DISABLED_MESSAGE.starts_with("self-update is disabled"));
    }

    #[test]
    fn default_session_name_is_shared_with_the_session_module() {
        assert_eq!(crate::session::DEFAULT_SESSION_NAME, DEFAULT_SESSION_NAME);
    }
}
