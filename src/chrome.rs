//! Chrome/Perfetto trace layer.
//!
//! Enabled at runtime by `CV_TRACE_CHROME=<path>`: records span lifetimes as
//! complete (`"X"`) events and log events as thread-scoped instants (`"i"`)
//! in the Catapult JSON trace format Perfetto loads. The file is written on
//! explicit `flush()` (panic/exit paths) and when the event cap forces a
//! spill; a hard-killed process simply leaves a stale file behind.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use tracing::{Event, Subscriber};
use tracing_log::AsLog as _;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::fields::FieldExtractor;

/// Environment variable that turns the layer on: `CV_TRACE_CHROME=/tmp/t.json`.
pub const ENV_CHROME_TRACE: &str = "CV_TRACE_CHROME";

/// Upper bound on recorded events before the layer degrades to a stub.
const MAX_EVENTS: usize = 100_000;

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

fn escape_json_into(out: &mut String, value: &str) {
    if !value.bytes().any(needs_escape) {
        out.push_str(value);
        return;
    }
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
}

const fn needs_escape(byte: u8) -> bool {
    byte == b'"' || byte == b'\\' || byte < 0x20
}

struct SpanOpen {
    ts_us: u64,
    name: &'static str,
    cat: &'static str,
    tid: u64,
}

struct ChromeState {
    path: PathBuf,
    /// Pre-joined JSON array body (comma-separated event objects); avoids
    /// one heap allocation per event plus the final join.
    events: String,
    event_count: usize,
    truncated: bool,
    span_starts: HashMap<u64, SpanOpen>,
    tids: HashMap<std::thread::ThreadId, u64>,
    next_tid: u64,
}

#[cfg_attr(not(feature = "chrome-trace"), allow(dead_code))]
impl ChromeState {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            events: String::with_capacity(16 * 1024),
            event_count: 0,
            truncated: false,
            span_starts: HashMap::new(),
            tids: HashMap::new(),
            next_tid: 1,
        }
    }

    fn tid(&mut self) -> u64 {
        let key = std::thread::current().id();
        *self.tids.entry(key).or_insert_with(|| {
            let tid = self.next_tid;
            self.next_tid += 1;
            tid
        })
    }

    fn push_event(&mut self, event: &str) {
        if self.event_count >= MAX_EVENTS {
            self.truncated = true;
            return;
        }
        if self.event_count > 0 {
            self.events.push(',');
        }
        self.events.push_str(event);
        self.event_count += 1;
    }

    fn render(&self) -> String {
        let mut json = String::with_capacity(self.events.len() + 128);
        json.push_str("{\"traceEvents\":[");
        json.push_str(&self.events);
        json.push(']');
        if self.truncated {
            json.push_str(
                ",\"metadata\":{\"note\":\"tracing-estuary: event cap reached, trace truncated\"}",
            );
        }
        json.push('}');
        json
    }

    fn write_file(&self) {
        if let Err(error) = std::fs::write(&self.path, self.render()) {
            eprintln!(
                "tracing-estuary: failed to write chrome trace to {}: {error}",
                self.path.display()
            );
        }
    }
}

/// Tracing layer producing the chrome trace.
pub struct ChromeTraceLayer {
    state: Arc<Mutex<ChromeState>>,
}

#[cfg_attr(not(feature = "chrome-trace"), allow(dead_code))]
impl ChromeTraceLayer {
    /// Creates the layer writing to `path`.
    pub fn new(path: PathBuf) -> Self {
        Self {
            state: Arc::new(Mutex::new(ChromeState::new(path))),
        }
    }

    /// Returns the flush handle for panic/exit paths.
    pub fn handle(&self) -> ChromeTraceHandle {
        ChromeTraceHandle {
            state: Some(Arc::clone(&self.state)),
        }
    }
}

impl<S> Layer<S> for ChromeTraceLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        _ctx: Context<'_, S>,
    ) {
        let mut state = self.state.lock();
        let tid = state.tid();
        state.span_starts.insert(
            id.into_u64(),
            SpanOpen {
                ts_us: now_micros(),
                name: attrs.metadata().name(),
                cat: attrs.metadata().target(),
                tid,
            },
        );
    }

    fn on_close(&self, id: tracing::span::Id, _ctx: Context<'_, S>) {
        let mut state = self.state.lock();
        let Some(open) = state.span_starts.remove(&id.into_u64()) else {
            return;
        };
        let end = now_micros();
        let mut event = String::with_capacity(128);
        event.push_str("{\"ph\":\"X\",\"name\":\"");
        escape_json_into(&mut event, open.name);
        event.push_str("\",\"cat\":\"");
        escape_json_into(&mut event, open.cat);
        event.push_str("\",\"ts\":");
        event.push_str(&open.ts_us.to_string());
        event.push_str(",\"dur\":");
        event.push_str(&end.saturating_sub(open.ts_us).to_string());
        event.push_str(",\"pid\":");
        event.push_str(&std::process::id().to_string());
        event.push_str(",\"tid\":");
        event.push_str(&open.tid.to_string());
        event.push_str(",\"args\":{}}");
        state.push_event(&event);
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut extractor = FieldExtractor::message_and_target();
        event.record(&mut extractor);
        let meta = event.metadata();
        let name = extractor.message.unwrap_or_else(|| meta.name().to_owned());
        let target = extractor.log_target.unwrap_or_else(|| meta.target().into());
        let level = meta.level().as_log();

        let mut state = self.state.lock();
        let (tid, ts) = (state.tid(), now_micros());
        let mut json = String::with_capacity(128 + name.len() + target.len());
        json.push_str("{\"ph\":\"i\",\"scope\":\"t\",\"name\":\"");
        escape_json_into(&mut json, &name);
        json.push_str("\",\"cat\":\"");
        escape_json_into(&mut json, &target);
        json.push_str("\",\"level\":\"");
        json.push_str(&level.as_str().to_lowercase());
        json.push_str("\",\"ts\":");
        json.push_str(&ts.to_string());
        json.push_str(",\"pid\":");
        json.push_str(&std::process::id().to_string());
        json.push_str(",\"tid\":");
        json.push_str(&tid.to_string());
        json.push_str(",\"args\":{}}");
        state.push_event(&json);
    }
}

/// Cloneable flush handle; a no-op stub when the layer is not built.
#[derive(Clone, Default)]
pub struct ChromeTraceHandle {
    state: Option<Arc<Mutex<ChromeState>>>,
}

impl ChromeTraceHandle {
    /// Best-effort write of the trace file.
    pub fn flush(&self) {
        if let Some(state) = &self.state {
            state.lock().write_file();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_json_handles_specials() {
        let mut out = String::new();
        escape_json_into(&mut out, "a\"b\\c\nd\u{1}");
        assert_eq!(out, "a\\\"b\\\\c\\nd\\u0001");
    }
}
