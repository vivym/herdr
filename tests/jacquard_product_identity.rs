//! Jacquard Term product identity integration tests (A1 Phase 1).
//!
//! These assertions only run for `--features jacquard-product` builds. The
//! upstream/default test suite keeps asserting the Herdr identity so the fork
//! stays rebasable onto Herdr tags.

#![cfg(feature = "jacquard-product")]

use std::process::Command;

fn product_binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
    for variable in [
        "HERDR_SOCKET_PATH",
        "HERDR_CLIENT_SOCKET_PATH",
        "HERDR_ENV",
        "HERDR_SESSION",
        "HERDR_PANE_ID",
        "HERDR_TAB_ID",
        "HERDR_BIN_PATH",
        "HERDR_STARTUP_CWD",
        "HERDR_WORKSPACE_ID",
    ] {
        command.env_remove(variable);
    }
    command
}

#[test]
fn version_prints_the_product_name() {
    let output = product_binary().arg("--version").output().unwrap();
    assert!(output.status.success(), "--version failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("jacquard-term "),
        "--version must start with the product name, got: {stdout}"
    );
    assert!(
        stdout.contains("0.9.5"),
        "--version must still report the bundled Herdr version, got: {stdout}"
    );
}

#[test]
fn help_uses_the_product_identity_and_hides_self_update() {
    let output = product_binary().arg("--help").output().unwrap();
    assert!(output.status.success(), "--help failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("jacquard-term —"),
        "help must print the product identity, got: {stdout}"
    );
    assert!(
        stdout.contains("jacquard-term/config.toml"),
        "help must point at the isolated product config dir, got: {stdout}"
    );
    assert!(
        stdout.contains("plugin") && stdout.contains("plugin marketplace"),
        "product help must keep advertising the plugin marketplace (D8), got: {stdout}"
    );
    for forbidden in ["herdr.dev", "herdr update", "channel set"] {
        assert!(
            !stdout.contains(forbidden),
            "product help must not advertise network update surface {forbidden:?}: {stdout}"
        );
    }
}

#[test]
fn self_update_command_fails_closed_with_product_guidance() {
    let output = product_binary().arg("update").output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "`update` must fail closed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("self-update is disabled"),
        "expected the product self-update message, got: {stderr}"
    );
    assert!(
        stderr.contains("jacquard-term") && stderr.contains("Jacquard desktop client"),
        "expected product update guidance, got: {stderr}"
    );
}

#[test]
fn channel_management_fails_closed() {
    let output = product_binary()
        .args(["channel", "set", "stable"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "`channel set` must fail closed; stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("channel management is disabled"),
        "expected the product channel message, got: {stderr}"
    );
}

#[test]
fn default_config_points_at_the_product_app_dir() {
    let output = product_binary().arg("--default-config").output().unwrap();
    assert!(output.status.success(), "--default-config failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("~/.config/jacquard-term/config.toml"),
        "default config must document the product app dir, got: {stdout}"
    );
    assert!(
        !stdout.contains("~/.config/herdr/config.toml"),
        "default config must not document the upstream app dir, got: {stdout}"
    );
}

#[test]
fn plugin_marketplace_commands_remain_available() {
    let output = product_binary().args(["plugin", "help"]).output().unwrap();
    assert!(
        output.status.success(),
        "`plugin help` must stay enabled in the product build; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let help = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        help.contains("plugin") && help.contains("plugin install"),
        "plugin marketplace help must stay available, got: {help}"
    );
}
