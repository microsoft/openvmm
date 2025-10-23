// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! [`tracing`] support.

use std::sync::Arc;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Initialize tracing, returning a mesh pipe to read logs from.
pub fn init_tracing() -> mesh::pipe::ReadPipe {
    let (log_read, log_write) = mesh::pipe::pipe();
    let targets = Targets::new()
        .with_default(tracing::level_filters::LevelFilter::DEBUG)
        .with_target("mesh_remote", tracing::level_filters::LevelFilter::INFO);

    tracing_subscriber::fmt()
        .compact()
        .with_ansi(false)
        .with_timer(tracing_subscriber::fmt::time::uptime())
        .with_writer(Arc::new(TracingWriter(log_write)))
        .with_max_level(tracing::level_filters::LevelFilter::DEBUG)
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .log_internal_errors(true)
        .finish()
        .with(targets)
        .init();

    tracing::info!("tracing initialized");
    log_read
}

struct TracingWriter(mesh::pipe::WritePipe);

impl std::io::Write for &TracingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Note that this will fail if the pipe fills up. This is probably fine
        // for this use case.
        self.0.write_nonblocking(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
