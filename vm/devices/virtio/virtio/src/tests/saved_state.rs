// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for device-private saved state and restore: private-state round trips,
//! restore validation, the restore of private state into the stopped device,
//! and the start, reset, and removal that follow it. Also tests that ordinary
//! stops and starts, which restore nothing, keep queue progress.

use super::*;
use std::sync::atomic::AtomicBool;
use test_with_tracing::test;
mod private_state {
    use mesh::payload::Protobuf;
    use vmcore::save_restore::SavedStateRoot;

    #[derive(Protobuf, SavedStateRoot)]
    #[mesh(package = "virtio.test_private_state")]
    pub struct SavedState {
        #[mesh(1)]
        pub value: u64,
    }
}

#[derive(InspectMut)]
#[inspect(skip)]
struct PrivateStateTestDevice {
    probe: PrivateStateProbe,
    max_queues: u16,
    running: Vec<bool>,
}

#[derive(Clone)]
struct PrivateStateProbe {
    value: Arc<AtomicUsize>,
    restore_count: Arc<AtomicUsize>,
    start_count: Arc<AtomicUsize>,
    reset_count: Arc<AtomicUsize>,
    running_count: Arc<AtomicUsize>,
    queue_event: Arc<Mutex<Option<Event>>>,
    initial_avail: Arc<AtomicUsize>,
    initial_used: Arc<AtomicUsize>,
    value_at_start: Arc<AtomicUsize>,
    fail_restore: Arc<AtomicBool>,
    /// The index of the queue whose start fails, or `usize::MAX` for none.
    fail_start_at: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
}

impl PrivateStateTestDevice {
    fn new(value: usize, max_queues: u16, fail_restore: bool) -> (Self, PrivateStateProbe) {
        let probe = PrivateStateProbe {
            value: Arc::new(AtomicUsize::new(value)),
            restore_count: Arc::new(AtomicUsize::new(0)),
            start_count: Arc::new(AtomicUsize::new(0)),
            reset_count: Arc::new(AtomicUsize::new(0)),
            running_count: Arc::new(AtomicUsize::new(0)),
            queue_event: Arc::new(Mutex::new(None)),
            initial_avail: Arc::new(AtomicUsize::new(usize::MAX)),
            initial_used: Arc::new(AtomicUsize::new(usize::MAX)),
            value_at_start: Arc::new(AtomicUsize::new(usize::MAX)),
            fail_restore: Arc::new(AtomicBool::new(fail_restore)),
            fail_start_at: Arc::new(AtomicUsize::new(usize::MAX)),
            dropped: Arc::new(AtomicBool::new(false)),
        };
        (
            Self {
                probe: probe.clone(),
                max_queues,
                running: vec![false; max_queues.into()],
            },
            probe,
        )
    }
}

impl Drop for PrivateStateTestDevice {
    fn drop(&mut self) {
        self.probe.dropped.store(true, Ordering::Relaxed);
    }
}

impl PrivateStateProbe {
    fn take_queue_kick(&self) -> bool {
        self.queue_event
            .lock()
            .as_ref()
            .is_some_and(Event::try_wait)
    }
}

impl VirtioDevice for PrivateStateTestDevice {
    fn traits(&self) -> DeviceTraits {
        DeviceTraits {
            device_id: VirtioDeviceType::CONSOLE,
            device_features: VirtioDeviceFeatures::new(),
            max_queues: self.max_queues,
            device_register_length: 4,
            ..Default::default()
        }
    }

    async fn read_registers_u32(&mut self, _offset: u16) -> u32 {
        self.probe.value.load(Ordering::Relaxed) as u32
    }

    async fn write_registers_u32(&mut self, _offset: u16, val: u32) {
        self.probe.value.store(val as usize, Ordering::Relaxed);
    }

    async fn start_queue(
        &mut self,
        idx: u16,
        resources: QueueResources,
        _features: &VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        if usize::from(idx) == self.probe.fail_start_at.load(Ordering::Relaxed) {
            anyhow::bail!("intentional queue start failure");
        }
        self.running[usize::from(idx)] = true;
        self.probe.start_count.fetch_add(1, Ordering::Relaxed);
        self.probe.running_count.fetch_add(1, Ordering::Relaxed);
        *self.probe.queue_event.lock() = Some(resources.event.clone());
        self.probe
            .value_at_start
            .store(self.probe.value.load(Ordering::Relaxed), Ordering::Relaxed);
        if let Some(initial_state) = initial_state {
            self.probe
                .initial_avail
                .store(initial_state.avail_index as usize, Ordering::Relaxed);
            self.probe
                .initial_used
                .store(initial_state.used_index as usize, Ordering::Relaxed);
        }
        Ok(())
    }

    async fn stop_queue(&mut self, idx: u16) -> Option<QueueState> {
        std::mem::take(&mut self.running[usize::from(idx)]).then(|| {
            self.probe.running_count.fetch_sub(1, Ordering::Relaxed);
            QueueState {
                avail_index: 3,
                used_index: 1,
            }
        })
    }

    async fn reset(&mut self) {
        self.probe.reset_count.fetch_add(1, Ordering::Relaxed);
        self.probe.value.store(0, Ordering::Relaxed);
    }

