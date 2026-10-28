// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for devices whose DMA is restricted to the RAM ranges of the microVM
//! e820 map: a device refuses a buffer or a ring in the shared
//! interrupt-status page, in the ISA hole, or across the MMIO aperture,
//! without reading or writing any of it, and the transport marks the device as
//! needing reset.

use super::*;
use crate::transport::VirtioMmioInterruptMode;
use std::ops::Range;
use test_with_tracing::test;

const PAGE: u64 = 0x1000;
/// The shared interrupt-status page of the microVM.
const STATUS_PAGE: u64 = 0x3_0000;
/// The legacy PC range from 640 KiB to 1 MiB.
const ISA_HOLE: Range<u64> = 0xa_0000..0x10_0000;
/// The fixed 3-to-4-GiB MMIO aperture.
const APERTURE: Range<u64> = 0xc000_0000..0x1_0000_0000;
/// The byte that guest memory holds until a device writes it.
const UNTOUCHED: u8 = 0x5a;
/// The byte that [`ReportingDevice`] writes to each writable buffer.
const WRITTEN: u8 = 0xee;

/// The RAM ranges of the e820 map of a microVM with 4 GiB of RAM.
fn microvm_ram() -> Vec<Range<u64>> {
    vec![
        0..STATUS_PAGE,
        STATUS_PAGE + PAGE..ISA_HOLE.start,
        ISA_HOLE.end..APERTURE.start,
        APERTURE.end..APERTURE.end + 0x4000_0000,
    ]
}

/// Buffers that lie in, or cross into, the shared interrupt-status page, the
/// ISA hole, or the aperture, as an address and a length.
const REFUSED_BUFFERS: [(u64, u32); 6] = [
    (STATUS_PAGE, 4),
    (STATUS_PAGE - 8, 16),
    (ISA_HOLE.start, PAGE as u32),
    (ISA_HOLE.end - 8, 16),
    (APERTURE.start - 8, 16),
    (APERTURE.start, 8),
];

/// Buffers that lie wholly inside RAM next to the ranges above.
const ADMITTED_BUFFERS: [(u64, u32); 4] = [
    (STATUS_PAGE - PAGE, PAGE as u32),
    (STATUS_PAGE + PAGE, 8),
    (ISA_HOLE.end, PAGE as u32),
    (APERTURE.start - PAGE, PAGE as u32),
];

/// Maps the guest memory around the reserved ranges, so that a test can tell
/// which bytes a device wrote. The status page holds a zero shared
/// interrupt-status word.
fn map_memory_around_reserved_ranges(test_mem: &VirtioTestMemoryAccess) {
    let untouched = [UNTOUCHED; PAGE as usize];
    for page in [
        STATUS_PAGE - PAGE,
        STATUS_PAGE + PAGE,
        ISA_HOLE.start,
        ISA_HOLE.end - PAGE,
        ISA_HOLE.end,
        APERTURE.start - PAGE,
        APERTURE.start,
    ] {
        test_mem.modify_memory_map(page, &untouched, true);
    }
    test_mem.modify_memory_map(STATUS_PAGE, &0u32.to_le_bytes(), true);
    test_mem.modify_memory_map(STATUS_PAGE + 4, &untouched[4..], true);
}

fn read_memory(test_mem: &VirtioTestMemoryAccess, address: u64, len: usize) -> Vec<u8> {
    let mut memory_map = test_mem.memory_map.lock();
    let (_, data) = memory_map.get(address, len).unwrap();
    data.to_vec()
}

/// Asserts that no device wrote `address..address + length`, apart from the
/// shared interrupt-status word, which the transport updates.
fn assert_untouched(test_mem: &VirtioTestMemoryAccess, address: u64, length: u32) {
    for gpa in address..address + u64::from(length) {
        if !(STATUS_PAGE..STATUS_PAGE + 4).contains(&gpa) {
            assert_eq!(read_memory(test_mem, gpa, 1), [UNTOUCHED], "{gpa:#x}");
        }
    }
}

