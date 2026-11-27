// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use test_with_tracing::test;

fn write_simple<T: RingMem>(out_ring: &mut OutgoingRing<T>, buf: &[u8]) -> Option<bool> {
    let mut outgoing = out_ring.outgoing().unwrap();
    match out_ring.write(
        &mut outgoing,
        &OutgoingPacket {
            typ: OutgoingPacketType::InBandNoCompletion,
            size: buf.len(),
            transaction_id: 0,
        },
    ) {
        Ok(range) => {
            range.writer(out_ring).write(buf).unwrap();
            Some(out_ring.commit_write(&mut outgoing))
        }
        Err(WriteError::Full(_)) => None,
        Err(err) => panic!("{}", err),
    }
}

fn read_simple<T: RingMem>(in_ring: &mut IncomingRing<T>) -> (Vec<u8>, bool) {
    let mut incoming = in_ring.incoming().unwrap();
    let msg = in_ring
        .read(&mut incoming)
        .unwrap()
        .payload
        .reader(in_ring)
        .read_all()
        .unwrap();
    let signal = in_ring.commit_read(&mut incoming);
    (msg, signal)
}

#[test]
fn test_ring() {
    let rmem = FlatRingMem::new(16384);
    let mut in_ring = IncomingRing::new(&rmem).unwrap();
    in_ring.set_interrupt_mask(false);
    let mut out_ring = OutgoingRing::new(&rmem).unwrap();

    let p = &[1, 2, 3, 4, 5, 6, 7, 8];
    assert!(write_simple(&mut out_ring, p).unwrap());

    let (msg, signal) = read_simple(&mut in_ring);
    assert!(!signal);

    assert_eq!(p, &msg[..]);
}

#[test]
fn test_interrupt_mask() {
    let rmem = FlatRingMem::new(16384);
    let mut in_ring = IncomingRing::new(&rmem).unwrap();
    let mut out_ring = OutgoingRing::new(&rmem).unwrap();

    // Interrupts are masked, so no signal is expected.
    assert!(!write_simple(&mut out_ring, &[1, 2, 3]).unwrap());
    assert!(!read_simple(&mut in_ring).1);

    // Unmask interrupts, then try again, expecting a signal this time.
    in_ring.set_interrupt_mask(false);
    assert!(write_simple(&mut out_ring, &[1, 2, 3]).unwrap());
    assert!(!read_simple(&mut in_ring).1);
}

#[test]
fn test_pending_send_size() {
    let rmem = FlatRingMem::new(16384);
    let mut in_ring = IncomingRing::new(&rmem).unwrap();
    let mut out_ring = OutgoingRing::new(&rmem).unwrap();

    // Fill the ring up with some packets.
    write_simple(&mut out_ring, &[1; 4000]).unwrap();
    write_simple(&mut out_ring, &[2; 4000]).unwrap();
    write_simple(&mut out_ring, &[3; 4000]).unwrap();
    write_simple(&mut out_ring, &[4; 4000]).unwrap();
    assert!(write_simple(&mut out_ring, &[5; 4000]).is_none());

    // No pending send size yet.
    assert!(!read_simple(&mut in_ring).1);

    // Fill the ring back up.
    write_simple(&mut out_ring, &[5; 4000]).unwrap();
    assert!(write_simple(&mut out_ring, &[6; 4000]).is_none());

    // Set a pending send size for two packets worth of space (packet size +
    // 16 bytes for the descriptor and 8 bytes for the footer).
    out_ring.set_pending_send_size(4024 * 2).unwrap();

    // There should be a signal after two packets, then no more signals.
    assert!(!read_simple(&mut in_ring).1);
    assert!(read_simple(&mut in_ring).1);
    assert!(!read_simple(&mut in_ring).1);
}

#[test]
fn test_malformed_gpa_direct_packet() {
    let rmem = FlatRingMem::new(16384);

    let desc = PacketDescriptor {
        packet_type: PACKET_TYPE_GPA_DIRECT,
        data_offset8: 2,
        length8: 3,
        flags: 0,
        transaction_id: 0,
    };

    rmem.write_aligned(0, desc.as_bytes());
    let header = GpaDirectHeader {
        reserved: 0,
        range_count: 1,
    };
    rmem.write_aligned(size_of::<PacketDescriptor>(), header.as_bytes());
    let err = parse_packet(&rmem, 0, 16384).unwrap_err();
    assert!(matches!(
        err,
        ReadError::Corrupt(Error::InvalidDescriptorLengths)
    ));
}

