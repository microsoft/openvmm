// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SMMUv3 Context Descriptor (CD) definitions.
//!
//! Each CD is 64 bytes (512 bits). The CD contains stage 1 translation table
//! pointers and ASID for a given stream/substream.

use bitfield_struct::bitfield;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Context descriptor size in bytes.
pub const CD_SIZE: usize = 64;

/// Context descriptor (64 bytes).
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Cd {
    /// Quadword 0: T0SZ, TG0, cacheability, EPD0, V, IPS, ASID, etc.
    pub qw0: CdDw0,
    /// Quadword 1: TTB0.
    pub qw1: CdDw1,
    /// Quadword 2: TTB1 (unused for TTB0-only translation).
    pub _qw2: u64,
    /// MAIR0 (Memory Attribute Indirection Register 0).
    pub mair0: u64,
    /// MAIR1 (Memory Attribute Indirection Register 1).
    pub mair1: u64,
    /// Quadwords 5-7: AMAIR, PARTID, permission indirection, etc.
    pub _qw5_7: [u64; 3],
}

impl Cd {
    /// Returns true if the CD is valid (V bit set).
    pub fn valid(&self) -> bool {
        self.qw0.v()
    }

    /// Returns the TTB0 physical address.
    ///
    /// TTB0 is stored in QW1 as address bits `[55:4]`.
    /// The actual address is the stored value shifted left by 4.
    pub fn ttb0(&self) -> u64 {
        self.qw1.ttb0() << 4
    }

    /// Returns T0SZ (VA region size for TTB0).
    pub fn t0sz(&self) -> u8 {
        self.qw0.t0sz()
    }

    /// Returns TG0 (granule size for TTB0).
    pub fn tg0(&self) -> Tg0 {
        Tg0(self.qw0.tg0())
    }

    /// Returns IPS (intermediate physical address size).
    pub fn ips(&self) -> Ips {
        Ips(self.qw0.ips())
    }

    /// Returns the ASID.
    pub fn asid(&self) -> u16 {
        self.qw0.asid()
    }

    /// Returns true if AA64 mode (VMSAv8-64) is selected.
    pub fn aa64(&self) -> bool {
        self.qw0.aa64()
    }

    /// Returns true if TTB0 walks are disabled (EPD0=1).
    pub fn epd0(&self) -> bool {
        self.qw0.epd0()
    }
}

/// CD QW0 (bits `[63:0]`): T0SZ, TG0, cacheability, EPD0, V, IPS, ASID, etc.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CdDw0 {
    /// VA region size for TTB0. VA range = 2^(64 - T0SZ).
    #[bits(6)]
    pub t0sz: u8,
    /// TTB0 granule size.
    #[bits(2)]
    pub tg0: u8,
    /// TTB0 inner cacheability.
    #[bits(2)]
    pub ir0: u8,
    /// TTB0 outer cacheability.
    #[bits(2)]
    pub or0: u8,
    /// TTB0 shareability.
    #[bits(2)]
    pub sh0: u8,
    /// Disable TTB0 walk (1 = fault on miss).
    pub epd0: bool,
    /// Translation table endianness (0=LE, 1=BE).
    pub endi: bool,
    /// VA region size for TTB1.
    #[bits(6)]
    pub t1sz: u8,
    /// TTB1 granule size.
    #[bits(2)]
    pub tg1: u8,
    /// TTB1 inner cacheability.
    #[bits(2)]
    pub ir1: u8,
    /// TTB1 outer cacheability.
    #[bits(2)]
    pub or1: u8,
    /// TTB1 shareability.
    #[bits(2)]
    pub sh1: u8,
    /// Disable TTB1 walk.
    pub epd1: bool,
    /// CD valid bit.
    pub v: bool,
    /// Intermediate physical address size.
    #[bits(3)]
    pub ips: u8,
    /// Access flag fault disable.
    pub affd: bool,
    /// Write implies XN.
    pub wxn: bool,
    /// Unprivileged write implies XN.
    pub uwxn: bool,
    /// Top byte ignore for TTB0 addresses.
    pub tbi0: bool,
    /// Top byte ignore for TTB1 addresses.
    pub tbi1: bool,
    /// Privileged Access Never.
    pub pan: bool,
    /// VMSAv8-64 mode (must be 1 for AArch64 page tables).
    pub aa64: bool,
    /// HW dirty bit management.
    pub hd: bool,
    /// HW access flag update.
    pub ha: bool,
    /// Stall (0=terminate, 1=stall on fault).
    pub s: bool,
    /// Non-shareable → OSH upgrade.
    pub r: bool,
    /// Abort flag.
    pub a: bool,
    /// ASID set (for TLB invalidation).
    pub aset: bool,
    /// ASID (16-bit).
    #[bits(16)]
    pub asid: u16,
}

