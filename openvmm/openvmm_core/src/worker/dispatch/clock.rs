// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Restore-time guest clock handling for the VM worker: the saved clock
//! contract, the destination clock checks made before the saved state is
//! restored, and the compensation for the host downtime between snapshot
//! capture and restore.

use super::LoadedVm;
use anyhow::Context;
use std::time::Duration;

/// Saved guest-clock contract of a restore: the host downtime to apply, the
/// saved guest TSC frequency, and the saved local APIC timer frequency, when
/// the snapshot recorded one.
pub(super) type RestoreTime = (Duration, u64, Option<u64>);

pub(super) fn restore_time_contract(
    downtime: Option<Duration>,
    tsc_frequency_hz: Option<u64>,
    apic_frequency_hz: Option<u64>,
) -> anyhow::Result<Option<RestoreTime>> {
    match (downtime, tsc_frequency_hz, apic_frequency_hz) {
        (Some(downtime), Some(tsc_frequency), apic_frequency) => {
            Ok(Some((downtime, tsc_frequency, apic_frequency)))
        }
        (None, None, None) => Ok(None),
        _ => anyhow::bail!("restore downtime and TSC frequency must be provided together"),
    }
}

#[cfg(guest_arch = "x86_64")]
pub(super) fn validate_snapshot_restore_partition_presence(
    saved_state: &openvmm_defs::worker::SavedState,
    restore_time: Option<RestoreTime>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        restore_time.is_none()
            || saved_state
                .units
                .iter()
                .any(|unit| unit.name == "partition"),
        "time-adjusted snapshot restore requires partition state"
    );
    Ok(())
}

impl LoadedVm {
    /// Checks the destination clock frequencies against the saved contract
    /// and requests the saved TSC frequency, before the saved state is
    /// restored.
    pub(super) fn validate_restore_clock(
        &self,
        restore_time: Option<RestoreTime>,
    ) -> anyhow::Result<()> {
        if let Some((_, saved_frequency, saved_apic_frequency)) = restore_time {
            let destination_frequency = self
                .inner
                .partition
                .tsc_frequency_hz()?
                .context("destination backend does not expose a guest TSC frequency")?;
            anyhow::ensure!(
                destination_frequency == saved_frequency,
                "destination TSC frequency {destination_frequency} Hz does not match saved frequency {saved_frequency} Hz"
            );
            self.inner.partition.set_tsc_frequency_hz(saved_frequency)?;
            let destination_apic_frequency = self
                .inner
                .partition
                .apic_frequency_hz()?
                .context("destination backend does not expose a local APIC frequency")?;
            if let Some(saved_apic_frequency) = saved_apic_frequency {
                anyhow::ensure!(
                    destination_apic_frequency == saved_apic_frequency,
                    "destination APIC frequency {destination_apic_frequency} Hz does not match saved frequency {saved_apic_frequency} Hz"
                );
            }
        }
        Ok(())
    }

    /// Advances the restored VM's clocks by the snapshot downtime, after the
    /// saved state is restored.
    pub(super) async fn advance_restored_clock(
        &mut self,
        restore_time: Option<RestoreTime>,
    ) -> anyhow::Result<()> {
        if let Some((downtime, frequency, saved_apic_frequency)) = restore_time {
            self.state_units
                .advance_time(downtime)
                .await
                .context("failed to advance restored VM time")?;
            #[cfg(guest_arch = "x86_64")]
            {
                let apic_frequency =
                    match saved_apic_frequency {
                        Some(frequency) => frequency,
                        None => self.inner.partition.apic_frequency_hz()?.context(
                            "destination backend does not expose a local APIC frequency",
                        )?,
                    };
                self.inner
                    .partition_unit
                    .advance_tsc(downtime, frequency, Some(apic_frequency))
                    .await
                    .context("failed to advance restored vCPU TSC")?;
            }
            self.inner
                .partition
                .advance_snapshot_time(downtime)
                .context("failed to advance backend snapshot clock")?;
        }
        Ok(())
    }
}

#[cfg(all(test, guest_arch = "x86_64"))]
mod tests {
    use super::*;
    use openvmm_defs::worker::SavedState;
    use state_unit::SavedStateUnit;
    use test_with_tracing::test;
    use vmcore::save_restore::NoSavedState;
    use vmcore::save_restore::SavedStateBlob;

    fn saved_state(unit_names: &[&str]) -> SavedState {
        SavedState {
            units: unit_names
                .iter()
                .map(|name| SavedStateUnit {
                    name: (*name).to_owned(),
                    state: SavedStateBlob::new(NoSavedState),
                })
                .collect(),
            inventory: Vec::new(),
        }
    }

    #[test]
    fn restore_without_time_adjustment_allows_missing_partition_state() {
        validate_snapshot_restore_partition_presence(&saved_state(&[]), None).unwrap();
    }

    #[test]
    fn time_adjusted_restore_allows_partition_state() {
        validate_snapshot_restore_partition_presence(
            &saved_state(&["partition"]),
            Some((Duration::ZERO, 1, None)),
        )
        .unwrap();
    }

    #[test]
    fn time_adjusted_restore_rejects_missing_partition_state() {
        let error = validate_snapshot_restore_partition_presence(
            &saved_state(&["other"]),
            Some((Duration::ZERO, 1, None)),
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "time-adjusted snapshot restore requires partition state"
        );
    }

    #[test]
    fn time_adjusted_restore_requires_partition_payload_not_inventory() {
        let mut saved_state = saved_state(&[]);
        saved_state.inventory.push("partition".to_owned());

        validate_snapshot_restore_partition_presence(&saved_state, Some((Duration::ZERO, 1, None)))
            .unwrap_err();
    }
}
