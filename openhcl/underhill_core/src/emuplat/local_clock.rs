// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use self::host_time::HostSystemTimeAccess;
use cvm_tracing::CVM_ALLOWED;
use futures::FutureExt;
use inspect::Inspect;
use local_clock::LocalClock;
use local_clock::LocalClockDelta;
use local_clock::LocalClockTime;
use local_clock::LocalClockTimeFuture;
use vmcore::non_volatile_store::NonVolatileStore;
use vmcore::non_volatile_store::NonVolatileStoreError;
use vmcore::save_restore::SaveRestore;

const MILLIS_IN_TWO_DAYS: i64 = 1000 * 60 * 60 * 24 * 2;

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
    #[inspect(skip)]
    store: Box<dyn NonVolatileStore>,
    host_time: HostSystemTimeAccess,
    offset_from_host_time: LocalClockDelta,
}

impl std::fmt::Debug for UnderhillLocalClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            store: _,
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
        get: guest_emulation_transport::GuestEmulationTransportClient,
        store: Box<dyn NonVolatileStore>,
        saved_state: Option<<Self as SaveRestore>::SavedState>,
    ) -> anyhow::Result<Self> {
        let host_time = HostSystemTimeAccess::new(get);

        let mut this = Self {
            store,
            host_time,
            offset_from_host_time: LocalClockDelta::default(),
        };

        match saved_state {
            Some(state) => this.restore(state)?,
            None => {
                this.offset_from_host_time = match fetch_skew_from_store(&mut this.store).await? {
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
                        let skew = this.host_time.now().offset();
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
        if this.offset_from_host_time < neg_two_days {
            this.offset_from_host_time = neg_two_days;
            tracing::warn!(
                CVM_ALLOWED,
                "Guest time was more than two days in the past."
            );
        }

        Ok(this)
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

    fn get_time_or_wait(&mut self) -> Result<LocalClockTime, LocalClockTimeFuture> {
        let offset = self.offset_from_host_time;
        match self.host_time.now_or_wait() {
            Ok(time) => Ok(LocalClockTime::from(time) + offset),
            Err(time) => Err(time
                .map(move |time| LocalClockTime::from(time) + offset)
                .boxed()),
        }
    }

    fn set_time(&mut self, new_time: LocalClockTime) {
        let new_skew = new_time - LocalClockTime::from(self.host_time.now());
        self.offset_from_host_time = new_skew;

        // persist the skew in units of 100ns
        let raw_skew: i64 = new_skew.as_millis() * 10_000; // millis to 100ns units

        // TODO: swap this out for a non-blocking version that guarantees the skew is written out _eventually_
        let res = pal_async::local::block_on(self.store.persist(raw_skew.to_le_bytes().into()));
        if let Err(err) = res {
            tracing::error!(
                CVM_ALLOWED,
                err = &err as &dyn std::error::Error,
                "failed to persist RTC skew"
            );
        }
    }
}

mod host_time {
    use futures::FutureExt;
    use futures::future::BoxFuture;
    use futures::future::Shared;
    use guest_emulation_transport::api::Time;
    use inspect::Inspect;
    use jiff::Zoned;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use std::time::Duration;
    use std::time::Instant;

    /// Sends a host time query.
    pub(super) type Query = Arc<dyn Fn() -> BoxFuture<'static, Time> + Send + Sync>;

    /// Returns the current local time.
    pub(super) type Now = Arc<dyn Fn() -> Instant + Send + Sync>;

    /// A read of the host time, and when the query for it was sent.
    type HostTime = Shared<BoxFuture<'static, (Instant, Zoned)>>;

    /// Encapsulates all the nitty-gritty details of how real time gets fetched
    /// from the Host.
    pub struct HostSystemTimeAccess {
        query: Query,
        now: Now,
        /// The latest read of the host time. Once it's done, it is the cached
        /// host time.
        latest: Mutex<Option<HostTime>>,
    }

    impl std::fmt::Debug for HostSystemTimeAccess {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("HostSystemTimeAccess")
                .field("latest", &self.latest)
                .finish_non_exhaustive()
        }
    }

    impl Inspect for HostSystemTimeAccess {
        fn inspect(&self, req: inspect::Request<'_>) {
            let mut res = req.respond();

            if let Some((last_query, cached_time)) =
                self.latest.lock().as_ref().and_then(|latest| latest.peek())
            {
                res.display_debug("since_last_query", &(Instant::now() - *last_query))
                    .display("cached_time", cached_time);
            }
        }
    }

    impl HostSystemTimeAccess {
        pub fn new(
            get: guest_emulation_transport::GuestEmulationTransportClient,
        ) -> HostSystemTimeAccess {
            Self::with(
                Arc::new(move || {
                    let get = get.clone();
                    async move { get.host_time().await }.boxed()
                }),
                Arc::new(Instant::now),
            )
        }

        pub(super) fn with(query: Query, now: Now) -> HostSystemTimeAccess {
            HostSystemTimeAccess {
                query,
                now,
                latest: Mutex::new(None),
            }
        }

        /// Return the host's current time
        pub fn now(&self) -> Zoned {
            match self.now_or_wait() {
                Ok(time) => time,
                // TODO: this block_on really ain't great, but since we're
                // not hammering the GET on _each_ access, it's okay for now...
                Err(time) => pal_async::local::block_on(time),
            }
        }

        /// Return the host's current time as `now` does, or if that would wait
        /// for the host, a future for the time it would return.
        pub fn now_or_wait(&self) -> Result<Zoned, BoxFuture<'static, Zoned>> {
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
            let mut latest = self.latest.lock();
            let before = match latest.as_ref().map(|latest| (latest, latest.peek())) {
                Some((_, Some((last_query, cached_time))))
                    if (self.now)().saturating_duration_since(*last_query)
                        < Duration::from_secs(1) =>
                {
                    return Ok(cached_time.clone());
                }
                Some((latest, None)) => Some(latest.clone()),
                _ => None,
            };

            // Read the host time after the read before this one, in turn, as
            // callers waiting for the clock did.
            let query = self.query.clone();
            let now = self.now.clone();
            let read = async move {
                if let Some(before) = before {
                    let (last_query, cached_time) = before.await;
                    if now().saturating_duration_since(last_query) < Duration::from_secs(1) {
                        return (last_query, cached_time);
                    }
                }
                let sent = now();
                (sent, query().await.to_jiff())
            }
            .boxed()
            .shared();
            *latest = Some(read.clone());
            Err(read.map(|(_, time)| time).boxed())
        }
    }
}

#[derive(Debug, Inspect)]
#[inspect(transparent)]
pub struct ArcMutexUnderhillLocalClock(pub std::sync::Arc<parking_lot::Mutex<UnderhillLocalClock>>);

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
        // Don't keep the clock locked while waiting for the host, so that
        // others don't wait for it to start their own reads.
        let time = self.0.lock().get_time_or_wait();
        time.unwrap_or_else(pal_async::local::block_on)
    }

    fn get_time_or_wait(&mut self) -> Result<LocalClockTime, LocalClockTimeFuture> {
        self.0.lock().get_time_or_wait()
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

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ArcMutexUnderhillLocalClock;
    use super::UnderhillLocalClock;
    use super::host_time::HostSystemTimeAccess;
    use futures::FutureExt;
    use futures::future::BoxFuture;
    use guest_emulation_transport::api::Time;
    use jiff::Timestamp;
    use jiff::Zoned;
    use local_clock::LocalClock;
    use local_clock::LocalClockDelta;
    use parking_lot::Mutex;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Waker;
    use std::time::Duration;
    use std::time::Instant;
    use test_with_tracing::test;
    use vmcore::non_volatile_store::EphemeralNonVolatileStore;

    /// A host whose queries the test answers, in order, at a local time the
    /// test sets.
    #[derive(Clone)]
    struct TestHost {
        base: Instant,
        /// The local time, past `base`.
        elapsed: Arc<Mutex<Duration>>,
        /// The queries sent, for the test to answer.
        queries: Arc<Mutex<VecDeque<mesh::OneshotSender<Time>>>>,
        sent: Arc<Mutex<usize>>,
    }

    impl TestHost {
        fn new() -> Self {
            Self {
                base: Instant::now(),
                elapsed: Default::default(),
                queries: Default::default(),
                sent: Default::default(),
            }
        }

        fn host_time(&self) -> HostSystemTimeAccess {
            let host = self.clone();
            let now = self.clone();
            HostSystemTimeAccess::with(
                Arc::new(move || {
                    let (send, recv) = mesh::oneshot();
                    host.queries.lock().push_back(send);
                    *host.sent.lock() += 1;
                    async move { recv.await.unwrap() }.boxed()
                }),
                Arc::new(move || now.base + *now.elapsed.lock()),
            )
        }

        fn set_elapsed(&self, millis: u64) {
            *self.elapsed.lock() = Duration::from_millis(millis);
        }

        /// Answer the oldest query with `hour`:00 on 2026-01-01, in UTC.
        fn answer(&self, hour: i8) {
            let time = jiff::civil::date(2026, 1, 1)
                .at(hour, 0, 0, 0)
                .to_zoned(jiff::tz::TimeZone::UTC)
                .unwrap()
                .timestamp();
            self.queries
                .lock()
                .pop_front()
                .expect("no query")
                .send(host_time_at(time));
        }

        fn sent(&self) -> usize {
            *self.sent.lock()
        }
    }

    /// A host time reply for `time`.
    fn host_time_at(time: Timestamp) -> Time {
        let since_windows_epoch = jiff::civil::date(1601, 1, 1)
            .at(0, 0, 0, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap()
            .timestamp()
            .duration_until(time);
        Time {
            utc: (since_windows_epoch.as_nanos() / 100) as i64,
            time_zone: 0,
        }
    }

    fn poll(read: &mut BoxFuture<'static, Zoned>) -> Option<i8> {
        match read.poll_unpin(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(time) => Some(time.hour()),
            Poll::Pending => None,
        }
    }

    fn wait(host_time: &HostSystemTimeAccess) -> BoxFuture<'static, Zoned> {
        host_time.now_or_wait().expect_err("the read didn't wait")
    }

    fn cached(host_time: &HostSystemTimeAccess) -> Zoned {
        let Ok(time) = host_time.now_or_wait() else {
            panic!("the read waited");
        };
        time
    }

    #[test]
    fn reads_take_turns() {
        let host = TestHost::new();
        let host_time = host.host_time();

        // A reads the time, and its query is sent. B reads 100ms later.
        let mut a = wait(&host_time);
        assert_eq!(poll(&mut a), None);
        assert_eq!(host.sent(), 1);
        host.set_elapsed(100);
        let mut b = wait(&host_time);

        // The reply comes in 1.5s after the query was sent. It is A's, and
        // as it's more than a second old when B's turn comes, B sends another
        // query, as from then.
        host.set_elapsed(1500);
        host.answer(12);
        assert_eq!(poll(&mut a), Some(12));
        assert_eq!(poll(&mut b), None);
        assert_eq!(host.sent(), 2);
        host.set_elapsed(1600);
        host.answer(13);
        assert_eq!(poll(&mut b), Some(13));

        // That reply is cached as of when it was sent, so a read right after
        // uses it, as setting the time after B would.
        host.set_elapsed(2499);
        assert_eq!(cached(&host_time).hour(), 13);
        host.set_elapsed(2500);
        assert_eq!(poll(&mut wait(&host_time)), None);
        assert_eq!(host.sent(), 3);
    }

    #[test]
    fn reads_share_a_reply_in_time() {
        let host = TestHost::new();
        let host_time = host.host_time();
        let mut a = wait(&host_time);
        assert_eq!(poll(&mut a), None);
        host.set_elapsed(100);
        let mut b = wait(&host_time);

        // The reply comes in less than a second after the query was sent, so
        // B uses it too. B's read can take it first.
        host.set_elapsed(900);
        host.answer(12);
        assert_eq!(poll(&mut b), Some(12));
        assert_eq!(poll(&mut a), Some(12));
        assert_eq!(host.sent(), 1);
    }

    #[test]
    fn later_read_drives_the_one_before() {
        let host = TestHost::new();
        let host_time = host.host_time();

        // Nothing polls A's read, which is waited for by B's, which sends
        // A's query.
        let _a = wait(&host_time);
        let mut b = wait(&host_time);
        assert_eq!(poll(&mut b), None);
        assert_eq!(host.sent(), 1);
        host.answer(12);
        assert_eq!(poll(&mut b), Some(12));
        assert_eq!(host.sent(), 1);
    }

    #[test]
    fn blocking_read_does_not_hold_the_clock() {
        let host = TestHost::new();
        let clock = ArcMutexUnderhillLocalClock(Arc::new(Mutex::new(UnderhillLocalClock {
            store: EphemeralNonVolatileStore::new_boxed(),
            host_time: host.host_time(),
            offset_from_host_time: LocalClockDelta::default(),
        })));

        // While another user of the clock waits for the host, the clock the
        // RTC uses doesn't wait for it to start its own read.
        let blocking = std::thread::spawn({
            let mut clock = clock.new_linked_clock();
            move || clock.get_time()
        });
        while host.sent() == 0 {
            std::thread::yield_now();
        }
        let (send, started) = mpsc::channel();
        let reader = std::thread::spawn({
            let mut clock = clock.new_linked_clock();
            move || send.send(clock.get_time_or_wait().is_err()).unwrap()
        });
        let started = started.recv_timeout(Duration::from_secs(10));
        host.answer(12);
        blocking.join().unwrap();
        reader.join().unwrap();
        assert_eq!(started, Ok(true));
    }

    #[test]
    fn reads_the_host() {
        // Through the GET, the first read waits, and the next uses the reply.
        let (_, get_driver) = pal_async::DefaultPool::spawn_on_thread("get");
        let get = futures::executor::block_on(
            guest_emulation_transport::test_utilities::new_transport_pair(
                get_driver,
                None,
                get_protocol::ProtocolVersion::NICKEL_REV2,
                None,
                None,
            ),
        );
        let host_time = HostSystemTimeAccess::new(get.client.clone());
        let time = pal_async::local::block_on(wait(&host_time));
        assert_eq!(cached(&host_time), time);
    }
}
