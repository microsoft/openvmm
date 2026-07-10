// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SMMUv3 Stream Table Entry (STE) definitions.
//!
//! Each STE is 64 bytes (512 bits). The STE describes how the SMMU processes
//! transactions for a given stream (device).

use bitfield_struct::bitfield;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Stream table entry size in bytes.
pub const STE_SIZE: usize = 64;

/// Stream table entry (64 bytes).
///
/// Only the first two quadwords have defined fields for stage 1 translation.
/// The remaining quadwords are used for stage 2 and other optional features.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Ste {
    /// Quadword 0: Valid, Config, S1 context pointer.
    pub qw0: SteDw0,
    /// Quadword 1: Stage 1 attributes, stream world.
    pub qw1: SteDw1,
    /// Quadwords 2-7: Stage 2 fields (unused for S1-only).
    pub _qw2_7: [u64; 6],
}

impl Ste {
    /// Returns true if the STE is valid (V bit set).
    pub fn valid(&self) -> bool {
        self.qw0.v()
    }

    /// Returns the stream configuration.
    pub fn config(&self) -> SteConfig {
        SteConfig(self.qw0.config())
    }

    /// Returns the stage 1 context descriptor pointer (physical address).
    ///
    /// The pointer is stored in bits `[55:6]` of QW0, so the actual address
    /// is the stored value shifted left by 6.
    pub fn s1_context_ptr(&self) -> u64 {
        self.qw0.s1_context_ptr() << 6
    }

    /// Returns the S1CDMax field (log2 of number of context descriptors).
    pub fn s1_cd_max(&self) -> u8 {
        self.qw0.s1_cd_max()
    }

    /// Returns the S1Fmt field (CD table format).
    pub fn s1_fmt(&self) -> u8 {
        self.qw0.s1_fmt()
    }
}

/// STE QW0 (bits `[63:0]`): Valid, Config, S1 pointers.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SteDw0 {
    /// Valid bit.
    pub v: bool,
    /// Stream configuration.
    #[bits(3)]
    pub config: u8,
    /// Stage 1 CD table format (0=linear, 1=2-level 4KB, 2=2-level 64KB).
    #[bits(2)]
    pub s1_fmt: u8,
    /// Stage 1 context descriptor pointer, bits `[55:6]` (address >> 6).
    #[bits(50)]
    pub s1_context_ptr: u64,
    #[bits(3)]
    _reserved: u64,
    /// Log2(number of CDs). 0 = single CD.
    #[bits(5)]
    pub s1_cd_max: u8,
}

/// STE QW1 (bits `[127:64]`): Stage 1 attributes, stream world, overrides.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SteDw1 {
    /// S1 default substream behavior (bits `[65:64]`).
    #[bits(2)]
    pub s1_dss: u8,
    /// CD pointer inner cacheability (bits `[67:66]`).
    #[bits(2)]
    pub s1_cir: u8,
    /// CD pointer outer cacheability (bits `[69:68]`).
    #[bits(2)]
    pub s1_cor: u8,
    /// CD pointer shareability (bits `[71:70]`).
    #[bits(2)]
    pub s1_csh: u8,
    /// S2HWU59-62 (bits `[75:72]`, RES0 for S1-only emulator).
    #[bits(4)]
    _s2hwu: u64,
    /// Destructive Read Enable (bit `[76]`).
    pub dre: bool,
    /// Contiguous hint — 4-bit span (bits `[80:77]`).
    #[bits(4)]
    pub cont: u8,
    /// Directed Cache Prefetch (bit `[81]`).
    pub dcp: bool,
    /// PPAR (bit `[82]`).
    pub ppar: bool,
    /// MEV — merge events (bit `[83]`).
    pub mev: bool,
    /// Software reserved (bits `[87:84]`).
    #[bits(4)]
    pub sw_reserved: u8,
    /// S1PIE (bit `[88]`).
    pub s1pie: bool,
    /// S2FWB (bit `[89]`).
    pub s2fwb: bool,
    /// S1MPAM (bit `[90]`).
    pub s1mpam: bool,
    /// S1STALLD — stage 1 stall disable (bit `[91]`).
    pub s1stalld: bool,
    /// EATS — ATS behavior (bits `[93:92]`).
    #[bits(2)]
    pub eats: u8,
    /// Stream world (bits `[95:94]`).
    #[bits(2)]
    pub strw: u8,
    /// Memory attribute for bypass (bits `[99:96]`).
    #[bits(4)]
    pub mem_attr: u8,
    /// Memory type config override (bit `[100]`).
    pub mtcfg: bool,
    /// Allocation hints override (bits `[104:101]`).
    #[bits(4)]
    pub alloccfg: u8,
    #[bits(3)]
    _reserved0: u64,
    /// Shareability override (bits `[109:108]`).
    #[bits(2)]
    pub shcfg: u8,
    /// NS configuration (bits `[111:110]`).
    #[bits(2)]
    pub nscfg: u8,
    /// Privilege override (bits `[113:112]`).
    #[bits(2)]
    pub privcfg: u8,
    /// Instruction/data override (bits `[115:114]`).
    #[bits(2)]
    pub instcfg: u8,
    /// Implementation defined (bits `[127:116]`).
    #[bits(12)]
    _impl_def: u64,
}

