//! EnvFilter construction with blocklist parity, plus the reload handle used
//! to swap the default level at runtime.

use std::sync::Arc;

use anyhow::Result;
use compact_str::CompactString;
use tracing_log::AsLog as _;
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

/// One preserved `RUST_LOG` module directive (`target=level`), parsed once
/// into structured form.
///
/// `target` is everything before the first `=`, kept verbatim so span or
/// field directives (`app[span]=info`) and targets EnvFilter will reject
/// round-trip unchanged. `level` holds the parsed level; when the level text
/// is not a valid EnvFilter level, `raw_level` keeps it verbatim so
/// composing reproduces EnvFilter's parse failure exactly (warn +
/// blocklist-only fallback in [`build_filter_from_directives`]) instead of
/// silently dropping or reinterpreting the directive.
pub(crate) struct ModuleDirective {
    target: CompactString,
    level: log::LevelFilter,
    raw_level: Option<CompactString>,
}

impl ModuleDirective {
    /// Parses one trimmed `RUST_LOG` token; bare-level tokens (no `=`) yield
    /// `None` — they are consumed separately as the default level.
    fn parse(token: &str) -> Option<Self> {
        let (target, level_text) = token.split_once('=')?;
        let (level, raw_level) = match parse_env_level(level_text) {
            Some(level) => (level, None),
            // Unparseable levels are carried raw; see the struct docs.
            None => (
                log::LevelFilter::Trace,
                Some(CompactString::from(level_text)),
            ),
        };
        Some(Self {
            target: CompactString::from(target),
            level,
            raw_level,
        })
    }

    /// Whether `target` equals `prefix` or sits directly under it
    /// (`prefix::...`); EnvFilter matches targets by plain prefix, so only a
    /// `::` boundary counts as "under".
    fn under_prefix(&self, prefix: &str) -> bool {
        match self.target.strip_prefix(prefix) {
            Some(rest) => rest.is_empty() || rest.starts_with("::"),
            None => false,
        }
    }

    /// Appends the directive as `target=level`, re-emitting the raw level
    /// text for unparseable levels.
    fn push_into(&self, directives: &mut String) {
        directives.push_str(&self.target);
        directives.push('=');
        match &self.raw_level {
            Some(raw) => directives.push_str(raw),
            None => directives.push_str(level_name(self.level)),
        }
    }
}

/// Parses a directive level exactly as EnvFilter does: names
/// case-insensitively, numerics 0-5, and an empty level meaning TRACE
/// (`target=` enables every level for the target). Uses tracing's own
/// `LevelFilter` parser — `log`'s rejects the numeric spellings EnvFilter
/// accepts.
fn parse_env_level(text: &str) -> Option<log::LevelFilter> {
    if text.is_empty() {
        return Some(log::LevelFilter::Trace);
    }
    Some(
        text.parse::<tracing_subscriber::filter::LevelFilter>()
            .ok()?
            .as_log(),
    )
}

/// Drops preserved directives that would fight the blocklist: EnvFilter
/// resolves conflicts by target specificity, so a user `wry::event_loop=info`
/// would override `wry=off`, and a user `tauri_plugin_mihomo=trace` would be
/// replaced by (or replace) the carve-out. The legacy pipeline ignored module
/// directives entirely in these domains, so dropping them preserves parity:
/// the blocklist is absolute and the carve-out follows the default level.
fn sanitize_preserved<'a>(
    preserved: &'a [ModuleDirective],
) -> impl Iterator<Item = &'a ModuleDirective> + 'a {
    preserved.iter().filter(|directive| {
        if directive.under_prefix(UNBLOCKED_MODULE) {
            return false;
        }
        !BLOCKED_MODULES
            .iter()
            .any(|blocked| directive.under_prefix(blocked))
    })
}

