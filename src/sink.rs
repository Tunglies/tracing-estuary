//! File sink: a tracing layer that renders events back into `log::Record`s
//! and writes them through a flexi `FileLogWriter`, preserving the size-based
//! rotation of the legacy pipeline.
//!
//! Writes are funneled through a bounded channel to a dedicated writer
//! thread. Producers never block: when the channel is full the record is
//! dropped and counted.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
};
use std::time::Duration;

use compact_str::CompactString;
use flexi_logger::{
    DeferredNow,
    writers::{FileLogWriter, FileLogWriterBuilder, LogWriter as _},
};
use tracing::{Event, Subscriber};
use tracing_log::AsLog as _;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::fields::FieldExtractor;

/// Bound of the writer channel.
const DEFAULT_CHANNEL_CAPACITY: usize = 1024;

/// Budget for control-path operations (flush/reset/shutdown); the hot
/// logging path is never blocked.
pub(crate) const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);

/// Owned form of the parts of a `log::Record` the file format needs.
struct OwnedRecord {
    level: log::Level,
    target: CompactString,
    message: String,
}

enum SinkMsg {
    Write(OwnedRecord),
    /// Drains everything queued before it, then flushes the file.
    Flush(SyncSender<()>),
    /// Hot-rebuilds the writer with new rotation settings.
    Reset(Box<FileLogWriterBuilder>, SyncSender<Result<(), String>>),
    /// Flushes and ends the writer thread.
    Shutdown(SyncSender<()>),
}

/// Non-blocking enqueue; drops (and counts) the message when full.
fn enqueue(tx: &SyncSender<SinkMsg>, dropped: &AtomicU64, msg: SinkMsg) {
    if let Err(TrySendError::Full(_)) = tx.try_send(msg) {
        dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// Blocking-but-bounded enqueue for control messages (flush/reset/shutdown):
/// retries `try_send` until the writer thread frees capacity or the deadline
/// passes. `SyncSender::send_timeout` is not stable on this toolchain.
fn send_with_deadline(
    tx: &SyncSender<SinkMsg>,
    msg: SinkMsg,
    timeout: Duration,
) -> Result<(), SinkMsg> {
    use std::time::{Duration as Dur, Instant};

    let retry_pause = Dur::from_millis(5);
    let deadline = Instant::now() + timeout;
    let mut pending = msg;
    loop {
        match tx.try_send(pending) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(back)) => {
                pending = back;
                if Instant::now() + retry_pause >= deadline {
                    return Err(pending);
                }
                std::thread::sleep(retry_pause);
            }
            Err(TrySendError::Disconnected(back)) => return Err(back),
        }
    }
}

fn writer_loop(rx: Receiver<SinkMsg>, writer: FileLogWriter) {
    // Blocking `recv` sleeps the thread whenever the queue runs dry; after
    // each wakeup drain whatever else is already queued so a burst costs
    // one sleep/wake cycle instead of one per record. FIFO order keeps the
    // flush/reset semantics unchanged.
    while let Ok(msg) = rx.recv() {
        let mut shutdown = false;
        handle_msg(msg, &writer, &mut shutdown);
        while !shutdown {
            match rx.try_recv() {
                Ok(msg) => handle_msg(msg, &writer, &mut shutdown),
                Err(_) => break,
            }
        }
        if shutdown {
            break;
        }
    }
}

fn handle_msg(msg: SinkMsg, writer: &FileLogWriter, shutdown: &mut bool) {
    match msg {
        SinkMsg::Write(record) => {
            let args = format_args!("{}", record.message);
            let log_record = log::Record::builder()
                .args(args)
                .level(record.level)
                .target(record.target.as_str())
                .build();
            let mut now = DeferredNow::default();
            if let Err(error) = writer.write(&mut now, &log_record) {
                eprintln!("tracing-estuary: file write failed: {error}");
            }
        }
        SinkMsg::Flush(ack) => {
            let _ = writer.flush();
            let _ = ack.try_send(());
        }
        SinkMsg::Reset(builder, ack) => {
            // `reset` swaps the writer onto the new builder config (rotation
            // size/count); on failure the old configuration keeps running.
            let result = writer.reset(&builder).map_err(|error| error.to_string());
            let _ = ack.try_send(result);
        }
        SinkMsg::Shutdown(ack) => {
            let _ = writer.flush();
            let _ = ack.try_send(());
            *shutdown = true;
        }
    }
}

