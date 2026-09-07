//! In-crate unit tests. Global-subscriber behavior (LogTracer) lives in
//! `tests/log_bridge.rs` because the global default can only be set once
//! per process.
#![allow(clippy::panic)] // assertion helpers in tests diverge via panic

use std::path::Path;
use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;

use flexi_logger::{
    Cleanup, Criterion, Naming,
    writers::{FileLogWriter, FileLogWriterBuilder},
};
use parking_lot::Mutex;
use tracing::{Event, Level, Subscriber};
use tracing_log::AsLog as _;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Registry, reload};

use crate::filter;
use crate::sink;

#[derive(Clone, Default)]
struct Recording(Arc<Mutex<Vec<(String, Level)>>>);

impl Recording {
    fn count_target(&self, target: &str) -> usize {
        self.0.lock().iter().filter(|(t, _)| t == target).count()
    }

    fn total(&self) -> usize {
        self.0.lock().len()
    }
}

impl<S> Layer<S> for Recording
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        self.0.lock().push((
            event.metadata().target().to_owned(),
            *event.metadata().level(),
        ));
    }
}

/// Emits through the real production bridge path (`log::Record` ->
/// `tracing_log`), which also allows arbitrary dynamic targets; tracing
/// macros require literal targets.
fn emit(dispatch: &tracing::Dispatch, target: &str, level: Level) {
    let record = log::Record::builder()
        .args(format_args!("msg"))
        .level(level.as_log())
        .target(target)
        .module_path(Some(target))
        .build();
    tracing::dispatcher::with_default(dispatch, || {
        let _ = tracing_log::format_trace(&record);
    });
}

fn level_from_name(name: &str) -> Level {
    match name {
        "error" => Level::ERROR,
        "warn" => Level::WARN,
        "info" => Level::INFO,
        "debug" => Level::DEBUG,
        _ => Level::TRACE,
    }
}

/// Inline copy of the legacy flexi `ModuleFilter` this crate's EnvFilter
/// directives must stay behaviorally equivalent to (prefix blocklist with an
/// exclude carve-out; ran after the log spec in the old pipeline).
struct LegacyModuleFilter {
    block: &'static [&'static str],
    exclude: &'static [&'static str],
}

impl LegacyModuleFilter {
    fn filter(&self, record: &log::Record<'_>) -> bool {
        let Some(module) = record.module_path() else {
            return true;
        };
        if self.exclude.iter().any(|e| module.starts_with(e)) {
            return true;
        }
        !self.block.iter().any(|b| module.starts_with(b))
    }
}

/// The legacy pipeline's decision = spec-level gate (default level, no
/// module overrides) AND `ModuleFilter`. The new pipeline must agree.
fn legacy_decision(module: &str, level: &str) -> bool {
    let Ok(level) = log::Level::from_str(level) else {
        panic!("bad level name {level:?}");
    };
    let spec_allows = level.to_level_filter() <= log::LevelFilter::Info;
    let record = log::Record::builder()
        .args(format_args!("msg"))
        .level(level)
        .target(module)
        .module_path(Some(module))
        .build();
    let module_filter = LegacyModuleFilter {
        block: &["wry", "tokio_tungstenite", "tungstenite", "tauri"],
        exclude: &["tauri_plugin_mihomo"],
    };
    spec_allows && module_filter.filter(&record)
}

#[test]
fn env_filter_matches_legacy_module_filter() {
    let env_filter = filter::build_filter(log::LevelFilter::Info, "");
    let recording = Recording::default();
    let dispatch =
        tracing::Dispatch::new(Registry::default().with(env_filter).with(recording.clone()));

    let cases: &[(&str, &str)] = &[
        ("tauri", "error"),
        ("tauri", "info"),
        ("tauri::app", "debug"),
        ("tauri_plugin_mihomo", "info"),
        ("tauri_plugin_mihomo::client", "warn"),
        ("wry", "error"),
        ("wry::event_loop", "info"),
        ("tungstenite", "error"),
        ("tokio_tungstenite::x", "info"),
        ("app", "info"),
        ("app_lib::feat", "debug"),
        ("sidecar", "warn"),
        ("some_app::core", "info"),
        ("serde_xml_rs", "error"),
    ];

    for (target, level) in cases {
        let legacy = legacy_decision(target, level);
        let before = recording.total();
        emit(&dispatch, target, level_from_name(level));
        let allowed = recording.total() > before;
        assert_eq!(
            legacy, allowed,
            "filter divergence for target {target:?} at {level}"
        );
    }
}

