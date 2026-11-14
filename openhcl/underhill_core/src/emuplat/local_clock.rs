// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest RTC updates commonly write several time registers in a batch. Apply
//! each offset change immediately in memory, but coalesce VMGS persistence with
//! a fixed one-second timer so those writes do not each block on storage I/O.
//! Later updates replace the pending offset without extending the deadline,
//! preventing continuous writes from postponing persistence indefinitely.
//! Failed writes are retried without discarding newer updates. Servicing and
//! orderly teardown flush pending writes; restore queues the saved offset again.
//! Abrupt termination can lose updates that have not yet been persisted.

use self::host_time::HostSystemTimeAccess;
use cvm_tracing::CVM_ALLOWED;
use futures::FutureExt;
use futures::StreamExt;
use inspect::Inspect;
use local_clock::LocalClock;
use local_clock::LocalClockDelta;
use local_clock::LocalClockTime;
use pal_async::driver::Driver;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::timer::Instant;
use pal_async::timer::PolledTimer;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use vmcore::non_volatile_store::NonVolatileStore;
use vmcore::non_volatile_store::NonVolatileStoreError;
use vmcore::save_restore::SaveRestore;

const MILLIS_IN_TWO_DAYS: i64 = 1000 * 60 * 60 * 24 * 2;
const RTC_SKEW_PERSIST_DELAY: Duration = Duration::from_secs(1);

/// Implementation of [`LocalClock`], backed a real time source on the host.
///
/// The linux kernel in VTL2 doesn't (currently) have any native way to track
/// "real time" outside of VTL2, and as such, Underhill is forced to query the
/// host whenever it needs to check the real time.
///
/// DEVNOTE: If VTL2 gains some kind of "notification on resume" functionality,
/// it should be possible to avoid querying the host on each `get_time` call,
/// and instead use VTL2-local time keeping facilities to track deltas from a
/// single host time query.
#[derive(Inspect)]
pub struct UnderhillLocalClock {
    persistence: SkewPersistence,
    host_time: HostSystemTimeAccess,
    offset_from_host_time: LocalClockDelta,
}

impl std::fmt::Debug for UnderhillLocalClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            persistence: _,
            host_time,
            offset_from_host_time,
        } = self;

        f.debug_struct("UnderhillLocalClock")
            .field("host_time", host_time)
            .field("offset_from_host_time", offset_from_host_time)
            .finish()
    }
}

impl UnderhillLocalClock {
    /// Create a new [`UnderhillLocalClock`]. Resolves immediately if provided
    /// with `saved_state`.
    pub async fn new(
        driver: &(impl Driver + Spawn),
        get: guest_emulation_transport::GuestEmulationTransportClient,
        mut store: Box<dyn NonVolatileStore>,
        saved_state: Option<<Self as SaveRestore>::SavedState>,
    ) -> anyhow::Result<Self> {
        let host_time = HostSystemTimeAccess::new(get);
        let restored = saved_state.is_some();
        let mut offset_from_host_time = match saved_state {
            Some(state) => LocalClockDelta::from_millis(state.offset_from_host_time_millis),
            None => {
                match fetch_skew_from_store(&mut store).await? {
                    Some(skew) => skew,
                    None => {
                        // If there is no existing host time offset, default to using
                        // the host's provided timezone offset.
                        //
                        // Hosts _could_ choose to pass time as UTC, but in Hyper-V,
                        // this is set to the host's _local_ time, as this allows
                        // Windows guests to report the correct time on first boot.
                        // Windows assumes the time stored in the RTC is the machine's
                        // _local_ time, whereas Linux assume the time stored in the RTC
                        // stores UTC.
                        let skew = host_time.now().offset();
                        let skew = LocalClockDelta::from_millis(skew.seconds() as i64 * 1000);
                        tracing::info!(
                            CVM_ALLOWED,
                            ?skew,
                            "no saved skew found: defaulting to host local time"
                        );
                        skew
                    }
                }
            }
        };

        // prevent guests from persisting an RTC time in the distant past,
        // which could be used to circumvent time-based licensing checks.
        let neg_two_days = LocalClockDelta::from_millis(-MILLIS_IN_TWO_DAYS);
        if offset_from_host_time < neg_two_days {
            offset_from_host_time = neg_two_days;
            tracing::warn!(
                CVM_ALLOWED,
                "Guest time was more than two days in the past."
            );
        }

        let mut this = Self {
            persistence: SkewPersistence::new(driver, store),
            host_time,
            offset_from_host_time,
        };
        if restored {
            this.persistence.queue(offset_from_host_time);
        }
        Ok(this)
    }