/// Makes the first free descriptor of queue 0 available as a single buffer.
fn post_buffer(
    test_mem: &VirtioTestMemoryAccess,
    guest: &mut VirtioTestGuest,
    address: u64,
    length: u32,
    flags: DescriptorFlags,
) {
    guest.add_to_avail_queue(0);
    let descriptor = guest.get_queue_descriptor(0, 0);
    test_mem.modify_memory_map(descriptor, &address.to_le_bytes(), false);
    test_mem.modify_memory_map(descriptor + 8, &length.to_le_bytes(), false);
    test_mem.modify_memory_map(descriptor + 12, &u16::from(flags).to_le_bytes(), false);
}

/// Returns queue 0 of `guest` over its memory restricted to the microVM RAM
/// ranges, and the receiver of the refusals of that memory.
fn new_restricted_queue(
    driver: &DefaultDriver,
    guest: &VirtioTestGuest,
    params: QueueParams,
) -> (
    Result<VirtioQueue, QueueError>,
    std::sync::mpsc::Receiver<(u64, u64)>,
) {
    let (refusals, refused) = std::sync::mpsc::channel();
    let mem = guest
        .mem()
        .restrict(
            microvm_ram(),
            Some(Arc::new(move |gpa, len| {
                let _ = refusals.send((gpa, len));
            })),
        )
        .unwrap();
    let queue = VirtioQueue::new(
        guest.queue_features(),
        params,
        mem,
        Interrupt::null(),
        PolledWait::new(driver, Event::new()).unwrap(),
        None,
    );
    (queue, refused)
}

#[async_test]
async fn queue_refuses_buffers_outside_ram(driver: DefaultDriver) {
    for (address, length) in REFUSED_BUFFERS {
        let test_mem = VirtioTestMemoryAccess::new();
        let mut guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, false);
        let (queue, refused) = new_restricted_queue(&driver, &guest, guest.queue_params(0));
        let mut queue = queue.unwrap();
        post_buffer(
            &test_mem,
            &mut guest,
            address,
            length,
            DescriptorFlags::new().with_write(true),
        );

        let Err(error) = queue.try_next() else {
            panic!("the buffer at {address:#x} must be refused");
        };
        assert!(
            matches!(queue_error(&error), Some(QueueError::BufferNotAdmitted(_))),
            "{error:?}"
        );
        assert_eq!(
            refused.try_iter().collect::<Vec<_>>(),
            [(address, length.into())]
        );
        // The refused chain is never consumed, so the queue stays retired.
        assert!(matches!(queue.try_next(), Ok(None)));
    }

    for (address, length) in ADMITTED_BUFFERS {
        let test_mem = VirtioTestMemoryAccess::new();
        let mut guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, false);
        let (queue, refused) = new_restricted_queue(&driver, &guest, guest.queue_params(0));
        let mut queue = queue.unwrap();
        post_buffer(
            &test_mem,
            &mut guest,
            address,
            length,
            DescriptorFlags::new(),
        );

        let work = queue.try_next().unwrap().unwrap();
        assert_eq!(work.payload.len(), 1);
        assert_eq!(work.payload[0].address, address);
        assert_eq!(work.payload[0].length, length);
        assert!(refused.try_recv().is_err());
        queue.complete(work, 0);
    }
}