    fn supports_save_restore(&self) -> bool {
        true
    }

    fn save_device(
        &mut self,
    ) -> Result<Option<vmcore::save_restore::SavedStateBlob>, vmcore::save_restore::SaveError> {
        Ok(Some(vmcore::save_restore::SavedStateBlob::new(
            private_state::SavedState {
                value: self.probe.value.load(Ordering::Relaxed) as u64,
            },
        )))
    }

    fn restore_device(
        &mut self,
        state: Option<vmcore::save_restore::SavedStateBlob>,
    ) -> Result<(), vmcore::save_restore::RestoreError> {
        self.probe.restore_count.fetch_add(1, Ordering::Relaxed);
        if self.probe.fail_restore.load(Ordering::Relaxed) {
            return Err(vmcore::save_restore::RestoreError::Other(anyhow::anyhow!(
                "intentional private-state restore failure"
            )));
        }
        let state = state.ok_or_else(|| {
            vmcore::save_restore::RestoreError::InvalidSavedState(anyhow::anyhow!(
                "missing private test state"
            ))
        })?;
        let state: private_state::SavedState = state.parse()?;
        let value = usize::try_from(state.value).map_err(|_| {
            vmcore::save_restore::RestoreError::InvalidSavedState(anyhow::anyhow!(
                "private test state is out of range"
            ))
        })?;
        self.probe.value.store(value, Ordering::Relaxed);
        Ok(())
    }

    fn device_state_validator(&self) -> crate::device::saved_state::DeviceStateValidator {
        Box::new(|state, _features, _queues, _guest_memory| {
            let state = state.ok_or_else(|| {
                vmcore::save_restore::RestoreError::InvalidSavedState(anyhow::anyhow!(
                    "missing private test state"
                ))
            })?;
            let _: private_state::SavedState = state.parse()?;
            Ok(())
        })
    }
}

impl VirtioPciTestDevice {
    /// Like `new_with_register_length`, for a caller-provided device.
    fn new_with_device(
        driver: &DefaultDriver,
        test_mem: &Arc<VirtioTestMemoryAccess>,
        device: Box<dyn DynVirtioDevice>,
    ) -> Self {
        let doorbell_registration: Arc<dyn DoorbellRegistration> = test_mem.clone();
        let mem = GuestMemory::new("test", test_mem.clone());
        let msi_conn = MsiConnection::new();

        let dev = VirtioPciDevice::new(
            device,
            driver,
            mem.clone(),
            PciInterruptModel::Msix(&msi_conn.target()),
            Some(doorbell_registration),
            &mut ExternallyManagedMmioIntercepts,
            None,
        )
        .unwrap();

        let test_intc = Arc::new(TestPciInterruptController::new());
        msi_conn.connect(test_intc.signal_msi());

        Self {
            pci_device: dev,
            test_intc,
        }
    }
}

/// Checks that restore validation accepts the saved queue and rejects
/// corrupted copies of it, leaving the saved state unchanged.
pub(super) fn check_restored_queue_validation(
    saved: &mut <VirtioMmioDevice as vmcore::save_restore::AsyncSaveRestore>::SavedState,
    features: VirtioDeviceFeatures,
    mem: &GuestMemory,
) {
    use crate::transport::core::restore::validate_restored_queue;

    let queue = &mut saved.queues[0].common;
    validate_restored_queue(0, queue, features, mem, &mut Vec::new())
        .expect("valid queue was rejected");

    let original_size = queue.size;
    queue.size = 3;
    assert!(validate_restored_queue(0, queue, features, mem, &mut Vec::new(),).is_err());
    queue.size = original_size;

    let original_avail = queue.avail_addr;
    queue.avail_addr = queue.desc_addr;
    assert!(validate_restored_queue(0, queue, features, mem, &mut Vec::new(),).is_err());
    queue.avail_addr = original_avail;

    let original_desc = queue.desc_addr;
    queue.desc_addr = u64::MAX - 15;
    assert!(validate_restored_queue(0, queue, features, mem, &mut Vec::new(),).is_err());
    queue.desc_addr = 1 << 60;
    assert!(validate_restored_queue(0, queue, features, mem, &mut Vec::new(),).is_err());
    queue.desc_addr = original_desc;

    let original_progress = queue.queue_state;
    queue.queue_state = Some(QueueState {
        avail_index: original_size + 1,
        used_index: 0,
    });
    assert!(validate_restored_queue(0, queue, features, mem, &mut Vec::new(),).is_err());
    queue.queue_state = original_progress;
}
/// Creates an MMIO transport for a private-state test device.
fn new_private_mmio(
    driver: &DefaultDriver,
    mem: GuestMemory,
    device: PrivateStateTestDevice,
) -> VirtioMmioDevice {
    VirtioMmioDevice::new(
        Box::new(device),
        driver,
        mem,
        LineInterrupt::detached(),
        None,
        0,
        0x1000,
    )
    .unwrap()
}

/// Parses the value of saved private test state.
fn saved_value(device_state: Option<vmcore::save_restore::SavedStateBlob>) -> u64 {
    device_state
        .expect("missing private test state")
        .parse::<private_state::SavedState>()
        .unwrap()
        .value
}

