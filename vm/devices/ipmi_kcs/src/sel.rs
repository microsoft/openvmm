// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! System Event Log (SEL) storage and IPMI SEL command handling.

use crate::protocol::CompletionCode;
use crate::protocol::IpmiCommand;
use crate::sink::SelDeps;
use inspect::Inspect;

/// Maximum number of SEL entries.
const MAX_SEL_ENTRIES: usize = 128;

/// Size of a single SEL record in bytes.
pub const SEL_RECORD_SIZE: usize = 16;

/// SEL version (IPMI v1.5 / v2.0 format).
const SEL_VERSION: u8 = 0x51;

/// A 16-byte SEL record per IPMI v2.0 Section 32.
struct SelEntry {
    record_id: u16,
    data: [u8; SEL_RECORD_SIZE],
}

impl Inspect for SelEntry {
    fn inspect(&self, req: inspect::Request<'_>) {
        let d = &self.data;
        let record_type = d[2];
        let timestamp = u32::from_le_bytes([d[3], d[4], d[5], d[6]]);
        let mut resp = req.respond();
        resp.hex("record_id", self.record_id)
            .hex("record_type", record_type)
            .field("timestamp", timestamp);

        if record_type == 0x02 {
            // Standard System Event Record (IPMI v2.0, Section 32.1)
            let generator_id = u16::from_le_bytes([d[7], d[8]]);
            resp.hex("generator_id", generator_id)
                .hex("evm_rev", d[9])
                .hex("sensor_type", d[10])
                .hex("sensor_number", d[11])
                .hex("event_dir_type", d[12])
                .hex("event_data1", d[13])
                .hex("event_data2", d[14])
                .hex("event_data3", d[15]);
        } else if (0xC0..=0xDF).contains(&record_type) {
            // OEM Timestamped Record (IPMI v2.0, Section 32.2)
            let manufacturer_id =
                u32::from_le_bytes([d[7], d[8], d[9], 0]);
            resp.field("manufacturer_id", manufacturer_id)
                .hex("oem_data", u64::from_le_bytes([d[10], d[11], d[12], d[13], d[14], d[15], 0, 0]));
        } else if record_type >= 0xE0 {
            // OEM Non-timestamped Record (IPMI v2.0, Section 32.3)
            // Bytes 3-15 are all OEM-defined (no timestamp)
            resp.hex("oem_data", &d[3..]);
        } else {
            // Unknown record type — dump raw bytes
            resp.hex("raw_data", &d[2..]);
        }
    }
}

/// SEL storage.
pub struct SelStore {
    entries: Vec<SelEntry>,
    next_record_id: u16,
    time_offset: i64,
    reservation_id: u16,
    deps: SelDeps,
}

impl Inspect for SelStore {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .field("entry_count", self.entries.len())
            .field("next_record_id", self.next_record_id)
            .field("time_offset", self.time_offset)
            .child("entries", |req| {
                let mut resp = req.respond();
                for entry in &self.entries {
                    resp.child(&format!("{}", entry.record_id), |req| {
                        entry.inspect(req);
                    });
                }
            });
    }
}

impl SelStore {
    /// Create a new empty SEL store with the given egress/clock dependencies.
    pub fn with_deps(deps: SelDeps) -> Self {
        Self {
            entries: Vec::new(),
            next_record_id: 1,
            time_offset: 0,
            reservation_id: 0,
            deps,
        }
    }

    /// Reset the SEL store, clearing all entries.
    pub fn reset(&mut self) {
        self.entries.clear();
        self.next_record_id = 1;
        self.time_offset = 0;
        self.reservation_id = 0;
    }

    /// Get the current BMC time as seconds since 1970-01-01.
    fn bmc_time(&self) -> u32 {
        let now = self.deps.clock.now_unix_secs();
        let adjusted = now.saturating_add(self.time_offset);
        adjusted.max(0) as u32
    }

    /// Handle an IPMI SEL command. Returns the response data (after NetFn/LUN
    /// and command byte — i.e., starting with the completion code).
    pub fn handle_command(&mut self, cmd: IpmiCommand, data: &[u8]) -> Vec<u8> {
        match cmd {
            IpmiCommand::GET_SEL_INFO => self.cmd_get_sel_info(),
            IpmiCommand::GET_SEL_ENTRY => self.cmd_get_sel_entry(data),
            IpmiCommand::ADD_SEL_ENTRY => self.cmd_add_sel_entry(data),
            IpmiCommand::CLEAR_SEL => self.cmd_clear_sel(data),
            IpmiCommand::GET_SEL_TIME => self.cmd_get_sel_time(),
            IpmiCommand::SET_SEL_TIME => self.cmd_set_sel_time(data),
            _ => vec![CompletionCode::INVALID_COMMAND.0],
        }
    }