#[test]
fn test_malformed_transfer_page_packet() {
    let rmem = FlatRingMem::new(16384);

    let desc = PacketDescriptor {
        packet_type: PACKET_TYPE_TRANSFER_PAGES,
        data_offset8: 2,
        length8: 3,
        flags: 0,
        transaction_id: 0,
    };

    rmem.write_aligned(0, desc.as_bytes());
    let header = TransferPageHeader {
        range_count: 1,
        transfer_page_set_id: 1,
        reserved: 0,
    };
    rmem.write_aligned(size_of::<PacketDescriptor>(), header.as_bytes());
    let err = parse_packet(&rmem, 0, 16384).unwrap_err();
    assert!(matches!(
        err,
        ReadError::Corrupt(Error::InvalidDescriptorLengths)
    ));
}

// Test: Exchange each supported packet type while repeatedly wrapping the ring.
// Expected: Payloads, completion IDs, and external range descriptors survive intact.
#[test]
fn packet_variants_preserve_metadata_and_payload() {
    let memory = FlatRingMem::new(4096);
    let reader = IncomingRing::new(&memory).unwrap();
    let writer = OutgoingRing::new(&memory).unwrap();
    let gpns = [0x1234, 0x5678];
    let ranges = [PagedRange::new(4088, 24, &gpns).unwrap()];
    let transfer = [TransferPageRange {
        byte_count: 24,
        byte_offset: 4088,
    }];
    let variants = [
        OutgoingPacketType::InBandNoCompletion,
        OutgoingPacketType::InBandWithCompletion,
        OutgoingPacketType::Completion,
        OutgoingPacketType::GpaDirect(&ranges),
        OutgoingPacketType::TransferPages(0xabcd, &transfer),
    ];

    for sequence in 0..64u64 {
        for variant in &variants {
            let payload = [sequence, !sequence, 0x0123_4567_89ab_cdef];
            let mut outgoing = writer.outgoing().unwrap();
            let range = writer
                .write(
                    &mut outgoing,
                    &OutgoingPacket {
                        typ: *variant,
                        size: size_of_val(&payload),
                        transaction_id: sequence,
                    },
                )
                .unwrap();
            range.write_aligned_full(&writer, &payload);
            writer.commit_write(&mut outgoing);

            let mut incoming = reader.incoming().unwrap();
            let packet = reader.read(&mut incoming).unwrap();
            let expected_id = if matches!(variant, OutgoingPacketType::InBandNoCompletion) {
                None
            } else {
                Some(sequence)
            };
            assert_eq!(packet.transaction_id, expected_id);
            assert_eq!(
                packet.payload.reader(&reader).read_all().unwrap(),
                payload.as_bytes()
            );
            match (variant, packet.typ) {
                (
                    OutgoingPacketType::InBandNoCompletion
                    | OutgoingPacketType::InBandWithCompletion,
                    IncomingPacketType::InBand,
                )
                | (OutgoingPacketType::Completion, IncomingPacketType::Completion) => {}
                (
                    OutgoingPacketType::GpaDirect(_),
                    IncomingPacketType::GpaDirect(count, descriptor),
                ) => {
                    assert_eq!(count, 1);
                    let mut expected = Vec::from(((4088u64 << 32) | 24).to_le_bytes());
                    expected.extend_from_slice(gpns.as_bytes());
                    assert_eq!(descriptor.reader(&reader).read_all().unwrap(), expected);
                }
                (
                    OutgoingPacketType::TransferPages(_, _),
                    IncomingPacketType::TransferPages(id, count, descriptor),
                ) => {
                    assert_eq!((id, count), (0xabcd, 1));
                    assert_eq!(
                        descriptor.reader(&reader).read_all().unwrap(),
                        transfer.as_bytes()
                    );
                }
                other => panic!("wrong packet type: {other:?}"),
            }
            reader.commit_read(&mut incoming);
            assert!(!reader.can_read(&mut incoming).unwrap());
        }
    }
}

