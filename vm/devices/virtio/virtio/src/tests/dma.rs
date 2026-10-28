// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for queues whose memory is restricted to the RAM ranges of the
//! microVM e820 map: a queue refuses a buffer, an indirect table, or a ring in
//! the shared interrupt-status page, in the ISA hole, or across the MMIO
//! aperture.

use super::*;
use std::ops::Range;
use test_with_tracing::test;

const PAGE: u64 = 0x1000;
/// The shared interrupt-status page of the microVM.
const STATUS_PAGE: u64 = 0x3_0000;
/// The legacy PC range from 640 KiB to 1 MiB.
const ISA_HOLE: Range<u64> = 0xa_0000..0x10_0000;
/// The fixed 3-to-4-GiB MMIO aperture.
const APERTURE: Range<u64> = 0xc000_0000..0x1_0000_0000;

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