    /// Get SEL Info (0x40).
    fn cmd_get_sel_info(&self) -> Vec<u8> {
        let count = self.entries.len() as u16;
        let free_space = ((MAX_SEL_ENTRIES - self.entries.len()) * SEL_RECORD_SIZE) as u16;

        // Most recent addition timestamp (0 if empty).
        let last_add_time: u32 = self
            .entries
            .last()
            .map(|e| u32::from_le_bytes([e.data[3], e.data[4], e.data[5], e.data[6]]))
            .unwrap_or(0);

        let mut resp = vec![CompletionCode::SUCCESS.0];
        resp.push(SEL_VERSION);
        resp.extend_from_slice(&count.to_le_bytes());
        resp.extend_from_slice(&free_space.to_le_bytes());
        resp.extend_from_slice(&last_add_time.to_le_bytes()); // Most recent addition timestamp
        resp.extend_from_slice(&last_add_time.to_le_bytes()); // Most recent erase timestamp (same)
        resp.push(0x00); // Operation support (no overflow, delete not supported)
        resp
    }

    /// Get SEL Entry (0x43).
    fn cmd_get_sel_entry(&self, data: &[u8]) -> Vec<u8> {
        // Data: [ResvID_lo, ResvID_hi, RecordID_lo, RecordID_hi, Offset, BytesToRead]
        if data.len() < 6 {
            return vec![CompletionCode::REQUEST_DATA_LENGTH_INVALID.0];
        }

        let record_id = u16::from_le_bytes([data[2], data[3]]);
        let offset = data[4] as usize;
        let bytes_to_read = data[5] as usize;

        // Special record IDs: 0x0000 = first, 0xFFFF = last.
        let entry = match record_id {
            0x0000 => self.entries.first(),
            0xFFFF => self.entries.last(),
            id => self.entries.iter().find(|e| e.record_id == id),
        };

        let entry = match entry {
            Some(e) => e,
            None => {
                // Record not found — return completion code 0xCB.
                return vec![0xCB];
            }
        };

        // Determine next record ID.
        let next_record_id = self
            .entries
            .iter()
            .position(|e| e.record_id == entry.record_id)
            .and_then(|idx| self.entries.get(idx + 1))
            .map(|e| e.record_id)
            .unwrap_or(0xFFFF); // 0xFFFF means no more records.

        // Extract the requested portion of the record.
        let end = (offset + bytes_to_read).min(SEL_RECORD_SIZE);
        let start = offset.min(SEL_RECORD_SIZE);
        let record_data = &entry.data[start..end];

        let mut resp = vec![CompletionCode::SUCCESS.0];
        resp.extend_from_slice(&next_record_id.to_le_bytes());
        resp.extend_from_slice(record_data);
        resp
    }

    /// Add SEL Entry (0x44).
    fn cmd_add_sel_entry(&mut self, data: &[u8]) -> Vec<u8> {
        if data.len() < SEL_RECORD_SIZE {
            return vec![CompletionCode::REQUEST_DATA_LENGTH_INVALID.0];
        }

        if self.entries.len() >= MAX_SEL_ENTRIES {
            // SEL is full — return "out of space" completion code (0x80
            // per IPMI v2.0 Table 5-2, command-specific range).
            return vec![0x80];
        }

        let record_id = self.next_record_id;
        self.next_record_id = self.next_record_id.wrapping_add(1);
        if self.next_record_id == 0 || self.next_record_id == 0xFFFF {
            self.next_record_id = 1;
        }

        let mut record_data = [0u8; SEL_RECORD_SIZE];
        record_data.copy_from_slice(&data[..SEL_RECORD_SIZE]);

        // Overwrite record ID with the assigned one.
        record_data[0] = record_id as u8;
        record_data[1] = (record_id >> 8) as u8;

        // Fill in timestamp with current BMC time.
        let timestamp = self.bmc_time();
        record_data[3..7].copy_from_slice(&timestamp.to_le_bytes());

        self.entries.push(SelEntry {
            record_id,
            data: record_data,
        });

        // Forward the committed record to the host sink (no-op by default).
        self.deps.sink.log_sel_entry(record_id, &record_data);

        let mut resp = vec![CompletionCode::SUCCESS.0];
        resp.extend_from_slice(&record_id.to_le_bytes());
        resp
    }

