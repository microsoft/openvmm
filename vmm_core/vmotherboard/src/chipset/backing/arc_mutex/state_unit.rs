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
use futures::future::Fuse;
use futures::task::ArcWake;
use futures::task::WakerRef;
use futures::task::waker_ref;
use inspect::InspectMut;
use mesh::error::RemoteError;
use mesh::rpc::FailableRpc;
use pal_async::task::Spawn;
use state_unit::SpawnedUnit;
use state_unit::StateRequest;
use state_unit::StateUnit;
use std::sync::Arc;
use std::task::Context;
use vmcore::device_state::InputHookCompletion;
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
    async fn stop(&mut self);
    fn quiesce_input(&mut self) -> InputHookCompletion;
    fn resume_input(&mut self) -> InputHookCompletion;
    async fn reset(&mut self);
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

    fn quiesce_input(&mut self) -> InputHookCompletion {
        self.quiesce_input()
    }

    fn resume_input(&mut self) -> InputHookCompletion {
        self.resume_input()
    }

    async fn reset(&mut self) {
        self.reset().await
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
        // An input hook request, whose answer waits for the hook's completion.
        // The unit keeps polling the device meanwhile, since vCPUs may still
        // access it and the completion may wait for the device to progress.
        let mut input_hook: Option<(FailableRpc<(), ()>, InputHookCompletion)> = None;
        // A state request that the unit has received but not applied. A
        // transaction sends no request before the input hook is answered, but
        // a dropped one can. Since only an inspection may overlap the hook, any
        // other request waits for it, and the unit receives no other request
        // until it has applied this one.
        let mut request = None;
        let mut closed = false;
        loop {
            if let Some(req) = request
                .take_if(|req| input_hook.is_none() || matches!(req, StateRequest::Inspect(_)))
            {
                match req {
                    StateRequest::QuiesceInput(rpc) => {
                        input_hook = Some((rpc, StateUnit::quiesce_input(&mut self)));
                    }
                    StateRequest::ResumeInput(rpc) => {
                        input_hook = Some((rpc, StateUnit::resume_input(&mut self)));
                    }
                    req => req.apply(&mut self).await,
                }
                continue;
            }
            if closed && input_hook.is_none() {
                break;
            }

            enum Event<'a> {
                StateChange(Option<StateRequest>),
                Poll(WakerRef<'a>),
                InputHookDone(anyhow::Result<()>),
            }

            // Wait for state requests until the channel closes, but not while a
            // request waits for the input hook.
            let mut state_change_fut = if closed || request.is_some() {
                Fuse::terminated()
            } else {
                state_change.next().fuse()
            };

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

            // Wait for the completion of the pending input hook.
            let mut input_hook_fut = match &mut input_hook {
                Some((_, completion)) => completion.fuse(),
                None => Fuse::terminated(),
            };

            let event = futures::select! {
                req = state_change_fut => {
                    Event::StateChange(req)
                }
                waker = poll_fut.fuse() => {
                    Event::Poll(waker)
                }
                result = input_hook_fut => {
                    Event::InputHookDone(result)
                }
            };

            match event {
                Event::StateChange(None) => closed = true,
                Event::StateChange(Some(req)) => request = Some(req),
                Event::Poll(waker) => {
                    let mut device = self.device.lock();
                    device.poll_device(&mut Context::from_waker(&waker));
                }
                Event::InputHookDone(result) => {
                    let (rpc, _) = input_hook.take().expect("input hook is pending");
                    rpc.complete(result.map_err(RemoteError::new));
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

    fn quiesce_input(&mut self) -> InputHookCompletion {
        // VPs are still running during input transitions, so keep the mutex
        // open: a concurrent guest access waits for the hook, as for any other
        // device call, instead of panicking on a closed mutex. The hook is
        // synchronous, and the unit awaits its completion after the lock is
        // released, so a guest access never waits for asynchronous work, which
        // might need the executor that the guest access would block.
        self.device.lock().quiesce_input()
    }

    fn resume_input(&mut self) -> InputHookCompletion {
        self.device.lock().resume_input()
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
    use super::DynamicDeviceUnit;
    use crate::DebugEventHandler;
    use crate::chipset::Chipset;
    use crate::chipset::io_ranges::IoRanges;
    use chipset_device::ChipsetDevice;
    use chipset_device::io::IoResult;
    use chipset_device::mmio::MmioIntercept;
    use chipset_device::pio::PortIoIntercept;
    use chipset_device::poll_device::PollDevice;
    use chipset_device_resources::ResolvedChipsetDevice;
    use closeable_mutex::CloseableMutex;
    use futures::future::Either;
    use futures::poll;
    use inspect::InspectMut;
    use mesh::rpc::FailableRpc;
    use mesh::rpc::PendingFailableRpc;
    use mesh::rpc::RpcSend;
    use pal_async::DefaultDriver;
    use pal_async::DefaultPool;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use pal_async::timer::PolledTimer;
    use state_unit::StateRequest;
    use state_unit::StateUnit;
    use state_unit::StateUnits;
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Waker;
    use std::time::Duration;
    use test_with_tracing::test;
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::device_state::InputHookCompletion;
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

    /// A device whose input hooks signal that they ran and complete only when
    /// the test releases them.
    struct InputHookDevice {
        started: Option<mesh::OneshotSender<()>>,
        release: Option<mesh::OneshotReceiver<()>>,
    }

    impl InputHookDevice {
        fn hook(&mut self) -> InputHookCompletion {
            self.started.take().expect("hook runs once").send(());
            let release = self.release.take().expect("hook runs once");
            InputHookCompletion::new(async move {
                release
                    .await
                    .map_err(|_| anyhow::anyhow!("hook release dropped"))
            })
        }
    }

    impl ChipsetDevice for InputHookDevice {}

    impl InspectMut for InputHookDevice {
        fn inspect_mut(&mut self, req: inspect::Request<'_>) {
            req.respond();
        }
    }

    impl ChangeDeviceState for InputHookDevice {
        fn start(&mut self) {}

        fn quiesce_input(&mut self) -> InputHookCompletion {
            self.hook()
        }

        fn resume_input(&mut self) -> InputHookCompletion {
            self.hook()
        }

        async fn stop(&mut self) {}

        async fn reset(&mut self) {}
    }

    impl AsyncSaveRestore for InputHookDevice {
        type SavedState = NoSavedState;

        async fn save(&mut self) -> Result<NoSavedState, SaveError> {
            Ok(NoSavedState)
        }

        async fn restore(&mut self, _: NoSavedState) -> Result<(), RestoreError> {
            Ok(())
        }
    }

    /// Sends `request` to the unit of an [`InputHookDevice`] whose hook does not
    /// complete until a guest access to the device, on the same single-threaded
    /// executor, releases it, as when a VP and the unit share a CPU's thread in
    /// OpenHCL's threadpool.
    ///
    /// The guest access locks the device as MMIO and PIO dispatch do. If the
    /// hook held the lock until its completion, the guest access would block
    /// the executor that must complete the hook, so this fails after a timeout
    /// instead of hanging.
    fn input_hook_with_guest_access_on_one_executor(
        request: fn(FailableRpc<(), ()>) -> StateRequest,
    ) {
        let (done_send, done_recv) = mpsc::channel();
        std::thread::spawn(move || {
            DefaultPool::run_with(async |driver| {
                let (started_send, started_recv) = mesh::oneshot();
                let (release_send, release_recv) = mesh::oneshot();
                let device = Arc::new(CloseableMutex::new(InputHookDevice {
                    started: Some(started_send),
                    release: Some(release_recv),
                }));
                let unit = ArcMutexChipsetDeviceUnit::new(device.clone(), false);
                let (send, recv) = mesh::channel();
                let unit_task = driver.spawn("unit", unit.run(recv));
                let guest_access = driver.spawn("guest-access", async move {
                    started_recv.await.unwrap();
                    drop(device.lock_if_open().expect("device is open"));
                    release_send.send(());
                });
                send.call_failable(request, ()).await.unwrap();
                guest_access.await;
                drop(send);
                unit_task.await;
            });
            done_send.send(()).unwrap();
        });
        done_recv
            .recv_timeout(Duration::from_secs(10))
            .expect("a guest access deadlocked with an input hook on its executor");
    }

    #[test]
    fn input_hooks_do_not_block_guest_access_on_their_executor() {
        input_hook_with_guest_access_on_one_executor(StateRequest::QuiesceInput);
        input_hook_with_guest_access_on_one_executor(StateRequest::ResumeInput);
    }

    /// A device whose input hooks complete only once the unit polls the device
    /// after the hook ran, as a device that finishes reading host input when it
    /// is polled would.
    #[derive(Default)]
    struct PolledHookDevice {
        waker: Option<Waker>,
        hook_started: Option<mesh::OneshotSender<()>>,
        polled: Option<mesh::OneshotSender<()>>,
    }

    impl PolledHookDevice {
        fn hook(&mut self) -> InputHookCompletion {
            if let Some(hook_started) = self.hook_started.take() {
                hook_started.send(());
            }
            let (send, recv) = mesh::oneshot();
            self.polled = Some(send);
            InputHookCompletion::new(async move {
                recv.await
                    .map_err(|_| anyhow::anyhow!("poll signal dropped"))
            })
        }
    }

    impl ChipsetDevice for PolledHookDevice {
        fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
            Some(self)
        }
    }

    impl PollDevice for PolledHookDevice {
        fn poll_device(&mut self, cx: &mut Context<'_>) {
            self.waker = Some(cx.waker().clone());
            if let Some(polled) = self.polled.take() {
                polled.send(());
            }
        }
    }

    impl InspectMut for PolledHookDevice {
        fn inspect_mut(&mut self, req: inspect::Request<'_>) {
            req.respond();
        }
    }

    impl ChangeDeviceState for PolledHookDevice {
        fn start(&mut self) {}

        fn quiesce_input(&mut self) -> InputHookCompletion {
            self.hook()
        }

        fn resume_input(&mut self) -> InputHookCompletion {
            self.hook()
        }

        async fn stop(&mut self) {}

        async fn reset(&mut self) {}
    }

    impl AsyncSaveRestore for PolledHookDevice {
        type SavedState = NoSavedState;

        async fn save(&mut self) -> Result<NoSavedState, SaveError> {
            Ok(NoSavedState)
        }

        async fn restore(&mut self, _: NoSavedState) -> Result<(), RestoreError> {
            Ok(())
        }
    }

    /// Starts the unit of a new [`PolledHookDevice`], with the device.
    async fn start_polled_hook_unit(
        driver: &DefaultDriver,
    ) -> (
        Arc<CloseableMutex<PolledHookDevice>>,
        mesh::Sender<StateRequest>,
        pal_async::task::Task<ArcMutexChipsetDeviceUnit>,
    ) {
        let device = Arc::new(CloseableMutex::new(PolledHookDevice::default()));
        let unit = ArcMutexChipsetDeviceUnit::new(device.clone(), false);
        let (send, recv) = mesh::channel();
        let unit_task = driver.spawn("unit", unit.run(recv));
        send.call(StateRequest::Start, ()).await.unwrap();
        (device, send, unit_task)
    }

    /// Sends `request` to the unit of `device`, without waiting for its answer,
    /// and returns once the device's hook has run.
    async fn start_polled_hook(
        device: &CloseableMutex<PolledHookDevice>,
        send: &mesh::Sender<StateRequest>,
        request: fn(FailableRpc<(), ()>) -> StateRequest,
    ) -> PendingFailableRpc<()> {
        let (started_send, started_recv) = mesh::oneshot();
        device.lock().hook_started = Some(started_send);
        let hook = send.call_failable(request, ());
        started_recv.await.unwrap();
        hook
    }

    /// Asks the unit of `device` to poll it.
    fn request_poll(device: &CloseableMutex<PolledHookDevice>) {
        device
            .lock()
            .waker
            .as_ref()
            .expect("device was polled")
            .wake_by_ref();
    }

    /// Waits for `fut`, failing after a timeout with `message`.
    async fn with_timeout<T>(
        driver: &DefaultDriver,
        fut: impl Future<Output = T>,
        message: &str,
    ) -> T {
        let mut timer = PolledTimer::new(driver);
        let timeout = timer.sleep(Duration::from_secs(10));
        match futures::future::select(pin!(fut), pin!(timeout)).await {
            Either::Left((output, _)) => output,
            Either::Right(_) => panic!("{message}"),
        }
    }

    #[async_test]
    async fn input_hooks_keep_polling_the_device(driver: DefaultDriver) {
        let (device, send, unit_task) = start_polled_hook_unit(&driver).await;

        let requests: [fn(FailableRpc<(), ()>) -> StateRequest; 2] =
            [StateRequest::QuiesceInput, StateRequest::ResumeInput];
        for request in requests {
            let hook = start_polled_hook(&device, &send, request).await;
            request_poll(&device);
            with_timeout(
                &driver,
                hook,
                "the unit did not poll the device during an input hook",
            )
            .await
            .unwrap();
        }

        send.call(StateRequest::Stop, ()).await.unwrap();
        drop(send);
        unit_task.await;
    }

    #[async_test]
    async fn queued_request_waits_for_a_polled_input_hook(driver: DefaultDriver) {
        let (device, send, unit_task) = start_polled_hook_unit(&driver).await;

        // Drop the answer to the hook, as a dropped input transaction would,
        // and stop the unit while the hook is pending.
        drop(start_polled_hook(&device, &send, StateRequest::QuiesceInput).await);
        let mut stop = pin!(send.call(StateRequest::Stop, ()));
        PolledTimer::new(&driver)
            .sleep(Duration::from_millis(100))
            .await;
        assert!(poll!(stop.as_mut()).is_pending());

        // The stop waits for the hook, which the unit completes when it polls
        // the device.
        request_poll(&device);
        with_timeout(
            &driver,
            stop,
            "the unit did not poll the device before a stop",
        )
        .await
        .unwrap();
        drop(send);
        unit_task.await;
    }

    #[async_test]
    async fn closed_channel_waits_for_a_polled_input_hook(driver: DefaultDriver) {
        let (device, send, unit_task) = start_polled_hook_unit(&driver).await;

        // Drop the answer to the hook and close the unit's channel, as a
        // removal that follows a dropped input transaction would.
        drop(start_polled_hook(&device, &send, StateRequest::QuiesceInput).await);
        drop(send);
        PolledTimer::new(&driver)
            .sleep(Duration::from_millis(100))
            .await;

        // The unit exits once the hook completes, when it polls the device.
        request_poll(&device);
        with_timeout(
            &driver,
            unit_task,
            "the unit did not poll the device before exiting",
        )
        .await;
    }

    #[async_test]
    async fn inspection_does_not_wait_for_a_polled_input_hook(driver: DefaultDriver) {
        let (device, send, unit_task) = start_polled_hook_unit(&driver).await;

        let hook = start_polled_hook(&device, &send, StateRequest::QuiesceInput).await;
        let mut inspection = inspect::inspect("", &inspect::send(&send, StateRequest::Inspect));
        with_timeout(
            &driver,
            inspection.resolve(),
            "the inspection waited for the input hook",
        )
        .await;

        request_poll(&device);
        hook.await.unwrap();
        drop(send);
        unit_task.await;
    }
}
