// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for virtio-mmio interrupt-status delivery: used-buffer acknowledgement
//! through the `INTERRUPT_ACK` doorbell, the shared interrupt-status mode, and
//! devices that opt out of accelerated doorbells.

use super::*;
use crate::transport::VirtioMmioInterruptMode;
use test_with_tracing::test;

/// The event of a doorbell registration, recorded so that tests can signal
/// the doorbell. Dropping it removes the record.
pub(super) struct DoorbellEvent {
    events: Arc<Mutex<Vec<(DoorbellSpec, Event)>>>,
    spec: DoorbellSpec,
}

impl DoorbellEvent {
    pub(super) fn register(
        events: &Arc<Mutex<Vec<(DoorbellSpec, Event)>>>,
        spec: DoorbellSpec,
        event: &Event,
    ) -> Self {
        events.lock().push((spec, event.clone()));
        Self {
            events: events.clone(),
            spec,
        }
    }
}

impl Drop for DoorbellEvent {
    fn drop(&mut self) {
        let mut events = self.events.lock();
        if let Some(i) = events.iter().position(|(spec, _)| *spec == self.spec) {
            events.remove(i);
        }
    }
}

impl VirtioTestMemoryAccess {
    fn signal_doorbell(&self, spec: DoorbellSpec) -> bool {
        let event = self
            .doorbell_events
            .lock()
            .iter()
            .find_map(|(registered, event)| (*registered == spec).then(|| event.clone()));
        if let Some(event) = event {
            event.signal();
            true
        } else {
            false
        }
    }

    /// The `compare_exchange_fallback` of the test memory, which the shared
    /// interrupt-status word uses.
    pub(super) fn compare_exchange_memory(
        &self,
        address: u64,
        current: &mut [u8],
        new: &[u8],
    ) -> Result<bool, GuestMemoryBackingError> {
        let mut map = self.memory_map.lock();
        let Some((true, value)) = map.get(address, current.len()) else {
            panic!("Unexpected compare exchange request at address {address:x}");
        };
        if value == current {
            value.copy_from_slice(new);
            Ok(true)
        } else {
            current.copy_from_slice(value);
            Ok(false)
        }
    }
}

/// Acknowledges the pending used-buffer interrupt of a restored device, through
/// the `INTERRUPT_ACK` doorbell when it is accelerated, and checks that the
/// status bit clears.
pub(super) fn check_used_buffer_ack(
    test_mem: &VirtioTestMemoryAccess,
    dev2: &mut VirtioMmioDevice,
    register_doorbells: bool,
) {
    use crate::spec::mmio::VirtioMmioRegister;

    let ack = DoorbellSpec {
        address: VirtioMmioRegister::INTERRUPT_ACK.0 as u64,
        value: Some(VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER.into()),
        length: Some(4),
    };
    let accelerated = register_doorbells && cfg!(target_os = "linux");
    assert_eq!(test_mem.installed_doorbells().contains(&ack), accelerated);
    if accelerated {
        assert!(test_mem.signal_doorbell(ack));
    } else {
        dev2.write_u32(
            VirtioMmioRegister::INTERRUPT_ACK.0 as u64,
            VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER,
        );
    }
    assert_eq!(
        dev2.read_u32(VirtioMmioRegister::INTERRUPT_STATUS.0 as u64)
            & VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER,
        0
    );
}

/// A [`TestDevice`] that opts out of accelerated queue doorbells.
#[derive(InspectMut)]
#[inspect(skip)]
struct NoAcceleratedDoorbells(TestDevice);

impl VirtioDevice for NoAcceleratedDoorbells {
    fn traits(&self) -> DeviceTraits {
        VirtioDevice::traits(&self.0)
    }

    fn supports_accelerated_doorbells(&self) -> bool {
        false
    }

    async fn read_registers_u32(&mut self, offset: u16) -> u32 {
        VirtioDevice::read_registers_u32(&mut self.0, offset).await
    }

    async fn write_registers_u32(&mut self, offset: u16, val: u32) {
        VirtioDevice::write_registers_u32(&mut self.0, offset, val).await
    }

    async fn start_queue(
        &mut self,
        idx: u16,
        resources: QueueResources,
        features: &VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        VirtioDevice::start_queue(&mut self.0, idx, resources, features, initial_state).await
    }

    async fn stop_queue(&mut self, idx: u16) -> Option<QueueState> {
        VirtioDevice::stop_queue(&mut self.0, idx).await
    }

    async fn reset(&mut self) {
        VirtioDevice::reset(&mut self.0).await
    }

    fn supports_save_restore(&self) -> bool {
        VirtioDevice::supports_save_restore(&self.0)
    }
}
#[async_test]
async fn mmio_used_buffer_ack_doorbell_deasserts_interrupt(driver: DefaultDriver) {
    for register_doorbells in [false, true] {
        mmio_used_buffer_ack_inner(driver.clone(), register_doorbells).await;
    }
}