    /// Clear SEL (0x47).
    fn cmd_clear_sel(&mut self, data: &[u8]) -> Vec<u8> {
        // Data: [ResvID_lo, ResvID_hi, 'C', 'L', 'R', action]
        if data.len() < 6 {
            return vec![CompletionCode::REQUEST_DATA_LENGTH_INVALID.0];
        }

        // Verify "CLR" signature.
        if data[2] != 0x43 || data[3] != 0x4C || data[4] != 0x52 {
            return vec![CompletionCode::REQUEST_DATA_LENGTH_INVALID.0];
        }

        let action = data[5];
        match action {
            0xAA => {
                // Initiate erase — for virtual device, complete immediately.
                self.entries.clear();
                self.next_record_id = 1;
                // Return erasure complete (0x01 = erasure completed).
                vec![CompletionCode::SUCCESS.0, 0x01]
            }
            0x00 => {
                // Get erasure status — always complete for virtual device.
                vec![CompletionCode::SUCCESS.0, 0x01]
            }
            _ => vec![CompletionCode::REQUEST_DATA_LENGTH_INVALID.0],
        }
    }

    /// Get SEL Time (0x48).
    fn cmd_get_sel_time(&self) -> Vec<u8> {
        let time = self.bmc_time();
        let mut resp = vec![CompletionCode::SUCCESS.0];
        resp.extend_from_slice(&time.to_le_bytes());
        resp
    }

