// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Definitions for the Generic Interrupt Controller (GIC) registers.

use bitfield_struct::bitfield;
use core::ops::Range;
use open_enum::open_enum;

open_enum! {
    pub enum GicdRegister: u16 {
        CTLR = 0x0000,
        TYPER = 0x0004,
        IIDR = 0x0008,
        TYPER2 = 0x000c,
        STATUSR = 0x0010,
        SETSPI_NSR = 0x0040,
        CLRSPI_NSR = 0x0048,
        SETSPI_SR = 0x0050,
        CLRSPI_SR = 0x0058,
        IGROUPR0 = 0x0080,       // 0x80
        ISENABLER0 = 0x0100,     // 0x80
        ICENABLER0 = 0x0180,     // 0x80
        ISPENDR0 = 0x0200,       // 0x80
        ICPENDR0 = 0x0280,       // 0x80
        ISACTIVER0 = 0x0300,     // 0x80
        ICACTIVER0 = 0x0380,     // 0x80
        IPRIORITYR0 = 0x0400,    // 0x400
        ITARGETSR0 = 0x0800,     // 0x400
        ICFGR0 = 0x0c00,         // 0x100
        IGRPMODR0 = 0x0d00,      // 0x100
        NSACR0 = 0x0e00,         // 0x100
        SGIR = 0x0f00,
        CPENDSGIR0 = 0x0f10,     // 0x10
        SPENDSGIR0 = 0x0f20,     // 0x10
        INMIR0 = 0x0f80,         // 0x80
        IROUTER0 = 0x6000,       // 0x2000, skip first 0x100,
        PIDR2 = 0xffe8,
    }
}

impl GicdRegister {
    pub const IGROUPR: Range<u16> = Self::IGROUPR0.0..Self::IGROUPR0.0 + 0x80;
    pub const ISENABLER: Range<u16> = Self::ISENABLER0.0..Self::ISENABLER0.0 + 0x80;
    pub const ICENABLER: Range<u16> = Self::ICENABLER0.0..Self::ICENABLER0.0 + 0x80;
    pub const ISPENDR: Range<u16> = Self::ISPENDR0.0..Self::ISPENDR0.0 + 0x80;
    pub const ICPENDR: Range<u16> = Self::ICPENDR0.0..Self::ICPENDR0.0 + 0x80;
    pub const ISACTIVER: Range<u16> = Self::ISACTIVER0.0..Self::ISACTIVER0.0 + 0x80;
    pub const ICACTIVER: Range<u16> = Self::ICACTIVER0.0..Self::ICACTIVER0.0 + 0x80;
    pub const ICFGR: Range<u16> = Self::ICFGR0.0..Self::ICFGR0.0 + 0x100;
    pub const IPRIORITYR: Range<u16> = Self::IPRIORITYR0.0..Self::IPRIORITYR0.0 + 0x400;
    pub const IROUTER: Range<u16> = Self::IROUTER0.0..Self::IROUTER0.0 + 0x2000;
}

#[bitfield(u32)]
pub struct GicdTyper {
    #[bits(5)]
    pub it_lines_number: u8,
    #[bits(3)]
    pub cpu_number: u8,
    pub espi: bool,
    pub nmi: bool,
    pub security_extn: bool,
    #[bits(5)]
    pub num_lpis: u8,
    pub mbis: bool,
    pub lpis: bool,
    pub dvis: bool,
    #[bits(5)]
    pub id_bits: u8,
    pub a3v: bool,
    pub no1n: bool,
    pub rss: bool,
    #[bits(5)]
    pub espi_range: u8,
}

#[bitfield(u32)]
pub struct GicdTyper2 {
    #[bits(5)]
    pub vid: u8,
    #[bits(2)]
    _res5_6: u8,
    pub vil: bool,
    pub n_assgi_cap: bool,
    #[bits(23)]
    _res9_31: u32,
}

#[bitfield(u32)]
pub struct GicdCtlr {
    pub enable_grp0: bool,
    pub enable_grp1: bool,
    #[bits(2)]
    _res_2_3: u8,
    pub are: bool,
    _res_5: bool,
    pub ds: bool,
    pub e1nwf: bool,
    pub n_assgi_req: bool,
    #[bits(22)]
    _res_9_30: u32,
    pub rwp: bool,
}

open_enum! {
    pub enum GicrRdRegister: u16 {
        CTLR = 0x0000,
        IIDR = 0x0004,
        TYPER = 0x0008,     // 64 bit
        STATUSR = 0x0010,
        WAKER = 0x0014,
        MPAMIDR = 0x0018,
        PARTIDR = 0x001c,
        SETLPIR = 0x0040,   // 64 bit
        CLRLPIR = 0x0048,   // 64 bit
        PROPBASER = 0x0070, // 64 bit
        PENDBASER = 0x0078, // 64 bit
        INVLPIR = 0x00A0,   // 64 bit
        SYNCR = 0x00C0,     // 64 bit
        PIDR2 = 0xffe8,
    }
}

open_enum! {
    pub enum GicrSgiRegister: u16 {
        IGROUPR0 = 0x0080,
        ISENABLER0 = 0x0100,
        ICENABLER0 = 0x0180,
        ISPENDR0 = 0x0200,
        ICPENDR0 = 0x0280,
        ISACTIVER0 = 0x0300,
        ICACTIVER0 = 0x0380,
        IPRIORITYR0 = 0x0400, // 0x20
        ICFGR0 = 0x0c00,
        ICFGR1 = 0x0c04,
        IGRPMODR0 = 0x0d00,
    }
}

impl GicrSgiRegister {
    pub const IPRIORITYR: Range<u16> = Self::IPRIORITYR0.0..Self::IPRIORITYR0.0 + 0x20;
}

#[bitfield(u64)]
pub struct GicrTyper {
    pub plpis: bool,
    pub vlpis: bool,
    pub dirty: bool,
    pub direct_lpi: bool,
    pub last: bool,
    pub dpgs: bool,
    pub mpam: bool,
    pub rvpeid: bool,
    pub processor_number: u16,
    #[bits(2)]
    pub common_lpi_aff: u8,
    pub vsgi: bool,
    #[bits(5)]
    pub ppi_num: u8,
    pub aff0: u8,
    pub aff1: u8,
    pub aff2: u8,
    pub aff3: u8,
}

#[bitfield(u32)]
pub struct GicrCtlr {
    pub enable_lpis: bool,
    pub ces: bool,
    pub ir: bool,
    pub rwp: bool,
    #[bits(20)]
    _res_4_23: u32,
    pub dpg0: bool,
    pub dpg1ns: bool,
    pub dpg1s: bool,
    #[bits(4)]
    _res_27_30: u32,
    pub uwp: bool,
}

#[bitfield(u32)]
pub struct GicrWaker {
    /// Implementation defined.
    pub bit_0: bool,
    pub processor_sleep: bool,
    pub children_asleep: bool,
    #[bits(28)]
    _res_3_30: u32,
    /// Implementation defined.
    pub bit_31: bool,
}

#[bitfield(u64)]
pub struct GicrSgi {
    pub target_list: u16,
    pub aff1: u8,
    #[bits(4)]
    pub intid: u32,
    #[bits(4)]
    _res_28_31: u16,
    pub aff2: u8,
    pub irm: bool,
    #[bits(3)]
    _res_41_43: u8,
    #[bits(4)]
    pub rs: u8,
    pub aff3: u8,
    _res_56_63: u8,
}
