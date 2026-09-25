// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Pure reconnect broker for the control console.

use crate::control_session_protocol;
use crate::control_session_protocol::Parser;
use crate::control_session_protocol::ParserSnapshot;
use crate::control_session_protocol::ProtocolError;
use crate::control_session_protocol::Record;
use crate::control_session_protocol::RecordType;
use std::collections::VecDeque;
use std::fmt;
use thiserror::Error;

/// Maximum records waiting behind the one current in-flight record.
pub const MAX_QUEUED_RECORDS_PER_LEG: usize = 16;
/// Maximum encoded bytes waiting behind the one current in-flight record.
pub const MAX_QUEUED_BYTES_PER_LEG: usize = 1024 * 1024;

/// The broker's externally meaningful state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum BrokerState {
    AwaitGuestAttach = 1,
    AwaitGuestAck = 2,
    ReadyNoHost = 3,
    Active = 4,
    ResetPending = 5,
    Failed = 6,
}

impl BrokerState {
    fn from_snapshot(value: u8) -> Result<Self, BrokerError> {
        match value {
            1 => Ok(Self::AwaitGuestAttach),
            2 => Ok(Self::AwaitGuestAck),
            3 => Ok(Self::ReadyNoHost),
            4 => Ok(Self::Active),
            5 => Ok(Self::ResetPending),
            6 => Ok(Self::Failed),
            _ => Err(BrokerError::InvalidSnapshot("invalid broker state")),
        }
    }
}

/// An output leg driven by a later physical-I/O adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputLegId {
    Guest,
    Host,
}

/// Result of accepting input into one broker leg.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputProgress {
    pub consumed: usize,
    pub status: InputStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputStatus {
    NeedMore,
    RecordAccepted,
    Backpressured,
}

/// Error counters safe to expose through inspection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BrokerCounters {
    pub protocol_errors: u64,
    pub authentication_errors: u64,
    pub sequence_errors: u64,
    pub ack_errors: u64,
    pub reset_errors: u64,
    pub reconnect_errors: u64,
    pub backpressure_errors: u64,
}

/// Pure broker failures.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum BrokerError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("host capability authentication failed")]
    Authentication,
    #[error("a host attachment already occupies the broker slot")]
    HostSlotBusy,
    #[error("record {record_type:?} is illegal in state {state:?}")]
    IllegalRecord {
        state: BrokerState,
        record_type: RecordType,
    },
    #[error("record instance does not match the broker")]
    InstanceMismatch,
    #[error("record epoch {actual} does not match expected epoch {expected}")]
    EpochMismatch { expected: u64, actual: u64 },
    #[error("record sequence {actual} does not match expected sequence {expected}")]
    SequenceMismatch { expected: u64, actual: u64 },
    #[error("guest acknowledged a session before RESET was emitted")]
    AckBeforeReset,
    #[error(
        "receive credit increment {increment} exceeds charged bytes with {available} available of window {window}"
    )]
    InvalidCredit {
        available: u32,
        increment: u32,
        window: u32,
    },
    #[error("output leg {0:?} is backpressured")]
    Backpressure(OutputLegId),
    #[error("control-session epoch overflow")]
    EpochOverflow,
    #[error("control-session sequence overflow")]
    SequenceOverflow,
    #[error("invalid output progress")]
    InvalidOutputProgress,
    #[error("invalid broker snapshot: {0}")]
    InvalidSnapshot(&'static str),
    #[error("broker is in a failed state")]
    Failed,
}

/// Serializable-neutral state for one partially emitted record.
#[derive(Clone, Eq, PartialEq)]
pub struct EncodedRecordSnapshot {
    pub bytes: Vec<u8>,
    pub offset: usize,
}

impl fmt::Debug for EncodedRecordSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncodedRecordSnapshot")
            .field("bytes_len", &self.bytes.len())
            .field("offset", &self.offset)
            .finish()
    }
}

/// Serializable-neutral state for an output leg.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct OutputSnapshot {
    pub current: Option<EncodedRecordSnapshot>,
    pub queued_records: Vec<Vec<u8>>,
}

impl fmt::Debug for OutputSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let queued_bytes = self
            .queued_records
            .iter()
            .fold(0usize, |total, record| total.saturating_add(record.len()));
        f.debug_struct("OutputSnapshot")
            .field("current", &self.current)
            .field("queued_record_count", &self.queued_records.len())
            .field("queued_bytes", &queued_bytes)
            .finish()
    }
}

/// Serializable-neutral broker state. It intentionally contains no capability
/// or live host-attachment identity.
#[derive(Clone, Eq, PartialEq)]
pub struct BrokerSnapshot {
    pub state: u8,
    pub instance_id: [u8; 16],
    pub drain_foreign_instance_records: bool,
    pub epoch: u64,
    pub guest_parser: ParserSnapshot,
    pub guest_output: OutputSnapshot,
    pub host_output: OutputSnapshot,
    pub guest_receive_sequence: u64,
    pub guest_send_sequence: u64,
    pub host_receive_sequence: u64,
    pub host_send_sequence: u64,
    pub pending_guest_record: Option<Record>,
    pub pending_host_record: Option<Record>,
    pub counters: BrokerCounters,
}

impl fmt::Debug for BrokerSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrokerSnapshot")
            .field("state", &self.state)
            .field("instance_id", &self.instance_id)
            .field(
                "drain_foreign_instance_records",
                &self.drain_foreign_instance_records,
            )
            .field("epoch", &self.epoch)
            .field("guest_parser", &self.guest_parser)
            .field("guest_output", &self.guest_output)
            .field("host_output", &self.host_output)
            .field("guest_receive_sequence", &self.guest_receive_sequence)
            .field("guest_send_sequence", &self.guest_send_sequence)
            .field("host_receive_sequence", &self.host_receive_sequence)
            .field("host_send_sequence", &self.host_send_sequence)
            .field("pending_guest_record", &self.pending_guest_record)
            .field("pending_host_record", &self.pending_host_record)
            .field("counters", &self.counters)
            .finish()
    }
}

#[derive(Clone, Debug)]
struct EncodedRecord {
    bytes: Vec<u8>,
    offset: usize,
}

#[derive(Clone, Debug, Default)]
struct OutputLeg {
    current: Option<EncodedRecord>,
    queue: VecDeque<Vec<u8>>,
    queued_bytes: usize,
}

impl OutputLeg {
    fn can_enqueue(&self, encoded_len: usize) -> bool {
        self.queue.len() < MAX_QUEUED_RECORDS_PER_LEG
            && self
                .queued_bytes
                .checked_add(encoded_len)
                .is_some_and(|total| total <= MAX_QUEUED_BYTES_PER_LEG)
    }

    fn enqueue(&mut self, bytes: Vec<u8>) -> Result<(), BrokerError> {
        if !self.can_enqueue(bytes.len()) {
            return Err(BrokerError::InvalidSnapshot(
                "output queue exceeded configured bound",
            ));
        }
        self.queued_bytes += bytes.len();
        self.queue.push_back(bytes);
        Ok(())
    }

    fn begin(&mut self) -> bool {
        if self.current.is_some() {
            return true;
        }
        let Some(bytes) = self.queue.pop_front() else {
            return false;
        };
        self.queued_bytes -= bytes.len();
        self.current = Some(EncodedRecord { bytes, offset: 0 });
        true
    }

    fn peek(&self, max_bytes: usize) -> Option<&[u8]> {
        let current = self.current.as_ref()?;
        let end = current
            .offset
            .saturating_add(max_bytes)
            .min(current.bytes.len());
        Some(&current.bytes[current.offset..end])
    }

    fn advance(&mut self, count: usize) -> Result<(), BrokerError> {
        let current = self
            .current
            .as_mut()
            .ok_or(BrokerError::InvalidOutputProgress)?;
        let remaining = current.bytes.len() - current.offset;
        if count > remaining {
            return Err(BrokerError::InvalidOutputProgress);
        }
        current.offset += count;
        if current.offset == current.bytes.len() {
            self.current = None;
        }
        Ok(())
    }

