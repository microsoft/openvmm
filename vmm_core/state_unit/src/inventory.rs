// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The state-unit inventory: the registered unit names, which a snapshot
//! records and restore requires to match exactly, regardless of order.

use super::StateUnits;

impl StateUnits {
    /// Returns every registered state-unit name in canonical name order.
    pub fn inventory(&self) -> Vec<String> {
        let mut inventory = self
            .inner
            .lock()
            .units
            .values()
            .map(|unit| unit.name.to_string())
            .collect::<Vec<_>>();
        inventory.sort_unstable();
        inventory
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