/// Composes the full directive string: sanitized `RUST_LOG` module
/// directives, the default level, and the blocklist.
///
/// The structured directives are stringified here, at the single
/// `EnvFilter::try_new` boundary. tracing-subscriber 0.3.23 exposes no
/// structural constructor for target directives (`Directive::target` is
/// `pub(crate)`; the public surface is `FromStr` and the target-less
/// `From<LevelFilter>`), so `.add_directive()` would parse strings anyway —
/// one per directive, with the same duplicate-target replacement — while
/// splitting the parse across two code paths. A single `try_new` call on one
/// composed string keeps exactly one boundary and identical error semantics.
fn compose_directives(default_level: log::LevelFilter, preserved: &[ModuleDirective]) -> String {
    // EnvFilter::try_new rejects whitespace after commas.
    let mut directives = String::with_capacity(96);
    for directive in sanitize_preserved(preserved) {
        directive.push_into(&mut directives);
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
    preserved: &[ModuleDirective],
) -> Result<EnvFilter, ParseError> {
    EnvFilter::try_new(compose_directives(default_level, preserved))
}

/// Builds the filter from already-parsed `RUST_LOG` directives. Invalid
/// directives are dropped with a warning instead of failing startup,
/// mirroring the legacy silent fallback to the configured level.
pub(crate) fn build_filter_from_directives(
    default_level: log::LevelFilter,
    preserved: &[ModuleDirective],
) -> EnvFilter {
    match try_build_filter(default_level, preserved) {
        Ok(filter) => filter,
        Err(error) => {
            eprintln!("tracing-estuary: ignoring invalid RUST_LOG directives: {error}");
            // The blocklist-only directive string cannot fail to parse.
            try_build_filter(default_level, &[])
                .unwrap_or_else(|_| EnvFilter::new(level_name(default_level)))
        }
    }
}

/// Test-fixture wrapper keeping the historical `build_filter(level, &str)`
/// call shape: parses a `RUST_LOG` fragment and delegates to
/// [`build_filter_from_directives`]. Production paths parse once and share
/// the structured list, so this never runs outside unit tests.
#[cfg(test)]
pub(crate) fn build_filter(default_level: log::LevelFilter, preserved: &str) -> EnvFilter {
    build_filter_from_directives(default_level, &preserved_module_directives(Some(preserved)))
}

/// Keeps only the module directives (`target=level`) of a `RUST_LOG` value,
/// parsed once into structured form. A bare level token is consumed
/// separately as the default level so it does not shadow the configured one.
pub(crate) fn preserved_module_directives(rust_log: Option<&str>) -> Vec<ModuleDirective> {
    let Some(raw) = rust_log else {
        return Vec::new();
    };
    raw.split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty() && token.contains('='))
        .filter_map(ModuleDirective::parse)
        .collect()
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
    preserved: Arc<[ModuleDirective]>,
}

impl FilterHandle {
    pub(crate) const fn new(
        reload: reload::Handle<EnvFilter, Registry>,
        preserved: Arc<[ModuleDirective]>,
    ) -> Self {
        Self { reload, preserved }
    }

    /// Replaces the default (and `tauri_plugin_mihomo` follower) level.
    /// Module directives sourced from `RUST_LOG` survive the swap.
    pub fn set_default_level(&self, level: log::LevelFilter) -> Result<()> {
        let filter = build_filter_from_directives(level, &self.preserved);
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
        let structured: Vec<(&str, log::LevelFilter)> = preserved
            .iter()
            .map(|directive| (directive.target.as_str(), directive.level))
            .collect();
        assert_eq!(
            structured,
            vec![
                ("app", log::LevelFilter::Trace),
                ("tauri", log::LevelFilter::Info),
            ]
        );
        assert!(preserved_module_directives(Some("warn")).is_empty());
        assert!(preserved_module_directives(None).is_empty());
    }

    #[test]
    fn blocklist_wins_over_user_directives_for_blocked_targets() {
        // tauri=info from RUST_LOG must not resurrect the blocked module.
        let filter = build_filter_from_directives(
            log::LevelFilter::Info,
            &preserved_module_directives(Some("tauri=info")),
        );
        let directives = format!("{filter:?}");
        assert!(
            directives.contains("tauri=off") || !directives.contains("tauri=info"),
            "unexpected directives: {directives}"
        );
    }

    #[test]
    fn sanitize_drops_blocked_and_carved_out_targets_only() {
        let preserved = preserved_module_directives(Some(
            "wry=info,wryx=trace,wry::event_loop=info,\
             tauri_plugin_mihomo=trace,tauri_plugin_mihomo::client=debug,app=debug",
        ));
        let kept: Vec<&str> = sanitize_preserved(&preserved)
            .map(|directive| directive.target.as_str())
            .collect();
        // Exact and `::`-nested targets are dropped; `wryx` survives.
        assert_eq!(kept, ["wryx", "app"]);
    }

    #[test]
    fn structured_directives_keep_verbatim_semantics() {
        // The pre-restructure composer passed each trimmed token through
        // verbatim; the structured round trip must agree with that both on
        // which spellings EnvFilter accepts and on the resulting filter.
        let legacy_compose = |token: &str| {
            format!(
                "{token},{},wry=off,tokio_tungstenite=off,tungstenite=off,tauri=off,\
                 tauri_plugin_mihomo={}",
                level_name(log::LevelFilter::Info),
                level_name(log::LevelFilter::Info),
            )
        };
        let cases = [
            ("app=trace", true),
            ("APP=Trace", true),     // level spelling canonicalized
            ("app=", true),          // levelless directive means trace
            ("app=5", true),         // numeric level spelling
            ("app=+3", true),        // usize::from_str accepts a leading '+'
            ("app=05", true),        // ...and leading zeros
            ("app[foo]=info", true), // span directive re-emitted verbatim
            ("app=veryverbose", false),
            ("a=b=c", false),
            ("=info", false),
        ];
        for (token, accepted) in cases {
            let legacy = EnvFilter::try_new(legacy_compose(token));
            let structured = try_build_filter(
                log::LevelFilter::Info,
                &preserved_module_directives(Some(token)),
            );
            assert_eq!(legacy.is_ok(), accepted, "legacy acceptance for {token:?}");
            assert_eq!(
                structured.is_ok(),
                accepted,
                "structured acceptance for {token:?}"
            );
            if let (Ok(legacy), Ok(structured)) = (legacy, structured) {
                assert_eq!(
                    format!("{legacy:?}"),
                    format!("{structured:?}"),
                    "divergence for {token:?}"
                );
            }
        }
    }
}