/// Tracing layer forwarding events to the writer thread.
#[derive(Clone)]
pub(crate) struct FlexiSinkLayer {
    tx: SyncSender<SinkMsg>,
    dropped: Arc<AtomicU64>,
}

impl<S> Layer<S> for FlexiSinkLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut extractor = FieldExtractor::message_and_target();
        event.record(&mut extractor);
        let record = OwnedRecord {
            level: event.metadata().level().as_log(),
            target: extractor
                .log_target
                .unwrap_or_else(|| CompactString::from(event.metadata().target())),
            message: extractor.message.unwrap_or_default(),
        };
        enqueue(&self.tx, &self.dropped, SinkMsg::Write(record));
    }
}

/// Cloneable handle for flush/reset of the sink from outside the layer.
#[derive(Clone)]
pub struct FlexiSink {
    tx: SyncSender<SinkMsg>,
    dropped: Arc<AtomicU64>,
}

impl FlexiSink {
    /// Enqueues a flush behind everything already queued and waits for the
    /// writer to drain and flush (bounded by `CONTROL_TIMEOUT`). Returns
    /// `false` on timeout/disconnect.
    pub fn flush(&self) -> bool {
        let (ack_tx, ack_rx) = sync_channel(1);
        if send_with_deadline(&self.tx, SinkMsg::Flush(ack_tx), CONTROL_TIMEOUT).is_err() {
            return false;
        }
        ack_rx.recv_timeout(CONTROL_TIMEOUT).is_ok()
    }

    /// Rebuilds the file writer (rotation size/count changes). The old file
    /// is flushed and rotated by flexi's `reset`.
    pub fn reset(&self, builder: FileLogWriterBuilder) -> Result<(), String> {
        let (ack_tx, ack_rx) = sync_channel(1);
        send_with_deadline(
            &self.tx,
            SinkMsg::Reset(Box::new(builder), ack_tx),
            CONTROL_TIMEOUT,
        )
        .map_err(|_| "log writer channel unavailable for reset".to_owned())?;
        match ack_rx.recv_timeout(CONTROL_TIMEOUT) {
            Ok(result) => result,
            Err(_) => Err("timed out waiting for log writer reset".to_owned()),
        }
    }

    /// Number of records dropped because the channel was full. Runtime
    /// health metric for consumers.
    pub fn dropped_records(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Ends the writer thread after a final flush. Explicit teardown for
    /// embedders that drop the pipeline before process exit. The flexi
    /// flusher thread behind `BufferAndFlushWith` cannot be stopped — it dies
    /// with the process, so short-lived pipelines are unaffected, but
    /// repeated init/shutdown cycles accumulate one thread and fd each.
    pub fn shutdown(&self) -> bool {
        let (ack_tx, ack_rx) = sync_channel(1);
        if send_with_deadline(&self.tx, SinkMsg::Shutdown(ack_tx), CONTROL_TIMEOUT).is_err() {
            return false;
        }
        ack_rx.recv_timeout(CONTROL_TIMEOUT).is_ok()
    }
}

/// Spawns the writer thread and returns the layer plus its handle. Building
/// the initial writer happens here so an unusable log directory fails
/// pipeline init loudly (matching the legacy `Logger::start()?` behavior).
pub(crate) fn spawn_sink(
    builder: FileLogWriterBuilder,
) -> Result<(FlexiSinkLayer, FlexiSink), String> {
    let writer = builder.try_build().map_err(|error| error.to_string())?;
    let (tx, rx) = sync_channel(DEFAULT_CHANNEL_CAPACITY);
    let dropped = Arc::new(AtomicU64::new(0));
    std::thread::Builder::new()
        .name("tracing-estuary-writer".to_owned())
        .spawn(move || writer_loop(rx, writer))
        .map_err(|error| error.to_string())?;
    Ok((
        FlexiSinkLayer {
            tx: tx.clone(),
            dropped: Arc::clone(&dropped),
        },
        FlexiSink { tx, dropped },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    fn owned_message(index: usize) -> SinkMsg {
        SinkMsg::Write(OwnedRecord {
            level: log::Level::Info,
            target: CompactString::from("app"),
            message: format!("message {index}"),
        })
    }

    #[test]
    fn enqueue_drops_and_counts_when_full() {
        let (tx, rx) = sync_channel(2);
        let dropped = AtomicU64::new(0);
        for index in 0..5 {
            enqueue(&tx, &dropped, owned_message(index));
        }
        assert_eq!(dropped.load(Ordering::Relaxed), 3);
        // The buffered messages are still retrievable.
        assert!(rx.try_recv().is_ok());
    }
}