/// Formats an error with its sources.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut chain = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        chain = format!("{chain}: {error}");
        source = error.source();
    }
    chain
}

#[async_test]
async fn mmio_restore_rejects_missing_active_queue_progress(driver: DefaultDriver) {
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::save_restore::AsyncSaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let mem = GuestMemory::new("test", test_mem.clone());
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
    let guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, true);
    let traits = DeviceTraits {
        device_id: VirtioDeviceType::CONSOLE,
        device_features: VirtioDeviceFeatures::new()
            .with_bank(0, VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX),
        max_queues: 1,
        device_register_length: 0,
        ..Default::default()
    };
    let mut source = VirtioMmioDevice::new(
        Box::new(TestDevice::new(&driver_source, traits.clone(), None)),
        &driver,
        mem.clone(),
        LineInterrupt::detached(),
        None,
        0,
        0x1000,
    )
    .unwrap();
    guest
        .setup_chipset_device(&mut source, guest.queue_features())
        .await;
    source.stop().await;
    let mut saved = source.save().await.unwrap();
    assert!(saved.queues[0].common.enable);
    assert!(saved.queues[0].common.queue_state.take().is_some());

    let mut destination = VirtioMmioDevice::new(
        Box::new(TestDevice::new(&driver_source, traits, None)),
        &driver,
        mem,
        LineInterrupt::detached(),
        None,
        0,
        0x1000,
    )
    .unwrap();
    assert!(destination.restore(saved).await.is_err());
}

#[async_test]
async fn pci_restore_rejects_missing_active_queue_progress(driver: DefaultDriver) {
    use vmcore::save_restore::AsyncSaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let mut saved = save_active_private_pci(&driver, &test_mem).await;
    assert!(saved.queues[0].common.enable);
    assert!(saved.queues[0].common.queue_state.take().is_some());
    let (device, probe) = PrivateStateTestDevice::new(0, 1, false);
    let mut destination =
        VirtioPciTestDevice::new_with_device(&driver, &test_mem, Box::new(device));

    let error = destination.pci_device.restore(saved).await.unwrap_err();
    assert!(
        error_chain(&error).contains("missing progress state"),
        "{error:?}"
    );
    // Validation rejects the state before the device sees it.
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 0);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);
}

#[async_test]
async fn mmio_private_state_round_trip(driver: DefaultDriver) {
    use crate::spec::mmio::VirtioMmioRegister;
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::save_restore::AsyncSaveRestore;

    let (source_device, _source_probe) = PrivateStateTestDevice::new(42, 0, false);
    let mut source = new_private_mmio(&driver, GuestMemory::empty(), source_device);
    source.stop().await;
    let saved = source
        .save()
        .await
        .expect("private state save should succeed");
    assert!(saved.device_state.is_some());

    let (destination_device, probe) = PrivateStateTestDevice::new(0, 0, false);
    let mut destination = new_private_mmio(&driver, GuestMemory::empty(), destination_device);
    destination
        .restore(saved)
        .await
        .expect("private state restore should succeed");
    // The restore hands the private state to the stopped device.
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.value.load(Ordering::Relaxed), 42);

    // A save before the first start captures the restored state.
    destination.stop().await;
    let resaved = destination.save().await.unwrap();
    assert_eq!(saved_value(resaved.device_state), 42);

    destination.start();
    let mut data = [0; 4];
    match destination.mmio_read(VirtioMmioRegister::CONFIG.0 as u64, &mut data) {
        IoResult::Defer(token) => token.read_future(&mut data).await.unwrap(),
        other => panic!("expected deferred config read, got {other:?}"),
    }
    assert_eq!(u32::from_ne_bytes(data), 42);
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);

    destination.stop().await;
    let mut invalid = destination.save().await.unwrap();
    invalid.device_state = Some(vmcore::save_restore::SavedStateBlob::new(
        vmcore::save_restore::NoSavedState,
    ));
    assert!(destination.restore(invalid).await.is_err());
    // Validation rejects the state before the device sees it.
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
}

#[async_test]
async fn pci_private_state_round_trip(driver: DefaultDriver) {
    use vmcore::save_restore::AsyncSaveRestore;

    const BAR0_BASE: u64 = 0x10000000000;
    const DEVICE_CONFIG_OFFSET: u64 = 64;

    let (source_device, _) = PrivateStateTestDevice::new(42, 0, false);
    let mut source = PciTestTransport::new(Box::new(source_device), &driver, 0);
    source.stop().await;
    let saved = source.dev.save().await.unwrap();

    let (destination_device, probe) = PrivateStateTestDevice::new(0, 0, false);
    let mut destination = PciTestTransport::new(Box::new(destination_device), &driver, 0);
    destination.dev.restore(saved).await.unwrap();
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.value.load(Ordering::Relaxed), 42);

    destination.stop().await;
    let resaved = destination.dev.save().await.unwrap();
    assert_eq!(saved_value(resaved.device_state), 42);

    destination.start();
    let mut data = [0; 4];
    match destination
        .dev
        .mmio_read(BAR0_BASE + DEVICE_CONFIG_OFFSET, &mut data)
    {
        IoResult::Defer(token) => token.read_future(&mut data).await.unwrap(),
        other => panic!("expected deferred config read, got {other:?}"),
    }
    assert_eq!(u32::from_ne_bytes(data), 42);
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
}

