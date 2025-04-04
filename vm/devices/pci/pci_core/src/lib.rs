// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core PCI infrastructure.
//!
//! A collection of constants, types, and traits that are shared across all PCI
//! implementations (i.e: vpci, pci_gen1, pcie).

pub mod test_helpers;

pub mod bar_mapping;
pub mod capabilities;
pub mod cfg_space_emu;
pub mod chipset_device_ext;
pub mod msi;
pub mod spec;

/// Defines one of the 4 legacy PCI INTx shared interrupt pins
#[expect(missing_docs)] // self explanatory variants
#[derive(Debug, Clone, Copy, inspect::Inspect)]
pub enum PciInterruptPin {
    IntA = 0,
    IntB,
    IntC,
    IntD,
}