#[test]
fn log_records_bridge_through_log_target_fields() {
    // Preserved RUST_LOG module directive: app=trace.
    let env_filter = filter::build_filter(log::LevelFilter::Info, "app=trace");
    let recording = Recording::default();
    let dispatch =
        tracing::Dispatch::new(Registry::default().with(env_filter).with(recording.clone()));

    tracing::dispatcher::with_default(&dispatch, || {
        let allowed = log::Record::builder()
            .args(format_args!("hello app"))
            .level(log::Level::Trace)
            .target("app")
            .module_path(Some("app_lib::feat"))
            .file(Some("src/feat.rs"))
            .line(Some(42))
            .build();
        let _ = tracing_log::format_trace(&allowed);
        let blocked = log::Record::builder()
            .args(format_args!("hello wry"))
            .level(log::Level::Error)
            .target("wry::event_loop")
            .module_path(Some("wry::event_loop"))
            .build();
        let _ = tracing_log::format_trace(&blocked);
        // Note: a target like `app_lib::x` would also pass, because EnvFilter
        // directives are prefixes (`app=trace` lifts the whole binary crate);
        // use an unrelated module to test the default-level gate.
        let spec_blocked = log::Record::builder()
            .args(format_args!("too verbose"))
            .level(log::Level::Trace)
            .target("serde_xml_rs")
            .module_path(Some("serde_xml_rs"))
            .build();
        let _ = tracing_log::format_trace(&spec_blocked);
    });

    // Log-bridged events surface with the static metadata target "log".
    assert_eq!(
        recording.count_target("log"),
        1,
        "only the app record passes both filter gates"
    );
}

#[test]
#[allow(clippy::cognitive_complexity)] // sequential reload scenario in one test
fn set_default_level_changes_filter_and_preserves_module_directives() {
    let (filter_layer, reload_handle) =
        reload::Layer::new(filter::build_filter(log::LevelFilter::Info, "app=trace"));
    let filter_handle = crate::FilterHandle::new(reload_handle, Arc::from("app=trace"));
    let recording = Recording::default();
    let dispatch = tracing::Dispatch::new(
        Registry::default()
            .with(filter_layer)
            .with(recording.clone()),
    );

    tracing::dispatcher::with_default(&dispatch, || {
        tracing::trace!(target: "app", "one");
    });
    assert_eq!(recording.count_target("app"), 1);

    if let Err(error) = filter_handle.set_default_level(log::LevelFilter::Error) {
        panic!("set_default_level failed: {error}");
    }

    tracing::dispatcher::with_default(&dispatch, || {
        tracing::error!(target: "app", "two");
        tracing::trace!(target: "app", "three"); // preserved directive still applies
        tracing::warn!(target: "serde_xml_rs", "blocked by new default");
        tracing::error!(target: "serde_xml_rs", "four");
    });

    assert_eq!(
        recording.count_target("app"),
        3,
        "app keeps its trace directive after the level swap"
    );
    assert_eq!(recording.count_target("serde_xml_rs"), 1);
}

fn test_file_builder(
    dir: &Path,
    basename: &str,
    max_bytes: u64,
    keep: usize,
) -> FileLogWriterBuilder {
    FileLogWriter::builder(
        flexi_logger::FileSpec::default()
            .directory(dir.to_owned())
            .basename(basename),
    )
    .format(crate::format::file_format_with_level)
    .write_mode(flexi_logger::WriteMode::BufferAndFlushWith(
        64 * 1024,
        Duration::from_millis(500),
    ))
    .rotate(
        Criterion::Size(max_bytes),
        Naming::TimestampsCustomFormat {
            current_infix: Some("latest"),
            format: "%Y-%m-%d_%H-%M-%S",
        },
        Cleanup::KeepLogFiles(keep),
    )
}

fn log_file_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn read_first_log(dir: &Path) -> String {
    let mut names = log_file_names(dir);
    names.sort();
    match names.first() {
        Some(name) => std::fs::read_to_string(dir.join(name)).unwrap_or_default(),
        None => String::new(),
    }
}

