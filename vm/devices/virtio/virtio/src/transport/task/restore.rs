// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Device-private state capture and restore staging in the device task.
//!
//! `ChangeDeviceState::stop()` captures the device-private state together
//! with the queue states. After a restore, `start()` stages the restored
//! private state, and the device task applies it on the first activation: the
//! start of an active device, DRIVER_OK, a device-config access, or a queue
//! kick.

use super::DeviceCommand;
use super::DeviceTask;
use super::EnableParams;
use super::StartParams;
use crate::queue::QueueState;
use chipset_device::io::IoError;
use mesh::rpc::FailableRpc;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;

/// Whether this start follows restore, and its optional private payload.
#[derive(Clone)]
pub enum DeviceRestoreState {
    NotRestored,
    Restored(Option<SavedStateBlob>),
}

impl DeviceRestoreState {
    pub fn is_restored(&self) -> bool {
        matches!(self, Self::Restored(_))
    }
}

/// Queue and device-private state captured after a device has stopped.
pub struct StopResult {
    pub queues: Vec<(bool, Option<QueueState>)>,
    pub device_state: Result<Option<SavedStateBlob>, SaveError>,
}

/// Restore staging state of the device task.
pub(super) struct RestoreStaging {
    /// The device type reported by restore lifecycle traces.
    pub(super) device_type: u16,
    /// Restored private state that has not been applied to the device yet.
    pending: DeviceRestoreState,
}

impl RestoreStaging {
    pub(super) fn new(device_type: u16) -> Self {
        Self {
            device_type,
            pending: DeviceRestoreState::NotRestored,
        }
    }
}

impl DeviceTask {
    /// Handles `DeviceCommand::Enable`: applies staged private state before
    /// the queues start.
    pub(super) async fn enable_with_restore(&mut self, params: EnableParams) -> bool {
        if let Err(error) = self.apply_pending_restore("driver-ok", None) {
            tracelimit::error_ratelimited!(
                error = &error as &dyn std::error::Error,
                "virtio device restore failed during enable"
            );
            self.stop_all_queues().await;
            self.clear_queue_events();
            self.device.reset().await;
            self.restore.pending = DeviceRestoreState::NotRestored;
            return false;
        }
        self.enable(params).await
    }

    /// Handles `DeviceCommand::Start`: stages restored private state and, for
    /// an active device, applies it before the queues start. Stops the queues
    /// again if one of them fails to start.
    pub(super) async fn start_with_restore(
        &mut self,
        mut params: StartParams,
    ) -> anyhow::Result<()> {
        let device_state =
            std::mem::replace(&mut params.device_state, DeviceRestoreState::NotRestored);
        self.stage_restore(device_state, params.active);
        if !params.active {
            return Ok(());
        }
        self.apply_pending_restore("active-start", None)?;
        let result = self.start(params).await;
        if result.is_err() {
            self.stop_all_queues().await;
        }
        result
    }

    fn stage_restore(&mut self, state: DeviceRestoreState, active: bool) {
        if state.is_restored() {
            tracing::debug!(
                target: "virtio_restore",
                event = "restore_staged",
                device_type = self.restore.device_type,
                trigger = if active { "active-start" } else { "inactive-start" },
                queue_index = -1,
                restored_progress = false,
                success = true,
                "virtio restore lifecycle"
            );
            self.restore.pending = state;
        }
    }

    pub(super) fn apply_pending_restore(
        &mut self,
        trigger: &'static str,
        queue_index: Option<u16>,
    ) -> Result<(), RestoreError> {
        let DeviceRestoreState::Restored(state) = &self.restore.pending else {
            return Ok(());
        };
        let result = self.device.restore_device(state.clone());
        tracing::debug!(
            target: "virtio_restore",
            event = "private_state_apply",
            device_type = self.restore.device_type,
            trigger,
            queue_index = queue_index.map(i32::from).unwrap_or(-1),
            restored_progress = false,
            success = result.is_ok(),
            "virtio restore lifecycle"
        );
        if result.is_ok() {
            self.restore.pending = DeviceRestoreState::NotRestored;
        }
        result
    }

    /// Discards staged kicks and staged private state before the device is
    /// reset.
    pub(super) fn discard_staged(&mut self) {
        self.clear_queue_events();
        self.restore.pending = DeviceRestoreState::NotRestored;
    }

    /// Builds the `DeviceCommand::Stop` result from the stopped queue states.
    ///
    /// Only queues that were started report their state. The device-private
    /// state is saved from the device, or is the staged restored state if it
    /// has not been applied yet.
    pub(super) fn stop_result(&mut self, states: Vec<Option<QueueState>>) -> StopResult {
        StopResult {
            queues: self
                .kicks
                .started
                .iter_mut()
                .map(std::mem::take)
                .zip(states)
                .collect(),
            device_state: match &self.restore.pending {
                DeviceRestoreState::NotRestored => self.device.save_device(),
                DeviceRestoreState::Restored(state) => Ok(state.clone()),
            },
        }
    }

    /// Applies staged private state before a device-config access.
    ///
    /// Returns the command to process, or `None` if the restore failed and the
    /// access was completed with an error.
    pub(super) fn restore_before_config(&mut self, cmd: DeviceCommand) -> Option<DeviceCommand> {
        match cmd {
            DeviceCommand::ReadConfig {
                offset,
                len,
                completion,
                deferred,
            } => {
                if let Err(error) = self.apply_pending_restore("config-read", None) {
                    tracelimit::error_ratelimited!(
                        error = &error as &dyn std::error::Error,
                        "virtio device restore failed before config read"
                    );
                    deferred.complete_error(IoError::NoResponse);
                    return None;
                }
                Some(DeviceCommand::ReadConfig {
                    offset,
                    len,
                    completion,
                    deferred,
                })
            }
            DeviceCommand::WriteConfig {
                offset,
                len,
                data,
                deferred,
            } => {
                if let Err(error) = self.apply_pending_restore("config-write", None) {
                    tracelimit::error_ratelimited!(
                        error = &error as &dyn std::error::Error,
                        "virtio device restore failed before config write"
                    );
                    deferred.complete_error(IoError::NoResponse);
                    return None;
                }
                Some(DeviceCommand::WriteConfig {
                    offset,
                    len,
                    data,
                    deferred,
                })
            }
            cmd => Some(cmd),
        }
    }

    /// Handles `DeviceCommand::QuiesceInput`.
    pub(super) async fn quiesce_input(&mut self, rpc: FailableRpc<(), ()>) {
        rpc.handle_failable(async |()| self.device.quiesce_input().await)
            .await;
    }

    /// Handles `DeviceCommand::ResumeInput`.
    pub(super) async fn resume_input(&mut self, rpc: FailableRpc<(), ()>) {
        rpc.handle_failable(async |()| self.device.resume_input().await)
            .await;
    }
}