// Test: Stage writes and reads, then revert them before committing.
// Expected: Uncommitted writes are invisible and reverted reads remain available.
#[test]
fn speculative_operations_preserve_packet_visibility() {
    let memory = FlatRingMem::new(4096);
    let reader = IncomingRing::new(&memory).unwrap();
    let writer = OutgoingRing::new(&memory).unwrap();
    let packet = OutgoingPacket {
        typ: OutgoingPacketType::InBandNoCompletion,
        size: 8,
        transaction_id: 0,
    };
    let mut outgoing = writer.outgoing().unwrap();
    writer
        .write(&mut outgoing, &packet)
        .unwrap()
        .write_aligned_full(&writer, &[111]);
    assert!(!reader.can_read(&mut reader.incoming().unwrap()).unwrap());
    outgoing.revert();
    assert!(!writer.commit_write(&mut outgoing));
    writer
        .write(&mut outgoing, &packet)
        .unwrap()
        .write_aligned_full(&writer, &[222]);
    writer.commit_write(&mut outgoing);
    let mut incoming = reader.incoming().unwrap();
    for _attempt in 0..2 {
        let received = reader.read(&mut incoming).unwrap();
        assert_eq!(
            received.payload.reader(&reader).read_all().unwrap(),
            222u64.to_ne_bytes()
        );
        incoming.revert();
    }
    reader.read(&mut incoming).unwrap();
    reader.commit_read(&mut incoming);
    assert!(!reader.commit_read(&mut incoming));
    assert!(matches!(reader.read(&mut incoming), Err(ReadError::Empty)));
}

// Test: Overrun range readers/writers and then perform valid partial accesses.
// Expected: Bounds errors neither consume bytes nor damage neighboring payloads.
#[test]
fn payload_access_bounds_preserve_cursor_and_data() {
    let memory = FlatRingMem::new(4096);
    let reader = IncomingRing::new(&memory).unwrap();
    let writer = OutgoingRing::new(&memory).unwrap();
    let mut outgoing = writer.outgoing().unwrap();
    let range = writer
        .write(
            &mut outgoing,
            &OutgoingPacket {
                typ: OutgoingPacketType::InBandNoCompletion,
                size: 16,
                transaction_id: 0,
            },
        )
        .unwrap();
    assert_eq!(range.len(), 16);
    assert!(!range.is_empty());
    assert!(RingRange::empty().is_empty());
    let neighbor_data = [0xa5, 0x5a, 0xc3, 0x3c, 0xf0, 0x0f, 0x96, 0x69];
    let neighbor = writer
        .write(
            &mut outgoing,
            &OutgoingPacket {
                typ: OutgoingPacketType::InBandNoCompletion,
                size: neighbor_data.len(),
                transaction_id: 0,
            },
        )
        .unwrap();
    neighbor.writer(&writer).write(&neighbor_data).unwrap();
    let mut payload = range.writer(&writer);
    assert!(payload.write(&[0; 17]).is_err());
    // This overrun would reach the next packet's payload if copied before rejection.
    assert!(payload.write(&[0xcc; 64]).is_err());
    assert_eq!(neighbor.reader(&writer).read_all().unwrap(), neighbor_data);
    payload.write(&[0x12; 8]).unwrap();
    payload.write(&[0x34; 8]).unwrap();
    assert_eq!(payload.len(), 0);
    writer.commit_write(&mut outgoing);
    let mut incoming = reader.incoming().unwrap();
    let packet = reader.read(&mut incoming).unwrap();
    let mut payload = packet.payload.reader(&reader);
    assert!(payload.read(&mut [0; 17]).is_err());
    assert!(payload.skip(17).is_err());
    let mut first = [0; 8];
    payload.read(&mut first).unwrap();
    assert_eq!(first, [0x12; 8]);
    payload.skip(4).unwrap();
    assert_eq!(payload.read_all().unwrap(), [0x34; 4]);
    reader.commit_read(&mut incoming);
    let neighbor = reader.read(&mut incoming).unwrap();
    assert_eq!(
        neighbor.payload.reader(&reader).read_all().unwrap(),
        neighbor_data
    );
    reader.commit_read(&mut incoming);
    assert!(!reader.can_read(&mut incoming).unwrap());
}

// Test: Present malformed peer packet headers through the incoming ring API.
// Expected: Invalid lengths, flags, types, and empty GPA lists return typed errors.
#[test]
fn malformed_peer_packets_are_rejected() {
    let cases = [
        (6, 1, 2, 0, Error::InvalidDescriptorLengths),
        (6, 4, 3, 0, Error::InvalidDescriptorLengths),
        (6, 2, 20, 0, Error::InvalidDescriptorLengths),
        (6, 2, 3, 2, Error::InvalidDescriptorFlags),
        (0xffff, 2, 3, 0, Error::InvalidDescriptorType),
        (9, 3, 3, 0, Error::InvalidDescriptorGpaDirectRangeCount),
    ];
    for (packet_type, data_offset8, length8, flags, expected) in cases {
        let memory = FlatRingMem::new(4096);
        let reader = IncomingRing::new(&memory).unwrap();
        memory.write_at(
            0,
            PacketDescriptor {
                packet_type,
                data_offset8,
                length8,
                flags,
                transaction_id: 0,
            }
            .as_bytes(),
        );
        Control(memory.control()).inp().store(32, Ordering::Relaxed);
        let mut incoming = reader.incoming().unwrap();
        let error = reader.read(&mut incoming).unwrap_err();
        assert_eq!(error.to_string(), expected.to_string());
        assert!(matches!(error, ReadError::Corrupt(_)));
    }
}