    pub fn flush(&self) -> impl Future<Output = anyhow::Result<()>> + Send + 'static {
        self.persistence.flush()
    }
}

async fn fetch_skew_from_store(
    store: &mut dyn NonVolatileStore,
) -> Result<Option<LocalClockDelta>, NonVolatileStoreError> {
    let raw_skew = match store.restore().await? {
        Some(x) => x,
        None => return Ok(None),
    };

    let raw_skew_100ns = i64::from_le_bytes(raw_skew.try_into().expect("invalid stored RTC skew"));
    let skew_millis = raw_skew_100ns / 10_000; // 100ns units to millis
    let skew = LocalClockDelta::from_millis(skew_millis);
    tracing::info!(CVM_ALLOWED, ?skew, "restored existing RTC skew");
    Ok(Some(skew))
}

impl LocalClock for UnderhillLocalClock {
    fn get_time(&mut self) -> LocalClockTime {
        LocalClockTime::from(self.host_time.now()) + self.offset_from_host_time
    }

    fn set_time(&mut self, new_time: LocalClockTime) {
        let new_skew = new_time - LocalClockTime::from(self.host_time.now());
        self.offset_from_host_time = new_skew;
        self.persistence.queue(new_skew);
    }
}

#[derive(Inspect)]
struct SkewPersistence {
    #[inspect(skip)]
    pending: Arc<Mutex<Option<LocalClockDelta>>>,
    #[inspect(skip)]
    requests: mesh::Sender<PersistenceRequest>,
    #[inspect(skip)]
    _task: Task<()>,
}

enum PersistenceRequest {
    Dirty,
    Flush(mesh::OneshotSender<Result<(), NonVolatileStoreError>>),
}

impl SkewPersistence {
    fn new(driver: &(impl Driver + Spawn), store: Box<dyn NonVolatileStore>) -> Self {
        let pending = Arc::new(Mutex::new(None));
        let (requests, recv) = mesh::channel();
        let task = driver.spawn(
            "RTC skew persistence",
            persist_skew_worker(store, pending.clone(), recv, PolledTimer::new(driver)),
        );
        Self {
            pending,
            requests,
            _task: task,
        }
    }

    fn queue(&mut self, skew: LocalClockDelta) {
        let first = self.pending.lock().replace(skew).is_none();
        if first {
            self.requests.send(PersistenceRequest::Dirty);
        }
    }

    fn flush(&self) -> impl Future<Output = anyhow::Result<()>> + Send + 'static {
        let requests = self.requests.clone();
        async move {
            let (send, recv) = mesh::oneshot();
            requests.send(PersistenceRequest::Flush(send));
            recv.await??;
            Ok(())
        }
    }
}

async fn persist_skew_worker(
    mut store: Box<dyn NonVolatileStore>,
    pending: Arc<Mutex<Option<LocalClockDelta>>>,
    mut requests: mesh::Receiver<PersistenceRequest>,
    mut timer: PolledTimer,
) {
    let mut deadline = None;
    loop {
        let completion = futures::select_biased! {
            _ = async {
                match deadline {
                    Some(deadline) => { timer.sleep_until(deadline).await; }
                    None => std::future::pending::<()>().await,
                }
            }.fuse() => None,
            request = requests.next().fuse() => match request {
                Some(PersistenceRequest::Dirty) => {
                    if pending.lock().is_some() {
                        deadline.get_or_insert_with(|| Instant::now() + RTC_SKEW_PERSIST_DELAY);
                    }
                    continue;
                }
                Some(PersistenceRequest::Flush(completion)) => Some(completion),
                None => return,
            },
        };
        let result = persist_pending_skew(&mut *store, &pending).await;
        if let Err(err) = &result {
            tracelimit::error_ratelimited!(
                CVM_ALLOWED,
                err = err as &dyn std::error::Error,
                "failed to persist RTC skew"
            );
        }
        if let Some(completion) = completion {
            completion.send(result);
        }
        deadline = pending
            .lock()
            .is_some()
            .then(|| Instant::now() + RTC_SKEW_PERSIST_DELAY);
    }
}

async fn persist_pending_skew(
    store: &mut dyn NonVolatileStore,
    pending: &Mutex<Option<LocalClockDelta>>,
) -> Result<(), NonVolatileStoreError> {
    let Some(skew) = pending.lock().take() else {
        return Ok(());
    };
    let raw_skew = skew.as_millis() * 10_000;
    if let Err(err) = store.persist(raw_skew.to_le_bytes().into()).await {
        pending.lock().get_or_insert(skew);
        return Err(err);
    }
    Ok(())
}

