// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The instantiated VP prefix.
//!
//! A VP set may instantiate only a prefix of the VPs in its capacity (the
//! topology's VP count). Save requires every VP to be instantiated. Restore
//! validates the complete saved VP inventory before it selects the states of
//! the instantiated prefix. Dump and debug access to an uninstantiated VP
//! fails.

#[cfg(any(feature = "dump", feature = "gdb"))]
use super::Vp;
use super::VpSet;
#[cfg(any(test, feature = "dump", feature = "gdb"))]
use anyhow::Context as _;
use virt::VpIndex;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;

fn validate_restore_vp_indices(
    vp_count: usize,
    indices: impl IntoIterator<Item = VpIndex>,
) -> Result<(), RestoreError> {
    let mut present = vec![false; vp_count];
    for vp_index in indices {
        let index = vp_index.index() as usize;
        let slot = present
            .get_mut(index)
            .ok_or_else(|| RestoreError::UnknownEntryId(format!("vp{}", vp_index.index())))?;
        if std::mem::replace(slot, true) {
            return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                "snapshot contains duplicate state for vp{}",
                vp_index.index()
            )));
        }
    }
    if let Some(index) = present.iter().position(|present| !present) {
        return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
            "snapshot is missing state for vp{index}"
        )));
    }
    Ok(())
}

fn select_instantiated_vp_states<T>(
    vp_capacity: usize,
    instantiated_vp_count: usize,
    states: Vec<(VpIndex, T)>,
) -> Result<Vec<(VpIndex, T)>, RestoreError> {
    validate_restore_vp_indices(vp_capacity, states.iter().map(|(vp_index, _)| *vp_index))?;
    Ok(states
        .into_iter()
        .filter(|(vp_index, _)| vp_index.index() < instantiated_vp_count as u32)
        .collect())
}

fn validate_save_vp_count(
    instantiated_vp_count: usize,
    vp_capacity: usize,
) -> Result<(), SaveError> {
    if instantiated_vp_count != vp_capacity {
        return Err(SaveError::NotSupported);
    }
    Ok(())
}

#[cfg(any(test, feature = "dump", feature = "gdb"))]
fn instantiated_vp_index(vp_count: usize, vp: VpIndex) -> anyhow::Result<usize> {
    let index = vp.index() as usize;
    (index < vp_count)
        .then_some(index)
        .with_context(|| format!("vp{} is not instantiated", vp.index()))
}

impl VpSet {
    /// Fails unless every VP is instantiated, because a prefix cannot produce
    /// a complete saved VP inventory.
    pub(super) fn validate_save_vp_count(&self) -> Result<(), SaveError> {
        validate_save_vp_count(self.vps.len(), self.vp_capacity)
    }

    /// Validates the complete saved VP inventory and returns the states of the
    /// instantiated prefix.
    pub(super) fn select_instantiated_vp_states<T>(
        &self,
        states: impl IntoIterator<Item = (VpIndex, T)>,
    ) -> Result<Vec<(VpIndex, T)>, RestoreError> {
        select_instantiated_vp_states(
            self.vp_capacity,
            self.vps.len(),
            states.into_iter().collect(),
        )
    }

    /// Returns VP `vp`, or an error if it is not instantiated.
    #[cfg(any(feature = "dump", feature = "gdb"))]
    pub(super) fn instantiated_vp(&self, vp: VpIndex) -> anyhow::Result<&Vp> {
        Ok(&self.vps[instantiated_vp_index(self.vps.len(), vp)?])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn requires_exactly_one_restore_entry_per_vp() {
        validate_restore_vp_indices(4, (0..4).map(VpIndex::new)).unwrap();

        let missing = validate_restore_vp_indices(4, [0, 1, 3].map(VpIndex::new)).unwrap_err();
        let RestoreError::InvalidSavedState(missing) = missing else {
            panic!("expected invalid saved state");
        };
        assert!(missing.to_string().contains("missing state for vp2"));

        let duplicate = validate_restore_vp_indices(4, [0, 1, 1, 3].map(VpIndex::new)).unwrap_err();
        let RestoreError::InvalidSavedState(duplicate) = duplicate else {
            panic!("expected invalid saved state");
        };
        assert!(duplicate.to_string().contains("duplicate state for vp1"));

        let unknown = validate_restore_vp_indices(4, [0, 1, 2, 4].map(VpIndex::new)).unwrap_err();
        assert!(unknown.to_string().contains("unknown entry id: vp4"));
    }

    #[test]
    fn validates_full_inventory_before_selecting_instantiated_prefix() {
        let states = (0..4).map(|index| (VpIndex::new(index), index)).collect();
        let selected = select_instantiated_vp_states(4, 2, states).unwrap();
        assert_eq!(
            selected
                .into_iter()
                .map(|(vp_index, state)| (vp_index.index(), state))
                .collect::<Vec<_>>(),
            [(0, 0), (1, 1)]
        );

        let missing_dormant_vp = [0, 1, 2]
            .map(|index| (VpIndex::new(index), index))
            .into_iter()
            .collect();
        let error = select_instantiated_vp_states(4, 2, missing_dormant_vp).unwrap_err();
        let RestoreError::InvalidSavedState(error) = error else {
            panic!("expected invalid saved state");
        };
        assert!(error.to_string().contains("missing state for vp3"));
    }

    #[test]
    fn rejects_saving_an_instantiated_prefix() {
        assert!(validate_save_vp_count(4, 4).is_ok());
        assert!(matches!(
            validate_save_vp_count(2, 4),
            Err(SaveError::NotSupported)
        ));
    }

    #[test]
    fn rejects_access_to_an_uninstantiated_vp() {
        assert_eq!(instantiated_vp_index(2, VpIndex::new(1)).unwrap(), 1);
        assert!(
            instantiated_vp_index(2, VpIndex::new(2))
                .unwrap_err()
                .to_string()
                .contains("vp2 is not instantiated")
        );
    }
}
