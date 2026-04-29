// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Apply task — writes logged pages to their final file offsets.
//!
//! The apply task receives [`ApplyBatch`] items from the
//! [log task](crate::log_task) via a mesh channel. For each batch, it
//! writes all pages to their final file offsets, **releases log permits**
//! (via [`LogPermits`](crate::log_permits::LogPermits)), and publishes
//! `applied_lsn` with the flush sequence number (FSN) needed to make
//! the writes durable.
//!
//! The apply task does **not** flush. Flushing is driven by consumers
//! who need durability:
//! - The log task flushes when it needs to advance the log tail
//!   (on `LogFull` or graceful close).
//! - [`VhdxFile::flush()`](crate::open::VhdxFile::flush) flushes for crash safety.
//!
//! Both callers use
//! [`FlushSequencer::flush_through()`](crate::flush::FlushSequencer::flush_through)
//! with the FSN from the watermark, which coalesces naturally.

use crate::AsyncFile;
use crate::flush::FlushSequencer;
use crate::log_permits::LogPermits;
use crate::log_task::LogData;
use crate::log_task::Lsn;
use crate::lsn_watermark::LsnWatermark;
use crate::open::FailureFlag;
use std::sync::Arc;

/// A batch of page-aligned data that has been logged and needs to be applied
/// (written to their final file offsets).
pub(crate) struct ApplyBatch<B> {
    /// The data to write.
    pub data: Vec<LogData<B>>,
    /// The LSN of the log entry that contains these pages.
    pub lsn: Lsn,
}

/// Run the apply task main loop.
///
/// Receives batches from the log task, writes pages to their final
/// file offsets, releases log permits, and publishes `applied_lsn`
/// with the FSN needed for durability.
pub(crate) async fn run_apply_task<F: AsyncFile>(
    mut rx: mesh::Receiver<ApplyBatch<F::Buffer>>,
    file: Arc<F>,
    flush_sequencer: Arc<FlushSequencer>,
    applied_lsn: Arc<LsnWatermark>,
    log_permits: Arc<LogPermits>,
    failure_flag: Arc<FailureFlag>,
) {
    loop {
        let batch = match rx.recv().await {
            Ok(batch) => batch,
            Err(_) => {
                // Channel closed — log task shut down. Exit.
                break;
            }
        };

        let lsn = batch.lsn;
        let mut page_count = 0;

        // Write each range to its final file offset (zero-copy via Arc).
        for entry in batch.data {
            page_count += entry.page_count();
            let (file_offset, data) = entry.into_parts();
            if let Err(e) = file.write_from(file_offset, data).await {
                tracing::error!(
                    "VHDX apply task: write error at offset {:#x}: {e}",
                    file_offset
                );
                log_permits.fail(format!("apply write failed: {e}"));
                applied_lsn.fail(format!("apply write failed: {e}"));
                failure_flag.set(&e);
                return;
            }
        }

        // The pages have been dropped by this point, so it's safe to release
        // the permits for this batch.
        log_permits.release(page_count);

        // Capture the FSN *after* the writes. Flushing through this FSN
        // will make all the writes above durable. We don't flush here —
        // the log task or VhdxFile::flush() will do it when needed.
        let fsn = flush_sequencer.current_fsn();

        // Publish (lsn, fsn): "pages through this LSN are at their final
        // offsets; flush through this FSN to make them durable."
        applied_lsn.advance(lsn, fsn);
    }
}
