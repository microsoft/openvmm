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
    fn start(&mut self);
    async fn stop(&mut self);
    async fn reset(&mut self);
    fn poll_device(&mut self, cx: &mut Context<'_>);
    async fn prepare_save(&mut self) -> Result<(), SaveError>;
    fn save(&mut self) -> Result<SavedStateBlob, SaveError>;
    fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError>;
    async fn finish_restore(&mut self) -> Result<(), RestoreError>;
}

#[async_trait]
impl<T: VmmChipsetDevice> DynDevice for T {
    fn start(&mut self) {
        self.start()
    }

    async fn stop(&mut self) {
        self.stop().await
    }

    async fn reset(&mut self) {
        self.reset().await
    }

    fn poll_device(&mut self, cx: &mut Context<'_>) {
        if let Some(poll) = self.supports_poll_device() {
            poll.poll_device(cx);
        }
    }

    async fn prepare_save(&mut self) -> Result<(), SaveError> {
        vmcore::device_state::ChangeDeviceState::prepare_save(self).await
    }

    fn save(&mut self) -> Result<SavedStateBlob, SaveError> {
        self.save()
    }

    fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError> {
        self.restore(state)
    }

    async fn finish_restore(&mut self) -> Result<(), RestoreError> {
        vmcore::device_state::ChangeDeviceState::finish_restore(self).await
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
    async fn start(&mut self) {
        self.running = true;

        // Poll the device at least once.
        let mut device = self.device.lock();
        device.start();
        device.poll_device(&mut Context::from_waker(&waker_ref(&self.poll_event)));
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

    async fn reset(&mut self) -> anyhow::Result<()> {
        self.device.clone().close().reset().await;
        Ok(())
    }

    async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        if self.omit_saved_state {
            return Ok(None);
        }

        let mut device = self.device.clone().close();
        device.prepare_save().await?;
        let state = device.save()?;
        Ok(Some(state))
    }

    async fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError> {
        let mut device = self.device.clone().close();
        device.restore(state)?;
        device.finish_restore().await
    }
}

#[cfg(test)]
mod tests {
    use super::ArcMutexChipsetDeviceUnit;
    use chipset_device::ChipsetDevice;
    use closeable_mutex::CloseableMutex;
    use inspect::InspectMut;
    use pal_async::async_test;
    use parking_lot::Mutex;
    use state_unit::StateUnit;
    use std::sync::Arc;
    use test_with_tracing::test;
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::save_restore::NoSavedState;
    use vmcore::save_restore::ProtobufSaveRestore;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SavedStateBlob;

    /// A device that logs its save and restore steps and fails the step named
    /// by `fail`.
    struct TestDevice {
        log: Arc<Mutex<Vec<&'static str>>>,
        fail: Option<&'static str>,
    }

    impl TestDevice {
        fn step(&mut self, name: &'static str) -> anyhow::Result<()> {
            self.log.lock().push(name);
            if self.fail == Some(name) {
                anyhow::bail!("{name} failed");
            }
            Ok(())
        }
    }

    impl ChipsetDevice for TestDevice {}

    impl InspectMut for TestDevice {
        fn inspect_mut(&mut self, req: inspect::Request<'_>) {
            req.respond();
        }
    }

    impl ChangeDeviceState for TestDevice {
        fn start(&mut self) {}

        async fn stop(&mut self) {}

        async fn reset(&mut self) {}

        async fn prepare_save(&mut self) -> Result<(), SaveError> {
            self.step("prepare_save").map_err(SaveError::Other)
        }

        async fn finish_restore(&mut self) -> Result<(), RestoreError> {
            self.step("finish_restore").map_err(RestoreError::Other)
        }
    }

    impl ProtobufSaveRestore for TestDevice {
        fn save(&mut self) -> Result<SavedStateBlob, SaveError> {
            self.step("save").map_err(SaveError::Other)?;
            Ok(SavedStateBlob::new(NoSavedState))
        }

        fn restore(&mut self, _state: SavedStateBlob) -> Result<(), RestoreError> {
            self.step("restore").map_err(RestoreError::Other)
        }
    }

    fn new_unit(
        fail: Option<&'static str>,
        omit_saved_state: bool,
    ) -> (ArcMutexChipsetDeviceUnit, Arc<Mutex<Vec<&'static str>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let device = TestDevice {
            log: log.clone(),
            fail,
        };
        let unit =
            ArcMutexChipsetDeviceUnit::new(Arc::new(CloseableMutex::new(device)), omit_saved_state);
        (unit, log)
    }

    #[async_test]
    async fn test_save_restore_hook_order() {
        let (mut unit, log) = new_unit(None, false);
        let state = unit.save().await.unwrap().unwrap();
        unit.restore(state).await.unwrap();
        assert_eq!(
            *log.lock(),
            ["prepare_save", "save", "restore", "finish_restore"]
        );
    }

    #[async_test]
    async fn test_prepare_save_failure_fails_save() {
        let (mut unit, log) = new_unit(Some("prepare_save"), false);
        let err = unit.save().await.unwrap_err();
        assert_eq!(err.to_string(), "prepare_save failed");
        assert_eq!(*log.lock(), ["prepare_save"]);
    }

    #[async_test]
    async fn test_restore_failure_skips_finish_restore() {
        let (mut unit, log) = new_unit(Some("restore"), false);
        let err = unit
            .restore(SavedStateBlob::new(NoSavedState))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "restore failed");
        assert_eq!(*log.lock(), ["restore"]);
    }

    #[async_test]
    async fn test_finish_restore_failure_fails_restore() {
        let (mut unit, log) = new_unit(Some("finish_restore"), false);
        let err = unit
            .restore(SavedStateBlob::new(NoSavedState))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "finish_restore failed");
        assert_eq!(*log.lock(), ["restore", "finish_restore"]);
    }

    #[async_test]
    async fn test_omitted_saved_state_skips_prepare_save() {
        let (mut unit, log) = new_unit(None, true);
        assert!(unit.save().await.unwrap().is_none());
        assert!(log.lock().is_empty());
    }
}