#[async_test]
async fn queue_refuses_indirect_tables_outside_ram(driver: DefaultDriver) {
    const TABLE: u64 = 0x9000;

    // A table in the ISA hole, and a table in RAM whose only buffer is the
    // shared interrupt-status word.
    for (table, refusal) in [
        (ISA_HOLE.start, (ISA_HOLE.start, 16)),
        (TABLE, (STATUS_PAGE, 4)),
    ] {
        let test_mem = VirtioTestMemoryAccess::new();
        let mut entry = Vec::new();
        entry.extend_from_slice(&STATUS_PAGE.to_le_bytes());
        entry.extend_from_slice(&4u32.to_le_bytes());
        entry.extend_from_slice(&u16::from(DescriptorFlags::new().with_write(true)).to_le_bytes());
        entry.extend_from_slice(&0u16.to_le_bytes());
        test_mem.modify_memory_map(TABLE, &entry, false);
        let mut guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, false);
        let (queue, refused) = new_restricted_queue(&driver, &guest, guest.queue_params(0));
        let mut queue = queue.unwrap();
        post_buffer(
            &test_mem,
            &mut guest,
            table,
            16,
            DescriptorFlags::new().with_indirect(true),
        );

        let Err(error) = queue.try_next() else {
            panic!("the indirect table at {table:#x} must be refused");
        };
        assert!(
            matches!(queue_error(&error), Some(QueueError::BufferNotAdmitted(_))),
            "{error:?}"
        );
        assert_eq!(refused.try_iter().collect::<Vec<_>>(), [refusal]);
    }
}

#[async_test]
async fn queue_refuses_rings_outside_ram(driver: DefaultDriver) {
    let test_mem = VirtioTestMemoryAccess::new();
    let guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, false);
    let params = guest.queue_params(0);
    for params in [
        QueueParams {
            desc_addr: STATUS_PAGE,
            ..params
        },
        QueueParams {
            avail_addr: ISA_HOLE.end - 8,
            ..params
        },
        QueueParams {
            used_addr: APERTURE.start - 8,
            ..params
        },
    ] {
        let (queue, refused) = new_restricted_queue(&driver, &guest, params);
        assert!(matches!(queue, Err(QueueError::Memory(_))));
        assert_eq!(refused.try_iter().count(), 1);
    }
}

/// What a [`ReportingDevice`] queue saw.
#[derive(Debug, PartialEq)]
enum Report {
    /// A chain, by the address of its first buffer.
    Work(u64),
    /// The error that retired the queue, and whether the device memory refused
    /// a buffer.
    Error { refused: bool },
}

/// A device that fills each writable buffer with [`WRITTEN`], completes the
/// chain, and reports it, and that reports the error that retires its queue.
#[derive(InspectMut)]
#[inspect(skip)]
struct ReportingDevice {
    driver: vmcore::vm_task::VmTaskDriver,
    reports: mesh::Sender<Report>,
    worker: Option<TaskControl<ReportingTask, ReportingQueue>>,
}

struct ReportingTask {
    reports: mesh::Sender<Report>,
}

struct ReportingQueue {
    queue: VirtioQueue,
    mem: GuestMemory,
}

impl VirtioDevice for ReportingDevice {
    fn traits(&self) -> DeviceTraits {
        DeviceTraits {
            device_id: VirtioDeviceType::CONSOLE,
            device_features: VirtioDeviceFeatures::new().with_bank(0, VIRTIO_F_RING_INDIRECT_DESC),
            max_queues: 1,
            device_register_length: 0,
            ..Default::default()
        }
    }

    async fn read_registers_u32(&mut self, _offset: u16) -> u32 {
        0
    }

    async fn write_registers_u32(&mut self, _offset: u16, _val: u32) {}

    async fn start_queue(
        &mut self,
        _idx: u16,
        resources: QueueResources,
        features: &VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        let queue_event = PolledWait::new(&self.driver, resources.event)?;
        let queue = VirtioQueue::new(
            *features,
            resources.params,
            resources.guest_memory.clone(),
            resources.notify,
            queue_event,
            initial_state,
        )?;
        let mut worker = TaskControl::new(ReportingTask {
            reports: self.reports.clone(),
        });
        worker.insert(
            self.driver.clone(),
            "virtio-reporting-queue",
            ReportingQueue {
                queue,
                mem: resources.guest_memory,
            },
        );
        worker.start();
        self.worker = Some(worker);
        Ok(())
    }

