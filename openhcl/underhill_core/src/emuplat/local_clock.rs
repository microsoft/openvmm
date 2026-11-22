// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use self::host_time::HostSystemTimeAccess;
use cvm_tracing::CVM_ALLOWED;
use inspect::Inspect;
use local_clock::LocalClock;
use local_clock::LocalClockDelta;
use local_clock::LocalClockTime;
use std::task::Context;
use std::task::Poll;
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

    fn poll_time(&mut self, cx: &mut Context<'_>) -> Poll<LocalClockTime> {
        self.host_time
            .poll_now(cx)
            .map(|time| LocalClockTime::from(time) + self.offset_from_host_time)
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
    use guest_emulation_transport::api::Time;
    use inspect::Inspect;
    use jiff::Zoned;
    use parking_lot::Mutex;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Waker;
    use std::time::Duration;
    use std::time::Instant;

    /// Encapsulates all the nitty-gritty details of how real time gets fetched
    /// from the Host.
    #[derive(Debug)]
    pub struct HostSystemTimeAccess {
        get: guest_emulation_transport::GuestEmulationTransportClient,
        pub(super) cached_host_time: Mutex<Option<(Instant, Zoned)>>,
        pub(super) pending_query: Mutex<Option<PendingQuery>>,
    }

    /// A host time query sent by [`HostSystemTimeAccess::poll_now`].
    pub(super) struct PendingQuery {
        /// When the query was sent.
        pub(super) sent: Instant,
        reply: Pin<Box<dyn Future<Output = Time> + Send>>,
        /// The waker of the caller waiting for the reply, to wake if `now`
        /// takes the reply instead.
        waker: Option<Waker>,
    }

    impl std::fmt::Debug for PendingQuery {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("PendingQuery")
                .field("sent", &self.sent)
                .finish_non_exhaustive()
        }
    }

    impl Inspect for HostSystemTimeAccess {
        fn inspect(&self, req: inspect::Request<'_>) {
            let HostSystemTimeAccess {
                get: _,
                cached_host_time,
                pending_query: _,
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
                pending_query: Mutex::new(None),
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

            // Take the reply to a query `poll_now` sent, as its caller would
            // have waited for it in `now`.
            if let Some(PendingQuery { sent, reply, waker }) = self.pending_query.lock().take() {
                let new_time = pal_async::local::block_with_io(|_| reply).to_jiff();
                *cached_host_time = Some((sent, new_time));
                if let Some(waker) = waker {
                    waker.wake();
                }
            }

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

        /// Poll for the host's current time, as `now` returns it, without
        /// waiting for the host: if `now` would query the host, send the query
        /// and return `Pending` until the reply comes in.
        pub fn poll_now(&self, cx: &mut Context<'_>) -> Poll<Zoned> {
            let now = Instant::now();
            let mut cached_host_time = self.cached_host_time.lock();

            if let Some((last_query, cached_time)) = &*cached_host_time
                && now.duration_since(*last_query) < Duration::from_secs(1)
            {
                return Poll::Ready(cached_time.clone());
            }

            let mut pending_query = self.pending_query.lock();
            let query = pending_query.get_or_insert_with(|| {
                let get = self.get.clone();
                PendingQuery {
                    sent: now,
                    reply: Box::pin(async move { get.host_time().await }),
                    waker: None,
                }
            });
            let Poll::Ready(reply) = query.reply.as_mut().poll(cx) else {
                query.waker = Some(cx.waker().clone());
                return Poll::Pending;
            };
            let new_time = reply.to_jiff();
            *cached_host_time = Some((query.sent, new_time.clone()));
            *pending_query = None;
            Poll::Ready(new_time)
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

    fn poll_time(&mut self, cx: &mut Context<'_>) -> Poll<LocalClockTime> {
        self.0.lock().poll_time(cx)
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
    use guest_emulation_transport::test_utilities::TestGet;
    use local_clock::LocalClock;
    use local_clock::LocalClockTime;
    use pal_async::DefaultDriver;
    use pal_async::DefaultPool;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Wake;
    use std::task::Waker;
    use std::time::Duration;
    use std::time::Instant;
    use test_with_tracing::test;
    use vmcore::non_volatile_store::EphemeralNonVolatileStore;

    /// Return the clock the RTC uses, with the GET served on its own thread,
    /// and with the cached host time a second old.
    async fn new_rtc_clock() -> (ArcMutexUnderhillLocalClock, TestGet, DefaultDriver) {
        let (_, get_driver) = DefaultPool::spawn_on_thread("get");
        let get = guest_emulation_transport::test_utilities::new_transport_pair(
            get_driver.clone(),
            None,
            get_protocol::ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await;
        let clock = UnderhillLocalClock::new(
            get.client.clone(),
            EphemeralNonVolatileStore::new_boxed(),
            None,
        )
        .await
        .unwrap();
        let clock = ArcMutexUnderhillLocalClock(Arc::new(parking_lot::Mutex::new(clock)));
        if let Some((last_query, _)) = &mut *clock.0.lock().host_time.cached_host_time.lock() {
            *last_query -= Duration::from_secs(1);
        }
        (clock, get, get_driver)
    }

    /// Hold up the GET's thread until the returned sender is sent to, or for
    /// ten seconds at most, so that a test that would wait for the GET fails
    /// rather than hangs.
    fn hold(get_driver: &DefaultDriver) -> mpsc::Sender<()> {
        let (started, wait) = mpsc::channel();
        let (release, held) = mpsc::channel();
        get_driver
            .spawn("hold", async move {
                started.send(()).unwrap();
                let _ = held.recv_timeout(Duration::from_secs(10));
            })
            .detach();
        wait.recv().unwrap();
        release
    }

    /// A waker that sends on a channel when woken.
    struct Notify(parking_lot::Mutex<mpsc::Sender<()>>);

    impl Wake for Notify {
        fn wake(self: Arc<Self>) {
            let _ = self.0.lock().send(());
        }
    }

    fn notify() -> (Waker, mpsc::Receiver<()>) {
        let (send, recv) = mpsc::channel();
        (
            Waker::from(Arc::new(Notify(parking_lot::Mutex::new(send)))),
            recv,
        )
    }

    #[async_test]
    async fn poll_time_does_not_wait_for_host(_driver: DefaultDriver) {
        let (clock, _get, get_driver) = new_rtc_clock().await;
        let mut rtc_clock = clock.new_linked_clock();
        let (waker, woken) = notify();
        let mut cx = Context::from_waker(&waker);

        // With the host held up, polling for the time sends a query and
        // returns without waiting for the reply.
        let release = hold(&get_driver);
        let before = Instant::now();
        assert!(rtc_clock.poll_time(&mut cx).is_pending());
        let after = Instant::now();
        release.send(()).unwrap();

        // Once the reply comes in, the time is the host's reply, cached as of
        // when the query was sent, as when waiting for it.
        woken.recv_timeout(Duration::from_secs(10)).unwrap();
        let Poll::Ready(time) = rtc_clock.poll_time(&mut cx) else {
            panic!("time not ready");
        };
        let clock = clock.0.lock();
        let (last_query, cached_time) = clock.host_time.cached_host_time.lock().clone().unwrap();
        assert!(before <= last_query && last_query <= after);
        assert_eq!(
            time,
            LocalClockTime::from(cached_time) + clock.offset_from_host_time
        );
    }

    #[async_test]
    async fn get_time_takes_the_reply_polled_for(_driver: DefaultDriver) {
        let (clock, _get, get_driver) = new_rtc_clock().await;
        let mut rtc_clock = clock.new_linked_clock();
        let (waker, woken) = notify();
        let mut cx = Context::from_waker(&waker);
        let release = hold(&get_driver);
        let before = Instant::now();
        assert!(rtc_clock.poll_time(&mut cx).is_pending());
        let after = Instant::now();

        // Reading the time waits for the query already sent, rather than
        // sending another, and wakes the caller polling for it.
        std::thread::scope(|s| {
            let time = s.spawn(|| clock.new_linked_clock().get_time());
            std::thread::sleep(Duration::from_millis(100));
            release.send(()).unwrap();
            time.join().unwrap();
        });
        woken.recv_timeout(Duration::from_secs(10)).unwrap();
        let clock = clock.0.lock();
        assert!(clock.host_time.pending_query.lock().is_none());
        let (last_query, _) = clock.host_time.cached_host_time.lock().clone().unwrap();
        assert!(before <= last_query && last_query <= after);
    }
}