async fn save_active_private_mmio(
    driver: &DefaultDriver,
    test_mem: &Arc<VirtioTestMemoryAccess>,
) -> <VirtioMmioDevice as vmcore::save_restore::AsyncSaveRestore>::SavedState {
    use vmcore::save_restore::AsyncSaveRestore;

    let guest = VirtioTestGuest::new_split(driver, test_mem, 1, 4, true);
    let (device, probe) = PrivateStateTestDevice::new(42, 1, false);
    let mut source = new_private_mmio(driver, GuestMemory::new("test", test_mem.clone()), device);
    guest
        .setup_chipset_device(&mut source, guest.queue_features())
        .await;
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 1);
    source.stop().await;
    let saved = source.save().await.unwrap();
    assert_eq!(
        saved.queues[0].common.queue_state,
        Some(QueueState {
            avail_index: 3,
            used_index: 1,
        })
    );
    saved
}

async fn save_active_private_pci(
    driver: &DefaultDriver,
    test_mem: &Arc<VirtioTestMemoryAccess>,
) -> <VirtioPciDevice as vmcore::save_restore::AsyncSaveRestore>::SavedState {
    use vmcore::save_restore::AsyncSaveRestore;

    let guest = VirtioTestGuest::new_split(driver, test_mem, 1, 4, true);
    let (device, probe) = PrivateStateTestDevice::new(42, 1, false);
    let mut source = VirtioPciTestDevice::new_with_device(driver, test_mem, Box::new(device));
    guest
        .setup_pci_device(&mut source, guest.queue_features())
        .await;
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 1);
    source.pci_device.stop().await;
    let saved = source.pci_device.save().await.unwrap();
    assert_eq!(
        saved.queues[0].common.queue_state,
        Some(QueueState {
            avail_index: 3,
            used_index: 1,
        })
    );
    saved
}

#[async_test]
async fn mmio_active_private_state_restores_before_queue(driver: DefaultDriver) {
    use vmcore::save_restore::AsyncSaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let saved = save_active_private_mmio(&driver, &test_mem).await;
    let (device, probe) = PrivateStateTestDevice::new(0, 1, false);
    let mut destination = new_private_mmio(&driver, GuestMemory::new("test", test_mem), device);

    destination.restore(saved).await.unwrap();
    // The restore starts no queue.
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);

    destination.start();
    destination.stop().await;
    assert_eq!(probe.value_at_start.load(Ordering::Relaxed), 42);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.initial_avail.load(Ordering::Relaxed), 3);
    assert_eq!(probe.initial_used.load(Ordering::Relaxed), 1);
    assert_eq!(destination.read_u32(112) & VIRTIO_DEVICE_NEEDS_RESET, 0);
}

#[async_test]
async fn pci_active_private_state_restores_before_queue(driver: DefaultDriver) {
    use vmcore::save_restore::AsyncSaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let saved = save_active_private_pci(&driver, &test_mem).await;
    let (device, probe) = PrivateStateTestDevice::new(0, 1, false);
    let mut destination =
        VirtioPciTestDevice::new_with_device(&driver, &test_mem, Box::new(device));

    destination.pci_device.restore(saved).await.unwrap();
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);

    destination.pci_device.start();
    destination.pci_device.stop().await;
    assert_eq!(probe.value_at_start.load(Ordering::Relaxed), 42);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.initial_avail.load(Ordering::Relaxed), 3);
    assert_eq!(probe.initial_used.load(Ordering::Relaxed), 1);
    assert_eq!(
        destination.pci_device.read_u32(20) & VIRTIO_DEVICE_NEEDS_RESET,
        0
    );
}

#[async_test]
async fn mmio_resave_before_first_start_keeps_restored_state(driver: DefaultDriver) {
    use vmcore::save_restore::AsyncSaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let saved = save_active_private_mmio(&driver, &test_mem).await;
    let (device, probe) = PrivateStateTestDevice::new(0, 1, false);
    let mut destination = new_private_mmio(&driver, GuestMemory::new("test", test_mem), device);

    destination.restore(saved).await.unwrap();
    destination.stop().await;
    let resaved = destination.save().await.unwrap();
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);
    assert_eq!(
        resaved.queues[0].common.queue_state,
        Some(QueueState {
            avail_index: 3,
            used_index: 1,
        })
    );
    assert_eq!(saved_value(resaved.device_state), 42);
}

#[async_test]
async fn pci_resave_before_first_start_keeps_restored_state(driver: DefaultDriver) {
    use vmcore::save_restore::AsyncSaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let saved = save_active_private_pci(&driver, &test_mem).await;
    let (device, probe) = PrivateStateTestDevice::new(0, 1, false);
    let mut destination =
        VirtioPciTestDevice::new_with_device(&driver, &test_mem, Box::new(device));

    destination.pci_device.restore(saved).await.unwrap();
    destination.pci_device.stop().await;
    let resaved = destination.pci_device.save().await.unwrap();
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);
    assert_eq!(
        resaved.queues[0].common.queue_state,
        Some(QueueState {
            avail_index: 3,
            used_index: 1,
        })
    );
    assert_eq!(saved_value(resaved.device_state), 42);
}

