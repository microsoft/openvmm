// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86 definitions of non-translated MSI address and data.

use crate::apic::APIC_BASE_ADDRESS;
use bitfield_struct::bitfield;

/// The layout of the MSI address element.
#[bitfield(u32)]
pub struct MsiAddress {
    #[bits(2)]
    _reserved: u32,
    pub destination_mode_logical: bool,
    pub redirection_hint: bool,
    pub extended_destination: u8,
    pub destination: u8,
    #[bits(12)]
    pub address: u16,
}

/// The expected value for MsiAddress::address.
pub const MSI_ADDRESS: u16 = (APIC_BASE_ADDRESS >> 20) as u16;

impl MsiAddress {
    /// Returns a 15-bit destination encoded in the MSI address. This is not
    /// architectural--normally only an 8-bit destination is supported unless
    /// interrupt redirection is enabled--but this is supported by some
    /// virtualization platforms (including Hyper-V and KVM).
    ///
    /// The high 7 bits are encoded as the high 7 bits of the extended
    /// destination field. The low bit of that field is ignored and presumed to
    /// be zero in this configuration.
    pub fn virt_destination(&self) -> u16 {
        self.destination() as u16 | ((self.extended_destination() as u16 & !1) << 7)
    }

    /// Returns a value with a 15-bit destination encoded as guests expect when
    /// running with Hyper-V or KVM virtualization extensions.
    ///
    /// This updates the destination and extended destination fields.
    pub fn with_virt_destination(self, destination: u16) -> Self {
        self.with_destination(destination as u8)
            .with_extended_destination((destination >> 7) as u8 & !1)
    }

    /// Updates the value with a 15-bit destination encoded as guests expect
    /// when running with Hyper-V or KVM virtualization extensions.
    ///
    /// This updates the destination and extended destination fields.
    pub fn set_virt_destination(&mut self, destination: u16) {
        *self = self.with_virt_destination(destination);
    }
}

/// The layout of the MSI data element.
///
/// Note that the significant bits correspond to low bits of
/// [`Icr`](super::apic::Icr).
#[bitfield(u32)]
pub struct MsiData {
    pub vector: u8,
    #[bits(3)]
    pub delivery_mode: u8,
    pub destination_mode_logical: bool,
    #[bits(2)]
    _reserved: u8,
    pub assert: bool,
    pub trigger_mode_level: bool,
    _reserved: u16,
}
