// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCIe extended capabilities.

use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
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

    /// Read a byte-enabled DWORD at the given capability-relative offset.
    /// The offset must be 32-bit aligned.
    fn read(&self, offset: u16, value: ByteEnabledDwordRead<'_>);

    /// Write a byte-enabled DWORD to the given capability-relative offset.
    /// The offset must be 32-bit aligned.
    fn write(&mut self, offset: u16, val: ByteEnabledDwordWrite);

    /// Reset the capability.
    fn reset(&mut self);
}

#[cfg(test)]
pub(crate) fn assert_extended_header_contract(cap: &dyn PciExtendedCapability) {
    let mut value = 0;
    cap.read(0, ByteEnabledDwordRead::with_all_bytes_enabled(&mut value));
    let expected =
        u32::from(cap.extended_capability_id()) | (u32::from(cap.capability_version()) << 16);

    // Capability-local header must contain ID+Version only.
    // Next-pointer bits are injected by cfg_space_emu list traversal.
    assert_eq!(value & 0x000f_ffff, expected);
    assert_eq!(value & 0xfff0_0000, 0);
}