#[async_test]
async fn mmio_active_zero_queue_device_restores_private_state(driver: DefaultDriver) {
    use vmcore::save_restore::AsyncSaveRestore;

    let (source_device, _) = PrivateStateTestDevice::new(42, 0, false);
    let mut source = MmioTestTransport::new(Box::new(source_device), &driver, 0);
    source.write_driver_ok();
    yield_and_poll(&mut source).await;
    assert_ne!(source.read_status() & VIRTIO_DRIVER_OK, 0);
    source.stop().await;
    let saved = source.dev.save().await.unwrap();

    let (destination_device, probe) = PrivateStateTestDevice::new(0, 0, false);
    let mut destination = MmioTestTransport::new(Box::new(destination_device), &driver, 0);
    destination.dev.restore(saved).await.unwrap();
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.value.load(Ordering::Relaxed), 42);
    destination.start();
    destination.stop().await;
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);
    assert_eq!(destination.read_status() & VIRTIO_DEVICE_NEEDS_RESET, 0);
}

#[async_test]
async fn pci_active_zero_queue_device_restores_private_state(driver: DefaultDriver) {
    use vmcore::save_restore::AsyncSaveRestore;

    let (source_device, _) = PrivateStateTestDevice::new(42, 0, false);
    let mut source = PciTestTransport::new(Box::new(source_device), &driver, 0);
    source.write_driver_ok();
    yield_and_poll(&mut source).await;
    assert_ne!(source.read_status() & VIRTIO_DRIVER_OK, 0);
    source.stop().await;
    let saved = source.dev.save().await.unwrap();

    let (destination_device, probe) = PrivateStateTestDevice::new(0, 0, false);
    let mut destination = PciTestTransport::new(Box::new(destination_device), &driver, 0);
    destination.dev.restore(saved).await.unwrap();
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.value.load(Ordering::Relaxed), 42);
    destination.start();
    destination.stop().await;
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);
    assert_eq!(destination.read_status() & VIRTIO_DEVICE_NEEDS_RESET, 0);
}

#[async_test]
async fn mmio_kick_before_driver_ok_reaches_restored_queue_at_start(driver: DefaultDriver) {
    use vmcore::save_restore::AsyncSaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let mut saved = save_active_private_mmio(&driver, &test_mem).await;
    saved.common.device_status &= !(VIRTIO_DRIVER_OK as u8);
    let (device, probe) = PrivateStateTestDevice::new(0, 1, false);
    let mut destination = new_private_mmio(&driver, GuestMemory::new("test", test_mem), device);

    destination.restore(saved).await.unwrap();
    destination.start();
    destination.write_u32(80, 0);
    destination.write_u32(80, 0);
    yield_now().await;
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);
    assert!(!probe.take_queue_kick());

    destination.write_u32(112, VIRTIO_DRIVER_OK);
    yield_and_poll_device(&mut destination).await;
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.initial_avail.load(Ordering::Relaxed), 3);
    assert_eq!(probe.initial_used.load(Ordering::Relaxed), 1);
    assert!(probe.take_queue_kick());
    assert!(!probe.take_queue_kick());
}

#[async_test]
async fn pci_kick_before_driver_ok_reaches_restored_queue_at_start(driver: DefaultDriver) {
    use vmcore::save_restore::AsyncSaveRestore;

    const BAR0_BASE: u64 = 0x10000000000;
    let test_mem = VirtioTestMemoryAccess::new();
    let mut saved = save_active_private_pci(&driver, &test_mem).await;
    saved.common.device_status &= !(VIRTIO_DRIVER_OK as u8);
    let (device, probe) = PrivateStateTestDevice::new(0, 1, false);
    let mut destination =
        VirtioPciTestDevice::new_with_device(&driver, &test_mem, Box::new(device));

    destination.pci_device.restore(saved).await.unwrap();
    destination.pci_device.start();
    for _ in 0..2 {
        destination
            .pci_device
            .mmio_write(
                BAR0_BASE + VIRTIO_PCI_COMMON_CFG_SIZE as u64,
                &0u16.to_ne_bytes(),
            )
            .unwrap();
    }
    yield_now().await;
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);
    assert!(!probe.take_queue_kick());

    let current = destination.pci_device.read_u32(20);
    destination
        .pci_device
        .write_u32(20, (current & !0xff) | VIRTIO_DRIVER_OK);
    yield_and_poll_device(&mut destination.pci_device).await;
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.initial_avail.load(Ordering::Relaxed), 3);
    assert_eq!(probe.initial_used.load(Ordering::Relaxed), 1);
    assert!(probe.take_queue_kick());
    assert!(!probe.take_queue_kick());
}