async fn mmio_used_buffer_ack_inner(driver: DefaultDriver, register_doorbells: bool) {
    const MMIO_BASE: u64 = 0x1000;

    let test_mem = VirtioTestMemoryAccess::new();
    let mut guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 2, true);
    let target = TestLineInterruptTarget::new_arc();
    let interrupt = LineInterrupt::new_with_target("test", target.clone(), 0);
    let base_addr = guest.get_queue_descriptor_backing_memory_address(0);
    let queue_work = Arc::new(
        move |_: u16, queue: &mut VirtioQueue, work: VirtioQueueCallbackWork| {
            assert_eq!(work.payload[0].address, base_addr);
            queue.complete(work, 123);
        },
    );
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
    let mut dev = VirtioMmioDevice::new(
        Box::new(TestDevice::new(
            &driver_source,
            DeviceTraits {
                device_id: VirtioDeviceType::CONSOLE,
                device_features: VirtioDeviceFeatures::new()
                    .with_bank(0, VIRTIO_F_RING_EVENT_IDX | 2)
                    .with_bank(1, VIRTIO_F_VERSION_1),
                max_queues: 1,
                device_register_length: 0,
                ..Default::default()
            },
            Some(queue_work),
        )),
        &driver,
        guest.mem(),
        interrupt,
        if register_doorbells {
            Some(test_mem.clone())
        } else {
            None
        },
        MMIO_BASE,
        0x1000,
    )
    .unwrap();

    guest
        .setup_chipset_device(&mut dev, guest.queue_features())
        .await;
    guest.add_to_avail_queue(0);
    dev.write_u32(80, 0);
    poll_fn(|cx| target.poll_high(cx, 0)).await;
    assert_eq!(
        dev.read_u32(96) & VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER,
        VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER
    );

    let ack = DoorbellSpec {
        address: MMIO_BASE + 100,
        value: Some(VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER.into()),
        length: Some(4),
    };
    let accelerated = register_doorbells && cfg!(target_os = "linux");
    assert_eq!(test_mem.installed_doorbells().contains(&ack), accelerated);
    if accelerated {
        assert!(test_mem.signal_doorbell(ack));
    } else {
        dev.write_u32(100, VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER);
    }
    assert_eq!(
        dev.read_u32(96) & VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER,
        0
    );
    assert!(!target.is_high(0));
    dev.stop().await;
}

#[async_test]
async fn mmio_ack_before_completion_keeps_new_interrupt_asserted(driver: DefaultDriver) {
    for register_doorbells in [false, true] {
        mmio_ack_before_completion_inner(driver.clone(), register_doorbells).await;
    }
}

async fn mmio_ack_before_completion_inner(driver: DefaultDriver, register_doorbells: bool) {
    const MMIO_BASE: u64 = 0x2000;

    let test_mem = VirtioTestMemoryAccess::new();
    let mut guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, true);
    let target = TestLineInterruptTarget::new_arc();
    let interrupt = LineInterrupt::new_with_target("test", target.clone(), 0);
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
    let (completed, mut completions) = mesh::channel();
    let mut dev = VirtioMmioDevice::new(
        Box::new(TestDevice::new(
            &driver_source,
            DeviceTraits {
                device_id: VirtioDeviceType::CONSOLE,
                device_features: VirtioDeviceFeatures::new()
                    .with_bank(0, VIRTIO_F_RING_EVENT_IDX | 2)
                    .with_bank(1, VIRTIO_F_VERSION_1),
                max_queues: 1,
                device_register_length: 0,
                ..Default::default()
            },
            Some(Arc::new(
                move |_: u16, queue: &mut VirtioQueue, work: VirtioQueueCallbackWork| {
                    queue.complete(work, 123);
                    completed.send(());
                },
            )),
        )),
        &driver,
        guest.mem(),
        interrupt,
        if register_doorbells {
            Some(test_mem.clone())
        } else {
            None
        },
        MMIO_BASE,
        0x1000,
    )
    .unwrap();

    guest
        .setup_chipset_device(&mut dev, guest.queue_features())
        .await;
    guest.add_to_avail_queue(0);
    dev.write_u32(80, 0);
    poll_fn(|cx| target.poll_high(cx, 0)).await;
    must_recv_in_timeout(&mut completions, Duration::from_secs(5)).await;
    assert_eq!(guest.get_next_completed(0), Some((0, 123)));
    assert_eq!(
        dev.read_u32(96) & VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER,
        VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER
    );
    let ack = DoorbellSpec {
        address: MMIO_BASE + 100,
        value: Some(VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER.into()),
        length: Some(4),
    };
    let accelerated = register_doorbells && cfg!(target_os = "linux");
    assert_eq!(test_mem.installed_doorbells().contains(&ack), accelerated);
    if !accelerated {
        dev.write_u32(100, VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER);
        assert!(!target.is_high(0));
    }

    guest.add_to_avail_queue(0);
    dev.write_u32(80, 0);
    must_recv_in_timeout(&mut completions, Duration::from_secs(5)).await;
    assert_eq!(guest.get_next_completed(0), Some((0, 123)));
    if accelerated {
        // A deferred ACK must not clear a completion newer than the status read.
        assert!(test_mem.signal_doorbell(ack));
    }
    assert_eq!(
        dev.read_u32(96) & VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER,
        VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER
    );
    assert!(target.is_high(0));
    dev.stop().await;
}