#[test]
fn sink_writes_legacy_line_format_and_flush_drains() {
    let Ok(dir) = tempfile::tempdir() else {
        panic!("tempdir failed");
    };
    let (layer, sink) = match sink::spawn_sink(test_file_builder(dir.path(), "fmt", 128 * 1024, 8))
    {
        Ok(parts) => parts,
        Err(error) => panic!("spawn failed: {error}"),
    };
    let env_filter = filter::build_filter(log::LevelFilter::Info, "");
    let dispatch = tracing::Dispatch::new(Registry::default().with(env_filter).with(layer));

    tracing::dispatcher::with_default(&dispatch, || {
        tracing::info!(target: "app", "[Core] Restarting core");
        tracing::trace!(target: "app", "must be filtered out");
        tracing::error!(target: "tauri", "blocked module");
    });

    assert!(sink.flush(), "sink should drain within timeout");
    assert_eq!(sink.dropped_records(), 0);

    let content = read_first_log(dir.path());
    let line = content.lines().next().unwrap_or_default();
    assert!(
        line.contains("] INFO [Core] Restarting core"),
        "unexpected line: {line:?} (content {content:?})"
    );
    // `[YYYY-MM-DD HH:MM:SS.mmm` prefix, byte-compatible with the legacy file.
    let Some(timestamp) = line.strip_prefix('[').map(|rest| &rest[..23]) else {
        panic!("missing legacy timestamp prefix in {line:?}");
    };
    let bytes = timestamp.as_bytes();
    assert_eq!(bytes[4], b'-');
    assert_eq!(bytes[7], b'-');
    assert_eq!(bytes[10], b' ');
    assert_eq!(bytes[13], b':');
    assert_eq!(bytes[16], b':');
    assert_eq!(bytes[19], b'.');
    assert!(!content.contains("must be filtered out"));
    assert!(!content.contains("blocked module"));
}

#[test]
fn sink_rotates_by_size_and_keeps_capped_files() {
    let Ok(dir) = tempfile::tempdir() else {
        panic!("tempdir failed");
    };
    // 200-byte cap, keep 2 rotated files.
    let (layer, sink) = match sink::spawn_sink(test_file_builder(dir.path(), "rot", 200, 2)) {
        Ok(parts) => parts,
        Err(error) => panic!("spawn failed: {error}"),
    };
    let env_filter = filter::build_filter(log::LevelFilter::Info, "");
    let dispatch = tracing::Dispatch::new(Registry::default().with(env_filter).with(layer));

    tracing::dispatcher::with_default(&dispatch, || {
        for index in 0..40 {
            tracing::info!(target: "app", "[Core] rotation filler entry {index} padding padding");
        }
    });

    assert!(sink.flush());
    assert!(sink.shutdown());

    let names = log_file_names(dir.path());
    // flexi keeps `keep` rotated files plus the current one, and cleanup runs
    // on rotation, so one extra rotated file may transiently remain.
    assert!(
        names.len() <= 4,
        "rotation should cap files (current + kept), got {names:?}"
    );
    assert!(
        names.iter().any(|name| name.contains("latest")),
        "active file should carry the latest infix, got {names:?}"
    );
}

#[cfg(feature = "chrome-trace")]
#[test]
fn chrome_layer_writes_perfetto_json() {
    use crate::chrome::ChromeTraceLayer;

    let Ok(dir) = tempfile::tempdir() else {
        panic!("tempdir failed");
    };
    let path = dir.path().join("trace.json");
    let layer = ChromeTraceLayer::new(path.clone());
    let handle = layer.handle();
    let env_filter = filter::build_filter(log::LevelFilter::Info, "");
    let dispatch = tracing::Dispatch::new(Registry::default().with(env_filter).with(layer));

    tracing::dispatcher::with_default(&dispatch, || {
        let span = tracing::info_span!("restart_core", core = "mihomo");
        let _guard = span.enter();
        tracing::info!(target: "app", "[Core] span body event");
    });

    handle.flush();
    let content = std::fs::read_to_string(path).unwrap_or_default();
    assert!(content.starts_with("{\"traceEvents\":["));
    assert!(
        content.contains("\"ph\":\"X\""),
        "span complete event missing"
    );
    assert!(content.contains("\"ph\":\"i\""), "instant event missing");
    assert!(content.contains("restart_core"));
    assert!(content.contains("span body event"));
    assert!(content.ends_with('}'), "unterminated trace json");
}

#[cfg(feature = "chrome-trace")]
#[test]
fn chrome_layer_records_info_level_span_with_default_filter() {
    use crate::chrome::ChromeTraceLayer;

    let Ok(dir) = tempfile::tempdir() else {
        panic!("tempdir failed");
    };
    let path = dir.path().join("t2.json");
    let layer = ChromeTraceLayer::new(path.clone());
    let handle = layer.handle();
    // Exact filter shape and span shape the app uses at startup.
    let env_filter = filter::build_filter(log::LevelFilter::Info, "");
    let dispatch = tracing::Dispatch::new(Registry::default().with(env_filter).with(layer));

    tracing::dispatcher::with_default(&dispatch, || {
        let span = tracing::info_span!(target: "app_lib::core::manager::config", "update_config_with_force", force = true);
        let _guard = span.enter();
        tracing::info!(target: "app", "inside span");
    });

    handle.flush();
    let content = std::fs::read_to_string(path).unwrap_or_default();
    assert!(
        content.contains("\"name\":\"update_config_with_force\""),
        "info span missing from chrome trace: {content}"
    );
}
