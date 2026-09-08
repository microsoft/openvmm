// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use crate::protocol::*;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use test_with_tracing::test;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateBlob;

const APPLICATION_REQUEST: u8 = NETFN_APPLICATION << 2;
const STORAGE_REQUEST: u8 = NETFN_STORAGE << 2;

#[derive(Clone)]
struct FakeClock(Arc<AtomicI64>);

impl FakeClock {
    fn new(seconds: i64) -> Self {
        Self(Arc::new(AtomicI64::new(seconds)))
    }

    fn set(&self, seconds: i64) {
        self.0.store(seconds, Ordering::Relaxed);
    }
}

impl TrustedClock for FakeClock {
    fn unix_seconds(&mut self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
struct SinkState {
    records: Vec<(u16, [u8; 16])>,
}

struct SharedSink {
    state: Arc<Mutex<SinkState>>,
    accept: Arc<AtomicBool>,
}

impl SelEventSink for SharedSink {
    fn try_send(&mut self, record_id: u16, record: [u8; 16]) -> SelEventDisposition {
        if !self.accept.load(Ordering::Relaxed) {
            return SelEventDisposition::Dropped;
        }
        self.state.lock().records.push((record_id, record));
        SelEventDisposition::Accepted
    }
}

fn device(seconds: i64) -> (FakeClock, IpmiKcs) {
    let clock = FakeClock::new(seconds);
    (clock.clone(), IpmiKcs::new(clock))
}

fn storage_request(command: u8, data: &[u8]) -> Vec<u8> {
    let mut request = vec![STORAGE_REQUEST, command];
    request.extend_from_slice(data);
    request
}

fn submit_request(device: &mut IpmiKcs, request: &[u8]) {
    assert!(!request.is_empty());
    device.write_command(KCS_COMMAND_WRITE_START);
    assert_eq!(device.read_status() & STATUS_STATE_MASK, KCS_STATE_WRITE);
    assert_eq!(device.read_data(), 0);

    for byte in &request[..request.len() - 1] {
        device.write_data(*byte);
        assert_eq!(device.read_status() & STATUS_STATE_MASK, KCS_STATE_WRITE);
        assert_eq!(device.read_data(), 0);
    }

    device.write_command(KCS_COMMAND_WRITE_END);
    assert_eq!(device.read_data(), 0);
    device.write_data(request[request.len() - 1]);
}

fn read_response(device: &mut IpmiKcs) -> Vec<u8> {
    let mut response = Vec::new();
    loop {
        assert_eq!(device.read_status() & STATUS_STATE_MASK, KCS_STATE_READ);
        assert_ne!(device.read_status() & STATUS_OBF, 0);
        response.push(device.read_data());
        assert_eq!(device.read_status() & STATUS_OBF, 0);

        device.write_data(KCS_DATA_READ_NEXT);
        if device.read_status() & STATUS_STATE_MASK == KCS_STATE_IDLE {
            assert_ne!(device.read_status() & STATUS_OBF, 0);
            assert_eq!(device.read_data(), 0);
            break;
        }
    }
    response
}

fn transact(device: &mut IpmiKcs, request: &[u8]) -> Vec<u8> {
    submit_request(device, request);
    read_response(device)
}

fn assert_completion(response: &[u8], request_netfn_lun: u8, command: u8, completion: u8) {
    assert_eq!(
        response.get(..3),
        Some([request_netfn_lun | 0x04, command, completion].as_slice())
    );
}

fn add_record(device: &mut IpmiKcs, fill: u8) -> Vec<u8> {
    transact(device, &storage_request(COMMAND_ADD_SEL_ENTRY, &[fill; 16]))
}

fn clear(device: &mut IpmiKcs, reservation: u16, action: u8) -> Vec<u8> {
    let mut data = Vec::from(reservation.to_le_bytes());
    data.extend_from_slice(b"CLR");
    data.push(action);
    transact(device, &storage_request(COMMAND_CLEAR_SEL, &data))
}

#[test]
fn kcs_transitions_and_get_device_id() {
    let (_, mut device) = device(100);
    assert_eq!(device.read_status(), KCS_STATE_IDLE);

    device.write_command(KCS_COMMAND_WRITE_START);
    let status = device.read_status();
    assert_eq!(status & STATUS_STATE_MASK, KCS_STATE_WRITE);
    assert_ne!(status & STATUS_OBF, 0);
    assert_ne!(status & STATUS_CD, 0);
    assert_eq!(status & STATUS_IBF, 0);
    assert_eq!(device.read_data(), 0);

    device.write_data(APPLICATION_REQUEST);
    let status = device.read_status();
    assert_eq!(status & STATUS_STATE_MASK, KCS_STATE_WRITE);
    assert_ne!(status & STATUS_OBF, 0);
    assert_eq!(status & STATUS_CD, 0);
    assert_eq!(device.read_data(), 0);

    device.write_command(KCS_COMMAND_WRITE_END);
    assert_eq!(device.read_status() & STATUS_STATE_MASK, KCS_STATE_WRITE);
    assert_ne!(device.read_status() & STATUS_CD, 0);
    assert_eq!(device.read_data(), 0);

    device.write_data(COMMAND_GET_DEVICE_ID);
    assert_eq!(device.read_status() & STATUS_STATE_MASK, KCS_STATE_READ);
    assert_eq!(device.read_status() & STATUS_CD, 0);
    let response = read_response(&mut device);
    assert_eq!(
        response,
        [
            APPLICATION_REQUEST | 0x04,
            COMMAND_GET_DEVICE_ID,
            COMPLETION_SUCCESS,
            0x20,
            0x01,
            0x02,
            0x00,
            0x02,
            0x04,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
        ]
    );
}

#[test]
fn abort_exposes_status_byte_then_returns_idle() {
    let (_, mut device) = device(0);
    device.write_command(KCS_COMMAND_WRITE_START);
    assert_eq!(device.read_data(), 0);
    device.write_data(APPLICATION_REQUEST);
    assert_eq!(device.read_data(), 0);

    device.write_command(KCS_COMMAND_GET_STATUS_ABORT);
    assert_eq!(device.read_status() & STATUS_STATE_MASK, KCS_STATE_READ);
    assert_ne!(device.read_status() & STATUS_CD, 0);
    assert_eq!(device.read_data(), 0);
    assert_eq!(device.read_status() & STATUS_OBF, 0);

    device.write_data(KCS_DATA_READ_NEXT);
    assert_eq!(device.read_status() & STATUS_STATE_MASK, KCS_STATE_READ);
    assert_eq!(device.read_data(), 0xff);

    device.write_data(KCS_DATA_READ_NEXT);
    assert_eq!(device.read_status() & STATUS_STATE_MASK, KCS_STATE_IDLE);
    assert_eq!(device.read_status() & STATUS_CD, 0);
    assert_eq!(device.read_data(), 0);
}

#[test]
fn malformed_kcs_sequences_are_handled_without_panicking() {
    let (_, mut device) = device(0);

    device.write_data(0);
    assert_eq!(device.read_status() & STATUS_STATE_MASK, KCS_STATE_IDLE);

    device.reset();
    device.write_command(KCS_DATA_READ_NEXT);
    assert_eq!(device.read_status() & STATUS_STATE_MASK, KCS_STATE_IDLE);

    device.reset();
    device.write_command(0xff);
    assert_eq!(device.read_status() & STATUS_STATE_MASK, KCS_STATE_READ);
    assert_eq!(device.read_data(), 0);
    device.write_data(KCS_DATA_READ_NEXT);
    assert_eq!(device.read_data(), 0xff);

    device.reset();
    device.write_command(KCS_COMMAND_WRITE_END);
    assert_eq!(device.read_status() & STATUS_STATE_MASK, KCS_STATE_WRITE);
    assert_eq!(device.read_data(), 0);

    device.reset();
    submit_request(&mut device, &[APPLICATION_REQUEST]);
    assert_eq!(device.read_status() & STATUS_STATE_MASK, KCS_STATE_ERROR);

    device.reset();
    submit_request(&mut device, &[APPLICATION_REQUEST, COMMAND_GET_DEVICE_ID]);
    assert_eq!(device.read_data(), APPLICATION_REQUEST | 0x04);
    device.write_data(0x69);
    assert_eq!(device.read_status() & STATUS_STATE_MASK, KCS_STATE_ERROR);
    assert_eq!(device.read_data(), 0xff);
}

#[test]
fn request_buffer_boundaries_are_safe() {
    let (_, mut device) = device(0);
    for length in [63, 64] {
        let response = transact(&mut device, &vec![0; length]);
        assert_completion(&response, 0, 0, COMPLETION_INVALID_COMMAND);
    }

    device.write_command(KCS_COMMAND_WRITE_START);
    assert_eq!(device.read_data(), 0);
    for _ in 0..64 {
        device.write_data(0);
        assert_eq!(device.read_status() & STATUS_STATE_MASK, KCS_STATE_WRITE);
        assert_eq!(device.read_data(), 0);
    }
    device.write_command(KCS_COMMAND_WRITE_END);
    assert_eq!(device.read_data(), 0);
    device.write_data(0);
    assert_eq!(device.read_status() & STATUS_STATE_MASK, KCS_STATE_READ);
    let response = read_response(&mut device);
    assert_completion(&response, 0, 0, COMPLETION_INVALID_COMMAND);
}

#[test]
fn command_lengths_and_unknown_commands_return_completion_codes() {
    let (_, mut device) = device(0);

    let response = transact(&mut device, &[APPLICATION_REQUEST, 0xfe]);
    assert_completion(
        &response,
        APPLICATION_REQUEST,
        0xfe,
        COMPLETION_INVALID_COMMAND,
    );
    let response = transact(&mut device, &storage_request(0xfe, &[]));
    assert_completion(&response, STORAGE_REQUEST, 0xfe, COMPLETION_INVALID_COMMAND);
    let response = transact(
        &mut device,
        &[APPLICATION_REQUEST, COMMAND_GET_DEVICE_ID, 0],
    );
    assert_completion(
        &response,
        APPLICATION_REQUEST,
        COMMAND_GET_DEVICE_ID,
        COMPLETION_SUCCESS,
    );

    for command in [
        COMMAND_GET_SEL_INFO,
        COMMAND_RESERVE_SEL,
        COMMAND_GET_SEL_TIME,
    ] {
        let response = transact(&mut device, &storage_request(command, &[0]));
        assert_completion(&response, STORAGE_REQUEST, command, COMPLETION_SUCCESS);
    }

    for (command, valid_length) in [
        (COMMAND_GET_SEL_ENTRY, 6),
        (COMMAND_ADD_SEL_ENTRY, 16),
        (COMMAND_CLEAR_SEL, 6),
        (COMMAND_SET_SEL_TIME, 4),
    ] {
        let response = transact(
            &mut device,
            &storage_request(command, &vec![0; valid_length - 1]),
        );
        assert_completion(
            &response,
            STORAGE_REQUEST,
            command,
            COMPLETION_INVALID_REQUEST_LENGTH,
        );
    }

    let response = transact(
        &mut device,
        &storage_request(COMMAND_GET_SEL_ENTRY, &[0, 0, 0, 0, 0, 0, 0xff]),
    );
    assert_completion(
        &response,
        STORAGE_REQUEST,
        COMMAND_GET_SEL_ENTRY,
        COMPLETION_RECORD_NOT_PRESENT,
    );

    for (command, data) in [
        (COMMAND_ADD_SEL_ENTRY, vec![0; 17]),
        (COMMAND_CLEAR_SEL, vec![0, 0, b'C', b'L', b'R', 0, 0xff]),
        (COMMAND_SET_SEL_TIME, vec![0; 5]),
    ] {
        let response = transact(&mut device, &storage_request(command, &data));
        assert_completion(&response, STORAGE_REQUEST, command, COMPLETION_SUCCESS);
    }
}

#[test]
fn sel_info_add_and_record_preservation() {
    let (_, mut device) = device(0x0102_0304);

    let info = transact(&mut device, &storage_request(COMMAND_GET_SEL_INFO, &[]));
    assert_eq!(
        info,
        [
            STORAGE_REQUEST | 0x04,
            COMMAND_GET_SEL_INFO,
            COMPLETION_SUCCESS,
            0x51,
            0,
            0,
            0,
            8,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0x02,
        ]
    );

    let mut record = [0xa5; 16];
    record[2] = 0x02;
    record[7] = 0x77;
    let response = transact(
        &mut device,
        &storage_request(COMMAND_ADD_SEL_ENTRY, &record),
    );
    assert_eq!(
        response,
        [
            STORAGE_REQUEST | 0x04,
            COMMAND_ADD_SEL_ENTRY,
            COMPLETION_SUCCESS,
            1,
            0,
        ]
    );

    let stored = device.sel_record(0).unwrap();
    assert_eq!(&stored[0..2], &1u16.to_le_bytes());
    assert_eq!(stored[2], 0x02);
    assert_eq!(&stored[3..7], &0x0102_0304u32.to_le_bytes());
    assert_eq!(stored[7], 0x77);
    assert_eq!(&stored[8..], &record[8..]);

    let info = transact(&mut device, &storage_request(COMMAND_GET_SEL_INFO, &[]));
    assert_eq!(u16::from_le_bytes([info[4], info[5]]), 1);
    assert_eq!(u16::from_le_bytes([info[6], info[7]]), 2032);
    assert_eq!(
        u32::from_le_bytes([info[8], info[9], info[10], info[11]]),
        0x0102_0304
    );
}

#[test]
fn sel_entry_ids_sentinels_partial_reads_and_reservations() {
    let (_, mut device) = device(10);
    let empty = transact(
        &mut device,
        &storage_request(COMMAND_GET_SEL_ENTRY, &[0, 0, 0, 0, 0, 16]),
    );
    assert_completion(
        &empty,
        STORAGE_REQUEST,
        COMMAND_GET_SEL_ENTRY,
        COMPLETION_RECORD_NOT_PRESENT,
    );

    assert_eq!(add_record(&mut device, 0x11)[3..5], [1, 0]);
    assert_eq!(add_record(&mut device, 0x22)[3..5], [2, 0]);

    let reserve_one = transact(&mut device, &storage_request(COMMAND_RESERVE_SEL, &[]));
    let reservation_one = u16::from_le_bytes([reserve_one[3], reserve_one[4]]);
    let reserve_two = transact(&mut device, &storage_request(COMMAND_RESERVE_SEL, &[]));
    let reservation_two = u16::from_le_bytes([reserve_two[3], reserve_two[4]]);
    assert_eq!(reservation_one, 1);
    assert_eq!(reservation_two, 2);

    let mut old_reservation = Vec::from(reservation_one.to_le_bytes());
    old_reservation.extend_from_slice(&[0, 0, 0, 16]);
    let response = transact(
        &mut device,
        &storage_request(COMMAND_GET_SEL_ENTRY, &old_reservation),
    );
    assert_completion(
        &response,
        STORAGE_REQUEST,
        COMMAND_GET_SEL_ENTRY,
        COMPLETION_SUCCESS,
    );

    let mut first_partial = Vec::from(0u16.to_le_bytes());
    first_partial.extend_from_slice(&0u16.to_le_bytes());
    first_partial.extend_from_slice(&[7, 4]);
    let response = transact(
        &mut device,
        &storage_request(COMMAND_GET_SEL_ENTRY, &first_partial),
    );
    assert_completion(
        &response,
        STORAGE_REQUEST,
        COMMAND_GET_SEL_ENTRY,
        COMPLETION_SUCCESS,
    );
    assert_eq!(&response[3..5], &2u16.to_le_bytes());
    assert_eq!(&response[5..], &[0x11; 4]);

    let mut last = Vec::from(reservation_two.to_le_bytes());
    last.extend_from_slice(&0xffffu16.to_le_bytes());
    last.extend_from_slice(&[0, 0xff]);
    let response = transact(&mut device, &storage_request(COMMAND_GET_SEL_ENTRY, &last));
    assert_eq!(&response[3..5], &0xffffu16.to_le_bytes());
    assert_eq!(response.len(), 21);
    assert_eq!(&response[5..7], &2u16.to_le_bytes());

    let mut missing = Vec::from(0u16.to_le_bytes());
    missing.extend_from_slice(&999u16.to_le_bytes());
    missing.extend_from_slice(&[0, 16]);
    let response = transact(
        &mut device,
        &storage_request(COMMAND_GET_SEL_ENTRY, &missing),
    );
    assert_completion(
        &response,
        STORAGE_REQUEST,
        COMMAND_GET_SEL_ENTRY,
        COMPLETION_RECORD_NOT_PRESENT,
    );

    let response = transact(
        &mut device,
        &storage_request(COMMAND_GET_SEL_ENTRY, &[0, 0, 0, 0, 16, 1]),
    );
    assert_completion(
        &response,
        STORAGE_REQUEST,
        COMMAND_GET_SEL_ENTRY,
        COMPLETION_PARAMETER_OUT_OF_RANGE,
    );
}

#[test]
fn sel_capacity_is_bounded_without_overwrite() {
    let (_, mut device) = device(1);
    for expected_id in 1..=128u16 {
        let response = add_record(&mut device, expected_id as u8);
        assert_completion(
            &response,
            STORAGE_REQUEST,
            COMMAND_ADD_SEL_ENTRY,
            COMPLETION_SUCCESS,
        );
        assert_eq!(u16::from_le_bytes([response[3], response[4]]), expected_id);
    }
    assert_eq!(device.sel_len(), 128);

    let response = add_record(&mut device, 0xff);
    assert_completion(
        &response,
        STORAGE_REQUEST,
        COMMAND_ADD_SEL_ENTRY,
        COMPLETION_SEL_FULL,
    );
    assert_eq!(device.sel_len(), 128);
    assert_eq!(device.sel_record(0).unwrap()[7], 1);
}

#[test]
fn record_and_reservation_ids_roll_over_without_reserved_values() {
    let (_, mut source) = device(1);
    let mut state = source.save().unwrap();
    state.next_record_id = 0xfffe;
    state.reservation_id = 0xffff;
    let mut device = restore_target(1, state);

    assert_eq!(add_record(&mut device, 0x11)[3..5], [0xfe, 0xff]);
    assert_eq!(add_record(&mut device, 0x22)[3..5], [1, 0]);
    let reservation = transact(&mut device, &storage_request(COMMAND_RESERVE_SEL, &[]));
    assert_eq!(reservation[3..5], [1, 0]);
}

#[test]
fn clear_sel_validates_fields_and_resets_store() {
    let clock = FakeClock::new(100);
    let mut device = IpmiKcs::new(clock.clone());
    add_record(&mut device, 0x33);

    let bad_signature = transact(
        &mut device,
        &storage_request(COMMAND_CLEAR_SEL, &[0, 0, b'X', b'L', b'R', 0xaa]),
    );
    assert_completion(
        &bad_signature,
        STORAGE_REQUEST,
        COMMAND_CLEAR_SEL,
        COMPLETION_INVALID_DATA_FIELD,
    );
    let bad_action = clear(&mut device, 0, 1);
    assert_completion(
        &bad_action,
        STORAGE_REQUEST,
        COMMAND_CLEAR_SEL,
        COMPLETION_INVALID_DATA_FIELD,
    );

    assert_eq!(
        clear(&mut device, 0xdead, 0),
        [
            STORAGE_REQUEST | 0x04,
            COMMAND_CLEAR_SEL,
            COMPLETION_SUCCESS,
            1,
        ]
    );
    assert_eq!(device.sel_len(), 1);

    clock.set(120);
    assert_eq!(
        clear(&mut device, 0xbeef, 0xaa),
        [
            STORAGE_REQUEST | 0x04,
            COMMAND_CLEAR_SEL,
            COMPLETION_SUCCESS,
            1,
        ]
    );
    assert_eq!(device.sel_len(), 0);
    assert_eq!(add_record(&mut device, 0x44)[3..5], [1, 0]);

    let info = transact(&mut device, &storage_request(COMMAND_GET_SEL_INFO, &[]));
    assert_eq!(
        u32::from_le_bytes([info[12], info[13], info[14], info[15]]),
        120
    );
}

#[test]
fn sel_time_supports_positive_negative_and_wrapping_offsets() {
    let clock = FakeClock::new(1000);
    let mut device = IpmiKcs::new(clock.clone());

    let time = transact(&mut device, &storage_request(COMMAND_GET_SEL_TIME, &[]));
    assert_eq!(u32::from_le_bytes(time[3..7].try_into().unwrap()), 1000);

    let response = transact(
        &mut device,
        &storage_request(COMMAND_SET_SEL_TIME, &1500u32.to_le_bytes()),
    );
    assert_completion(
        &response,
        STORAGE_REQUEST,
        COMMAND_SET_SEL_TIME,
        COMPLETION_SUCCESS,
    );
    assert_eq!(device.sel_time_offset_seconds(), 500);
    clock.set(1100);
    let time = transact(&mut device, &storage_request(COMMAND_GET_SEL_TIME, &[]));
    assert_eq!(u32::from_le_bytes(time[3..7].try_into().unwrap()), 1600);

    clock.set(1000);
    transact(
        &mut device,
        &storage_request(COMMAND_SET_SEL_TIME, &100u32.to_le_bytes()),
    );
    assert_eq!(device.sel_time_offset_seconds(), -900);
    clock.set(500);
    let time = transact(&mut device, &storage_request(COMMAND_GET_SEL_TIME, &[]));
    assert_eq!(u32::from_le_bytes(time[3..7].try_into().unwrap()), 0);

    clock.set(0);
    transact(
        &mut device,
        &storage_request(COMMAND_SET_SEL_TIME, &u32::MAX.to_le_bytes()),
    );
    clock.set(10);
    let time = transact(&mut device, &storage_request(COMMAND_GET_SEL_TIME, &[]));
    assert_eq!(u32::from_le_bytes(time[3..7].try_into().unwrap()), 9);
}

#[test]
fn sink_results_do_not_change_committed_records() {
    let clock = FakeClock::new(10);
    let sink_state = Arc::new(Mutex::new(SinkState::default()));
    let accept = Arc::new(AtomicBool::new(true));
    let sink = SharedSink {
        state: sink_state.clone(),
        accept: accept.clone(),
    };
    let mut device = IpmiKcs::with_event_sink(clock, sink);

    add_record(&mut device, 0x11);
    accept.store(false, Ordering::Relaxed);
    add_record(&mut device, 0x22);

    assert_eq!(device.sel_len(), 2);
    assert_eq!(
        device.stats(),
        SelStats {
            committed: 2,
            forwarded: 1,
            rate_limited: 0,
            sink_dropped: 1,
        }
    );
    let state = sink_state.lock();
    assert_eq!(state.records.len(), 1);
    assert_eq!(state.records[0].0, 1);
    assert_eq!(state.records[0].1, *device.sel_record(0).unwrap());
}

#[test]
fn sink_forwarding_is_limited_to_256_per_trusted_second() {
    let clock = FakeClock::new(10);
    let sink_state = Arc::new(Mutex::new(SinkState::default()));
    let sink = SharedSink {
        state: sink_state.clone(),
        accept: Arc::new(AtomicBool::new(true)),
    };
    let mut device = IpmiKcs::with_event_sink(clock.clone(), sink);

    for _ in 0..256 {
        assert_completion(
            &add_record(&mut device, 0x5a),
            STORAGE_REQUEST,
            COMMAND_ADD_SEL_ENTRY,
            COMPLETION_SUCCESS,
        );
        clear(&mut device, 0, 0xaa);
    }
    add_record(&mut device, 0x5b);

    assert_eq!(device.sel_len(), 1);
    assert_eq!(device.sel_record(0).unwrap()[7], 0x5b);
    assert_eq!(device.stats().committed, 257);
    assert_eq!(device.stats().forwarded, 256);
    assert_eq!(device.stats().rate_limited, 1);
    assert_eq!(sink_state.lock().records.len(), 256);

    clock.set(11);
    clear(&mut device, 0, 0xaa);
    add_record(&mut device, 0x5c);
    assert_eq!(device.stats().forwarded, 257);
    assert_eq!(device.stats().rate_limited, 1);
    for _ in 1..256 {
        clear(&mut device, 0, 0xaa);
        add_record(&mut device, 0x5c);
    }
    assert_eq!(device.stats().forwarded, 512);

    clock.set(10);
    clear(&mut device, 0, 0xaa);
    add_record(&mut device, 0x5d);
    assert_eq!(device.stats().forwarded, 512);
    assert_eq!(device.stats().rate_limited, 2);
}

#[test]
fn reset_preserves_sel_time_reservation_and_stats() {
    let clock = FakeClock::new(100);
    let mut device = IpmiKcs::new(clock);
    transact(
        &mut device,
        &storage_request(COMMAND_SET_SEL_TIME, &200u32.to_le_bytes()),
    );
    add_record(&mut device, 0x42);
    let reservation = transact(&mut device, &storage_request(COMMAND_RESERVE_SEL, &[]));

    device.write_command(KCS_COMMAND_WRITE_START);
    assert_eq!(device.read_data(), 0);
    device.write_data(APPLICATION_REQUEST);
    assert_eq!(device.read_status() & STATUS_STATE_MASK, KCS_STATE_WRITE);
    device.reset();

    assert_eq!(device.read_status(), KCS_STATE_IDLE);
    assert_eq!(device.sel_len(), 1);
    assert_eq!(device.sel_time_offset_seconds(), 100);
    assert_eq!(device.stats().committed, 1);

    let current_reservation = transact(&mut device, &storage_request(COMMAND_RESERVE_SEL, &[]));
    assert_eq!(
        u16::from_le_bytes([current_reservation[3], current_reservation[4]]),
        u16::from_le_bytes([reservation[3], reservation[4]]) + 1
    );
}

fn restore_target(seconds: i64, state: save_restore::SavedState) -> IpmiKcs {
    let (_, mut target) = device(seconds);
    target.restore(state).unwrap();
    target
}

#[test]
fn save_restore_idle_write_and_read_transactions() {
    let (_, mut idle) = device(1);
    let idle_state = idle.save().unwrap();
    let idle = restore_target(1, idle_state);
    assert_eq!(idle.read_status(), KCS_STATE_IDLE);

    let (_, mut writing) = device(1);
    writing.write_command(KCS_COMMAND_WRITE_START);
    assert_eq!(writing.read_data(), 0);
    writing.write_data(APPLICATION_REQUEST);
    assert_eq!(writing.read_data(), 0);
    writing.write_command(KCS_COMMAND_WRITE_END);
    assert_eq!(writing.read_data(), 0);
    let write_state = writing.save().unwrap();
    let mut writing = restore_target(1, write_state);
    assert_eq!(writing.read_status() & STATUS_STATE_MASK, KCS_STATE_WRITE);
    writing.write_data(COMMAND_GET_DEVICE_ID);
    let response = read_response(&mut writing);
    assert_completion(
        &response,
        APPLICATION_REQUEST,
        COMMAND_GET_DEVICE_ID,
        COMPLETION_SUCCESS,
    );

    let (_, mut reading) = device(1);
    submit_request(&mut reading, &[APPLICATION_REQUEST, COMMAND_GET_DEVICE_ID]);
    let read_state = reading.save().unwrap();
    let mut reading = restore_target(1, read_state);
    assert_eq!(reading.read_status() & STATUS_STATE_MASK, KCS_STATE_READ);
    let response = read_response(&mut reading);
    assert_completion(
        &response,
        APPLICATION_REQUEST,
        COMMAND_GET_DEVICE_ID,
        COMPLETION_SUCCESS,
    );
}

#[test]
fn save_restore_full_sel_and_adjusted_time() {
    let clock = FakeClock::new(1000);
    let mut source = IpmiKcs::new(clock.clone());
    transact(
        &mut source,
        &storage_request(COMMAND_SET_SEL_TIME, &1500u32.to_le_bytes()),
    );
    for fill in 0..128u8 {
        add_record(&mut source, fill);
    }
    clear(&mut source, 0, 0);

    let state = source.save().unwrap();
    let state = SavedStateBlob::new(state)
        .parse::<save_restore::SavedState>()
        .unwrap();
    let mut restored = restore_target(1100, state);
    assert_eq!(restored.sel_len(), 128);
    assert_eq!(restored.sel_time_offset_seconds(), 500);
    assert_eq!(restored.stats(), SelStats::default());
    let time = transact(&mut restored, &storage_request(COMMAND_GET_SEL_TIME, &[]));
    assert_eq!(u32::from_le_bytes(time[3..7].try_into().unwrap()), 1600);
    assert_completion(
        &add_record(&mut restored, 0xff),
        STORAGE_REQUEST,
        COMMAND_ADD_SEL_ENTRY,
        COMPLETION_SEL_FULL,
    );
}

fn assert_invalid_state(state: save_restore::SavedState) {
    let (_, mut target) = device(0);
    assert!(matches!(
        target.restore(state),
        Err(RestoreError::InvalidSavedState(_))
    ));
}

#[test]
fn malformed_saved_state_is_rejected() {
    let (_, mut source) = device(0);
    let valid = source.save().unwrap();

    let mut state = valid.clone();
    state.status = 0x10;
    assert_invalid_state(state);

    let mut state = valid.clone();
    state.status = STATUS_IBF;
    assert_invalid_state(state);

    let mut state = valid.clone();
    state.request = vec![0; 65];
    assert_invalid_state(state);

    let mut state = valid.clone();
    state.response = vec![0; 65];
    assert_invalid_state(state);

    let mut state = valid.clone();
    state.response = vec![0];
    state.response_position = 2;
    assert_invalid_state(state);

    let mut state = valid.clone();
    state.status = KCS_STATE_READ;
    state.response.clear();
    assert_invalid_state(state);

    let mut state = valid.clone();
    state.write_end_pending = true;
    assert_invalid_state(state);

    let mut state = valid.clone();
    state.sel_count = 1;
    assert_invalid_state(state);

    let mut state = valid.clone();
    state.sel_records = (1..=129u16)
        .map(|record_id| {
            let mut record = vec![0; 16];
            record[0..2].copy_from_slice(&record_id.to_le_bytes());
            record
        })
        .collect();
    state.sel_count = 129;
    state.next_record_id = 130;
    assert_invalid_state(state);

    let mut state = valid.clone();
    state.sel_records = vec![vec![0; 15]];
    state.sel_count = 1;
    assert_invalid_state(state);

    let mut state = valid.clone();
    state.sel_records = vec![vec![0; 16]];
    state.sel_count = 1;
    assert_invalid_state(state);

    let mut state = valid.clone();
    let mut record = vec![0; 16];
    record[0..2].copy_from_slice(&1u16.to_le_bytes());
    state.sel_records = vec![record.clone(), record];
    state.sel_count = 2;
    state.next_record_id = 2;
    assert_invalid_state(state);

    for next_record_id in [0, 0xffff] {
        let mut state = valid.clone();
        state.next_record_id = next_record_id;
        assert_invalid_state(state);
    }

    let mut state = valid;
    let mut record = vec![0; 16];
    record[0..2].copy_from_slice(&1u16.to_le_bytes());
    state.sel_records = vec![record];
    state.sel_count = 1;
    state.next_record_id = 1;
    assert_invalid_state(state);
}
