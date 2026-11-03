// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::VmmChipsetDevice;
use async_trait::async_trait;
use chipset_device::mmio::ControlMmioIntercept;
use chipset_device::pio::ControlPortIoIntercept;
use closeable_mutex::CloseableMutex;
use futures::FutureExt;
use futures::StreamExt;
use futures::task::ArcWake;
use futures::task::WakerRef;
use futures::task::waker_ref;
use inspect::InspectMut;
use pal_async::task::Spawn;
use state_unit::SpawnedUnit;
use state_unit::StateRequest;
use state_unit::StateUnit;
use std::sync::Arc;
use std::task::Context;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;
use vmcore::slim_event::SlimEvent;
use vmcore::vm_task::VmTaskDriver;

pub struct ArcMutexChipsetDeviceUnit {
    device: Arc<CloseableMutex<dyn DynDevice>>,
    poll_event: Arc<PollEvent>,
    running: bool,
    omit_saved_state: bool,
    _static_mmio: Vec<Box<dyn ControlMmioIntercept>>,
    _static_pio: Vec<Box<dyn ControlPortIoIntercept>>,
}

/// A unit handle for a dynamically managed device.
pub struct DynamicDeviceUnit {
    pub(crate) unit: SpawnedUnit<ArcMutexChipsetDeviceUnit>,
    pub(crate) driver: VmTaskDriver,
}

impl DynamicDeviceUnit {
    /// Removes the dynamically managed device and releases its backing resources.
    ///
    /// Waits for the state-unit task to exit, stops a running device, and then
    /// invokes teardown with guest access permanently revoked.
    /// Resources owned by device-specific background tasks must be released
    /// before this returns, even if other references to the device remain.
    ///
    /// Dropping the returned future does not cancel removal or issued I/O.
    pub fn remove(self) -> impl Future<Output = ()> {
        let (send, recv) = mesh::oneshot();
        self.driver
            .spawn("remove-chipset-device", async move {
                let mut unit = self.unit.remove().await;
                let mut device = unit.device.clone().close_permanently();
                if unit.running {
                    device.stop().await;
                    unit.running = false;
                }
                device.teardown().await;
                device.destroy();
                drop(unit);
                send.send(());
            })
            .detach();
        async move {
            recv.await.expect("device removal task failed");
        }
    }
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
    async fn teardown(&mut self);
    fn poll_device(&mut self, cx: &mut Context<'_>);
    async fn save(&mut self) -> Result<SavedStateBlob, SaveError>;
    async fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError>;
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

    async fn teardown(&mut self) {
        self.teardown().await
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
            _static_mmio: Vec::new(),
            _static_pio: Vec::new(),
        }
    }

    pub(super) fn with_static_regions(
        mut self,
        mmio: Vec<Box<dyn ControlMmioIntercept>>,
        pio: Vec<Box<dyn ControlPortIoIntercept>>,
    ) -> Self {
        self._static_mmio = mmio;
        self._static_pio = pio;
        self
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DebugEventHandler;
    use crate::chipset::Chipset;
    use crate::chipset::io_ranges::IoRanges;
    use chipset_device::ChipsetDevice;
    use chipset_device::io::IoResult;
    use chipset_device::mmio::MmioIntercept;
    use chipset_device::pio::PortIoIntercept;
    use pal_async::DefaultDriver;
    use state_unit::StateUnits;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::save_restore::NoSavedState;
    use vmcore::save_restore::SaveRestore;
    use vmcore::vm_task::SingleDriverBackend;
    use vmcore::vm_task::VmTaskDriverSource;

    struct DropMarker(Arc<AtomicBool>);

    impl Drop for DropMarker {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[derive(InspectMut)]
    #[inspect(skip)]
    struct RemovingDevice {
        stopped: bool,
        stop_count: Arc<AtomicUsize>,
        accesses: Arc<AtomicUsize>,
        entered: mesh::Sender<()>,
        release: mesh::Receiver<()>,
        finished: mesh::Sender<()>,
        io: Option<DropMarker>,
    }

    impl Drop for RemovingDevice {
        fn drop(&mut self) {
            self.finished.send(());
        }
    }

    struct TestDebugEventHandler;

    impl DebugEventHandler for TestDebugEventHandler {
        fn on_debug_break(&self, _: Option<u32>) {
            panic!("unexpected debug break");
        }
    }

    impl ChipsetDevice for RemovingDevice {
        fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
            Some(self)
        }

        fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
            Some(self)
        }
    }

