// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The timesync IC.

use crate::common::IcPipe;
use crate::common::NegotiateState;
use crate::common::Versions;
use async_trait::async_trait;
use guestmem::GuestMemory;
use hyperv_ic_protocol::timesync as proto;
use inspect::Inspect;
use inspect::InspectMut;
use pal_async::driver::Driver;
use pal_async::timer::Instant;
use pal_async::timer::PolledTimer;
use std::future::pending;
use task_control::Cancelled;
use task_control::StopTask;
use vmbus_channel::RawAsyncChannel;
use vmbus_channel::bus::ChannelType;
use vmbus_channel::bus::OfferParams;
use vmbus_channel::channel::ChannelOpenError;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_channel::simple::SaveRestoreSimpleVmbusDevice;
use vmbus_channel::simple::SimpleVmbusDevice;
use vmcore::reference_time::ReferenceTimeSource;
use zerocopy::IntoBytes;

const TIMESYNC_VERSIONS: &[hyperv_ic_protocol::Version] = &[proto::TIMESYNC_VERSION_4];

/// Send samples every 5 seconds.
const SAMPLE_PERIOD: std::time::Duration = std::time::Duration::from_secs(5);

/// Timesync IC device.
#[derive(InspectMut)]
#[non_exhaustive]
pub struct TimesyncIc {
    #[inspect(skip)]
    timer: PolledTimer,
    #[inspect(skip)]
    ref_time: ReferenceTimeSource,
}

#[doc(hidden)]
#[derive(InspectMut)]
pub struct TimesyncChannel {
    #[inspect(mut)]
    pipe: IcPipe,
    state: ChannelState,
}

#[derive(Inspect)]
#[inspect(external_tag)]
enum ChannelState {
    Negotiate(#[inspect(rename = "state")] NegotiateState),
    Ready {
        versions: Versions,
        state: ReadyState,
    },
    Failed,
}

#[derive(Inspect)]
#[inspect(external_tag)]
enum ReadyState {
    SleepUntilNextSample {
        #[inspect(with = "inspect_instant")]
        next_sample: Instant,
    },
    SendMessage {
        is_sync: bool,
    },
    WaitForResponse,
}

fn inspect_instant(&instant: &Instant) -> inspect::AsDisplay<jiff::Timestamp> {
    let now = Instant::now();
    let time = jiff::Timestamp::now();
    let sd = if now <= instant {
        jiff::SignedDuration::try_from(instant - now).unwrap()
    } else {
        -jiff::SignedDuration::try_from(now - instant).unwrap()
    };
    inspect::AsDisplay(time + sd)
}

impl TimesyncIc {
    /// Create a new timesync IC.
    pub fn new(driver: &(impl Driver + ?Sized), ref_time: ReferenceTimeSource) -> Self {
        Self {
            timer: PolledTimer::new(driver),
            ref_time,
        }
    }
}

#[async_trait]
impl SimpleVmbusDevice for TimesyncIc {
    type SavedState = save_restore::state::SavedState;
    type Runner = TimesyncChannel;

    fn offer(&self) -> OfferParams {
        OfferParams {
            interface_name: "timesync_ic".to_owned(),
            instance_id: proto::INSTANCE_ID,
            interface_id: proto::INTERFACE_ID,
            channel_type: ChannelType::Pipe { message_mode: true },
            ..Default::default()
        }
    }

    fn inspect(&mut self, req: inspect::Request<'_>, runner: Option<&mut Self::Runner>) {
        req.respond().merge(self).merge(runner);
    }

    fn open(
        &mut self,
        channel: RawAsyncChannel<GpadlRingMem>,
        _guest_memory: GuestMemory,
    ) -> Result<Self::Runner, ChannelOpenError> {
        TimesyncChannel::new(channel, None)
    }

    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        runner: &mut Self::Runner,
    ) -> Result<(), Cancelled> {
        stop.until_stopped(async { runner.process(self).await })
            .await
    }

    fn supports_save_restore(
        &mut self,
    ) -> Option<
        &mut dyn SaveRestoreSimpleVmbusDevice<SavedState = Self::SavedState, Runner = Self::Runner>,
    > {
        Some(self)
    }
}

impl TimesyncChannel {
    fn new(
        channel: RawAsyncChannel<GpadlRingMem>,
        restore_state: Option<ChannelState>,
    ) -> Result<Self, ChannelOpenError> {
        let pipe = IcPipe::new(channel)?;
        Ok(Self {
            pipe,
            state: restore_state.unwrap_or(ChannelState::Negotiate(NegotiateState::default())),
        })
    }

    async fn process(&mut self, ic: &mut TimesyncIc) -> ! {
        loop {
            if let Err(err) = self.process_state_machine(ic).await {
                tracing::error!(
                    error = err.as_ref() as &dyn std::error::Error,
                    "timesync ic error"
                );
                self.state = ChannelState::Failed;
            }
        }
    }

