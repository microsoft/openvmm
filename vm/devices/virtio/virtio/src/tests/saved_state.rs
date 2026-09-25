// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for device-private saved state and restore: private-state round trips,
//! restore validation, the activation of staged private state, and staged queue
//! kicks.

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
}

#[derive(Clone)]
struct PrivateStateProbe {
    value: Arc<AtomicUsize>,
    restore_count: Arc<AtomicUsize>,
    start_count: Arc<AtomicUsize>,
    queue_event: Arc<Mutex<Option<Event>>>,
    initial_avail: Arc<AtomicUsize>,
    initial_used: Arc<AtomicUsize>,
    value_at_start: Arc<AtomicUsize>,
    fail_restore: Arc<AtomicBool>,
}

impl PrivateStateTestDevice {
    fn new(value: usize, max_queues: u16, fail_restore: bool) -> (Self, PrivateStateProbe) {
        let probe = PrivateStateProbe {
            value: Arc::new(AtomicUsize::new(value)),
            restore_count: Arc::new(AtomicUsize::new(0)),
            start_count: Arc::new(AtomicUsize::new(0)),
            queue_event: Arc::new(Mutex::new(None)),
            initial_avail: Arc::new(AtomicUsize::new(usize::MAX)),
            initial_used: Arc::new(AtomicUsize::new(usize::MAX)),
            value_at_start: Arc::new(AtomicUsize::new(usize::MAX)),
            fail_restore: Arc::new(AtomicBool::new(fail_restore)),
        };
        (
            Self {
                probe: probe.clone(),
                max_queues,
            },
            probe,
        )
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
        _idx: u16,
        resources: QueueResources,
        _features: &VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        self.probe.start_count.fetch_add(1, Ordering::Relaxed);
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

    async fn stop_queue(&mut self, _idx: u16) -> Option<QueueState> {
        (self.probe.start_count.load(Ordering::Relaxed) != 0).then_some(QueueState {
            avail_index: 3,
            used_index: 1,
        })
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
    saved: &mut <VirtioMmioDevice as vmcore::save_restore::SaveRestore>::SavedState,
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
#[async_test]
async fn mmio_restore_rejects_missing_active_queue_progress(driver: DefaultDriver) {
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::save_restore::SaveRestore;

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
    let mut saved = source.save().unwrap();
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
    assert!(destination.restore(saved).is_err());
}

#[async_test]
async fn mmio_private_state_round_trip(driver: DefaultDriver) {
    use crate::spec::mmio::VirtioMmioRegister;
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::save_restore::SaveRestore;

    let (source_device, _source_probe) = PrivateStateTestDevice::new(42, 0, false);
    let mut source = VirtioMmioDevice::new(
        Box::new(source_device),
        &driver,
        GuestMemory::empty(),
        LineInterrupt::detached(),
        None,
        0,
        0x1000,
    )
    .unwrap();
    source.stop().await;
    let saved = source.save().expect("private state save should succeed");
    assert!(saved.device_state.is_some());

    let (destination_device, probe) = PrivateStateTestDevice::new(0, 0, false);
    let mut destination = VirtioMmioDevice::new(
        Box::new(destination_device),
        &driver,
        GuestMemory::empty(),
        LineInterrupt::detached(),
        None,
        0,
        0x1000,
    )
    .unwrap();
    destination
        .restore(saved)
        .expect("private state restore should validate");
    destination.start_fallible().await.unwrap();
    assert_eq!(probe.value.load(Ordering::Relaxed), 0);
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 0);

    destination.stop().await;
    let resaved = destination.save().unwrap();
    assert_eq!(
        resaved
            .device_state
            .as_ref()
            .unwrap()
            .parse::<private_state::SavedState>()
            .unwrap()
            .value,
        42
    );
    destination.start_fallible().await.unwrap();
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 0);

    for expected_count in [1, 1] {
        let mut data = [0; 4];
        match destination.mmio_read(VirtioMmioRegister::CONFIG.0 as u64, &mut data) {
            IoResult::Defer(token) => token.read_future(&mut data).await.unwrap(),
            other => panic!("expected deferred config read, got {other:?}"),
        }
        assert_eq!(probe.value.load(Ordering::Relaxed), 42);
        assert_eq!(probe.restore_count.load(Ordering::Relaxed), expected_count);
    }

    destination.stop().await;
    let mut invalid = destination.save().unwrap();
    invalid.device_state = Some(vmcore::save_restore::SavedStateBlob::new(
        vmcore::save_restore::NoSavedState,
    ));
    assert!(destination.restore(invalid).is_err());
}

#[async_test]
async fn mmio_private_state_config_write_triggers_restore(driver: DefaultDriver) {
    use crate::spec::mmio::VirtioMmioRegister;
    use vmcore::save_restore::SaveRestore;

    let (source_device, _) = PrivateStateTestDevice::new(42, 0, false);
    let mut source = MmioTestTransport::new(Box::new(source_device), &driver, 0);
    source.stop().await;
    let saved = source.dev.save().unwrap();

    let (destination_device, probe) = PrivateStateTestDevice::new(0, 0, false);
    let mut destination = MmioTestTransport::new(Box::new(destination_device), &driver, 0);
    destination.dev.restore(saved).unwrap();
    destination.dev.start_fallible().await.unwrap();

    let value = 7u32;
    match destination
        .dev
        .mmio_write(VirtioMmioRegister::CONFIG.0 as u64, &value.to_ne_bytes())
    {
        IoResult::Defer(token) => token.write_future().await.unwrap(),
        other => panic!("expected deferred config write, got {other:?}"),
    }
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.value.load(Ordering::Relaxed), value as usize);
}