    impl MmioIntercept for RemovingDevice {
        fn get_static_regions(&mut self) -> &[(&str, std::ops::RangeInclusive<u64>)] {
            &[("mmio", 0x1000..=0x1fff)]
        }

        fn mmio_read(&mut self, _: u64, data: &mut [u8]) -> IoResult {
            self.accesses.fetch_add(1, Ordering::SeqCst);
            data.fill(0x5a);
            IoResult::Ok
        }

        fn mmio_write(&mut self, _: u64, _: &[u8]) -> IoResult {
            self.accesses.fetch_add(1, Ordering::SeqCst);
            IoResult::Ok
        }
    }

    impl PortIoIntercept for RemovingDevice {
        fn get_static_regions(&mut self) -> &[(&str, std::ops::RangeInclusive<u16>)] {
            &[("pio", 0x100..=0x103)]
        }

        fn io_read(&mut self, _: u16, data: &mut [u8]) -> IoResult {
            self.mmio_read(0, data)
        }

        fn io_write(&mut self, _: u16, data: &[u8]) -> IoResult {
            self.mmio_write(0, data)
        }
    }

    impl SaveRestore for RemovingDevice {
        type SavedState = NoSavedState;

        fn save(&mut self) -> Result<NoSavedState, SaveError> {
            Ok(NoSavedState)
        }

        fn restore(&mut self, _: NoSavedState) -> Result<(), RestoreError> {
            Ok(())
        }
    }

    impl ChangeDeviceState for RemovingDevice {
        fn start(&mut self) {
            self.stopped = false;
        }

        async fn stop(&mut self) {
            assert!(!self.stopped, "device stopped twice");
            self.stopped = true;
            self.stop_count.fetch_add(1, Ordering::SeqCst);
        }

        async fn reset(&mut self) {}

        async fn teardown(&mut self) {
            assert!(self.stopped, "teardown before stop");
            let io = self.io.take();
            self.entered.send(());
            self.release.recv().await.unwrap();
            drop(io);
        }
    }

    #[pal_async::async_test]
    async fn static_regions_live_until_unit_removal(driver: DefaultDriver) {
        let source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
        let units = StateUnits::new();
        let (time_send, _time_recv) = mesh::channel();
        let time_unit = units.add("vmtime").build(time_send).unwrap();
        let keeper =
            vmcore::vmtime::VmTimeKeeper::new(&driver, vmcore::vmtime::VmTime::from_100ns(0));
        let time = keeper.builder().build(&driver).await.unwrap();
        let builder = crate::ChipsetBuilder::new(
            &source,
            &units,
            Arc::new(TestDebugEventHandler),
            &time,
            &time_unit,
            false,
            false,
            None,
        );
        let (entered, _entered_recv) = mesh::channel();
        let (release, release_recv) = mesh::channel();
        let (finished, _finished_recv) = mesh::channel();
        let device = builder
            .arc_mutex_device("test")
            .add(|_| RemovingDevice {
                stopped: true,
                stop_count: Arc::new(AtomicUsize::new(0)),
                accesses: Arc::new(AtomicUsize::new(0)),
                entered,
                release: release_recv,
                finished,
                io: None,
            })
            .unwrap();
        let (unit, mmio_ranges, pio_ranges) = {
            let mut inner = builder.inner.lock();
            (
                inner.arc_mutex_device_units.pop().unwrap(),
                inner.vm_chipset.mmio_ranges.clone(),
                inner.vm_chipset.pio_ranges.clone(),
            )
        };
        drop(builder);
        assert!(mmio_ranges.is_occupied(0x1000));
        assert!(pio_ranges.is_occupied(0x100));

        release.send(());
        DynamicDeviceUnit {
            unit,
            driver: source.simple(),
        }
        .remove()
        .await;

        assert!(!mmio_ranges.is_occupied(0x1000));
        assert!(!pio_ranges.is_occupied(0x100));
        assert!(device.lock_if_open().is_none());
    }

