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
    /// Create a new [`UnderhillLocalClock`].
    pub async fn new(
        get: guest_emulation_transport::GuestEmulationTransportClient,
        store: Box<dyn NonVolatileStore>,
        saved_state: Option<<Self as SaveRestore>::SavedState>,
    ) -> anyhow::Result<Self> {
        let host_time = HostSystemTimeAccess::new(get).await;

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
    use guest_emulation_transport::api::Time;
    use inspect::Inspect;
    use jiff::SignedDuration;
    use jiff::Zoned;
    use parking_lot::Mutex;
    use std::future::Future;
    use std::pin::Pin;
    use std::time::Duration;
    use std::time::Instant;

    /// Encapsulates all the nitty-gritty details of how real time gets fetched
    /// from the Host.
    #[derive(Debug, Inspect)]
    pub struct HostSystemTimeAccess {
        #[inspect(skip)]
        get: guest_emulation_transport::GuestEmulationTransportClient,
        #[inspect(flatten)]
        pub(super) cached_host_time: Mutex<CachedHostTime>,
    }

    #[derive(Inspect)]
    pub(super) struct CachedHostTime {
        /// When the query for `time` was sent.
        #[inspect(
            rename = "since_last_query",
            with = "|x| inspect::AsDebug(x.elapsed())"
        )]
        pub(super) last_query: Instant,
        /// The host time from the last query.
        #[inspect(rename = "cached_time", display)]
        pub(super) time: Zoned,
        /// The latest time returned.
        #[inspect(display)]
        latest: Zoned,
        /// An outstanding query, and when it was sent.
        #[inspect(with = "Option::is_some")]
        refresh: Option<(Instant, Pin<Box<dyn Future<Output = Time> + Send>>)>,
    }

    impl std::fmt::Debug for CachedHostTime {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let Self {
                last_query,
                time,
                latest,
                refresh,
            } = self;

            f.debug_struct("CachedHostTime")
                .field("last_query", last_query)
                .field("time", time)
                .field("latest", latest)
                .field("refresh", &refresh.as_ref().map(|(sent, _)| sent))
                .finish()
        }
    }

    impl HostSystemTimeAccess {
        pub async fn new(
            get: guest_emulation_transport::GuestEmulationTransportClient,
        ) -> HostSystemTimeAccess {
            let last_query = Instant::now();
            let time = get.host_time().await.to_jiff();
            HostSystemTimeAccess {
                get,
                cached_host_time: Mutex::new(CachedHostTime {
                    last_query,
                    latest: time.clone(),
                    time,
                    refresh: None,
                }),
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
            //
            // This is called on the VP thread, so don't wait for the host to
            // respond. Until it does, keep advancing the previous host time;
            // if the host paused the VM, which VTL2 can't see, this is behind
            // by the pause until the reply comes in.
            let now = Instant::now();
            let mut cached_host_time = self.cached_host_time.lock();
            let cached_host_time = &mut *cached_host_time;

            if cached_host_time.refresh.is_none()
                && now.duration_since(cached_host_time.last_query) >= Duration::from_secs(1)
            {
                let get = self.get.clone();
                cached_host_time.refresh =
                    Some((now, Box::pin(async move { get.host_time().await })));
            }

            if let Some((sent, refresh)) = &mut cached_host_time.refresh
                && let Some(time) = refresh.now_or_never()
            {
                cached_host_time.last_query = *sent;
                cached_host_time.time = time.to_jiff();
                cached_host_time.refresh = None;
            }

            let time = &cached_host_time.time + now.duration_since(cached_host_time.last_query);

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
    use super::host_time::HostSystemTimeAccess;
    use guest_emulation_transport::test_utilities::TestGet;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::timer::PolledTimer;
    use std::time::Duration;
    use test_with_tracing::test;

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

    #[async_test]
    async fn stale_time_does_not_wait_for_host(driver: DefaultDriver) {
        let get = new_get(&driver).await;
        let host_time = HostSystemTimeAccess::new(get.client.clone()).await;
        let initial = host_time.now();

        // The host is served by this thread, so it can't respond until the
        // test yields.
        {
            let mut cached_host_time = host_time.cached_host_time.lock();
            cached_host_time.last_query -= Duration::from_secs(10);
        }
        let last_query = host_time.cached_host_time.lock().last_query;
        assert!(host_time.now() >= &initial + Duration::from_secs(10));

        let mut timer = PolledTimer::new(&driver);
        while host_time.cached_host_time.lock().last_query == last_query {
            timer.sleep(Duration::from_millis(10)).await;
            host_time.now();
        }
        // The reply replaces the advanced time.
        assert!(host_time.now() < &initial + Duration::from_secs(10));
    }

    #[async_test]
    async fn correction_does_not_go_backwards(driver: DefaultDriver) {
        let get = new_get(&driver).await;
        let host_time = HostSystemTimeAccess::new(get.client.clone()).await;

        // Pretend the local clock ran 800ms ahead of the host over the
        // last second.
        {
            let mut cached_host_time = host_time.cached_host_time.lock();
            cached_host_time.last_query -= Duration::from_secs(1);
            cached_host_time.time = &cached_host_time.time - Duration::from_millis(200);
        }
        let last_query = host_time.cached_host_time.lock().last_query;
        let ahead = host_time.now();

        let mut timer = PolledTimer::new(&driver);
        while host_time.cached_host_time.lock().last_query == last_query {
            timer.sleep(Duration::from_millis(10)).await;
            assert!(host_time.now() >= ahead);
        }
        assert!(host_time.cached_host_time.lock().time < ahead);
    }
}