#[async_test]
async fn pci_private_state_config_triggers_restore(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    const BAR0_BASE: u64 = 0x10000000000;
    const DEVICE_CONFIG_OFFSET: u64 = 64;

    let (source_device, _) = PrivateStateTestDevice::new(42, 0, false);
    let mut source = PciTestTransport::new(Box::new(source_device), &driver, 0);
    source.stop().await;
    let saved = source.dev.save().unwrap();

    let (destination_device, probe) = PrivateStateTestDevice::new(0, 0, false);
    let mut destination = PciTestTransport::new(Box::new(destination_device), &driver, 0);
    destination.dev.restore(saved).unwrap();
    destination.dev.start_fallible().await.unwrap();
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 0);

    destination.stop().await;
    let resaved = destination.dev.save().unwrap();
    assert_eq!(
        resaved
            .device_state
            .as_ref()
            .unwrap()
            .parse::<private_state::SavedState>()
            .unwrap()
            .value,
        42
    );
    destination.dev.start_fallible().await.unwrap();
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 0);

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

#[async_test]
async fn pci_private_state_config_write_triggers_restore(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    const BAR0_BASE: u64 = 0x10000000000;
    const DEVICE_CONFIG_OFFSET: u64 = 64;

    let (source_device, _) = PrivateStateTestDevice::new(42, 0, false);
    let mut source = PciTestTransport::new(Box::new(source_device), &driver, 0);
    source.stop().await;
    let saved = source.dev.save().unwrap();

    let (destination_device, probe) = PrivateStateTestDevice::new(0, 0, false);
    let mut destination = PciTestTransport::new(Box::new(destination_device), &driver, 0);
    destination.dev.restore(saved).unwrap();
    destination.dev.start_fallible().await.unwrap();

    let value = 7u32;
    match destination
        .dev
        .mmio_write(BAR0_BASE + DEVICE_CONFIG_OFFSET, &value.to_ne_bytes())
    {
        IoResult::Defer(token) => token.write_future().await.unwrap(),
        other => panic!("expected deferred config write, got {other:?}"),
    }
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.value.load(Ordering::Relaxed), value as usize);
}

