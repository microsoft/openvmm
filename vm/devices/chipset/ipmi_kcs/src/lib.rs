// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A transport-independent virtual IPMI BMC with a byte-oriented KCS interface.
//!
//! This crate implements the KCS register state machine and a bounded System
//! Event Log (SEL). Platform adapters are intentionally separate: callers map
//! [`IpmiKcs::read_data`], [`IpmiKcs::read_status`],
//! [`IpmiKcs::write_data`], and [`IpmiKcs::write_command`] onto their chosen
//! PIO or MMIO transport.

#![forbid(unsafe_code)]

mod protocol;
mod save_restore;
mod sel;

use sel::RateLimiter;
use sel::SelState;

/// Maximum KCS request or response size.
pub const KCS_MESSAGE_MAX: usize = 64;

/// KCS output-buffer-full status bit.
pub const STATUS_OBF: u8 = 0x01;
/// KCS input-buffer-full status bit.
pub const STATUS_IBF: u8 = 0x02;
/// KCS system-management-software-attention status bit.
pub const STATUS_SMS_ATN: u8 = 0x04;
/// KCS command/data status bit.
pub const STATUS_CD: u8 = 0x08;
/// KCS state field mask.
pub const STATUS_STATE_MASK: u8 = 0xc0;

/// KCS idle state.
pub const KCS_STATE_IDLE: u8 = 0x00;
/// KCS read state.
pub const KCS_STATE_READ: u8 = 0x40;
/// KCS write state.
pub const KCS_STATE_WRITE: u8 = 0x80;
/// KCS error state.
pub const KCS_STATE_ERROR: u8 = 0xc0;

/// KCS Get Status/Abort command.
pub const KCS_COMMAND_GET_STATUS_ABORT: u8 = 0x60;
/// KCS Write Start command.
pub const KCS_COMMAND_WRITE_START: u8 = 0x61;
/// KCS Write End command.
pub const KCS_COMMAND_WRITE_END: u8 = 0x62;
/// KCS Read Next control byte.
pub const KCS_DATA_READ_NEXT: u8 = 0x68;

/// Trusted wall-clock source used for SEL timestamps and rate limiting.
///
/// Implementations must return UTC seconds since the Unix epoch. Negative
/// values are accepted so that startup and test clocks can be represented
/// without lossy conversion.
pub trait TrustedClock {
    /// Returns the current trusted host time in Unix seconds.
    fn unix_seconds(&mut self) -> i64;
}

/// Result of a nonblocking SEL event-forwarding attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelEventDisposition {
    /// The event was accepted by the sink.
    Accepted,
    /// The sink dropped the event without blocking the virtual BMC.
    Dropped,
}

/// Best-effort, nonblocking sink for finalized SEL records.
pub trait SelEventSink {
    /// Attempts to forward one committed SEL record.
    fn try_send(&mut self, record_id: u16, record: [u8; 16]) -> SelEventDisposition;
}

/// Lifetime diagnostic counters for SEL additions.
///
/// These counters are intentionally excluded from saved state.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SelStats {
    /// Records successfully committed to the SEL.
    pub committed: u64,
    /// Committed records accepted by the configured event sink.
    pub forwarded: u64,
    /// Committed records not forwarded because the per-second budget was exhausted.
    pub rate_limited: u64,
    /// Committed records rejected by the configured event sink.
    pub sink_dropped: u64,
}

#[derive(Clone)]
struct KcsTransaction {
    status: u8,
    data_out: u8,
    request: [u8; KCS_MESSAGE_MAX],
    request_len: usize,
    response: [u8; KCS_MESSAGE_MAX],
    response_len: usize,
    response_pos: usize,
    write_end_pending: bool,
}

impl Default for KcsTransaction {
    fn default() -> Self {
        Self {
            status: KCS_STATE_IDLE,
            data_out: 0,
            request: [0; KCS_MESSAGE_MAX],
            request_len: 0,
            response: [0; KCS_MESSAGE_MAX],
            response_len: 0,
            response_pos: 0,
            write_end_pending: false,
        }
    }
}

impl KcsTransaction {
    fn state(&self) -> u8 {
        self.status & STATUS_STATE_MASK
    }

    fn set_state(&mut self, state: u8) {
        self.status = (self.status & !STATUS_STATE_MASK) | state;
    }

    fn stage_dummy(&mut self) {
        self.data_out = 0;
        self.status |= STATUS_OBF;
    }

    fn clear_buffers(&mut self) {
        self.request.fill(0);
        self.request_len = 0;
        self.response.fill(0);
        self.response_len = 0;
        self.response_pos = 0;
        self.write_end_pending = false;
    }
}

/// A minimal virtual IPMI BMC exposed through byte-oriented KCS registers.
pub struct IpmiKcs {
    transaction: KcsTransaction,
    sel: SelState,
    clock: Box<dyn TrustedClock>,
    sink: Option<Box<dyn SelEventSink>>,
    rate_limiter: RateLimiter,
    stats: SelStats,
}

