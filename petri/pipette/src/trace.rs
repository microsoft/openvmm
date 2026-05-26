// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! [`tracing`] support.

use parking_lot::Mutex;
use std::sync::Arc;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Global writer shared with the tracing subscriber so that the underlying
/// pipe can be swapped on reconnect (e.g. after save/restore).
static TRACING_WRITER: Mutex<Option<Arc<TracingWriter>>> = Mutex::new(None);

/// Initialize tracing, returning a mesh pipe to read logs from.
///
/// On the first call this installs the global tracing subscriber. On
/// subsequent calls (after a save/restore reconnect) it swaps the
/// underlying write pipe so that logs flow to the new host connection.
pub fn init_tracing() -> mesh::pipe::ReadPipe {
    let (log_read, log_write) = mesh::pipe::pipe();

    let mut global = TRACING_WRITER.lock();
    if let Some(writer) = global.as_ref() {
        // Already initialized — swap to the new pipe so logs flow
        // to the new host connection after a save/restore reconnect.
        *writer.0.lock() = log_write;
    } else {
        let writer = Arc::new(TracingWriter(Mutex::new(log_write)));
        *global = Some(writer.clone());
        drop(global);

        let targets = Targets::new()
            .with_default(tracing::level_filters::LevelFilter::DEBUG)
            .with_target("mesh_remote", tracing::level_filters::LevelFilter::INFO);

        tracing_subscriber::fmt()
            .compact()
            .with_ansi(false)
            .with_timer(tracing_subscriber::fmt::time::uptime())
            .with_writer(writer)
            .with_max_level(tracing::level_filters::LevelFilter::DEBUG)
            .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
            .log_internal_errors(true)
            .finish()
            .with(targets)
            .init();
    }

    tracing::info!("tracing initialized");
    log_read
}

struct TracingWriter(Mutex<mesh::pipe::WritePipe>);

impl std::io::Write for &TracingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Note that this will fail if the pipe fills up. This is probably fine
        // for this use case.
        self.0.lock().write_nonblocking(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
