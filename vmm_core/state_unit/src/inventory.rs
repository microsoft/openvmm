// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The state-unit inventory: the registered unit names, which a snapshot
//! records and restore requires to match exactly.

use super::StateUnits;

impl StateUnits {
    /// Returns every registered state-unit name in stable registration order.
    pub fn inventory(&self) -> Vec<String> {
        self.inner
            .lock()
            .units
            .values()
            .map(|unit| unit.name.to_string())
            .collect()
    }

    /// Verifies that a saved inventory exactly matches the registered units.
    pub fn validate_inventory(&self, saved_inventory: &[String]) -> anyhow::Result<()> {
        let current_inventory = self.inventory();
        anyhow::ensure!(
            saved_inventory == current_inventory,
            "saved state-unit inventory does not match the current machine: saved={saved_inventory:?}, current={current_inventory:?}"
        );
        Ok(())
    }
}
