// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::IpmiKcs;
use crate::KCS_MESSAGE_MAX;
use crate::SelEventDisposition;
use crate::protocol::COMMAND_ADD_SEL_ENTRY;
use crate::protocol::COMMAND_CLEAR_SEL;
use crate::protocol::COMMAND_GET_SEL_ENTRY;
use crate::protocol::COMMAND_GET_SEL_INFO;
use crate::protocol::COMMAND_GET_SEL_TIME;
use crate::protocol::COMMAND_RESERVE_SEL;
use crate::protocol::COMMAND_SET_SEL_TIME;
use crate::protocol::COMPLETION_INVALID_COMMAND;
use crate::protocol::COMPLETION_INVALID_DATA_FIELD;
use crate::protocol::COMPLETION_PARAMETER_OUT_OF_RANGE;
use crate::protocol::COMPLETION_RECORD_NOT_PRESENT;
use crate::protocol::COMPLETION_SEL_FULL;
use crate::protocol::COMPLETION_SUCCESS;
use crate::protocol::completion;
use crate::protocol::invalid_length;

pub(crate) const SEL_RECORD_SIZE: usize = 16;
pub(crate) const SEL_CAPACITY: usize = 128;
const SEL_VERSION: u8 = 0x51;
const SEL_FORWARD_LIMIT: u32 = 256;

#[derive(Default)]
pub(crate) struct SelState {
    pub(crate) records: Vec<[u8; SEL_RECORD_SIZE]>,
    pub(crate) next_record_id: u16,
    pub(crate) reservation_id: u16,
    pub(crate) time_offset_seconds: i64,
    pub(crate) last_erase_timestamp: u32,
}

impl SelState {
    pub(crate) fn new() -> Self {
        Self {
            next_record_id: 1,
            ..Self::default()
        }
    }
}

#[derive(Default)]
pub(crate) struct RateLimiter {
    window_second: Option<i64>,
    forwarded_in_window: u32,
}

impl RateLimiter {
    fn allow(&mut self, trusted_second: i64) -> bool {
        if self.window_second.is_none()
            || trusted_second.saturating_sub(self.window_second.unwrap_or(trusted_second)) >= 1
        {
            self.window_second = Some(trusted_second);
            self.forwarded_in_window = 0;
        }

        if self.forwarded_in_window >= SEL_FORWARD_LIMIT {
            false
        } else {
            self.forwarded_in_window += 1;
            true
        }
    }

    pub(crate) fn reset(&mut self) {
        self.window_second = None;
        self.forwarded_in_window = 0;
    }
}

impl IpmiKcs {
    pub(crate) fn handle_sel_command(
        &mut self,
        command: u8,
        data: &[u8],
        out: &mut [u8; KCS_MESSAGE_MAX],
    ) -> usize {
        match command {
            COMMAND_GET_SEL_INFO => self.get_sel_info(out),
            COMMAND_RESERVE_SEL => self.reserve_sel(out),
            COMMAND_GET_SEL_ENTRY => self.get_sel_entry(data, out),
            COMMAND_ADD_SEL_ENTRY => self.add_sel_entry(data, out),
            COMMAND_CLEAR_SEL => self.clear_sel(data, out),
            COMMAND_GET_SEL_TIME => self.get_sel_time(out),
            COMMAND_SET_SEL_TIME => self.set_sel_time(data, out),
            _ => completion(out, COMPLETION_INVALID_COMMAND),
        }
    }

    fn get_sel_info(&mut self, out: &mut [u8; KCS_MESSAGE_MAX]) -> usize {
        let count = self.sel.records.len() as u16;
        let free_bytes = ((SEL_CAPACITY - self.sel.records.len()) * SEL_RECORD_SIZE) as u16;
        let last_addition_timestamp = self
            .sel
            .records
            .last()
            .map(|record| u32::from_le_bytes([record[3], record[4], record[5], record[6]]))
            .unwrap_or(0);
        let mut pos = 0;
        out[pos] = COMPLETION_SUCCESS;
        pos += 1;
        out[pos] = SEL_VERSION;
        pos += 1;
        put_u16(out, &mut pos, count);
        put_u16(out, &mut pos, free_bytes);
        put_u32(out, &mut pos, last_addition_timestamp);
        put_u32(out, &mut pos, self.sel.last_erase_timestamp);
        out[pos] = 0x02;
        pos + 1
    }

    fn reserve_sel(&mut self, out: &mut [u8; KCS_MESSAGE_MAX]) -> usize {
        self.sel.reservation_id = self.sel.reservation_id.wrapping_add(1);
        if self.sel.reservation_id == 0 {
            self.sel.reservation_id = 1;
        }

        out[0] = COMPLETION_SUCCESS;
        out[1..3].copy_from_slice(&self.sel.reservation_id.to_le_bytes());
        3
    }

    fn get_sel_entry(&mut self, data: &[u8], out: &mut [u8; KCS_MESSAGE_MAX]) -> usize {
        let Some(data) = data.get(..6) else {
            return invalid_length(out);
        };

        let offset = usize::from(data[4]);
        if offset >= SEL_RECORD_SIZE {
            return completion(out, COMPLETION_PARAMETER_OUT_OF_RANGE);
        }

        let record_id = u16::from_le_bytes([data[2], data[3]]);
        let Some(index) = self.find_record(record_id) else {
            return completion(out, COMPLETION_RECORD_NOT_PRESENT);
        };

        let next_record_id = self
            .sel
            .records
            .get(index + 1)
            .map(|record| u16::from_le_bytes([record[0], record[1]]))
            .unwrap_or(0xffff);
        let end = offset
            .saturating_add(usize::from(data[5]))
            .min(SEL_RECORD_SIZE);
        let record = &self.sel.records[index];
        let bytes = &record[offset..end];

        out[0] = COMPLETION_SUCCESS;
        out[1..3].copy_from_slice(&next_record_id.to_le_bytes());
        out[3..3 + bytes.len()].copy_from_slice(bytes);
        3 + bytes.len()
    }

