// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI capabilities.

pub use self::read_only::ReadOnlyCapability;

use inspect::Inspect;
use vmcore::save_restore::ProtobufSaveRestore;

pub mod msix;
pub mod read_only;

/// A generic PCI configuration space capability structure.
pub trait PciCapability: Send + Sync + Inspect + ProtobufSaveRestore {
    /// A descriptive label for use in Save/Restore + Inspect output
    fn label(&self) -> &str;

    /// Length of the capability structure
    fn len(&self) -> usize;

    /// Read a u32 at the given offset
    fn read_u32(&self, offset: u16) -> u32;

    /// Write a u32 at the given offset
    fn write_u32(&mut self, offset: u16, val: u32);

    /// Reset the capability
    fn reset(&mut self);
}
