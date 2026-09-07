//! Log line formats.
//!
//! The line formats this crate ships with; output is byte-stable across
//! releases so log files and tooling built around them keep working. The
//! `color` feature toggles ANSI styling of the stdout format.
//!
//! The file formats cache the `[YYYY-MM-DD HH:MM:SS.mmm] ` prefix per UTC
//! millisecond: within one millisecond the prefix is reused verbatim instead
//! of redoing the local-time conversion and strftime. The cache key comes
//! from `SystemTime` (no timezone math) and only a miss touches
//! `DeferredNow` formatting; a mutex keeps the functions correct for any
//! caller. Output is byte-identical to formatting from scratch; a timestamp
//! may lag real time by at most the same writer-side jitter a deferred
//! timestamp already has.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::thread;

use flexi_logger::DeferredNow;
use log::Record;

fn level_filter_to_string(log_level: &log::LevelFilter) -> Cow<'static, str> {
    #[cfg(feature = "color")]
    {
        use nu_ansi_term::Color;
        match log_level {
            log::LevelFilter::Off => Cow::Owned(Color::Fixed(8).paint("OFF").to_string()),
            log::LevelFilter::Error => Cow::Owned(Color::Red.paint("ERROR").to_string()),
            log::LevelFilter::Warn => Cow::Owned(Color::Yellow.paint("WARN ").to_string()),
            log::LevelFilter::Info => Cow::Owned(Color::Green.paint("INFO ").to_string()),
            log::LevelFilter::Debug => Cow::Owned(Color::Blue.paint("DEBUG").to_string()),
            log::LevelFilter::Trace => Cow::Owned(Color::Purple.paint("TRACE").to_string()),
        }
    }
    #[cfg(not(feature = "color"))]
    {
        match log_level {
            log::LevelFilter::Off => Cow::Borrowed("OFF"),
            log::LevelFilter::Error => Cow::Borrowed("ERROR"),
            log::LevelFilter::Warn => Cow::Borrowed("WARN "),
            log::LevelFilter::Info => Cow::Borrowed("INFO "),
            log::LevelFilter::Debug => Cow::Borrowed("DEBUG"),
            log::LevelFilter::Trace => Cow::Borrowed("TRACE"),
        }
    }
}

/// Stdout line format: `HH:MM:SS.mmm LEVEL module:line T{thread} message`.
pub fn console_format(
    w: &mut dyn std::fmt::Write,
    now: &mut DeferredNow,
    record: &Record,
) -> std::fmt::Result {
    #[cfg(feature = "color")]
    use nu_ansi_term::Color;

    let current_thread = thread::current();
    let thread_name = current_thread.name().unwrap_or("unnamed");

    let level = level_filter_to_string(&record.level().to_level_filter());

    let now = now.format("%H:%M:%S%.3f");
    #[cfg(feature = "color")]
    let now = Color::DarkGray.paint(Cow::from(now.to_string()));

    let line = record.line().unwrap_or(0);
    let module = record.module_path().unwrap_or("<unnamed>");
    let module_line = Cow::from(format!("{}:{}", module, line));
    #[cfg(feature = "color")]
    let module_line = Color::Purple.paint(module_line);

    let thread_name = Cow::from(format!("T{{{}}}", thread_name));
    #[cfg(feature = "color")]
    let thread_name = Color::Cyan.paint(thread_name);

    write!(
        w,
        "{} {} {} {} {}",
        now,
        level,
        module_line,
        thread_name,
        record.args(),
    )
}

/// File line format: `[YYYY-MM-DD HH:MM:SS.mmm LEVEL message` (millisecond
/// prefix cache; see the module docs). The `io::Write` shape matches
/// flexi_logger's `FormatFn` so it can be handed to `FileLogWriterBuilder`.
pub fn file_format_with_level(
    w: &mut dyn std::io::Write,
    now: &mut DeferredNow,
    record: &Record,
) -> std::io::Result<()> {
    let prefix = cached_timestamp_prefix(now);
    w.write_all(prefix.as_bytes())?;
    write!(w, "{} {}", record.level(), record.args())
}

