// Copyright (C) Microsoft Corporation. All rights reserved.

use self::host_time::HostSystemTimeAccess;
use inspect::Inspect;
use local_clock::LocalClock;
use local_clock::LocalClockDelta;
use local_clock::LocalClockTime;
use vmcore::non_volatile_store::NonVolatileStore;
use vmcore::non_volatile_store::NonVolatileStoreError;
use vmcore::save_restore::SaveRestore;

const NANOS_IN_SECOND: i64 = 1_000_000_000;
const NANOS_100_IN_SECOND: i64 = NANOS_IN_SECOND / 100;
const MILLIS_IN_TWO_DAYS: i64 = 100 * 60 * 60 * 24 * 2;

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
                        let skew = time::Duration::seconds(skew.whole_seconds().into());
                        tracing::info!(?skew, "no saved skew found: defaulting to host local time");
                        skew.into()
                    }
                }
            }
        };

        // prevent guests from persisting an RTC time in the distant past,
        // which could be used to circumvent time-based licensing checks.
        let neg_two_days = LocalClockDelta::from_millis(-MILLIS_IN_TWO_DAYS);
        if this.offset_from_host_time < neg_two_days {
            this.offset_from_host_time = neg_two_days;
            tracing::warn!("Guest time was more than two days in the past.");
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
    let skew = time::Duration::new(
        raw_skew_100ns / NANOS_100_IN_SECOND,
        (raw_skew_100ns % NANOS_100_IN_SECOND) as i32,
    );
    tracing::info!(?skew, "restored existing RTC skew");
    Ok(Some(skew.into()))
}

impl LocalClock for UnderhillLocalClock {
    fn get_time(&mut self) -> LocalClockTime {
        LocalClockTime::from(self.host_time.now()) + self.offset_from_host_time
    }

    fn set_time(&mut self, new_time: LocalClockTime) {
        let new_skew = new_time - LocalClockTime::from(self.host_time.now());
        self.offset_from_host_time = new_skew;

        // persist the skew in units of 100ns
        let raw_skew: i64 = (time::Duration::from(new_skew).whole_nanoseconds() / 100)
            .try_into()
            .unwrap();

        // TODO: swap this out for a non-blocking version that guarantees the skew is written out _eventually_
        let res =
            pal_async::local::block_with_io(|_| self.store.persist(raw_skew.to_le_bytes().into()));
        if let Err(err) = res {
            tracing::error!(
                err = &err as &dyn std::error::Error,
                "failed to persist RTC skew"
            );
        }
    }
}

mod host_time {
    use super::NANOS_100_IN_SECOND;
    use inspect::Inspect;
    use parking_lot::Mutex;
    use std::time::Duration;
    use std::time::Instant;
    use time::OffsetDateTime;
    use time::UtcOffset;

    /// Encapsulates all the nitty-gritty details of how real time gets fetched
    /// from the Host.
    #[derive(Debug)]
    pub struct HostSystemTimeAccess {
        get: guest_emulation_transport::GuestEmulationTransportClient,
        cached_host_time: Mutex<Option<(Instant, OffsetDateTime)>>,
    }

    impl Inspect for HostSystemTimeAccess {
        fn inspect(&self, req: inspect::Request<'_>) {
            let HostSystemTimeAccess {
                get: _,
                cached_host_time,
            } = self;

            let mut res = req.respond();

            if let Some((last_query, cached_time)) = *cached_host_time.lock() {
                res.display_debug("since_last_query", &(Instant::now() - last_query))
                    .display("cached_time", &cached_time);
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
        pub fn now(&self) -> OffsetDateTime {
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

            match *cached_host_time {
                Some((last_query, cached_time))
                    if now.duration_since(last_query) < Duration::from_secs(1) =>
                {
                    cached_time
                }
                _ => {
                    // TODO: this block_on really ain't great, but since we're
                    // not hammering the GET on _each_ access, it's okay for now...
                    let new_time = get_time_to_date_time(pal_async::local::block_with_io(|_| {
                        self.get.host_time()
                    }));
                    *cached_host_time = Some((now, new_time));
                    new_time
                }
            }
        }
    }

    fn get_time_to_date_time(time: guest_emulation_transport::api::Time) -> OffsetDateTime {
        const WINDOWS_EPOCH: OffsetDateTime = time::macros::datetime!(1601-01-01 0:00 UTC);

        let host_time_since_windows_epoch = time::Duration::new(
            time.utc / NANOS_100_IN_SECOND,
            (time.utc % NANOS_100_IN_SECOND) as i32,
        );

        let host_time_utc = WINDOWS_EPOCH + host_time_since_windows_epoch;

        // the timezone reported by the host is negative minutes from utc
        // i.e. Localtime = UTC - TimeZone
        host_time_utc.to_offset(
            UtcOffset::from_whole_seconds(-time.time_zone as i32 * 60)
                .expect("unexpectedly large timezone offset"),
        )
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