// Test: Supply invalid ring sizes, peer pointers, and oversized writes.
// Expected: Invalid input is rejected without making a valid ring unusable.
#[test]
fn invalid_ring_inputs_do_not_publish_packets() {
    for size in [8, 4095, 4097] {
        assert!(matches!(
            IncomingRing::new(FlatRingMem::new(size)),
            Err(Error::InvalidRingMemory)
        ));
    }
    for pointer in [1, 4096, u32::MAX] {
        let memory = FlatRingMem::new(4096);
        Control(memory.control())
            .inp()
            .store(pointer, Ordering::Relaxed);
        assert!(matches!(
            IncomingRing::new(&memory).unwrap().incoming(),
            Err(Error::InvalidRingPointer)
        ));
        assert!(matches!(
            OutgoingRing::new(&memory).unwrap().outgoing(),
            Err(Error::InvalidRingPointer)
        ));
    }
    let memory = FlatRingMem::new(4096);
    let reader = IncomingRing::new(&memory).unwrap();
    let writer = OutgoingRing::new(&memory).unwrap();
    let mut outgoing = writer.outgoing().unwrap();
    assert_eq!(writer.maximum_packet_size(), 4088);
    assert!(reader.supports_pending_send_size());
    assert!(matches!(
        reader.verify_interrupts_unmasked(),
        Err(Error::InterruptsExternallyMasked)
    ));
    reader.set_interrupt_mask(false);
    reader.verify_interrupts_unmasked().unwrap();
    assert!(matches!(
        writer.set_pending_send_size(4096),
        Err(Error::InvalidMessageLength)
    ));
    assert!(matches!(
        writer.write(
            &mut outgoing,
            &OutgoingPacket {
                typ: OutgoingPacketType::InBandNoCompletion,
                size: 4096,
                transaction_id: 0
            }
        ),
        Err(WriteError::Corrupt(Error::InvalidMessageLength))
    ));
    assert!(!writer.commit_write(&mut outgoing));
    assert!(!reader.can_read(&mut reader.incoming().unwrap()).unwrap());

    let expected = [0xa5, 0x5a, 0, 0xff, 0x12, 0x34, 0x56, 0x78];
    let payload = writer
        .write(
            &mut outgoing,
            &OutgoingPacket {
                typ: OutgoingPacketType::InBandWithCompletion,
                size: expected.len(),
                transaction_id: 0x1234,
            },
        )
        .unwrap();
    payload.writer(&writer).write(&expected).unwrap();
    assert!(writer.commit_write(&mut outgoing));
    let mut incoming = reader.incoming().unwrap();
    let packet = reader.read(&mut incoming).unwrap();
    assert!(matches!(packet.typ, IncomingPacketType::InBand));
    assert_eq!(packet.transaction_id, Some(0x1234));
    assert_eq!(packet.payload.reader(&reader).read_all().unwrap(), expected);
    reader.commit_read(&mut incoming);
    assert!(!reader.can_read(&mut incoming).unwrap());
}

#[repr(C, align(4096))]
struct TestPages([[AtomicU8; PAGE_SIZE]; 3]);

impl TestPages {
    fn new() -> Self {
        Self(core::array::from_fn(|_| {
            core::array::from_fn(|_| AtomicU8::new(0))
        }))
    }
}

impl AsRef<[AtomicU8]> for TestPages {
    fn as_ref(&self) -> &[AtomicU8] {
        self.0.as_flattened()
    }
}

impl PagedMemory for TestPages {
    fn control(&self) -> &[AtomicU8; PAGE_SIZE] {
        &self.0[0]
    }

    fn data_page_count(&self) -> usize {
        2
    }

    fn data(&self, page: usize) -> &[AtomicU8; PAGE_SIZE] {
        &self.0[1 + page % 2]
    }
}

// Logical data pages occupy slots 1 and 3, with an unmapped sentinel page between them.
#[repr(C, align(4096))]
struct GappedTestPages([[AtomicU8; PAGE_SIZE]; 4]);

impl GappedTestPages {
    fn new() -> Self {
        Self(core::array::from_fn(|page| {
            core::array::from_fn(|_| AtomicU8::new(if page == 2 { 0xa5 } else { 0 }))
        }))
    }
}