impl IpmiKcs {
    /// Creates a virtual BMC without a SEL event sink.
    pub fn new(clock: impl TrustedClock + 'static) -> Self {
        Self::from_boxed_parts(Box::new(clock), None)
    }

    /// Creates a virtual BMC with a best-effort SEL event sink.
    pub fn with_event_sink(
        clock: impl TrustedClock + 'static,
        sink: impl SelEventSink + 'static,
    ) -> Self {
        Self::from_boxed_parts(Box::new(clock), Some(Box::new(sink)))
    }

    fn from_boxed_parts(clock: Box<dyn TrustedClock>, sink: Option<Box<dyn SelEventSink>>) -> Self {
        Self {
            transaction: KcsTransaction::default(),
            sel: SelState::new(),
            clock,
            sink,
            rate_limiter: RateLimiter::default(),
            stats: SelStats::default(),
        }
    }

    /// Replaces or removes the nonblocking SEL event sink.
    pub fn set_event_sink(&mut self, sink: Option<Box<dyn SelEventSink>>) {
        self.sink = sink;
    }

    /// Reads the KCS data register and clears OBF.
    pub fn read_data(&mut self) -> u8 {
        let value = self.transaction.data_out;
        self.transaction.status &= !STATUS_OBF;
        value
    }

    /// Reads the KCS status register without changing device state.
    pub fn read_status(&self) -> u8 {
        self.transaction.status
    }

    /// Writes one byte to the KCS command register.
    pub fn write_command(&mut self, command: u8) {
        self.transaction.status |= STATUS_IBF | STATUS_CD;

        match command {
            KCS_COMMAND_WRITE_START => {
                self.transaction.request.fill(0);
                self.transaction.request_len = 0;
                self.transaction.write_end_pending = false;
                self.transaction.set_state(KCS_STATE_WRITE);
                self.transaction.stage_dummy();
            }
            KCS_COMMAND_WRITE_END => {
                self.transaction.write_end_pending = true;
                self.transaction.set_state(KCS_STATE_WRITE);
                self.transaction.stage_dummy();
            }
            KCS_COMMAND_GET_STATUS_ABORT => self.handle_abort(),
            KCS_DATA_READ_NEXT => {}
            _ => self.handle_abort(),
        }

        self.transaction.status &= !STATUS_IBF;
    }

    /// Writes one byte to the KCS data register.
    pub fn write_data(&mut self, byte: u8) {
        self.transaction.status |= STATUS_IBF;
        self.transaction.status &= !STATUS_CD;

        match self.transaction.state() {
            KCS_STATE_WRITE => self.handle_write_data(byte),
            KCS_STATE_READ if byte == KCS_DATA_READ_NEXT => self.handle_read_next(),
            KCS_STATE_READ => self.enter_error_state(),
            KCS_STATE_IDLE | KCS_STATE_ERROR => {}
            _ => {}
        }

        self.transaction.status &= !STATUS_IBF;
    }

    /// Resets volatile KCS transaction state while preserving SEL and BMC time.
    pub fn reset(&mut self) {
        self.transaction = KcsTransaction::default();
    }

    /// Returns current SEL diagnostic counters.
    pub fn stats(&self) -> SelStats {
        self.stats
    }

    /// Returns the number of records currently stored in the SEL.
    pub fn sel_len(&self) -> usize {
        self.sel.records.len()
    }

    /// Returns a stored SEL record by insertion index.
    pub fn sel_record(&self, index: usize) -> Option<&[u8; 16]> {
        self.sel.records.get(index)
    }

    /// Returns the guest-selected signed SEL time offset in seconds.
    pub fn sel_time_offset_seconds(&self) -> i64 {
        self.sel.time_offset_seconds
    }

    fn handle_write_data(&mut self, byte: u8) {
        if self.transaction.request_len < KCS_MESSAGE_MAX {
            self.transaction.request[self.transaction.request_len] = byte;
            self.transaction.request_len += 1;
        }

        if self.transaction.write_end_pending {
            self.transaction.write_end_pending = false;
            self.process_ipmi_message();
        } else {
            self.transaction.stage_dummy();
        }
    }

    fn handle_read_next(&mut self) {
        if self.transaction.response_pos < self.transaction.response_len {
            self.transaction.data_out = self.transaction.response[self.transaction.response_pos];
            self.transaction.response_pos += 1;
            self.transaction.status |= STATUS_OBF;
        } else {
            self.transaction.stage_dummy();
            self.transaction.set_state(KCS_STATE_IDLE);
        }
    }

    fn handle_abort(&mut self) {
        self.transaction.clear_buffers();
        self.transaction.response[0] = 0xff;
        self.transaction.response_len = 1;
        self.transaction.response_pos = 0;
        self.transaction.data_out = 0;
        self.transaction.status |= STATUS_OBF;
        self.transaction.set_state(KCS_STATE_READ);
    }

    fn enter_error_state(&mut self) {
        self.transaction.clear_buffers();
        self.transaction.data_out = 0xff;
        self.transaction.status |= STATUS_OBF;
        self.transaction.set_state(KCS_STATE_ERROR);
    }
}

#[cfg(test)]
mod tests;