    fn discard_unstarted(&mut self) {
        self.queue.clear();
        self.queued_bytes = 0;
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.offset == 0)
        {
            self.current = None;
        }
    }

    fn clear(&mut self) {
        self.current = None;
        self.queue.clear();
        self.queued_bytes = 0;
    }

    fn snapshot(&self) -> OutputSnapshot {
        OutputSnapshot {
            current: self.current.as_ref().map(|current| EncodedRecordSnapshot {
                bytes: current.bytes.clone(),
                offset: current.offset,
            }),
            queued_records: self.queue.iter().cloned().collect(),
        }
    }

    fn restore(snapshot: OutputSnapshot) -> Result<Self, BrokerError> {
        Self::validate_snapshot(&snapshot)?;
        let current = if let Some(current) = snapshot.current {
            Some(EncodedRecord {
                bytes: current.bytes,
                offset: current.offset,
            })
        } else {
            None
        };

        let mut queue = VecDeque::with_capacity(snapshot.queued_records.len());
        let mut queued_bytes = 0usize;
        for bytes in snapshot.queued_records {
            queued_bytes =
                queued_bytes
                    .checked_add(bytes.len())
                    .ok_or(BrokerError::InvalidSnapshot(
                        "queued output byte count overflow",
                    ))?;
            if queued_bytes > MAX_QUEUED_BYTES_PER_LEG {
                return Err(BrokerError::InvalidSnapshot(
                    "queued output bytes exceed configured bound",
                ));
            }
            queue.push_back(bytes);
        }
        Ok(Self {
            current,
            queue,
            queued_bytes,
        })
    }

    fn validate_snapshot(snapshot: &OutputSnapshot) -> Result<(), BrokerError> {
        if let Some(current) = &snapshot.current {
            validate_encoded_output(&current.bytes)?;
            if current.offset >= current.bytes.len() {
                return Err(BrokerError::InvalidSnapshot(
                    "current output offset is out of bounds",
                ));
            }
        }
        if snapshot.queued_records.len() > MAX_QUEUED_RECORDS_PER_LEG {
            return Err(BrokerError::InvalidSnapshot(
                "too many queued output records",
            ));
        }
        let mut queued_bytes = 0usize;
        for bytes in &snapshot.queued_records {
            validate_encoded_output(bytes)?;
            queued_bytes =
                queued_bytes
                    .checked_add(bytes.len())
                    .ok_or(BrokerError::InvalidSnapshot(
                        "queued output byte count overflow",
                    ))?;
            if queued_bytes > MAX_QUEUED_BYTES_PER_LEG {
                return Err(BrokerError::InvalidSnapshot(
                    "queued output bytes exceed configured bound",
                ));
            }
        }
        Ok(())
    }
}

fn validate_encoded_output(bytes: &[u8]) -> Result<(), BrokerError> {
    let record = control_session_protocol::decode_exact(bytes)?;
    if matches!(
        record.record_type,
        RecordType::GuestAttach | RecordType::HostAttach
    ) {
        return Err(BrokerError::InvalidSnapshot(
            "bootstrap record cannot be broker output",
        ));
    }
    Ok(())
}

/// Pure protocol and reconnect state machine.
pub struct ControlSessionBroker {
    instance_id: [u8; 16],
    capability: [u8; 32],
    epoch: u64,
    state: BrokerState,
    guest_parser: Parser,
    host_parser: Parser,
    guest_output: OutputLeg,
    host_output: OutputLeg,
    guest_receive_sequence: u64,
    guest_send_sequence: u64,
    host_receive_sequence: u64,
    host_send_sequence: u64,
    guest_receive_window: u32,
    guest_receive_credit: u32,
    host_connected: bool,
    host_authenticated: bool,
    host_rejected: bool,
    host_bound_epoch: u64,
    pending_guest_record: Option<Record>,
    pending_host_record: Option<Record>,
    drain_foreign_instance_records: bool,
    counters: BrokerCounters,
}

impl ControlSessionBroker {
    pub fn new(instance_id: [u8; 16], capability: [u8; 32]) -> Self {
        Self {
            instance_id,
            capability,
            epoch: 1,
            state: BrokerState::AwaitGuestAttach,
            guest_parser: Parser::new(),
            host_parser: Parser::new(),
            guest_output: OutputLeg::default(),
            host_output: OutputLeg::default(),
            guest_receive_sequence: 0,
            guest_send_sequence: 0,
            host_receive_sequence: 0,
            host_send_sequence: 0,
            guest_receive_window: 0,
            guest_receive_credit: 0,
            host_connected: false,
            host_authenticated: false,
            host_rejected: false,
            host_bound_epoch: 0,
            pending_guest_record: None,
            pending_host_record: None,
            drain_foreign_instance_records: false,
            counters: BrokerCounters::default(),
        }
    }

