// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The state-unit inventory: the part of the VM's contract that each unit
//! contributes. A snapshot records the inventory together with the units'
//! saved state, and a restore checks it before it applies that state.
//!
//! Each unit declares its contract when it is added, through
//! [`UnitBuilder`](crate::UnitBuilder): a guest-visible stable identity, a
//! kind, a [`UnitLifetime`], and descriptors of what the guest sees of it. A
//! save records the contracts of the units that it saved, from the same
//! snapshot of the unit set as their saved state, so a unit removed while the
//! save runs appears in both or in neither. A restore requires the
//! destination's static and derived units to match the recorded ones exactly,
//! whatever order either VM registered them in, and plans the re-creation of
//! the recorded dynamic units that the destination lacks.

use super::SavedStateUnit;
use super::State;
use super::StateRequest;
use super::StateTransitionError;
use super::StateUnits;
use super::extract;
use mesh::payload::Protobuf;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use thiserror::Error;

/// How a state unit became part of the VM.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Protobuf)]
#[mesh(package = "state_unit")]
pub enum UnitLifetime {
    /// The VM's configuration created the unit, so a restore's destination
    /// must have it already.
    #[mesh(1)]
    Static,
    /// The unit was added while the VM ran, such as a hot-plugged device. A
    /// restore whose destination lacks it re-creates it from its contract.
    #[mesh(2)]
    Dynamic,
    /// VMM infrastructure that other units imply, such as VM time or an
    /// interrupt line set. A restore's destination must have it already.
    #[mesh(3)]
    Derived,
}

impl UnitLifetime {
    /// Returns the lifetime's name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Dynamic => "dynamic",
            Self::Derived => "derived",
        }
    }
}

impl fmt::Display for UnitLifetime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A guest-visible property of a state unit, such as an address range that its
/// device decodes or the PCI ID that it reports.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Protobuf)]
#[mesh(package = "state_unit")]
pub struct UnitDescriptor {
    /// What the property is, such as `pio` or `pci.id`.
    #[mesh(1)]
    pub key: String,
    /// The property's value, in a canonical text form.
    #[mesh(2)]
    pub value: String,
}

impl UnitDescriptor {
    /// Returns a descriptor of `key` with `value`.
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }
}

impl fmt::Display for UnitDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={}", self.key, self.value)
    }
}

/// The part of the VM's contract that a state unit contributes, which a
/// snapshot records for every unit, including those with no saved state.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "state_unit")]
pub struct UnitContract {
    /// The unit's name, which keys its saved state.
    #[mesh(1)]
    pub name: String,
    /// The guest-visible identity of what the unit implements, such as the
    /// PCIe port that its device occupies; the unit's name unless declared.
    #[mesh(2)]
    pub stable_id: String,
    /// What the unit implements, such as its device's resource kind; empty
    /// unless declared.
    #[mesh(3)]
    pub kind: String,
    /// How the unit became part of the VM.
    #[mesh(4)]
    pub lifetime: UnitLifetime,
    /// The unit's guest-visible properties, in canonical order.
    #[mesh(5)]
    pub descriptors: Vec<UnitDescriptor>,
}

impl fmt::Display for UnitContract {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}", self.name, self.lifetime)?;
        if self.stable_id != self.name {
            write!(f, ", stable id {}", self.stable_id)?;
        }
        if !self.kind.is_empty() {
            write!(f, ", kind {}", self.kind)?;
        }
        for descriptor in &self.descriptors {
            write!(f, ", {descriptor}")?;
        }
        f.write_str(")")
    }
}

/// The contract that a unit declares when it is added, without its name.
#[derive(Clone, Debug)]
pub(crate) struct UnitIdentity {
    /// The declared stable identity, or `None` for the unit's name.
    pub(crate) stable_id: Option<String>,
    pub(crate) kind: String,
    pub(crate) lifetime: UnitLifetime,
    /// Sorted, without duplicates.
    descriptors: Vec<UnitDescriptor>,
}

impl Default for UnitIdentity {
    fn default() -> Self {
        Self {
            stable_id: None,
            kind: String::new(),
            lifetime: UnitLifetime::Static,
            descriptors: Vec::new(),
        }
    }
}

impl UnitIdentity {
    /// Adds `descriptor` in canonical order, unless it is already present.
    pub(crate) fn add_descriptor(&mut self, descriptor: UnitDescriptor) {
        if let Err(index) = self.descriptors.binary_search(&descriptor) {
            self.descriptors.insert(index, descriptor);
        }
    }

    /// Returns the contract of the unit named `name`.
    pub(crate) fn contract(&self, name: &str) -> UnitContract {
        UnitContract {
            name: name.to_owned(),
            stable_id: self.stable_id.clone().unwrap_or_else(|| name.to_owned()),
            kind: self.kind.clone(),
            lifetime: self.lifetime,
            descriptors: self.descriptors.clone(),
        }
    }
}

/// What a restore must do, before it applies saved state, for its destination
/// to satisfy the saved inventory: the plan that
/// [`StateUnits::check_inventory`] returns.
#[derive(Debug, Default)]
pub struct InventoryPlan {
    /// The recorded dynamic units that the destination lacks, ordered by name,
    /// which the caller must re-create from their contracts before it checks
    /// the inventory again.
    pub recreate: Vec<UnitContract>,
}

/// A saved inventory that the state units do not satisfy.
#[derive(Debug, Default, Error)]
pub struct InventoryMismatch {
    /// Recorded static or derived units that the state units lack.
    pub missing: Vec<UnitContract>,
    /// State units that the inventory does not record.
    pub unexpected: Vec<UnitContract>,
    /// State units whose contract differs from the recorded one: each recorded
    /// contract, with the unit's.
    pub changed: Vec<(UnitContract, UnitContract)>,
    /// Names that the inventory records more than once.
    pub duplicates: Vec<String>,
    /// Names of saved state that the inventory does not record.
    pub unrecorded: Vec<String>,
}