mod host_time {
    use inspect::Inspect;
    use jiff::Zoned;
    use parking_lot::Mutex;
    use std::time::Duration;
    use std::time::Instant;

    /// Encapsulates all the nitty-gritty details of how real time gets fetched
    /// from the Host.
    #[derive(Debug)]
    pub struct HostSystemTimeAccess {
        get: guest_emulation_transport::GuestEmulationTransportClient,
        cached_host_time: Mutex<Option<(Instant, Zoned)>>,
    }

    impl Inspect for HostSystemTimeAccess {
        fn inspect(&self, req: inspect::Request<'_>) {
            let HostSystemTimeAccess {
                get: _,
                cached_host_time,
            } = self;

            let mut res = req.respond();

            if let Some((last_query, ref cached_time)) = *cached_host_time.lock() {
                res.display_debug("since_last_query", &(Instant::now() - last_query))
                    .display("cached_time", cached_time);
            }
        }
    }

    impl HostSystemTimeAccess {
        pub fn new(
            get: guest_emulation_transport::GuestEmulationTransportClient,
        ) -> HostSystemTimeAccess {
            HostSystemTimeAccess {
                get,
                cached_host_time: Mutex::new(None),
            }
        }

        /// Return the host's current time
        pub fn now(&self) -> Zoned {
            // The RTC only has 1s time granularity, so there's no reason to
            // spam the GET with time requests if the previous request was less
            // than a second ago.
            //
            // TODO: if the GET was updated to include a "on VTL2 resume"
            // packet, we could hook into that notification to avoid having to
            // constantly query the host over the GET to get current time (using
            // VTL2 local time-keeping to maintain a delta since last host
            // query).
            //
            // ...but this is fine for now.
            let now = Instant::now();
            let mut cached_host_time = self.cached_host_time.lock();

            match &*cached_host_time {
                Some((last_query, cached_time))
                    if now.duration_since(*last_query) < Duration::from_secs(1) =>
                {
                    cached_time.clone()
                }
                _ => {
                    // TODO: this block_on really ain't great, but since we're
                    // not hammering the GET on _each_ access, it's okay for now...
                    let new_time =
                        pal_async::local::block_with_io(|_| self.get.host_time()).to_jiff();
                    let result = new_time.clone();
                    *cached_host_time = Some((now, new_time));
                    result
                }
            }
        }
    }
}

#[derive(Debug, Inspect)]
#[inspect(transparent)]
pub struct ArcMutexUnderhillLocalClock(pub Arc<Mutex<UnderhillLocalClock>>);

impl ArcMutexUnderhillLocalClock {
    /// Creates a new clock that is backed by the same time source.
    ///
    /// It is appropriate to use this method if the system is expected to have
    /// one time source / RTC device, like a normal physical machine. It would
    /// not be appropriate to use this method if there are multiple independent
    /// time sources in the system. The VMGS file can only store the state for
    /// one time source, so the time sources would trample each other without
    /// extending the VMGS file or saving a second one.
    pub fn new_linked_clock(&self) -> Self {
        ArcMutexUnderhillLocalClock(self.0.clone())
    }
}

// required for emuplat servicing optimization
impl LocalClock for ArcMutexUnderhillLocalClock {
    fn get_time(&mut self) -> LocalClockTime {
        self.0.lock().get_time()
    }