open_enum! {
    /// STE Config field values (bits `[3:1]` of DW0).
    pub enum SteConfig: u8 {
        /// Abort: all transactions are aborted.
        ABORT = 0b000,
        /// Bypass: S1 bypass, S2 bypass (identity mapping).
        BYPASS = 0b100,
        /// S1 Translate, S2 Bypass.
        S1_TRANS = 0b101,
        /// S1 Bypass, S2 Translate.
        S2_TRANS = 0b110,
        /// S1 Translate, S2 Translate.
        S1S2_TRANS = 0b111,
    }
}

open_enum! {
    /// STE S1Fmt (CD table format) values.
    pub enum S1Fmt: u8 {
        /// Linear CD table.
        LINEAR = 0b00,
        /// 2-level CD table, 4KB L2.
        TWO_LEVEL_4K = 0b01,
        /// 2-level CD table, 64KB L2.
        TWO_LEVEL_64K = 0b10,
    }
}

open_enum! {
    /// STE stream world values.
    pub enum Strw: u8 {
        /// Non-secure EL1.
        NS_EL1 = 0b00,
        /// Non-secure EL2.
        NS_EL2 = 0b10,
        /// EL2 with E2H.
        EL2_E2H = 0b11,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ste_size() {
        assert_eq!(size_of::<Ste>(), STE_SIZE);
    }

    #[test]
    fn test_ste_valid() {
        let ste = Ste {
            qw0: SteDw0::new(),
            qw1: SteDw1::new(),
            _qw2_7: [0; 6],
        };
        assert!(!ste.valid());

        let ste = Ste {
            qw0: SteDw0::new().with_v(true),
            ..ste
        };
        assert!(ste.valid());
    }

    #[test]
    fn test_ste_config_values() {
        assert_eq!(SteConfig::ABORT.0, 0b000);
        assert_eq!(SteConfig::BYPASS.0, 0b100);
        assert_eq!(SteConfig::S1_TRANS.0, 0b101);
        assert_eq!(SteConfig::S2_TRANS.0, 0b110);
        assert_eq!(SteConfig::S1S2_TRANS.0, 0b111);
    }

    #[test]
    fn test_ste_bypass() {
        let ste = Ste {
            qw0: SteDw0::new().with_v(true).with_config(SteConfig::BYPASS.0),
            qw1: SteDw1::new(),
            _qw2_7: [0; 6],
        };

        assert!(ste.valid());
        assert_eq!(ste.config(), SteConfig::BYPASS);
    }

    #[test]
    fn test_ste_s1_trans() {
        let cd_addr: u64 = 0x8000_0000;
        let ste = Ste {
            qw0: SteDw0::new()
                .with_v(true)
                .with_config(SteConfig::S1_TRANS.0)
                .with_s1_fmt(S1Fmt::LINEAR.0)
                .with_s1_context_ptr(cd_addr >> 6)
                .with_s1_cd_max(0),
            qw1: SteDw1::new()
                .with_s1_cir(0b01)
                .with_s1_cor(0b01)
                .with_s1_csh(0b11)
                .with_strw(Strw::NS_EL1.0),
            _qw2_7: [0; 6],
        };

        assert!(ste.valid());
        assert_eq!(ste.config(), SteConfig::S1_TRANS);
        assert_eq!(ste.s1_context_ptr(), cd_addr);
        assert_eq!(ste.s1_cd_max(), 0);
        assert_eq!(ste.s1_fmt(), S1Fmt::LINEAR.0);
    }
}
