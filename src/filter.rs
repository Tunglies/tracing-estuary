//! EnvFilter construction with blocklist parity, plus the reload handle used
//! to swap the default level at runtime.

use std::sync::Arc;

use anyhow::Result;
use tracing_subscriber::{
    Registry,
    filter::{EnvFilter, ParseError},
    reload,
};

/// Modules silenced by the layered pipeline. Mirrors the legacy flexi
/// `ModuleFilter` blocklist: `wry`, `tokio_tungstenite`, `tungstenite` and
/// `tauri` are extremely chatty and drown out the app's own logs.
const BLOCKED_MODULES: &[&str] = &["wry", "tokio_tungstenite", "tungstenite", "tauri"];

/// Carve-out from the blocked `tauri` prefix: EnvFilter target matching is a
/// plain `starts_with`, so `tauri=off` would also swallow `tauri_plugin_mihomo`.
/// A more specific directive wins, exactly like the legacy exclude list.
const UNBLOCKED_MODULE: &str = "tauri_plugin_mihomo";

const fn level_name(level: log::LevelFilter) -> &'static str {
    match level {
        log::LevelFilter::Off => "off",
        log::LevelFilter::Error => "error",
        log::LevelFilter::Warn => "warn",
        log::LevelFilter::Info => "info",
        log::LevelFilter::Debug => "debug",
        log::LevelFilter::Trace => "trace",
    }
}

/// Drops preserved `RUST_LOG` directives that would fight the blocklist:
/// EnvFilter resolves conflicts by target specificity, so a user
/// `wry::event_loop=info` would override `wry=off`, and a user
/// `tauri_plugin_mihomo=trace` would be replaced by (or replace) the
/// carve-out. The legacy pipeline ignored module directives entirely in
/// these domains, so dropping them preserves parity: the blocklist is
/// absolute and the carve-out follows the default level.
fn sanitize_preserved(preserved: &str) -> String {
    preserved
        .split(',')
        .filter(|token| {
            let target = token.split('=').next().unwrap_or_default();
            if target == UNBLOCKED_MODULE
                || target.starts_with(&(UNBLOCKED_MODULE.to_owned() + "::"))
            {
                return false;
            }
            !BLOCKED_MODULES
                .iter()
                .any(|blocked| target == *blocked || target.starts_with(&format!("{blocked}::")))
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Composes the full directive string: sanitized `RUST_LOG` module
/// directives, the default level, and the blocklist.
fn compose_directives(default_level: log::LevelFilter, preserved: &str) -> String {
    // EnvFilter::try_new rejects whitespace after commas.
    let preserved = sanitize_preserved(preserved);
    let mut directives = String::with_capacity(96);
    if !preserved.is_empty() {
        directives.push_str(&preserved);
        directives.push(',');
    }
    directives.push_str(level_name(default_level));
    for module in BLOCKED_MODULES {
        directives.push(',');
        directives.push_str(module);
        directives.push_str("=off");
    }
    directives.push(',');
    directives.push_str(UNBLOCKED_MODULE);
    directives.push('=');
    directives.push_str(level_name(default_level));
    directives
}

fn try_build_filter(
    default_level: log::LevelFilter,
    preserved: &str,
) -> Result<EnvFilter, ParseError> {
    let directives = compose_directives(default_level, preserved);
    EnvFilter::try_new(&directives)
}

/// Builds the initial filter. Invalid `RUST_LOG` directives are dropped with
/// a warning instead of failing startup, mirroring the legacy silent
/// fallback to the configured level.
pub(crate) fn build_filter(default_level: log::LevelFilter, preserved: &str) -> EnvFilter {
    match try_build_filter(default_level, preserved) {
        Ok(filter) => filter,
        Err(error) => {
            eprintln!("tracing-estuary: ignoring invalid RUST_LOG directives: {error}");
            // The blocklist-only directive string cannot fail to parse.
            try_build_filter(default_level, "")
                .unwrap_or_else(|_| EnvFilter::new(level_name(default_level)))
        }
    }
}

/// Keeps only the module directives (`target=level`) of a `RUST_LOG` value.
/// A bare level token is consumed separately as the default level so it does
/// not shadow the configured one.
pub(crate) fn preserved_module_directives(rust_log: Option<&str>) -> String {
    let Some(raw) = rust_log else {
        return String::new();
    };
    raw.split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty() && token.contains('='))
        .collect::<Vec<_>>()
        .join(",")
}

/// A bare `RUST_LOG=level` value, for parity with the legacy init which
/// parsed the whole variable as a plain `LevelFilter`.
pub(crate) fn bare_env_level(rust_log: Option<&str>) -> Option<log::LevelFilter> {
    let raw = rust_log?.trim();
    if raw.contains(',') || raw.contains('=') || raw.is_empty() {
        return None;
    }
    raw.parse().ok()
}

/// Handle for swapping the default level while keeping preserved `RUST_LOG`
/// module directives and the blocklist intact.
#[derive(Clone)]
pub struct FilterHandle {
    reload: reload::Handle<EnvFilter, Registry>,
    preserved: Arc<str>,
}

impl FilterHandle {
    pub(crate) const fn new(
        reload: reload::Handle<EnvFilter, Registry>,
        preserved: Arc<str>,
    ) -> Self {
        Self { reload, preserved }
    }

    /// Replaces the default (and `tauri_plugin_mihomo` follower) level.
    /// Module directives sourced from `RUST_LOG` survive the swap.
    pub fn set_default_level(&self, level: log::LevelFilter) -> Result<()> {
        let filter = build_filter(level, &self.preserved);
        self.reload
            .reload(filter)
            .map_err(|error| anyhow::anyhow!("failed to reload tracing filter: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_level_is_extracted_only_without_directives() {
        assert_eq!(bare_env_level(Some("trace")), Some(log::LevelFilter::Trace));
        assert_eq!(
            bare_env_level(Some(" TRACE ")),
            Some(log::LevelFilter::Trace)
        );
        assert_eq!(bare_env_level(None), None);
        assert_eq!(bare_env_level(Some("app=trace,tauri=info")), None);
        assert_eq!(bare_env_level(Some("trace,debug")), None);
        assert_eq!(bare_env_level(Some("")), None);
    }

    #[test]
    fn module_directives_preserve_order_and_drop_bare_tokens() {
        // " debug " is a bare level token, consumed as the default level.
        let preserved = preserved_module_directives(Some("app=trace, debug ,tauri=info"));
        assert_eq!(preserved, "app=trace,tauri=info");
        assert_eq!(preserved_module_directives(Some("warn")), "");
        assert_eq!(preserved_module_directives(None), "");
    }

    #[test]
    fn blocklist_wins_over_user_directives_for_blocked_targets() {
        // tauri=info from RUST_LOG must not resurrect the blocked module.
        let filter = build_filter(
            log::LevelFilter::Info,
            &preserved_module_directives(Some("tauri=info")),
        );
        let directives = format!("{filter:?}");
        assert!(
            directives.contains("tauri=off") || !directives.contains("tauri=info"),
            "unexpected directives: {directives}"
        );
    }
}
