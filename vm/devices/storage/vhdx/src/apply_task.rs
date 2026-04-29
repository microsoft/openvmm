// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Apply task — writes logged pages to their final file offsets.
//!
//! The apply task receives [`ApplyBatch`] items from the
//! [log task](crate::log_task) via a mesh channel. For each batch, it
//! writes all pages to their final file offsets, releases log permits,
//! and publishes `applied_lsn` with the flush sequence number (FSN)
//! needed to make the writes durable.

#![allow(dead_code)]

use crate::AsyncFile;
use crate::flush::FlushSequencer;
use crate::log_permits::LogPermits;
use crate::log_task::LogData;
use crate::log_task::Lsn;
use crate::lsn_watermark::LsnWatermark;
use crate::open::FailureFlag;
use std::sync::Arc;

/// A batch of page-aligned data that has been logged and needs to be applied.
pub(crate) struct ApplyBatch<B> {
    /// The data to write.
    pub data: Vec<LogData<B>>,
    /// The LSN of the log entry that contains these pages.
    pub lsn: Lsn,
}

/// Run the apply task main loop.
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
            Err(_) => break,
        };

        let lsn = batch.lsn;
        let mut page_count = 0;

        for entry in batch.data {
            page_count += entry.page_count();
            let (file_offset, data) = entry.into_parts();
            if let Err(err) = file.write_from(file_offset, data).await {
                tracing::error!(
                    "VHDX apply task: write error at offset {:#x}: {err}",
                    file_offset
                );
                let message = format!("apply write failed: {err}");
                log_permits.fail(message.clone());
                applied_lsn.fail(message);
                failure_flag.set(&err);
                return;
            }
        }

        log_permits.release(page_count);

        let fsn = flush_sequencer.current_fsn();
        applied_lsn.advance(lsn, fsn);
    }
}
