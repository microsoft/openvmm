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
use underhill_attestation::runtime_sealing::SavedRuntimeSealingState;
use vmcore::save_restore::SavedStateRoot;

#[derive(Debug, Clone, PartialEq, Eq, Protobuf, SavedStateRoot)]
#[mesh(package = "underhill.hardware_reseal")]
pub(crate) struct SavedHardwareResealState {
    /// Required to equal 1. Missing (zero) and unknown versions are rejected.
    #[mesh(1)]
    pub version: u32,
    /// Optional on the wire solely to detect missing initialized state.
    #[mesh(2)]
    pub sealing_state: Option<SavedRuntimeSealingState>,
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
    pub next_attempt_remaining_ns: u64,
}

impl HardwareReseal {
    pub(super) fn save_stopped(&self) -> anyhow::Result<SavedHardwareResealState> {
        anyhow::ensure!(
            !self.schedule.running,
            "hardware reseal must be stopped before save"
        );
        let state = self.sealing_state.lock();
        let state = state
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("hardware resealing state is not initialized"))?;
        let config_json = serde_json::to_string(&*self.config)?;
        let now = Instant::now();
        let remaining = self
            .schedule
            .next_attempt_at
            .saturating_sub(now)
            .min(MAX_RETRY_INTERVAL)
            .as_nanos() as u64;
        Ok(SavedHardwareResealState {
            version: 1,
            sealing_state: Some(state.save()),
            config_json,
            failures: self.schedule.failures,
            force_reseal: self.schedule.force_reseal,
            // Saving is observational. A failed VM save followed by resume
            // must still see this notification in the original worker.
            pending_notification: self.notification.pending.load(Ordering::Acquire),
            next_attempt_remaining_ns: remaining,
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
        let delay = Duration::from_nanos(state.next_attempt_remaining_ns);
        anyhow::ensure!(
            delay <= MAX_RETRY_INTERVAL,
            "hardware reseal retry delay exceeds maximum"
        );
        let restored = runtime_sealing::RuntimeSealingState::restore(
            state
                .sealing_state
                .ok_or_else(|| anyhow::anyhow!("missing hardware resealing state"))?,
            &*self.tee,
            &self.config,
        )?;
        let mut resident = self.sealing_state.lock();
        if let Some(resident) = resident.as_ref() {
            resident.check_successor(&restored)?;
        }

        // Nothing above mutates state or performs hardware/broker I/O. Keep the
        // same Arc and hold its lock across the policy check and entire commit.
        let now = Instant::now();
        let schedule = Schedule {
            running: false,
            // Restore, unlike ordinary start/reset, always owes a durable
            // rewrite even at the same SVN: hardware identity can have changed
            // during downtime or after the source's notification save cutoff.
            force_reseal: true,
            failures: state.failures,
            next_attempt_at: now.saturating_add(delay),
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