async fn save_active_private_mmio(
    driver: &DefaultDriver,
    test_mem: &Arc<VirtioTestMemoryAccess>,
) -> <VirtioMmioDevice as vmcore::save_restore::SaveRestore>::SavedState {
    use vmcore::save_restore::SaveRestore;

    let guest = VirtioTestGuest::new_split(driver, test_mem, 1, 4, true);
    let (device, probe) = PrivateStateTestDevice::new(42, 1, false);
    let mut source = VirtioMmioDevice::new(
        Box::new(device),
        driver,
        GuestMemory::new("test", test_mem.clone()),
        LineInterrupt::detached(),
        None,
        0,
        0x1000,
    )
    .unwrap();
    guest
        .setup_chipset_device(&mut source, guest.queue_features())
        .await;
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 1);
    source.stop().await;
    let saved = source.save().unwrap();
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
) -> <VirtioPciDevice as vmcore::save_restore::SaveRestore>::SavedState {
    use vmcore::save_restore::SaveRestore;

    let guest = VirtioTestGuest::new_split(driver, test_mem, 1, 4, true);
    let (device, probe) = PrivateStateTestDevice::new(42, 1, false);
    let mut source = VirtioPciTestDevice::new_with_device(driver, test_mem, Box::new(device));
    guest
        .setup_pci_device(&mut source, guest.queue_features())
        .await;
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 1);
    source.pci_device.stop().await;
    let saved = source.pci_device.save().unwrap();
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
    use vmcore::save_restore::SaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let saved = save_active_private_mmio(&driver, &test_mem).await;
    let (device, probe) = PrivateStateTestDevice::new(0, 1, false);
    let mut destination = VirtioMmioDevice::new(
        Box::new(device),
        &driver,
        GuestMemory::new("test", test_mem),
        LineInterrupt::detached(),
        None,
        0,
        0x1000,
    )
    .unwrap();

    destination.restore(saved).unwrap();
    destination.start_fallible().await.unwrap();
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.value_at_start.load(Ordering::Relaxed), 42);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.initial_avail.load(Ordering::Relaxed), 3);
    assert_eq!(probe.initial_used.load(Ordering::Relaxed), 1);
}

#[async_test]
async fn pci_active_private_state_restores_before_queue(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let saved = save_active_private_pci(&driver, &test_mem).await;
    let (device, probe) = PrivateStateTestDevice::new(0, 1, false);
    let mut destination =
        VirtioPciTestDevice::new_with_device(&driver, &test_mem, Box::new(device));

    destination.pci_device.restore(saved).unwrap();
    destination.pci_device.start_fallible().await.unwrap();
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.value_at_start.load(Ordering::Relaxed), 42);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.initial_avail.load(Ordering::Relaxed), 3);
    assert_eq!(probe.initial_used.load(Ordering::Relaxed), 1);
}

#[async_test]
async fn mmio_active_zero_queue_device_restores_private_state(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    let (source_device, _) = PrivateStateTestDevice::new(42, 0, false);
    let mut source = MmioTestTransport::new(Box::new(source_device), &driver, 0);
    source.write_driver_ok();
    yield_and_poll(&mut source).await;
    assert_ne!(source.read_status() & VIRTIO_DRIVER_OK, 0);
    source.stop().await;
    let saved = source.dev.save().unwrap();

    let (destination_device, probe) = PrivateStateTestDevice::new(0, 0, false);
    let mut destination = MmioTestTransport::new(Box::new(destination_device), &driver, 0);
    destination.dev.restore(saved).unwrap();
    destination.dev.start_fallible().await.unwrap();
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.value.load(Ordering::Relaxed), 42);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);
}

#[async_test]
async fn pci_active_zero_queue_device_restores_private_state(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    let (source_device, _) = PrivateStateTestDevice::new(42, 0, false);
    let mut source = PciTestTransport::new(Box::new(source_device), &driver, 0);
    source.write_driver_ok();
    yield_and_poll(&mut source).await;
    assert_ne!(source.read_status() & VIRTIO_DRIVER_OK, 0);
    source.stop().await;
    let saved = source.dev.save().unwrap();

    let (destination_device, probe) = PrivateStateTestDevice::new(0, 0, false);
    let mut destination = PciTestTransport::new(Box::new(destination_device), &driver, 0);
    destination.dev.restore(saved).unwrap();
    destination.dev.start_fallible().await.unwrap();
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.value.load(Ordering::Relaxed), 42);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);
}

#[async_test]
async fn mmio_staged_kick_starts_after_driver_ok(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let mut saved = save_active_private_mmio(&driver, &test_mem).await;
    saved.common.device_status &= !(VIRTIO_DRIVER_OK as u8);
    let (device, probe) = PrivateStateTestDevice::new(0, 1, false);
    let mut destination = VirtioMmioDevice::new(
        Box::new(device),
        &driver,
        GuestMemory::new("test", test_mem),
        LineInterrupt::detached(),
        None,
        0,
        0x1000,
    )
    .unwrap();

    destination.restore(saved).unwrap();
    destination.start_fallible().await.unwrap();
    destination.write_u32(80, 0);
    destination.write_u32(80, 0);
    yield_now().await;
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);
    assert!(!probe.take_queue_kick());

    destination.write_u32(112, VIRTIO_DRIVER_OK);
    yield_and_poll_device(&mut destination).await;
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.initial_avail.load(Ordering::Relaxed), 3);
    assert_eq!(probe.initial_used.load(Ordering::Relaxed), 1);
    assert!(probe.take_queue_kick());
    assert!(!probe.take_queue_kick());
}