    async fn stop_queue(&mut self, _idx: u16) -> Option<QueueState> {
        let mut worker = self.worker.take()?;
        worker.stop().await;
        Some(worker.remove().queue.queue_state())
    }

    async fn reset(&mut self) {
        self.worker = None;
    }
}

impl AsyncRun<ReportingQueue> for ReportingTask {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut ReportingQueue,
    ) -> Result<(), Cancelled> {
        loop {
            let work = stop.until_stopped(state.queue.next()).await?;
            match work.expect("a virtio queue stream never ends") {
                Ok(work) => {
                    let length = work.get_payload_length(true);
                    work.write(&state.mem, &vec![WRITTEN; length as usize])
                        .unwrap();
                    self.reports.send(Report::Work(work.payload[0].address));
                    state.queue.complete(work, length as u32);
                }
                Err(error) => self.reports.send(Report::Error {
                    refused: matches!(queue_error(&error), Some(QueueError::BufferNotAdmitted(_))),
                }),
            }
        }
    }
}

/// Returns a device whose DMA is restricted to the microVM RAM ranges, with
/// its shared interrupt-status word at the start of the status page, and the
/// receiver of its reports.
fn new_restricted_mmio_device(
    driver: &DefaultDriver,
    test_mem: &Arc<VirtioTestMemoryAccess>,
) -> (VirtioMmioDevice, mesh::Receiver<Report>) {
    let (reports, reports_recv) = mesh::channel();
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
    let mut dev = VirtioMmioDevice::new_with_disabled_features_and_interrupt_mode(
        Box::new(ReportingDevice {
            driver: driver_source.simple(),
            reports,
            worker: None,
        }),
        &driver_source.simple(),
        GuestMemory::new("test", test_mem.clone()),
        LineInterrupt::detached(),
        None,
        0,
        0x1000,
        0,
        VirtioMmioInterruptMode::SharedStatus {
            status_gpa: STATUS_PAGE,
        },
    )
    .unwrap();
    dev.restrict_dma(microvm_ram()).unwrap();
    (dev, reports_recv)
}

/// Polls the device until its status register reads `status`.
async fn wait_for_status(dev: &mut VirtioMmioDevice, status: u32) {
    for _ in 0..1000 {
        if dev.read_u32(112) == status {
            return;
        }
        yield_and_poll_device(dev).await;
    }
    panic!(
        "device status {:#x} never became {status:#x}",
        dev.read_u32(112)
    );
}

/// Resets the device and checks that the reset clears its needing reset and
/// its shared interrupt-status word.
async fn reset_device(test_mem: &VirtioTestMemoryAccess, dev: &mut VirtioMmioDevice) {
    dev.write_u32(112, 0);
    wait_for_status(dev, 0).await;
    assert_eq!(test_mem.memory_map_get_u32(STATUS_PAGE), 0);
}

#[async_test]
async fn mmio_device_refuses_buffers_outside_ram_and_needs_reset(driver: DefaultDriver) {
    for (address, length) in REFUSED_BUFFERS {
        let test_mem = VirtioTestMemoryAccess::new();
        map_memory_around_reserved_ranges(&test_mem);
        let mut guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, false);
        let (mut dev, mut reports) = new_restricted_mmio_device(&driver, &test_mem);
        guest
            .setup_chipset_device(&mut dev, guest.queue_features())
            .await;
        let status = dev.read_u32(112);

        post_buffer(
            &test_mem,
            &mut guest,
            address,
            length,
            DescriptorFlags::new().with_write(true),
        );
        dev.write_u32(80, 0);
        assert_eq!(
            must_recv_in_timeout(&mut reports, Duration::from_secs(5)).await,
            Report::Error { refused: true }
        );

        // The device marks itself as needing reset and notifies the driver,
        // which is the only change to the status page. The device wrote none
        // of the buffer, not even the part in RAM.
        wait_for_status(&mut dev, status | VIRTIO_DEVICE_NEEDS_RESET).await;
        assert_eq!(
            test_mem.memory_map_get_u32(STATUS_PAGE),
            VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE
        );
        assert_untouched(&test_mem, STATUS_PAGE, PAGE as u32);
        assert_untouched(&test_mem, address, length);

        reset_device(&test_mem, &mut dev).await;
        assert!(reports.try_recv().is_err());
    }
}