#[async_test]
async fn mmio_shared_status_save_restore_and_reset(driver: DefaultDriver) {
    use vmcore::save_restore::SaveRestore;

    const STATUS_GPA: u64 = 0xfeed_0000;

    let test_mem = VirtioTestMemoryAccess::new();
    test_mem.modify_memory_map(STATUS_GPA, &0u32.to_ne_bytes(), true);
    let doorbell_registration: Arc<dyn DoorbellRegistration> = test_mem.clone();
    let mem = GuestMemory::new("test", test_mem.clone());
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
    let guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, true);
    let device = || {
        Box::new(TestDevice::new(
            &driver_source,
            DeviceTraits {
                device_id: VirtioDeviceType::CONSOLE,
                device_features: VirtioDeviceFeatures::new()
                    .with_bank(0, 2 | VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX),
                max_queues: 1,
                device_register_length: 0,
                ..Default::default()
            },
            None,
        ))
    };

    let source_target = TestLineInterruptTarget::new_arc();
    let mut source = VirtioMmioDevice::new_with_disabled_features_and_interrupt_mode(
        device(),
        &driver_source.simple(),
        mem.clone(),
        LineInterrupt::new_with_target("shared-status-source", source_target.clone(), 0),
        Some(doorbell_registration.clone()),
        0,
        0x1000,
        0,
        VirtioMmioInterruptMode::SharedStatus {
            status_gpa: STATUS_GPA,
        },
    )
    .unwrap();
    guest
        .setup_chipset_device(&mut source, guest.queue_features())
        .await;
    assert_eq!(test_mem.memory_map_get_u32(STATUS_GPA), 0);
    test_mem.modify_memory_map(
        STATUS_GPA,
        &VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE.to_ne_bytes(),
        true,
    );
    assert_eq!(
        test_mem.memory_map_get_u32(STATUS_GPA),
        VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE
    );
    assert!(!source_target.is_high(0));
    source.stop().await;
    let saved = source.save().unwrap();
    assert_eq!(
        saved.interrupt_status,
        VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE
    );
    drop(source);

    let destination_target = TestLineInterruptTarget::new_arc();
    let mut destination = VirtioMmioDevice::new_with_disabled_features_and_interrupt_mode(
        device(),
        &driver_source.simple(),
        mem,
        LineInterrupt::new_with_target("shared-status-destination", destination_target.clone(), 0),
        Some(doorbell_registration),
        0,
        0x1000,
        0,
        VirtioMmioInterruptMode::SharedStatus {
            status_gpa: STATUS_GPA,
        },
    )
    .unwrap();
    destination.restore(saved).unwrap();
    assert_eq!(
        test_mem.memory_map_get_u32(STATUS_GPA),
        VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE
    );
    assert!(!destination_target.is_high(0));

    destination.reset().await;
    assert_eq!(test_mem.memory_map_get_u32(STATUS_GPA), 0);
    assert!(!destination_target.is_high(0));
}

#[async_test]
async fn mmio_respects_accelerated_doorbell_opt_out(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let doorbell_registration: Arc<dyn DoorbellRegistration> = test_mem.clone();
    let mem = GuestMemory::new("test", test_mem.clone());
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
    let guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, true);

    let device = NoAcceleratedDoorbells(TestDevice::new(
        &driver_source,
        DeviceTraits {
            device_id: VirtioDeviceType::CONSOLE,
            device_features: VirtioDeviceFeatures::new()
                .with_bank(0, 2 | VIRTIO_F_RING_INDIRECT_DESC | VIRTIO_F_RING_EVENT_IDX),
            max_queues: 1,
            device_register_length: 0,
            ..Default::default()
        },
        None,
    ));
    let mut dev = VirtioMmioDevice::new(
        Box::new(device),
        &driver,
        mem,
        LineInterrupt::detached(),
        Some(doorbell_registration),
        0,
        1,
    )
    .unwrap();

    guest
        .setup_chipset_device(&mut dev, guest.queue_features())
        .await;

    assert!(test_mem.installed_doorbells().is_empty());
    assert_eq!(test_mem.doorbell_count.load(Ordering::Relaxed), 0);
    dev.stop().await;
}
