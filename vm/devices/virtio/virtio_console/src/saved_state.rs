// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Save and restore of the device-private virtio-console state.
//!
//! The worker state outlives queue restarts so that the TX progress of the
//! current descriptor, staged host input, and the control-session broker can be
//! captured once both queues are stopped. This module defines the versioned
//! saved-state format ([`SavedState`]), validates it before restore, and
//! implements save, restore, input quiescing, and reset of that state.

use crate::BUF_SIZE;
use crate::VirtioConsoleDevice;
use crate::broker::HostTransportState;
use crate::control_session_broker;
use crate::control_session_broker::BrokerCounters;
use crate::control_session_broker::BrokerSnapshot;
use crate::control_session_broker::EncodedRecordSnapshot;
use crate::control_session_broker::OutputSnapshot;
use crate::control_session_protocol;
use crate::control_session_protocol::ParserSnapshot;
use crate::control_session_protocol::Record;
use crate::direct::ConsoleWorkerMode;
use crate::spec::VirtioConsoleConfig;
use guestmem::GuestMemory;
use mesh::payload::Protobuf;
use virtio::VirtioQueue;
use virtio::device::saved_state::DeviceQueueState;
use virtio::device::saved_state::DeviceStateValidator;
use virtio::device::saved_state::restored_queue_front_readable_length;
use virtio::spec::VirtioDeviceFeatures;
use virtio_resources::console::attachment::VirtioConsoleDisconnectPolicy;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;
use vmcore::save_restore::SavedStateRoot;

/// Maximum accepted, not-yet-delivered host input. The direct worker stages at
/// most one [`BUF_SIZE`] read.
pub(crate) const MAX_STAGED_RX_BYTES: usize = BUF_SIZE;
pub(crate) const MAX_SAVED_STATE_BYTES: usize = 512 * 1024;
pub(crate) const DIRECT_SAVED_STATE_VERSION: u32 = 1;
pub(crate) const BROKER_SAVED_STATE_VERSION: u32 = 2;
#[cfg(test)]
pub(crate) const SAVED_STATE_VERSION: u32 = DIRECT_SAVED_STATE_VERSION;

impl VirtioConsoleDevice {
    /// Resets the private state that persists across queue restarts.
    pub(crate) fn reset_private_state(&mut self) {
        self.config = VirtioConsoleConfig::default();
        let (worker, mut state) = self.worker.get_mut();
        let state = state.as_mut().unwrap();
        state.partial_transmit = 0;
        state.staged_rx.clear();
        state.input_gated = false;
        state.mem = GuestMemory::empty();
        if let ConsoleWorkerMode::Broker(mode) = &mut worker.mode {
            mode.reset_for_device();
        }
    }

    /// Stops or resumes accepting new host input, preserving device and queue
    /// state.
    pub(crate) async fn set_input_gated(&mut self, input_gated: bool) -> anyhow::Result<()> {
        self.worker.stop().await;
        let state = self.worker.state_mut().unwrap();
        state.input_gated = input_gated;
        if state.receiveq.is_some() || state.transmitq.is_some() {
            self.worker.start();
        }
        Ok(())
    }

    pub(crate) fn save_private_state(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        let (worker, state) = self.worker.get();
        let state = state.as_ref().unwrap();
        if state.receiveq.is_some() || state.transmitq.is_some() {
            return Err(SaveError::Other(anyhow::anyhow!(
                "virtio-console queues are still running"
            )));
        }
        if state.staged_rx.len() > MAX_STAGED_RX_BYTES {
            return Err(SaveError::InvalidChildSavedState(anyhow::anyhow!(
                "virtio-console staged RX exceeds its ABI bound"
            )));
        }
        let (schema_version, staged_rx, disconnect_policy_id, broker) = match &worker.mode {
            ConsoleWorkerMode::Direct {
                disconnect_policy, ..
            } => (
                DIRECT_SAVED_STATE_VERSION,
                state.staged_rx.iter().copied().collect(),
                disconnect_policy_id(*disconnect_policy),
                None,
            ),
            ConsoleWorkerMode::Broker(mode) => {
                if !state.staged_rx.is_empty() {
                    return Err(SaveError::InvalidChildSavedState(anyhow::anyhow!(
                        "control-console broker has direct-mode staged RX"
                    )));
                }
                let broker = SavedBrokerSnapshot::from(mode.broker.snapshot());
                (BROKER_SAVED_STATE_VERSION, Vec::new(), 0, Some(broker))
            }
        };
        Ok(Some(SavedStateBlob::new(SavedState {
            schema_version,
            columns: self.config.cols.into(),
            rows: self.config.rows.into(),
            partial_transmit: state.partial_transmit as u64,
            staged_rx,
            disconnect_policy_id,
            broker,
        })))
    }