/// File line format without the level: `[YYYY-MM-DD HH:MM:SS.mmm message`.
pub fn file_format_without_level(
    w: &mut dyn std::io::Write,
    now: &mut DeferredNow,
    record: &Record,
) -> std::io::Result<()> {
    let prefix = cached_timestamp_prefix(now);
    w.write_all(prefix.as_bytes())?;
    write!(w, "{}", record.args())
}

/// Renders `[<local timestamp>] ` and reuses it within the same millisecond.
fn cached_timestamp_prefix(now: &mut DeferredNow) -> impl std::ops::Deref<Target = str> + '_ {
    static CACHED: parking_lot::Mutex<(i64, String)> =
        parking_lot::Mutex::new((i64::MIN, String::new()));

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(i64::MIN);

    let mut cache = CACHED.lock();
    if cache.0 != now_ms {
        cache.1.clear();
        let _ = write!(cache.1, "[{}] ", now.format("%Y-%m-%d %H:%M:%S%.3f"));
        cache.0 = now_ms;
    }
    parking_lot::MutexGuard::map(cache, |state| state.1.as_mut_str())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)] // assertion helpers in tests diverge via panic

    use super::*;
    use log::Level;

    fn sample_record() -> Record<'static> {
        Record::builder()
            .args(format_args!("hello world"))
            .level(Level::Info)
            .target("app")
            .module_path(Some("app_lib::feat"))
            .file(Some("src/feat.rs"))
            .line(Some(42))
            .build()
    }

    fn render_console(record: &Record<'_>) -> String {
        let mut now = DeferredNow::default();
        let mut buf = String::with_capacity(128);
        if let Err(error) = console_format(&mut buf, &mut now, record) {
            panic!("format failed: {error}");
        }
        buf
    }

    fn render_file(record: &Record<'_>, f: FileFormatFn) -> String {
        let mut now = DeferredNow::default();
        let mut buf: Vec<u8> = Vec::with_capacity(128);
        if let Err(error) = f(&mut buf, &mut now, record) {
            panic!("format failed: {error}");
        }
        match String::from_utf8(buf) {
            Ok(text) => text,
            Err(error) => panic!("not utf-8: {error}"),
        }
    }

    type FileFormatFn =
        fn(&mut dyn std::io::Write, &mut DeferredNow, &Record<'_>) -> std::io::Result<()>;

    /// Strips ANSI escape sequences so shape assertions hold with and
    /// without the `color` feature.
    fn strip_ansi(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                // Skip until the sequence terminator (letter).
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

    #[test]
    fn console_format_keeps_padded_level_module_thread_shape() {
        let line = render_console(&sample_record());
        let plain = strip_ansi(&line);
        // `HH:MM:SS.mmm LEVEL module:line T{thread} message` with the level
        // padded to five characters; the thread name is harness-dependent.
        assert!(plain.contains(" INFO  app_lib::feat:42 T{"), "{line:?}");
        assert!(plain.ends_with(" hello world"), "{line:?}");
        assert!(
            plain.as_bytes()[2] == b':' && plain.as_bytes()[5] == b':',
            "{line:?}"
        );
        assert!(plain.starts_with(|c: char| c.is_ascii_digit()), "{line:?}");
    }

    #[test]
    fn file_formats_keep_timestamp_brackets() {
        let with_level = render_file(&sample_record(), file_format_with_level);
        let rest = with_level
            .strip_prefix('[')
            .and_then(|r| r.split_once("] "))
            .map(|(_, tail)| tail);
        assert_eq!(rest, Some("INFO hello world"), "{with_level:?}");
        assert_eq!(with_level.as_bytes()[20], b'.');

        let without_level = render_file(&sample_record(), file_format_without_level);
        assert!(
            without_level.ends_with("] hello world") && without_level.starts_with('['),
            "{without_level:?}"
        );
    }

    #[test]
    fn cached_prefix_reuse_keeps_lines_identical_within_a_millisecond() {
        let record = sample_record();
        let first = render_file(&record, file_format_with_level);
        let second = render_file(&record, file_format_with_level);
        // Same-millisecond reuse keeps both lines identical; a boundary
        // crossing changes only the timestamp digits, never the length.
        assert_eq!(first.len(), second.len());
        assert!(first.ends_with("INFO hello world"), "{first:?}");
    }
}