#[async_test]
async fn mmio_reset_before_first_start_resets_restored_private_state(driver: DefaultDriver) {
    use crate::spec::mmio::VirtioMmioRegister;
    use vmcore::save_restore::AsyncSaveRestore;

    let (source_device, _) = PrivateStateTestDevice::new(42, 0, false);
    let mut source = MmioTestTransport::new(Box::new(source_device), &driver, 0);
    source.stop().await;
    let saved = source.dev.save().await.unwrap();

    let (destination_device, probe) = PrivateStateTestDevice::new(0, 0, false);
    let mut destination = MmioTestTransport::new(Box::new(destination_device), &driver, 0);
    destination.dev.restore(saved).await.unwrap();
    destination.start();
    destination.write_status_zero();

    let mut data = [0; 4];
    match destination
        .dev
        .mmio_read(VirtioMmioRegister::CONFIG.0 as u64, &mut data)
    {
        IoResult::Defer(token) => token.read_future(&mut data).await.unwrap(),
        other => panic!("expected deferred config read, got {other:?}"),
    }
    assert_eq!(probe.reset_count.load(Ordering::Relaxed), 1);
    assert_eq!(u32::from_ne_bytes(data), 0);
}

#[async_test]
async fn pci_reset_before_first_start_resets_restored_private_state(driver: DefaultDriver) {
    use vmcore::save_restore::AsyncSaveRestore;

    const BAR0_BASE: u64 = 0x10000000000;
    const DEVICE_CONFIG_OFFSET: u64 = 64;
    let (source_device, _) = PrivateStateTestDevice::new(42, 0, false);
    let mut source = PciTestTransport::new(Box::new(source_device), &driver, 0);
    source.stop().await;
    let saved = source.dev.save().await.unwrap();

    let (destination_device, probe) = PrivateStateTestDevice::new(0, 0, false);
    let mut destination = PciTestTransport::new(Box::new(destination_device), &driver, 0);
    destination.dev.restore(saved).await.unwrap();
    destination.start();
    destination.write_status_zero();

    let mut data = [0; 4];
    match destination
        .dev
        .mmio_read(BAR0_BASE + DEVICE_CONFIG_OFFSET, &mut data)
    {
        IoResult::Defer(token) => token.read_future(&mut data).await.unwrap(),
        other => panic!("expected deferred config read, got {other:?}"),
    }
    assert_eq!(probe.reset_count.load(Ordering::Relaxed), 1);
    assert_eq!(u32::from_ne_bytes(data), 0);
}

#[async_test]
async fn mmio_reset_discards_restored_queue_progress(driver: DefaultDriver) {
    use vmcore::save_restore::AsyncSaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, true);
    let mut saved = save_active_private_mmio(&driver, &test_mem).await;
    saved.common.device_status &= !(VIRTIO_DRIVER_OK as u8);
    let (device, probe) = PrivateStateTestDevice::new(0, 1, false);
    let mut destination = new_private_mmio(&driver, GuestMemory::new("test", test_mem), device);
    destination.restore(saved).await.unwrap();
    destination.start();

    destination.write_u32(112, 0);
    guest
        .setup_chipset_device(&mut destination, guest.queue_features())
        .await;
    assert_eq!(probe.reset_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.initial_avail.load(Ordering::Relaxed), usize::MAX);
    assert_eq!(probe.initial_used.load(Ordering::Relaxed), usize::MAX);
}

#[async_test]
async fn pci_reset_discards_restored_queue_progress(driver: DefaultDriver) {
    use vmcore::save_restore::AsyncSaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, true);
    let mut saved = save_active_private_pci(&driver, &test_mem).await;
    saved.common.device_status &= !(VIRTIO_DRIVER_OK as u8);
    let (device, probe) = PrivateStateTestDevice::new(0, 1, false);
    let mut destination =
        VirtioPciTestDevice::new_with_device(&driver, &test_mem, Box::new(device));
    destination.pci_device.restore(saved).await.unwrap();
    destination.pci_device.start();

    let current = destination.pci_device.read_u32(20);
    destination.pci_device.write_u32(20, current & !0xff);
    guest
        .setup_pci_device(&mut destination, guest.queue_features())
        .await;
    assert_eq!(probe.reset_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.initial_avail.load(Ordering::Relaxed), usize::MAX);
    assert_eq!(probe.initial_used.load(Ordering::Relaxed), usize::MAX);
}

/// Waits for the device task of a dropped transport to drop its device.
async fn assert_device_dropped(probe: &PrivateStateProbe) {
    for _ in 0..10 {
        if probe.dropped.load(Ordering::Relaxed) {
            break;
        }
        yield_now().await;
    }
    assert!(probe.dropped.load(Ordering::Relaxed));
}

#[async_test]
async fn mmio_removal_before_first_start_releases_device(driver: DefaultDriver) {
    use vmcore::save_restore::AsyncSaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let saved = save_active_private_mmio(&driver, &test_mem).await;
    let (device, probe) = PrivateStateTestDevice::new(0, 1, false);
    let mut destination = new_private_mmio(&driver, GuestMemory::new("test", test_mem), device);
    destination.restore(saved).await.unwrap();
    assert!(!probe.dropped.load(Ordering::Relaxed));

    drop(destination);
    assert_device_dropped(&probe).await;
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);
}

