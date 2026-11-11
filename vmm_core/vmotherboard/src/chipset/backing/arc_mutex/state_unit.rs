// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::VmmChipsetDevice;
use async_trait::async_trait;
use chipset_device::mmio::ControlMmioIntercept;
use chipset_device::pio::ControlPortIoIntercept;
use closeable_mutex::CloseableMutex;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
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
use vmcore::device_state::InputGateWait;
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

/// A removal handle for a chipset device.
pub struct DynamicDeviceUnit {
    unit: SpawnedUnit<ArcMutexChipsetDeviceUnit>,
    driver: VmTaskDriver,
    teardown: Box<dyn FnOnce(bool) -> BoxFuture<'static, ()> + Send + Sync>,
}

impl DynamicDeviceUnit {
    pub(crate) fn new<T: 'static + VmmChipsetDevice>(
        unit: SpawnedUnit<ArcMutexChipsetDeviceUnit>,
        driver: VmTaskDriver,
        device: Arc<CloseableMutex<T>>,
    ) -> Self {
        Self {
            unit,
            driver,
            teardown: Box::new(move |running| {
                Box::pin(async move {
                    let mut device = device.close_permanently().take();
                    if running {
                        device.stop().await;
                    }
                    device.teardown().await;
                })
            }),
        }
    }

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
                let unit = self.unit.remove().await;
                (self.teardown)(unit.running).await;
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
    async fn confirm_start(&mut self) -> anyhow::Result<()>;
    async fn stop(&mut self);
    fn quiesce_input(&mut self) -> InputGateWait;
    fn resume_input(&mut self) -> InputGateWait;
    async fn reset(&mut self);
    async fn advance_time(&mut self, duration: std::time::Duration) -> anyhow::Result<()>;
    fn poll_device(&mut self, cx: &mut Context<'_>);
    async fn save(&mut self) -> Result<SavedStateBlob, SaveError>;
    async fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError>;
}

#[async_trait]
impl<T: VmmChipsetDevice> DynDevice for T {
    fn start(&mut self) {
        self.start()
    }

    async fn confirm_start(&mut self) -> anyhow::Result<()> {
        vmcore::device_state::ChangeDeviceState::confirm_start(self).await
    }