    pub(crate) fn restore_private_state(
        &mut self,
        state: Option<SavedStateBlob>,
    ) -> Result<(), RestoreError> {
        let (worker, runtime) = self.worker.get_mut();
        let mut saved = validate_saved_state(state.as_ref(), worker.mode.validation_mode())?;
        let runtime = runtime.ok_or_else(|| {
            RestoreError::Other(anyhow::anyhow!(
                "virtio-console worker state is unavailable"
            ))
        })?;
        if runtime.receiveq.is_some() || runtime.transmitq.is_some() {
            return Err(RestoreError::Other(anyhow::anyhow!(
                "cannot restore a running virtio-console"
            )));
        }
        let columns = u16::try_from(saved.columns)
            .map_err(|_| invalid_saved_state("console column count is out of range"))?;
        let rows = u16::try_from(saved.rows)
            .map_err(|_| invalid_saved_state("console row count is out of range"))?;
        let partial_transmit = usize::try_from(saved.partial_transmit)
            .map_err(|_| invalid_saved_state("console TX offset is out of range"))?;

        let restored_broker = match &mut worker.mode {
            ConsoleWorkerMode::Direct { .. } => None,
            ConsoleWorkerMode::Broker(mode) => {
                let saved_broker = saved
                    .broker
                    .take()
                    .ok_or_else(|| invalid_saved_state("missing control-console broker state"))?;
                let snapshot = saved_broker.try_into().map_err(invalid_saved_state)?;
                let broker = control_session_broker::ControlSessionBroker::restore(
                    snapshot,
                    mode.config.instance_id,
                    mode.config.capability,
                )
                .map_err(|error| invalid_saved_state(error.to_string()))?;
                if mode.host_io.is_connected() {
                    mode.host_io.disconnect_current().map_err(|error| {
                        RestoreError::Other(
                            anyhow::Error::new(error)
                                .context("failed to disconnect control-console host for restore"),
                        )
                    })?;
                }
                Some(broker)
            }
        };

        self.config = VirtioConsoleConfig {
            cols: columns,
            rows,
        };
        runtime.partial_transmit = partial_transmit;
        runtime.staged_rx = saved.staged_rx.into();
        if let (ConsoleWorkerMode::Broker(mode), Some(broker)) = (&mut worker.mode, restored_broker)
        {
            mode.broker = broker;
            mode.host_input.clear();
            mode.transport_state = HostTransportState::WaitingForConnect;
            mode.auth_deadline = None;
        }
        Ok(())
    }

    pub(crate) fn private_state_validator(&self) -> DeviceStateValidator {
        let mode = self.worker.get().0.mode.validation_mode();
        Box::new(move |state, features, queues, guest_memory| {
            let saved = validate_saved_state(state, mode)?;
            validate_saved_tx_offset(saved.partial_transmit, *features, queues, guest_memory)?;
            Ok(())
        })
    }
}