    pub fn state(&self) -> BrokerState {
        self.state
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "used by protocol tests"))]
    pub fn instance_id(&self) -> [u8; 16] {
        self.instance_id
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn counters(&self) -> &BrokerCounters {
        &self.counters
    }

    pub fn guest_receive_window(&self) -> u32 {
        self.guest_receive_window
    }

    pub fn guest_receive_credit(&self) -> u32 {
        self.guest_receive_credit
    }

    pub fn host_is_authenticated(&self) -> bool {
        self.host_authenticated
    }

    pub fn host_is_connected(&self) -> bool {
        self.host_connected
    }

    pub fn output_record_count(&self, leg: OutputLegId) -> usize {
        let output = self.output(leg);
        output.queue.len() + usize::from(output.current.is_some())
    }

    pub fn output_byte_count(&self, leg: OutputLegId) -> usize {
        let output = self.output(leg);
        output.queued_bytes
            + output
                .current
                .as_ref()
                .map_or(0, |current| current.bytes.len() - current.offset)
    }

    pub fn guest_parser_buffered_bytes(&self) -> usize {
        let snapshot = self.guest_parser.snapshot();
        snapshot.header_count + snapshot.body_bytes.len()
    }

    pub fn has_pending_guest_record(&self) -> bool {
        self.pending_guest_record.is_some()
    }

    pub fn has_pending_host_record(&self) -> bool {
        self.pending_host_record.is_some()
    }

    /// Resets the device-facing protocol state without changing constructor
    /// credentials. A still-open host transport must be detached separately by
    /// the physical adapter before it can attach again.
    pub fn reset_for_device(&mut self) {
        *self = Self::new(self.instance_id, self.capability);
    }

    /// Reserves the single host slot after OS-level identity verification.
    pub fn begin_host_attachment(&mut self) -> Result<(), BrokerError> {
        if self.host_connected {
            increment(&mut self.counters.reconnect_errors);
            return Err(BrokerError::HostSlotBusy);
        }
        if self.state == BrokerState::Failed {
            return Err(BrokerError::Failed);
        }
        self.host_connected = true;
        self.host_authenticated = false;
        self.host_rejected = false;
        self.host_bound_epoch = 0;
        self.host_parser = Parser::new();
        self.host_output.clear();
        self.pending_host_record = None;
        self.host_receive_sequence = 0;
        self.host_send_sequence = 0;
        Ok(())
    }

    pub fn accept_guest_input(&mut self, input: &[u8]) -> Result<InputProgress, BrokerError> {
        if self.state == BrokerState::Failed {
            return Err(BrokerError::Failed);
        }
        if let Some(record) = self.pending_guest_record.take() {
            if let Err(error) = self.handle_guest_record(record.clone()) {
                if error == BrokerError::Backpressure(OutputLegId::Host) {
                    self.pending_guest_record = Some(record);
                    return Ok(InputProgress {
                        consumed: 0,
                        status: InputStatus::Backpressured,
                    });
                }
                return Err(error);
            }
            if input.is_empty() {
                return Ok(InputProgress {
                    consumed: 0,
                    status: InputStatus::RecordAccepted,
                });
            }
        }

        let progress = match self.guest_parser.accept(input) {
            Ok(progress) => progress,
            Err(error) => {
                increment(&mut self.counters.protocol_errors);
                return Err(error.into());
            }
        };
        let Some(record) = progress.record else {
            return Ok(InputProgress {
                consumed: progress.consumed,
                status: InputStatus::NeedMore,
            });
        };
        match self.handle_guest_record(record.clone()) {
            Ok(()) => Ok(InputProgress {
                consumed: progress.consumed,
                status: InputStatus::RecordAccepted,
            }),
            Err(BrokerError::Backpressure(OutputLegId::Host)) => {
                self.pending_guest_record = Some(record);
                Ok(InputProgress {
                    consumed: progress.consumed,
                    status: InputStatus::Backpressured,
                })
            }
            Err(error) => Err(error),
        }
    }

    pub fn accept_host_input(&mut self, input: &[u8]) -> Result<InputProgress, BrokerError> {
        if !self.host_connected || self.host_rejected {
            increment(&mut self.counters.protocol_errors);
            return Err(BrokerError::IllegalRecord {
                state: self.state,
                record_type: RecordType::HostAttach,
            });
        }
        if self.state == BrokerState::Failed {
            return Err(BrokerError::Failed);
        }
        if let Some(record) = self.pending_host_record.take() {
            if let Err(error) = self.handle_host_record(record.clone()) {
                if error == BrokerError::Backpressure(OutputLegId::Guest) {
                    self.pending_host_record = Some(record);
                    return Ok(InputProgress {
                        consumed: 0,
                        status: InputStatus::Backpressured,
                    });
                }
                return Err(error);
            }
            if input.is_empty() {
                return Ok(InputProgress {
                    consumed: 0,
                    status: InputStatus::RecordAccepted,
                });
            }
        }

        let progress = match self.host_parser.accept(input) {
            Ok(progress) => progress,
            Err(error) => {
                increment(&mut self.counters.protocol_errors);
                return Err(error.into());
            }
        };
        let Some(record) = progress.record else {
            return Ok(InputProgress {
                consumed: progress.consumed,
                status: InputStatus::NeedMore,
            });
        };
        match self.handle_host_record(record.clone()) {
            Ok(()) => Ok(InputProgress {
                consumed: progress.consumed,
                status: InputStatus::RecordAccepted,
            }),
            Err(BrokerError::Backpressure(OutputLegId::Guest)) => {
                self.pending_host_record = Some(record);
                Ok(InputProgress {
                    consumed: progress.consumed,
                    status: InputStatus::Backpressured,
                })
            }
            Err(error) => Err(error),
        }
    }

    /// Drops a host attachment. Only loss of an active authenticated host
    /// advances the epoch.
    pub fn host_disconnected(&mut self) -> Result<(), BrokerError> {
        let active_loss =
            self.host_connected && self.host_authenticated && self.state == BrokerState::Active;
        self.host_connected = false;
        self.host_authenticated = false;
        self.host_rejected = false;
        self.host_bound_epoch = 0;
        self.host_parser = Parser::new();
        self.host_output.clear();
        self.pending_host_record = None;
        self.host_receive_sequence = 0;
        self.host_send_sequence = 0;
        if !active_loss {
            return Ok(());
        }

        self.state = BrokerState::ResetPending;
        let Some(next_epoch) = self.epoch.checked_add(1) else {
            increment(&mut self.counters.reset_errors);
            return Err(self.fail(BrokerError::EpochOverflow));
        };
        self.epoch = next_epoch;
        self.guest_output.discard_unstarted();
        self.pending_guest_record = None;
        self.guest_receive_sequence = 0;
        self.guest_send_sequence = 0;
        self.guest_receive_window = 0;
        self.guest_receive_credit = 0;
        self.enqueue_guest_control(RecordType::Reset)?;
        Ok(())
    }

    /// Promotes one complete record to the current physical record.
    pub fn begin_output(&mut self, leg: OutputLegId) -> bool {
        if self.state == BrokerState::Failed {
            return false;
        }
        self.output_mut(leg).begin()
    }

    /// Returns bytes from the current physical record without advancing them.
    pub fn peek_output(&self, leg: OutputLegId, max_bytes: usize) -> Option<&[u8]> {
        if self.state == BrokerState::Failed {
            return None;
        }
        self.output(leg).peek(max_bytes)
    }

    /// Commits bytes reported written by the physical adapter.
    pub fn advance_output(&mut self, leg: OutputLegId, count: usize) -> Result<(), BrokerError> {
        if self.state == BrokerState::Failed {
            return Err(BrokerError::Failed);
        }
        let reset_completed =
            if leg == OutputLegId::Guest && self.state == BrokerState::ResetPending {
                self.guest_output
                    .current
                    .as_ref()
                    .filter(|current| current.bytes.len() - current.offset == count)
                    .map(|current| control_session_protocol::decode_exact(&current.bytes))
                    .transpose()?
                    .is_some_and(|record| {
                        record.record_type == RecordType::Reset
                            && record.instance_id == self.instance_id
                            && record.epoch == self.epoch
                    })
            } else {
                false
            };
        self.output_mut(leg).advance(count)?;
        if reset_completed {
            self.state = BrokerState::AwaitGuestAck;
        }
        Ok(())
    }

    pub fn snapshot(&self) -> BrokerSnapshot {
        let mut guest_output = self.guest_output.snapshot();
        guest_output.queued_records.clear();
        if guest_output
            .current
            .as_ref()
            .is_some_and(|current| current.offset == 0)
        {
            guest_output.current = None;
        }
        BrokerSnapshot {
            state: self.state as u8,
            instance_id: self.instance_id,
            drain_foreign_instance_records: self.drain_foreign_instance_records,
            epoch: self.epoch,
            guest_parser: self.guest_parser.snapshot(),
            guest_output,
            host_output: OutputSnapshot::default(),
            guest_receive_sequence: self.guest_receive_sequence,
            guest_send_sequence: self.guest_send_sequence,
            host_receive_sequence: self.host_receive_sequence,
            host_send_sequence: self.host_send_sequence,
            pending_guest_record: None,
            pending_host_record: None,
            counters: self.counters.clone(),
        }
    }

    /// Restores only physical guest alignment state, then starts a fresh
    /// instance at epoch one and queues RESET.
    pub fn restore(
        snapshot: BrokerSnapshot,
        new_instance_id: [u8; 16],
        new_capability: [u8; 32],
    ) -> Result<Self, BrokerError> {
        Self::validate_restore(&snapshot, new_instance_id)?;
        let guest_parser = Parser::restore(snapshot.guest_parser)?;
        let mut saved_guest_output = OutputLeg::restore(snapshot.guest_output)?;
        let _saved_host_output = OutputLeg::restore(snapshot.host_output)?;

        saved_guest_output.queue.clear();
        saved_guest_output.queued_bytes = 0;
        if saved_guest_output
            .current
            .as_ref()
            .is_some_and(|current| current.offset == 0)
        {
            saved_guest_output.current = None;
        }

        let mut broker = Self::new(new_instance_id, new_capability);
        broker.state = BrokerState::ResetPending;
        broker.guest_parser = guest_parser;
        broker.guest_output = saved_guest_output;
        broker.drain_foreign_instance_records = true;
        broker.counters = snapshot.counters;
        broker.enqueue_guest_control(RecordType::Reset)?;
        Ok(broker)
    }

    pub fn validate_restore(
        snapshot: &BrokerSnapshot,
        new_instance_id: [u8; 16],
    ) -> Result<(), BrokerError> {
        Self::validate_snapshot(snapshot)?;
        if new_instance_id == [0; 16] {
            return Err(BrokerError::InvalidSnapshot("instance ID is zero"));
        }
        if snapshot.instance_id == new_instance_id {
            return Err(BrokerError::InvalidSnapshot(
                "restore reused the saved instance ID",
            ));
        }
        if let Some(current) = &snapshot.guest_output.current
            && current.offset != 0
        {
            let record = control_session_protocol::decode_exact(&current.bytes)?;
            if record.instance_id == [0; 16] || record.instance_id == new_instance_id {
                return Err(BrokerError::InvalidSnapshot(
                    "partial guest output does not belong to an old instance",
                ));
            }
        }
        Ok(())
    }

    /// Validates all snapshot bounds and encoded values without constructing a
    /// broker or allocating output queues.
    pub fn validate_snapshot(snapshot: &BrokerSnapshot) -> Result<(), BrokerError> {
        let _saved_state = BrokerState::from_snapshot(snapshot.state)?;
        if snapshot.epoch == 0 {
            return Err(BrokerError::InvalidSnapshot("saved epoch is zero"));
        }
        if snapshot.instance_id == [0; 16] {
            return Err(BrokerError::InvalidSnapshot("instance ID is zero"));
        }
        validate_pending(snapshot.pending_guest_record.as_ref())?;
        validate_pending(snapshot.pending_host_record.as_ref())?;
        Parser::validate_snapshot(&snapshot.guest_parser)?;
        OutputLeg::validate_snapshot(&snapshot.guest_output)?;
        OutputLeg::validate_snapshot(&snapshot.host_output)?;
        Ok(())
    }

    fn handle_guest_record(&mut self, record: Record) -> Result<(), BrokerError> {
        if self.drain_foreign_instance_records
            && matches!(
                self.state,
                BrokerState::ResetPending | BrokerState::AwaitGuestAck
            )
            && record.instance_id != self.instance_id
        {
            return Ok(());
        }
        match self.state {
            BrokerState::AwaitGuestAttach => {
                if record.record_type != RecordType::GuestAttach {
                    return self.illegal(record.record_type);
                }
                self.enqueue_guest_control(RecordType::Reset)?;
                self.state = BrokerState::ResetPending;
                Ok(())
            }
            BrokerState::AwaitGuestAck | BrokerState::ResetPending => {
                self.handle_guest_while_awaiting_ack(record)
            }
            BrokerState::ReadyNoHost => {
                if record.record_type == RecordType::Ack {
                    increment(&mut self.counters.ack_errors);
                }
                self.illegal(record.record_type)
            }
            BrokerState::Active => {
                if record.record_type == RecordType::Ack {
                    increment(&mut self.counters.ack_errors);
                    return self.illegal(record.record_type);
                }
                if record.record_type == RecordType::Credit {
                    self.validate_current_guest_record(&record)?;
                    let credit_increment =
                        control_session_protocol::decode_credit_payload(&record.payload)?;
                    let Some(available) = self.guest_receive_credit.checked_add(credit_increment)
                    else {
                        increment(&mut self.counters.protocol_errors);
                        return Err(BrokerError::InvalidCredit {
                            available: self.guest_receive_credit,
                            increment: credit_increment,
                            window: self.guest_receive_window,
                        });
                    };
                    if available > self.guest_receive_window {
                        increment(&mut self.counters.protocol_errors);
                        return Err(BrokerError::InvalidCredit {
                            available: self.guest_receive_credit,
                            increment: credit_increment,
                            window: self.guest_receive_window,
                        });
                    }
                    let next_guest_receive =
                        self.checked_next_sequence(self.guest_receive_sequence)?;
                    self.guest_receive_credit = available;
                    self.guest_receive_sequence = next_guest_receive;
                    return Ok(());
                }
                if record.record_type != RecordType::Data {
                    return self.illegal(record.record_type);
                }
                self.validate_current_guest_record(&record)?;
                let encoded = self.make_host_data(&record.payload)?;
                if !self.host_output.can_enqueue(encoded.len()) {
                    increment(&mut self.counters.backpressure_errors);
                    return Err(BrokerError::Backpressure(OutputLegId::Host));
                }
                let next_guest_receive = self.checked_next_sequence(self.guest_receive_sequence)?;
                let next_host_send = self.checked_next_sequence(self.host_send_sequence)?;
                self.host_output.enqueue(encoded)?;
                self.guest_receive_sequence = next_guest_receive;
                self.host_send_sequence = next_host_send;
                Ok(())
            }
            BrokerState::Failed => Err(BrokerError::Failed),
        }
    }

    fn handle_guest_while_awaiting_ack(&mut self, record: Record) -> Result<(), BrokerError> {
        if self.state == BrokerState::ResetPending
            && record.record_type == RecordType::Ack
            && record.instance_id == self.instance_id
            && record.epoch == self.epoch
        {
            increment(&mut self.counters.ack_errors);
            return Err(BrokerError::AckBeforeReset);
        }
        if record.record_type != RecordType::Ack {
            if record.instance_id == self.instance_id && record.epoch < self.epoch {
                return Ok(());
            }
            return self.illegal(record.record_type);
        }
        if record.instance_id != self.instance_id {
            increment(&mut self.counters.ack_errors);
            return Err(BrokerError::InstanceMismatch);
        }
        if record.epoch != self.epoch {
            increment(&mut self.counters.ack_errors);
            return Err(BrokerError::EpochMismatch {
                expected: self.epoch,
                actual: record.epoch,
            });
        }
        if record.sequence != self.guest_receive_sequence {
            increment(&mut self.counters.ack_errors);
            increment(&mut self.counters.sequence_errors);
            return Err(BrokerError::SequenceMismatch {
                expected: self.guest_receive_sequence,
                actual: record.sequence,
            });
        }
        let initial_credit = control_session_protocol::decode_credit_payload(&record.payload)?;
        let next_guest_receive = self.checked_next_sequence(self.guest_receive_sequence)?;
        if self.host_authenticated {
            self.enqueue_host_control(RecordType::Ready, Vec::new())?;
            self.state = BrokerState::Active;
        } else {
            self.state = BrokerState::ReadyNoHost;
        }
        self.guest_receive_sequence = next_guest_receive;
        self.guest_receive_window = initial_credit;
        self.guest_receive_credit = initial_credit;
        self.drain_foreign_instance_records = false;
        Ok(())
    }

    fn handle_host_record(&mut self, record: Record) -> Result<(), BrokerError> {
        if !self.host_authenticated {
            if record.record_type != RecordType::HostAttach {
                return self.illegal(record.record_type);
            }
            if !constant_time_eq_32(&self.capability, &record.payload) {
                increment(&mut self.counters.authentication_errors);
                self.host_rejected = true;
                // Authentication rejection is close-only; the adapter detaches the host.
                return Err(BrokerError::Authentication);
            }
            self.host_authenticated = true;
            self.host_bound_epoch = self.epoch;
            self.host_receive_sequence = 0;
            self.host_send_sequence = 0;
            self.host_output.clear();
            match self.state {
                BrokerState::ReadyNoHost => {
                    self.enqueue_host_control(RecordType::Ready, Vec::new())?;
                    self.state = BrokerState::Active;
                }
                BrokerState::AwaitGuestAttach
                | BrokerState::AwaitGuestAck
                | BrokerState::ResetPending => {
                    self.enqueue_host_control(RecordType::Wait, Vec::new())?;
                }
                BrokerState::Active => {
                    increment(&mut self.counters.reconnect_errors);
                    return Err(BrokerError::HostSlotBusy);
                }
                BrokerState::Failed => return Err(BrokerError::Failed),
            }
            return Ok(());
        }

        if self.state != BrokerState::Active || record.record_type != RecordType::Data {
            return self.illegal(record.record_type);
        }
        if record.instance_id != self.instance_id {
            increment(&mut self.counters.protocol_errors);
            return Err(BrokerError::InstanceMismatch);
        }
        if record.epoch != self.host_bound_epoch || record.epoch != self.epoch {
            increment(&mut self.counters.protocol_errors);
            return Err(BrokerError::EpochMismatch {
                expected: self.epoch,
                actual: record.epoch,
            });
        }
        if record.sequence != self.host_receive_sequence {
            increment(&mut self.counters.sequence_errors);
            return Err(BrokerError::SequenceMismatch {
                expected: self.host_receive_sequence,
                actual: record.sequence,
            });
        }
        let payload_len =
            u32::try_from(record.payload.len()).map_err(|_| ProtocolError::LengthOverflow)?;
        if payload_len > self.guest_receive_credit {
            increment(&mut self.counters.backpressure_errors);
            return Err(BrokerError::Backpressure(OutputLegId::Guest));
        }
        let encoded = self.make_guest_data(&record.payload)?;
        if !self.guest_output.can_enqueue(encoded.len()) {
            increment(&mut self.counters.backpressure_errors);
            return Err(BrokerError::Backpressure(OutputLegId::Guest));
        }
        let next_host_receive = self.checked_next_sequence(self.host_receive_sequence)?;
        let next_guest_send = self.checked_next_sequence(self.guest_send_sequence)?;
        self.guest_output.enqueue(encoded)?;
        self.guest_receive_credit -= payload_len;
        self.host_receive_sequence = next_host_receive;
        self.guest_send_sequence = next_guest_send;
        Ok(())
    }

    fn validate_current_guest_record(&mut self, record: &Record) -> Result<(), BrokerError> {
        if record.instance_id != self.instance_id {
            increment(&mut self.counters.protocol_errors);
            return Err(BrokerError::InstanceMismatch);
        }
        if record.epoch != self.epoch {
            increment(&mut self.counters.protocol_errors);
            return Err(BrokerError::EpochMismatch {
                expected: self.epoch,
                actual: record.epoch,
            });
        }
        if record.sequence != self.guest_receive_sequence {
            increment(&mut self.counters.sequence_errors);
            return Err(BrokerError::SequenceMismatch {
                expected: self.guest_receive_sequence,
                actual: record.sequence,
            });
        }
        Ok(())
    }

    fn enqueue_guest_control(&mut self, record_type: RecordType) -> Result<(), BrokerError> {
        let record = Record::session(
            record_type,
            self.instance_id,
            self.epoch,
            self.guest_send_sequence,
            Vec::new(),
        );
        let encoded = control_session_protocol::encode(&record)?;
        if !self.guest_output.can_enqueue(encoded.len()) {
            increment(&mut self.counters.backpressure_errors);
            return Err(BrokerError::Backpressure(OutputLegId::Guest));
        }
        let next_guest_send = self.checked_next_sequence(self.guest_send_sequence)?;
        self.guest_output.enqueue(encoded)?;
        self.guest_send_sequence = next_guest_send;
        Ok(())
    }

    fn enqueue_host_control(
        &mut self,
        record_type: RecordType,
        payload: Vec<u8>,
    ) -> Result<(), BrokerError> {
        let record = Record::session(
            record_type,
            self.instance_id,
            self.epoch,
            self.host_send_sequence,
            payload,
        );
        let encoded = control_session_protocol::encode(&record)?;
        if !self.host_output.can_enqueue(encoded.len()) {
            increment(&mut self.counters.backpressure_errors);
            return Err(BrokerError::Backpressure(OutputLegId::Host));
        }
        let next_host_send = self.checked_next_sequence(self.host_send_sequence)?;
        self.host_output.enqueue(encoded)?;
        self.host_send_sequence = next_host_send;
        Ok(())
    }

    fn make_guest_data(&self, payload: &[u8]) -> Result<Vec<u8>, BrokerError> {
        Ok(control_session_protocol::encode(&Record::session(
            RecordType::Data,
            self.instance_id,
            self.epoch,
            self.guest_send_sequence,
            payload.to_vec(),
        ))?)
    }

    fn make_host_data(&self, payload: &[u8]) -> Result<Vec<u8>, BrokerError> {
        Ok(control_session_protocol::encode(&Record::session(
            RecordType::Data,
            self.instance_id,
            self.epoch,
            self.host_send_sequence,
            payload.to_vec(),
        ))?)
    }

    fn checked_next_sequence(&mut self, sequence: u64) -> Result<u64, BrokerError> {
        sequence
            .checked_add(1)
            .ok_or(BrokerError::SequenceOverflow)
            .map_err(|error| self.fail(error))
    }

    fn fail(&mut self, error: BrokerError) -> BrokerError {
        self.state = BrokerState::Failed;
        self.guest_parser = Parser::new();
        self.host_parser = Parser::new();
        self.guest_output.clear();
        self.host_output.clear();
        self.pending_guest_record = None;
        self.pending_host_record = None;
        error
    }

    fn illegal<T>(&mut self, record_type: RecordType) -> Result<T, BrokerError> {
        increment(&mut self.counters.protocol_errors);
        Err(BrokerError::IllegalRecord {
            state: self.state,
            record_type,
        })
    }

    fn output(&self, leg: OutputLegId) -> &OutputLeg {
        match leg {
            OutputLegId::Guest => &self.guest_output,
            OutputLegId::Host => &self.host_output,
        }
    }

    fn output_mut(&mut self, leg: OutputLegId) -> &mut OutputLeg {
        match leg {
            OutputLegId::Guest => &mut self.guest_output,
            OutputLegId::Host => &mut self.host_output,
        }
    }
}

