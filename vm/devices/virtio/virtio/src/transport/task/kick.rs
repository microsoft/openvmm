// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Queue kicks serialized through the device task.
//!
//! Emulated queue notifications reach the device task as
//! [`DeviceCommand::Kick`](super::DeviceCommand::Kick), so that they are
//! ordered with device-private state activation. Kicks coalesce until the
//! device task dequeues them. A kick for a queue that has not started is staged
//! and dispatched once the queue starts.

use super::DeviceTask;
use crate::QueueResources;
use crate::queue::QueueState;
use crate::spec::VirtioDeviceFeatures;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

/// Queue notification, serialized with private-state activation.
pub struct Kick {
    pub idx: u16,
    pub event: pal_event::Event,
    pub queued: Arc<AtomicBool>,
}

/// Marks a queue kick as queued. Returns `true` if the caller must send a
/// [`Kick`], or `false` if one is already queued.
pub(crate) fn mark_kick_queued(queued: &AtomicBool) -> bool {
    !queued.swap(true, Ordering::AcqRel)
}

/// Per-queue kick state of the device task.
pub(super) struct KickState {
    /// The notification event of each queue that has been kicked.
    events: Vec<Option<pal_event::Event>>,
    /// Kicks staged for queues that have not started.
    pending: Vec<bool>,
    /// Queues that are started.
    pub(super) started: Vec<bool>,
}

impl KickState {
    pub(super) fn new(max_queues: u16) -> Self {
        Self {
            events: vec![None; max_queues as usize],
            pending: vec![false; max_queues as usize],
            started: vec![false; max_queues as usize],
        }
    }
}

/// Queues started by one `enable` or `start`, whose staged kicks are
/// dispatched once all of the queues have started.
pub(super) struct StartedQueues<'a> {
    /// The trigger reported by the `queue_start` restore lifecycle trace.
    trigger: &'static str,
    features: &'a VirtioDeviceFeatures,
    events: Vec<(u16, pal_event::Event)>,
}

impl<'a> StartedQueues<'a> {
    pub(super) fn new(
        trigger: &'static str,
        features: &'a VirtioDeviceFeatures,
        queue_count: usize,
    ) -> Self {
        Self {
            trigger,
            features,
            events: Vec::with_capacity(queue_count),
        }
    }
}

impl DeviceTask {
    /// Starts one queue and records it in `started`.
    pub(super) async fn start_queue(
        &mut self,
        idx: u16,
        resources: QueueResources,
        initial_state: Option<QueueState>,
        started: &mut StartedQueues<'_>,
    ) -> anyhow::Result<()> {
        let event = resources.event.clone();
        let restored_progress = initial_state.is_some();
        let result = self
            .device
            .start_queue(idx, resources, started.features, initial_state)
            .await;
        tracing::debug!(
            target: "virtio_restore",
            event = "queue_start",
            device_type = self.restore.device_type,
            trigger = started.trigger,
            queue_index = i32::from(idx),
            restored_progress,
            success = result.is_ok(),
            "virtio restore lifecycle"
        );
        if result.is_ok() {
            self.kicks.started[idx as usize] = true;
            started.events.push((idx, event));
        }
        result
    }

    /// Handles `DeviceCommand::Kick`.
    pub(super) fn kick(&mut self, kick: Kick) {
        let Kick { idx, event, queued } = kick;
        queued.store(false, Ordering::Release);
        if let Some(slot) = self.kicks.events.get_mut(idx as usize) {
            *slot = Some(event.clone());
        }
        if let Err(error) = self.apply_pending_restore("kick", Some(idx)) {
            tracelimit::error_ratelimited!(
                error = &error as &dyn std::error::Error,
                idx,
                "virtio device restore failed before queue kick"
            );
            return;
        }
        if self.kicks.started.get(idx as usize) == Some(&true) {
            event.signal();
            tracing::debug!(
                target: "virtio_restore",
                event = "kick_dispatch",
                device_type = self.restore.device_type,
                trigger = "kick",
                queue_index = i32::from(idx),
                restored_progress = false,
                success = true,
                "virtio restore lifecycle"
            );
        } else if let Some(pending) = self.kicks.pending.get_mut(idx as usize) {
            let newly_staged = !*pending;
            *pending = true;
            if newly_staged {
                tracing::debug!(
                    target: "virtio_restore",
                    event = "kick_staged",
                    device_type = self.restore.device_type,
                    trigger = "kick",
                    queue_index = i32::from(idx),
                    restored_progress = false,
                    success = true,
                    "virtio restore lifecycle"
                );
            }
        }
    }

    /// Drains stale queue notifications and discards staged kicks.
    pub(super) fn clear_queue_events(&mut self) {
        for event in self.kicks.events.iter().flatten() {
            while event.try_wait() {}
        }
        self.kicks.pending.fill(false);
    }

    /// Dispatches the kicks staged for the queues in `started`.
    pub(super) fn dispatch_pending_kicks(&mut self, started: StartedQueues<'_>) {
        for (idx, event) in started.events {
            self.dispatch_pending_kick(idx, &event);
        }
    }

    fn dispatch_pending_kick(&mut self, idx: u16, event: &pal_event::Event) {
        if !std::mem::take(&mut self.kicks.pending[idx as usize]) {
            return;
        }
        event.signal();
        tracing::debug!(
            target: "virtio_restore",
            event = "kick_dispatch",
            device_type = self.restore.device_type,
            trigger = "driver-ok",
            queue_index = i32::from(idx),
            restored_progress = false,
            success = true,
            "virtio restore lifecycle"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::mark_kick_queued;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;

    #[test]
    fn queue_kicks_coalesce_until_the_command_is_dequeued() {
        let queued = AtomicBool::new(false);

        assert!(mark_kick_queued(&queued));
        assert!(!mark_kick_queued(&queued));

        queued.store(false, Ordering::Release);
        assert!(mark_kick_queued(&queued));
    }
}
