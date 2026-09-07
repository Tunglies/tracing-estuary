//! Compact console formatter wired into the release-only `stdout_layer`;
//! compiled (and tested) in every profile so release-only regressions
//! surface in debug test runs too.
//!
//! Delegates to `crate::format::console_format` with a synthesized
//! `log::Record` so stdout output stays byte-identical to the legacy flexi
//! `duplicate_to_stdout` path (level padding, `module:line`, `T{thread}`).

use std::fmt;

use flexi_logger::DeferredNow;
use tracing::{Event, Subscriber};
use tracing_log::AsLog as _;
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::fmt::format::{FormatEvent, Writer};
use tracing_subscriber::registry::LookupSpan;

use crate::fields::FieldExtractor;

// In debug builds the only production caller (the release `stdout_layer`)
// is cfg'd out, yet the item must stay compiled for the tests below.
#[cfg_attr(debug_assertions, allow(dead_code))]
pub(crate) struct CompactConsoleFormat;

impl<S, N> FormatEvent<S, N> for CompactConsoleFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut extractor = FieldExtractor::full();
        event.record(&mut extractor);
        let meta = event.metadata();

        let message = extractor.message.unwrap_or_default();
        let args = format_args!("{}", message);
        let target = extractor
            .log_target
            .as_deref()
            .unwrap_or_else(|| meta.target());
        let record = log::Record::builder()
            .args(args)
            .level(meta.level().as_log())
            .target(target)
            .module_path(
                extractor
                    .log_module_path
                    .as_deref()
                    .or_else(|| meta.module_path()),
            )
            .file(extractor.log_file.as_deref().or_else(|| meta.file()))
            .line(extractor.log_line.or_else(|| meta.line()))
            .build();

        let mut now = DeferredNow::default();
        crate::format::console_format(&mut writer, &mut now, &record)?;
        // flexi's stdout duplication terminated each line; the custom event
        // formatter must do the same.
        writer.write_str("\n")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)] // assertion helpers in tests diverge via panic

    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::{Registry, fmt::MakeWriter};

    /// Captures the fmt layer's stdout output for assertions.
    #[derive(Clone, Default)]
    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .map(|mut sink| {
                    sink.extend_from_slice(buf);
                    buf.len()
                })
                .map_err(|_| std::io::Error::other("poisoned"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl MakeWriter<'_> for CaptureWriter {
        type Writer = Self;
        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    // Exercises the full compact stdout path (field extraction -> synthesized
    // log::Record -> console_format -> trailing newline); runs in both debug
    // and release test profiles.
    #[test]
    fn compact_console_format_renders_bridged_log_records() {
        let capture = CaptureWriter::default();
        let env_filter = crate::filter::build_filter(log::LevelFilter::Info, "");
        let dispatch = tracing::Dispatch::new(
            Registry::default().with(env_filter).with(
                tracing_subscriber::fmt::layer()
                    .with_writer(capture.clone())
                    .event_format(CompactConsoleFormat),
            ),
        );

        tracing::dispatcher::with_default(&dispatch, || {
            let message = format!("[Setup] bridged message {}", 42);
            let args = format_args!("{}", message);
            let record = log::Record::builder()
                .args(args)
                .level(log::Level::Info)
                .target("app")
                .module_path(Some("app_lib::app_init"))
                .file(Some("src-tauri/src/lib.rs"))
                .line(Some(286))
                .build();
            let _ = tracing_log::format_trace(&record);
        });

        let text = {
            let Ok(guard) = capture.0.try_lock() else {
                panic!("writer lock poisoned");
            };
            String::from_utf8_lossy(&guard).into_owned()
        };
        // Strip ANSI so the shape assertions hold with and without `color`.
        let plain = strip_ansi(&text);
        assert!(
            plain.contains(" INFO  app_lib::app_init:286 T{"),
            "unexpected stdout line: {text:?}"
        );
        assert!(
            plain.contains("[Setup] bridged message 42"),
            "unexpected stdout line: {text:?}"
        );
        assert!(plain.ends_with('\n'), "missing trailing newline: {text:?}");
    }

    fn strip_ansi(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(ch);
            }
        }
        out
    }
}
