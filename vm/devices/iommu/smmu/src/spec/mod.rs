// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SMMUv3 spec-derived type definitions.
//!
//! Register layouts, stream table entries, context descriptors, command/event
//! queue entries, and page table descriptors — all derived from the Arm SMMUv3
//! architecture specification (IHI 0070).
//!
//! This module contains only type definitions, not algorithms.

pub mod cd;
pub mod commands;
pub mod events;
pub mod pt;
pub mod registers;
pub mod ste;

open_enum::open_enum! {
    /// SMMUv3 address-size encoding, shared by `IDR5.OAS` (Output Address Size)
    /// and `CD.IPS` (Intermediate Physical Address Size) — both use the same
    /// 3-bit field values from the architecture's address-size table.
    pub enum AddrSize: u8 {
        /// 32-bit (4GB).
        BITS_32 = 0b000,
        /// 36-bit (64GB).
        BITS_36 = 0b001,
        /// 40-bit (1TB).
        BITS_40 = 0b010,
        /// 42-bit (4TB).
        BITS_42 = 0b011,
        /// 44-bit (16TB).
        BITS_44 = 0b100,
        /// 48-bit (256TB).
        BITS_48 = 0b101,
        /// 52-bit (4PB).
        BITS_52 = 0b110,
    }
}

impl AddrSize {
    /// Returns the number of address bits for this encoding, or `None` if the
    /// value is not a recognized encoding.
    pub fn addr_bits(self) -> Option<u8> {
        Some(match self {
            Self::BITS_32 => 32,
            Self::BITS_36 => 36,
            Self::BITS_40 => 40,
            Self::BITS_42 => 42,
            Self::BITS_44 => 44,
            Self::BITS_48 => 48,
            Self::BITS_52 => 52,
            _ => return None,
        })
    }

    /// Returns the encoding for a given number of address bits, rounding down
    /// to the nearest supported size if not an exact match (e.g. 39 → 36-bit).
    pub fn from_addr_bits(bits: u8) -> Self {
        match bits {
            52..=u8::MAX => Self::BITS_52,
            48..=51 => Self::BITS_48,
            44..=47 => Self::BITS_44,
            42..=43 => Self::BITS_42,
            40..=41 => Self::BITS_40,
            36..=39 => Self::BITS_36,
            _ => Self::BITS_32,
        }
    }

    /// Raw-encoding accessor for `bitfield_struct` field use.
    const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    /// Raw-encoding accessor for `bitfield_struct` field use.
    const fn into_bits(self) -> u8 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::AddrSize;

    #[test]
    fn addr_size_bits_round_trip() {
        assert_eq!(AddrSize::BITS_32.addr_bits(), Some(32));
        assert_eq!(AddrSize::BITS_36.addr_bits(), Some(36));
        assert_eq!(AddrSize::BITS_40.addr_bits(), Some(40));
        assert_eq!(AddrSize::BITS_42.addr_bits(), Some(42));
        assert_eq!(AddrSize::BITS_44.addr_bits(), Some(44));
        assert_eq!(AddrSize::BITS_48.addr_bits(), Some(48));
        assert_eq!(AddrSize::BITS_52.addr_bits(), Some(52));
        assert_eq!(AddrSize(0b111).addr_bits(), None);
    }
}
