// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCIe extended capabilities.

use inspect::Inspect;
use vmcore::save_restore::ProtobufSaveRestore;

pub mod acs;

/// A generic PCIe extended capability structure.
pub trait PciExtendedCapability: Send + Sync + Inspect + ProtobufSaveRestore {
    /// A descriptive label for use in Save/Restore + Inspect output.
    fn label(&self) -> &str;

    /// Returns the PCIe extended capability ID for this capability.
    fn extended_capability_id(&self) -> u16;

    /// Returns this extended capability structure version.
    fn capability_version(&self) -> u8;

    /// Length of the extended capability structure in bytes.
    ///
    /// Implementations must satisfy all of the following invariants:
    /// - Length must be non-zero.
    /// - Length must be 32-bit aligned (a multiple of 4).
    /// - When packed into config space by `cfg_space_emu` starting at 0x100,
    ///   the cumulative size of all extended capabilities must not exceed
    ///   0x1000.
    fn len(&self) -> usize;

    /// Read a u32 at the given capability-relative offset.
    fn read_u32(&self, offset: u16) -> u32;

    /// Write a u32 at the given capability-relative offset.
    fn write_u32(&mut self, offset: u16, val: u32);

    /// Reset the capability.
    fn reset(&mut self);
}

#[cfg(test)]
pub(crate) fn assert_extended_header_contract(cap: &dyn PciExtendedCapability) {
    let value = cap.read_u32(0);
    let expected =
        u32::from(cap.extended_capability_id()) | (u32::from(cap.capability_version()) << 16);

    // Capability-local header must contain ID+Version only.
    // Next-pointer bits are injected by cfg_space_emu list traversal.
    assert_eq!(value & 0x000f_ffff, expected);
    assert_eq!(value & 0xfff0_0000, 0);
}
