// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

extern crate alloc;

use super::*;
use alloc::collections::VecDeque;
use inv_packet::{OpenTMKAckPacket, OpenTMKErrorPacket, OpenTMKPacket};

#[derive(Default)]
pub(crate) struct MockSerialIo {
    reads: VecDeque<u8>,
    writes: Vec<u8>,
    init_calls: usize,
    drain_calls: usize,
}

impl MockSerialIo {
    pub(crate) fn with_reads(reads: Vec<u8>) -> Self {
        Self {
            reads: reads.into(),
            writes: Vec::new(),
            init_calls: 0,
            drain_calls: 0,
        }
    }

    pub(crate) fn written_bytes(&self) -> &[u8] {
        &self.writes
    }

    pub(crate) fn init_calls(&self) -> usize {
        self.init_calls
    }

    pub(crate) fn drain_calls(&self) -> usize {
        self.drain_calls
    }
}

impl SerialIo for MockSerialIo {
    fn init(&mut self) {
        self.init_calls += 1;
    }

    fn drain(&mut self) {
        self.drain_calls += 1;
    }

    fn write_byte(&mut self, byte: u8) {
        self.writes.push(byte);
    }

    fn read_byte(&mut self) -> u8 {
        self.reads
            .pop_front()
            .expect("mock serial transport was read past the queued bytes")
    }
}

fn append_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn frame_packet(packet: &OpenTMKPacket) -> Vec<u8> {
    let payload = postcard::to_allocvec(packet).expect("packet serialization should succeed");
    let mut bytes = Vec::new();
    append_u64(&mut bytes, COMMS_PACKET_HEADER_MAGIC);
    append_u64(&mut bytes, payload.len() as u64);
    bytes.extend_from_slice(&payload);
    append_u64(&mut bytes, COMMS_PACKET_FOOTER_MAGIC);
    bytes
}

#[test]
fn handshake_success() {
    // Case: a valid SYN/SYN-ACK/ACK exchange marks the transport connected and emits only SYN-ACK.
    let mut reads = Vec::new();
    append_u64(&mut reads, COMMS_SYN_MAGIC);
    append_u64(&mut reads, COMMS_ACK_MAGIC);

    let mut comms = SerialCommsServer::new_with_transport(MockSerialIo::with_reads(reads));
    comms.handshake().expect("handshake should succeed");

    assert!(comms.connected);
    assert_eq!(
        comms.handle.written_bytes(),
        &COMMS_SYN_ACK_MAGIC.to_le_bytes()
    );
    assert_eq!(comms.handle.init_calls(), 1);
    assert_eq!(comms.handle.drain_calls(), 1);
}

#[test]
fn handshake_bad_syn() {
    // Case: an unexpected first qword is rejected before any SYN-ACK bytes are written.
    let mut reads = Vec::new();
    append_u64(&mut reads, 0xdead_beef_dead_beef);

    let mut comms = SerialCommsServer::new_with_transport(MockSerialIo::with_reads(reads));
    let err = comms.handshake().expect_err("handshake should fail");

    assert_eq!(err, ExecutorError::HandshakeInvalidSynMagic);
    assert!(!comms.connected);
    assert!(comms.handle.written_bytes().is_empty());
    assert_eq!(comms.handle.init_calls(), 1);
    assert_eq!(comms.handle.drain_calls(), 1);
}

#[test]
fn valid_packet_deserialize() {
    // Case: a correctly framed packet is deserialized into the original packet payload.
    let expected = OpenTMKPacket::Error(OpenTMKErrorPacket {
        message: String::from("boom"),
        fatal: false,
    });

    let mut comms =
        SerialCommsServer::new_with_transport(MockSerialIo::with_reads(frame_packet(&expected)));

    let packet = comms
        .read_packet_blocking()
        .expect("packet read should succeed");
    assert_eq!(packet, expected);
}

#[test]
fn invalid_footer() {
    // Case: a packet with a corrupted footer is rejected after the payload bytes are read.
    let packet = OpenTMKPacket::Ack(OpenTMKAckPacket { code: 7 });
    let payload = postcard::to_allocvec(&packet).expect("packet serialization should succeed");

    let mut bytes = Vec::new();
    append_u64(&mut bytes, COMMS_PACKET_HEADER_MAGIC);
    append_u64(&mut bytes, payload.len() as u64);
    bytes.extend_from_slice(&payload);
    append_u64(&mut bytes, 0x0102_0304_0506_0708);

    let mut comms = SerialCommsServer::new_with_transport(MockSerialIo::with_reads(bytes));
    let err = comms
        .read_packet_blocking()
        .expect_err("packet read should fail");

    assert_eq!(err, ExecutorError::PacketInvalidFooterMagic);
}

#[test]
fn packet_write_framing() {
    // Case: writing a packet emits header, length, serialized payload, and footer in order.
    let packet = OpenTMKPacket::Ack(OpenTMKAckPacket { code: 99 });
    let mut comms = SerialCommsServer::new_with_transport(MockSerialIo::default());

    comms
        .write_packet_blocking(&packet)
        .expect("packet write should succeed");

    assert_eq!(comms.handle.written_bytes(), frame_packet(&packet));
}