#[async_test]
async fn pci_staged_kick_starts_after_driver_ok(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    const BAR0_BASE: u64 = 0x10000000000;
    let test_mem = VirtioTestMemoryAccess::new();
    let mut saved = save_active_private_pci(&driver, &test_mem).await;
    saved.common.device_status &= !(VIRTIO_DRIVER_OK as u8);
    let (device, probe) = PrivateStateTestDevice::new(0, 1, false);
    let mut destination =
        VirtioPciTestDevice::new_with_device(&driver, &test_mem, Box::new(device));

    destination.pci_device.restore(saved).unwrap();
    destination.pci_device.start_fallible().await.unwrap();
    destination
        .pci_device
        .mmio_write(
            BAR0_BASE + VIRTIO_PCI_COMMON_CFG_SIZE as u64,
            &0u16.to_ne_bytes(),
        )
        .unwrap();
    destination
        .pci_device
        .mmio_write(
            BAR0_BASE + VIRTIO_PCI_COMMON_CFG_SIZE as u64,
            &0u16.to_ne_bytes(),
        )
        .unwrap();
    yield_now().await;
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);
    assert!(!probe.take_queue_kick());

    let current = destination.pci_device.read_u32(20);
    destination
        .pci_device
        .write_u32(20, (current & !0xff) | VIRTIO_DRIVER_OK);
    yield_and_poll_device(&mut destination.pci_device).await;
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.initial_avail.load(Ordering::Relaxed), 3);
    assert_eq!(probe.initial_used.load(Ordering::Relaxed), 1);
    assert!(probe.take_queue_kick());
    assert!(!probe.take_queue_kick());
}

#[async_test]
async fn mmio_reset_discards_pending_private_state(driver: DefaultDriver) {
    use crate::spec::mmio::VirtioMmioRegister;
    use vmcore::save_restore::SaveRestore;

    let (source_device, _) = PrivateStateTestDevice::new(42, 0, false);
    let mut source = MmioTestTransport::new(Box::new(source_device), &driver, 0);
    source.stop().await;
    let saved = source.dev.save().unwrap();

    let (destination_device, probe) = PrivateStateTestDevice::new(0, 0, false);
    let mut destination = MmioTestTransport::new(Box::new(destination_device), &driver, 0);
    destination.dev.restore(saved).unwrap();
    destination.dev.start_fallible().await.unwrap();
    destination.write_status_zero();

    let mut data = [0; 4];
    match destination
        .dev
        .mmio_read(VirtioMmioRegister::CONFIG.0 as u64, &mut data)
    {
        IoResult::Defer(token) => token.read_future(&mut data).await.unwrap(),
        other => panic!("expected deferred config read, got {other:?}"),
    }
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 0);
    assert_eq!(u32::from_ne_bytes(data), 0);
}

#[async_test]
async fn pci_reset_discards_pending_private_state(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    const BAR0_BASE: u64 = 0x10000000000;
    const DEVICE_CONFIG_OFFSET: u64 = 64;
    let (source_device, _) = PrivateStateTestDevice::new(42, 0, false);
    let mut source = PciTestTransport::new(Box::new(source_device), &driver, 0);
    source.stop().await;
    let saved = source.dev.save().unwrap();

    let (destination_device, probe) = PrivateStateTestDevice::new(0, 0, false);
    let mut destination = PciTestTransport::new(Box::new(destination_device), &driver, 0);
    destination.dev.restore(saved).unwrap();
    destination.dev.start_fallible().await.unwrap();
    destination.write_status_zero();

    let mut data = [0; 4];
    match destination
        .dev
        .mmio_read(BAR0_BASE + DEVICE_CONFIG_OFFSET, &mut data)
    {
        IoResult::Defer(token) => token.read_future(&mut data).await.unwrap(),
        other => panic!("expected deferred config read, got {other:?}"),
    }
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 0);
    assert_eq!(u32::from_ne_bytes(data), 0);
}