impl PagedMemory for &GappedTestPages {
    fn control(&self) -> &[AtomicU8; PAGE_SIZE] {
        &self.0[0]
    }

    fn data_page_count(&self) -> usize {
        2
    }

    fn data(&self, page: usize) -> &[AtomicU8; PAGE_SIZE] {
        &self.0[1 + (page % 2) * 2]
    }
}

fn check_memory_boundaries(memory: impl RingMem + Sync) {
    let len = memory.len();
    let mut expected = vec![0; len];
    for (sequence, address) in [0, PAGE_SIZE - 3, len - 5, len, len + 11]
        .into_iter()
        .enumerate()
    {
        let payload = (0..32)
            .map(|offset| (sequence * 32 + offset) as u8)
            .collect::<Vec<_>>();
        memory.write_at(address, &payload);
        for (offset, value) in payload.iter().enumerate() {
            expected[(address + offset) % len] = *value;
        }
        let mut received = vec![0; len];
        memory.read_at(0, &mut received);
        assert_eq!(received, expected);
        let mut received = [0; 32];
        memory.read_at(address, &mut received);
        assert_eq!(received.as_slice(), payload);
    }
    for (sequence, address) in [PAGE_SIZE - 8, len - 8, len + 8].into_iter().enumerate() {
        let payload: [u8; 32] =
            core::array::from_fn(|offset| (0x80 + sequence * 32 + offset) as u8);
        memory.write_aligned(address, &payload);
        for (offset, value) in payload.iter().enumerate() {
            expected[(address + offset) % len] = *value;
        }
        let mut received = vec![0; len];
        memory.read_at(0, &mut received);
        assert_eq!(received, expected);
        let mut received = [0; 32];
        memory.read_aligned(address, &mut received);
        assert_eq!(received, payload);
    }

    let mut reader = IncomingRing::new(&memory).unwrap();
    let mut writer = OutgoingRing::new(&memory).unwrap();
    for sequence in 0..8u8 {
        let payload = vec![sequence; 4000];
        write_simple(&mut writer, &payload).unwrap();
        assert_eq!(read_simple(&mut reader).0, payload);
    }
}

// Test: Copy bytes and exchange packets through a single-mapped ring at boundaries.
// Expected: Wrapped and aligned accesses preserve bytes without corrupting neighbors.
#[test]
fn single_mapped_memory_preserves_wrapped_data() {
    check_memory_boundaries(SingleMappedRingMem(TestPages::new()));
}

// Test: Copy bytes and exchange packets across discontiguous pages and ring wrap.
// Expected: Ordinary and atomic aligned accesses preserve exact bytes and leave the unmapped gap intact.
#[test]
fn paged_memory_preserves_wrapped_data() {
    let pages = GappedTestPages::new();
    check_memory_boundaries(PagedRingMem::new(&pages));
    assert!(
        pages.0[2]
            .iter()
            .all(|byte| byte.load(Ordering::Relaxed) == 0xa5)
    );
}

// Test: Use advertised packet sizes to fill a ring with unaligned payload lengths.
// Expected: The capacity estimate agrees with actual writes and completion sizes.
#[test]
fn packet_sizes_match_admission_limits() {
    for payload_len in [0, 1, 7, 8, 9, 127] {
        assert_eq!(
            PacketSize::in_band(payload_len),
            24 + payload_len.div_ceil(8) * 8
        );
        assert_eq!(
            PacketSize::completion(payload_len),
            PacketSize::in_band(payload_len)
        );
        assert_eq!(
            PacketSize::transfer_pages(3, payload_len),
            PacketSize::in_band(payload_len) + 32
        );
        let memory = FlatRingMem::new(4096);
        let writer = OutgoingRing::new(&memory).unwrap();
        let mut outgoing = writer.outgoing().unwrap();
        let packet = OutgoingPacket {
            typ: OutgoingPacketType::InBandNoCompletion,
            size: payload_len,
            transaction_id: 0,
        };
        let packet_size = PacketSize::in_band(payload_len);
        let count = writer.maximum_packet_size() / packet_size;
        for _packet in 0..count {
            assert!(writer.can_write(&mut outgoing, packet_size).unwrap());
            writer.write(&mut outgoing, &packet).unwrap();
        }
        assert!(!writer.can_write(&mut outgoing, packet_size).unwrap());
        assert!(
            matches!(writer.write(&mut outgoing, &packet), Err(WriteError::Full(required)) if required == packet_size)
        );
    }
}

mod reference_tests;
