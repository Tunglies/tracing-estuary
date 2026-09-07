//! Shared helpers for interpreting tracing events, including the extra
//! fields [`tracing_log`] attaches to events bridged from `log` records.

use compact_str::CompactString;
use tracing::field::{Field, Visit};

/// Extracts the rendered message plus the `log.*` bridging fields (if any)
/// from an event.
///
/// Events produced by [`tracing_log::LogTracer`] carry the real target,
/// module path and source location in `log.target` / `log.module_path` /
/// `log.file` / `log.line` fields, because the shared static event metadata
/// only carries the target `"log"`.
///
/// `capture_origin` controls whether the `log.module_path` / `log.file` /
/// `log.line` origin fields are materialized; the message and target are
/// always captured. Hot layers that only need the message and target skip
/// two heap allocations per bridged event.
#[derive(Default)]
pub(crate) struct FieldExtractor {
    pub message: Option<String>,
    /// CompactString keeps the short, common targets (`app`, `sidecar`, ...)
    /// heap-allocation free.
    pub log_target: Option<CompactString>,
    pub log_module_path: Option<String>,
    pub log_file: Option<String>,
    pub log_line: Option<u32>,
    capture_origin: bool,
}

impl FieldExtractor {
    /// Captures only `message` and `log.target` (sink/chrome hot paths).
    pub(crate) const fn message_and_target() -> Self {
        Self {
            message: None,
            log_target: None,
            log_module_path: None,
            log_file: None,
            log_line: None,
            capture_origin: false,
        }
    }

    /// Captures all `log.*` fields (stdout formatter).
    #[cfg_attr(debug_assertions, allow(dead_code))] // release-only caller
    pub(crate) const fn full() -> Self {
        Self {
            message: None,
            log_target: None,
            log_module_path: None,
            log_file: None,
            log_line: None,
            capture_origin: true,
        }
    }

    /// Non-message structured fields, rendered as ` key=value` pairs and
    /// appended to the message. Log-bridged events have none, so their
    /// rendered message stays byte-identical to the legacy pipeline.
    fn append_extra(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        match self.message.as_mut() {
            Some(message) => {
                message.push(' ');
                message.push_str(field.name());
                message.push('=');
                message.push_str(&rendered);
            }
            None => {
                let mut line = String::with_capacity(field.name().len() + rendered.len() + 1);
                line.push_str(field.name());
                line.push('=');
                line.push_str(&rendered);
                self.message = Some(line);
            }
        }
    }
}

impl Visit for FieldExtractor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            // `Debug for format::Arguments` renders the formatted message
            // without quoting.
            self.message = Some(format!("{value:?}"));
        } else {
            self.append_extra(field, value);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_owned());
            return;
        }
        match field.name() {
            "log.target" => self.log_target = Some(CompactString::from(value)),
            "log.module_path" => {
                if self.capture_origin {
                    self.log_module_path = Some(value.to_owned());
                }
            }
            "log.file" => {
                if self.capture_origin {
                    self.log_file = Some(value.to_owned());
                }
            }
            _ => self.append_extra(field, &value),
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
            return;
        }
        if field.name() == "log.line" {
            if self.capture_origin {
                self.log_line = u32::try_from(value).ok();
            }
        } else {
            self.append_extra(field, &value);
        }
    }
}