/// CD QW1 (bits `[127:64]`): TTB0.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct CdDw1 {
    /// Control bits (HAFT, E0PD0, NSCFG0, DisCH0).
    #[bits(4)]
    pub control: u8,
    /// TTB0 address bits `[55:4]`. Actual address = stored << 4.
    #[bits(52)]
    pub ttb0: u64,
    /// HW use fields (HWU0xx).
    #[bits(4)]
    pub hwu: u8,
    /// SKL0 (start level override, if supported).
    #[bits(2)]
    pub skl0: u8,
    #[bits(2)]
    _reserved: u64,
}

open_enum! {
    /// TTB0 granule size (CD DW0 TG0 field).
    pub enum Tg0: u8 {
        /// 4KB granule.
        GRAN_4K = 0b00,
        /// 64KB granule.
        GRAN_64K = 0b01,
        /// 16KB granule.
        GRAN_16K = 0b10,
    }
}

open_enum! {
    /// Intermediate Physical Address Size (CD DW0 IPS field).
    pub enum Ips: u8 {
        /// 32-bit (4GB).
        IPS_32 = 0b000,
        /// 36-bit (64GB).
        IPS_36 = 0b001,
        /// 40-bit (1TB).
        IPS_40 = 0b010,
        /// 42-bit (4TB).
        IPS_42 = 0b011,
        /// 44-bit (16TB).
        IPS_44 = 0b100,
        /// 48-bit (256TB).
        IPS_48 = 0b101,
        /// 52-bit (4PB).
        IPS_52 = 0b110,
    }
}

impl Ips {
    /// Returns the number of physical address bits for this IPS value,
    /// or `None` if the value is not a recognized encoding.
    pub fn bits(self) -> Option<u8> {
        Some(match self {
            Self::IPS_32 => 32,
            Self::IPS_36 => 36,
            Self::IPS_40 => 40,
            Self::IPS_42 => 42,
            Self::IPS_44 => 44,
            Self::IPS_48 => 48,
            Self::IPS_52 => 52,
            _ => return None,
        })
    }

    /// Returns the IPS encoding for a given number of address bits.
    /// Rounds down to the nearest supported encoding if the value is
    /// not an exact match (e.g., 39 → 36-bit).
    pub fn from_bits(bits: u8) -> Self {
        match bits {
            52..=u8::MAX => Self::IPS_52,
            48..=51 => Self::IPS_48,
            44..=47 => Self::IPS_44,
            42..=43 => Self::IPS_42,
            40..=41 => Self::IPS_40,
            36..=39 => Self::IPS_36,
            _ => Self::IPS_32,
        }
    }
}

impl Tg0 {
    /// Returns the granule size in bytes, or `None` if the value is not
    /// a recognized encoding.
    pub fn granule_size(self) -> Option<u64> {
        Some(match self {
            Self::GRAN_4K => 4096,
            Self::GRAN_16K => 16384,
            Self::GRAN_64K => 65536,
            _ => return None,
        })
    }

    /// Returns the number of bits per page table level index, or `None`
    /// if the value is not a recognized encoding.
    pub fn bits_per_level(self) -> Option<u8> {
        Some(match self {
            Self::GRAN_4K => 9,
            Self::GRAN_16K => 11,
            Self::GRAN_64K => 13,
            _ => return None,
        })
    }

