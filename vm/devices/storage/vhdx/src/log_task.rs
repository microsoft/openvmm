// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Log task — a single async task that owns all log state and provides
//! crash-consistent metadata persistence.
//!
//! The log task receives [`LogRequest`] messages via a `mesh` channel.
//! [`LogRequest::Commit`] is fire-and-forget: the cache sends a batch
//! of dirty pages and moves on. The log task writes WAL entries,
//! releases permits, and publishes `logged_through_lsn`.
//!
//! After logging a batch, the log task sends it to the
//! [apply task](crate::apply_task) for writing to final file offsets.
//! The apply task publishes `applied_through_lsn`, which the log task
//! reads to advance its tail.
//!
//! # Crash Consistency
//!
//! Metadata changes (BAT entries, sector bitmap bits) are journaled before
//! being committed to their final locations. On crash,
//! [`replay_log()`](crate::log::replay_log) restores them.

use crate::AsyncFile;
use crate::apply_task::ApplyBatch;
use crate::error::PipelineFailed;
use crate::error::VhdxIoError;
use crate::error::VhdxIoErrorInner;
use crate::flush::FlushSequencer;
use crate::flush::Fsn;
use crate::format::LOG_SECTOR_SIZE;
use crate::log::DataPage;
use crate::log::LogWriter;
use crate::log_permits::LogPermits;
use crate::lsn_watermark::LsnWatermark;
use crate::open::FailureFlag;
use mesh::rpc::Rpc;
use std::collections::VecDeque;
use std::sync::Arc;
use thiserror::Error;

const LOG_DATA_PAGE_SIZE: usize = LOG_SECTOR_SIZE as usize;

