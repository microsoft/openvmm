// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! APIC-related definitions.

use bitfield_struct::bitfield;
use open_enum::open_enum;

/// The physical address of the APIC at reset.
pub const APIC_BASE_ADDRESS: u32 = 0xfee00000;
/// The 4KB page number of the physical address of the APIC at reset.
pub const APIC_BASE_PAGE: u32 = APIC_BASE_ADDRESS >> 12;

pub const APIC_LEGACY_ID_COUNT: u32 = 255;

/// The APIC base MSR.
#[bitfield(u64)]
pub struct ApicBase {
    _reserved: u8,
    /// True if this processor is the BSP.
    pub bsp: bool,
    _reserved2: bool,
    pub x2apic: bool,
    pub enable: bool,
    /// The page number of the APIC (usually APIC_BASE_PAGE).
    #[bits(24)]
    pub base_page: u32,
    #[bits(28)]
    _reserved3: u64,
}

/// Spurious vector register.
#[bitfield(u32)]
pub struct Svr {
    pub vector: u8,
    pub enable: bool,
    pub focus_processor_checking: bool,
    #[bits(2)]
    _rsvd: u32,
    pub eoi_broadcast_suppression: bool,
    #[bits(19)]
    _rsvd2: u32,
}

/// Local vector table
#[bitfield(u32)]
pub struct Lvt {
    pub vector: u8,
    #[bits(3)]
    pub delivery_mode: u8,
    _rsvd: bool,
    pub delivery_status: bool,
    pub input_pin_polarity: bool,
    pub remote_irr: bool,
    pub trigger_mode_level: bool,
    pub masked: bool,
    #[bits(2)]
    pub timer_mode: u8,
    #[bits(13)]
    _rsvd2: u32,
}

open_enum! {
    pub enum TimerMode: u8 {
        ONE_SHOT = 0,
        PERIODIC = 1,
        TSC_DEADLINE = 2,
    }
}

#[bitfield(u32)]
pub struct Dcr {
    #[bits(2)]
    pub value_low: u8,
    _rsvd: bool,
    #[bits(1)]
    pub value_high: u8,
    #[bits(28)]
    _rsvd2: u32,
}

open_enum! {
    pub enum Dfr: u32 {
        FLAT_MODE = 0xffff_ffff,
        CLUSTERED_MODE = 0x0fff_ffff,
    }
}

#[bitfield(u64)]
pub struct Icr {
    pub vector: u8,
    #[bits(3)]
    pub delivery_mode: u8,
    pub destination_mode_logical: bool,
    pub delivery_pending: bool,
    pub reserved1: bool,
    pub level_assert: bool,
    pub trigger_mode_level: bool,
    #[bits(2)]
    pub remote_read_status: u8,
    #[bits(2)]
    pub destination_shorthand: u8,
    #[bits(12)]
    pub reserved3: u16,
    pub x2apic_mda: u32,
}

impl Icr {
    pub const fn xapic_mda(&self) -> u8 {
        (self.x2apic_mda() >> 24) as u8
    }

    pub fn set_xapic_mda(&mut self, value: u8) {
        *self = self.with_xapic_mda(value);
    }

    pub const fn with_xapic_mda(self, value: u8) -> Self {
        self.with_x2apic_mda((value as u32) << 24)
    }
}

#[bitfield(u32)]
pub struct X2ApicLogicalId {
    pub logical_id: u16,
    pub cluster_id: u16,
}

#[bitfield(u8)]
pub struct XApicClusterLogicalId {
    #[bits(4)]
    pub logical_id: u8,
    #[bits(4)]
    pub cluster_id: u8,
}

open_enum! {
    pub enum DeliveryMode: u8 {
        FIXED = 0,
        LOWEST_PRIORITY = 1,
        SMI = 2,
        REMOTE_READ = 3,
        NMI = 4,
        INIT = 5,
        SIPI = 6,
        EXTINT = 7,
    }
}

open_enum! {
    pub enum DestinationShorthand: u8 {
        NONE = 0,
        SELF = 1,
        ALL_INCLUDING_SELF = 2,
        ALL_EXCLUDING_SELF = 3,
    }
}

open_enum! {
    pub enum ApicRegister: u8 {
        ID = 0x2,               // RW
        VERSION = 0x3,          // RO
        TPR = 0x8,              // RW
        APR = 0x9,              // RO
        PPR = 0xa,              // RO
        EOI = 0xb,              // WO
        RRD = 0xc,              // RO
        LDR = 0xd,              // RW
        DFR = 0xe,              // RW
        SVR = 0xf,              // RW
        ISR0 = 0x10,            // RO
        ISR1 = 0x11,
        ISR2 = 0x12,
        ISR3 = 0x13,
        ISR4 = 0x14,
        ISR5 = 0x15,
        ISR6 = 0x16,
        ISR7 = 0x17,
        TMR0 = 0x18,            // RO
        TMR1 = 0x19,
        TMR2 = 0x1a,
        TMR3 = 0x1b,
        TMR4 = 0x1c,
        TMR5 = 0x1d,
        TMR6 = 0x1e,
        TMR7 = 0x1f,
        IRR0 = 0x20,            // RO
        IRR1 = 0x21,
        IRR2 = 0x22,
        IRR3 = 0x23,
        IRR4 = 0x24,
        IRR5 = 0x25,
        IRR6 = 0x26,
        IRR7 = 0x27,
        ESR = 0x28,             // RW
        INTEL_LVT_CMCI = 0x2f,  // RW
        ICR0 = 0x30,            // RW
        ICR1 = 0x31,            // RW, XAPIC only
        LVT_TIMER = 0x32,       // RW
        LVT_THERMAL = 0x33,     // RW
        LVT_PMC = 0x34,         // RW
        LVT_LINT0 = 0x35,       // RW
        LVT_LINT1 = 0x36,       // RW
        LVT_ERROR = 0x37,       // RW
        TIMER_ICR = 0x38,       // RW
        TIMER_CCR = 0x39,       // RO
        TIMER_DCR = 0x3e,       // RW
        SELF_IPI = 0x3f,        // WO, X2APIC only
    }
}

pub const X2APIC_MSR_BASE: u32 = 0x800;
pub const X2APIC_MSR_END: u32 = 0x83f;

impl ApicRegister {
    pub fn x2apic_msr(&self) -> u32 {
        X2APIC_MSR_BASE + self.0 as u32
    }
}

#[bitfield(u32)]
pub struct ApicVersion {
    pub version: u8,
    _rsvd: u8,
    pub max_lvt_entry: u8,
    pub eoi_broadcast_suppression: bool,
    #[bits(7)]
    _rsvd: u8,
}