    async fn stop(&mut self) {
        self.stop().await
    }
    fn quiesce_input(&mut self) -> InputGateWait {
        vmcore::device_state::ChangeDeviceState::quiesce_input(self)
    }
    fn resume_input(&mut self) -> InputGateWait {
        vmcore::device_state::ChangeDeviceState::resume_input(self)
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

    async fn confirm_start(&mut self) -> anyhow::Result<()> {
        self.device.clone().close().confirm_start().await
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

    // The input gate is the one state change that the vCPUs may run through:
    // the snapshot boundary gates host input before it stops them, and
    // resumes a gate that failed before any of them stops. So, unlike the
    // other state changes, it does not close the device: the chipset answers
    // no port or MMIO access to a closed device, drops its interrupts, and
    // panics on an EOI or a PCI configuration access to it. The gate's
    // synchronous part runs under the ordinary lock, as a vCPU's access does,
    // and its wait runs without the device.
    async fn quiesce_input(&mut self) -> anyhow::Result<()> {
        let wait = self.device.lock().quiesce_input();
        wait.await
    }

    async fn resume_input(&mut self) -> anyhow::Result<()> {
        let wait = self.device.lock().resume_input();
        wait.await
    }

    async fn reset(&mut self) -> anyhow::Result<()> {
        self.device.clone().close().reset().await;
        Ok(())
    }

    // The devices that keep guest-visible time of their own, such as the
    // RTC's calendar, hold it while they are stopped and set it as they
    // start, so only the duration applies to them.
    async fn advance_time(
        &mut self,
        duration: std::time::Duration,
        _resumed: pal_async::timer::Instant,
    ) -> anyhow::Result<()> {
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
    use super::DynamicDeviceUnit;
    use crate::DebugEventHandler;
    use crate::chipset::Chipset;
    use crate::chipset::io_ranges::IoRanges;
    use chipset_device::ChipsetDevice;
    use chipset_device::interrupt::HandleEoi;
    use chipset_device::interrupt::LineInterruptTarget;
    use chipset_device::io::IoResult;
    use chipset_device::mmio::MmioIntercept;
    use chipset_device::pio::PortIoIntercept;
    use chipset_device_resources::IRQ_LINE_SET;
    use chipset_device_resources::ResolvedChipsetDevice;
    use closeable_mutex::CloseableMutex;
    use futures::poll;
    use inspect::InspectMut;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use state_unit::StateUnit;
    use state_unit::StateUnits;
    use std::panic::AssertUnwindSafe;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::task::Poll;
    use test_with_tracing::test;
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::device_state::InputGateWait;
    use vmcore::line_interrupt::LineInterrupt;
    use vmcore::save_restore::AsyncSaveRestore;
    use vmcore::save_restore::NoSavedState;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;
    use vmcore::save_restore::SavedStateBlob;
    use vmcore::vm_task::SingleDriverBackend;
    use vmcore::vm_task::VmTaskDriverSource;

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

    /// A device whose start fails once `gate` opens.
    #[derive(InspectMut)]
    #[inspect(skip)]
    struct StartFailingDevice {
        gate: Option<mesh::OneshotReceiver<()>>,
    }

    impl ChipsetDevice for StartFailingDevice {}

    impl ChangeDeviceState for StartFailingDevice {
        fn start(&mut self) {}
        async fn confirm_start(&mut self) -> anyhow::Result<()> {
            if let Some(gate) = self.gate.take() {
                gate.await?;
            }
            anyhow::bail!("the device did not start")
        }
        async fn stop(&mut self) {}
        async fn reset(&mut self) {}
    }

    impl AsyncSaveRestore for StartFailingDevice {
        type SavedState = NoSavedState;

        async fn save(&mut self) -> Result<NoSavedState, SaveError> {
            Ok(NoSavedState)
        }

        async fn restore(&mut self, _: NoSavedState) -> Result<(), RestoreError> {
            Ok(())
        }
    }

    #[async_test]
    async fn start_confirmation_waits_with_exclusive_device_access() {
        let (send, recv) = mesh::oneshot();
        let device = Arc::new(CloseableMutex::new(StartFailingDevice { gate: Some(recv) }));
        let mut unit = ArcMutexChipsetDeviceUnit::new(device.clone(), false);
        unit.start().await;
        {
            let mut confirm = std::pin::pin!(unit.confirm_start());
            assert!(poll!(&mut confirm).is_pending());
            assert!(device.lock_if_open().is_none());
            send.send(());
            let error = confirm.await.unwrap_err();
            assert!(error.to_string().contains("did not start"), "{error:#}");
        }
        assert!(device.lock_if_open().is_some());
        unit.stop().await;
    }

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

    #[derive(InspectMut)]
    #[inspect(skip)]
    struct DefaultTeardownDevice(Arc<AtomicBool>);

    impl Drop for DefaultTeardownDevice {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    impl ChipsetDevice for DefaultTeardownDevice {}

    impl ChangeDeviceState for DefaultTeardownDevice {
        fn start(&mut self) {}
        async fn stop(&mut self) {}
        async fn reset(&mut self) {}
    }

    impl SaveRestore for DefaultTeardownDevice {
        type SavedState = NoSavedState;

        fn save(&mut self) -> Result<NoSavedState, SaveError> {
            Ok(NoSavedState)
        }

        fn restore(&mut self, _: NoSavedState) -> Result<(), RestoreError> {
            Ok(())
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

        async fn teardown(mut self) {
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
        unit.remove().await;

        assert!(!mmio_ranges.is_occupied(0x1000));
        assert!(!pio_ranges.is_occupied(0x100));
        assert!(device.lock_if_open().is_none());
    }

    #[pal_async::async_test]
    async fn static_teardown_keeps_issued_io_owned_when_waiter_is_abandoned(driver: DefaultDriver) {
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
        let (entered, mut entered_recv) = mesh::channel();
        let (release, release_recv) = mesh::channel();
        let (finished, mut finished_recv) = mesh::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let device = builder
            .arc_mutex_device("test")
            .add(|_| RemovingDevice {
                stopped: true,
                stop_count: Arc::new(AtomicUsize::new(0)),
                accesses: Arc::new(AtomicUsize::new(0)),
                entered,
                release: release_recv,
                finished,
                io: Some(DropMarker(dropped.clone())),
            })
            .unwrap();
        let (_chipset, mut devices) = builder.build().unwrap();
        let teardown = devices.teardown();
        entered_recv.recv().await.unwrap();
        assert!(device.lock_if_open().is_none());
        assert!(!dropped.load(Ordering::SeqCst));
        drop(teardown);
        drop(devices);
        release.send(());
        finished_recv.recv().await.unwrap();
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[pal_async::async_test]
    async fn default_teardown_destroys_payload_before_removal_ack(driver: DefaultDriver) {
        let dropped = Arc::new(AtomicBool::new(false));
        let teardown = DefaultTeardownDevice(dropped.clone()).teardown();
        assert!(!dropped.load(Ordering::SeqCst));
        teardown.await;
        assert!(dropped.load(Ordering::SeqCst));

        dropped.store(false, Ordering::SeqCst);
        let device = Arc::new(CloseableMutex::new(DefaultTeardownDevice(dropped.clone())));
        let source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let driver = source.simple();
        let units = StateUnits::new();
        let unit = ArcMutexChipsetDeviceUnit::new(device.clone(), false);
        let unit = units
            .add("default-teardown")
            .spawn(driver.clone(), |recv| unit.run(recv))
            .unwrap();
        DynamicDeviceUnit::new(unit, driver, device.clone())
            .remove()
            .await;
        assert!(dropped.load(Ordering::SeqCst));
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
                let device = Arc::new(CloseableMutex::new(
                    ResolvedChipsetDevice::from(RemovingDevice {
                        stopped: true,
                        stop_count: stop_count.clone(),
                        accesses: accesses.clone(),
                        entered,
                        release: release_recv,
                        finished,
                        io: Some(DropMarker(io_dropped.clone())),
                    })
                    .0,
                ));
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
                let mut removal =
                    Some(DynamicDeviceUnit::new(unit, task_driver, device.clone()).remove());
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

    /// The VM time unit's stand-in, which the chipset's devices depend on.
    #[derive(InspectMut)]
    #[inspect(skip)]
    struct NoopUnit;

    impl StateUnit for NoopUnit {
        async fn start(&mut self) {}

        async fn stop(&mut self) {}

        async fn reset(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
            Ok(None)
        }

        async fn restore(&mut self, _: SavedStateBlob) -> Result<(), RestoreError> {
            Ok(())
        }
    }

    /// A started chipset with the state units that the VM worker gives it: VM
    /// time, the chipset, and the devices that the test adds; and a line of
    /// its IRQ line set.
    struct StartedChipset {
        units: StateUnits,
        chipset: Arc<Chipset>,
        irq: Arc<LineInterrupt>,
        _devices: crate::ChipsetDevices,
        _time_unit: state_unit::SpawnedUnit<NoopUnit>,
        _keeper: vmcore::vmtime::VmTimeKeeper,
    }

    impl StartedChipset {
        async fn new(driver: &DefaultDriver, add: impl FnOnce(&crate::ChipsetBuilder<'_>)) -> Self {
            let source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
            let mut units = StateUnits::new();
            let time_unit = units
                .add("vmtime")
                .spawn(driver.clone(), |recv| state_unit::run_unit(NoopUnit, recv))
                .unwrap();
            let keeper =
                vmcore::vmtime::VmTimeKeeper::new(driver, vmcore::vmtime::VmTime::from_100ns(0));
            let time = keeper.builder().build(driver).await.unwrap();
            let builder = crate::ChipsetBuilder::new(
                &source,
                &units,
                Arc::new(TestDebugEventHandler),
                &time,
                time_unit.handle(),
                false,
                false,
                None,
            );
            add(&builder);
            let irq = {
                let mut inner = builder.inner.lock();
                let (line_set, _) = inner.line_sets.line_set(&source, &units, IRQ_LINE_SET);
                line_set.new_line(0, "test").unwrap()
            };
            let (chipset, devices) = builder.build().unwrap();
            units.start().await.unwrap();
            Self {
                units,
                chipset,
                irq: Arc::new(irq),
                _devices: devices,
                _time_unit: time_unit,
                _keeper: keeper,
            }
        }
    }

    const GATED_PORT: u16 = 0x6f0;

    /// One change of a [`GatedDevice`]'s input gate: the device reports the
    /// change on `entered` when it makes it, and the change's wait completes
    /// when `release` does, as a virtio transport's gate waits for its device
    /// task's reply.
    struct GateChange {
        entered: mesh::OneshotSender<()>,
        release: mesh::OneshotReceiver<()>,
    }

    /// The control of a [`GateChange`]: when the change began, and the
    /// release of its wait.
    struct GateChangeControl {
        entered: mesh::OneshotReceiver<()>,
        release: mesh::OneshotSender<()>,
    }

    fn gate_change() -> (GateChange, GateChangeControl) {
        let (entered, entered_recv) = mesh::oneshot();
        let (release_send, release) = mesh::oneshot();
        (
            GateChange { entered, release },
            GateChangeControl {
                entered: entered_recv,
                release: release_send,
            },
        )
    }

    /// A device that the VPs reach in each way that the chipset dispatches to
    /// it: port I/O, EOIs, and its interrupt line. Each change of its input
    /// gate waits for the test, and it records each access that it serves,
    /// with whether its gate was closed then.
    #[derive(InspectMut)]
    #[inspect(skip)]
    struct GatedDevice {
        gated: bool,
        changes: std::collections::VecDeque<GateChange>,
        accesses: Arc<parking_lot::Mutex<Vec<(&'static str, bool)>>>,
    }

    impl GatedDevice {
        fn change_gate(
            &mut self,
            gated: bool,
        ) -> impl 'static + Send + Future<Output = anyhow::Result<()>> {
            self.gated = gated;
            let GateChange { entered, release } =
                self.changes.pop_front().expect("unexpected gate change");
            entered.send(());
            async move {
                release.await?;
                Ok(())
            }
        }

        fn record(&mut self, access: &'static str) {
            self.accesses.lock().push((access, self.gated));
        }
    }

    impl ChangeDeviceState for GatedDevice {
        fn start(&mut self) {}

        fn quiesce_input(&mut self) -> InputGateWait {
            InputGateWait::new(self.change_gate(true))
        }

        fn resume_input(&mut self) -> InputGateWait {
            InputGateWait::new(self.change_gate(false))
        }

        async fn stop(&mut self) {}

        async fn reset(&mut self) {}
    }

    impl ChipsetDevice for GatedDevice {
        fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
            Some(self)
        }

        fn supports_handle_eoi(&mut self) -> Option<&mut dyn HandleEoi> {
            Some(self)
        }

        fn supports_line_interrupt_target(&mut self) -> Option<&mut dyn LineInterruptTarget> {
            Some(self)
        }
    }

    impl PortIoIntercept for GatedDevice {
        fn get_static_regions(&mut self) -> &[(&str, std::ops::RangeInclusive<u16>)] {
            &[("gated", GATED_PORT..=GATED_PORT)]
        }

        fn io_read(&mut self, _: u16, data: &mut [u8]) -> IoResult {
            data.fill(0);
            IoResult::Ok
        }

        fn io_write(&mut self, _: u16, _: &[u8]) -> IoResult {
            self.record("port write");
            IoResult::Ok
        }
    }

    impl HandleEoi for GatedDevice {
        fn handle_eoi(&mut self, _: u32) {
            self.record("eoi");
        }
    }

    impl LineInterruptTarget for GatedDevice {
        fn set_irq(&mut self, _: u32, high: bool) {
            self.record(if high { "irq high" } else { "irq low" });
        }

        fn valid_lines(&self) -> &[std::ops::RangeInclusive<u32>] {
            &[0..=0]
        }
    }

    impl SaveRestore for GatedDevice {
        type SavedState = NoSavedState;

        fn save(&mut self) -> Result<NoSavedState, SaveError> {
            Ok(NoSavedState)
        }

        fn restore(&mut self, _: NoSavedState) -> Result<(), RestoreError> {
            Ok(())
        }
    }

    fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
        panic
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_default()
    }

    /// Plays a VP on a thread of its own that, once a gate change began,
    /// writes the gated port, signals an EOI, and sets the interrupt line to
    /// `irq_level`, then releases the change's wait. Returns the panic
    /// message of each access that panicked.
    fn vp_during_gate_change(
        started: &StartedChipset,
        control: GateChangeControl,
        irq_level: bool,
    ) -> mesh::OneshotReceiver<Vec<String>> {
        let chipset = started.chipset.clone();
        let irq = started.irq.clone();
        let (send, recv) = mesh::oneshot();
        std::thread::spawn(move || {
            futures::executor::block_on(control.entered).unwrap();
            let mut panics = Vec::new();
            let accesses: [&dyn Fn(); 3] = [
                &|| futures::executor::block_on(chipset.io_write(1, GATED_PORT, &[1])),
                &|| chipset.handle_eoi(0),
                &|| irq.set_level(irq_level),
            ];
            for access in accesses {
                if let Err(panic) = std::panic::catch_unwind(AssertUnwindSafe(access)) {
                    panics.push(panic_message(panic));
                }
            }
            control.release.send(());
            send.send(panics);
        });
        recv
    }

    /// The snapshot boundary gates host input while the VPs run, and resumes
    /// a gate that failed before any VP stops: the port writes, EOIs, and
    /// interrupts of a VP during a device's gate change, as it waits, reach
    /// the device after the change.
    #[pal_async::async_test]
    async fn vp_accesses_during_an_input_gate_change_reach_the_device(driver: DefaultDriver) {
        let (quiesce, quiesce_control) = gate_change();
        let (resume, resume_control) = gate_change();
        let accesses = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let started = StartedChipset::new(&driver, |builder| {
            builder
                .arc_mutex_device("gated")
                .add(|services| {
                    services.add_line_target(IRQ_LINE_SET, 0..=0, 0);
                    GatedDevice {
                        gated: false,
                        changes: [quiesce, resume].into(),
                        accesses: accesses.clone(),
                    }
                })
                .unwrap();
        })
        .await;

        let vp = vp_during_gate_change(&started, quiesce_control, true);
        started.units.quiesce_input_for_save().await.unwrap();
        assert_eq!(
            (vp.await.unwrap(), std::mem::take(&mut *accesses.lock())),
            (
                Vec::new(),
                vec![("port write", true), ("eoi", true), ("irq high", true)]
            )
        );

        let vp = vp_during_gate_change(&started, resume_control, false);
        started.units.resume_input_after_save().await.unwrap();
        assert_eq!(
            (vp.await.unwrap(), std::mem::take(&mut *accesses.lock())),
            (
                Vec::new(),
                vec![("port write", false), ("eoi", false), ("irq low", false)]
            )
        );
    }

    const COUNTING_PORT: u16 = 0x6f1;

    /// A device whose input gate is the default, which completes at once, as
    /// the gates of most devices do, such as the IOAPIC's and the RTC's. It
    /// counts the port writes and EOIs that it serves.
    #[derive(InspectMut)]
    #[inspect(skip)]
    struct CountingDevice {
        served: Arc<AtomicUsize>,
    }

    impl ChangeDeviceState for CountingDevice {
        fn start(&mut self) {}

        async fn stop(&mut self) {}

        async fn reset(&mut self) {}
    }

    impl ChipsetDevice for CountingDevice {
        fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
            Some(self)
        }

        fn supports_handle_eoi(&mut self) -> Option<&mut dyn HandleEoi> {
            Some(self)
        }
    }

    impl PortIoIntercept for CountingDevice {
        fn get_static_regions(&mut self) -> &[(&str, std::ops::RangeInclusive<u16>)] {
            &[("counting", COUNTING_PORT..=COUNTING_PORT)]
        }

        fn io_read(&mut self, _: u16, data: &mut [u8]) -> IoResult {
            data.fill(0);
            IoResult::Ok
        }

        fn io_write(&mut self, _: u16, _: &[u8]) -> IoResult {
            self.served.fetch_add(1, Ordering::SeqCst);
            IoResult::Ok
        }
    }

    impl HandleEoi for CountingDevice {
        fn handle_eoi(&mut self, _: u32) {
            self.served.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl SaveRestore for CountingDevice {
        type SavedState = NoSavedState;

        fn save(&mut self) -> Result<NoSavedState, SaveError> {
            Ok(NoSavedState)
        }

        fn restore(&mut self, _: NoSavedState) -> Result<(), RestoreError> {
            Ok(())
        }
    }

    /// A gate that completes at once, as the default does, changes under the
    /// lock that the VPs take too: a VP that writes a port and signals EOIs in
    /// a loop, while the snapshot boundary takes and resumes the gate again
    /// and again, as the guest's snapshot requests make it, finds every access
    /// served.
    #[pal_async::async_test]
    async fn immediate_input_gates_serve_concurrent_vp_accesses(driver: DefaultDriver) {
        const ROUNDS: usize = 1000;
        let served = Arc::new(AtomicUsize::new(0));
        let started = StartedChipset::new(&driver, |builder| {
            builder
                .arc_mutex_device("counting")
                .add(|_| CountingDevice {
                    served: served.clone(),
                })
                .unwrap();
        })
        .await;

        let done = Arc::new(AtomicBool::new(false));
        let (running, running_recv) = mesh::oneshot();
        let (send, vp) = mesh::oneshot();
        std::thread::spawn({
            let chipset = started.chipset.clone();
            let done = done.clone();
            move || {
                let mut running = Some(running);
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    let mut issued = 0;
                    while !done.load(Ordering::SeqCst) {
                        futures::executor::block_on(chipset.io_write(1, COUNTING_PORT, &[1]));
                        chipset.handle_eoi(0);
                        issued += 2;
                        if let Some(running) = running.take() {
                            running.send(());
                        }
                    }
                    issued
                }));
                send.send(result.map_err(panic_message));
            }
        });
        running_recv.await.unwrap();
        for _ in 0..ROUNDS {
            started.units.quiesce_input_for_save().await.unwrap();
            started.units.resume_input_after_save().await.unwrap();
        }
        done.store(true, Ordering::SeqCst);
        let issued = vp.await.unwrap().unwrap();
        assert_eq!(served.load(Ordering::SeqCst), issued);
    }
}
