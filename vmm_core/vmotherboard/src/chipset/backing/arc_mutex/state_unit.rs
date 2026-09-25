// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::VmmChipsetDevice;
use async_trait::async_trait;
use closeable_mutex::CloseableMutex;
use futures::FutureExt;
use futures::StreamExt;
use futures::task::ArcWake;
use futures::task::WakerRef;
use futures::task::waker_ref;
use inspect::InspectMut;
use state_unit::StateRequest;
use state_unit::StateUnit;
use std::sync::Arc;
use std::task::Context;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;
use vmcore::slim_event::SlimEvent;

pub struct ArcMutexChipsetDeviceUnit {
    device: Arc<CloseableMutex<dyn DynDevice>>,
    poll_event: Arc<PollEvent>,
    running: bool,
    omit_saved_state: bool,
}

impl InspectMut for ArcMutexChipsetDeviceUnit {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.device.lock().inspect_mut(req);
    }
}

/// Object-safe trait for the subset of [`VmmChipsetDevice`] that we use here.
#[async_trait]
trait DynDevice: InspectMut + Send {
    async fn start(&mut self) -> anyhow::Result<()>;
    async fn stop(&mut self);
    async fn quiesce_input(&mut self) -> anyhow::Result<()>;
    async fn resume_input(&mut self) -> anyhow::Result<()>;
    async fn reset(&mut self);
    async fn advance_time(&mut self, duration: std::time::Duration) -> anyhow::Result<()>;
    fn poll_device(&mut self, cx: &mut Context<'_>);
    async fn save(&mut self) -> Result<SavedStateBlob, SaveError>;
    async fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError>;
}

#[async_trait]
impl<T: VmmChipsetDevice> DynDevice for T {
    async fn start(&mut self) -> anyhow::Result<()> {
        self.start_fallible().await
    }

    async fn stop(&mut self) {
        self.stop().await
    }
    async fn quiesce_input(&mut self) -> anyhow::Result<()> {
        vmcore::device_state::ChangeDeviceState::quiesce_input(self).await
    }
    async fn resume_input(&mut self) -> anyhow::Result<()> {
        vmcore::device_state::ChangeDeviceState::resume_input(self).await
    }

    async fn reset(&mut self) {
        self.reset().await
    }

    async fn advance_time(&mut self, duration: std::time::Duration) -> anyhow::Result<()> {
        self.advance_time(duration).await
    }

    fn poll_device(&mut self, cx: &mut Context<'_>) {
        if let Some(poll) = self.supports_poll_device() {
            poll.poll_device(cx);
        }
    }

    async fn save(&mut self) -> Result<SavedStateBlob, SaveError> {
        self.save().await
    }

    async fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError> {
        self.restore(state).await
    }
}

struct PollEvent(SlimEvent);

impl ArcWake for PollEvent {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.0.signal();
    }
}

impl ArcMutexChipsetDeviceUnit {
    pub fn new(
        device: Arc<CloseableMutex<impl 'static + VmmChipsetDevice>>,
        omit_saved_state: bool,
    ) -> Self {
        Self {
            device,
            poll_event: Arc::new(PollEvent(SlimEvent::new())),
            running: false,
            omit_saved_state,
        }
    }

    pub async fn run(mut self, mut state_change: mesh::Receiver<StateRequest>) -> Self {
        loop {
            enum Event<'a> {
                StateChange(StateRequest),
                Poll(WakerRef<'a>),
            }

            // Wait for poll requests.
            let poll_fut = async {
                if self.running {
                    self.poll_event.0.wait().await;
                    return waker_ref(&self.poll_event);
                }
                // The device is not running. Never
                // complete this future.
                std::future::pending().await
            };

            let event = futures::select! {
                req = state_change.next() => {
                    if let Some(req) = req {
                        Event::StateChange(req)
                    } else {
                        break;
                    }
                }
                waker = poll_fut.fuse() => {
                    Event::Poll(waker)
                }
            };

            match event {
                Event::StateChange(req) => {
                    req.apply(&mut self).await;
                }
                Event::Poll(waker) => {
                    let mut device = self.device.lock();
                    device.poll_device(&mut Context::from_waker(&waker));
                }
            }
        }
        self
    }
}

impl StateUnit for ArcMutexChipsetDeviceUnit {
    async fn start(&mut self) -> anyhow::Result<()> {
        self.device.clone().close().start().await?;
        self.running = true;

        // Poll the device at least once after startup completes.
        let mut device = self.device.lock();
        device.poll_device(&mut Context::from_waker(&waker_ref(&self.poll_event)));
        Ok(())
    }

