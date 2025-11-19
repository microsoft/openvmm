// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI capabilities.

pub use self::read_only::ReadOnlyCapability;

use crate::spec::caps::CapabilityId;
use inspect::Inspect;
use vmcore::save_restore::ProtobufSaveRestore;

pub mod msi_cap;
pub mod msix;
pub mod pci_express;
pub mod read_only;

/// A generic PCI configuration space capability structure.
pub trait PciCapability: Send + Sync + Inspect + ProtobufSaveRestore {
    /// A descriptive label for use in Save/Restore + Inspect output
    fn label(&self) -> &str;

    /// Returns the PCI capability ID for this capability
    fn capability_id(&self) -> CapabilityId;

    /// Length of the capability structure
    fn len(&self) -> usize;

    /// Read a u32 at the given offset
    fn read_u32(&self, offset: u16) -> u32;

    /// Write a u32 at the given offset
    fn write_u32(&mut self, offset: u16, val: u32);

    /// Reset the capability
    fn reset(&mut self);

    // Specific downcast methods for known capability types

    /// Downcast to PCI Express capability
    fn as_pci_express(&self) -> Option<&pci_express::PciExpressCapability> {
        None
    }

    /// Downcast to PCI Express capability (mutable)
    fn as_pci_express_mut(&mut self) -> Option<&mut pci_express::PciExpressCapability> {
        None
    }
}
