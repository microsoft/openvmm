// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The state-unit inventory: the registered unit names, which a snapshot
//! records and restore requires to match exactly, regardless of order.

use super::SavedStateUnit;
use super::StateTransitionError;
use super::StateUnits;

fn canonical_inventory(inventory: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut inventory = inventory.into_iter().collect::<Vec<_>>();
    inventory.sort_unstable();
    inventory
}

impl StateUnits {
    /// Returns every registered state-unit name in canonical name order.
    pub fn inventory(&self) -> Vec<String> {
        canonical_inventory(
            self.inner
                .lock()
                .units
                .values()
                .map(|unit| unit.name.to_string()),
        )
    }

    /// Saves every state unit and its exact inventory from the same snapshot.
    ///
    /// Panics if running.
    pub async fn save_with_inventory(
        &mut self,
    ) -> Result<(Vec<SavedStateUnit>, Vec<String>), StateTransitionError> {
        let (states, ready_set) = self.save_inner().await?;
        let inventory = canonical_inventory(ready_set.0.values().map(|unit| unit.name.to_string()));
        Ok((states, inventory))
    }

    /// Verifies that a saved inventory contains exactly the registered units.
    pub fn validate_inventory(&self, saved_inventory: &[String]) -> anyhow::Result<()> {
        let mut sorted_saved_inventory = saved_inventory.to_vec();
        sorted_saved_inventory.sort_unstable();
        let current_inventory = self.inventory();
        anyhow::ensure!(
            sorted_saved_inventory == current_inventory,
            "saved state-unit inventory does not match the current machine: saved={saved_inventory:?}, current={current_inventory:?}"
        );
        Ok(())
    }
}
