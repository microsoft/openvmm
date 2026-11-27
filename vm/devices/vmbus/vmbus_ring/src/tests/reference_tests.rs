// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use test_with_tracing::test;

// Test: A peer requests an empty-ring wakeup using aligned and legacy unaligned thresholds.
// Expected: Masked reads signal only after draining all packets, and a blocked send can retry.
#[test]
fn legacy_drain_threshold_wakes_a_blocked_writer_after_wraparound() {
    for threshold in [4088, 4089, 4095] {
        let memory = FlatRingMem::new(4096);
        let mut reader = IncomingRing::new(&memory).unwrap();
        let mut writer = OutgoingRing::new(&memory).unwrap();
        for sequence in 0..4u8 {
            let first = [sequence + 1; 1000];
            let second = [sequence + 0x41; 1000];
            assert!(!write_simple(&mut writer, &first).unwrap());
            assert!(!write_simple(&mut writer, &second).unwrap());
            let retry = [sequence + 0x81; 4056];
            assert!(write_simple(&mut writer, &retry).is_none());

            Control(memory.control())
                .pending_send_size()
                .store(threshold, Ordering::SeqCst);
            let (payload, signal) = read_simple(&mut reader);
            assert_eq!(payload, first);
            assert!(!signal);
            assert!(write_simple(&mut writer, &retry).is_none());
            let (payload, signal) = read_simple(&mut reader);
            assert_eq!(payload, second);
            assert!(signal, "threshold {threshold}");
            let mut incoming = reader.incoming().unwrap();
            assert!(!reader.can_read(&mut incoming).unwrap());
            assert!(!reader.commit_read(&mut incoming));

            assert!(!write_simple(&mut writer, &retry).unwrap());
            let (payload, signal) = read_simple(&mut reader);
            assert_eq!(payload, retry);
            assert!(signal);
        }
    }
}

// Test: Publish a complete descriptor and payload with and without its trailing offset word.
// Expected: Complete packets retain their payload, while a missing trailer is rejected before consumption.
#[test]
#[ignore = "Confirmed product bug: packets without a published trailer are accepted and advance past available data"]
fn known_bug_peer_packet_requires_the_trailer_before_it_can_be_consumed() {
    for start in [0, 4080] {
        for packet_len in [16, 24, 64] {
            for trailer_present in [true, false] {
                let memory = FlatRingMem::new(4096);
                let reader = IncomingRing::new(&memory).unwrap();
                let descriptor = PacketDescriptor {
                    packet_type: PACKET_TYPE_IN_BAND,
                    data_offset8: 2,
                    length8: packet_len / 8,
                    flags: 0,
                    transaction_id: 0,
                };
                memory.write_at(start, descriptor.as_bytes());
                let payload = vec![0x5a; packet_len as usize - 16];
                memory.write_at(start + 16, &payload);
                let published = packet_len as usize + usize::from(trailer_present) * 8;
                Control(memory.control())
                    .outp()
                    .store(start as u32, Ordering::SeqCst);
                Control(memory.control())
                    .inp()
                    .store(((start + published) % 4096) as u32, Ordering::SeqCst);
                let mut incoming = reader.incoming().unwrap();
                let received = reader.read(&mut incoming);
                if trailer_present {
                    let received = received.unwrap();
                    assert_eq!(
                        received.payload.reader(&reader).read_all().unwrap(),
                        payload
                    );
                    reader.commit_read(&mut incoming);
                    assert!(!reader.can_read(&mut incoming).unwrap());
                } else {
                    assert!(
                        received.is_err(),
                        "accepted packet without trailer at {start}"
                    );
                }
            }
        }
    }
}