impl InventoryMismatch {
    fn is_empty(&self) -> bool {
        let Self {
            missing,
            unexpected,
            changed,
            duplicates,
            unrecorded,
        } = self;
        missing.is_empty()
            && unexpected.is_empty()
            && changed.is_empty()
            && duplicates.is_empty()
            && unrecorded.is_empty()
    }
}

impl fmt::Display for InventoryMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn section<T: fmt::Display>(
            f: &mut fmt::Formatter<'_>,
            first: &mut bool,
            title: &str,
            items: impl IntoIterator<Item = T>,
        ) -> fmt::Result {
            let mut items = items.into_iter().peekable();
            if items.peek().is_none() {
                return Ok(());
            }
            f.write_str(if *first { ": " } else { "; " })?;
            *first = false;
            write!(f, "{title} [")?;
            for (index, item) in items.enumerate() {
                if index > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{item}")?;
            }
            f.write_str("]")
        }

        let Self {
            missing,
            unexpected,
            changed,
            duplicates,
            unrecorded,
        } = self;
        f.write_str("the saved state-unit inventory does not match the VM")?;
        let mut first = true;
        section(f, &mut first, "missing", missing)?;
        section(f, &mut first, "unexpected", unexpected)?;
        section(
            f,
            &mut first,
            "changed",
            changed
                .iter()
                .map(|(saved, current)| format!("saved {saved} is now {current}")),
        )?;
        section(f, &mut first, "recorded more than once", duplicates)?;
        section(f, &mut first, "saved state without a record", unrecorded)
    }
}

impl StateUnits {
    /// Returns the contract of every unit, ordered by name.
    pub fn inventory(&self) -> Vec<UnitContract> {
        let mut inventory = self
            .inner
            .lock()
            .units
            .values()
            .map(|unit| unit.identity.contract(&unit.name))
            .collect::<Vec<_>>();
        inventory.sort_by(|a, b| a.name.cmp(&b.name));
        inventory
    }

    /// Saves all the state units, as [`Self::save`] does, and returns their
    /// contracts, ordered by name, with their saved state.
    ///
    /// Both come from the same snapshot of the unit set and cover exactly the
    /// units that answered the save: a unit removed while the save runs
    /// appears in both if it answered, and in neither if it did not.
    ///
    /// Fails without sending a request while a unit is in a terminal state,
    /// whether or not the units run. Otherwise panics if running.
    pub async fn save_with_inventory(
        &mut self,
    ) -> Result<(Vec<SavedStateUnit>, Vec<UnitContract>), StateTransitionError> {
        self.check_no_terminal_units("save")?;
        assert!(!self.running);
        // Save can occur in any order since it will not observably mutate
        // state.
        let (results, ready_set) = self
            .run_op_tracked(
                "save",
                None,
                State::Stopped,
                State::Saving,
                State::Stopped,
                StateRequest::Save,
                |_, _| Some(()),
                |_| None,
            )
            .await;

        let mut inventory = Vec::with_capacity(results.len());
        let results = results
            .into_iter()
            .map(|(id, name, result)| {
                inventory.push(ready_set.0[&id].identity.contract(&name));
                (name, result)
            })
            .collect::<Vec<_>>();
        let states = extract("save", results, |name, state| {
            state.map(|state| SavedStateUnit {
                name: name.to_string(),
                state,
            })
        })?;
        inventory.sort_by(|a, b| a.name.cmp(&b.name));
        Ok((states, inventory))
    }

    /// Checks that the state units satisfy a saved inventory, and that the
    /// inventory records each unit whose saved state `states` holds, before a
    /// restore applies that state.
    ///
    /// The units satisfy the inventory when their contracts equal the recorded
    /// ones, whatever order either VM registered its units in, except that a
    /// recorded dynamic unit may be missing: the returned plan lists those,
    /// which the caller re-creates from their contracts before checking again.
    /// A unit that the inventory does not record fails the check, whatever its
    /// lifetime.
    pub fn check_inventory(
        &self,
        inventory: &[UnitContract],
        states: &[SavedStateUnit],
    ) -> Result<InventoryPlan, InventoryMismatch> {
        let mut mismatch = InventoryMismatch::default();
        let mut recorded = BTreeMap::new();
        let mut duplicates = BTreeSet::new();
        for contract in inventory {
            if recorded.insert(contract.name.as_str(), contract).is_some() {
                duplicates.insert(contract.name.clone());
            }
        }
        mismatch.duplicates = duplicates.into_iter().collect();
        mismatch.unrecorded = states
            .iter()
            .filter(|state| !recorded.contains_key(state.name.as_str()))
            .map(|state| state.name.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        let current = self.inventory();
        let mut current_by_name = current
            .iter()
            .map(|contract| (contract.name.as_str(), contract))
            .collect::<BTreeMap<_, _>>();
        let mut plan = InventoryPlan::default();
        for (name, &saved) in &recorded {
            match current_by_name.remove(name) {
                Some(unit) => {
                    if unit != saved {
                        mismatch.changed.push((saved.clone(), unit.clone()));
                    }
                }
                None if saved.lifetime == UnitLifetime::Dynamic => {
                    plan.recreate.push(saved.clone());
                }
                None => mismatch.missing.push(saved.clone()),
            }
        }
        mismatch.unexpected = current_by_name.into_values().cloned().collect();

        if mismatch.is_empty() {
            Ok(plan)
        } else {
            Err(mismatch)
        }
    }
}