    /// Returns the page offset bits (log2 of granule size), or `None`
    /// if the value is not a recognized encoding.
    pub fn page_shift(self) -> Option<u8> {
        Some(match self {
            Self::GRAN_4K => 12,
            Self::GRAN_16K => 14,
            Self::GRAN_64K => 16,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cd_size() {
        assert_eq!(size_of::<Cd>(), CD_SIZE);
    }

    #[test]
    fn test_cd_valid() {
        let cd = new_cd();
        assert!(!cd.valid());

        let cd = Cd {
            qw0: CdDw0::new().with_v(true),
            ..new_cd()
        };
        assert!(cd.valid());
    }

    fn new_cd() -> Cd {
        Cd {
            qw0: CdDw0::new(),
            qw1: CdDw1::new(),
            _qw2: 0,
            mair0: 0,
            mair1: 0,
            _qw5_7: [0; 3],
        }
    }

    #[test]
    fn test_cd_ttb0_address() {
        let ttb0_addr: u64 = 0x4000_0000;
        let cd = Cd {
            qw1: CdDw1::new().with_ttb0(ttb0_addr >> 4),
            ..new_cd()
        };
        assert_eq!(cd.ttb0(), ttb0_addr);
    }

    #[test]
    fn test_cd_ttb0_large_address() {
        let ttb0_addr: u64 = 0x00FF_FFFF_F000;
        let cd = Cd {
            qw1: CdDw1::new().with_ttb0(ttb0_addr >> 4),
            ..new_cd()
        };
        assert_eq!(cd.ttb0(), ttb0_addr);
    }

    #[test]
    fn test_tg0_granule_sizes() {
        assert_eq!(Tg0::GRAN_4K.granule_size(), Some(4096));
        assert_eq!(Tg0::GRAN_16K.granule_size(), Some(16384));
        assert_eq!(Tg0::GRAN_64K.granule_size(), Some(65536));
        assert_eq!(Tg0(0b11).granule_size(), None);
    }

    #[test]
    fn test_tg0_bits_per_level() {
        assert_eq!(Tg0::GRAN_4K.bits_per_level(), Some(9));
        assert_eq!(Tg0::GRAN_16K.bits_per_level(), Some(11));
        assert_eq!(Tg0::GRAN_64K.bits_per_level(), Some(13));
    }

    #[test]
    fn test_tg0_page_shift() {
        assert_eq!(Tg0::GRAN_4K.page_shift(), Some(12));
        assert_eq!(Tg0::GRAN_16K.page_shift(), Some(14));
        assert_eq!(Tg0::GRAN_64K.page_shift(), Some(16));
    }

    #[test]
    fn test_ips_bits() {
        assert_eq!(Ips::IPS_32.bits(), Some(32));
        assert_eq!(Ips::IPS_36.bits(), Some(36));
        assert_eq!(Ips::IPS_40.bits(), Some(40));
        assert_eq!(Ips::IPS_42.bits(), Some(42));
        assert_eq!(Ips::IPS_44.bits(), Some(44));
        assert_eq!(Ips::IPS_48.bits(), Some(48));
        assert_eq!(Ips::IPS_52.bits(), Some(52));
        assert_eq!(Ips(0b111).bits(), None);
    }

    #[test]
    fn test_cd_epd0_disables_walk() {
        let cd = Cd {
            qw0: CdDw0::new().with_v(true).with_epd0(true),
            ..new_cd()
        };

        assert!(cd.valid());
        assert!(cd.epd0());
    }

    #[test]
    fn test_translation_context_from_cd() {
        let cd = Cd {
            qw0: CdDw0::new()
                .with_t0sz(16) // 48-bit VA
                .with_tg0(Tg0::GRAN_4K.0)
                .with_ips(Ips::IPS_48.0)
                .with_v(true)
                .with_aa64(true),
            ..new_cd()
        };

        let tg0 = cd.tg0();
        let va_bits = 64 - cd.t0sz() as u32;
        let page_shift = tg0.page_shift().unwrap() as u32;
        let bits_per_level = tg0.bits_per_level().unwrap() as u32;

        assert_eq!(va_bits, 48);
        assert_eq!(page_shift, 12);
        assert_eq!(bits_per_level, 9);

        // For 4K/48-bit: start at level 0, 4 levels
        let total_bits = va_bits - page_shift;
        let num_levels = total_bits.div_ceil(bits_per_level);
        assert_eq!(num_levels, 4);
    }
}