#[async_test]
async fn mmio_device_writes_buffers_inside_ram(driver: DefaultDriver) {
    for (address, length) in ADMITTED_BUFFERS {
        let test_mem = VirtioTestMemoryAccess::new();
        map_memory_around_reserved_ranges(&test_mem);
        let mut guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, 4, false);
        let (mut dev, mut reports) = new_restricted_mmio_device(&driver, &test_mem);
        guest
            .setup_chipset_device(&mut dev, guest.queue_features())
            .await;
        let status = dev.read_u32(112);

        post_buffer(
            &test_mem,
            &mut guest,
            address,
            length,
            DescriptorFlags::new().with_write(true),
        );
        dev.write_u32(80, 0);
        assert_eq!(
            must_recv_in_timeout(&mut reports, Duration::from_secs(5)).await,
            Report::Work(address)
        );
        assert!(
            read_memory(&test_mem, address, length as usize)
                .iter()
                .all(|&byte| byte == WRITTEN)
        );
        yield_and_poll_device(&mut dev).await;
        assert_eq!(dev.read_u32(112), status);
        assert_eq!(
            test_mem.memory_map_get_u32(STATUS_PAGE) & VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE,
            0
        );

        reset_device(&test_mem, &mut dev).await;
    }
}

#[async_test]
async fn mmio_device_refuses_rings_outside_ram_and_needs_reset(driver: DefaultDriver) {
    const QUEUE_SIZE: u16 = 4;

    for (desc_addr, avail_addr, used_addr) in [
        (STATUS_PAGE, 0x2000, 0x3000),
        (0x1000, ISA_HOLE.start, 0x3000),
        (0x1000, 0x2000, APERTURE.start - 8),
    ] {
        let test_mem = VirtioTestMemoryAccess::new();
        map_memory_around_reserved_ranges(&test_mem);
        let guest = VirtioTestGuest::new_split(&driver, &test_mem, 1, QUEUE_SIZE, false);
        let (mut dev, mut reports) = new_restricted_mmio_device(&driver, &test_mem);
        let features = guest.queue_features();

        dev.write_u32(112, VIRTIO_ACKNOWLEDGE);
        dev.write_u32(112, VIRTIO_DRIVER);
        dev.write_u32(36, 0);
        dev.write_u32(32, features.bank(0));
        dev.write_u32(36, 1);
        dev.write_u32(32, features.bank(1));
        dev.write_u32(112, VIRTIO_FEATURES_OK);
        dev.write_u32(48, 0);
        dev.write_u32(56, QUEUE_SIZE.into());
        dev.write_u32(128, desc_addr as u32);
        dev.write_u32(132, (desc_addr >> 32) as u32);
        dev.write_u32(144, avail_addr as u32);
        dev.write_u32(148, (avail_addr >> 32) as u32);
        dev.write_u32(160, used_addr as u32);
        dev.write_u32(164, (used_addr >> 32) as u32);
        dev.write_u32(68, 1);
        dev.write_u32(112, VIRTIO_DRIVER_OK);

        // The queue does not start, so DRIVER_OK fails and the device keeps
        // only its needing reset. Without DRIVER_OK there is no notification.
        wait_for_status(&mut dev, VIRTIO_DEVICE_NEEDS_RESET).await;
        assert_eq!(test_mem.memory_map_get_u32(STATUS_PAGE), 0);
        assert_untouched(&test_mem, STATUS_PAGE, PAGE as u32);
        assert!(reports.try_recv().is_err());

        reset_device(&test_mem, &mut dev).await;
    }
}
