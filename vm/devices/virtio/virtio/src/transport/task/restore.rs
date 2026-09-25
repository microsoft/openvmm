// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Device-private state in the device task.
//!
//! `ChangeDeviceState::stop()` captures the device-private state together
//! with the queue states. A restore hands the restored private state to the
//! stopped device, before any queue starts, and waits for the result.

use super::DeviceTask;
use crate::queue::QueueState;
use mesh::rpc::FailableRpc;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;

/// Queue and device-private state captured after a device has stopped.
pub struct StopResult {
    pub queues: Vec<(bool, Option<QueueState>)>,
    pub device_state: Result<Option<SavedStateBlob>, SaveError>,
}

impl DeviceTask {
    /// Handles `DeviceCommand::Restore`: restores device-private state into
    /// the device, which must have no started queue.
    pub(super) fn restore_device(&mut self, state: Option<SavedStateBlob>) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.started.contains(&true),
            "virtio device restore requires a stopped device"
        );
        let result = self.device.restore_device(state);
        tracing::debug!(
            target: "virtio_restore",
            event = "private_state_restore",
            device_type = self.device_type,
            success = result.is_ok(),
            "virtio restore lifecycle"
        );
        Ok(result?)
    }

    /// Builds the `DeviceCommand::Stop` result from the stopped queue states.
    ///
    /// Only queues that were started report their state.
    pub(super) fn stop_result(&mut self, states: Vec<Option<QueueState>>) -> StopResult {
        StopResult {
            queues: self
                .started
                .iter_mut()
                .map(std::mem::take)
                .zip(states)
                .collect(),
            device_state: self.device.save_device(),
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