    async fn stop(&mut self) {
        self.device.clone().close().stop().await;
        self.running = false;
        // FUTURE: consider closing the mutex while the device is stopped to
        // find bugs. This may be difficult or not worth it since it requires
        // that:
        //
        // 1. all cross-device dependencies are exactly correct.
        // 2. no device manipulation happens externally to normal VM operation
        //    (e.g., no calls are made to the device while the VM is stopped).
        //
        // These are currently not true.
    }

    async fn quiesce_input(&mut self) -> anyhow::Result<()> {
        self.device.clone().close().quiesce_input().await
    }

    async fn resume_input(&mut self) -> anyhow::Result<()> {
        self.device.clone().close().resume_input().await
    }

    async fn reset(&mut self) -> anyhow::Result<()> {
        self.device.clone().close().reset().await;
        Ok(())
    }

    async fn advance_time(&mut self, duration: std::time::Duration) -> anyhow::Result<()> {
        self.device.clone().close().advance_time(duration).await
    }

    async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        if self.omit_saved_state {
            return Ok(None);
        }

        let state = self.device.clone().close().save().await?;
        Ok(Some(state))
    }

    async fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError> {
        self.device.clone().close().restore(state).await
    }
}

#[cfg(test)]
mod tests {
    use super::ArcMutexChipsetDeviceUnit;
    use chipset_device::ChipsetDevice;
    use closeable_mutex::CloseableMutex;
    use futures::poll;
    use inspect::InspectMut;
    use pal_async::async_test;
    use state_unit::StateUnit;
    use std::sync::Arc;
    use std::task::Poll;
    use test_with_tracing::test;
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::save_restore::AsyncSaveRestore;
    use vmcore::save_restore::NoSavedState;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SavedStateBlob;

    #[derive(InspectMut)]
    struct Device {
        #[inspect(skip)]
        gate: Option<mesh::OneshotReceiver<()>>,
        restored: bool,
    }

    impl ChipsetDevice for Device {}

    impl ChangeDeviceState for Device {
        fn start(&mut self) {}
        async fn stop(&mut self) {}
        async fn reset(&mut self) {}
    }

    impl AsyncSaveRestore for Device {
        type SavedState = NoSavedState;

        async fn save(&mut self) -> Result<NoSavedState, SaveError> {
            if let Some(gate) = self.gate.take() {
                gate.await.map_err(|e| SaveError::Other(e.into()))?;
            }
            Ok(NoSavedState)
        }

        async fn restore(&mut self, _: NoSavedState) -> Result<(), RestoreError> {
            if let Some(gate) = self.gate.take() {
                gate.await.map_err(|e| RestoreError::Other(e.into()))?;
            }
            self.restored = true;
            Ok(())
        }
    }

    #[async_test]
    async fn save_restore_waits_with_exclusive_device_access() {
        let (send, recv) = mesh::oneshot();
        let device = Arc::new(CloseableMutex::new(Device {
            gate: Some(recv),
            restored: false,
        }));
        let mut unit = ArcMutexChipsetDeviceUnit::new(device.clone(), false);
        let state = {
            let mut save = std::pin::pin!(unit.save());
            assert!(poll!(&mut save).is_pending());
            assert!(device.lock_if_open().is_none());
            send.send(());
            save.await.unwrap().unwrap()
        };
        assert!(device.lock_if_open().is_some());

        let (send, recv) = mesh::oneshot();
        device.lock().gate = Some(recv);
        {
            let mut restore = std::pin::pin!(unit.restore(state));
            assert!(poll!(&mut restore).is_pending());
            assert!(device.lock_if_open().is_none());
            send.send(());
            restore.await.unwrap();
        }
        assert!(device.lock().restored);
    }

    #[async_test]
    async fn omitted_saved_state_does_not_save_device() {
        let (_send, recv) = mesh::oneshot();
        let device = Arc::new(CloseableMutex::new(Device {
            gate: Some(recv),
            restored: false,
        }));
        let mut unit = ArcMutexChipsetDeviceUnit::new(device.clone(), true);
        assert!(matches!(
            poll!(std::pin::pin!(unit.save())),
            Poll::Ready(Ok(None))
        ));
        assert!(device.lock().gate.is_some());
    }

    #[async_test]
    async fn device_errors_fail_the_state_transition() {
        let device = Arc::new(CloseableMutex::new(Device {
            gate: None,
            restored: false,
        }));
        let mut unit = ArcMutexChipsetDeviceUnit::new(device.clone(), false);
        let (send, recv) = mesh::oneshot();
        drop(send);
        device.lock().gate = Some(recv);
        assert!(unit.save().await.is_err());
        assert!(device.lock_if_open().is_some());

        let (send, recv) = mesh::oneshot();
        drop(send);
        device.lock().gate = Some(recv);
        assert!(
            unit.restore(SavedStateBlob::new(NoSavedState))
                .await
                .is_err()
        );
        assert!(!device.lock().restored);
    }
}