#[async_test]
async fn mmio_config_restore_failure_completes_and_retains_state(driver: DefaultDriver) {
    use crate::spec::mmio::VirtioMmioRegister;
    use vmcore::save_restore::SaveRestore;

    let (source_device, _) = PrivateStateTestDevice::new(42, 0, false);
    let mut source = MmioTestTransport::new(Box::new(source_device), &driver, 0);
    source.stop().await;
    let saved = source.dev.save().unwrap();

    let (destination_device, probe) = PrivateStateTestDevice::new(0, 0, true);
    let mut destination = MmioTestTransport::new(Box::new(destination_device), &driver, 0);
    destination.dev.restore(saved).unwrap();
    destination.dev.start_fallible().await.unwrap();

    let mut data = [0; 4];
    let read_result = match destination
        .dev
        .mmio_read(VirtioMmioRegister::CONFIG.0 as u64, &mut data)
    {
        IoResult::Defer(token) => token.read_future(&mut data).await,
        other => panic!("expected deferred config read, got {other:?}"),
    };
    assert!(matches!(read_result, Err(IoError::NoResponse)));

    let write_result = match destination
        .dev
        .mmio_write(VirtioMmioRegister::CONFIG.0 as u64, &7u32.to_ne_bytes())
    {
        IoResult::Defer(token) => token.write_future().await,
        other => panic!("expected deferred config write, got {other:?}"),
    };
    assert!(matches!(write_result, Err(IoError::NoResponse)));
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 2);

    destination.stop().await;
    let resaved = destination.dev.save().unwrap();
    assert_eq!(
        resaved
            .device_state
            .unwrap()
            .parse::<private_state::SavedState>()
            .unwrap()
            .value,
        42
    );
}

#[async_test]
async fn pci_config_restore_failure_completes_and_retains_state(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    const BAR0_BASE: u64 = 0x10000000000;
    const DEVICE_CONFIG_OFFSET: u64 = 64;
    let (source_device, _) = PrivateStateTestDevice::new(42, 0, false);
    let mut source = PciTestTransport::new(Box::new(source_device), &driver, 0);
    source.stop().await;
    let saved = source.dev.save().unwrap();

    let (destination_device, probe) = PrivateStateTestDevice::new(0, 0, true);
    let mut destination = PciTestTransport::new(Box::new(destination_device), &driver, 0);
    destination.dev.restore(saved).unwrap();
    destination.dev.start_fallible().await.unwrap();

    let mut data = [0; 4];
    let read_result = match destination
        .dev
        .mmio_read(BAR0_BASE + DEVICE_CONFIG_OFFSET, &mut data)
    {
        IoResult::Defer(token) => token.read_future(&mut data).await,
        other => panic!("expected deferred config read, got {other:?}"),
    };
    assert!(matches!(read_result, Err(IoError::NoResponse)));

    let write_result = match destination
        .dev
        .mmio_write(BAR0_BASE + DEVICE_CONFIG_OFFSET, &7u32.to_ne_bytes())
    {
        IoResult::Defer(token) => token.write_future().await,
        other => panic!("expected deferred config write, got {other:?}"),
    };
    assert!(matches!(write_result, Err(IoError::NoResponse)));
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 2);

    destination.stop().await;
    let resaved = destination.dev.save().unwrap();
    assert_eq!(
        resaved
            .device_state
            .unwrap()
            .parse::<private_state::SavedState>()
            .unwrap()
            .value,
        42
    );
}

#[async_test]
async fn mmio_active_start_failure_preserves_saved_state(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let saved = save_active_private_mmio(&driver, &test_mem).await;
    let (device, probe) = PrivateStateTestDevice::new(0, 1, true);
    let mut destination = VirtioMmioDevice::new(
        Box::new(device),
        &driver,
        GuestMemory::new("test", test_mem),
        LineInterrupt::detached(),
        None,
        0,
        0x1000,
    )
    .unwrap();

    destination.restore(saved).unwrap();
    assert!(destination.start_fallible().await.is_err());
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);
    destination.stop().await;
    let resaved = destination.save().unwrap();
    assert_eq!(
        resaved.queues[0].common.queue_state,
        Some(QueueState {
            avail_index: 3,
            used_index: 1,
        })
    );
    assert_eq!(
        resaved
            .device_state
            .unwrap()
            .parse::<private_state::SavedState>()
            .unwrap()
            .value,
        42
    );
}

