// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI capabilities.

pub use self::extended::PciExtendedCapability;
pub use self::read_only::ReadOnlyCapability;

use crate::spec::caps::CapabilityId;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use inspect::Inspect;
use vmcore::save_restore::ProtobufSaveRestore;

pub mod extended;
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

    /// Read a byte-enabled DWORD at the given capability-relative offset.
    /// The offset must be 32-bit aligned.
    fn read(&self, offset: u16, value: ByteEnabledDwordRead<'_>);

    /// Write a byte-enabled DWORD to the given capability-relative offset.
    /// The offset must be 32-bit aligned.
    fn write(&mut self, offset: u16, val: ByteEnabledDwordWrite);

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

    /// Downcast to MSI capability
    fn as_msi_cap(&self) -> Option<&msi_cap::MsiCapability> {
        None
    }

    /// Downcast to MSI capability (mutable)
    fn as_msi_cap_mut(&mut self) -> Option<&mut msi_cap::MsiCapability> {
        None
    }
}
