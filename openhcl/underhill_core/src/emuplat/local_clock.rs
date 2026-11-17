// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use self::host_time::HostSystemTimeAccess;
use cvm_tracing::CVM_ALLOWED;
use inspect::Inspect;
use local_clock::LocalClock;
use local_clock::LocalClockDelta;
use local_clock::LocalClockTime;
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

    fn set_time(&mut self, new_time: LocalClockTime) {
        let new_skew = new_time - LocalClockTime::from(self.host_time.query());
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
    use guest_emulation_transport::api::Time;
    use inspect::Inspect;
    use jiff::SignedDuration;
    use jiff::Timestamp;
    use jiff::Zoned;
    use parking_lot::Mutex;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Wake;
    use std::task::Waker;
    use std::time::Duration;
    use std::time::Instant;

    /// Encapsulates all the nitty-gritty details of how real time gets fetched
    /// from the Host.
    #[derive(Debug, Inspect)]
    pub struct HostSystemTimeAccess {
        #[inspect(skip)]
        get: guest_emulation_transport::GuestEmulationTransportClient,
        #[inspect(flatten)]
        pub(super) cached_host_time: Mutex<Option<CachedHostTime>>,
        /// The latest time returned before servicing, to hold once the host
        /// has been queried.
        #[inspect(debug)]
        held: Option<Timestamp>,
    }

    #[derive(Inspect)]
    pub(super) struct CachedHostTime {
        /// When the last query to be answered was sent.
        #[inspect(
            rename = "since_last_query",
            with = "|x| inspect::AsDebug(x.elapsed())"
        )]
        pub(super) last_query: Instant,
        /// The host time at `anchor`.
        #[inspect(rename = "cached_time", display)]
        pub(super) time: Zoned,
        /// The local time `time` is advanced from.
        #[inspect(skip)]
        pub(super) anchor: Instant,
        /// The latest time returned.
        #[inspect(display)]
        latest: Zoned,
        /// An outstanding query: when it was sent, when it was last woken, and
        /// the reply.
        #[inspect(with = "Option::is_some")]
        refresh: Option<Refresh>,
    }

    type Refresh = (
        Instant,
        Arc<WokenAt>,
        Pin<Box<dyn Future<Output = Time> + Send>>,
    );

    impl std::fmt::Debug for CachedHostTime {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let Self {
                last_query,
                time,
                anchor,
                latest,
                refresh,
            } = self;

            f.debug_struct("CachedHostTime")
                .field("last_query", last_query)
                .field("time", time)
                .field("anchor", anchor)
                .field("latest", latest)
                .field("refresh", &refresh.as_ref().map(|(sent, _, _)| sent))
                .finish()
        }
    }

    impl CachedHostTime {
        /// Take a reply to a query sent at `sent`, which came in at `replied`.
        fn update(&mut self, sent: Instant, replied: Instant, reply: Time, now: Instant) {
            // The host sampled its time somewhere between sending the query
            // and the reply coming in. Keep advancing the time as before if it
            // falls in that range, and otherwise move it to the nearer end, so
            // that a slow reply doesn't move the time by however long the host
            // took.
            let reply = reply.to_jiff();
            let advanced = &self.time + now.duration_since(self.anchor);
            self.time = advanced
                .clamp(
                    &reply + now.duration_since(replied),
                    &reply + now.duration_since(sent),
                )
                .with_time_zone(reply.time_zone().clone());
            self.anchor = now;
            self.last_query = sent;
            self.refresh = None;
        }
    }

    /// Records when a query was last woken, which is when its reply came in.
    struct WokenAt(Mutex<Option<Instant>>);

    impl Wake for WokenAt {
        fn wake(self: Arc<Self>) {
            *self.0.lock() = Some(Instant::now());
        }
    }

    impl HostSystemTimeAccess {
        pub fn new(
            get: guest_emulation_transport::GuestEmulationTransportClient,
        ) -> HostSystemTimeAccess {
            HostSystemTimeAccess {
                get,
                cached_host_time: Mutex::new(None),
                held: None,
            }
        }

        /// Return the cached host time, waiting for the host if nothing has
        /// been cached yet.
        fn cached<'a>(
            &self,
            cached_host_time: &'a mut Option<CachedHostTime>,
        ) -> &'a mut CachedHostTime {
            cached_host_time.get_or_insert_with(|| {
                // There is nothing to advance yet, so wait for the host.
                let sent = Instant::now();
                let time = pal_async::local::block_with_io(|_| self.get.host_time()).to_jiff();
                let held = self
                    .held
                    .map(|held| held.to_zoned(time.time_zone().clone()));
                CachedHostTime {
                    last_query: sent,
                    latest: held.map_or_else(|| time.clone(), |held| held.max(time.clone())),
                    time,
                    anchor: Instant::now(),
                    refresh: None,
                }
            })
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
            //
            // This is called on the VP thread, so after the first query, don't
            // wait for the host to respond. Until it does, keep advancing the
            // previous host time; if the host paused the VM, which VTL2 can't
            // see, this is behind by the pause until the reply comes in, and
            // after the time has gone unread for a while it is off by the local
            // clock's drift.
            let mut cached_host_time = self.cached_host_time.lock();
            let cached_host_time = self.cached(&mut cached_host_time);
            let now = Instant::now();
            self.refresh(cached_host_time, now);

            let time = &cached_host_time.time + now.duration_since(cached_host_time.anchor);

            // The local clock may run ahead of the host between queries, so a
            // reply can put the time behind what was returned before. Hold it
            // until the corrected time catches up rather than moving it
            // backwards, unless it is a second or more behind: that is a change
            // to the host time, or drift over a long gap between queries, and
            // holding it that long would stop the RTC.
            if time < cached_host_time.latest
                && cached_host_time.latest.duration_since(&time) < SignedDuration::from_secs(1)
            {
                return cached_host_time.latest.clone();
            }
            cached_host_time.latest = time.clone();
            time
        }

        /// Send a query if the cached time is a second old, and take the reply
        /// if it has come in.
        fn refresh(&self, cached_host_time: &mut CachedHostTime, now: Instant) {
            if cached_host_time.refresh.is_none()
                && now.duration_since(cached_host_time.last_query) >= Duration::from_secs(1)
            {
                cached_host_time.refresh = Some(self.send(now));
            }

            if let Some((sent, woken_at, reply)) = &mut cached_host_time.refresh
                && let Poll::Ready(reply) = reply
                    .as_mut()
                    .poll(&mut Context::from_waker(&Waker::from(woken_at.clone())))
            {
                let sent = *sent;
                let replied = woken_at.0.lock().unwrap_or(now);
                cached_host_time.update(sent, replied, reply, now);
            }
        }

        fn send(&self, now: Instant) -> Refresh {
            let get = self.get.clone();
            (
                now,
                Arc::new(WokenAt(Mutex::new(None))),
                Box::pin(async move { get.host_time().await }),
            )
        }

        /// Return the host time as of the call, for setting the time. Unlike
        /// `now`, wait for the host once the cached time is a second old: the
        /// advanced time falls behind if the host paused the VM, which would
        /// put the pause into the skew. The reply to a query sent within the
        /// last second is used, so a pause within a second of that query is
        /// still missed. Times returned before aren't held, since the time is
        /// being set.
        pub fn query(&self) -> Zoned {
            let now = Instant::now();
            let mut cached_host_time = self.cached_host_time.lock();
            let cached_host_time = self.cached(&mut cached_host_time);
            self.refresh(cached_host_time, now);

            if now.duration_since(cached_host_time.last_query) >= Duration::from_secs(1) {
                let (sent, _, reply) = cached_host_time
                    .refresh
                    .take()
                    .filter(|(sent, _, _)| now.duration_since(*sent) < Duration::from_secs(1))
                    .unwrap_or_else(|| self.send(now));
                // TODO: this block_on really ain't great, but since we're
                // not hammering the GET on _each_ access, it's okay for now...
                let reply = pal_async::local::block_with_io(|_| reply);
                let replied = Instant::now();
                cached_host_time.update(sent, replied, reply, replied);
            }

            // The time is set as of when the guest wrote it, not when the
            // reply came in.
            cached_host_time.latest = if now >= cached_host_time.anchor {
                &cached_host_time.time + now.duration_since(cached_host_time.anchor)
            } else {
                &cached_host_time.time - cached_host_time.anchor.duration_since(now)
            };
            cached_host_time.latest.clone()
        }

        /// Return the latest time returned, if any.
        pub fn latest(&self) -> Option<Timestamp> {
            match &*self.cached_host_time.lock() {
                Some(cached_host_time) => Some(cached_host_time.latest.timestamp()),
                None => self.held,
            }
        }

        /// Hold times before `latest`, as returned before servicing.
        pub fn hold(&mut self, latest: Timestamp) {
            match self.cached_host_time.get_mut() {
                Some(cached_host_time) => {
                    let latest = latest.to_zoned(cached_host_time.latest.time_zone().clone());
                    if latest > cached_host_time.latest {
                        cached_host_time.latest = latest;
                    }
                }
                None => self.held = Some(latest),
            }
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
            /// The latest host time returned, so that the time doesn't go
            /// back across servicing.
            #[mesh(2)]
            pub latest_host_time_millis: Option<i64>,
        }
    }

    impl SaveRestore for UnderhillLocalClock {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(state::SavedState {
                offset_from_host_time_millis: self.offset_from_host_time.as_millis(),
                latest_host_time_millis: self
                    .host_time
                    .latest()
                    .map(|latest| latest.as_millisecond()),
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                offset_from_host_time_millis,
                latest_host_time_millis,
            } = state;

            self.offset_from_host_time = LocalClockDelta::from_millis(offset_from_host_time_millis);
            if let Some(latest) = latest_host_time_millis {
                self.host_time.hold(
                    jiff::Timestamp::from_millisecond(latest)
                        .map_err(|err| RestoreError::InvalidSavedState(err.into()))?,
                );
            }

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::UnderhillLocalClock;
    use super::fetch_skew_from_store;
    use super::host_time::CachedHostTime;
    use super::host_time::HostSystemTimeAccess;
    use guest_emulation_transport::test_utilities::TestGet;
    use jiff::Zoned;
    use local_clock::LocalClock;
    use local_clock::LocalClockTime;
    use pal_async::DefaultDriver;
    use pal_async::DefaultPool;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use pal_async::timer::PolledTimer;
    use parking_lot::MappedMutexGuard;
    use parking_lot::MutexGuard;
    use std::time::Duration;
    use std::time::Instant;
    use std::time::SystemTime;
    use test_with_tracing::test;
    use vmcore::non_volatile_store::EphemeralNonVolatileStore;
    use vmcore::save_restore::SaveRestore;

    async fn new_get(driver: &DefaultDriver) -> TestGet {
        guest_emulation_transport::test_utilities::new_transport_pair(
            driver.clone(),
            None,
            get_protocol::ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await
    }

    /// Create a host time access with the first query answered. The GED is
    /// served by this thread, so make the query on another one.
    async fn new_host_time(driver: &DefaultDriver, get: &TestGet) -> HostSystemTimeAccess {
        let host_time = HostSystemTimeAccess::new(get.client.clone());
        let thread = std::thread::spawn(move || {
            host_time.now();
            host_time
        });
        let mut timer = PolledTimer::new(driver);
        while !thread.is_finished() {
            timer.sleep(Duration::from_millis(1)).await;
        }
        thread.join().unwrap()
    }

    fn cached(host_time: &HostSystemTimeAccess) -> MappedMutexGuard<'_, CachedHostTime> {
        MutexGuard::map(host_time.cached_host_time.lock(), |cached_host_time| {
            cached_host_time.as_mut().unwrap()
        })
    }

    /// Create a clock with the GED served on its own thread, since setting
    /// the time waits for the host. With no skew stored, creating the clock
    /// queries the host.
    async fn new_clock() -> (UnderhillLocalClock, TestGet, DefaultDriver) {
        let (_, get_driver) = DefaultPool::spawn_on_thread("get");
        let get = new_get(&get_driver).await;
        let clock = UnderhillLocalClock::new(
            get.client.clone(),
            EphemeralNonVolatileStore::new_boxed(),
            None,
        )
        .await
        .unwrap();
        (clock, get, get_driver)
    }

    fn is_near_host_time(time: &Zoned) -> bool {
        time.duration_since(&Zoned::now()).unsigned_abs() < Duration::from_millis(500)
    }

    #[async_test]
    async fn stale_time_does_not_wait_for_host(driver: DefaultDriver) {
        let get = new_get(&driver).await;
        let host_time = new_host_time(&driver, &get).await;
        let initial = host_time.now();

        // The host is served by this thread, so it can't respond until the
        // test yields.
        {
            let mut cached_host_time = cached(&host_time);
            cached_host_time.last_query -= Duration::from_secs(10);
            cached_host_time.anchor -= Duration::from_secs(10);
        }
        let last_query = cached(&host_time).last_query;
        assert!(host_time.now() >= &initial + Duration::from_secs(10));

        let mut timer = PolledTimer::new(&driver);
        while cached(&host_time).last_query == last_query {
            timer.sleep(Duration::from_millis(10)).await;
            host_time.now();
        }
        // The reply replaces the advanced time.
        assert!(host_time.now() < &initial + Duration::from_secs(10));
    }

    #[async_test]
    async fn correction_does_not_go_backwards(driver: DefaultDriver) {
        let get = new_get(&driver).await;
        let host_time = new_host_time(&driver, &get).await;

        // Pretend the local clock ran 800ms ahead of the host over the
        // last second.
        {
            let mut cached_host_time = cached(&host_time);
            cached_host_time.last_query -= Duration::from_secs(1);
            cached_host_time.anchor -= Duration::from_secs(1);
            cached_host_time.time = &cached_host_time.time - Duration::from_millis(200);
        }
        let last_query = cached(&host_time).last_query;
        let ahead = host_time.now();

        let mut timer = PolledTimer::new(&driver);
        while cached(&host_time).last_query == last_query {
            timer.sleep(Duration::from_millis(10)).await;
            assert!(host_time.now() >= ahead);
        }
        assert!(cached(&host_time).time < ahead);
    }

    #[async_test]
    async fn slow_reply_does_not_move_time(driver: DefaultDriver) {
        let get = new_get(&driver).await;
        let host_time = new_host_time(&driver, &get).await;

        // The host is served by this thread, so blocking it delays when the
        // host samples its time.
        let last_query = {
            let mut cached_host_time = cached(&host_time);
            cached_host_time.last_query -= Duration::from_secs(1);
            cached_host_time.last_query
        };
        host_time.now();
        std::thread::sleep(Duration::from_millis(1500));

        let mut timer = PolledTimer::new(&driver);
        while cached(&host_time).last_query == last_query {
            timer.sleep(Duration::from_millis(10)).await;
            host_time.now();
        }
        assert!(is_near_host_time(&host_time.now()));
    }

    #[async_test]
    async fn reply_is_timed_when_it_comes_in(driver: DefaultDriver) {
        let get = new_get(&driver).await;
        let host_time = new_host_time(&driver, &get).await;

        // Pretend the host paused the VM for an hour, and nothing reads the
        // time for a while after the reply to the next query comes in.
        {
            let mut cached_host_time = cached(&host_time);
            cached_host_time.last_query -= Duration::from_secs(1);
            cached_host_time.time = &cached_host_time.time - Duration::from_secs(3600);
        }
        host_time.now();
        PolledTimer::new(&driver)
            .sleep(Duration::from_millis(100))
            .await;
        std::thread::sleep(Duration::from_millis(1500));

        assert!(is_near_host_time(&host_time.now()));
    }

    #[async_test]
    async fn servicing_does_not_go_backwards(_driver: DefaultDriver) {
        let (mut clock, get, _) = new_clock().await;

        // Pretend the local clock ran 800ms ahead of the host.
        cached(&clock.host_time).anchor -= Duration::from_millis(800);
        let ahead = clock.get_time();

        // Service twice, the second time before the time is read.
        for _ in 0..2 {
            let state = clock.save().unwrap();
            clock = UnderhillLocalClock::new(
                get.client.clone(),
                EphemeralNonVolatileStore::new_boxed(),
                Some(state),
            )
            .await
            .unwrap();
        }
        assert!(clock.get_time() >= ahead);
    }

    #[async_test]
    async fn set_time_after_pause(_driver: DefaultDriver) {
        let (mut clock, get, _) = new_clock().await;

        // Pretend the host paused the VM for an hour, a second after the last
        // query.
        {
            let mut cached_host_time = cached(&clock.host_time);
            cached_host_time.last_query -= Duration::from_secs(1);
            cached_host_time.time = &cached_host_time.time - Duration::from_secs(3601);
        }
        clock.set_time(SystemTime::now().into());
        let skew = fetch_skew_from_store(&mut *clock.store)
            .await
            .unwrap()
            .unwrap();
        assert!(skew.as_millis().abs() < 100);

        let state = clock.save().unwrap();
        let mut clock = UnderhillLocalClock::new(
            get.client.clone(),
            EphemeralNonVolatileStore::new_boxed(),
            Some(state),
        )
        .await
        .unwrap();
        let error = clock.get_time() - LocalClockTime::from(SystemTime::now());
        assert!(error.as_millis().abs() < 100);
    }

    #[async_test]
    async fn set_time_queries_the_host_once(_driver: DefaultDriver) {
        let (mut clock, _get, _) = new_clock().await;
        let time = LocalClockTime::from(SystemTime::now());

        // The guest sets the time a register at a time, reading the time
        // before each write. With the cached time a second old, the first
        // read sends a query, the first write waits for its reply, and the
        // rest use the cached time.
        cached(&clock.host_time).last_query -= Duration::from_secs(1);
        let before = Instant::now();
        clock.get_time();
        let after = Instant::now();
        for _ in 0..7 {
            clock.get_time();
            clock.set_time(time);
            let last_query = cached(&clock.host_time).last_query;
            assert!(before <= last_query && last_query <= after);
        }
    }

    #[async_test]
    async fn set_time_does_not_use_an_old_query(_driver: DefaultDriver) {
        // Block the GED's thread for 1.5s so that a query sent now is still
        // outstanding a second later.
        let (mut clock, _get, get_driver) = new_clock().await;
        let (started, wait) = std::sync::mpsc::channel();
        get_driver
            .spawn("block", async move {
                started.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(1500));
            })
            .detach();
        wait.recv().unwrap();

        // The host may have sampled its time for a query sent a second ago
        // before pausing the VM, so set the time from a new query.
        cached(&clock.host_time).last_query -= Duration::from_secs(1);
        clock.get_time();
        std::thread::sleep(Duration::from_millis(1100));
        let before = Instant::now();
        clock.set_time(LocalClockTime::from(SystemTime::now()));
        assert!(cached(&clock.host_time).last_query >= before);
        // The time is set as of the write, not as of the reply.
        let skew = fetch_skew_from_store(&mut *clock.store)
            .await
            .unwrap()
            .unwrap();
        assert!(skew.as_millis().abs() < 100, "{skew:?}");
    }

    #[async_test]
    async fn first_read_waits_for_host(_driver: DefaultDriver) {
        let (mut clock, get, get_driver) = new_clock().await;
        let state = clock.save().unwrap();

        // Block the GED's thread for 1s.
        let (started, wait) = std::sync::mpsc::channel();
        get_driver
            .spawn("block", async move {
                started.send(()).unwrap();
                std::thread::sleep(Duration::from_secs(1));
            })
            .detach();
        wait.recv().unwrap();

        // Restoring doesn't query the host, and the first read waits for it.
        let before = Instant::now();
        let mut clock = UnderhillLocalClock::new(
            get.client.clone(),
            EphemeralNonVolatileStore::new_boxed(),
            Some(state),
        )
        .await
        .unwrap();
        assert!(before.elapsed() < Duration::from_millis(500));
        let time = clock.get_time();
        assert!(before.elapsed() >= Duration::from_millis(900));
        let error = time - clock.offset_from_host_time - LocalClockTime::from(SystemTime::now());
        assert!(error.as_millis().abs() < 100);
    }
}