#[async_test]
async fn pci_removal_before_first_start_releases_device(driver: DefaultDriver) {
    use vmcore::save_restore::AsyncSaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let saved = save_active_private_pci(&driver, &test_mem).await;
    let (device, probe) = PrivateStateTestDevice::new(0, 1, false);
    let mut destination =
        VirtioPciTestDevice::new_with_device(&driver, &test_mem, Box::new(device));
    destination.pci_device.restore(saved).await.unwrap();
    assert!(!probe.dropped.load(Ordering::Relaxed));

    drop(destination);
    assert_device_dropped(&probe).await;
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);
}

#[async_test]
async fn mmio_restore_reports_private_state_failure(driver: DefaultDriver) {
    use vmcore::save_restore::AsyncSaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let saved = save_active_private_mmio(&driver, &test_mem).await;
    let (device, probe) = PrivateStateTestDevice::new(0, 1, true);
    let mut destination = new_private_mmio(&driver, GuestMemory::new("test", test_mem), device);

    let error = destination.restore(saved).await.unwrap_err();
    assert!(
        error_chain(&error).contains("intentional private-state restore failure"),
        "{error:?}"
    );
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);
}

#[async_test]
async fn pci_restore_reports_private_state_failure(driver: DefaultDriver) {
    use vmcore::save_restore::AsyncSaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let saved = save_active_private_pci(&driver, &test_mem).await;
    let (device, probe) = PrivateStateTestDevice::new(0, 1, true);
    let mut destination =
        VirtioPciTestDevice::new_with_device(&driver, &test_mem, Box::new(device));

    let error = destination.pci_device.restore(saved).await.unwrap_err();
    assert!(
        error_chain(&error).contains("intentional private-state restore failure"),
        "{error:?}"
    );
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);
}

/// Starts the device's two queues, then resumes it with the second queue
/// failing to start.
async fn fail_resume(transport: &mut impl TestTransport, probe: &PrivateStateProbe) {
    transport.write_driver_ok();
    yield_and_poll(transport).await;
    assert_ne!(transport.read_status() & VIRTIO_DRIVER_OK, 0);
    assert_eq!(probe.running_count.load(Ordering::Relaxed), 2);
    transport.stop().await;

    probe.fail_start_at.store(1, Ordering::Relaxed);
    transport.start();
}

/// A resume whose second queue fails to start fails its confirmation and
/// leaves the device as it was: the queue that started is stopped again, and
/// the device is not marked as needing reset, since the guest does not run
/// until a start succeeds. The VM's next start then starts both queues.
async fn verify_failed_resume_fails_its_confirmation(
    transport: &mut impl TestTransport,
    probe: &PrivateStateProbe,
) {
    fail_resume(transport, probe).await;
    let error = transport.confirm_start().await.unwrap_err();
    assert!(
        format!("{error:#}").contains("intentional queue start failure"),
        "{error:#}"
    );
    let status = transport.read_status();
    assert_eq!(status & VIRTIO_DEVICE_NEEDS_RESET, 0);
    assert_ne!(status & VIRTIO_DRIVER_OK, 0);
    assert_eq!(probe.running_count.load(Ordering::Relaxed), 0);

    // The VM stops the device again, and its next start succeeds.
    transport.stop().await;
    assert_eq!(transport.read_status() & VIRTIO_DEVICE_NEEDS_RESET, 0);
    probe.fail_start_at.store(usize::MAX, Ordering::Relaxed);
    transport.start();
    transport.confirm_start().await.unwrap();
    assert_eq!(probe.running_count.load(Ordering::Relaxed), 2);
}

/// A resume that nothing confirms leaves the failure to the next stop, which
/// marks the device as needing reset.
async fn verify_stop_applies_failed_start(
    transport: &mut impl TestTransport,
    probe: &PrivateStateProbe,
) {
    fail_resume(transport, probe).await;
    transport.stop().await;
    assert_ne!(transport.read_status() & VIRTIO_DEVICE_NEEDS_RESET, 0);
    assert_eq!(probe.running_count.load(Ordering::Relaxed), 0);
}

#[async_test]
async fn failed_resume_fails_its_confirmation_mmio(driver: DefaultDriver) {
    let (device, probe) = PrivateStateTestDevice::new(0, 2, false);
    let mut transport = MmioTestTransport::new(Box::new(device), &driver, 2);
    verify_failed_resume_fails_its_confirmation(&mut transport, &probe).await;
}

#[async_test]
async fn failed_resume_fails_its_confirmation_pci(driver: DefaultDriver) {
    let (device, probe) = PrivateStateTestDevice::new(0, 2, false);
    let mut transport = PciTestTransport::new(Box::new(device), &driver, 2);
    verify_failed_resume_fails_its_confirmation(&mut transport, &probe).await;
}

#[async_test]
async fn stop_applies_failed_start_mmio(driver: DefaultDriver) {
    let (device, probe) = PrivateStateTestDevice::new(0, 2, false);
    let mut transport = MmioTestTransport::new(Box::new(device), &driver, 2);
    verify_stop_applies_failed_start(&mut transport, &probe).await;
}

