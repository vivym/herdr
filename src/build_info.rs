//! Build identity helpers.
//!
//! `BASE_VERSION`/`version()` come from the upstream Herdr package version and
//! stay unchanged. The Jacquard Term product bundle additionally records its
//! own source/build identity at compile time (V-07a):
//!
//! - `HERDR_PRODUCT_REVISION` — monotonic product bundle revision (for example
//!   `jq.7`); this is *not* part of the visible `version()` string.
//! - `HERDR_FORK_COMMIT` — full commit of the Herdr fork the binary was built
//!   from (`HERDR_BUILD_COMMIT` is accepted as a fallback for existing
//!   release tooling).
//! - `HERDR_BUILD_ID` — release build identifier (already used upstream).
//!
//! All values are optional: a developer build reports no identity rather than a
//! fabricated one, and consumers must treat "unknown" as distinct from "matches".

use serde::Serialize;

pub const BASE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Machine-readable build identity.
///
/// Field values are `None` when the corresponding compile-time variable was
/// unset or empty; serialization omits them so readers can tell "unknown" apart
/// from a known value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct BuildIdentity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_revision: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork_commit: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_id: Option<&'static str>,
}

impl BuildIdentity {
    /// The identity this binary was compiled with.
    pub fn current() -> Self {
        Self {
            product_revision: product_revision(),
            fork_commit: fork_commit(),
            build_id: build_id(),
        }
    }

    /// Whether at least one identity value is known for this build.
    pub fn is_known(&self) -> bool {
        self.product_revision.is_some() || self.fork_commit.is_some() || self.build_id.is_some()
    }
}

/// The identity this binary was compiled with.
pub fn identity() -> BuildIdentity {
    BuildIdentity::current()
}

pub fn channel() -> &'static str {
    non_empty(option_env!("HERDR_BUILD_CHANNEL")).unwrap_or("stable")
}

/// Compile-time release build identifier, when the builder set one.
pub fn build_id() -> Option<&'static str> {
    non_empty(option_env!("HERDR_BUILD_ID"))
}

/// Compile-time product bundle revision, when the builder set one.
pub fn product_revision() -> Option<&'static str> {
    non_empty(option_env!("HERDR_PRODUCT_REVISION"))
}

/// Compile-time Herdr fork commit, when the builder set one.
///
/// `HERDR_FORK_COMMIT` is the V-07a variable; `HERDR_BUILD_COMMIT` is accepted
/// as a fallback so existing fork release tooling keeps working.
pub fn fork_commit() -> Option<&'static str> {
    non_empty(option_env!("HERDR_FORK_COMMIT"))
        .or_else(|| non_empty(option_env!("HERDR_BUILD_COMMIT")))
}

pub fn version() -> String {
    match channel() {
        "stable" => BASE_VERSION.to_string(),
        channel => match build_id() {
            Some(build_id) => format!("{BASE_VERSION}-{channel}.{build_id}"),
            None => format!("{BASE_VERSION}-{channel}"),
        },
    }
}

pub fn is_preview() -> bool {
    channel() == "preview"
}

fn non_empty(value: Option<&'static str>) -> Option<&'static str> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_version_defaults_to_cargo_version() {
        assert!(!super::version().is_empty());
    }

    #[test]
    fn build_identity_json_reports_exactly_the_compile_time_values() {
        let identity = super::identity();
        let value = serde_json::to_value(identity).expect("identity should serialize");

        for (field, expected) in [
            ("product_revision", super::product_revision()),
            ("fork_commit", super::fork_commit()),
            ("build_id", super::build_id()),
        ] {
            assert_eq!(
                value.get(field).and_then(serde_json::Value::as_str),
                expected,
                "identity field {field} must mirror its accessor"
            );
        }

        if !identity.is_known() {
            assert_eq!(
                value,
                serde_json::json!({}),
                "an unknown identity must serialize without fabricated values"
            );
        }
    }

    #[test]
    fn build_identity_is_known_iff_one_field_is_present() {
        let identity = super::identity();
        assert_eq!(
            identity.is_known(),
            super::product_revision().is_some()
                || super::fork_commit().is_some()
                || super::build_id().is_some()
        );
    }

    #[test]
    fn build_identity_does_not_change_the_visible_version() {
        // V-07a: product revision/build identity is a side channel, never part
        // of the visible version string golden-tested by release tooling.
        assert_eq!(super::version(), {
            let channel = super::channel();
            if channel == "stable" {
                BASE_VERSION.to_string()
            } else {
                match super::build_id() {
                    Some(build_id) => format!("{BASE_VERSION}-{channel}.{build_id}"),
                    None => format!("{BASE_VERSION}-{channel}"),
                }
            }
        });
    }
}
