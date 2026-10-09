// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Trusted servicing state only: the transport must establish authentication,
//! VM binding, and freshness. Never restore this DTO from host-controlled VMGS
//! or protector metadata. No key, report, or process-specific clock is saved.

use super::HardwareReseal;
use super::MAX_RETRY_INTERVAL;
use super::Schedule;
use mesh::payload::Protobuf;
use pal_async::timer::Instant;
use std::sync::atomic::Ordering;
use std::time::Duration;
use underhill_attestation::runtime_sealing;
use underhill_attestation::runtime_sealing::SavedRuntimeTcbFloor;
use vmcore::save_restore::SavedStateRoot;

#[derive(Debug, Clone, PartialEq, Eq, Protobuf, SavedStateRoot)]
#[mesh(package = "underhill.hardware_reseal")]
pub(crate) struct SavedHardwareResealState {
    /// Required to equal 1. Missing (zero) and unknown versions are rejected.
    #[mesh(1)]
    pub version: u32,
    /// Optional on the wire solely to detect a missing required floor.
    #[mesh(2)]
    pub floor: Option<SavedRuntimeTcbFloor>,
    /// Exact JSON used by the hardware sealing KDF, not a replacement for the
    /// destination's configuration. No current_time normalization is performed.
    #[mesh(3)]
    pub config_json: String,
    #[mesh(4)]
    pub failures: u32,
    #[mesh(5)]
    pub force_reseal: bool,
    #[mesh(6)]
    pub pending_notification: bool,
    /// Remaining delay, bounded by MAX_RETRY_INTERVAL, relative to restore time.
    #[mesh(7)]
    pub deadline_remaining_ns: u64,
    #[mesh(8)]
    pub not_before_remaining_ns: u64,
}

impl HardwareReseal {
    pub(super) fn save_stopped(&self) -> anyhow::Result<SavedHardwareResealState> {
        anyhow::ensure!(
            !self.schedule.running,
            "hardware reseal must be stopped before save"
        );
        let floor = self.tcb_floor.lock();
        let floor = floor
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("hardware reseal floor is not initialized"))?;
        let config_json = serde_json::to_string(&*self.config)?;
        let now = Instant::now();
        let remaining = |deadline: Instant| {
            // Clamp obsolete deadlines to zero and bound all saved delays.
            deadline
                .saturating_sub(now)
                .min(MAX_RETRY_INTERVAL)
                .as_nanos() as u64
        };
        Ok(SavedHardwareResealState {
            version: 1,
            floor: Some(floor.save()),
            config_json,
            failures: self.schedule.failures,
            force_reseal: self.schedule.force_reseal,
            // Saving is observational. A failed VM save followed by resume
            // must still see this notification in the original worker.
            pending_notification: self.notification.pending.load(Ordering::Acquire),
            deadline_remaining_ns: remaining(self.schedule.deadline),
            not_before_remaining_ns: remaining(self.schedule.not_before),
        })
    }

    pub(super) fn restore_stopped(
        &mut self,
        state: SavedHardwareResealState,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.schedule.running,
            "hardware reseal must be stopped before restore"
        );
        anyhow::ensure!(
            state.version == 1,
            "unsupported hardware reseal state version"
        );
        anyhow::ensure!(
            state.config_json == serde_json::to_string(&*self.config)?,
            "hardware reseal VM configuration changed"
        );
        anyhow::ensure!(
            state.failures == 0 || state.force_reseal,
            "hardware reseal failures without pending recovery"
        );
        let deadline = Duration::from_nanos(state.deadline_remaining_ns);
        let not_before = Duration::from_nanos(state.not_before_remaining_ns);
        anyhow::ensure!(
            deadline <= MAX_RETRY_INTERVAL && not_before <= MAX_RETRY_INTERVAL,
            "hardware reseal retry delay exceeds maximum"
        );
        let restored = runtime_sealing::RuntimeTcbFloor::restore(
            state
                .floor
                .ok_or_else(|| anyhow::anyhow!("missing hardware reseal floor"))?,
            &*self.tee,
            &self.config,
        )?;
        let mut resident = self.tcb_floor.lock();
        if let Some(resident) = resident.as_ref() {
            resident.check_restored_successor(&restored)?;
        }

        // Nothing above mutates state or performs hardware/broker I/O. Keep the
        // same Arc and hold its lock across the ratchet check and entire commit.
        let now = Instant::now();
        let schedule = Schedule {
            running: false,
            // Restore, unlike ordinary start/reset, always owes a durable
            // rewrite even at the same SVN: hardware identity can have changed
            // during downtime or after the source's notification save cutoff.
            force_reseal: true,
            failures: state.failures,
            deadline: now.saturating_add(deadline),
            not_before: now.saturating_add(not_before),
        };
        *resident = Some(restored);
        self.schedule = schedule;
        // OR, never overwrite: callbacks racing restore must remain latched.
        if state.pending_notification {
            self.notification.notify();
        }
        Ok(())
    }
}