/// Checks a restored TX offset against the current descriptor when the
/// transmit queue (`idx` 1) is started.
pub(crate) fn check_restored_tx_offset(
    idx: u16,
    partial_transmit: usize,
    queue: &mut VirtioQueue,
) -> anyhow::Result<()> {
    if idx == 1 && partial_transmit != 0 {
        let work = queue
            .try_peek()
            .map_err(|error| anyhow::anyhow!(error).context("invalid restored TX queue"))?
            .ok_or_else(|| anyhow::anyhow!("restored TX offset has no current descriptor"))?;
        anyhow::ensure!(
            partial_transmit <= work.readable_length() as usize,
            "restored TX offset {} exceeds descriptor length {}",
            partial_transmit,
            work.readable_length()
        );
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum SavedStateMode {
    Direct(VirtioConsoleDisconnectPolicy),
    Broker { current_instance_id: [u8; 16] },
}

impl ConsoleWorkerMode {
    fn validation_mode(&self) -> SavedStateMode {
        match self {
            Self::Direct {
                disconnect_policy, ..
            } => SavedStateMode::Direct(*disconnect_policy),
            Self::Broker(mode) => SavedStateMode::Broker {
                current_instance_id: mode.config.instance_id,
            },
        }
    }
}

fn disconnect_policy_id(policy: VirtioConsoleDisconnectPolicy) -> u32 {
    match policy {
        VirtioConsoleDisconnectPolicy::Discard => 0,
        VirtioConsoleDisconnectPolicy::Retain => 1,
    }
}

fn invalid_saved_state(message: impl Into<String>) -> RestoreError {
    RestoreError::InvalidSavedState(anyhow::anyhow!(message.into()))
}

fn validate_saved_state(
    state: Option<&SavedStateBlob>,
    mode: SavedStateMode,
) -> Result<SavedState, RestoreError> {
    let state = state.ok_or_else(|| invalid_saved_state("missing console private state"))?;
    if state.encoded_len() > MAX_SAVED_STATE_BYTES {
        return Err(invalid_saved_state(
            "console private state exceeds its encoded size bound",
        ));
    }
    let saved: SavedState = state.parse()?;
    match mode {
        SavedStateMode::Direct(disconnect_policy) => {
            if saved.schema_version != DIRECT_SAVED_STATE_VERSION {
                return Err(invalid_saved_state(format!(
                    "direct console requires schema version {DIRECT_SAVED_STATE_VERSION}, got {}",
                    saved.schema_version
                )));
            }
            if saved.broker.is_some() {
                return Err(invalid_saved_state(
                    "direct console saved state contains broker state",
                ));
            }
            if saved.disconnect_policy_id != disconnect_policy_id(disconnect_policy) {
                return Err(invalid_saved_state(
                    "console disconnect policy does not match the saved policy",
                ));
            }
        }
        SavedStateMode::Broker {
            current_instance_id,
        } => {
            if saved.schema_version != BROKER_SAVED_STATE_VERSION {
                return Err(invalid_saved_state(format!(
                    "control console requires schema version {BROKER_SAVED_STATE_VERSION}, got {}",
                    saved.schema_version
                )));
            }
            if !saved.staged_rx.is_empty() {
                return Err(invalid_saved_state(
                    "control-console saved state contains direct-mode staged RX",
                ));
            }
            let broker = saved
                .broker
                .as_ref()
                .ok_or_else(|| invalid_saved_state("missing control-console broker state"))?;
            let snapshot = validate_saved_broker(broker)?;
            control_session_broker::ControlSessionBroker::validate_restore(
                &snapshot,
                current_instance_id,
            )
            .map_err(|error| invalid_saved_state(error.to_string()))?;
        }
    }
    u16::try_from(saved.columns)
        .map_err(|_| invalid_saved_state("console column count is out of range"))?;
    u16::try_from(saved.rows)
        .map_err(|_| invalid_saved_state("console row count is out of range"))?;
    usize::try_from(saved.partial_transmit)
        .map_err(|_| invalid_saved_state("console TX offset is out of range"))?;
    if saved.staged_rx.len() > MAX_STAGED_RX_BYTES {
        return Err(invalid_saved_state(
            "console staged RX exceeds its ABI bound",
        ));
    }
    Ok(saved)
}

fn validate_saved_tx_offset(
    partial_transmit: u64,
    features: VirtioDeviceFeatures,
    queues: &[DeviceQueueState],
    guest_memory: &GuestMemory,
) -> Result<(), RestoreError> {
    if partial_transmit == 0 {
        return Ok(());
    }
    let transmitq = queues
        .get(1)
        .ok_or_else(|| invalid_saved_state("console saved state has no transmit queue"))?;
    if !transmitq.params.enable {
        return Err(invalid_saved_state(
            "console TX offset requires an enabled transmit queue",
        ));
    }
    let readable_length = restored_queue_front_readable_length(
        features,
        transmitq.params,
        guest_memory.clone(),
        transmitq.queue_state,
    )
    .map_err(|error| {
        RestoreError::InvalidSavedState(
            anyhow::Error::new(error).context("console transmit queue is invalid"),
        )
    })?
    .ok_or_else(|| invalid_saved_state("console TX offset has no current descriptor"))?;
    if partial_transmit > readable_length {
        return Err(invalid_saved_state(format!(
            "console TX offset {partial_transmit} exceeds descriptor length {readable_length}"
        )));
    }
    Ok(())
}

#[derive(Protobuf, SavedStateRoot)]
#[mesh(package = "virtio.console")]
pub struct SavedState {
    #[mesh(1)]
    pub schema_version: u32,
    #[mesh(2)]
    pub columns: u32,
    #[mesh(3)]
    pub rows: u32,
    #[mesh(4)]
    pub partial_transmit: u64,
    #[mesh(5)]
    pub staged_rx: Vec<u8>,
    #[mesh(6)]
    pub disconnect_policy_id: u32,
    #[mesh(7)]
    pub broker: Option<SavedBrokerSnapshot>,
}

#[derive(Protobuf)]
#[mesh(package = "virtio.console")]
pub struct SavedBrokerSnapshot {
    #[mesh(1)]
    pub state: u32,
    #[mesh(2)]
    pub instance_id: Vec<u8>,
    #[mesh(3)]
    pub drain_foreign_instance_records: bool,
    #[mesh(4)]
    pub epoch: u64,
    #[mesh(5)]
    pub guest_parser: SavedParserSnapshot,
    #[mesh(6)]
    pub guest_output: SavedOutputSnapshot,
    #[mesh(7)]
    pub host_output: SavedOutputSnapshot,
    #[mesh(8)]
    pub guest_receive_sequence: u64,
    #[mesh(9)]
    pub guest_send_sequence: u64,
    #[mesh(10)]
    pub host_receive_sequence: u64,
    #[mesh(11)]
    pub host_send_sequence: u64,
    #[mesh(12)]
    pub pending_guest_record: Option<SavedRecord>,
    #[mesh(13)]
    pub pending_host_record: Option<SavedRecord>,
    #[mesh(14)]
    pub counters: SavedBrokerCounters,
}

#[derive(Protobuf)]
#[mesh(package = "virtio.console")]
pub struct SavedParserSnapshot {
    #[mesh(1)]
    pub header_bytes: Vec<u8>,
    #[mesh(2)]
    pub header_count: u32,
    #[mesh(3)]
    pub body_bytes: Vec<u8>,
    #[mesh(4)]
    pub declared_body_len: Option<u32>,
}

#[derive(Protobuf)]
#[mesh(package = "virtio.console")]
pub struct SavedEncodedRecord {
    #[mesh(1)]
    pub bytes: Vec<u8>,
    #[mesh(2)]
    pub offset: u64,
}

#[derive(Protobuf)]
#[mesh(package = "virtio.console")]
pub struct SavedOutputSnapshot {
    #[mesh(1)]
    pub current: Option<SavedEncodedRecord>,
    #[mesh(2)]
    pub queued_records: Vec<Vec<u8>>,
}

#[derive(Protobuf)]
#[mesh(package = "virtio.console")]
pub struct SavedRecord {
    #[mesh(1)]
    pub record_type: u32,
    #[mesh(2)]
    pub instance_id: Vec<u8>,
    #[mesh(3)]
    pub epoch: u64,
    #[mesh(4)]
    pub sequence: u64,
    #[mesh(5)]
    pub payload: Vec<u8>,
}

#[derive(Protobuf)]
#[mesh(package = "virtio.console")]
pub struct SavedBrokerCounters {
    #[mesh(1)]
    pub protocol_errors: u64,
    #[mesh(2)]
    pub authentication_errors: u64,
    #[mesh(3)]
    pub sequence_errors: u64,
    #[mesh(4)]
    pub ack_errors: u64,
    #[mesh(5)]
    pub reset_errors: u64,
    #[mesh(6)]
    pub reconnect_errors: u64,
    #[mesh(7)]
    pub backpressure_errors: u64,
}

impl From<BrokerSnapshot> for SavedBrokerSnapshot {
    fn from(snapshot: BrokerSnapshot) -> Self {
        Self {
            state: snapshot.state.into(),
            instance_id: snapshot.instance_id.to_vec(),
            drain_foreign_instance_records: snapshot.drain_foreign_instance_records,
            epoch: snapshot.epoch,
            guest_parser: snapshot.guest_parser.into(),
            guest_output: snapshot.guest_output.into(),
            host_output: snapshot.host_output.into(),
            guest_receive_sequence: snapshot.guest_receive_sequence,
            guest_send_sequence: snapshot.guest_send_sequence,
            host_receive_sequence: snapshot.host_receive_sequence,
            host_send_sequence: snapshot.host_send_sequence,
            pending_guest_record: snapshot.pending_guest_record.map(Into::into),
            pending_host_record: snapshot.pending_host_record.map(Into::into),
            counters: snapshot.counters.into(),
        }
    }
}

impl TryFrom<SavedBrokerSnapshot> for BrokerSnapshot {
    type Error = String;

    fn try_from(snapshot: SavedBrokerSnapshot) -> Result<Self, Self::Error> {
        let state =
            u8::try_from(snapshot.state).map_err(|_| "broker state is out of range".to_string())?;
        let instance_id = fixed_bytes(snapshot.instance_id, "broker instance ID")?;
        let broker = Self {
            state,
            instance_id,
            drain_foreign_instance_records: snapshot.drain_foreign_instance_records,
            epoch: snapshot.epoch,
            guest_parser: snapshot.guest_parser.try_into()?,
            guest_output: snapshot.guest_output.try_into()?,
            host_output: snapshot.host_output.try_into()?,
            guest_receive_sequence: snapshot.guest_receive_sequence,
            guest_send_sequence: snapshot.guest_send_sequence,
            host_receive_sequence: snapshot.host_receive_sequence,
            host_send_sequence: snapshot.host_send_sequence,
            pending_guest_record: snapshot
                .pending_guest_record
                .map(TryInto::try_into)
                .transpose()?,
            pending_host_record: snapshot
                .pending_host_record
                .map(TryInto::try_into)
                .transpose()?,
            counters: snapshot.counters.into(),
        };
        control_session_broker::ControlSessionBroker::validate_snapshot(&broker)
            .map_err(|error| error.to_string())?;
        Ok(broker)
    }
}

impl From<ParserSnapshot> for SavedParserSnapshot {
    fn from(snapshot: ParserSnapshot) -> Self {
        Self {
            header_bytes: snapshot.header_bytes,
            header_count: snapshot.header_count as u32,
            body_bytes: snapshot.body_bytes,
            declared_body_len: snapshot.declared_body_len,
        }
    }
}

impl TryFrom<SavedParserSnapshot> for ParserSnapshot {
    type Error = String;

    fn try_from(snapshot: SavedParserSnapshot) -> Result<Self, Self::Error> {
        Ok(Self {
            header_bytes: snapshot.header_bytes,
            header_count: snapshot.header_count as usize,
            body_bytes: snapshot.body_bytes,
            declared_body_len: snapshot.declared_body_len,
        })
    }
}

impl From<OutputSnapshot> for SavedOutputSnapshot {
    fn from(snapshot: OutputSnapshot) -> Self {
        Self {
            current: snapshot.current.map(Into::into),
            queued_records: snapshot.queued_records,
        }
    }
}

impl TryFrom<SavedOutputSnapshot> for OutputSnapshot {
    type Error = String;

    fn try_from(snapshot: SavedOutputSnapshot) -> Result<Self, Self::Error> {
        Ok(Self {
            current: snapshot.current.map(TryInto::try_into).transpose()?,
            queued_records: snapshot.queued_records,
        })
    }
}

impl From<EncodedRecordSnapshot> for SavedEncodedRecord {
    fn from(record: EncodedRecordSnapshot) -> Self {
        Self {
            bytes: record.bytes,
            offset: record.offset as u64,
        }
    }
}

impl TryFrom<SavedEncodedRecord> for EncodedRecordSnapshot {
    type Error = String;

    fn try_from(record: SavedEncodedRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            bytes: record.bytes,
            offset: usize::try_from(record.offset)
                .map_err(|_| "broker output offset is out of range".to_string())?,
        })
    }
}

