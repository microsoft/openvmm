// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use self::host_time::HostSystemTimeAccess;
use cvm_tracing::CVM_ALLOWED;
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
            Err(time) => Err(Box::pin(async move {
                LocalClockTime::from(time.await) + offset
            })),
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

    /// Encapsulates all the nitty-gritty details of how real time gets fetched
    /// from the Host.
    #[derive(Debug)]
    pub struct HostSystemTimeAccess {
        pub(super) get: guest_emulation_transport::GuestEmulationTransportClient,
        pub(super) state: Arc<Mutex<HostTimeState>>,
    }

    #[derive(Debug, Default)]
    pub(super) struct HostTimeState {
        /// The latest host time, and when the query for it was sent.
        pub(super) cached_host_time: Option<(Instant, Zoned)>,
        /// The query sent for the host time, until its reply is taken.
        pub(super) query: Option<HostTimeQuery>,
    }

    /// A host time query, which any number of callers can wait for.
    #[derive(Clone)]
    pub(super) struct HostTimeQuery {
        pub(super) sent: Instant,
        pub(super) reply: Shared<BoxFuture<'static, Time>>,
    }

    impl std::fmt::Debug for HostTimeQuery {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("HostTimeQuery")
                .field("sent", &self.sent)
                .finish_non_exhaustive()
        }
    }

    impl Inspect for HostSystemTimeAccess {
        fn inspect(&self, req: inspect::Request<'_>) {
            let HostSystemTimeAccess { get: _, state } = self;

            let mut res = req.respond();

            if let Some((last_query, ref cached_time)) = state.lock().cached_host_time {
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
                state: Default::default(),
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
            let now = Instant::now();
            let query = match start(&self.get, &self.state, now) {
                Ok(time) => return Ok(time),
                Err(query) => query,
            };
            let get = self.get.clone();
            let state = self.state.clone();
            Err(async move { wait(&get, &state, now, query).await }.boxed())
        }
    }

    /// Return the host's time as of `now`, or the query to wait for it.
    fn start(
        get: &guest_emulation_transport::GuestEmulationTransportClient,
        state: &Mutex<HostTimeState>,
        now: Instant,
    ) -> Result<Zoned, HostTimeQuery> {
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
        let mut state = state.lock();
        if let Some((last_query, cached_time)) = &state.cached_host_time
            && now.saturating_duration_since(*last_query) < Duration::from_secs(1)
        {
            return Ok(cached_time.clone());
        }

        // Callers that need the time while a query is out wait for it, rather
        // than send another.
        let query = state.query.get_or_insert_with(|| {
            let get = get.clone();
            HostTimeQuery {
                sent: now,
                reply: async move { get.host_time().await }.boxed().shared(),
            }
        });
        Err(query.clone())
    }

    /// Wait for `query`, and then for the host's time as of `now`.
    pub(super) async fn wait(
        get: &guest_emulation_transport::GuestEmulationTransportClient,
        state: &Mutex<HostTimeState>,
        now: Instant,
        mut query: HostTimeQuery,
    ) -> Zoned {
        loop {
            let time = query.reply.clone().await.to_jiff();
            {
                let mut state = state.lock();
                if state
                    .cached_host_time
                    .as_ref()
                    .is_none_or(|(last_query, _)| *last_query <= query.sent)
                {
                    state.cached_host_time = Some((query.sent, time.clone()));
                }
                if state
                    .query
                    .as_ref()
                    .is_some_and(|sent| Shared::ptr_eq(&sent.reply, &query.reply))
                {
                    state.query = None;
                }
            }

            // The reply is the time if the query was sent less than a second
            // before `now`, as for the query sent for `now`. Otherwise, as
            // the query was out before, go on as if the reply had been cached.
            if now.saturating_duration_since(query.sent) < Duration::from_secs(1) {
                return time;
            }
            query = match start(get, state, now) {
                Ok(time) => return time,
                Err(query) => query,
            };
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
    use super::host_time::HostTimeQuery;
    use futures::FutureExt;
    use guest_emulation_transport::api::Time;
    use guest_emulation_transport::test_utilities::TestGet;
    use jiff::Timestamp;
    use local_clock::LocalClock;
    use local_clock::LocalClockTime;
    use pal_async::DefaultDriver;
    use pal_async::DefaultPool;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use pal_async::timer::PolledTimer;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Wake;
    use std::task::Waker;
    use std::time::Duration;
    use std::time::Instant;
    use test_with_tracing::test;
    use vmcore::non_volatile_store::EphemeralNonVolatileStore;

    /// Create a GET served on its own thread.
    async fn new_get() -> (TestGet, DefaultDriver) {
        let (_, get_driver) = DefaultPool::spawn_on_thread("get");
        let get = guest_emulation_transport::test_utilities::new_transport_pair(
            get_driver.clone(),
            None,
            get_protocol::ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await;
        (get, get_driver)
    }

    /// Hold up the GET's thread until the returned sender is sent to or
    /// dropped, or for ten seconds at most, so that a test that wrongly
    /// waits for the host fails rather than hangs.
    fn hold(get_driver: &DefaultDriver) -> std::sync::mpsc::Sender<()> {
        let (started, wait) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel();
        get_driver
            .spawn("hold", async move {
                started.send(()).unwrap();
                let _ = held.recv_timeout(Duration::from_secs(10));
            })
            .detach();
        wait.recv().unwrap();
        release
    }

    /// Return a query sent at `sent`, answered with `time` once the returned
    /// sender is sent to.
    fn query(sent: Instant) -> (HostTimeQuery, mesh::OneshotSender<Time>) {
        let (send, recv) = mesh::oneshot();
        let query = HostTimeQuery {
            sent,
            reply: async move { recv.await.unwrap() }.boxed().shared(),
        };
        (query, send)
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

    /// 2026-01-01T12:00:00Z.
    fn noon() -> Timestamp {
        Timestamp::from_millisecond(1_767_268_800_000).unwrap()
    }

    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[async_test]
    async fn rtc_clock_does_not_wait_for_host(_driver: DefaultDriver) {
        let (get, get_driver) = new_get().await;
        let clock = UnderhillLocalClock::new(
            get.client.clone(),
            EphemeralNonVolatileStore::new_boxed(),
            None,
        )
        .await
        .unwrap();
        let clock = ArcMutexUnderhillLocalClock(Arc::new(parking_lot::Mutex::new(clock)));
        let mut rtc_clock = clock.new_linked_clock();
        if let Some((last_query, _)) = &mut clock.0.lock().host_time.state.lock().cached_host_time {
            *last_query -= Duration::from_secs(1);
        }

        // With the host held up, the clock the RTC uses returns a future for
        // the time rather than waiting.
        let release = hold(&get_driver);
        let before = Instant::now();
        let Err(time) = rtc_clock.get_time_or_wait() else {
            panic!("the clock didn't wait");
        };
        let after = Instant::now();
        release.send(()).unwrap();

        // The time is the host's reply, cached as of when the query was sent.
        let time = time.await;
        let clock = clock.0.lock();
        let (last_query, cached_time) = clock
            .host_time
            .state
            .lock()
            .cached_host_time
            .clone()
            .unwrap();
        assert!(before <= last_query && last_query <= after);
        assert_eq!(
            time,
            LocalClockTime::from(cached_time) + clock.offset_from_host_time
        );
    }

    #[async_test]
    async fn read_keeps_the_reply_it_waited_for(driver: DefaultDriver) {
        let (get, get_driver) = new_get().await;
        let host_time = Arc::new(HostSystemTimeAccess::new(get.client.clone()));

        // A read waits for a query it sent.
        let sent = Instant::now();
        let (first, answer) = query(sent);
        host_time.state.lock().query = Some(first.clone());
        let [Err(read), Err(later_read)] = [0, 1].map(|_| host_time.now_or_wait()) else {
            panic!("the reads didn't wait");
        };

        // Another caller, more than a second later, waits for the same query,
        // takes its reply first, and as that is too old for it, sends another,
        // which the host holds up.
        let release = hold(&get_driver);
        let other = std::thread::spawn({
            let host_time = host_time.clone();
            move || {
                pal_async::local::block_on(super::host_time::wait(
                    &host_time.get,
                    &host_time.state,
                    sent + Duration::from_millis(1500),
                    first,
                ))
            }
        });
        answer.send(host_time_at(noon()));
        let mut timer = PolledTimer::new(&driver);
        while !host_time
            .state
            .lock()
            .query
            .as_ref()
            .is_some_and(|query| query.sent > sent)
        {
            timer.sleep(Duration::from_millis(1)).await;
        }

        // The read still gets the reply to its query, while the other query
        // is out, and after its reply is in.
        assert_eq!(read.await.timestamp(), noon());
        release.send(()).unwrap();
        assert_ne!(other.join().unwrap().timestamp(), noon());
        assert_eq!(later_read.await.timestamp(), noon());
    }

    #[async_test]
    async fn rtc_clock_does_not_wait_behind_a_blocking_read(driver: DefaultDriver) {
        let (get, get_driver) = new_get().await;
        let clock = ArcMutexUnderhillLocalClock(Arc::new(parking_lot::Mutex::new(
            UnderhillLocalClock::new(
                get.client.clone(),
                EphemeralNonVolatileStore::new_boxed(),
                None,
            )
            .await
            .unwrap(),
        )));
        if let Some((last_query, _)) = &mut clock.0.lock().host_time.state.lock().cached_host_time {
            *last_query -= Duration::from_secs(1);
        }

        // While another user of the clock waits for the host, the clock the
        // RTC uses doesn't wait for it to start its own read.
        let state = clock.0.lock().host_time.state.clone();
        let release = hold(&get_driver);
        let blocking = std::thread::spawn({
            let mut clock = clock.new_linked_clock();
            move || clock.get_time()
        });
        let mut timer = PolledTimer::new(&driver);
        while state.lock().query.is_none() {
            timer.sleep(Duration::from_millis(1)).await;
        }
        let (send, started) = std::sync::mpsc::channel();
        let reader = std::thread::spawn({
            let mut clock = clock.new_linked_clock();
            move || send.send(clock.get_time_or_wait().is_err()).unwrap()
        });
        let started = started.recv_timeout(Duration::from_secs(5));
        release.send(()).unwrap();
        blocking.join().unwrap();
        reader.join().unwrap();
        assert_eq!(started, Ok(true));
    }

    #[test]
    fn everyone_waiting_for_a_query_is_woken() {
        let (get, _) = futures::executor::block_on(new_get());
        let host_time = HostSystemTimeAccess::new(get.client.clone());
        let (first, answer) = query(Instant::now());
        host_time.state.lock().query = Some(first);

        // Two reads wait for the same query, with their own wakers.
        let mut reads = [0, 1].map(|_| {
            let Err(read) = host_time.now_or_wait() else {
                panic!("the read didn't wait");
            };
            read
        });
        let wakes = [0, 1].map(|_| Arc::new(WakeCount::default()));
        for (read, wakes) in reads.iter_mut().zip(&wakes) {
            let waker = Waker::from(wakes.clone());
            assert!(
                read.poll_unpin(&mut Context::from_waker(&waker))
                    .is_pending()
            );
        }

        // The reply wakes both, and both get it.
        answer.send(host_time_at(noon()));
        for (read, wakes) in reads.iter_mut().zip(&wakes) {
            assert_eq!(wakes.0.load(Ordering::Relaxed), 1);
            let Poll::Ready(time) = read.poll_unpin(&mut Context::from_waker(Waker::noop())) else {
                panic!("the read didn't get the reply");
            };
            assert_eq!(time.timestamp(), noon());
        }
        assert!(host_time.state.lock().query.is_none());
    }

    #[test]
    fn now_waits_for_the_query_out() {
        let (get, _) = futures::executor::block_on(new_get());
        let host_time = HostSystemTimeAccess::new(get.client.clone());
        let (first, answer) = query(Instant::now());
        host_time.state.lock().query = Some(first);

        // A blocking read takes the reply to the query that is out, rather
        // than sending another.
        std::thread::scope(|s| {
            let now = s.spawn(|| host_time.now());
            answer.send(host_time_at(noon()));
            assert_eq!(now.join().unwrap().timestamp(), noon());
        });
        assert!(host_time.state.lock().query.is_none());
    }
}