#[async_test]
async fn mmio_failed_kick_does_not_signal_restored_queue(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let mut saved = save_active_private_mmio(&driver, &test_mem).await;
    saved.common.device_status &= !(VIRTIO_DRIVER_OK as u8);
    let (device, probe) = PrivateStateTestDevice::new(0, 1, true);
    let mut destination = VirtioMmioDevice::new(
        Box::new(device),
        &driver,
        GuestMemory::new("test", test_mem),
        LineInterrupt::detached(),
        None,
        0,
        0x1000,
    )
    .unwrap();

    destination.restore(saved).unwrap();
    destination.start_fallible().await.unwrap();
    destination.write_u32(80, 0);
    yield_now().await;
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);

    probe.fail_restore.store(false, Ordering::Relaxed);
    destination.write_u32(112, VIRTIO_DRIVER_OK);
    yield_and_poll_device(&mut destination).await;
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 2);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 1);
    assert!(!probe.take_queue_kick());
}

#[async_test]
async fn pci_failed_kick_does_not_signal_restored_queue(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    const BAR0_BASE: u64 = 0x10000000000;
    let test_mem = VirtioTestMemoryAccess::new();
    let mut saved = save_active_private_pci(&driver, &test_mem).await;
    saved.common.device_status &= !(VIRTIO_DRIVER_OK as u8);
    let (device, probe) = PrivateStateTestDevice::new(0, 1, true);
    let mut destination =
        VirtioPciTestDevice::new_with_device(&driver, &test_mem, Box::new(device));

    destination.pci_device.restore(saved).unwrap();
    destination.pci_device.start_fallible().await.unwrap();
    destination
        .pci_device
        .mmio_write(
            BAR0_BASE + VIRTIO_PCI_COMMON_CFG_SIZE as u64,
            &0u16.to_ne_bytes(),
        )
        .unwrap();
    yield_now().await;
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);

    probe.fail_restore.store(false, Ordering::Relaxed);
    let current = destination.pci_device.read_u32(20);
    destination
        .pci_device
        .write_u32(20, (current & !0xff) | VIRTIO_DRIVER_OK);
    yield_and_poll_device(&mut destination.pci_device).await;
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 2);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 1);
    assert!(!probe.take_queue_kick());
}

#[async_test]
async fn mmio_driver_ok_restore_failure_releases_write(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let mut saved = save_active_private_mmio(&driver, &test_mem).await;
    saved.common.device_status &= !(VIRTIO_DRIVER_OK as u8);
    let (device, probe) = PrivateStateTestDevice::new(0, 1, true);
    let mut destination = VirtioMmioDevice::new(
        Box::new(device),
        &driver,
        GuestMemory::new("test", test_mem),
        LineInterrupt::detached(),
        None,
        0,
        0x1000,
    )
    .unwrap();
    destination.restore(saved).unwrap();
    destination.start_fallible().await.unwrap();

    let token = match destination.mmio_write(112, &VIRTIO_DRIVER_OK.to_ne_bytes()) {
        IoResult::Defer(token) => token,
        other => panic!("expected deferred status write, got {other:?}"),
    };
    yield_and_poll_device(&mut destination).await;
    token.write_future().await.unwrap();
    assert_eq!(destination.read_u32(112), 0);
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);

    let mut data = [0; 4];
    match destination.mmio_read(0x100, &mut data) {
        IoResult::Defer(token) => token.read_future(&mut data).await.unwrap(),
        other => panic!("expected deferred config read, got {other:?}"),
    }
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
}

#[async_test]
async fn pci_driver_ok_restore_failure_releases_write(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    const BAR0_BASE: u64 = 0x10000000000;
    let test_mem = VirtioTestMemoryAccess::new();
    let mut saved = save_active_private_pci(&driver, &test_mem).await;
    saved.common.device_status &= !(VIRTIO_DRIVER_OK as u8);
    let (device, probe) = PrivateStateTestDevice::new(0, 1, true);
    let mut destination =
        VirtioPciTestDevice::new_with_device(&driver, &test_mem, Box::new(device));
    destination.pci_device.restore(saved).unwrap();
    destination.pci_device.start_fallible().await.unwrap();

    let token = match destination
        .pci_device
        .mmio_write(BAR0_BASE + 20, &[VIRTIO_DRIVER_OK as u8])
    {
        IoResult::Defer(token) => token,
        other => panic!("expected deferred status write, got {other:?}"),
    };
    yield_and_poll_device(&mut destination.pci_device).await;
    token.write_future().await.unwrap();
    assert_eq!(destination.pci_device.read_u32(20) & 0xff, 0);
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);

    let mut data = [0; 4];
    match destination.pci_device.mmio_read(BAR0_BASE + 64, &mut data) {
        IoResult::Defer(token) => token.read_future(&mut data).await.unwrap(),
        other => panic!("expected deferred config read, got {other:?}"),
    }
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
}

