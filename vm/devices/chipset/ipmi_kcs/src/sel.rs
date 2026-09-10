// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! System Event Log command handling and emulator state.

use crate::IpmiKcs;
use crate::KCS_MESSAGE_MAX;
use crate::SendOutcome;
use crate::protocol::completion;
use crate::protocol::invalid_length;
use crate::protocol::write_response;
use ipmi_protocol::AddSelEntryRequest;
use ipmi_protocol::AddSelEntryResponse;
use ipmi_protocol::CLEAR_SEL_ERASE_COMPLETE;
use ipmi_protocol::CLEAR_SEL_GET_STATUS;
use ipmi_protocol::CLEAR_SEL_INITIATE_ERASE;
use ipmi_protocol::CLEAR_SEL_SIGNATURE;
use ipmi_protocol::COMMAND_ADD_SEL_ENTRY;
use ipmi_protocol::COMMAND_CLEAR_SEL;
use ipmi_protocol::COMMAND_GET_SEL_ENTRY;
use ipmi_protocol::COMMAND_GET_SEL_INFO;
use ipmi_protocol::COMMAND_GET_SEL_TIME;
use ipmi_protocol::COMMAND_RESERVE_SEL;
use ipmi_protocol::COMMAND_SET_SEL_TIME;
use ipmi_protocol::COMPLETION_INVALID_COMMAND;
use ipmi_protocol::COMPLETION_INVALID_DATA_FIELD;
use ipmi_protocol::COMPLETION_PARAMETER_OUT_OF_RANGE;
use ipmi_protocol::COMPLETION_RECORD_NOT_PRESENT;
use ipmi_protocol::COMPLETION_SEL_FULL;
use ipmi_protocol::COMPLETION_SUCCESS;
use ipmi_protocol::ClearSelRequest;
use ipmi_protocol::ClearSelResponse;
use ipmi_protocol::GetSelEntryRequest;
use ipmi_protocol::GetSelEntryResponseHeader;
use ipmi_protocol::GetSelInfoResponse;
use ipmi_protocol::GetSelTimeResponse;
use ipmi_protocol::ReserveSelResponse;
use ipmi_protocol::SEL_OPERATION_SUPPORT_RESERVE;
use ipmi_protocol::SEL_RECORD_SIZE;
use ipmi_protocol::SEL_VERSION;
use ipmi_protocol::SelRecord;
use ipmi_protocol::SetSelTimeRequest;
use zerocopy::FromBytes;
use zerocopy::U16;
use zerocopy::U32;

pub(crate) const SEL_CAPACITY: usize = 128;
const SEL_FORWARD_LIMIT: u32 = 256;

#[derive(Default)]
pub(crate) struct SelState {
    pub(crate) records: Vec<SelRecord>,
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
    /// Dispatches an IPMI storage command implemented by the SEL.
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
        write_response(
            out,
            &GetSelInfoResponse {
                completion_code: COMPLETION_SUCCESS,
                sel_version: SEL_VERSION,
                entry_count: U16::new(count),
                free_space: U16::new(free_bytes),
                last_addition_timestamp: U32::new(last_addition_timestamp),
                last_erase_timestamp: U32::new(self.sel.last_erase_timestamp),
                operation_support: SEL_OPERATION_SUPPORT_RESERVE,
            },
        )
    }

    fn reserve_sel(&mut self, out: &mut [u8; KCS_MESSAGE_MAX]) -> usize {
        self.sel.reservation_id = self.sel.reservation_id.wrapping_add(1);
        if self.sel.reservation_id == 0 {
            self.sel.reservation_id = 1;
        }

        write_response(
            out,
            &ReserveSelResponse {
                completion_code: COMPLETION_SUCCESS,
                reservation_id: U16::new(self.sel.reservation_id),
            },
        )
    }

    fn get_sel_entry(&mut self, data: &[u8], out: &mut [u8; KCS_MESSAGE_MAX]) -> usize {
        let Ok((request, _)) = GetSelEntryRequest::read_from_prefix(data) else {
            return invalid_length(out);
        };

        let offset = usize::from(request.offset);
        if offset >= SEL_RECORD_SIZE {
            return completion(out, COMPLETION_PARAMETER_OUT_OF_RANGE);
        }

        let record_id = request.record_id.get();
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
            .saturating_add(usize::from(request.bytes_to_read))
            .min(SEL_RECORD_SIZE);
        let record = &self.sel.records[index];
        let bytes = &record[offset..end];

        let header_len = write_response(
            out,
            &GetSelEntryResponseHeader {
                completion_code: COMPLETION_SUCCESS,
                next_record_id: U16::new(next_record_id),
            },
        );
        out[header_len..header_len + bytes.len()].copy_from_slice(bytes);
        header_len + bytes.len()
    }

    fn add_sel_entry(&mut self, data: &[u8], out: &mut [u8; KCS_MESSAGE_MAX]) -> usize {
        let Ok((request, _)) = AddSelEntryRequest::read_from_prefix(data) else {
            return invalid_length(out);
        };
        let mut record = request.record;

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
                    SendOutcome::Accepted => {
                        self.stats.forwarded = self.stats.forwarded.saturating_add(1);
                    }
                    SendOutcome::Dropped => {
                        self.stats.sink_dropped = self.stats.sink_dropped.saturating_add(1);
                    }
                }
            } else {
                self.stats.rate_limited = self.stats.rate_limited.saturating_add(1);
            }
        }

        write_response(
            out,
            &AddSelEntryResponse {
                completion_code: COMPLETION_SUCCESS,
                record_id: U16::new(record_id),
            },
        )
    }

    fn clear_sel(&mut self, data: &[u8], out: &mut [u8; KCS_MESSAGE_MAX]) -> usize {
        let Ok((request, _)) = ClearSelRequest::read_from_prefix(data) else {
            return invalid_length(out);
        };

        if request.signature != CLEAR_SEL_SIGNATURE {
            return completion(out, COMPLETION_INVALID_DATA_FIELD);
        }

        match request.operation {
            CLEAR_SEL_INITIATE_ERASE => {
                self.sel.records.clear();
                self.sel.next_record_id = 1;
                let trusted_seconds = self.clock.unix_seconds();
                self.sel.last_erase_timestamp =
                    adjusted_timestamp(trusted_seconds, self.sel.time_offset_seconds);
            }
            CLEAR_SEL_GET_STATUS => {}
            _ => return completion(out, COMPLETION_INVALID_DATA_FIELD),
        }

        write_response(
            out,
            &ClearSelResponse {
                completion_code: COMPLETION_SUCCESS,
                erase_status: CLEAR_SEL_ERASE_COMPLETE,
            },
        )
    }

    fn get_sel_time(&mut self, out: &mut [u8; KCS_MESSAGE_MAX]) -> usize {
        let trusted_seconds = self.clock.unix_seconds();
        write_response(
            out,
            &GetSelTimeResponse {
                completion_code: COMPLETION_SUCCESS,
                timestamp: U32::new(adjusted_timestamp(
                    trusted_seconds,
                    self.sel.time_offset_seconds,
                )),
            },
        )
    }

    fn set_sel_time(&mut self, data: &[u8], out: &mut [u8; KCS_MESSAGE_MAX]) -> usize {
        let Ok((request, _)) = SetSelTimeRequest::read_from_prefix(data) else {
            return invalid_length(out);
        };

        let requested = i64::from(request.timestamp.get());
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