fn validate_pending(record: Option<&Record>) -> Result<(), BrokerError> {
    if let Some(record) = record {
        control_session_protocol::encode(record)?;
    }
    Ok(())
}

fn increment(counter: &mut u64) {
    *counter = counter.saturating_add(1);
}

fn constant_time_eq_32(expected: &[u8; 32], actual: &[u8]) -> bool {
    let mut difference = actual.len() ^ expected.len();
    for (index, expected_byte) in expected.iter().enumerate() {
        let actual_byte = actual.get(index).copied().unwrap_or(0);
        difference |= usize::from(*expected_byte ^ actual_byte);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    const INSTANCE: [u8; 16] = [0x22; 16];
    const CAPABILITY: [u8; 32] = [0x33; 32];

    fn feed_guest(
        broker: &mut ControlSessionBroker,
        record: &Record,
    ) -> Result<InputProgress, BrokerError> {
        let bytes = control_session_protocol::encode(record)?;
        broker.accept_guest_input(&bytes)
    }

    fn feed_host(
        broker: &mut ControlSessionBroker,
        record: &Record,
    ) -> Result<InputProgress, BrokerError> {
        let bytes = control_session_protocol::encode(record)?;
        broker.accept_host_input(&bytes)
    }

    fn drain_record(
        broker: &mut ControlSessionBroker,
        leg: OutputLegId,
    ) -> Result<Record, BrokerError> {
        assert!(broker.begin_output(leg));
        let bytes = broker
            .peek_output(leg, usize::MAX)
            .ok_or(BrokerError::InvalidOutputProgress)?
            .to_vec();
        broker.advance_output(leg, bytes.len())?;
        Ok(control_session_protocol::decode_exact(&bytes)?)
    }

    fn ack(broker: &ControlSessionBroker) -> Record {
        ack_with_credit(broker, control_session_protocol::MIN_RECEIVE_CREDIT)
    }

    fn ack_with_credit(broker: &ControlSessionBroker, credit: u32) -> Record {
        Record::session(
            RecordType::Ack,
            broker.instance_id(),
            broker.epoch(),
            0,
            control_session_protocol::credit_payload(credit),
        )
    }

    fn make_active() -> Result<ControlSessionBroker, BrokerError> {
        make_active_with_window(control_session_protocol::MIN_RECEIVE_CREDIT)
    }

    fn make_active_with_window(window: u32) -> Result<ControlSessionBroker, BrokerError> {
        let mut broker = ControlSessionBroker::new(INSTANCE, CAPABILITY);
        feed_guest(
            &mut broker,
            &Record::bootstrap(RecordType::GuestAttach, Vec::new()),
        )?;
        assert_eq!(
            drain_record(&mut broker, OutputLegId::Guest)?.record_type,
            RecordType::Reset
        );
        let guest_ack = ack_with_credit(&broker, window);
        feed_guest(&mut broker, &guest_ack)?;
        broker.begin_host_attachment()?;
        feed_host(
            &mut broker,
            &Record::bootstrap(RecordType::HostAttach, CAPABILITY.to_vec()),
        )?;
        assert_eq!(
            drain_record(&mut broker, OutputLegId::Host)?.record_type,
            RecordType::Ready
        );
        assert_eq!(broker.state(), BrokerState::Active);
        Ok(broker)
    }

    #[test]
    fn cold_boot_authentication_and_independent_sequences() -> Result<(), BrokerError> {
        let mut broker = ControlSessionBroker::new(INSTANCE, CAPABILITY);
        feed_guest(
            &mut broker,
            &Record::bootstrap(RecordType::GuestAttach, Vec::new()),
        )?;
        let reset = drain_record(&mut broker, OutputLegId::Guest)?;
        assert_eq!((reset.epoch, reset.sequence), (1, 0));

        broker.begin_host_attachment()?;
        feed_host(
            &mut broker,
            &Record::bootstrap(RecordType::HostAttach, CAPABILITY.to_vec()),
        )?;
        let wait = drain_record(&mut broker, OutputLegId::Host)?;
        assert_eq!((wait.record_type, wait.sequence), (RecordType::Wait, 0));

        let guest_ack = ack(&broker);
        feed_guest(&mut broker, &guest_ack)?;
        let ready = drain_record(&mut broker, OutputLegId::Host)?;
        assert_eq!((ready.record_type, ready.sequence), (RecordType::Ready, 1));

        feed_guest(
            &mut broker,
            &Record::session(RecordType::Data, INSTANCE, 1, 1, b"guest".to_vec()),
        )?;
        feed_host(
            &mut broker,
            &Record::session(RecordType::Data, INSTANCE, 1, 0, b"host".to_vec()),
        )?;
        let to_host = drain_record(&mut broker, OutputLegId::Host)?;
        let to_guest = drain_record(&mut broker, OutputLegId::Guest)?;
        assert_eq!((to_host.sequence, to_host.payload), (2, b"guest".to_vec()));
        assert_eq!((to_guest.sequence, to_guest.payload), (1, b"host".to_vec()));
        Ok(())
    }

    #[test]
    fn wrong_capability_fails_without_advancing_epoch() -> Result<(), BrokerError> {
        let mut broker = ControlSessionBroker::new(INSTANCE, CAPABILITY);
        broker.begin_host_attachment()?;
        let error = feed_host(
            &mut broker,
            &Record::bootstrap(RecordType::HostAttach, vec![0x44; 32]),
        )
        .expect_err("wrong capability must fail");
        assert_eq!(error, BrokerError::Authentication);
        assert_eq!(broker.epoch(), 1);
        assert!(!broker.host_is_authenticated());
        assert_eq!(broker.output_record_count(OutputLegId::Host), 0);
        assert_eq!(broker.counters().authentication_errors, 1);
        Ok(())
    }

    #[test]
    fn stale_future_wrong_and_duplicate_acks_are_rejected() -> Result<(), BrokerError> {
        let mut broker = ControlSessionBroker::new(INSTANCE, CAPABILITY);
        feed_guest(
            &mut broker,
            &Record::bootstrap(RecordType::GuestAttach, Vec::new()),
        )?;
        let early_ack = ack(&broker);
        assert_eq!(
            feed_guest(&mut broker, &early_ack),
            Err(BrokerError::AckBeforeReset)
        );
        assert_eq!(
            drain_record(&mut broker, OutputLegId::Guest)?.record_type,
            RecordType::Reset
        );
        let wrong_instance = Record::session(
            RecordType::Ack,
            [9; 16],
            1,
            0,
            control_session_protocol::credit_payload(control_session_protocol::MIN_RECEIVE_CREDIT),
        );
        assert_eq!(
            feed_guest(&mut broker, &wrong_instance),
            Err(BrokerError::InstanceMismatch)
        );
        let stale = Record::session(
            RecordType::Ack,
            INSTANCE,
            0,
            0,
            control_session_protocol::credit_payload(control_session_protocol::MIN_RECEIVE_CREDIT),
        );
        assert!(matches!(
            feed_guest(&mut broker, &stale),
            Err(BrokerError::EpochMismatch { actual: 0, .. })
        ));
        let future = Record::session(
            RecordType::Ack,
            INSTANCE,
            2,
            0,
            control_session_protocol::credit_payload(control_session_protocol::MIN_RECEIVE_CREDIT),
        );
        assert!(matches!(
            feed_guest(&mut broker, &future),
            Err(BrokerError::EpochMismatch { actual: 2, .. })
        ));
        let wrong_sequence = Record::session(
            RecordType::Ack,
            INSTANCE,
            1,
            1,
            control_session_protocol::credit_payload(control_session_protocol::MIN_RECEIVE_CREDIT),
        );
        assert!(matches!(
            feed_guest(&mut broker, &wrong_sequence),
            Err(BrokerError::SequenceMismatch { actual: 1, .. })
        ));
        let guest_ack = ack(&broker);
        feed_guest(&mut broker, &guest_ack)?;
        assert!(
            feed_guest(
                &mut broker,
                &Record::session(
                    RecordType::Ack,
                    INSTANCE,
                    1,
                    0,
                    control_session_protocol::credit_payload(
                        control_session_protocol::MIN_RECEIVE_CREDIT,
                    ),
                )
            )
            .is_err()
        );
        assert_eq!(broker.counters().ack_errors, 6);
        Ok(())
    }

    #[test]
    fn ready_without_host_rejects_data() -> Result<(), BrokerError> {
        let mut broker = ControlSessionBroker::new(INSTANCE, CAPABILITY);
        feed_guest(
            &mut broker,
            &Record::bootstrap(RecordType::GuestAttach, Vec::new()),
        )?;
        assert_eq!(
            drain_record(&mut broker, OutputLegId::Guest)?.record_type,
            RecordType::Reset
        );
        let guest_ack = ack(&broker);
        feed_guest(&mut broker, &guest_ack)?;
        assert_eq!(broker.state(), BrokerState::ReadyNoHost);
        assert!(
            feed_guest(
                &mut broker,
                &Record::session(RecordType::Data, INSTANCE, 1, 1, vec![1])
            )
            .is_err()
        );
        assert!(broker.pending_guest_record.is_none());
        Ok(())
    }

    #[test]
    fn partial_output_disconnect_finishes_record_then_reset() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        for (sequence, payload) in [(0, b"first".as_slice()), (1, b"second".as_slice())] {
            feed_host(
                &mut broker,
                &Record::session(RecordType::Data, INSTANCE, 1, sequence, payload.to_vec()),
            )?;
        }
        assert!(broker.begin_output(OutputLegId::Guest));
        let first_len = broker
            .peek_output(OutputLegId::Guest, usize::MAX)
            .ok_or(BrokerError::InvalidOutputProgress)?
            .len();
        broker.advance_output(OutputLegId::Guest, first_len - 1)?;
        broker.host_disconnected()?;
        assert_eq!(broker.state(), BrokerState::ResetPending);
        assert_eq!(broker.epoch(), 2);

        let remainder = broker
            .peek_output(OutputLegId::Guest, usize::MAX)
            .ok_or(BrokerError::InvalidOutputProgress)?
            .len();
        assert_eq!(remainder, 1);
        broker.advance_output(OutputLegId::Guest, remainder)?;
        let reset = drain_record(&mut broker, OutputLegId::Guest)?;
        assert_eq!(
            (reset.record_type, reset.epoch, reset.sequence),
            (RecordType::Reset, 2, 0)
        );
        assert!(!broker.begin_output(OutputLegId::Guest));
        Ok(())
    }

    #[test]
    fn disconnect_after_last_byte_drops_unstarted_next_record() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        for sequence in 0..2 {
            feed_host(
                &mut broker,
                &Record::session(
                    RecordType::Data,
                    INSTANCE,
                    1,
                    sequence,
                    vec![sequence as u8],
                ),
            )?;
        }
        let _first = drain_record(&mut broker, OutputLegId::Guest)?;
        broker.host_disconnected()?;
        let next = drain_record(&mut broker, OutputLegId::Guest)?;
        assert_eq!(next.record_type, RecordType::Reset);
        Ok(())
    }

    #[test]
    fn disconnect_at_every_host_record_offset_forwards_no_partial_data() -> Result<(), BrokerError>
    {
        let encoded = control_session_protocol::encode(&Record::session(
            RecordType::Data,
            INSTANCE,
            1,
            0,
            b"body-with-NVXS".to_vec(),
        ))?;
        for split in 0..encoded.len() {
            let mut broker = make_active()?;
            let progress = broker.accept_host_input(&encoded[..split])?;
            assert_eq!(progress.status, InputStatus::NeedMore);
            broker.host_disconnected()?;
            let output = drain_record(&mut broker, OutputLegId::Guest)?;
            assert_eq!(output.record_type, RecordType::Reset, "split {split}");
            assert!(!broker.begin_output(OutputLegId::Guest), "split {split}");
        }
        Ok(())
    }

    #[test]
    fn rapid_reconnect_waits_without_another_epoch() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        broker.host_disconnected()?;
        assert_eq!(broker.epoch(), 2);
        broker.begin_host_attachment()?;
        feed_host(
            &mut broker,
            &Record::bootstrap(RecordType::HostAttach, CAPABILITY.to_vec()),
        )?;
        assert_eq!(
            drain_record(&mut broker, OutputLegId::Host)?.record_type,
            RecordType::Wait
        );
        assert_eq!(broker.epoch(), 2);
        assert_eq!(
            broker.begin_host_attachment(),
            Err(BrokerError::HostSlotBusy)
        );

        assert_eq!(
            drain_record(&mut broker, OutputLegId::Guest)?.record_type,
            RecordType::Reset
        );
        let current_ack = Record::session(
            RecordType::Ack,
            INSTANCE,
            2,
            0,
            control_session_protocol::credit_payload(control_session_protocol::MIN_RECEIVE_CREDIT),
        );
        feed_guest(&mut broker, &current_ack)?;
        assert_eq!(broker.state(), BrokerState::Active);
        assert_eq!(
            drain_record(&mut broker, OutputLegId::Host)?.record_type,
            RecordType::Ready
        );
        Ok(())
    }

    #[test]
    fn bounded_queue_returns_backpressure_without_data_loss() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        let payload = vec![0x5a; control_session_protocol::MAX_DATA_LEN];
        let mut sequence = 0;
        loop {
            let progress = feed_guest(
                &mut broker,
                &Record::session(RecordType::Data, INSTANCE, 1, sequence + 1, payload.clone()),
            )?;
            if progress.status == InputStatus::Backpressured {
                break;
            }
            sequence += 1;
            assert!(sequence <= MAX_QUEUED_RECORDS_PER_LEG as u64);
        }
        assert!(broker.pending_guest_record.is_some());
        assert_eq!(broker.counters().backpressure_errors, 1);
        assert!(broker.output_byte_count(OutputLegId::Host) <= MAX_QUEUED_BYTES_PER_LEG);
        let _ = drain_record(&mut broker, OutputLegId::Host)?;
        let progress = broker.accept_guest_input(&[])?;
        assert_eq!(progress.status, InputStatus::RecordAccepted);
        assert!(broker.pending_guest_record.is_none());
        Ok(())
    }

    #[test]
    fn queue_limits_exclude_the_current_in_flight_record() -> Result<(), BrokerError> {
        let mut records = OutputLeg::default();
        for _ in 0..MAX_QUEUED_RECORDS_PER_LEG {
            records.enqueue(vec![0])?;
        }
        assert!(records.begin());
        assert!(records.can_enqueue(1));
        records.enqueue(vec![0])?;
        assert_eq!(records.queue.len(), MAX_QUEUED_RECORDS_PER_LEG);
        assert!(records.current.is_some());
        assert!(!records.can_enqueue(1));

        let mut bytes = OutputLeg::default();
        bytes.enqueue(vec![0])?;
        assert!(bytes.begin());
        assert!(bytes.can_enqueue(MAX_QUEUED_BYTES_PER_LEG));
        bytes.enqueue(vec![0; MAX_QUEUED_BYTES_PER_LEG])?;
        assert_eq!(bytes.queued_bytes, MAX_QUEUED_BYTES_PER_LEG);
        assert!(bytes.current.is_some());
        assert!(!bytes.can_enqueue(1));
        Ok(())
    }

    #[test]
    fn credit_gates_complete_records_and_releases_atomically() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        let first_payload = vec![0x5a; control_session_protocol::MAX_DATA_LEN];
        feed_host(
            &mut broker,
            &Record::session(RecordType::Data, INSTANCE, 1, 0, first_payload.clone()),
        )?;
        assert_eq!(broker.guest_receive_credit(), 0);
        assert_eq!(
            feed_host(
                &mut broker,
                &Record::session(RecordType::Data, INSTANCE, 1, 1, vec![0x44])
            )?
            .status,
            InputStatus::Backpressured
        );
        assert_eq!(
            drain_record(&mut broker, OutputLegId::Guest)?.payload,
            first_payload
        );
        assert_eq!(
            broker.accept_host_input(&[])?.status,
            InputStatus::Backpressured
        );

        feed_guest(
            &mut broker,
            &Record::session(
                RecordType::Credit,
                INSTANCE,
                1,
                1,
                control_session_protocol::credit_payload(1),
            ),
        )?;
        assert_eq!(
            broker.accept_host_input(&[])?.status,
            InputStatus::RecordAccepted
        );
        assert_eq!(broker.guest_receive_credit(), 0);
        assert_eq!(
            drain_record(&mut broker, OutputLegId::Guest)?.payload,
            vec![0x44]
        );
        Ok(())
    }

    #[test]
    fn queue_backpressure_does_not_spend_pending_record_credit() -> Result<(), BrokerError> {
        let mut broker = make_active_with_window(control_session_protocol::MAX_RECEIVE_CREDIT)?;
        let payload = vec![0x5a; control_session_protocol::MAX_DATA_LEN];
        let mut accepted = 0u32;
        loop {
            let progress = feed_host(
                &mut broker,
                &Record::session(
                    RecordType::Data,
                    INSTANCE,
                    1,
                    accepted as u64,
                    payload.clone(),
                ),
            )?;
            if progress.status == InputStatus::Backpressured {
                break;
            }
            accepted += 1;
        }
        let credit_before_retry = broker.guest_receive_credit();
        assert_eq!(
            credit_before_retry,
            control_session_protocol::MAX_RECEIVE_CREDIT
                - accepted * control_session_protocol::MIN_RECEIVE_CREDIT
        );
        assert_eq!(
            broker.accept_host_input(&[])?.status,
            InputStatus::Backpressured
        );
        assert_eq!(broker.guest_receive_credit(), credit_before_retry);

        let _ = drain_record(&mut broker, OutputLegId::Guest)?;
        assert_eq!(
            broker.accept_host_input(&[])?.status,
            InputStatus::RecordAccepted
        );
        assert_eq!(
            broker.guest_receive_credit(),
            credit_before_retry - control_session_protocol::MIN_RECEIVE_CREDIT
        );
        Ok(())
    }

    #[test]
    fn invalid_duplicate_and_overflowing_credits_do_not_mint_capacity() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        let credit = Record::session(
            RecordType::Credit,
            INSTANCE,
            1,
            1,
            control_session_protocol::credit_payload(1),
        );
        assert!(matches!(
            feed_guest(&mut broker, &credit),
            Err(BrokerError::InvalidCredit { .. })
        ));
        feed_host(
            &mut broker,
            &Record::session(
                RecordType::Data,
                INSTANCE,
                1,
                0,
                vec![0; control_session_protocol::MAX_DATA_LEN],
            ),
        )?;
        feed_guest(&mut broker, &credit)?;
        assert_eq!(broker.guest_receive_credit(), 1);
        assert!(matches!(
            feed_guest(&mut broker, &credit),
            Err(BrokerError::SequenceMismatch { .. })
        ));
        assert_eq!(broker.guest_receive_credit(), 1);

        assert!(matches!(
            feed_guest(
                &mut broker,
                &Record::session(
                    RecordType::Credit,
                    INSTANCE,
                    2,
                    2,
                    control_session_protocol::credit_payload(1),
                )
            ),
            Err(BrokerError::EpochMismatch { .. })
        ));
        assert_eq!(broker.guest_receive_credit(), 1);

        broker.guest_receive_sequence = u64::MAX;
        let before = broker.guest_receive_credit();
        assert_eq!(
            feed_guest(
                &mut broker,
                &Record::session(
                    RecordType::Credit,
                    INSTANCE,
                    1,
                    u64::MAX,
                    control_session_protocol::credit_payload(1),
                )
            ),
            Err(BrokerError::SequenceOverflow)
        );
        assert_eq!(broker.guest_receive_credit(), before);
        Ok(())
    }

    #[test]
    fn zero_credit_does_not_block_disconnect_or_fresh_ack() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        feed_host(
            &mut broker,
            &Record::session(
                RecordType::Data,
                INSTANCE,
                1,
                0,
                vec![0x6a; control_session_protocol::MAX_DATA_LEN],
            ),
        )?;
        let _ = drain_record(&mut broker, OutputLegId::Guest)?;
        assert_eq!(
            feed_host(
                &mut broker,
                &Record::session(RecordType::Data, INSTANCE, 1, 1, vec![1])
            )?
            .status,
            InputStatus::Backpressured
        );

        broker.host_disconnected()?;
        assert!(!broker.has_pending_host_record());
        assert_eq!(
            (broker.guest_receive_window(), broker.guest_receive_credit()),
            (0, 0)
        );
        feed_guest(
            &mut broker,
            &Record::session(
                RecordType::Credit,
                INSTANCE,
                1,
                1,
                control_session_protocol::credit_payload(
                    control_session_protocol::MIN_RECEIVE_CREDIT,
                ),
            ),
        )?;
        assert_eq!(
            (broker.guest_receive_window(), broker.guest_receive_credit()),
            (0, 0)
        );
        assert_eq!(
            drain_record(&mut broker, OutputLegId::Guest)?.record_type,
            RecordType::Reset
        );

        broker.begin_host_attachment()?;
        feed_host(
            &mut broker,
            &Record::bootstrap(RecordType::HostAttach, CAPABILITY.to_vec()),
        )?;
        assert_eq!(
            drain_record(&mut broker, OutputLegId::Host)?.record_type,
            RecordType::Wait
        );
        feed_guest(
            &mut broker,
            &Record::session(
                RecordType::Ack,
                INSTANCE,
                2,
                0,
                control_session_protocol::credit_payload(
                    control_session_protocol::MIN_RECEIVE_CREDIT,
                ),
            ),
        )?;
        assert_eq!(broker.state(), BrokerState::Active);
        assert_eq!(
            broker.guest_receive_credit(),
            control_session_protocol::MIN_RECEIVE_CREDIT
        );
        Ok(())
    }

    #[test]
    fn disconnect_preserves_each_started_record_prefix_only() -> Result<(), BrokerError> {
        let payload = b"body-with-NVXS".to_vec();
        for split in 0..control_session_protocol::HEADER_LEN + payload.len() {
            let mut broker = make_active()?;
            feed_host(
                &mut broker,
                &Record::session(RecordType::Data, INSTANCE, 1, 0, payload.clone()),
            )?;
            feed_host(
                &mut broker,
                &Record::session(RecordType::Data, INSTANCE, 1, 1, b"drop".to_vec()),
            )?;
            assert!(broker.begin_output(OutputLegId::Guest));
            let encoded = broker
                .peek_output(OutputLegId::Guest, usize::MAX)
                .ok_or(BrokerError::InvalidOutputProgress)?
                .to_vec();
            broker.advance_output(OutputLegId::Guest, split)?;
            broker.host_disconnected()?;
            if split != 0 {
                assert_eq!(
                    broker
                        .peek_output(OutputLegId::Guest, usize::MAX)
                        .ok_or(BrokerError::InvalidOutputProgress)?,
                    &encoded[split..]
                );
                broker.advance_output(OutputLegId::Guest, encoded.len() - split)?;
            }
            assert_eq!(
                drain_record(&mut broker, OutputLegId::Guest)?.record_type,
                RecordType::Reset
            );
            assert!(!broker.begin_output(OutputLegId::Guest));
        }
        Ok(())
    }

    #[test]
    fn reverse_backpressure_cannot_block_disconnect_recovery() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        let payload = vec![0x5a; control_session_protocol::MAX_DATA_LEN];
        let mut sequence = 1;
        loop {
            let progress = feed_guest(
                &mut broker,
                &Record::session(RecordType::Data, INSTANCE, 1, sequence, payload.clone()),
            )?;
            if progress.status == InputStatus::Backpressured {
                break;
            }
            sequence += 1;
        }
        assert!(broker.has_pending_guest_record());
        broker.host_disconnected()?;
        assert!(!broker.has_pending_guest_record());
        assert_eq!(broker.output_record_count(OutputLegId::Host), 0);
        assert_eq!(
            drain_record(&mut broker, OutputLegId::Guest)?.record_type,
            RecordType::Reset
        );
        Ok(())
    }

    #[test]
    fn streams_maximum_ttrpc_message_with_bounded_credit() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        let mut remaining = control_session_protocol::MAX_RECEIVE_CREDIT as usize;
        let mut host_sequence = 0;
        let mut guest_sequence = 1;
        let mut offset = 0usize;
        while remaining != 0 {
            let length = remaining.min(control_session_protocol::MAX_DATA_LEN);
            let payload = (offset..offset + length)
                .map(|index| (index % 251) as u8)
                .collect::<Vec<_>>();
            feed_host(
                &mut broker,
                &Record::session(
                    RecordType::Data,
                    INSTANCE,
                    1,
                    host_sequence,
                    payload.clone(),
                ),
            )?;
            assert_eq!(
                drain_record(&mut broker, OutputLegId::Guest)?.payload,
                payload
            );
            feed_guest(
                &mut broker,
                &Record::session(
                    RecordType::Credit,
                    INSTANCE,
                    1,
                    guest_sequence,
                    control_session_protocol::credit_payload(length as u32),
                ),
            )?;
            host_sequence += 1;
            guest_sequence += 1;
            offset += length;
            remaining -= length;
        }
        assert_eq!(
            offset,
            control_session_protocol::MAX_RECEIVE_CREDIT as usize
        );
        assert_eq!(
            broker.guest_receive_credit(),
            control_session_protocol::MIN_RECEIVE_CREDIT
        );
        Ok(())
    }

    #[test]
    fn epoch_overflow_fails_closed() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        feed_host(
            &mut broker,
            &Record::session(RecordType::Data, INSTANCE, 1, 0, b"queued".to_vec()),
        )?;
        assert!(broker.output_record_count(OutputLegId::Guest) != 0);
        broker.epoch = u64::MAX;
        assert_eq!(broker.host_disconnected(), Err(BrokerError::EpochOverflow));
        assert_eq!(broker.state(), BrokerState::Failed);
        assert_eq!(broker.counters().reset_errors, 1);
        assert_eq!(broker.output_record_count(OutputLegId::Guest), 0);
        assert_eq!(broker.output_record_count(OutputLegId::Host), 0);
        assert!(!broker.begin_output(OutputLegId::Guest));
        assert_eq!(broker.peek_output(OutputLegId::Guest, 1), None);
        assert_eq!(
            broker.advance_output(OutputLegId::Guest, 0),
            Err(BrokerError::Failed)
        );
        Ok(())
    }

    #[test]
    fn sequence_overflow_discards_enqueued_output() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        broker.guest_send_sequence = u64::MAX;
        assert_eq!(
            feed_host(
                &mut broker,
                &Record::session(RecordType::Data, INSTANCE, 1, 0, b"queued".to_vec())
            ),
            Err(BrokerError::SequenceOverflow)
        );
        assert_eq!(broker.state(), BrokerState::Failed);
        assert_eq!(broker.output_record_count(OutputLegId::Guest), 0);
        assert_eq!(broker.output_record_count(OutputLegId::Host), 0);
        assert!(!broker.begin_output(OutputLegId::Guest));
        Ok(())
    }

    #[test]
    fn restore_preserves_only_partial_guest_physical_records() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        feed_host(
            &mut broker,
            &Record::session(RecordType::Data, INSTANCE, 1, 0, b"outbound".to_vec()),
        )?;
        assert!(broker.begin_output(OutputLegId::Guest));
        broker.advance_output(OutputLegId::Guest, 7)?;

        let old_guest_record =
            Record::session(RecordType::Data, INSTANCE, 1, 1, b"inbound".to_vec());
        let old_bytes = control_session_protocol::encode(&old_guest_record)?;
        let split = control_session_protocol::HEADER_LEN + 2;
        let progress = broker.accept_guest_input(&old_bytes[..split])?;
        assert_eq!(progress.status, InputStatus::NeedMore);

        let snapshot = broker.snapshot();
        let new_instance = [0x77; 16];
        let new_capability = [0x88; 32];
        let mut restored = ControlSessionBroker::restore(snapshot, new_instance, new_capability)?;
        assert_eq!(restored.state(), BrokerState::ResetPending);
        assert_eq!(restored.epoch(), 1);
        assert_eq!(restored.instance_id(), new_instance);
        assert!(!restored.host_is_authenticated());

        let progress = restored.accept_guest_input(&old_bytes[split..])?;
        assert_eq!(progress.status, InputStatus::RecordAccepted);
        let remainder = restored
            .peek_output(OutputLegId::Guest, usize::MAX)
            .ok_or(BrokerError::InvalidOutputProgress)?
            .len();
        restored.advance_output(OutputLegId::Guest, remainder)?;
        let reset = drain_record(&mut restored, OutputLegId::Guest)?;
        assert_eq!(
            (reset.record_type, reset.instance_id, reset.epoch),
            (RecordType::Reset, new_instance, 1)
        );
        feed_guest(
            &mut restored,
            &Record::session(
                RecordType::Ack,
                new_instance,
                1,
                0,
                control_session_protocol::credit_payload(
                    control_session_protocol::MIN_RECEIVE_CREDIT,
                ),
            ),
        )?;
        assert_eq!(restored.state(), BrokerState::ReadyNoHost);
        assert!(
            feed_guest(
                &mut restored,
                &Record::session(RecordType::Data, INSTANCE, 1, 4, vec![1])
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn restore_keeps_alignment_but_requires_fresh_credit() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        feed_host(
            &mut broker,
            &Record::session(
                RecordType::Data,
                INSTANCE,
                1,
                0,
                b"partially-emitted".to_vec(),
            ),
        )?;
        assert!(broker.begin_output(OutputLegId::Guest));
        broker.advance_output(OutputLegId::Guest, 7)?;
        assert!(broker.guest_receive_credit() < control_session_protocol::MIN_RECEIVE_CREDIT);

        let snapshot = broker.snapshot();
        let new_instance = [0x77; 16];
        let mut restored = ControlSessionBroker::restore(snapshot, new_instance, [0x88; 32])?;
        assert_eq!(
            (
                restored.guest_receive_window(),
                restored.guest_receive_credit()
            ),
            (0, 0)
        );

        let remainder = restored
            .peek_output(OutputLegId::Guest, usize::MAX)
            .ok_or(BrokerError::InvalidOutputProgress)?
            .len();
        restored.advance_output(OutputLegId::Guest, remainder)?;
        assert_eq!(
            drain_record(&mut restored, OutputLegId::Guest)?.record_type,
            RecordType::Reset
        );
        feed_guest(
            &mut restored,
            &Record::session(
                RecordType::Ack,
                new_instance,
                1,
                0,
                control_session_protocol::credit_payload(
                    control_session_protocol::MIN_RECEIVE_CREDIT,
                ),
            ),
        )?;
        assert_eq!(
            restored.guest_receive_credit(),
            control_session_protocol::MIN_RECEIVE_CREDIT
        );
        Ok(())
    }

    #[test]
    fn invalid_snapshot_fields_are_rejected() {
        let broker = ControlSessionBroker::new(INSTANCE, CAPABILITY);
        let mut snapshot = broker.snapshot();
        snapshot.state = 99;
        assert!(ControlSessionBroker::restore(snapshot, [1; 16], [2; 32]).is_err());

        let mut snapshot = broker.snapshot();
        snapshot.guest_output.current = Some(EncodedRecordSnapshot {
            bytes: vec![1, 2, 3],
            offset: 4,
        });
        assert!(ControlSessionBroker::restore(snapshot, [1; 16], [2; 32]).is_err());
    }

    #[test]
    fn snapshot_debug_redacts_encoded_payloads() {
        let current_secret = (0x80..0xa0).collect::<Vec<_>>();
        let queued_secret = (0xa0..0xc0).collect::<Vec<_>>();
        let pending_secret = (0xc0..0xe0).collect::<Vec<_>>();
        let output = OutputSnapshot {
            current: Some(EncodedRecordSnapshot {
                bytes: current_secret.clone(),
                offset: 7,
            }),
            queued_records: vec![queued_secret.clone()],
        };
        let mut snapshot = ControlSessionBroker::new(INSTANCE, CAPABILITY).snapshot();
        snapshot.guest_output = output.clone();
        snapshot.pending_guest_record = Some(Record::session(
            RecordType::Data,
            INSTANCE,
            1,
            0,
            pending_secret.clone(),
        ));

        let debug_outputs = [
            format!("{:?}", &output.current),
            format!("{output:?}"),
            format!("{snapshot:?}"),
        ];
        for debug in &debug_outputs {
            for secret in [&current_secret, &queued_secret, &pending_secret] {
                assert!(!debug.contains(&format!("{secret:?}")));
            }
        }
        assert!(debug_outputs[0].contains("bytes_len: 32"));
        assert!(debug_outputs[0].contains("offset: 7"));
        assert!(debug_outputs[1].contains("queued_record_count: 1"));
        assert!(debug_outputs[1].contains("queued_bytes: 32"));
        assert!(debug_outputs[2].contains("payload_len: 32"));
    }

    #[test]
    fn restore_drains_multiple_old_instance_records_and_ack() -> Result<(), BrokerError> {
        let broker = make_active()?;
        let snapshot = broker.snapshot();
        let new_instance = [0x77; 16];
        let mut restored = ControlSessionBroker::restore(snapshot, new_instance, [0x88; 32])?;

        for (record_type, sequence) in [
            (RecordType::Data, 1),
            (RecordType::Data, 2),
            (RecordType::Ack, 3),
        ] {
            let payload = if record_type == RecordType::Data {
                vec![sequence as u8]
            } else {
                control_session_protocol::credit_payload(
                    control_session_protocol::MIN_RECEIVE_CREDIT,
                )
            };
            feed_guest(
                &mut restored,
                &Record::session(record_type, INSTANCE, 1, sequence, payload),
            )?;
        }

        assert_eq!(
            drain_record(&mut restored, OutputLegId::Guest)?.record_type,
            RecordType::Reset
        );
        feed_guest(
            &mut restored,
            &Record::session(
                RecordType::Ack,
                new_instance,
                1,
                0,
                control_session_protocol::credit_payload(
                    control_session_protocol::MIN_RECEIVE_CREDIT,
                ),
            ),
        )?;
        assert_eq!(restored.state(), BrokerState::ReadyNoHost);
        Ok(())
    }

    #[test]
    fn restore_finishes_old_reset_before_new_reset() -> Result<(), BrokerError> {
        let mut broker = ControlSessionBroker::new(INSTANCE, CAPABILITY);
        feed_guest(
            &mut broker,
            &Record::bootstrap(RecordType::GuestAttach, Vec::new()),
        )?;
        assert!(broker.begin_output(OutputLegId::Guest));
        broker.advance_output(OutputLegId::Guest, 7)?;

        let new_instance = [0x77; 16];
        let mut restored =
            ControlSessionBroker::restore(broker.snapshot(), new_instance, [0x88; 32])?;
        let old_remainder = restored
            .peek_output(OutputLegId::Guest, usize::MAX)
            .ok_or(BrokerError::InvalidOutputProgress)?
            .len();
        restored.advance_output(OutputLegId::Guest, old_remainder)?;
        feed_guest(
            &mut restored,
            &Record::session(
                RecordType::Ack,
                INSTANCE,
                1,
                0,
                control_session_protocol::credit_payload(
                    control_session_protocol::MIN_RECEIVE_CREDIT,
                ),
            ),
        )?;
        let reset = drain_record(&mut restored, OutputLegId::Guest)?;
        assert_eq!(
            (reset.record_type, reset.instance_id, reset.epoch),
            (RecordType::Reset, new_instance, 1)
        );
        feed_guest(
            &mut restored,
            &Record::session(
                RecordType::Ack,
                new_instance,
                1,
                0,
                control_session_protocol::credit_payload(
                    control_session_protocol::MIN_RECEIVE_CREDIT,
                ),
            ),
        )?;
        assert_eq!(restored.state(), BrokerState::ReadyNoHost);
        Ok(())
    }

    #[test]
    fn repeated_completed_restores_do_not_accumulate_identity_state() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        for index in 0..8u8 {
            let new_instance = [0x80 + index; 16];
            broker =
                ControlSessionBroker::restore(broker.snapshot(), new_instance, [0x40 + index; 32])?;
            assert_eq!(
                drain_record(&mut broker, OutputLegId::Guest)?.record_type,
                RecordType::Reset
            );
            feed_guest(
                &mut broker,
                &Record::session(
                    RecordType::Ack,
                    new_instance,
                    1,
                    0,
                    control_session_protocol::credit_payload(
                        control_session_protocol::MIN_RECEIVE_CREDIT,
                    ),
                ),
            )?;
            assert_eq!(broker.state(), BrokerState::ReadyNoHost);
            assert!(!broker.drain_foreign_instance_records);
        }
        Ok(())
    }

    #[test]
    fn device_reset_discards_attachment_and_protocol_progress() -> Result<(), BrokerError> {
        let mut broker = make_active()?;
        feed_guest(
            &mut broker,
            &Record::session(RecordType::Data, INSTANCE, 1, 1, b"queued".to_vec()),
        )?;
        assert!(broker.output_record_count(OutputLegId::Host) != 0);

        broker.reset_for_device();

        assert_eq!(broker.state(), BrokerState::AwaitGuestAttach);
        assert_eq!(broker.epoch(), 1);
        assert!(!broker.host_is_connected());
        assert!(!broker.host_is_authenticated());
        assert_eq!(broker.output_record_count(OutputLegId::Guest), 0);
        assert_eq!(broker.output_record_count(OutputLegId::Host), 0);
        assert_eq!(broker.guest_parser_buffered_bytes(), 0);
        Ok(())
    }
}