impl From<Record> for SavedRecord {
    fn from(record: Record) -> Self {
        Self {
            record_type: record.record_type as u32,
            instance_id: record.instance_id.to_vec(),
            epoch: record.epoch,
            sequence: record.sequence,
            payload: record.payload,
        }
    }
}

impl TryFrom<SavedRecord> for Record {
    type Error = String;

    fn try_from(record: SavedRecord) -> Result<Self, Self::Error> {
        let record_type = u8::try_from(record.record_type)
            .map_err(|_| "broker record type is out of range".to_string())?;
        Ok(Self {
            record_type: control_session_protocol::RecordType::try_from(record_type)
                .map_err(|error| error.to_string())?,
            instance_id: fixed_bytes(record.instance_id, "broker record instance ID")?,
            epoch: record.epoch,
            sequence: record.sequence,
            payload: record.payload,
        })
    }
}

impl From<BrokerCounters> for SavedBrokerCounters {
    fn from(counters: BrokerCounters) -> Self {
        Self {
            protocol_errors: counters.protocol_errors,
            authentication_errors: counters.authentication_errors,
            sequence_errors: counters.sequence_errors,
            ack_errors: counters.ack_errors,
            reset_errors: counters.reset_errors,
            reconnect_errors: counters.reconnect_errors,
            backpressure_errors: counters.backpressure_errors,
        }
    }
}

