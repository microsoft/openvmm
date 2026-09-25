// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Save and restore of the device-private virtio-console state.
//!
//! The worker state outlives queue restarts so that the TX progress of the
//! current descriptor and staged host input can be captured once both queues
//! are stopped. This module defines the versioned
//! saved-state format ([`SavedState`]), validates it before restore, and
//! implements save, restore, input quiescing, and reset of that state.

use crate::BUF_SIZE;
use crate::VirtioConsoleDevice;
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
        let ConsoleWorkerMode::Direct {
            disconnect_policy, ..
        } = &worker.mode
        else {
            return Err(SaveError::NotSupported);
        };
        Ok(Some(SavedStateBlob::new(SavedState {
            schema_version: DIRECT_SAVED_STATE_VERSION,
            columns: self.config.cols.into(),
            rows: self.config.rows.into(),
            partial_transmit: state.partial_transmit as u64,
            staged_rx: state.staged_rx.iter().copied().collect(),
            disconnect_policy_id: disconnect_policy_id(*disconnect_policy),
        })))
    }

    pub(crate) fn restore_private_state(
        &mut self,
        state: Option<SavedStateBlob>,
    ) -> Result<(), RestoreError> {
        let (worker, runtime) = self.worker.get_mut();
        let saved = validate_saved_state(state.as_ref(), worker.mode.validation_mode())?;
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

        self.config = VirtioConsoleConfig {
            cols: columns,
            rows,
        };
        runtime.partial_transmit = partial_transmit;
        runtime.staged_rx = saved.staged_rx.into();
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
    Broker,
}

impl ConsoleWorkerMode {
    fn validation_mode(&self) -> SavedStateMode {
        match self {
            Self::Direct {
                disconnect_policy, ..
            } => SavedStateMode::Direct(*disconnect_policy),
            Self::Broker(_) => SavedStateMode::Broker,
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
    let SavedStateMode::Direct(disconnect_policy) = mode else {
        return Err(RestoreError::SavedStateNotSupported);
    };
    if saved.schema_version != DIRECT_SAVED_STATE_VERSION {
        return Err(invalid_saved_state(format!(
            "direct console requires schema version {DIRECT_SAVED_STATE_VERSION}, got {}",
            saved.schema_version
        )));
    }
    if saved.disconnect_policy_id != disconnect_policy_id(disconnect_policy) {
        return Err(invalid_saved_state(
            "console disconnect policy does not match the saved policy",
        ));
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
}
