//! Global-subscriber integration test. The global default and LogTracer can
//! each be installed only once per process, so this binary contains exactly
//! one `#[test]`.
#![allow(clippy::panic)] // test assertions diverge via panic

use flexi_logger::{
    Cleanup, Criterion, Naming,
    writers::{FileLogWriter, FileLogWriterBuilder},
};

use tracing_estuary::{FlexiSink, PipelineBuilder};

fn file_builder(dir: &std::path::Path) -> FileLogWriterBuilder {
    FileLogWriter::builder(
        flexi_logger::FileSpec::default()
            .directory(dir.to_owned())
            .basename("bridge"),
    )
    .format(tracing_estuary::file_format_with_level)
    .rotate(
        Criterion::Size(128 * 1024),
        Naming::TimestampsCustomFormat {
            current_infix: Some("latest"),
            format: "%Y-%m-%d_%H-%M-%S",
        },
        Cleanup::KeepLogFiles(8),
    )
}

fn log_file_content(dir: &std::path::Path) -> String {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    match names.first() {
        Some(name) => std::fs::read_to_string(dir.join(name)).unwrap_or_default(),
        None => String::new(),
    }
}

#[test]
fn global_log_bridge_end_to_end() {
    let Ok(dir) = tempfile::tempdir() else {
        panic!("tempdir failed");
    };
    // Keep the single-shot global init hermetic against ambient knobs.
    // Safety: single-threaded test binary at this point.
    unsafe {
        std::env::remove_var("RUST_LOG");
        std::env::remove_var("CV_TRACE_CHROME");
    }
    let pipeline = match PipelineBuilder::new()
        .default_level(log::LevelFilter::Info)
        .file_writer(file_builder(dir.path()))
        .init()
    {
        Ok(pipeline) => pipeline,
        Err(error) => panic!("pipeline init failed: {error}"),
    };

    // The `logging!` macro shape: target "app", prefixed message.
    log::info!(target: "app", "{} {}", "[Core]", "global bridge works");
    // Blocked module.
    log::error!(target: "wry::event_loop", "must not reach the file");
    // Below the default level.
    log::trace!(target: "app", "too verbose");

    assert!(
        pipeline.sink.as_ref().is_some_and(FlexiSink::flush),
        "sink flush should drain"
    );

    let content = log_file_content(dir.path());
    assert!(
        content.contains("] INFO [Core] global bridge works"),
        "app record missing, content: {content:?}"
    );
    assert!(!content.contains("must not reach the file"));
    assert!(!content.contains("too verbose"));

    // Runtime level switch through the reload handle.
    if let Err(error) = pipeline.filter.set_default_level(log::LevelFilter::Trace) {
        panic!("reload failed: {error}");
    }
    log::trace!(target: "app_lib::feat", "now visible");
    assert!(pipeline.sink.as_ref().is_some_and(FlexiSink::flush));
    let content = log_file_content(dir.path());
    assert!(
        content.contains("] TRACE now visible"),
        "trace level should be live after reload, content: {content:?}"
    );
}