impl From<SavedBrokerCounters> for BrokerCounters {
    fn from(counters: SavedBrokerCounters) -> Self {
        Self {
            protocol_errors: counters.protocol_errors,
            authentication_errors: counters.authentication_errors,
            sequence_errors: counters.sequence_errors,
            ack_errors: counters.ack_errors,
            reset_errors: counters.reset_errors,
            reconnect_errors: counters.reconnect_errors,
            backpressure_errors: counters.backpressure_errors,
        }
    }
}

fn fixed_bytes<const N: usize>(bytes: Vec<u8>, name: &str) -> Result<[u8; N], String> {
    bytes
        .try_into()
        .map_err(|_| format!("{name} must be exactly {N} bytes"))
}

fn validate_saved_broker(saved: &SavedBrokerSnapshot) -> Result<BrokerSnapshot, RestoreError> {
    use control_session_protocol::HEADER_LEN;
    use control_session_protocol::MAX_DATA_LEN;

    if !(1..=6).contains(&saved.state) {
        return Err(invalid_saved_state("invalid broker state"));
    }
    if saved.instance_id.len() != 16 || saved.instance_id.iter().all(|byte| *byte == 0) {
        return Err(invalid_saved_state("invalid broker instance ID"));
    }
    if saved.epoch == 0 {
        return Err(invalid_saved_state("saved broker epoch is zero"));
    }
    if saved.guest_parser.header_bytes.len() != HEADER_LEN
        || saved.guest_parser.header_count as usize > HEADER_LEN
        || saved.guest_parser.body_bytes.len() > MAX_DATA_LEN
        || saved
            .guest_parser
            .declared_body_len
            .is_some_and(|length| length as usize > MAX_DATA_LEN)
    {
        return Err(invalid_saved_state("invalid broker parser bounds"));
    }
    validate_saved_output(&saved.guest_output)?;
    validate_saved_output(&saved.host_output)?;
    if let Some(record) = &saved.pending_guest_record {
        validate_saved_record(record)?;
    }
    if let Some(record) = &saved.pending_host_record {
        validate_saved_record(record)?;
    }

    let snapshot = SavedBrokerSnapshot {
        state: saved.state,
        instance_id: saved.instance_id.clone(),
        drain_foreign_instance_records: saved.drain_foreign_instance_records,
        epoch: saved.epoch,
        guest_parser: SavedParserSnapshot {
            header_bytes: saved.guest_parser.header_bytes.clone(),
            header_count: saved.guest_parser.header_count,
            body_bytes: saved.guest_parser.body_bytes.clone(),
            declared_body_len: saved.guest_parser.declared_body_len,
        },
        guest_output: clone_saved_output(&saved.guest_output),
        host_output: clone_saved_output(&saved.host_output),
        guest_receive_sequence: saved.guest_receive_sequence,
        guest_send_sequence: saved.guest_send_sequence,
        host_receive_sequence: saved.host_receive_sequence,
        host_send_sequence: saved.host_send_sequence,
        pending_guest_record: saved.pending_guest_record.as_ref().map(clone_saved_record),
        pending_host_record: saved.pending_host_record.as_ref().map(clone_saved_record),
        counters: SavedBrokerCounters {
            protocol_errors: saved.counters.protocol_errors,
            authentication_errors: saved.counters.authentication_errors,
            sequence_errors: saved.counters.sequence_errors,
            ack_errors: saved.counters.ack_errors,
            reset_errors: saved.counters.reset_errors,
            reconnect_errors: saved.counters.reconnect_errors,
            backpressure_errors: saved.counters.backpressure_errors,
        },
    };
    snapshot.try_into().map_err(invalid_saved_state)
}

