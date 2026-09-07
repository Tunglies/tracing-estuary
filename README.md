# tracing-estuary

*Where the `log` rivers meet the `tracing` sea.*

A layered `tracing` subscriber pipeline whose file sink keeps
`flexi_logger`'s `FileLogWriter` — the size-based rotation the official
`tracing-appender` still lacks ([tokio-rs/tracing #1940](https://github.com/tokio-rs/tracing/issues/1940)).

```text
log::Record --tracing-log LogTracer (global log logger)--> tracing Event
                                |
                    Registry + EnvFilter (reloadable)
                                |
   +---------------+------------------+------------------+----------------+
   | fmt(stdout)   | flexi sink layer | ConsoleLayer     | ChromeLayer
   |               | FileLogWriter    | (tokio-console)  | (Perfetto JSON)
   |               | size rotation    | feature+runtime  | feature+env
```

## Why

- **Size rotation, today**: the sink wraps `flexi_logger::FileLogWriter`
  (rotation size, retained-file count, hot rebuild) as an ordinary layer —
  unlike `flexi_logger`'s own `trc` module, which routes tracing *into* a
  global flexi logger instead of the other way around, so layers like
  tokio-console or a chrome trace cannot coexist.
- **Bounded and non-blocking**: writes flow through a bounded channel to a
  dedicated writer thread. Producers never block; overflow drops and counts
  (`FlexiSink::dropped_records()`), unlike `tracing-appender`'s unbounded
  `NonBlocking` queue.
- **Two-leveled filtering**: the default level is hot-swappable at runtime
  (`FilterHandle::set_default_level`) while `RUST_LOG` module directives
  survive every swap; a configurable prefix blocklist (with carve-out) keeps
  chatty dependencies quiet.
- **Extras as layers**: optional tokio-console and Perfetto chrome-trace
  layers coexist with file/stdout logging.

## Usage

```rust,ignore
let pipeline = tracing_estuary::PipelineBuilder::new()
    .default_level(log::LevelFilter::Info)
    .file_writer(file_log_writer_builder) // rotation config included
    .init()?;                              // global subscriber + log bridge, once per process

pipeline.filter.set_default_level(log::LevelFilter::Debug)?; // hot level switch
pipeline.sink.as_ref().map(|sink| sink.reset(builder));      // rotation re-config
pipeline.flush_all();                                         // panic / shutdown drain
```

### Features

| feature | effect |
|---|---|
| `tokio-trace` | add the tokio-console layer (coexists with file/stdout logging) |
| `chrome-trace` | add the Perfetto chrome-trace layer, gated at runtime by `CV_TRACE_CHROME=<path>` |
| `color` | ANSI styling for the stdout console format |

Runtime knobs: `RUST_LOG` (full directive syntax), `CV_TRACE_CHROME`,
`TOKIO_CONSOLE_BIND`.

## Notes

- Buffered write mode (`WriteMode::BufferAndFlushWith`) bounds a hard-kill
  loss window; panic/exit paths drain explicitly.
- With `tauri-plugin-devtools` enabled, the host app should skip this
  pipeline (that plugin installs its own global subscriber and panics if one
  exists).

## License

MIT OR Apache-2.0.