#[async_test]
async fn stop_applies_failed_start_pci(driver: DefaultDriver) {
    let (device, probe) = PrivateStateTestDevice::new(0, 2, false);
    let mut transport = PciTestTransport::new(Box::new(device), &driver, 2);
    verify_stop_applies_failed_start(&mut transport, &probe).await;
}

/// Stops and starts an active device twice, as ordinary pauses and resumes
/// do: each start resumes both queues from the progress of the stop before
/// it, and nothing restores or resets the device.
async fn verify_stop_start_keeps_queue_progress(
    transport: &mut impl TestTransport,
    probe: &PrivateStateProbe,
) {
    transport.write_driver_ok();
    yield_and_poll(transport).await;
    assert_eq!(probe.running_count.load(Ordering::Relaxed), 2);
    probe.value.store(42, Ordering::Relaxed);
    transport.stop().await;
    assert_eq!(probe.running_count.load(Ordering::Relaxed), 0);

    for starts in [4, 6] {
        probe.initial_avail.store(usize::MAX, Ordering::Relaxed);
        probe.initial_used.store(usize::MAX, Ordering::Relaxed);
        transport.start();
        // The stop waits for the start, then stops the queues again.
        transport.stop().await;
        assert_eq!(probe.start_count.load(Ordering::Relaxed), starts);
        assert_eq!(probe.running_count.load(Ordering::Relaxed), 0);
        assert_eq!(probe.initial_avail.load(Ordering::Relaxed), 3);
        assert_eq!(probe.initial_used.load(Ordering::Relaxed), 1);
    }

    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 0);
    assert_eq!(probe.reset_count.load(Ordering::Relaxed), 0);
    assert_eq!(probe.value.load(Ordering::Relaxed), 42);
    let status = transport.read_status();
    assert_ne!(status & VIRTIO_DRIVER_OK, 0);
    assert_eq!(status & VIRTIO_DEVICE_NEEDS_RESET, 0);
}

#[async_test]
async fn stop_start_keeps_queue_progress_mmio(driver: DefaultDriver) {
    let (device, probe) = PrivateStateTestDevice::new(0, 2, false);
    let mut transport = MmioTestTransport::new(Box::new(device), &driver, 2);
    verify_stop_start_keeps_queue_progress(&mut transport, &probe).await;
}

#[async_test]
async fn stop_start_keeps_queue_progress_pci(driver: DefaultDriver) {
    let (device, probe) = PrivateStateTestDevice::new(0, 2, false);
    let mut transport = PciTestTransport::new(Box::new(device), &driver, 2);
    verify_stop_start_keeps_queue_progress(&mut transport, &probe).await;
}

#[async_test]
async fn mmio_restore_skips_disabled_zero_address_ring(driver: DefaultDriver) {
    use vmcore::save_restore::AsyncSaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let mut saved = save_active_private_mmio(&driver, &test_mem).await;
    saved.common.device_status &= !(VIRTIO_DRIVER_OK as u8);
    let queue = &mut saved.queues[0].common;
    queue.enable = false;
    queue.desc_addr = 0;
    queue.avail_addr = 0;
    queue.used_addr = 0;
    queue.queue_state = None;
    let (device, probe) = PrivateStateTestDevice::new(0, 1, false);
    let mut destination = new_private_mmio(&driver, GuestMemory::new("test", test_mem), device);
    destination.restore(saved).await.unwrap();
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    destination.start();
    destination.stop().await;
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);
}

#[async_test]
async fn pci_restore_skips_disabled_zero_address_ring(driver: DefaultDriver) {
    use vmcore::save_restore::AsyncSaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let mut saved = save_active_private_pci(&driver, &test_mem).await;
    saved.common.device_status &= !(VIRTIO_DRIVER_OK as u8);
    let queue = &mut saved.queues[0].common;
    queue.enable = false;
    queue.desc_addr = 0;
    queue.avail_addr = 0;
    queue.used_addr = 0;
    queue.queue_state = None;
    let (device, probe) = PrivateStateTestDevice::new(0, 1, false);
    let mut destination =
        VirtioPciTestDevice::new_with_device(&driver, &test_mem, Box::new(device));
    destination.pci_device.restore(saved).await.unwrap();
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    destination.pci_device.start();
    destination.pci_device.stop().await;
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);
}

#[async_test]
async fn pci_restore_rejects_invalid_private_state(driver: DefaultDriver) {
    use vmcore::save_restore::AsyncSaveRestore;

    let (source_device, _) = PrivateStateTestDevice::new(42, 0, false);
    let mut source = PciTestTransport::new(Box::new(source_device), &driver, 0);
    source.stop().await;
    let mut saved = source.dev.save().await.unwrap();
    saved.device_state = Some(vmcore::save_restore::SavedStateBlob::new(
        vmcore::save_restore::NoSavedState,
    ));

    let (destination_device, probe) = PrivateStateTestDevice::new(0, 0, false);
    let mut destination = PciTestTransport::new(Box::new(destination_device), &driver, 0);
    assert!(destination.dev.restore(saved).await.is_err());
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 0);
}