    /// Set SEL Time (0x49).
    fn cmd_set_sel_time(&mut self, data: &[u8]) -> Vec<u8> {
        if data.len() < 4 {
            return vec![CompletionCode::REQUEST_DATA_LENGTH_INVALID.0];
        }

        let new_time = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        self.time_offset = (new_time as i64) - now;

        vec![CompletionCode::SUCCESS.0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::BmcClock;
    use crate::sink::SelDeps;
    use crate::sink::SelSink;
    use std::sync::Arc;
    use std::sync::Mutex;
    use test_with_tracing::test;

    impl SelStore {
        /// Empty SEL store with default (no-op sink, system clock) deps.
        fn new() -> Self {
            Self::with_deps(SelDeps::default())
        }
    }

    /// Sink that records forwarded entries for assertions.
    #[derive(Default)]
    struct CapturingSink {
        entries: Mutex<Vec<(u16, Vec<u8>)>>,
    }

    impl SelSink for CapturingSink {
        fn log_sel_entry(&self, record_id: u16, record: &[u8]) {
            self.entries.lock().unwrap().push((record_id, record.to_vec()));
        }
    }

    /// Fixed clock for deterministic timestamps.
    struct FixedClock(i64);

    impl BmcClock for FixedClock {
        fn now_unix_secs(&self) -> i64 {
            self.0
        }
    }

    fn make_sel_record() -> [u8; 16] {
        [
            0x00, 0x00, // Record ID (ignored, assigned by BMC)
            0x02, // Record Type = System Event
            0x00, 0x00, 0x00, 0x00, // Timestamp (BMC fills in)
            0x20, 0x00, // Generator ID
            0x04, // EvM Rev
            0x01, // Sensor Type = Temperature
            0x42, // Sensor Number
            0x6F, // Event Dir / Event Type
            0x01, 0x02, 0x03, // Event Data 1-3
        ]
    }

    #[test]
    fn sel_add_and_get_entry() {
        let mut store = SelStore::new();
        let record = make_sel_record();

        // Add an entry.
        let resp = store.handle_command(IpmiCommand::ADD_SEL_ENTRY, &record);
        assert_eq!(resp[0], CompletionCode::SUCCESS.0);
        assert_eq!(resp.len(), 3); // CC + 2 bytes record ID
        let record_id = u16::from_le_bytes([resp[1], resp[2]]);
        assert_eq!(record_id, 1);

        // Get the entry back.
        let get_data = [
            0x00, 0x00, // Reservation ID
            resp[1], resp[2], // Record ID
            0x00,    // Offset
            0xFF,    // Read all
        ];
        let resp = store.handle_command(IpmiCommand::GET_SEL_ENTRY, &get_data);
        assert_eq!(resp[0], CompletionCode::SUCCESS.0);
        // Next record ID = 0xFFFF (no more).
        assert_eq!(u16::from_le_bytes([resp[1], resp[2]]), 0xFFFF);
        // Record data starts at offset 3.
        let record_data = &resp[3..3 + SEL_RECORD_SIZE];
        // Record ID should be 1.
        assert_eq!(u16::from_le_bytes([record_data[0], record_data[1]]), 1);
        // Record type should match.
        assert_eq!(record_data[2], 0x02);
        // Sensor number should match (offset 11 in SEL record).
        assert_eq!(record_data[11], 0x42);
        // Event data should match.
        assert_eq!(record_data[13], 0x01);
        assert_eq!(record_data[14], 0x02);
        assert_eq!(record_data[15], 0x03);
    }

    #[test]
    fn sel_get_info() {
        let mut store = SelStore::new();

        // Empty SEL.
        let resp = store.handle_command(IpmiCommand::GET_SEL_INFO, &[]);
        assert_eq!(resp[0], CompletionCode::SUCCESS.0);
        assert_eq!(resp[1], SEL_VERSION);
        // Count = 0.
        assert_eq!(u16::from_le_bytes([resp[2], resp[3]]), 0);

        // Add an entry.
        let record = make_sel_record();
        store.handle_command(IpmiCommand::ADD_SEL_ENTRY, &record);

        let resp = store.handle_command(IpmiCommand::GET_SEL_INFO, &[]);
        assert_eq!(resp[0], CompletionCode::SUCCESS.0);
        // Count = 1.
        assert_eq!(u16::from_le_bytes([resp[2], resp[3]]), 1);
    }

    #[test]
    fn sel_clear() {
        let mut store = SelStore::new();
        let record = make_sel_record();

        // Add two entries.
        store.handle_command(IpmiCommand::ADD_SEL_ENTRY, &record);
        store.handle_command(IpmiCommand::ADD_SEL_ENTRY, &record);

        // Verify count = 2.
        let resp = store.handle_command(IpmiCommand::GET_SEL_INFO, &[]);
        assert_eq!(u16::from_le_bytes([resp[2], resp[3]]), 2);

        // Clear SEL.
        let clear_data = [0x00, 0x00, 0x43, 0x4C, 0x52, 0xAA];
        let resp = store.handle_command(IpmiCommand::CLEAR_SEL, &clear_data);
        assert_eq!(resp[0], CompletionCode::SUCCESS.0);
        assert_eq!(resp[1], 0x01); // Erasure complete.

        // Verify count = 0.
        let resp = store.handle_command(IpmiCommand::GET_SEL_INFO, &[]);
        assert_eq!(u16::from_le_bytes([resp[2], resp[3]]), 0);
    }

    #[test]
    fn sel_get_entry_not_found() {
        let mut store = SelStore::new();
        let get_data = [0x00, 0x00, 0x01, 0x00, 0x00, 0xFF];
        let resp = store.handle_command(IpmiCommand::GET_SEL_ENTRY, &get_data);
        // 0xCB = requested record not found.
        assert_eq!(resp[0], 0xCB);
    }

    #[test]
    fn sel_get_first_and_last() {
        let mut store = SelStore::new();
        let record = make_sel_record();

        store.handle_command(IpmiCommand::ADD_SEL_ENTRY, &record);
        store.handle_command(IpmiCommand::ADD_SEL_ENTRY, &record);
        store.handle_command(IpmiCommand::ADD_SEL_ENTRY, &record);

        // Get first (record ID 0x0000).
        let get_data = [0x00, 0x00, 0x00, 0x00, 0x00, 0xFF];
        let resp = store.handle_command(IpmiCommand::GET_SEL_ENTRY, &get_data);
        assert_eq!(resp[0], CompletionCode::SUCCESS.0);
        let record_data = &resp[3..];
        assert_eq!(u16::from_le_bytes([record_data[0], record_data[1]]), 1);

        // Get last (record ID 0xFFFF).
        let get_data = [0x00, 0x00, 0xFF, 0xFF, 0x00, 0xFF];
        let resp = store.handle_command(IpmiCommand::GET_SEL_ENTRY, &get_data);
        assert_eq!(resp[0], CompletionCode::SUCCESS.0);
        let record_data = &resp[3..];
        assert_eq!(u16::from_le_bytes([record_data[0], record_data[1]]), 3);
    }

    #[test]
    fn sel_invalid_data_length() {
        let mut store = SelStore::new();

        // Add with too few bytes.
        let resp = store.handle_command(IpmiCommand::ADD_SEL_ENTRY, &[0x00; 5]);
        assert_eq!(resp[0], CompletionCode::REQUEST_DATA_LENGTH_INVALID.0);

        // Get with too few bytes.
        let resp = store.handle_command(IpmiCommand::GET_SEL_ENTRY, &[0x00; 2]);
        assert_eq!(resp[0], CompletionCode::REQUEST_DATA_LENGTH_INVALID.0);

        // Clear with too few bytes.
        let resp = store.handle_command(IpmiCommand::CLEAR_SEL, &[0x00; 3]);
        assert_eq!(resp[0], CompletionCode::REQUEST_DATA_LENGTH_INVALID.0);

        // Set time with too few bytes.
        let resp = store.handle_command(IpmiCommand::SET_SEL_TIME, &[0x00; 2]);
        assert_eq!(resp[0], CompletionCode::REQUEST_DATA_LENGTH_INVALID.0);
    }

    #[test]
    fn sel_unknown_command() {
        let mut store = SelStore::new();
        let resp = store.handle_command(IpmiCommand(0xFF), &[]);
        assert_eq!(resp[0], CompletionCode::INVALID_COMMAND.0);
    }

    #[test]
    fn sel_time_get_and_set() {
        let mut store = SelStore::new();

        // Get time (should be current time approximately).
        let resp = store.handle_command(IpmiCommand::GET_SEL_TIME, &[]);
        assert_eq!(resp[0], CompletionCode::SUCCESS.0);
        assert_eq!(resp.len(), 5);
        let time = u32::from_le_bytes([resp[1], resp[2], resp[3], resp[4]]);
        assert!(time > 0);

        // Set time to a known value.
        let new_time: u32 = 1_000_000;
        let resp = store.handle_command(IpmiCommand::SET_SEL_TIME, &new_time.to_le_bytes());
        assert_eq!(resp[0], CompletionCode::SUCCESS.0);

        // Get time should return approximately the same value.
        let resp = store.handle_command(IpmiCommand::GET_SEL_TIME, &[]);
        let time = u32::from_le_bytes([resp[1], resp[2], resp[3], resp[4]]);
        // Allow 2 seconds of drift for test execution time.
        assert!((1_000_000..=1_000_002).contains(&time));
    }

    #[test]
    fn sel_multiple_entries_next_record_id() {
        let mut store = SelStore::new();
        let record = make_sel_record();

        store.handle_command(IpmiCommand::ADD_SEL_ENTRY, &record);
        store.handle_command(IpmiCommand::ADD_SEL_ENTRY, &record);

        // Get first entry — next record should be second.
        let get_data = [0x00, 0x00, 0x01, 0x00, 0x00, 0xFF];
        let resp = store.handle_command(IpmiCommand::GET_SEL_ENTRY, &get_data);
        assert_eq!(resp[0], CompletionCode::SUCCESS.0);
        let next_id = u16::from_le_bytes([resp[1], resp[2]]);
        assert_eq!(next_id, 2);

        // Get second entry — next record should be 0xFFFF (end).
        let get_data = [0x00, 0x00, 0x02, 0x00, 0x00, 0xFF];
        let resp = store.handle_command(IpmiCommand::GET_SEL_ENTRY, &get_data);
        assert_eq!(resp[0], CompletionCode::SUCCESS.0);
        let next_id = u16::from_le_bytes([resp[1], resp[2]]);
        assert_eq!(next_id, 0xFFFF);
    }

    #[test]
    fn sel_sink_receives_entries() {
        let sink = Arc::new(CapturingSink::default());
        let deps = SelDeps {
            sink: sink.clone(),
            clock: Arc::new(FixedClock(1_700_000_000)),
        };
        let mut store = SelStore::with_deps(deps);

        let resp = store.handle_command(IpmiCommand::ADD_SEL_ENTRY, &make_sel_record());
        assert_eq!(resp[0], CompletionCode::SUCCESS.0);

        let captured = sink.entries.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let (record_id, record) = &captured[0];
        assert_eq!(*record_id, 1);
        // Record id stamped into the forwarded record.
        assert_eq!(u16::from_le_bytes([record[0], record[1]]), 1);
        // Timestamp comes from the injected clock.
        assert_eq!(u32::from_le_bytes([record[3], record[4], record[5], record[6]]), 1_700_000_000);
    }

    #[test]
    fn sel_injected_clock_used_for_time() {
        let deps = SelDeps {
            sink: Arc::new(crate::sink::NullSelSink),
            clock: Arc::new(FixedClock(1_700_000_000)),
        };
        let mut store = SelStore::with_deps(deps);
        let resp = store.handle_command(IpmiCommand::GET_SEL_TIME, &[]);
        let time = u32::from_le_bytes([resp[1], resp[2], resp[3], resp[4]]);
        assert_eq!(time, 1_700_000_000);
    }
}
