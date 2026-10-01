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
use virt::vp::AccessVpState;

/// A request to advance the LAPIC timer of a stopped VP by the restore
/// downtime.
#[derive(Debug, Copy, Clone)]
pub(super) struct LapicAdvance {
    downtime_ns: u64,
    apic_hz: u64,
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
    /// Advances the one-shot LAPIC timer of the stopped VP `vp` and sets its
    /// LAPIC state again, even when no timer is armed: KVM derives its timer
    /// deadline from the guest TSC when the LAPIC state is set.
    pub(super) fn apply(self, vp: &mut impl Processor) -> anyhow::Result<()> {
        let mut access = vp.access_state(Vtl::Vtl0);
        let mut apic = access.apic().context("failed to read stopped LAPIC")?;
        apic.advance_one_shot_timer(self.downtime_ns, self.apic_hz)?;
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
    /// Checks the LAPIC timer of every stopped VP: capture step 1 of the time
    /// ABI. Restore needs no separate check, because the LAPIC advance
    /// rejects the same timers.
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

    /// Advances the one-shot LAPIC timer of every stopped VP by `downtime_ns`
    /// at `apic_hz` and sets every VP's LAPIC state again: restore step 14 of
    /// the time ABI, after the synchronized TSC set. A periodic or
    /// TSC-deadline timer fails it (`E_LAPIC_PERIODIC`,
    /// `E_LAPIC_TSC_DEADLINE`) before any VP runs.
    pub async fn advance_lapic_timers(
        &mut self,
        downtime_ns: u64,
        apic_hz: u64,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.started,
            "vCPUs must be stopped before advancing LAPIC timers"
        );
        self.vps
            .iter()
            .enumerate()
            .map(|(index, vp)| async move {
                vp.send
                    .call_failable(
                        |rpc| VpEvent::State(StateEvent::AdvanceLapic(rpc)),
                        LapicAdvance {
                            downtime_ns,
                            apic_hz,
                        },
                    )
                    .await
                    .with_context(|| format!("vp{index} LAPIC timer advance"))
            })
            .collect::<TryJoinAll<_>>()
            .await?;
        Ok(())
    }
}