    async fn process_state_machine(&mut self, ic: &mut TimesyncIc) -> anyhow::Result<()> {
        match self.state {
            ChannelState::Negotiate(ref mut state) => {
                if let Some(versions) = self.pipe.negotiate(state, TIMESYNC_VERSIONS).await? {
                    tracelimit::info_ratelimited!(
                        framework = %versions.framework_version,
                        version = %versions.message_version,
                        "timesync versions negotiated"
                    );
                    // Send a sync message to provide the initial time.
                    self.state = ChannelState::Ready {
                        versions,
                        state: ReadyState::SendMessage { is_sync: true },
                    };
                }
            }
            ChannelState::Ready {
                ref versions,
                ref mut state,
            } => match *state {
                ReadyState::SleepUntilNextSample { next_sample } => {
                    ic.timer.sleep_until(next_sample).await;
                    *state = ReadyState::SendMessage { is_sync: false };
                }
                ReadyState::SendMessage { is_sync } => {
                    // Wait for space in the ring before computing the next time to ensure that there is not
                    // too much drift before the guest sees it.
                    let message_size = size_of::<hyperv_ic_protocol::Header>()
                        + size_of::<proto::TimesyncMessageV4>();
                    self.pipe.pipe.wait_write_ready(message_size).await?;

                    // In case the backend doesn't provide a system time
                    // snapshot, capture the system time as soon as possible to
                    // avoid drift.
                    let r = ic.ref_time.now();
                    let ref_time = r.ref_time;
                    let time = r.system_time.unwrap_or_else(jiff::Timestamp::now);

                    let message = proto::TimesyncMessageV4 {
                        parent_time: ((time.duration_since(proto::EPOCH).as_nanos() / 100) as u64)
                            .into(),
                        vm_reference_time: ref_time,
                        flags: proto::TimesyncFlags::new()
                            .with_sync(is_sync)
                            .with_sample(!is_sync),
                        leap_indicator: 0,
                        stratum: 0,
                        reserved: [0; 5],
                    };
                    self.pipe
                        .write_message(
                            versions,
                            hyperv_ic_protocol::MessageType::TIME_SYNC,
                            hyperv_ic_protocol::HeaderFlags::new()
                                .with_request(true)
                                .with_transaction(true),
                            message.as_bytes(),
                        )
                        .await?;

                    if is_sync {
                        tracelimit::info_ratelimited!(%time, ref_time, "sent time sync");
                    } else {
                        tracing::debug!(%time, ref_time, "sent time sample");
                    }

                    // This was sent as a transaction, which is kind of
                    // pointless (we don't need the response), but Windows
                    // ignores non-transactional time sync requests.
                    *state = ReadyState::WaitForResponse;
                }
                ReadyState::WaitForResponse => {
                    self.pipe.read_response().await?;
                    // Send another sample in a few seconds.
                    *state = ReadyState::SleepUntilNextSample {
                        next_sample: Instant::now() + SAMPLE_PERIOD,
                    };
                }
            },
            ChannelState::Failed => pending().await,
        }
        Ok(())
    }
}

mod save_restore {
    use super::*;

    pub mod state {
        use crate::common::SavedVersions;
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "timesync_ic")]
        pub struct SavedState {
            #[mesh(1)]
            pub versions: Option<SavedVersions>,
            #[mesh(2)]
            pub waiting_on_version: bool,
            #[mesh(3)]
            pub waiting_on_response: bool,
            #[mesh(4)]
            pub failed: bool,
        }
    }

    impl SaveRestoreSimpleVmbusDevice for TimesyncIc {
        fn save_open(&mut self, runner: &Self::Runner) -> state::SavedState {
            let TimesyncChannel { pipe: _, state } = runner;
            let (versions, waiting_on_version, waiting_on_response, failed) = match state {
                ChannelState::Negotiate(state) => {
                    let waiting_on_version = match state {
                        NegotiateState::SendVersion | NegotiateState::Invalid => false,
                        NegotiateState::WaitVersion => true,
                    };
                    (None, waiting_on_version, false, false)
                }
                ChannelState::Ready { versions, state } => {
                    let waiting_on_response = match state {
                        ReadyState::SleepUntilNextSample { next_sample: _ }
                        | ReadyState::SendMessage { is_sync: _ } => false,
                        ReadyState::WaitForResponse => true,
                    };
                    (Some((*versions).into()), false, waiting_on_response, false)
                }
                ChannelState::Failed => (None, false, false, true),
            };
            state::SavedState {
                versions,
                waiting_on_version,
                waiting_on_response,
                failed,
            }
        }

        fn restore_open(
            &mut self,
            saved_state: Self::SavedState,
            channel: RawAsyncChannel<GpadlRingMem>,
        ) -> Result<Self::Runner, ChannelOpenError> {
            let state::SavedState {
                versions,
                waiting_on_version,
                waiting_on_response,
                failed,
            } = saved_state;
            let state = if failed {
                ChannelState::Failed
            } else if let Some(versions) = versions {
                // Time may have stopped while the VM was saved, so send a new
                // sync message rather than resuming the sample timer. If a
                // response was outstanding it must be consumed first.
                let state = if waiting_on_response {
                    ReadyState::WaitForResponse
                } else {
                    ReadyState::SendMessage { is_sync: true }
                };
                ChannelState::Ready {
                    versions: versions.into(),
                    state,
                }
            } else {
                ChannelState::Negotiate(if waiting_on_version {
                    NegotiateState::WaitVersion
                } else {
                    NegotiateState::SendVersion
                })
            };
            TimesyncChannel::new(channel, Some(state))
        }
    }
}