#[async_test]
async fn mmio_reset_discards_staged_kick(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    let test_mem = VirtioTestMemoryAccess::new();
    let guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, true);
    let mut saved = save_active_private_mmio(&driver, &test_mem).await;
    saved.common.device_status &= !(VIRTIO_DRIVER_OK as u8);
    let (device, probe) = PrivateStateTestDevice::new(0, 1, false);
    let mut destination = VirtioMmioDevice::new(
        Box::new(device),
        &driver,
        GuestMemory::new("test", test_mem),
        LineInterrupt::detached(),
        None,
        0,
        0x1000,
    )
    .unwrap();
    destination.restore(saved).unwrap();
    destination.start_fallible().await.unwrap();
    destination.write_u32(80, 0);
    yield_now().await;
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);

    destination.write_u32(112, 0);
    guest
        .setup_chipset_device(&mut destination, guest.queue_features())
        .await;
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.initial_avail.load(Ordering::Relaxed), usize::MAX);
    assert_eq!(probe.initial_used.load(Ordering::Relaxed), usize::MAX);
    assert!(!probe.take_queue_kick());
}

#[async_test]
async fn pci_reset_discards_staged_kick(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    const BAR0_BASE: u64 = 0x10000000000;
    let test_mem = VirtioTestMemoryAccess::new();
    let guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, true);
    let mut saved = save_active_private_pci(&driver, &test_mem).await;
    saved.common.device_status &= !(VIRTIO_DRIVER_OK as u8);
    let (device, probe) = PrivateStateTestDevice::new(0, 1, false);
    let mut destination =
        VirtioPciTestDevice::new_with_device(&driver, &test_mem, Box::new(device));
    destination.pci_device.restore(saved).unwrap();
    destination.pci_device.start_fallible().await.unwrap();
    destination
        .pci_device
        .mmio_write(
            BAR0_BASE + VIRTIO_PCI_COMMON_CFG_SIZE as u64,
            &0u16.to_ne_bytes(),
        )
        .unwrap();
    yield_now().await;
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);

    let current = destination.pci_device.read_u32(20);
    destination.pci_device.write_u32(20, current & !0xff);
    guest
        .setup_pci_device(&mut destination, guest.queue_features())
        .await;
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 1);
    assert_eq!(probe.initial_avail.load(Ordering::Relaxed), usize::MAX);
    assert_eq!(probe.initial_used.load(Ordering::Relaxed), usize::MAX);
    assert!(!probe.take_queue_kick());
}

#[async_test]
async fn mmio_restore_skips_disabled_zero_address_ring(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

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
    let mut destination = VirtioMmioDevice::new(
        Box::new(device),
        &driver,
        GuestMemory::new("test", test_mem),
        LineInterrupt::detached(),
        None,
        0,
        0x1000,
    )
    .unwrap();
    destination.restore(saved).unwrap();
    destination.start_fallible().await.unwrap();
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 0);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);
}

#[async_test]
async fn pci_restore_skips_disabled_zero_address_ring(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

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
    destination.pci_device.restore(saved).unwrap();
    destination.pci_device.start_fallible().await.unwrap();
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 0);
    assert_eq!(probe.start_count.load(Ordering::Relaxed), 0);
}

#[async_test]
async fn pci_restore_rejects_invalid_private_state(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    let (source_device, _) = PrivateStateTestDevice::new(42, 0, false);
    let mut source = PciTestTransport::new(Box::new(source_device), &driver, 0);
    source.stop().await;
    let mut saved = source.dev.save().unwrap();
    saved.device_state = Some(vmcore::save_restore::SavedStateBlob::new(
        vmcore::save_restore::NoSavedState,
    ));

    let (destination_device, probe) = PrivateStateTestDevice::new(0, 0, false);
    let mut destination = PciTestTransport::new(Box::new(destination_device), &driver, 0);
    assert!(destination.dev.restore(saved).is_err());
    assert_eq!(probe.restore_count.load(Ordering::Relaxed), 0);
}
