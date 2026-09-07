//! Layered observability pipeline: where the `log` rivers meet the
//! `tracing` sea.
//!
//! A `tracing` Registry stack for applications migrating from a
//! "flexi_logger as the global `log` implementation" setup:
//!
//! ```text
//! log::Record --tracing-log LogTracer (global log logger)--> tracing Event
//!                                |
//!                    Registry + EnvFilter (reloadable)
//!                                |
//!   +---------------+------------------+------------------+----------------+
//!   | fmt(stdout)   | flexi sink layer | ConsoleLayer     | ChromeLayer
//!   |               | FileLogWriter    | (tokio-console)  | (Perfetto JSON)
//!   |               | size rotation    | feature+runtime  | feature+env
//! ```
//!
//! * Existing `log` calls (including the `logging!` macro) keep working
//!   unchanged via [`tracing_log::LogTracer`].
//! * The file sink keeps flexi_logger's `FileLogWriter` for size-based
//!   rotation, and its line format is byte-identical to the legacy pipeline
//!   because the very same crate-local format function is used.
//! * Module blocklist parity with the legacy `ModuleFilter` is expressed as
//!   `EnvFilter` directives; see [`FilterHandle`].
//!
//! # tauri-plugin-devtools conflict
//!
//! `tauri-plugin-devtools` installs its own global tracing subscriber and
//! panics if one is already set, so the app skips this pipeline entirely
//! while its `tauri-dev` feature is enabled (dev-only mode). The
//! tokio-console layer, in contrast, coexists with file/stdout logging.

use std::sync::Arc;

use anyhow::Result;
use flexi_logger::writers::FileLogWriterBuilder;
use tracing::Dispatch;
use tracing_subscriber::Registry;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::reload;

mod chrome;
mod fields;
mod filter;
#[cfg(not(debug_assertions))]
mod fmt_layer;
mod format;
mod sink;

pub use chrome::{ChromeTraceHandle, ENV_CHROME_TRACE};
pub use filter::FilterHandle;
pub use format::{console_format, file_format_with_level, file_format_without_level};
pub use sink::FlexiSink;

/// Running handles of the installed pipeline.
pub struct Pipeline {
    /// Swaps the default log level (UI level switch) without touching
    /// `RUST_LOG`-sourced module directives.
    pub filter: FilterHandle,
    /// File sink controls; `None` when the pipeline runs without a file.
    pub sink: Option<FlexiSink>,
    /// Chrome trace best-effort flush (no-op unless the layer is active).
    chrome: ChromeTraceHandle,
}

impl Pipeline {
    /// Drains and flushes the file sink, then writes the chrome trace file.
    /// Used from the panic hook and shutdown paths; bounded by the crate's
    /// control timeout.
    pub fn flush_all(&self) {
        if let Some(sink) = &self.sink {
            sink.flush();
        }
        self.chrome.flush();
    }
}

/// Builder for the global pipeline. Synchronous; configuration is supplied
/// by the caller.
pub struct PipelineBuilder {
    default_level: log::LevelFilter,
    file_writer: Option<FileLogWriterBuilder>,
}

impl Default for PipelineBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PipelineBuilder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            default_level: log::LevelFilter::Info,
            file_writer: None,
        }
    }

    /// Default filter level used when `RUST_LOG` carries no bare level.
    #[must_use]
    pub const fn default_level(mut self, level: log::LevelFilter) -> Self {
        self.default_level = level;
        self
    }

    /// Flexi file writer (rotation config included); omit for stdout-only.
    #[must_use]
    pub fn file_writer(mut self, builder: FileLogWriterBuilder) -> Self {
        self.file_writer = Some(builder);
        self
    }

    /// Installs the subscriber as the global default plus the `log` bridge.
    /// Must be called only once per process.
    pub fn init(self) -> Result<Pipeline> {
        let (dispatch, pipeline) = self.assemble()?;
        tracing::dispatcher::set_global_default(dispatch)
            .map_err(|error| anyhow::anyhow!("global tracing subscriber already set: {error}"))?;
        tracing_log::LogTracer::builder()
            .with_max_level(log::LevelFilter::Trace)
            .init()
            .map_err(|error| anyhow::anyhow!("global log logger already set: {error}"))?;
        Ok(pipeline)
    }

    /// Assembles the layered subscriber without touching globals; used by
    /// `init` and by tests through a thread-local dispatch.
    fn assemble(self) -> Result<(Dispatch, Pipeline)> {
        let rust_log = std::env::var("RUST_LOG").ok();
        let preserved = filter::preserved_module_directives(rust_log.as_deref());
        let default_level =
            filter::bare_env_level(rust_log.as_deref()).unwrap_or(self.default_level);

        let env_filter = filter::build_filter(default_level, &preserved);
        let (filter_layer, reload_handle) = reload::Layer::new(env_filter);
        let filter_handle = FilterHandle::new(reload_handle, Arc::from(preserved));

        let sink_parts = self
            .file_writer
            .map(sink::spawn_sink)
            .transpose()
            .map_err(anyhow::Error::msg)?;

        let subscriber = Registry::default()
            .with(filter_layer)
            .with(stdout_layer())
            .with(sink_parts.as_ref().map(|(layer, _)| layer.clone()));

        #[cfg(feature = "tokio-trace")]
        let subscriber = subscriber.with(console_layer_opt());

        let (chrome_layer, chrome_handle) = chrome_layer_from_env();
        let subscriber = subscriber.with(chrome_layer);

        let pipeline = Pipeline {
            filter: filter_handle,
            sink: sink_parts.map(|(_, handle)| handle),
            chrome: chrome_handle,
        };
        Ok((Dispatch::new(subscriber), pipeline))
    }
}

/// Stdout layer: pretty with span close events in dev, the legacy compact
/// `console_format` in release.
#[cfg(debug_assertions)]
fn stdout_layer<S>() -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber,
    S: for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    use tracing_subscriber::fmt::format::FmtSpan;
    tracing_subscriber::fmt::layer()
        .pretty()
        .with_span_events(FmtSpan::CLOSE)
}

#[cfg(not(debug_assertions))]
fn stdout_layer<S>() -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber,
    S: for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    tracing_subscriber::fmt::layer().event_format(fmt_layer::CompactConsoleFormat)
}

/// Console layer needs a live tokio runtime to spawn its server task; skip
/// it (with a note) when initialized outside one.
#[cfg(feature = "tokio-trace")]
fn console_layer_opt<S>() -> Option<impl tracing_subscriber::Layer<S>>
where
    S: tracing::Subscriber,
    S: for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        Some(
            console_subscriber::ConsoleLayer::builder()
                .with_default_env()
                .spawn(),
        )
    } else {
        eprintln!("tracing-estuary: no tokio runtime, tokio-console layer disabled");
        None
    }
}

#[cfg(feature = "chrome-trace")]
fn chrome_layer_from_env() -> (Option<chrome::ChromeTraceLayer>, ChromeTraceHandle) {
    use chrome::ENV_CHROME_TRACE;
    use std::path::PathBuf;
    match std::env::var_os(ENV_CHROME_TRACE) {
        Some(path) => {
            let layer = chrome::ChromeTraceLayer::new(PathBuf::from(path));
            let handle = layer.handle();
            (Some(layer), handle)
        }
        None => (None, ChromeTraceHandle::default()),
    }
}

#[cfg(not(feature = "chrome-trace"))]
fn chrome_layer_from_env() -> (Option<chrome::ChromeTraceLayer>, ChromeTraceHandle) {
    (None, ChromeTraceHandle::default())
}

#[cfg(test)]
mod tests;
