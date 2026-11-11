// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! LAPIC timer checks and advancement for an NVX time ABI snapshot.

use super::StateEvent;
use super::VpEvent;
use super::VpSet;
use anyhow::Context as _;
use futures::future::TryJoinAll;
use hvdef::Vtl;
use mesh::rpc::RpcSend;
use virt::Processor;
use virt::VpIndex;
use virt::vp::AccessVpState;
use virt::x86::vp::LapicRestoreClock;
use virt::x86::vp::SavedOneShotTimer;

/// A request to set the LAPIC timer of a stopped VP after the synchronized
/// TSC set of a restore.
#[derive(Debug, Copy, Clone)]
pub(super) struct LapicAdvance {
    clock: LapicRestoreClock,
    /// The timer that the restore took out of the VP's saved state, if any.
    timer: Option<SavedOneShotTimer>,
}

/// Checks the LAPIC timer of the stopped VP `vp` (`E_LAPIC_PERIODIC`,
/// `E_LAPIC_TSC_DEADLINE`).
pub(super) fn check_one_shot_timer(vp: &mut impl Processor) -> anyhow::Result<()> {
    let apic = vp
        .access_state(Vtl::Vtl0)
        .apic()
        .context("failed to read stopped LAPIC")?;
    apic.check_one_shot_timer()?;
    Ok(())
}

impl LapicAdvance {
    /// Sets the one-shot LAPIC timer that the restore took out of the stopped
    /// VP `vp` again, to run as the VP's TSC does, and sets the VP's LAPIC
    /// state again, even when no timer is armed: KVM derives its timer
    /// deadline from the guest TSC when the LAPIC state is set. The timer of a
    /// VP whose saved state held none advances by the downtime.
    pub(super) fn apply(self, vp: &mut impl Processor) -> anyhow::Result<()> {
        let mut access = vp.access_state(Vtl::Vtl0);
        let mut apic = access.apic().context("failed to read stopped LAPIC")?;
        match self.timer {
            Some(timer) => {
                // Read the TSC after the LAPIC state, so that the time to set
                // the state can only delay the timer.
                let now = access.tsc().context("failed to read the restored TSC")?;
                apic.restore_one_shot_timer(timer, &self.clock, now.value)?;
            }
            None => apic.advance_one_shot_timer(self.clock.downtime_ns, self.clock.apic_hz)?,
        }
        access
            .set_apic(&apic)
            .context("failed to set the restored LAPIC")?;
        access
            .commit()
            .context("failed to commit the restored LAPIC")?;
        Ok(())
    }
}

impl VpSet {
    /// Checks the LAPIC timer of every stopped VP before a capture takes its
    /// time ABI records. Restore needs no separate check, because the LAPIC
    /// advance rejects the same timers.
    pub async fn check_one_shot_timers(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.started,
            "vCPUs must be stopped before checking LAPIC timers"
        );
        self.vps
            .iter()
            .enumerate()
            .map(|(index, vp)| async move {
                vp.send
                    .call_failable(|rpc| VpEvent::State(StateEvent::CheckTimers(rpc)), ())
                    .await
                    .with_context(|| format!("vp{index} LAPIC timer"))
            })
            .collect::<TryJoinAll<_>>()
            .await?;
        Ok(())
    }

    /// Sets the one-shot LAPIC timer of every stopped VP again after the
    /// synchronized TSC set of a restore, from `timers`, which the restore
    /// took out of the VPs' saved state, over `clock`, and sets every VP's
    /// LAPIC state again. A periodic or TSC-deadline timer fails it
    /// (`E_LAPIC_PERIODIC`, `E_LAPIC_TSC_DEADLINE`) before any VP runs.
    pub async fn advance_lapic_timers(
        &mut self,
        clock: LapicRestoreClock,
        timers: impl IntoIterator<Item = (VpIndex, SavedOneShotTimer)>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.started,
            "vCPUs must be stopped before advancing LAPIC timers"
        );
        let mut vp_timers = vec![None; self.vps.len()];
        for (vp_index, timer) in timers {
            if let Some(slot) = vp_timers.get_mut(vp_index.index() as usize) {
                *slot = Some(timer);
            }
        }
        self.vps
            .iter()
            .zip(vp_timers)
            .enumerate()
            .map(|(index, (vp, timer))| async move {
                vp.send
                    .call_failable(
                        |rpc| VpEvent::State(StateEvent::AdvanceLapic(rpc)),
                        LapicAdvance { clock, timer },
                    )
                    .await
                    .with_context(|| format!("vp{index} LAPIC timer advance"))
            })
            .collect::<TryJoinAll<_>>()
            .await?;
        Ok(())
    }
}