fn validate_saved_output(saved: &SavedOutputSnapshot) -> Result<(), RestoreError> {
    use control_session_broker::MAX_QUEUED_BYTES_PER_LEG;
    use control_session_broker::MAX_QUEUED_RECORDS_PER_LEG;
    use control_session_protocol::HEADER_LEN;
    use control_session_protocol::MAX_DATA_LEN;

    if saved.queued_records.len() > MAX_QUEUED_RECORDS_PER_LEG {
        return Err(invalid_saved_state("too many broker output records"));
    }
    let mut queued_bytes = 0usize;
    for bytes in &saved.queued_records {
        if !(HEADER_LEN..=HEADER_LEN + MAX_DATA_LEN).contains(&bytes.len()) {
            return Err(invalid_saved_state("invalid broker output record length"));
        }
        control_session_protocol::decode_exact(bytes)
            .map_err(|error| invalid_saved_state(error.to_string()))?;
        queued_bytes = queued_bytes
            .checked_add(bytes.len())
            .ok_or_else(|| invalid_saved_state("broker output byte count overflow"))?;
        if queued_bytes > MAX_QUEUED_BYTES_PER_LEG {
            return Err(invalid_saved_state(
                "broker output bytes exceed configured bound",
            ));
        }
    }
    if let Some(current) = &saved.current {
        if !(HEADER_LEN..=HEADER_LEN + MAX_DATA_LEN).contains(&current.bytes.len()) {
            return Err(invalid_saved_state(
                "invalid current broker output record length",
            ));
        }
        control_session_protocol::decode_exact(&current.bytes)
            .map_err(|error| invalid_saved_state(error.to_string()))?;
        let offset = usize::try_from(current.offset)
            .map_err(|_| invalid_saved_state("broker output offset is out of range"))?;
        if offset >= current.bytes.len() {
            return Err(invalid_saved_state(
                "broker output offset exceeds record length",
            ));
        }
    }
    Ok(())
}