    fn add_sel_entry(&mut self, data: &[u8], out: &mut [u8; KCS_MESSAGE_MAX]) -> usize {
        let Some(data) = data.get(..SEL_RECORD_SIZE) else {
            return invalid_length(out);
        };
        let mut record = [0; SEL_RECORD_SIZE];
        record.copy_from_slice(data);

        if self.sel.records.len() >= SEL_CAPACITY {
            return completion(out, COMPLETION_SEL_FULL);
        }

        let Some(record_id) = self.allocate_record_id() else {
            return completion(out, COMPLETION_SEL_FULL);
        };
        let trusted_seconds = self.clock.unix_seconds();
        let timestamp = adjusted_timestamp(trusted_seconds, self.sel.time_offset_seconds);
        record[0..2].copy_from_slice(&record_id.to_le_bytes());
        record[3..7].copy_from_slice(&timestamp.to_le_bytes());

        self.sel.records.push(record);
        self.stats.committed = self.stats.committed.saturating_add(1);

        if let Some(sink) = self.sink.as_mut() {
            if self.rate_limiter.allow(trusted_seconds) {
                match sink.try_send(record_id, record) {
                    SelEventDisposition::Accepted => {
                        self.stats.forwarded = self.stats.forwarded.saturating_add(1);
                    }
                    SelEventDisposition::Dropped => {
                        self.stats.sink_dropped = self.stats.sink_dropped.saturating_add(1);
                    }
                }
            } else {
                self.stats.rate_limited = self.stats.rate_limited.saturating_add(1);
            }
        }

        out[0] = COMPLETION_SUCCESS;
        out[1..3].copy_from_slice(&record_id.to_le_bytes());
        3
    }

    fn clear_sel(&mut self, data: &[u8], out: &mut [u8; KCS_MESSAGE_MAX]) -> usize {
        let Some(data) = data.get(..6) else {
            return invalid_length(out);
        };

        if data[2..5] != *b"CLR" {
            return completion(out, COMPLETION_INVALID_DATA_FIELD);
        }

        match data[5] {
            0xaa => {
                self.sel.records.clear();
                self.sel.next_record_id = 1;
                let trusted_seconds = self.clock.unix_seconds();
                self.sel.last_erase_timestamp =
                    adjusted_timestamp(trusted_seconds, self.sel.time_offset_seconds);
            }
            0x00 => {}
            _ => return completion(out, COMPLETION_INVALID_DATA_FIELD),
        }

        out[0] = COMPLETION_SUCCESS;
        out[1] = 1;
        2
    }

    fn get_sel_time(&mut self, out: &mut [u8; KCS_MESSAGE_MAX]) -> usize {
        let trusted_seconds = self.clock.unix_seconds();
        out[0] = COMPLETION_SUCCESS;
        out[1..5].copy_from_slice(
            &adjusted_timestamp(trusted_seconds, self.sel.time_offset_seconds).to_le_bytes(),
        );
        5
    }

    fn set_sel_time(&mut self, data: &[u8], out: &mut [u8; KCS_MESSAGE_MAX]) -> usize {
        let Some(data) = data.get(..4) else {
            return invalid_length(out);
        };

        let requested = i64::from(u32::from_le_bytes([data[0], data[1], data[2], data[3]]));
        self.sel.time_offset_seconds = requested.saturating_sub(self.clock.unix_seconds());
        completion(out, COMPLETION_SUCCESS)
    }

    fn find_record(&self, record_id: u16) -> Option<usize> {
        match record_id {
            0 => (!self.sel.records.is_empty()).then_some(0),
            0xffff => self.sel.records.len().checked_sub(1),
            _ => self
                .sel
                .records
                .iter()
                .position(|record| u16::from_le_bytes([record[0], record[1]]) == record_id),
        }
    }

    fn allocate_record_id(&mut self) -> Option<u16> {
        let mut candidate = normalize_record_id(self.sel.next_record_id);
        for _ in 0..=SEL_CAPACITY {
            let used = self
                .sel
                .records
                .iter()
                .any(|record| u16::from_le_bytes([record[0], record[1]]) == candidate);
            if !used {
                self.sel.next_record_id = increment_record_id(candidate);
                return Some(candidate);
            }
            candidate = increment_record_id(candidate);
        }
        None
    }
}

fn normalize_record_id(record_id: u16) -> u16 {
    if record_id == 0 || record_id == 0xffff {
        1
    } else {
        record_id
    }
}

fn increment_record_id(record_id: u16) -> u16 {
    normalize_record_id(record_id.wrapping_add(1))
}

fn adjusted_timestamp(trusted_seconds: i64, offset_seconds: i64) -> u32 {
    let adjusted = trusted_seconds.saturating_add(offset_seconds);
    if adjusted < 0 { 0 } else { adjusted as u32 }
}

fn put_u16(out: &mut [u8; KCS_MESSAGE_MAX], pos: &mut usize, value: u16) {
    out[*pos..*pos + 2].copy_from_slice(&value.to_le_bytes());
    *pos += 2;
}

fn put_u32(out: &mut [u8; KCS_MESSAGE_MAX], pos: &mut usize, value: u32) {
    out[*pos..*pos + 4].copy_from_slice(&value.to_le_bytes());
    *pos += 4;
}