    fn set_time(&mut self, new_time: LocalClockTime) {
        self.0.lock().set_time(new_time)
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "underhill.emuplat.local_clock")]
        pub struct SavedState {
            #[mesh(1)]
            pub offset_from_host_time_millis: i64,
        }
    }

    impl SaveRestore for UnderhillLocalClock {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(state::SavedState {
                offset_from_host_time_millis: self.offset_from_host_time.as_millis(),
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                offset_from_host_time_millis,
            } = state;

            self.offset_from_host_time = LocalClockDelta::from_millis(offset_from_host_time_millis);
            self.persistence.queue(self.offset_from_host_time);

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use test_with_tracing::test;

    struct RecordingStore {
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        completion: Option<mesh::OneshotReceiver<bool>>,
        attempts: Option<mesh::Sender<Vec<u8>>>,
    }

    #[async_trait::async_trait]
    impl NonVolatileStore for RecordingStore {
        async fn persist(&mut self, data: Vec<u8>) -> Result<(), NonVolatileStoreError> {
            self.writes.lock().push(data.clone());
            if let Some(attempts) = &self.attempts {
                attempts.send(data);
            }
            if let Some(completion) = self.completion.take() {
                if !completion.await.unwrap() {
                    return Err(NonVolatileStoreError::new(std::io::Error::other(
                        "test persistence failure",
                    )));
                }
            }
            Ok(())
        }

        async fn restore(&mut self) -> Result<Option<Vec<u8>>, NonVolatileStoreError> {
            Ok(None)
        }
    }

    #[test]
    fn pending_skew_coalesces_writes() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mut store = RecordingStore {
            writes: writes.clone(),
            completion: None,
            attempts: None,
        };
        let pending = Mutex::new(None);
        for millis in [100, 200, 300] {
            *pending.lock() = Some(LocalClockDelta::from_millis(millis));
        }

        futures::executor::block_on(persist_pending_skew(&mut store, &pending)).unwrap();
        futures::executor::block_on(persist_pending_skew(&mut store, &pending)).unwrap();
        assert_eq!(*writes.lock(), [3_000_000i64.to_le_bytes().to_vec()]);
        assert!(pending.lock().is_none());
    }

    #[test]
    fn pending_skew_preserves_writes_during_persistence() {
        for success in [true, false] {
            let writes = Arc::new(Mutex::new(Vec::new()));
            let (complete, completion) = mesh::oneshot();
            let mut store = RecordingStore {
                writes: writes.clone(),
                completion: Some(completion),
                attempts: None,
            };
            let pending = Mutex::new(Some(LocalClockDelta::from_millis(100)));
            let mut persist = std::pin::pin!(persist_pending_skew(&mut store, &pending));
            assert!(persist.as_mut().now_or_never().is_none());
            *pending.lock() = Some(LocalClockDelta::from_millis(200));
            complete.send(success);
            assert_eq!(futures::executor::block_on(persist).is_ok(), success);
            assert_eq!(*pending.lock(), Some(LocalClockDelta::from_millis(200)));
            assert_eq!(*writes.lock(), [1_000_000i64.to_le_bytes().to_vec()]);
        }
    }

    #[test]
    fn pending_skew_retries_failed_persistence() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let (complete, completion) = mesh::oneshot();
        let mut store = RecordingStore {
            writes: writes.clone(),
            completion: Some(completion),
            attempts: None,
        };
        let skew = LocalClockDelta::from_millis(100);
        let pending = Mutex::new(Some(skew));
        complete.send(false);
        assert!(futures::executor::block_on(persist_pending_skew(&mut store, &pending)).is_err());
        assert_eq!(*pending.lock(), Some(skew));

        futures::executor::block_on(persist_pending_skew(&mut store, &pending)).unwrap();
        assert!(pending.lock().is_none());
        assert_eq!(*writes.lock(), vec![1_000_000i64.to_le_bytes().to_vec(); 2]);
    }

    async fn next_attempt(
        attempts: &mut mesh::Receiver<Vec<u8>>,
        driver: &DefaultDriver,
    ) -> Vec<u8> {
        let mut timer = PolledTimer::new(driver);
        futures::select! {
            attempt = attempts.next().fuse() => attempt.unwrap(),
            _ = timer.sleep(Duration::from_secs(5)).fuse() => {
                panic!("timed out waiting for RTC skew persistence")
            }
        }
    }

    #[async_test]
    async fn persistence_worker_batches_writes(driver: DefaultDriver) {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let (send, mut attempts) = mesh::channel();
        let store = RecordingStore {
            writes: writes.clone(),
            completion: None,
            attempts: Some(send),
        };
        let mut persistence = SkewPersistence::new(&driver, Box::new(store));
        for millis in 1..=100 {
            persistence.queue(LocalClockDelta::from_millis(millis));
        }
        assert!(writes.lock().is_empty());
        assert_eq!(
            next_attempt(&mut attempts, &driver).await,
            1_000_000i64.to_le_bytes()
        );
        persistence.flush().await.unwrap();
        assert_eq!(writes.lock().len(), 1);
        assert!(persistence.pending.lock().is_none());
    }

    #[async_test]
    async fn persistence_worker_flushes_without_waiting_for_timer(driver: DefaultDriver) {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let store = RecordingStore {
            writes: writes.clone(),
            completion: None,
            attempts: None,
        };
        let mut persistence = SkewPersistence::new(&driver, Box::new(store));
        persistence.queue(LocalClockDelta::from_millis(100));
        persistence.queue(LocalClockDelta::from_millis(200));
        persistence.flush().await.unwrap();
        persistence.flush().await.unwrap();
        assert_eq!(*writes.lock(), [2_000_000i64.to_le_bytes().to_vec()]);
    }

    #[async_test]
    async fn persistence_worker_retries_without_another_guest_write(driver: DefaultDriver) {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let (complete, completion) = mesh::oneshot();
        let (send, mut attempts) = mesh::channel();
        let store = RecordingStore {
            writes: writes.clone(),
            completion: Some(completion),
            attempts: Some(send),
        };
        let mut persistence = SkewPersistence::new(&driver, Box::new(store));
        complete.send(false);
        persistence.queue(LocalClockDelta::from_millis(100));
        for _ in 0..2 {
            assert_eq!(
                next_attempt(&mut attempts, &driver).await,
                1_000_000i64.to_le_bytes()
            );
        }
        persistence.flush().await.unwrap();
        assert_eq!(writes.lock().len(), 2);
    }

    #[async_test]
    async fn persistence_worker_flush_preserves_newer_write_after_failure(driver: DefaultDriver) {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let (complete, completion) = mesh::oneshot();
        let (send, mut attempts) = mesh::channel();
        let store = RecordingStore {
            writes: writes.clone(),
            completion: Some(completion),
            attempts: Some(send),
        };
        let mut persistence = SkewPersistence::new(&driver, Box::new(store));
        persistence.queue(LocalClockDelta::from_millis(100));
        assert_eq!(
            next_attempt(&mut attempts, &driver).await,
            1_000_000i64.to_le_bytes()
        );
        persistence.queue(LocalClockDelta::from_millis(200));
        let flush = persistence.flush();
        complete.send(false);
        flush.await.unwrap();
        assert_eq!(
            *writes.lock(),
            [
                1_000_000i64.to_le_bytes().to_vec(),
                2_000_000i64.to_le_bytes().to_vec(),
            ]
        );
        assert!(persistence.pending.lock().is_none());
    }

    #[async_test]
    async fn persistence_worker_does_not_postpone_for_continuous_writes(driver: DefaultDriver) {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let (send, mut attempts) = mesh::channel();
        let store = RecordingStore {
            writes: writes.clone(),
            completion: None,
            attempts: Some(send),
        };
        let mut persistence = SkewPersistence::new(&driver, Box::new(store));
        let mut millis = 100;
        persistence.queue(LocalClockDelta::from_millis(millis));
        let mut timer = PolledTimer::new(&driver);
        {
            let mut attempt = std::pin::pin!(next_attempt(&mut attempts, &driver).fuse());
            loop {
                futures::select_biased! {
                    _ = attempt => break,
                    _ = timer.sleep(Duration::from_millis(10)).fuse() => {
                        millis += 1;
                        persistence.queue(LocalClockDelta::from_millis(millis));
                    }
                }
            }
        }
        assert!(millis > 100);
        persistence.flush().await.unwrap();
        assert_eq!(
            writes.lock().last().unwrap(),
            &(millis * 10_000).to_le_bytes().to_vec()
        );
    }

    #[async_test]
    async fn persistence_worker_reports_flush_failure_and_retains_pending(driver: DefaultDriver) {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let (complete, completion) = mesh::oneshot();
        let store = RecordingStore {
            writes: writes.clone(),
            completion: Some(completion),
            attempts: None,
        };
        let mut persistence = SkewPersistence::new(&driver, Box::new(store));
        let skew = LocalClockDelta::from_millis(100);
        persistence.queue(skew);
        complete.send(false);
        assert!(persistence.flush().await.is_err());
        assert_eq!(*persistence.pending.lock(), Some(skew));
        persistence.flush().await.unwrap();
        assert_eq!(writes.lock().len(), 2);
        assert!(persistence.pending.lock().is_none());
    }

    #[async_test]
    async fn restore_queues_unflushed_saved_offset(driver: DefaultDriver) {
        let get = guest_emulation_transport::test_utilities::new_transport_pair(
            driver.clone(),
            None,
            get_protocol::ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await;
        let writes = Arc::new(Mutex::new(Vec::new()));
        let store = || {
            Box::new(RecordingStore {
                writes: writes.clone(),
                completion: None,
                attempts: None,
            }) as Box<dyn NonVolatileStore>
        };
        let mut clock = UnderhillLocalClock {
            persistence: SkewPersistence::new(&driver, store()),
            host_time: HostSystemTimeAccess::new(get.client.clone()),
            offset_from_host_time: LocalClockDelta::from_millis(100),
        };
        clock.persistence.queue(clock.offset_from_host_time);
        let saved = clock.save().unwrap();
        assert!(writes.lock().is_empty());
        drop(clock);

        let restored = UnderhillLocalClock::new(&driver, get.client.clone(), store(), Some(saved))
            .await
            .unwrap();
        assert_eq!(
            restored.offset_from_host_time,
            LocalClockDelta::from_millis(100)
        );
        restored.flush().await.unwrap();
        assert_eq!(*writes.lock(), [1_000_000i64.to_le_bytes().to_vec()]);
    }
}