fn validate_saved_record(saved: &SavedRecord) -> Result<(), RestoreError> {
    let record_type = u8::try_from(saved.record_type)
        .ok()
        .and_then(|record_type| control_session_protocol::RecordType::try_from(record_type).ok())
        .ok_or_else(|| invalid_saved_state("invalid pending broker record type"))?;
    let payload_len = saved.payload.len();
    let payload_is_valid = match record_type {
        control_session_protocol::RecordType::Ack
        | control_session_protocol::RecordType::Credit => payload_len == 4,
        control_session_protocol::RecordType::GuestAttach
        | control_session_protocol::RecordType::Reset
        | control_session_protocol::RecordType::Wait
        | control_session_protocol::RecordType::Ready => payload_len == 0,
        control_session_protocol::RecordType::HostAttach => payload_len == 32,
        control_session_protocol::RecordType::Data => {
            (1..=control_session_protocol::MAX_DATA_LEN).contains(&payload_len)
        }
        control_session_protocol::RecordType::Error => payload_len == 4,
    };
    if saved.instance_id.len() != 16 || !payload_is_valid {
        return Err(invalid_saved_state("invalid pending broker record"));
    }
    Ok(())
}

fn clone_saved_output(saved: &SavedOutputSnapshot) -> SavedOutputSnapshot {
    SavedOutputSnapshot {
        current: saved.current.as_ref().map(|record| SavedEncodedRecord {
            bytes: record.bytes.clone(),
            offset: record.offset,
        }),
        queued_records: saved.queued_records.clone(),
    }
}

fn clone_saved_record(saved: &SavedRecord) -> SavedRecord {
    SavedRecord {
        record_type: saved.record_type,
        instance_id: saved.instance_id.clone(),
        epoch: saved.epoch,
        sequence: saved.sequence,
        payload: saved.payload.clone(),
    }
}
