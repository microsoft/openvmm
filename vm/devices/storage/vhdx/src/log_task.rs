// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Log task — a single async task that owns all log state and provides
//! crash-consistent metadata persistence.
//!
//! The log task receives [`LogRequest`] messages via a `mesh` channel.
//! [`LogRequest::Commit`] is fire-and-forget: the cache sends a batch
//! of dirty pages and moves on. The log task writes WAL entries and
//! publishes `logged_lsn` without forcing them to stable storage.
//!
//! Once a batch's WAL is durable, the log task sends it to the
//! [apply task](crate::apply_task) for writing to final file offsets.
//! The apply task releases permits and publishes `applied_lsn`, which
//! the log task reads to advance its tail.
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
use futures::FutureExt;
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
struct PendingApply<B> {
    batch: ApplyBatch<B>,
    wal_fsn: Fsn,
}

enum LogEvent<B> {
    Request(LogRequest<B>),
    Closed,
    Durable,
    Pressure,
}

/// Tracks a logged batch whose tail hasn't been advanced yet.
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
    pending_applies: VecDeque<PendingApply<F::Buffer>>,
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
            pending_applies: VecDeque::new(),
            pending_tails: VecDeque::new(),
            failure_flag,
        }
    }

    /// Run the log task main loop.
    ///
    /// Consumes requests from `rx` until a `Close` request is received
    /// or the channel is dropped.
    pub async fn run(mut self, mut rx: mesh::Receiver<LogRequest<F::Buffer>>) {
        if let Err(error) = self.run_inner(&mut rx).await {
            self.fail(&error);
        }
    }

    fn fail(&self, error: &LogTaskError) {
        tracing::error!("VHDX log task fatal error: {error}");
        self.log_permits.fail(error.to_string());
        self.logged_lsn.fail(error.to_string());
        self.applied_lsn.fail(error.to_string());
        self.failure_flag.set(error);
    }

    async fn run_inner(
        &mut self,
        rx: &mut mesh::Receiver<LogRequest<F::Buffer>>,
    ) -> Result<(), LogTaskError> {
        loop {
            self.dispatch_durable()?;
            self.advance_tails();

            let event = {
                let durable = async {
                    if let Some(front) = self.pending_applies.front() {
                        self.flush_sequencer
                            .wait_for(front.wal_fsn)
                            .await
                            .map_err(LogTaskError::PipelineFailed)?;
                    } else {
                        std::future::pending::<()>().await;
                    }
                    Ok::<_, LogTaskError>(LogEvent::Durable)
                }
                .fuse();
                let pressure = async {
                    if self.pending_applies.is_empty() {
                        std::future::pending::<()>().await;
                    }
                    self.log_permits
                        .wait_for_pressure()
                        .await
                        .map_err(LogTaskError::PipelineFailed)?;
                    Ok::<_, LogTaskError>(LogEvent::Pressure)
                }
                .fuse();
                let request = rx.recv().fuse();
                let mut durable = std::pin::pin!(durable);
                let mut pressure = std::pin::pin!(pressure);
                let mut request = std::pin::pin!(request);
                futures::select_biased! {
                    result = durable => result?,
                    result = request => match result {
                        Ok(request) => LogEvent::Request(request),
                        Err(_) => LogEvent::Closed,
                    },
                    result = pressure => result?,
                }
            };

            match event {
                LogEvent::Closed => {
                    tracing::warn!("VHDX log task: channel closed without close() — file is dirty");
                    return Ok(());
                }
                LogEvent::Durable => {}
                LogEvent::Pressure => self.flush_pending_applies().await?,
                LogEvent::Request(LogRequest::Commit(txn)) => {
                    self.handle_commit(txn).await?;
                }
                LogEvent::Request(LogRequest::Close(rpc)) => {
                    rpc.handle(async |()| {
                        let result = self.graceful_close().await;
                        if let Err(error) = &result {
                            self.fail(error);
                        }
                        result
                    })
                    .await;
                    return Ok(());
                }
            }
        }
    }

    fn dispatch_durable(&mut self) -> Result<(), LogTaskError> {
        self.failure_flag
            .check()
            .map_err(|error| LogTaskError::PipelineFailed(PipelineFailed(error.to_string())))?;
        let completed = self.flush_sequencer.completed_fsn();
        while let Some(pending) = self
            .pending_applies
            .pop_front_if(|pending| pending.wal_fsn <= completed)
        {
            self.apply_tx.send(pending.batch);
        }
        Ok(())
    }

    async fn flush_pending_applies(&mut self) -> Result<(), LogTaskError> {
        if let Some(last) = self.pending_applies.back() {
            self.flush_sequencer
                .flush_through(self.file.as_ref(), last.wal_fsn)
                .await
                .map_err(LogTaskError::Flush)?;
        }
        self.dispatch_durable()
    }

    /// Advance the log tail for all batches whose applied data has
    /// been flushed (i.e., `applied_fsn <= completed_fsn`).
    fn advance_tails(&mut self) {
        let flushed_fsn = self.flush_sequencer.completed_fsn();
        let (applied, applied_fsn) = self.applied_lsn.get_with_fsn();
        while let Some(front) = self
            .pending_tails
            .pop_front_if(|front| front.lsn <= applied && applied_fsn <= flushed_fsn)
        {
            self.log_writer.advance_tail(front.new_tail);
        }
    }

    /// Flush applied data and advance tails. Used when the log is full
    /// and we need to reclaim space.
    async fn flush_and_advance_tails(&mut self) -> Result<(), LogTaskError> {
        // The applier cannot make progress until its WAL is durable.
        self.flush_pending_applies().await?;
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

    /// Write a WAL entry, queue its apply, and publish the logged LSN.
    /// If the log is full, flushes and applies preceding batches to reclaim it.
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

        // Multiple entries can share this FSN. Dispatch only after an explicit
        // flush or pressure makes their WAL durable.
        let wal_fsn = self.flush_sequencer.current_fsn();
        let new_tail = self.log_writer.head();
        self.pending_applies.push_back(PendingApply {
            batch: ApplyBatch {
                data: txn.data,
                lsn,
            },
            wal_fsn,
        });
        self.pending_tails.push_back(PendingTail { lsn, new_tail });
        self.logged_lsn.advance(lsn, wal_fsn);
        Ok(())
    }

    /// Graceful close: wait for all applies, flush, advance tails.
    ///
    /// After this returns, the log region is fully drained. The caller
    /// is responsible for clearing the log GUID in the header.
    async fn graceful_close(&mut self) -> Result<(), LogTaskError> {
        self.flush_pending_applies().await?;
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
    use crate::tests::support::CrashTestFile;
    use crate::tests::support::InMemoryFile;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use parking_lot::Mutex;
    use std::borrow::Borrow;
    use std::io;
    use std::pin::pin;
    use test_with_tracing::test;

    const LOG_SIZE: u32 = 64 * 4096; // 256 KiB — deliberately small
    const LOG_OFFSET: u64 = 1024 * 1024; // 1 MiB into the file
    const APPLY_OFFSET: u64 = 2 * 1024 * 1024;
    const DURABILITY_LOG_SIZE: u32 = 8 * LOG_SECTOR_SIZE as u32;

    struct WalTestFile {
        inner: CrashTestFile,
        state: Mutex<WalTestIo>,
    }

    #[derive(Default)]
    struct WalTestIo {
        flush_gate: Option<mesh::OneshotReceiver<io::Result<()>>>,
        write_gate: Option<(u64, mesh::OneshotReceiver<()>)>,
        crash_after_write: Option<u64>,
        crashed: bool,
    }

    #[derive(Default)]
    struct WakeFlag(std::sync::atomic::AtomicBool);

    impl std::task::Wake for WakeFlag {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    impl WalTestFile {
        fn new() -> Self {
            let mut data = vec![0; 4 * 1024 * 1024];
            let start = APPLY_OFFSET as usize;
            data[start..start + LOG_DATA_PAGE_SIZE].fill(0x11);
            data[start + LOG_DATA_PAGE_SIZE..start + 2 * LOG_DATA_PAGE_SIZE].fill(0x22);
            Self::from_durable(data)
        }

        fn from_durable(data: Vec<u8>) -> Self {
            Self {
                inner: CrashTestFile::from_durable(data),
                state: Mutex::new(WalTestIo::default()),
            }
        }

        fn hold_flush(&self) -> mesh::OneshotSender<io::Result<()>> {
            let (tx, rx) = mesh::oneshot();
            assert!(self.state.lock().flush_gate.replace(rx).is_none());
            tx
        }

        fn hold_write(&self, offset: u64) -> mesh::OneshotSender<()> {
            let (tx, rx) = mesh::oneshot();
            assert!(self.state.lock().write_gate.replace((offset, rx)).is_none());
            tx
        }

        fn check_available(&self) -> io::Result<()> {
            if self.state.lock().crashed {
                return Err(io::Error::other("crash: disk unavailable"));
            }
            Ok(())
        }
    }

    impl AsyncFile for WalTestFile {
        type Buffer = Vec<u8>;

        fn alloc_buffer(&self, len: usize) -> Self::Buffer {
            self.inner.alloc_buffer(len)
        }

        async fn read_into(&self, offset: u64, buf: Self::Buffer) -> io::Result<Self::Buffer> {
            self.check_available()?;
            self.inner.read_into(offset, buf).await
        }

        async fn write_from(
            &self,
            offset: u64,
            buf: impl Borrow<Self::Buffer> + Send + 'static,
        ) -> io::Result<()> {
            self.check_available()?;
            let gate = {
                let mut state = self.state.lock();
                if state
                    .write_gate
                    .as_ref()
                    .is_some_and(|(at, _)| *at == offset)
                {
                    state.write_gate.take().map(|(_, gate)| gate)
                } else {
                    None
                }
            };
            if let Some(gate) = gate {
                gate.await.unwrap();
            }
            let len = buf.borrow().len();
            self.inner.write_from(offset, buf).await?;
            let mut state = self.state.lock();
            if state.crash_after_write == Some(offset) {
                // This write reaches disk independently; other unflushed writes
                // (including the WAL) must not hitch a ride into the snapshot.
                self.inner
                    .persist_range(offset as usize..offset as usize + len);
                state.crashed = true;
            }
            Ok(())
        }

        async fn flush(&self) -> io::Result<()> {
            self.check_available()?;
            let gate = self.state.lock().flush_gate.take();
            if let Some(gate) = gate {
                gate.await.unwrap()?;
            }
            self.inner.flush().await
        }

        async fn file_size(&self) -> io::Result<u64> {
            self.check_available()?;
            self.inner.file_size().await
        }

        async fn set_file_size(&self, size: u64) -> io::Result<()> {
            self.check_available()?;
            self.inner.set_file_size(size).await
        }
    }

    struct DurabilityClient {
        file: Arc<WalTestFile>,
        tx: mesh::Sender<LogRequest<Vec<u8>>>,
        flush_sequencer: Arc<FlushSequencer>,
        permits: Arc<LogPermits>,
        logged: Arc<LsnWatermark>,
        applied: Arc<LsnWatermark>,
        failed: Arc<FailureFlag>,
        region: LogRegion,
        guid: guid::Guid,
    }

    impl DurabilityClient {
        async fn commit(&self, txn: Transaction<Vec<u8>>) {
            self.permits
                .acquire(txn.data.iter().map(LogData::page_count).sum())
                .await
                .unwrap();
            self.tx.send(LogRequest::Commit(txn));
        }

        async fn flush(&self, lsn: Lsn) -> io::Result<()> {
            let data_fsn = self.flush_sequencer.current_fsn();
            let wal_fsn = self.logged.wait_for(lsn).await.map_err(io::Error::other)?;
            self.flush_sequencer
                .flush_through(self.file.as_ref(), data_fsn.max(wal_fsn))
                .await
        }

        async fn assert_pages(&self, first: u8, second: u8) {
            let data = self
                .file
                .read_into(APPLY_OFFSET, vec![0; 2 * LOG_DATA_PAGE_SIZE])
                .await
                .unwrap();
            assert!(
                data[..LOG_DATA_PAGE_SIZE].iter().all(|&b| b == first),
                "first destination page differs from {first:#x}"
            );
            assert!(
                data[LOG_DATA_PAGE_SIZE..].iter().all(|&b| b == second),
                "second destination page differs from {second:#x}"
            );
        }
    }

    async fn durability_pipeline(
        wrap: bool,
        log_size: u32,
        permit_count: usize,
    ) -> (
        DurabilityClient,
        impl Future<Output = ()>,
        impl Future<Output = ()>,
    ) {
        let file = Arc::new(WalTestFile::new());
        let region = LogRegion {
            file_offset: LOG_OFFSET,
            length: log_size,
        };
        let guid = guid::Guid::new_random();
        let mut writer =
            LogWriter::initialize(file.as_ref(), region.clone(), guid, 4 * 1024 * 1024)
                .await
                .unwrap();
        if wrap {
            // Empty entries move the next header to the last log sector,
            // so its data sectors must be written at the start of the log.
            while writer.head() != log_size - LOG_SECTOR_SIZE as u32 {
                assert!(
                    writer
                        .write_entry(file.as_ref(), &[], &[])
                        .await
                        .unwrap()
                        .is_some()
                );
            }
            file.flush().await.unwrap();
            writer.advance_tail(writer.head());
        }
        let failed = Arc::new(FailureFlag::new());
        let mut flush_sequencer = FlushSequencer::new();
        flush_sequencer.set_failure_flag(failed.clone());
        let flush_sequencer = Arc::new(flush_sequencer);
        let permits = Arc::new(LogPermits::new(permit_count));
        let logged = Arc::new(LsnWatermark::new());
        let applied = Arc::new(LsnWatermark::new());
        let (tx, rx) = mesh::channel();
        let (apply_tx, apply_rx) = mesh::channel();
        let logger = LogTask::new(
            file.clone(),
            writer,
            flush_sequencer.clone(),
            permits.clone(),
            logged.clone(),
            applied.clone(),
            apply_tx,
            failed.clone(),
        )
        .run(rx);
        let apply = apply_task::run_apply_task(
            apply_rx,
            file.clone(),
            flush_sequencer.clone(),
            applied.clone(),
            permits.clone(),
            failed.clone(),
        );
        (
            DurabilityClient {
                file,
                tx,
                flush_sequencer,
                permits,
                logged,
                applied,
                failed,
                region,
                guid,
            },
            logger,
            apply,
        )
    }

    fn durability_transaction(lsn: Lsn) -> Transaction<Vec<u8>> {
        Transaction {
            lsn,
            data: vec![
                LogData::new(
                    APPLY_OFFSET,
                    Arc::new(vec![0xA0 + lsn.0 as u8; LOG_DATA_PAGE_SIZE]),
                ),
                LogData::new(
                    APPLY_OFFSET + LOG_DATA_PAGE_SIZE as u64,
                    Arc::new(vec![0xB0 + lsn.0 as u8; LOG_DATA_PAGE_SIZE]),
                ),
            ],
            pre_log_fsn: None,
        }
    }

    #[async_test]
    async fn wal_durable_before_apply() {
        check_wal_durable_before_apply(false).await;
    }

    #[async_test]
    async fn wal_durable_before_apply_wrapped() {
        check_wal_durable_before_apply(true).await;
    }

    async fn check_wal_durable_before_apply(wrap: bool) {
        let (client, logger, apply) = durability_pipeline(wrap, DURABILITY_LOG_SIZE, 4).await;
        let mut logger = pin!(logger);
        let mut apply = pin!(apply);

        for n in 1..=2 {
            client.commit(durability_transaction(Lsn(n))).await;
            let finish_flush = client.file.hold_flush();
            let finish_write = (wrap && n == 1).then(|| client.file.hold_write(LOG_OFFSET));
            let mut flush = pin!(client.flush(Lsn(n)));

            if let Some(finish_write) = finish_write {
                assert!(futures::poll!(&mut logger).is_pending());
                assert!(client.file.state.lock().write_gate.is_none());
                assert!(client.file.state.lock().flush_gate.is_some());
                assert!(futures::poll!(&mut apply).is_pending());
                assert_eq!(client.applied.get(), Lsn(n - 1));
                finish_write.send(());
            }

            assert!(futures::poll!(&mut logger).is_pending());
            assert!(futures::poll!(&mut flush).is_pending());
            assert!(futures::poll!(&mut apply).is_pending());
            if n == 1 {
                client.assert_pages(0x11, 0x22).await;
            } else {
                client.assert_pages(0xA1, 0xB1).await;
            }
            assert_eq!(client.applied.get(), Lsn(n - 1));
            assert!(client.file.state.lock().flush_gate.is_none());

            finish_flush.send(Ok(()));
            assert!(futures::poll!(&mut logger).is_pending());
            flush.await.unwrap();
            assert!(futures::poll!(&mut logger).is_pending());
            assert!(futures::poll!(&mut apply).is_pending());
            assert_eq!(client.applied.get(), Lsn(n));
            client.assert_pages(0xA0 + n as u8, 0xB0 + n as u8).await;
        }
    }

    #[async_test]
    async fn wal_waits_for_data_durability_before_logging() {
        let (client, logger, apply) = durability_pipeline(false, DURABILITY_LOG_SIZE, 4).await;
        let mut logger = pin!(logger);
        let mut apply = pin!(apply);
        let file = &client.file;
        let data_offset = 3 * 1024 * 1024;
        let payload = vec![0xCC; LOG_DATA_PAGE_SIZE];
        let before = file.inner.durable_snapshot();
        file.write_at(data_offset, &payload).await.unwrap();
        let mut txn = durability_transaction(Lsn(1));
        txn.pre_log_fsn = Some(client.flush_sequencer.current_fsn());
        client.commit(txn).await;
        let finish_data_flush = file.hold_flush();
        let mut flush = pin!(client.flush(Lsn(1)));

        assert!(futures::poll!(&mut logger).is_pending());
        assert!(futures::poll!(&mut flush).is_pending());
        assert!(file.state.lock().flush_gate.is_none());
        assert!(futures::poll!(&mut apply).is_pending());
        let log_range = LOG_OFFSET as usize..(LOG_OFFSET + DURABILITY_LOG_SIZE as u64) as usize;
        let wal = file
            .read_into(LOG_OFFSET, vec![0; DURABILITY_LOG_SIZE as usize])
            .await
            .unwrap();
        assert!(
            wal == before[log_range.clone()],
            "WAL was written before the data flush completed"
        );
        assert_eq!(file.inner.durable_snapshot(), before);
        assert_eq!(client.applied.get(), Lsn::ZERO);

        let finish_wal_flush = file.hold_flush();
        finish_data_flush.send(Ok(()));
        assert!(futures::poll!(&mut logger).is_pending());
        assert!(futures::poll!(&mut flush).is_pending());
        assert!(file.state.lock().flush_gate.is_none());
        assert!(futures::poll!(&mut apply).is_pending());
        let snapshot = file.inner.durable_snapshot();
        assert_eq!(
            &snapshot[data_offset as usize..data_offset as usize + payload.len()],
            payload
        );
        assert!(
            snapshot[log_range.clone()] == before[log_range.clone()],
            "the data flush must not include subsequent WAL writes"
        );
        let wal = file
            .read_into(LOG_OFFSET, vec![0; DURABILITY_LOG_SIZE as usize])
            .await
            .unwrap();
        assert!(
            wal != before[log_range],
            "the WAL should be written before its flush starts"
        );
        assert_eq!(client.applied.get(), Lsn::ZERO);

        finish_wal_flush.send(Ok(()));
        assert!(futures::poll!(&mut logger).is_pending());
        flush.await.unwrap();
        assert!(futures::poll!(&mut logger).is_pending());
        assert!(futures::poll!(&mut apply).is_pending());
        assert_eq!(client.applied.get(), Lsn(1));
        client.assert_pages(0xA1, 0xB1).await;
    }

    #[async_test]
    async fn wal_flush_failure_prevents_apply() {
        let (client, logger, apply) = durability_pipeline(false, DURABILITY_LOG_SIZE, 4).await;
        let file = &client.file;
        let before = file.inner.durable_snapshot();
        let finish_flush = file.hold_flush();
        client.commit(durability_transaction(Lsn(1))).await;
        let mut logger = pin!(logger);
        let mut flush = pin!(client.flush(Lsn(1)));
        let wake = Arc::new(WakeFlag::default());
        let waker = std::task::Waker::from(wake.clone());
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(futures::poll!(&mut logger).is_pending());
        assert!(futures::poll!(&mut flush).is_pending());
        assert!(file.state.lock().flush_gate.is_none());
        let mut acquire = pin!(client.permits.acquire(3));
        assert!(futures::poll!(&mut acquire).is_pending());
        finish_flush.send(Err(io::Error::other("injected WAL flush failure")));
        let logger_result = logger.as_mut().poll(&mut cx);
        wake.0.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(matches!(
            futures::poll!(&mut flush),
            std::task::Poll::Ready(Err(_))
        ));
        if logger_result.is_pending() {
            assert!(
                wake.0.load(std::sync::atomic::Ordering::Relaxed),
                "flush failure did not wake the log task"
            );
            assert!(futures::poll!(&mut logger).is_ready());
        }
        apply.await;

        assert!(client.failed.check().is_err());
        assert!(client.logged.wait_for(Lsn(1)).await.is_err());
        assert!(acquire.await.is_err());
        assert!(client.permits.acquire(1).await.is_err());
        assert_eq!(client.applied.get(), Lsn::ZERO);
        assert_eq!(file.inner.durable_snapshot(), before);
        let start = APPLY_OFFSET as usize;
        assert_eq!(
            file.read_into(APPLY_OFFSET, vec![0; 2 * LOG_DATA_PAGE_SIZE])
                .await
                .unwrap(),
            &before[start..start + 2 * LOG_DATA_PAGE_SIZE],
        );
    }

    #[async_test]
    async fn wal_replays_after_selectively_persisted_partial_apply() {
        check_wal_replays_after_selectively_persisted_partial_apply(false).await;
    }

    #[async_test]
    async fn wal_replays_after_selectively_persisted_partial_apply_wrapped() {
        check_wal_replays_after_selectively_persisted_partial_apply(true).await;
    }

    async fn check_wal_replays_after_selectively_persisted_partial_apply(wrap: bool) {
        let (client, logger, apply) = durability_pipeline(wrap, DURABILITY_LOG_SIZE, 4).await;
        let file = &client.file;
        let before = file.inner.durable_snapshot();
        file.state.lock().crash_after_write = Some(APPLY_OFFSET);
        client.commit(durability_transaction(Lsn(1))).await;
        let mut logger = pin!(logger);
        let mut apply = pin!(apply);
        assert!(futures::poll!(&mut logger).is_pending());
        // Give an unsafe applier a chance to persist a page before requesting
        // durability. Never repair its crash snapshot with a later flush.
        if futures::poll!(&mut apply).is_pending() {
            client.flush(Lsn(1)).await.unwrap();
            assert!(futures::poll!(&mut logger).is_pending());
            assert!(futures::poll!(&mut apply).is_ready());
        }
        // The first page persists, then the second write fails. No close
        // or additional flush may make the crash snapshot more durable.
        assert!(file.state.lock().crashed);
        assert!(client.failed.check().is_err());
        assert_eq!(client.applied.get(), Lsn::ZERO);

        let snapshot = file.inner.durable_snapshot();
        let start = APPLY_OFFSET as usize;
        assert_eq!(
            &snapshot[start..start + LOG_DATA_PAGE_SIZE],
            vec![0xA1; LOG_DATA_PAGE_SIZE]
        );
        assert_eq!(
            &snapshot[start + LOG_DATA_PAGE_SIZE..start + 2 * LOG_DATA_PAGE_SIZE],
            &before[start + LOG_DATA_PAGE_SIZE..start + 2 * LOG_DATA_PAGE_SIZE]
        );
        let recovered = InMemoryFile::from_snapshot(snapshot);
        let result = crate::log::replay_log(&recovered, &client.region, client.guid)
            .await
            .unwrap();
        assert!(result.replayed);
        let data = recovered
            .read_into(APPLY_OFFSET, vec![0; 2 * LOG_DATA_PAGE_SIZE])
            .await
            .unwrap();
        assert_eq!(&data[..LOG_DATA_PAGE_SIZE], vec![0xA1; LOG_DATA_PAGE_SIZE]);
        assert!(
            data[LOG_DATA_PAGE_SIZE..].iter().all(|&b| b == 0xB1),
            "replay did not recover the unapplied page (wrap={wrap})"
        );
    }

    #[async_test]
    async fn wal_commits_share_one_demand_driven_flush() {
        let (client, logger, apply) = durability_pipeline(false, DURABILITY_LOG_SIZE, 4).await;
        let mut logger = pin!(logger);
        let mut apply = pin!(apply);
        let flushes = client.file.inner.flush_count();
        client.commit(durability_transaction(Lsn(1))).await;
        client.commit(durability_transaction(Lsn(2))).await;
        assert!(futures::poll!(&mut logger).is_pending());
        assert_eq!(client.logged.get(), Lsn(2));
        assert_eq!(
            client.file.inner.flush_count(),
            flushes,
            "logging alone must not force durability"
        );
        assert!(futures::poll!(&mut apply).is_pending());
        client.assert_pages(0x11, 0x22).await;

        let finish_flush = client.file.hold_flush();
        let mut flush = pin!(client.flush(Lsn(2)));
        assert!(futures::poll!(&mut flush).is_pending());
        assert!(client.file.state.lock().flush_gate.is_none());
        let wake = Arc::new(WakeFlag::default());
        let waker = std::task::Waker::from(wake.clone());
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(logger.as_mut().poll(&mut cx).is_pending());
        assert!(futures::poll!(&mut apply).is_pending());
        client.assert_pages(0x11, 0x22).await;
        wake.0.store(false, std::sync::atomic::Ordering::Relaxed);
        finish_flush.send(Ok(()));
        flush.await.unwrap();
        assert!(
            wake.0.load(std::sync::atomic::Ordering::Relaxed),
            "flush completion did not wake the log task without another commit"
        );
        assert!(futures::poll!(&mut logger).is_pending());
        assert!(futures::poll!(&mut apply).is_pending());
        assert_eq!(client.applied.get(), Lsn(2));
        client.assert_pages(0xA2, 0xB2).await;
        assert_eq!(client.permits.available(), 4);
        assert_eq!(client.file.inner.flush_count(), flushes + 1);
    }

    #[async_test]
    async fn wal_caller_reuses_a_covering_flush_while_waiting_for_logging(
        driver: pal_async::DefaultDriver,
    ) {
        use crate::cache::WriteMode;

        let (file, _) = InMemoryFile::create_test_vhdx(crate::format::GB1).await;
        let image = crate::VhdxFile::open(WalTestFile::from_durable(file.snapshot()))
            .writable(&driver)
            .await
            .unwrap();
        {
            let mut page = image
                .cache
                .acquire_write(image.bat_page_key_for_block(0), WriteMode::Modify)
                .await
                .unwrap();
            page[0] = crate::format::BatEntryState::Zero as u8;
        }
        let state = image.log_state.as_ref().unwrap();
        let (log_offset, _) = image.header_state.log_region();
        let finish_write = image.file.hold_write(log_offset + LOG_SECTOR_SIZE);
        {
            let mut flush = pin!(image.flush());
            assert!(futures::poll!(&mut flush).is_pending());
            let lsn = image.cache.commit().unwrap();
            assert!(lsn > Lsn::ZERO);
            finish_write.send(());
            let fsn = state.logged_lsn.wait_for(lsn).await.unwrap();
            state
                .flush_sequencer
                .flush_through(image.file.as_ref(), fsn)
                .await
                .unwrap();
            let flushes = image.file.inner.flush_count();
            flush.await.unwrap();
            assert_eq!(
                image.file.inner.flush_count(),
                flushes,
                "the caller ignored a flush that already covered its payload and WAL"
            );
        }
        image.abort().await;
    }

    #[async_test]
    async fn wal_does_not_apply_after_an_older_inflight_flush() {
        let (client, logger, apply) = durability_pipeline(false, DURABILITY_LOG_SIZE, 4).await;
        let mut logger = pin!(logger);
        let mut apply = pin!(apply);
        let finish_old_flush = client.file.hold_flush();
        let mut old_flush = pin!(client.flush_sequencer.flush(client.file.as_ref()));
        assert!(futures::poll!(&mut old_flush).is_pending());

        let finish_wal_flush = client.file.hold_flush();
        client.commit(durability_transaction(Lsn(1))).await;
        assert!(futures::poll!(&mut logger).is_pending());
        let mut flush = pin!(client.flush(Lsn(1)));
        assert!(futures::poll!(&mut flush).is_pending());

        finish_old_flush.send(Ok(()));
        old_flush.await.unwrap();
        assert!(futures::poll!(&mut logger).is_pending());
        assert!(futures::poll!(&mut apply).is_pending());
        client.assert_pages(0x11, 0x22).await;
        assert_eq!(client.applied.get(), Lsn::ZERO);

        finish_wal_flush.send(Ok(()));
        assert!(futures::poll!(&mut logger).is_pending());
        flush.await.unwrap();
        assert!(futures::poll!(&mut logger).is_pending());
        assert!(futures::poll!(&mut apply).is_pending());
        assert_eq!(client.applied.get(), Lsn(1));
        client.assert_pages(0xA1, 0xB1).await;
    }

    #[async_test]
    async fn wal_permit_pressure_with_nonzero_available_makes_progress() {
        check_wal_permit_pressure(false).await;
    }

    #[async_test]
    async fn wal_permit_pressure_before_commit_is_not_lost() {
        check_wal_permit_pressure(true).await;
    }

    async fn check_wal_permit_pressure(pressure_first: bool) {
        let (client, logger, apply) = durability_pipeline(false, DURABILITY_LOG_SIZE, 4).await;
        let mut logger = pin!(logger);
        let mut apply = pin!(apply);
        let wake = Arc::new(WakeFlag::default());
        let waker = std::task::Waker::from(wake.clone());
        let mut cx = std::task::Context::from_waker(&waker);
        client.permits.acquire(2).await.unwrap();
        if !pressure_first {
            client
                .tx
                .send(LogRequest::Commit(durability_transaction(Lsn(1))));
            assert!(logger.as_mut().poll(&mut cx).is_pending());
        }

        // Two permits remain, but this request needs three. Neither the log
        // nor the permit pool is full, so only actual acquisition pressure
        // can drive the deferred WAL to disk.
        assert_eq!(client.permits.available(), 2);
        wake.0.store(false, std::sync::atomic::Ordering::Relaxed);
        let mut acquire = pin!(client.permits.acquire(3));
        assert!(futures::poll!(&mut acquire).is_pending());
        if !pressure_first {
            assert!(
                wake.0.load(std::sync::atomic::Ordering::Relaxed),
                "blocked permit acquisition did not wake the log task"
            );
        }
        if pressure_first {
            assert!(futures::poll!(&mut logger).is_pending());
            client
                .tx
                .send(LogRequest::Commit(durability_transaction(Lsn(1))));
        }
        assert!(futures::poll!(&mut logger).is_pending());
        assert!(futures::poll!(&mut apply).is_pending());
        assert!(
            matches!(futures::poll!(&mut acquire), std::task::Poll::Ready(Ok(()))),
            "permit pressure did not make progress without a caller flush"
        );
        assert_eq!(client.applied.get(), Lsn(1));
        client.assert_pages(0xA1, 0xB1).await;
        client.permits.release(3);
    }

    #[async_test]
    async fn wal_cancelled_permit_pressure_does_not_flush() {
        let (client, logger, apply) = durability_pipeline(false, DURABILITY_LOG_SIZE, 4).await;
        let mut logger = pin!(logger);
        let mut apply = pin!(apply);
        let flushes = client.file.inner.flush_count();
        client.commit(durability_transaction(Lsn(1))).await;
        {
            let mut acquire = pin!(client.permits.acquire(3));
            assert!(futures::poll!(&mut acquire).is_pending());
        }
        assert!(futures::poll!(&mut logger).is_pending());
        assert_eq!(client.logged.get(), Lsn(1));
        assert!(futures::poll!(&mut apply).is_pending());
        assert_eq!(
            client.file.inner.flush_count(),
            flushes,
            "cancelled demand must not leave a pressure flush outstanding"
        );
        client.assert_pages(0x11, 0x22).await;
    }

    #[async_test]
    async fn wal_log_full_flushes_before_waiting_for_apply() {
        let (client, logger, apply) = durability_pipeline(false, DURABILITY_LOG_SIZE, 8).await;
        let mut logger = pin!(logger);
        let mut apply = pin!(apply);
        let flushes = client.file.inner.flush_count();
        for n in 1..=3 {
            let mut txn = durability_transaction(Lsn(n));
            txn.data = txn
                .data
                .into_iter()
                .map(|entry| {
                    let (offset, data) = entry.into_parts();
                    LogData::new(offset + (n - 1) * 2 * LOG_DATA_PAGE_SIZE as u64, data)
                })
                .collect();
            client.commit(txn).await;
        }
        // Only two entries fit. The third must flush the preceding WAL,
        // dispatch it, and wait for application before reclaiming space.
        assert!(futures::poll!(&mut logger).is_pending());
        assert_eq!(client.logged.get(), Lsn(2));
        assert!(futures::poll!(&mut apply).is_pending());
        assert_eq!(client.applied.get(), Lsn(2));
        assert!(futures::poll!(&mut logger).is_pending());
        assert_eq!(client.logged.get(), Lsn(3));
        assert!(futures::poll!(&mut apply).is_pending());
        assert_eq!(
            client.applied.get(),
            Lsn(2),
            "the new entry must remain deferred after reclaiming log space"
        );
        assert_eq!(client.file.inner.flush_count(), flushes + 2);

        client.flush(Lsn(3)).await.unwrap();
        assert!(futures::poll!(&mut logger).is_pending());
        assert!(futures::poll!(&mut apply).is_pending());
        assert_eq!(client.applied.get(), Lsn(3));
        let recovered = InMemoryFile::from_snapshot(client.file.inner.durable_snapshot());
        crate::log::replay_log(&recovered, &client.region, client.guid)
            .await
            .unwrap();
        let data = recovered
            .read_into(APPLY_OFFSET, vec![0; 6 * LOG_DATA_PAGE_SIZE])
            .await
            .unwrap();
        for (i, page) in data.chunks_exact(LOG_DATA_PAGE_SIZE).enumerate() {
            let expected = if i % 2 == 0 { 0xA1 } else { 0xB1 } + (i / 2) as u8;
            assert!(
                page.iter().all(|&b| b == expected),
                "reclaiming the log lost destination page {i}"
            );
        }
    }

    #[async_test]
    async fn wal_close_groups_pending_entries_and_drains_apply() {
        use mesh::rpc::RpcSend;

        let (client, logger, apply) = durability_pipeline(false, DURABILITY_LOG_SIZE, 4).await;
        let mut logger = pin!(logger);
        let mut apply = pin!(apply);
        let flushes = client.file.inner.flush_count();
        client.commit(durability_transaction(Lsn(1))).await;
        client.commit(durability_transaction(Lsn(2))).await;
        assert!(futures::poll!(&mut logger).is_pending());

        let mut close = pin!(client.tx.call(LogRequest::Close, ()));
        assert!(futures::poll!(&mut close).is_pending());
        assert!(futures::poll!(&mut logger).is_pending());
        assert!(futures::poll!(&mut apply).is_pending());
        assert!(futures::poll!(&mut logger).is_ready());
        close.await.unwrap().unwrap();
        assert!(futures::poll!(&mut apply).is_ready());
        assert_eq!(client.applied.get(), Lsn(2));
        assert_eq!(
            client.file.inner.flush_count(),
            flushes + 2,
            "close needs one grouped WAL flush and one applied-data flush"
        );
        let snapshot = client.file.inner.durable_snapshot();
        let start = APPLY_OFFSET as usize;
        assert!(
            snapshot[start..start + LOG_DATA_PAGE_SIZE]
                .iter()
                .all(|&b| b == 0xA2)
        );
        assert!(
            snapshot[start + LOG_DATA_PAGE_SIZE..start + 2 * LOG_DATA_PAGE_SIZE]
                .iter()
                .all(|&b| b == 0xB2)
        );
    }

    #[async_test]
    async fn wal_close_reports_flush_failure_without_applying() {
        use mesh::rpc::RpcSend;

        let (client, logger, apply) = durability_pipeline(false, DURABILITY_LOG_SIZE, 4).await;
        let mut logger = pin!(logger);
        let mut apply = pin!(apply);
        client.commit(durability_transaction(Lsn(1))).await;
        assert!(futures::poll!(&mut logger).is_pending());
        let finish = client.file.hold_flush();
        let mut close = pin!(client.tx.call(LogRequest::Close, ()));
        assert!(futures::poll!(&mut close).is_pending());
        assert!(futures::poll!(&mut logger).is_pending());
        finish.send(Err(io::Error::other("injected close flush failure")));
        assert!(futures::poll!(&mut logger).is_ready());
        assert!(close.await.unwrap().is_err());
        assert!(futures::poll!(&mut apply).is_ready());
        assert!(client.failed.check().is_err());
        assert!(client.logged.wait_for(Lsn(1)).await.is_err());
        assert!(client.permits.acquire(1).await.is_err());
        client.assert_pages(0x11, 0x22).await;
    }

    #[async_test]
    async fn wal_pressure_flush_failure_wakes_blocked_writer() {
        let (client, logger, apply) = durability_pipeline(false, DURABILITY_LOG_SIZE, 4).await;
        let mut logger = pin!(logger);
        let mut apply = pin!(apply);
        client.commit(durability_transaction(Lsn(1))).await;
        assert!(futures::poll!(&mut logger).is_pending());
        let mut acquire = pin!(client.permits.acquire(3));
        assert!(futures::poll!(&mut acquire).is_pending());
        let finish = client.file.hold_flush();
        assert!(futures::poll!(&mut logger).is_pending());
        finish.send(Err(io::Error::other("injected pressure flush failure")));
        assert!(futures::poll!(&mut logger).is_ready());
        assert!(futures::poll!(&mut apply).is_ready());
        assert!(matches!(
            futures::poll!(&mut acquire),
            std::task::Poll::Ready(Err(_))
        ));
        assert!(client.failed.check().is_err());
        assert_eq!(client.flush_sequencer.completed_fsn(), Fsn::ZERO);
        client.assert_pages(0x11, 0x22).await;
    }

    #[async_test]
    async fn wal_abort_discards_unflushed_entries_without_io() {
        let (client, logger, apply) = durability_pipeline(false, DURABILITY_LOG_SIZE, 4).await;
        let mut logger = pin!(logger);
        let mut apply = pin!(apply);
        let file = client.file.clone();
        let before = file.inner.durable_snapshot();
        let flushes = file.inner.flush_count();
        client.commit(durability_transaction(Lsn(1))).await;
        assert!(futures::poll!(&mut logger).is_pending());
        assert_eq!(client.logged.get(), Lsn(1));
        drop(client);

        assert!(futures::poll!(&mut logger).is_ready());
        assert!(
            futures::poll!(&mut apply).is_ready(),
            "abort must not wait for an unissued durability barrier"
        );
        assert_eq!(file.inner.flush_count(), flushes);
        assert_eq!(file.inner.durable_snapshot(), before);
        let start = APPLY_OFFSET as usize;
        assert!(
            file.read_into(APPLY_OFFSET, vec![0; 2 * LOG_DATA_PAGE_SIZE])
                .await
                .unwrap()
                == before[start..start + 2 * LOG_DATA_PAGE_SIZE],
            "abort dispatched an unflushed batch"
        );
    }

    #[async_test]
    async fn wal_cache_batch_and_permit_pressure_use_distinct_pages() {
        use crate::cache::{CacheLogState, MAX_COMMIT_PAGES, PageCache, PageKey, WriteMode};

        let permit_count = MAX_COMMIT_PAGES + 4;
        let (client, logger, apply) = durability_pipeline(false, 2 * LOG_SIZE, permit_count).await;
        let mut logger = pin!(logger);
        let mut apply = pin!(apply);
        let mut cache = PageCache::new(
            client.file.clone(),
            Some(LogClient::new(client.tx.clone())),
            Some(CacheLogState {
                permits: client.permits.clone(),
                applied_lsn: client.applied.clone(),
            }),
            0,
        );
        cache.register_tag(0, APPLY_OFFSET);
        for i in 0..permit_count {
            let mut page = cache
                .acquire_write(
                    PageKey {
                        tag: 0,
                        offset: (i * LOG_DATA_PAGE_SIZE) as u64,
                    },
                    WriteMode::Overwrite,
                )
                .await
                .unwrap();
            page.fill((i + 1) as u8);
        }
        assert_eq!(client.permits.available(), 0);
        let mut acquire = pin!(cache.acquire_write(
            PageKey {
                tag: 0,
                offset: (permit_count * LOG_DATA_PAGE_SIZE) as u64,
            },
            WriteMode::Overwrite,
        ));
        assert!(futures::poll!(&mut acquire).is_pending());
        assert!(futures::poll!(&mut logger).is_pending());
        assert_eq!(client.logged.get(), Lsn(1), "batch-full commit did not run");
        assert!(futures::poll!(&mut apply).is_pending());
        let mut page = match futures::poll!(&mut acquire) {
            std::task::Poll::Ready(Ok(page)) => page,
            _ => panic!("cache writer remained blocked despite permit pressure"),
        };
        page.fill((permit_count + 1) as u8);
        drop(page);
        let lsn = cache.commit().unwrap();
        assert_eq!(lsn, Lsn(2));
        assert!(futures::poll!(&mut logger).is_pending());
        client.flush(lsn).await.unwrap();
        assert!(futures::poll!(&mut logger).is_pending());
        assert!(futures::poll!(&mut apply).is_pending());
        assert_eq!(client.applied.get(), lsn);
        assert_eq!(client.permits.available(), permit_count);

        let recovered = InMemoryFile::from_snapshot(client.file.inner.durable_snapshot());
        crate::log::replay_log(&recovered, &client.region, client.guid)
            .await
            .unwrap();
        let data = recovered
            .read_into(
                APPLY_OFFSET,
                vec![0; (permit_count + 1) * LOG_DATA_PAGE_SIZE],
            )
            .await
            .unwrap();
        for (i, page) in data.chunks_exact(LOG_DATA_PAGE_SIZE).enumerate() {
            assert!(
                page.iter().all(|&b| b == (i + 1) as u8),
                "page {i} was lost across batch-full commit and pressure"
            );
        }
    }

    /// Set up a log task + apply task connected via channels.
    /// Returns (log_tx, file, permits, logged_lsn, applied_lsn,
    /// log_task_handle, apply_task_handle, flush_sequencer).
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
        Arc<FlushSequencer>,
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
        Arc<FlushSequencer>,
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
                flush_sequencer.clone(),
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
            flush_sequencer,
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
        let (
            tx,
            _file,
            permits,
            logged_lsn,
            _applied_lsn,
            _log_task,
            _apply_task,
            _flush_sequencer,
        ) = setup_pipeline(&driver, LOG_SIZE, 100).await;

        send_commit(&tx, &permits, Lsn(1), 1).await;
        logged_lsn.wait_for(Lsn(1)).await.unwrap();
    }

    #[async_test]
    async fn permits_return_after_apply(driver: pal_async::DefaultDriver) {
        let permit_count = 10;
        let (tx, file, permits, logged_lsn, applied_lsn, _log_task, _apply_task, flush_sequencer) =
            setup_pipeline(&driver, LOG_SIZE, permit_count).await;

        // Send a commit of 5 pages (acquires 5 permits).
        send_commit(&tx, &permits, Lsn(1), 5).await;

        // Wait for the apply task to finish.
        let fsn = logged_lsn.wait_for(Lsn(1)).await.unwrap();
        flush_sequencer
            .flush_through(file.as_ref(), fsn)
            .await
            .unwrap();
        applied_lsn.wait_for(Lsn(1)).await.unwrap();

        // The apply task should have released 5 permits.
        // All 10 should be available again.
        assert_eq!(permits.available(), permit_count);
    }

    #[async_test]
    async fn multiple_commits_sequential(driver: pal_async::DefaultDriver) {
        let (
            tx,
            _file,
            permits,
            logged_lsn,
            _applied_lsn,
            _log_task,
            _apply_task,
            _flush_sequencer,
        ) = setup_pipeline(&driver, LOG_SIZE, 100).await;

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
        let (
            tx,
            _file,
            permits,
            logged_lsn,
            _applied_lsn,
            _log_task,
            _apply_task,
            _flush_sequencer,
        ) = setup_pipeline(&driver, LOG_SIZE, 500).await;

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
        let (
            tx,
            _file,
            permits,
            logged_lsn,
            _applied_lsn,
            _log_task,
            _apply_task,
            _flush_sequencer,
        ) = setup_pipeline(&driver, LOG_SIZE, 500).await;

        for lsn in 1..=30u64 {
            send_commit(&tx, &permits, Lsn(lsn), 5).await;
        }

        logged_lsn.wait_for(Lsn(30)).await.unwrap();
    }

    #[async_test]
    async fn close_after_commits(driver: pal_async::DefaultDriver) {
        use mesh::rpc::RpcSend;

        let (tx, _file, permits, logged_lsn, applied_lsn, _log_task, _apply_task, _flush_sequencer) =
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
        let (tx, file, permits, logged_lsn, applied_lsn, _log_task, _apply_task, flush_sequencer) =
            setup_pipeline(&driver, LOG_SIZE, 100).await;

        let target_offset: u64 = 2 * 1024 * 1024; // 2 MiB
        let data = Arc::new(vec![0xAB_u8; LOG_DATA_PAGE_SIZE]);
        permits.acquire(1).await.unwrap();
        tx.send(LogRequest::Commit(Transaction {
            lsn: Lsn(1),
            data: vec![LogData::new(target_offset, data.clone())],
            pre_log_fsn: None,
        }));

        let fsn = logged_lsn.wait_for(Lsn(1)).await.unwrap();
        flush_sequencer
            .flush_through(file.as_ref(), fsn)
            .await
            .unwrap();
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
            fn before_write(&self, offset: u64, _data: &[u8]) -> Result<(), io::Error> {
                // Log region is at LOG_OFFSET (1 MiB). Apply writes go
                // to 2 MiB+. Only fail writes outside the log region.
                if self.fail.load(std::sync::atomic::Ordering::Relaxed) && offset >= 2 * 1024 * 1024
                {
                    return Err(io::Error::other("injected apply write failure"));
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

        let (tx, file, permits, logged_lsn, applied_lsn, _log_task, _apply_task, flush_sequencer) =
            setup_pipeline_with_file(&driver, file, LOG_SIZE, 100).await;

        // First commit succeeds end-to-end.
        send_commit(&tx, &permits, Lsn(1), 1).await;
        let fsn = logged_lsn.wait_for(Lsn(1)).await.unwrap();
        flush_sequencer
            .flush_through(file.as_ref(), fsn)
            .await
            .unwrap();
        applied_lsn.wait_for(Lsn(1)).await.unwrap();

        // Now fail apply writes (but not WAL writes).
        interceptor
            .fail
            .store(true, std::sync::atomic::Ordering::Relaxed);

        // Second commit: WAL write succeeds, but apply write will fail.
        send_commit(&tx, &permits, Lsn(2), 1).await;
        let fsn = logged_lsn.wait_for(Lsn(2)).await.unwrap();
        flush_sequencer
            .flush_through(file.as_ref(), fsn)
            .await
            .unwrap();

        // The apply task should have poisoned permits after the write failure.
        // Future permit acquires must fail.
        assert!(applied_lsn.wait_for(Lsn(2)).await.is_err());
        let result = permits.acquire(1).await;
        assert!(result.is_err(), "acquire should fail after apply error");
    }
}
