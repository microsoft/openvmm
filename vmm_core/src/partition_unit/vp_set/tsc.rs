// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TSC advancement for snapshot restore downtime.

use super::StateEvent;
use super::VpEvent;
use super::VpSet;
use anyhow::Context as _;
use futures::future::TryJoinAll;
use hvdef::Vtl;
use mesh::rpc::RpcSend;
use std::time::Duration;
use virt::Processor;
use virt::vp::AccessVpState;

/// A request to advance the TSC of a stopped VP by the restore downtime.
#[derive(Debug, Copy, Clone)]
pub(super) struct TscAdvance {
    duration: Duration,
    frequency_hz: u64,
    apic_frequency_hz: Option<u64>,
}

struct AdvancedTsc {
    value: u64,
    cycles: u64,
}

impl AdvancedTsc {
    fn validate(&self, observed_tsc: u64) -> anyhow::Result<()> {
        anyhow::ensure!(
            observed_tsc >= self.value,
            "restored TSC did not advance by the required downtime: requested {:#x}, observed {observed_tsc:#x}, adjustment {} cycles",
            self.value,
            self.cycles,
        );
        Ok(())
    }
}

fn advance_tsc_state(
    previous_tsc: u64,
    duration: Duration,
    frequency_hz: u64,
) -> anyhow::Result<AdvancedTsc> {
    let cycles = duration
        .as_nanos()
        .checked_mul(u128::from(frequency_hz))
        .context("TSC downtime adjustment overflows")?
        / 1_000_000_000;
    let cycles = u64::try_from(cycles).context("TSC downtime adjustment exceeds u64")?;
    let value = previous_tsc
        .checked_add(cycles)
        .context("TSC downtime adjustment exceeds the counter range")?;
    Ok(AdvancedTsc { value, cycles })
}

impl TscAdvance {
    /// Advances the TSC of the stopped VP `vp`, together with its LAPIC timer
    /// and TSC deadline when present.
    pub(super) fn apply(self, vp: &mut impl Processor) -> anyhow::Result<()> {
        let Self {
            duration,
            frequency_hz,
            apic_frequency_hz,
        } = self;
        let mut access = vp.access_state(Vtl::Vtl0);
        let tsc = access.tsc().context("failed to read stopped vCPU TSC")?;
        let mut tsc_deadline = access
            .caps()
            .tsc_deadline
            .then(|| access.tsc_deadline())
            .transpose()
            .context("failed to read stopped vCPU TSC deadline")?;
        let mut apic = apic_frequency_hz
            .map(|frequency| {
                let mut apic = access
                    .apic()
                    .context("failed to read stopped LAPIC timer")?;
                apic.advance_timer(duration, frequency)?;
                anyhow::Ok(apic)
            })
            .transpose()?;
        let previous_tsc = tsc.value;
        let advanced = advance_tsc_state(previous_tsc, duration, frequency_hz)?;
        if let (Some(apic), Some(tsc_deadline)) = (apic.as_mut(), tsc_deadline.as_mut()) {
            tsc_deadline.value =
                apic.advance_tsc_deadline(previous_tsc, advanced.value, tsc_deadline.value);
        }
        drop(access);
        vp.advance_tsc(advanced.cycles)
            .context("failed to adjust stopped vCPU TSC")?;
        let mut access = vp.access_state(Vtl::Vtl0);
        if let Some(apic) = apic.take() {
            access
                .set_apic(&apic)
                .context("failed to reprogram stopped LAPIC timer")?;
        }
        if let Some(tsc_deadline) = tsc_deadline {
            access
                .set_tsc_deadline(&tsc_deadline)
                .context("failed to reprogram stopped vCPU TSC deadline")?;
        }
        access
            .commit()
            .context("failed to commit adjusted vCPU TSC")?;
        let observed_tsc = access
            .tsc()
            .context("failed to read back adjusted vCPU TSC")?
            .value;
        tracing::debug!(
            previous_tsc,
            requested_tsc = advanced.value,
            observed_tsc,
            cycles = advanced.cycles,
            frequency_hz,
            ?duration,
            "adjusted restored vCPU TSC"
        );
        advanced.validate(observed_tsc)?;
        Ok(())
    }
}

impl VpSet {
    /// Advances TSC state on every stopped vCPU.
    pub async fn advance_tsc(
        &mut self,
        duration: Duration,
        frequency_hz: u64,
        apic_frequency_hz: Option<u64>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!self.started, "vCPUs must be stopped before adjusting TSC");
        self.vps
            .iter()
            .enumerate()
            .map(|(index, vp)| async move {
                vp.send
                    .call_failable(
                        |rpc| VpEvent::State(StateEvent::AdvanceTsc(rpc)),
                        TscAdvance {
                            duration,
                            frequency_hz,
                            apic_frequency_hz,
                        },
                    )
                    .await
                    .with_context(|| format!("vp{index} TSC adjustment"))
            })
            .collect::<TryJoinAll<_>>()
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use test_with_tracing::test;

    #[test]
    fn downtime_advances_tsc_by_elapsed_cycles() {
        let advanced = advance_tsc_state(1_000, Duration::from_secs(1), 1_000).unwrap();

        assert_eq!(advanced.value, 2_000);
        assert_eq!(advanced.cycles, 1_000);
    }

    #[test]
    fn downtime_rejects_tsc_overflow() {
        assert!(advance_tsc_state(u64::MAX, Duration::from_nanos(1), 1_000_000_000).is_err());
        assert!(advance_tsc_state(0, Duration::MAX, u64::MAX).is_err());
    }

    #[test]
    fn downtime_rejects_discarded_or_partial_tsc_advancement() {
        let advanced = advance_tsc_state(1_000, Duration::from_millis(250), 1_000_000_000).unwrap();

        assert!(advanced.validate(1_000).is_err());
        assert!(advanced.validate(advanced.value - 1).is_err());
    }

    #[test]
    fn downtime_allows_tsc_progress_during_adjustment() {
        for duration in [Duration::ZERO, Duration::from_millis(250)] {
            let advanced = advance_tsc_state(1_000, duration, 1_000_000_000).unwrap();

            advanced.validate(advanced.value).unwrap();
            advanced.validate(advanced.value + 1_000).unwrap();
        }
    }
}