/// Internal error type for the log task.
///
/// Captures the three failure modes of the log pipeline without
/// pulling in the public [`VhdxIoError`]. Converted to [`VhdxIoError`]
/// only at the [`LogClient::close`] boundary.
#[derive(Debug, Error)]
pub(crate) enum LogTaskError {
    /// An I/O error from WAL writes or flushes.
    #[error("flush error")]
    Flush(#[source] std::io::Error),
    /// The apply task (or another pipeline stage) has failed.
    #[error("pipeline failed")]
    PipelineFailed(#[source] PipelineFailed),
    #[error("failed to write log entry")]
    Write(#[source] std::io::Error),
    #[error("log transaction too big ({0} pages)")]
    TransactionTooBig(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Lsn(u64);

impl Lsn {
    pub const ZERO: Lsn = Lsn(0);

    #[cfg(test)]
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

/// A request to the log task.
pub(crate) enum LogRequest<B> {
    /// Log a batch of dirty pages (fire-and-forget).
    Commit(Transaction<B>),

    /// Graceful shutdown: log all pending, wait for apply, clear log GUID.
    Close(Rpc<(), Result<(), LogTaskError>>),
}

/// Committed data at a log-data-page-aligned file offset.
pub(crate) struct LogData<B> {
    /// File offset where this data should ultimately be written.
    file_offset: u64,
    /// Log-data-page-aligned data (shared with the cache via Arc COW).
    data: Arc<B>,
}

impl<B: AsRef<[u8]>> LogData<B> {
    pub(crate) fn new(file_offset: u64, data: Arc<B>) -> Self {
        let len = data.as_ref().as_ref().len();
        assert_ne!(len, 0);
        assert!(
            file_offset.is_multiple_of(LOG_DATA_PAGE_SIZE as u64),
            "committed data offset {file_offset:#x} is not {LOG_DATA_PAGE_SIZE}-byte aligned",
        );
        assert!(
            len.is_multiple_of(LOG_DATA_PAGE_SIZE),
            "committed data length {len} is not {LOG_DATA_PAGE_SIZE}-byte aligned"
        );
        Self { file_offset, data }
    }

    pub(crate) fn page_count(&self) -> usize {
        self.data.as_ref().as_ref().len() / LOG_DATA_PAGE_SIZE
    }

    #[cfg(test)]
    pub(crate) fn data(&self) -> &[u8] {
        self.data.as_ref().as_ref()
    }

    pub(crate) fn into_parts(self) -> (u64, Arc<B>) {
        (self.file_offset, self.data)
    }
}

/// A batch of dirty pages to be logged atomically.
pub(crate) struct Transaction<B> {
    /// The LSN assigned by the cache at commit time.
    pub lsn: Lsn,
    /// The data in this batch.
    pub data: Vec<LogData<B>>,
    /// If set, the log task must wait for this FSN to complete before
    /// writing the WAL entry.
    pub pre_log_fsn: Option<Fsn>,
}

/// Client-side handle for sending transactions to the log task.
///
/// Couples the `Sender<LogRequest>` with the LSN counter so that
/// LSN assignment and channel send are always atomic. All methods
/// take `&mut self` — the caller (cache's `PageMap` lock) provides
/// exclusivity.
pub(crate) struct LogClient<B> {
    sender: mesh::Sender<LogRequest<B>>,
    current_lsn: Lsn,
}

impl<B: Send + Sync + 'static> LogClient<B> {
    /// Create a new log client wrapping the given sender.
    pub fn new(sender: mesh::Sender<LogRequest<B>>) -> Self {
        Self {
            sender,
            current_lsn: Lsn::ZERO,
        }
    }

    /// Returns the most recently committed LSN (0 if none).
    pub fn current_lsn(&self) -> Lsn {
        self.current_lsn
    }

    /// Begin a new transaction. The returned [`LogTransaction`] borrows
    /// `self` mutably, preventing interleaved transactions.
    ///
    /// The LSN is not assigned until [`LogTransaction::commit()`] is
    /// called. Dropping the transaction without committing is a no-op.
    pub fn begin(&mut self) -> LogTransaction<'_, B> {
        LogTransaction { client: self }
    }

    /// Send a graceful close request to the log task and wait for
    /// it to finish processing all pending batches.
    ///
    /// Consumes the client (drops the sender after the RPC completes).
    pub async fn close(self) -> Result<(), VhdxIoError> {
        use mesh::rpc::RpcSend;
        self.sender
            .call(LogRequest::Close, ())
            .await
            .map_err(|_| VhdxIoErrorInner::Failed(PipelineFailed("log task closed".into())))?
            .map_err(VhdxIoErrorInner::LogClose)?;
        Ok(())
    }
}

/// An in-progress log transaction. Borrows the [`LogClient`] mutably
/// to prevent interleaved sends.
///
/// Call [`commit()`](Self::commit) to assign an LSN and send the
/// transaction to the log task. Dropping without committing is safe
/// and does not advance the LSN.
pub(crate) struct LogTransaction<'a, B> {
    client: &'a mut LogClient<B>,
}

impl<B: Send + Sync + 'static> LogTransaction<'_, B> {
    /// The LSN that will be assigned if this transaction is committed.
    pub fn lsn(&self) -> Lsn {
        Lsn(self.client.current_lsn.0 + 1)
    }

    /// Commit the transaction: assign the next LSN and send it to the
    /// log task. Consumes the transaction.
    pub fn commit(self, log_data: Vec<LogData<B>>, pre_log_fsn: Option<Fsn>) -> Lsn {
        self.client.current_lsn.0 += 1;
        let lsn = self.client.current_lsn;
        self.client.sender.send(LogRequest::Commit(Transaction {
            lsn,
            data: log_data,
            pre_log_fsn,
        }));
        lsn
    }
}
/// Tracks a batch that has been sent to the applier but whose tail
/// hasn't been advanced yet.
struct PendingTail {
    /// The LSN of the batch. Once `applied_lsn >= lsn`, the tail
    /// can advance to `new_tail`.
    lsn: Lsn,
    /// The log-region offset to advance the tail to.
    new_tail: u32,
}

/// All mutable state owned by the log task.
pub(crate) struct LogTask<F: AsyncFile> {
    file: Arc<F>,
    log_writer: LogWriter,
    flush_sequencer: Arc<FlushSequencer>,
    log_permits: Arc<LogPermits>,
    logged_lsn: Arc<LsnWatermark>,
    applied_lsn: Arc<LsnWatermark>,
    apply_tx: mesh::Sender<ApplyBatch<F::Buffer>>,
    pending_tails: VecDeque<PendingTail>,
    failure_flag: Arc<FailureFlag>,
}

impl<F: AsyncFile> LogTask<F> {
    /// Create a new log task with the given dependencies.
    pub(crate) fn new(
        file: Arc<F>,
        log_writer: LogWriter,
        flush_sequencer: Arc<FlushSequencer>,
        log_permits: Arc<LogPermits>,
        logged_lsn: Arc<LsnWatermark>,
        applied_lsn: Arc<LsnWatermark>,
        apply_tx: mesh::Sender<ApplyBatch<F::Buffer>>,
        failure_flag: Arc<FailureFlag>,
    ) -> Self {
        Self {
            file,
            log_writer,
            flush_sequencer,
            log_permits,
            logged_lsn,
            applied_lsn,
            apply_tx,
            pending_tails: VecDeque::new(),
            failure_flag,
        }
    }

    /// Run the log task main loop.
    ///
    /// Consumes requests from `rx` until a `Close` request is received
    /// or the channel is dropped.
    pub async fn run(mut self, mut rx: mesh::Receiver<LogRequest<F::Buffer>>) {
        loop {
            self.advance_tails();

            let request = match rx.recv().await {
                Ok(req) => req,
                Err(_) => {
                    tracing::warn!("VHDX log task: channel closed without close() — file is dirty");
                    break;
                }
            };

            match request {
                LogRequest::<F::Buffer>::Commit(txn) => {
                    if let Err(e) = self.handle_commit(txn).await {
                        tracing::error!("VHDX log task fatal error: {e}");
                        self.log_permits.fail(e.to_string());
                        self.logged_lsn.fail(e.to_string());
                        self.failure_flag.set(&e);
                        break;
                    }
                }
                LogRequest::<F::Buffer>::Close(rpc) => {
                    rpc.handle(async |()| self.graceful_close().await).await;
                    break;
                }
            }
        }
    }

    /// Advance the log tail for all batches whose applied data has
    /// been flushed (i.e., `applied_fsn <= completed_fsn`).
    fn advance_tails(&mut self) {
        let flushed_fsn = self.flush_sequencer.completed_fsn();
        let (applied, applied_fsn) = self.applied_lsn.get_with_fsn();
        while let Some(front) = self.pending_tails.front() {
            if front.lsn <= applied && applied_fsn <= flushed_fsn {
                self.log_writer.advance_tail(front.new_tail);
                self.pending_tails.pop_front();
            } else {
                break;
            }
        }
    }

    /// Flush applied data and advance tails. Used when the log is full
    /// and we need to reclaim space.
    async fn flush_and_advance_tails(&mut self) -> Result<(), LogTaskError> {
        if let Some(front) = self.pending_tails.front() {
            let target = front.lsn;
            let applied_fsn = self
                .applied_lsn
                .wait_for(target)
                .await
                .map_err(LogTaskError::PipelineFailed)?;
            self.flush_sequencer
                .flush_through(self.file.as_ref(), applied_fsn)
                .await
                .map_err(LogTaskError::Flush)?;
            self.advance_tails();
        }
        Ok(())
    }

    /// Write a WAL entry for the given pages (no flush).
    ///
    /// Returns `Ok(true)` if the entry was written, `Ok(false)` if the
    /// log is full (caller should drain and retry), or `Err` on I/O error.
    async fn write_log_entry(
        &mut self,
        pages: &[LogData<F::Buffer>],
    ) -> Result<bool, LogTaskError> {
        let page_count = pages.iter().map(LogData::page_count).sum();
        let mut data_pages = Vec::with_capacity(page_count);
        for p in pages {
            for (i, payload) in p.data.as_ref().as_ref().as_chunks().0.iter().enumerate() {
                data_pages.push(DataPage {
                    file_offset: p.file_offset + (i * LOG_DATA_PAGE_SIZE) as u64,
                    payload,
                });
            }
        }

        Ok(self
            .log_writer
            .write_entry(self.file.as_ref(), &data_pages, &[])
            .await
            .map_err(LogTaskError::Write)?
            .is_some())
    }

    /// Handle a Commit request: write WAL entry, publish LSN, send batch
    /// to applier. If the log is full, flushes applied data and retries.
    ///
    /// Returns `Err` on any fatal error. The caller (`run`) poisons
    /// the permits and watermarks — individual methods don't.
    async fn handle_commit(&mut self, txn: Transaction<F::Buffer>) -> Result<(), LogTaskError> {
        let lsn = txn.lsn;

        // Ensure pre_log_fsn constraint is met before logging.
        if let Some(fsn) = txn.pre_log_fsn {
            self.flush_sequencer
                .flush_through(self.file.as_ref(), fsn)
                .await
                .map_err(LogTaskError::Flush)?;
        }

        // Write WAL entry, retrying if the log is full.
        while !self.write_log_entry(&txn.data).await? {
            if self.pending_tails.is_empty() {
                return Err(LogTaskError::TransactionTooBig(
                    txn.data.iter().map(LogData::page_count).sum(),
                ));
            }
            self.flush_and_advance_tails().await?;
        }

        // Capture FSN after the WAL write. Flushing through this FSN
        // makes the WAL entry durable. We don't flush here —
        // VhdxFile::flush() will do it, or the LogFull path will if
        // space is needed.
        let wal_fsn = self.flush_sequencer.current_fsn();
        self.logged_lsn.advance(lsn, wal_fsn);

        let new_tail = self.log_writer.head();

        // Send to applier for background apply.
        self.apply_tx.send(ApplyBatch {
            data: txn.data,
            lsn,
        });

        self.pending_tails.push_back(PendingTail { lsn, new_tail });
        Ok(())
    }

    /// Graceful close: wait for all applies, flush, advance tails.
    ///
    /// After this returns, the log region is fully drained. The caller
    /// is responsible for clearing the log GUID in the header.
    async fn graceful_close(&mut self) -> Result<(), LogTaskError> {
        // Wait for all pending applies and flush.
        if let Some(last) = self.pending_tails.back() {
            let target_lsn = last.lsn;
            let applied_fsn = self
                .applied_lsn
                .wait_for(target_lsn)
                .await
                .map_err(LogTaskError::PipelineFailed)?;
            self.flush_sequencer
                .flush_through(self.file.as_ref(), applied_fsn)
                .await
                .map_err(LogTaskError::Flush)?;
        }

        // Advance all tails — data is durable at final offsets.
        for pt in self.pending_tails.drain(..) {
            self.log_writer.advance_tail(pt.new_tail);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AsyncFileExt;
    use crate::apply_task;
    use crate::log::LogRegion;
    use crate::tests::support::InMemoryFile;
    use pal_async::async_test;
    use pal_async::task::Spawn;

    const LOG_SIZE: u32 = 64 * 4096; // 256 KiB — deliberately small
    const LOG_OFFSET: u64 = 1024 * 1024; // 1 MiB into the file

    /// Set up a log task + apply task connected via channels.
    /// Returns (log_tx, file, permits, logged_lsn, applied_lsn,
    /// log_task_handle, apply_task_handle).
    async fn setup_pipeline(
        driver: &pal_async::DefaultDriver,
        log_size: u32,
        permit_count: usize,
    ) -> (
        mesh::Sender<LogRequest<Vec<u8>>>,
        Arc<InMemoryFile>,
        Arc<LogPermits>,
        Arc<LsnWatermark>,
        Arc<LsnWatermark>,
        pal_async::task::Task<()>,
        pal_async::task::Task<()>,
    ) {
        let file = Arc::new(InMemoryFile::new(4 * 1024 * 1024));
        setup_pipeline_with_file(driver, file, log_size, permit_count).await
    }

    /// Like `setup_pipeline`, but with a caller-provided file.
    async fn setup_pipeline_with_file(
        driver: &pal_async::DefaultDriver,
        file: Arc<InMemoryFile>,
        log_size: u32,
        permit_count: usize,
    ) -> (
        mesh::Sender<LogRequest<Vec<u8>>>,
        Arc<InMemoryFile>,
        Arc<LogPermits>,
        Arc<LsnWatermark>,
        Arc<LsnWatermark>,
        pal_async::task::Task<()>,
        pal_async::task::Task<()>,
    ) {
        let region = LogRegion {
            file_offset: LOG_OFFSET,
            length: log_size,
        };
        let guid = guid::Guid::new_random();
        let log_writer = LogWriter::initialize(file.as_ref(), region, guid, 4 * 1024 * 1024)
            .await
            .unwrap();

        let flush_sequencer = Arc::new(FlushSequencer::new());
        let log_permits = Arc::new(LogPermits::new(permit_count));
        let logged_lsn = Arc::new(LsnWatermark::new());
        let applied_lsn = Arc::new(LsnWatermark::new());

        let failure_flag = Arc::new(FailureFlag::new());

        let (apply_tx, apply_rx) = mesh::channel::<ApplyBatch<Vec<u8>>>();
        let (log_tx, log_rx) = mesh::channel::<LogRequest<Vec<u8>>>();

        // Spawn apply task.
        let apply_task = driver.spawn(
            "test-apply",
            apply_task::run_apply_task(
                apply_rx,
                file.clone(),
                flush_sequencer.clone(),
                applied_lsn.clone(),
                log_permits.clone(),
                failure_flag.clone(),
            ),
        );

        // Spawn log task.
        let log_task = driver.spawn(
            "test-log",
            LogTask::new(
                file.clone(),
                log_writer,
                flush_sequencer,
                log_permits.clone(),
                logged_lsn.clone(),
                applied_lsn.clone(),
                apply_tx,
                failure_flag,
            )
            .run(log_rx),
        );

        (
            log_tx,
            file,
            log_permits,
            logged_lsn,
            applied_lsn,
            log_task,
            apply_task,
        )
    }

    /// Build a Transaction with `n` fake pages.
    fn make_txn(lsn: Lsn, n: usize) -> Transaction<Vec<u8>> {
        let pages = (0..n)
            .map(|i| {
                LogData::new(
                    (2 * 1024 * 1024 + i * LOG_DATA_PAGE_SIZE) as u64,
                    Arc::new(vec![lsn.0 as u8; LOG_DATA_PAGE_SIZE]),
                )
            })
            .collect();
        Transaction {
            lsn,
            data: pages,
            pre_log_fsn: None,
        }
    }

    /// Acquire permits and send a commit. Mirrors what the cache does:
    /// acquire permits for each page, then commit (which sends the
    /// transaction to the log task).
    async fn send_commit(
        tx: &mesh::Sender<LogRequest<Vec<u8>>>,
        permits: &LogPermits,
        lsn: Lsn,
        page_count: usize,
    ) {
        permits.acquire(page_count).await.unwrap();
        tx.send(LogRequest::Commit(make_txn(lsn, page_count)));
    }

    #[async_test]
    async fn single_commit_publishes_lsn(driver: pal_async::DefaultDriver) {
        let (tx, _file, permits, logged_lsn, _applied_lsn, _log_task, _apply_task) =
            setup_pipeline(&driver, LOG_SIZE, 100).await;

        send_commit(&tx, &permits, Lsn(1), 1).await;
        logged_lsn.wait_for(Lsn(1)).await.unwrap();
    }

    #[async_test]
    async fn permits_return_after_apply(driver: pal_async::DefaultDriver) {
        let permit_count = 10;
        let (tx, _file, permits, logged_lsn, applied_lsn, _log_task, _apply_task) =
            setup_pipeline(&driver, LOG_SIZE, permit_count).await;

        // Send a commit of 5 pages (acquires 5 permits).
        send_commit(&tx, &permits, Lsn(1), 5).await;

        // Wait for the apply task to finish.
        logged_lsn.wait_for(Lsn(1)).await.unwrap();
        applied_lsn.wait_for(Lsn(1)).await.unwrap();

        // The apply task should have released 5 permits.
        // All 10 should be available again.
        assert_eq!(permits.available(), permit_count);
    }

    #[async_test]
    async fn multiple_commits_sequential(driver: pal_async::DefaultDriver) {
        let (tx, _file, permits, logged_lsn, _applied_lsn, _log_task, _apply_task) =
            setup_pipeline(&driver, LOG_SIZE, 100).await;

        for lsn in 1..=10u64 {
            send_commit(&tx, &permits, Lsn(lsn), 1).await;
        }

        // All 10 should be logged.
        logged_lsn.wait_for(Lsn(10)).await.unwrap();
    }

    #[async_test]
    async fn log_full_retry_makes_progress(driver: pal_async::DefaultDriver) {
        // Use a small log (256 KiB). Each page + entry overhead ~ 8 KiB.
        // With ~30 entries the log will fill up, forcing the retry path.
        let (tx, _file, permits, logged_lsn, _applied_lsn, _log_task, _apply_task) =
            setup_pipeline(&driver, LOG_SIZE, 500).await;

        // Send 50 single-page commits. This will exceed the 256 KiB log
        // and force LogFull → wait for apply → advance tail → retry.
        for lsn in 1..=50u64 {
            send_commit(&tx, &permits, Lsn(lsn), 1).await;
        }

        // If LogFull retry works, all 50 will eventually be logged.
        logged_lsn.wait_for(Lsn(50)).await.unwrap();
    }

    #[async_test]
    async fn large_batches_through_small_log(driver: pal_async::DefaultDriver) {
        // Each batch has 5 pages (~24 KiB with overhead). 256 KiB log
        // fits maybe 10 batches. Send 30 — forces multiple cycles of
        // LogFull → drain → retry.
        let (tx, _file, permits, logged_lsn, _applied_lsn, _log_task, _apply_task) =
            setup_pipeline(&driver, LOG_SIZE, 500).await;

        for lsn in 1..=30u64 {
            send_commit(&tx, &permits, Lsn(lsn), 5).await;
        }

        logged_lsn.wait_for(Lsn(30)).await.unwrap();
    }

    #[async_test]
    async fn close_after_commits(driver: pal_async::DefaultDriver) {
        use mesh::rpc::RpcSend;

        let (tx, _file, permits, logged_lsn, applied_lsn, _log_task, _apply_task) =
            setup_pipeline(&driver, LOG_SIZE, 100).await;

        for lsn in 1..=5u64 {
            send_commit(&tx, &permits, Lsn(lsn), 1).await;
        }
        logged_lsn.wait_for(Lsn(5)).await.unwrap();

        // Graceful close should wait for all applies and succeed.
        let result = tx.call(LogRequest::<Vec<u8>>::Close, ()).await.unwrap();
        result.unwrap();

        // All commits should be applied.
        assert!(applied_lsn.get() >= Lsn(5));
    }

    #[async_test]
    async fn applied_data_is_at_final_offset(driver: pal_async::DefaultDriver) {
        let (tx, file, permits, logged_lsn, applied_lsn, _log_task, _apply_task) =
            setup_pipeline(&driver, LOG_SIZE, 100).await;

        let target_offset: u64 = 2 * 1024 * 1024; // 2 MiB
        let data = Arc::new(vec![0xAB_u8; LOG_DATA_PAGE_SIZE]);
        permits.acquire(1).await.unwrap();
        tx.send(LogRequest::Commit(Transaction {
            lsn: Lsn(1),
            data: vec![LogData::new(target_offset, data.clone())],
            pre_log_fsn: None,
        }));

        logged_lsn.wait_for(Lsn(1)).await.unwrap();
        applied_lsn.wait_for(Lsn(1)).await.unwrap();

        // Read back from the final offset — should match.
        let mut buf = [0u8; LOG_DATA_PAGE_SIZE];
        file.read_at(target_offset, &mut buf).await.unwrap();
        assert!(buf.iter().all(|&b| b == 0xAB));
    }

    #[async_test]
    async fn apply_write_failure_poisons_pipeline(driver: pal_async::DefaultDriver) {
        use crate::tests::support::IoInterceptor;

        // Interceptor that fails writes only outside the log region
        // (i.e., apply writes to final offsets), not WAL writes.
        struct FailApplyInterceptor {
            fail: std::sync::atomic::AtomicBool,
        }
        impl IoInterceptor for FailApplyInterceptor {
            fn before_write(&self, offset: u64, _data: &[u8]) -> Result<(), std::io::Error> {
                // Log region is at LOG_OFFSET (1 MiB). Apply writes go
                // to 2 MiB+. Only fail writes outside the log region.
                if self.fail.load(std::sync::atomic::Ordering::Relaxed) && offset >= 2 * 1024 * 1024
                {
                    return Err(std::io::Error::other("injected apply write failure"));
                }
                Ok(())
            }
        }

        let interceptor = Arc::new(FailApplyInterceptor {
            fail: std::sync::atomic::AtomicBool::new(false),
        });
        let file = Arc::new(InMemoryFile::with_interceptor(
            4 * 1024 * 1024,
            interceptor.clone() as Arc<dyn IoInterceptor>,
        ));

        let (tx, _file, permits, logged_lsn, _applied_lsn, _log_task, _apply_task) =
            setup_pipeline_with_file(&driver, file, LOG_SIZE, 100).await;

        // First commit succeeds end-to-end.
        send_commit(&tx, &permits, Lsn(1), 1).await;
        logged_lsn.wait_for(Lsn(1)).await.unwrap();

        // Now fail apply writes (but not WAL writes).
        interceptor
            .fail
            .store(true, std::sync::atomic::Ordering::Relaxed);

        // Second commit: WAL write succeeds, but apply write will fail.
        send_commit(&tx, &permits, Lsn(2), 1).await;
        logged_lsn.wait_for(Lsn(2)).await.unwrap();

        // The apply task should have poisoned permits after the write failure.
        // Future permit acquires must fail.
        let result = permits.acquire(1).await;
        assert!(result.is_err(), "acquire should fail after apply error");
    }
}