    #[pal_async::async_test]
    async fn removal_revokes_io_without_cancelling_cleanup(driver: DefaultDriver) {
        for state in ["never-started", "running", "paused"] {
            for abandon_waiter in [false, true] {
                let (entered, mut entered_recv) = mesh::channel();
                let (release, release_recv) = mesh::channel();
                let (finished, mut finished_recv) = mesh::channel();
                let io_dropped = Arc::new(AtomicBool::new(false));
                let accesses = Arc::new(AtomicUsize::new(0));
                let stop_count = Arc::new(AtomicUsize::new(0));
                let device = Arc::new(CloseableMutex::new(RemovingDevice {
                    stopped: true,
                    stop_count: stop_count.clone(),
                    accesses: accesses.clone(),
                    entered,
                    release: release_recv,
                    finished,
                    io: Some(DropMarker(io_dropped.clone())),
                }));
                let erased: Arc<CloseableMutex<dyn ChipsetDevice>> = device.clone();
                let mmio_ranges = IoRanges::<u64>::new(false, None);
                mmio_ranges
                    .register(
                        0x1000,
                        0x1fff,
                        "bar".into(),
                        Arc::downgrade(&erased),
                        "test".into(),
                    )
                    .unwrap();
                let pio_ranges = IoRanges::<u16>::new(false, None);
                pio_ranges
                    .register(
                        0x100,
                        0x103,
                        "pio".into(),
                        Arc::downgrade(&erased),
                        "test".into(),
                    )
                    .unwrap();
                let stale_lookup = mmio_ranges.lookup(0x1000, true);
                let chipset = Chipset {
                    mmio_ranges,
                    pio_ranges,
                    pic: None,
                    eoi_handler: None,
                    debug_event_handler: Arc::new(TestDebugEventHandler),
                };
                let mut data = [0; 4];
                chipset.mmio_read(0, 0x1000, &mut data).await;
                assert_eq!(data, [0x5a; 4]);
                let source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
                let task_driver = source.simple();
                let units = StateUnits::new();
                let mut unit = ArcMutexChipsetDeviceUnit::new(device.clone(), false);
                if state != "never-started" {
                    unit.start().await;
                    if state == "paused" {
                        unit.stop().await;
                    }
                }
                let unit = units
                    .add("test")
                    .spawn(task_driver.clone(), |recv| unit.run(recv))
                    .unwrap();
                let mut removal = Some(
                    DynamicDeviceUnit {
                        unit,
                        driver: task_driver,
                    }
                    .remove(),
                );
                entered_recv.recv().await.unwrap();
                if abandon_waiter {
                    drop(removal.take());
                }

                assert!(stale_lookup.dev.lock_if_open().is_none());
                chipset.mmio_read(0, 0x1000, &mut data).await;
                assert_eq!(data, [0xff; 4]);
                chipset.mmio_write(0, 0x1000, &[1; 4]).await;
                chipset.io_read(0, 0x100, &mut data).await;
                assert_eq!(data, [0xff; 4]);
                chipset.io_write(0, 0x100, &[1; 4]).await;
                assert_eq!(accesses.load(Ordering::SeqCst), 1);
                assert!(!io_dropped.load(Ordering::SeqCst));

                release.send(());
                finished_recv.recv().await.unwrap();
                if let Some(removal) = removal {
                    removal.await;
                }
                assert!(io_dropped.load(Ordering::SeqCst));
                assert_eq!(
                    stop_count.load(Ordering::SeqCst),
                    usize::from(state != "never-started")
                );
                assert!(device.lock_if_open().is_none());
                chipset.mmio_read(0, 0x1000, &mut data).await;
                assert_eq!(data, [0xff; 4]);
            }
        }
    }
}
