// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Definitions for the Generic Interrupt Controller (GIC) registers.
//!
//! GICv3 has two main components:
//! 1. GICD (distributor) - the central hub for all interrupts
//! 2. GICR (re-distributors) - per-CPU interrupt management
//!
//! GICD functions, most notably:
//! - routes interrupts to correct CPU,
//! - stores global interrupt state (enabled/pending),
//! - handles interrupt prioritization,
//! - broadcasts SGIs (Software Generated Interrupts)
//!
//! The distributor has the size of 64KiB.
//!
//! Crucial GICR functions (per CPU):
//! - manages CPU-private interrupts (SGIs 0-15, PPIs 16-31),
//! - handles interrupt signaling to individual cores,
//! - provides wakeup control for power management
//!
//! Each redistributor defines two 64KiB frames in the physical address map:
//! - RD_base for controlling the overall behavior of the Redistributor, for
//!   controlling LPIs, and for generating LPIs in a system that does not
//!   include at least one ITS,
//! - SGI_base for controlling and generating PPIs and SGIs.
//!
//! For overview, refer to [GICv3 and GICv4 Software Overview](https://developer.arm.com/documentation/dai0492/b/)
//! See [GIC architecture version 3 and version 4](https://developer.arm.com/documentation/ihi0069/latest/)
//! for more details. That document is refernced below unless stated otherwise.

use bitfield_struct::bitfield;
use core::ops::Range;
use open_enum::open_enum;

// GIC registers, "12.9 The GIC Distributor register descriptions"

pub const GICD_SIZE: usize = 0x10000;

open_enum! {
    /// GIC Distributor register map
    ///
    /// See "12.8 The GIC Distributor register map".
    pub enum GicdRegister: u16 {
        /// 0x0000 - Distributor Control Register (GICD_CTLR)
        ///
        /// Controls overall operation of the Distributor.
        /// The reset value is implementation defined.
        CTLR = 0x0000, // u32

        /// 0x0004 - Interrupt Controller Type Register (GICD_TYPER)
        ///
        /// Provides information about the configuration of the GIC.
        /// Read-only, implementation defined.
        TYPER = 0x0004, // u32

        /// 0x0008 - Distributor Implementer Identification Register (GICD_IIDR)
        ///
        /// Identifies the implementer of the GIC.
        /// Read-only, implementation defined.
        IIDR = 0x0008, // u32

        /// 0x000C - Interrupt Controller Type Register 2 (GICD_TYPER2)
        ///
        /// Additional type information about the GIC.
        /// Read-only, implementation defined.
        TYPER2 = 0x000C, // u32

        /// 0x0010 - Error Reporting Status Register (optional) (GICD_STATUSR)
        ///
        /// Reports error conditions in the Distributor.
        /// Reset value: 0x00000000
        STATUSR = 0x0010, // u32

        /// 0x0040 - Set SPI Register (Non-secure) (GICD_SETSPI_NSR)
        ///
        /// Writing to this register sets the corresponding SPI interrupt
        /// pending state in the non-secure state.
        SETSPI_NSR = 0x0040, // u32

        /// 0x0048 - Clear SPI Register (Non-secure) (GICD_CLRSPI_NSR)
        ///
        /// Writing to this register clears the corresponding SPI interrupt
        /// pending state in the non-secure state.
        CLRSPI_NSR = 0x0048, // u32

        /// 0x0050 - Set SPI Register (Secure) (GICD_SETSPI_SR)
        ///
        /// Writing to this register sets the corresponding SPI interrupt
        /// pending state in the secure state.
        SETSPI_SR = 0x0050, // u32

        /// 0x0058 - Clear SPI Register (Secure) (GICD_CLRSPI_SR)
        ///
        /// Writing to this register clears the corresponding SPI interrupt
        /// pending state in the secure state.
        CLRSPI_SR = 0x0058, // u32

        /// 0x0080-0x00FC - Interrupt Group Registers (GICD_IGROUPR<n>)
        ///
        /// Configures interrupts as Group 0 or Group 1.
        /// Reset value: implementation defined.
        IGROUPR0 = 0x0080, // [u32; 32]

        /// 0x0100-0x017C - Interrupt Set-Enable Registers (GICD_ISENABLER<n>)
        ///
        /// Enables forwarding of interrupts to CPU interfaces.
        /// Reset value: implementation defined.
        ISENABLER0 = 0x0100, // [u32; 32]

        /// 0x0180-0x01FC - Interrupt Clear-Enable Registers (GICD_ICENABLER<n>)
        ///
        /// Disables forwarding of interrupts to CPU interfaces.
        /// Reset value: implementation defined.
        ICENABLER0 = 0x0180, // [u32; 32]

        /// 0x0200-0x027C - Interrupt Set-Pending Registers (GICD_ISPENDR<n>)
        ///
        /// Sets the pending state of interrupts.
        /// Reset value: 0x00000000
        ISPENDR0 = 0x0200, // [u32; 32]

        /// 0x0280-0x02FC - Interrupt Clear-Pending Registers (GICD_ICPENDR<n>)
        ///
        /// Clears the pending state of interrupts.
        /// Reset value: 0x00000000
        ICPENDR0 = 0x0280, // [u32; 32]

        /// 0x0300-0x037C - Interrupt Set-Active Registers (GICD_ISACTIVER<n>)
        ///
        /// Sets the active state of interrupts.
        /// Reset value: 0x00000000
        ISACTIVER0 = 0x0300, // [u32; 32]

        /// 0x0380-0x03FC - Interrupt Clear-Active Registers (GICD_ICACTIVER<n>)
        ///
        /// Clears the active state of interrupts.
        /// Reset value: 0x00000000
        ICACTIVER0 = 0x0380, // [u32; 32]

        /// 0x0400-0x07F8 - Interrupt Priority Registers (GICD_IPRIORITYR<n>)
        ///
        /// Configures the priority of each interrupt.
        /// Reset value: 0x00000000
        IPRIORITYR0 = 0x0400, // [u32; 256]

        /// 0x0800-0x081C - Interrupt Processor Targets Registers (GICD_ITARGETSR<n>)
        ///
        /// Configures which CPUs receive each interrupt. Read-only, implementation defined.
        /// RES0 when affinity routing is enabled.
        ITARGETSR = 0x0800, // [u32; 8]

        /// 0x0C00-0x0CFC - Interrupt Configuration Registers (GICD_ICFGR<n>)
        ///
        /// Configures interrupts as level-sensitive or edge-triggered.
        /// Reset value: implementation defined.
        ICFGR0 = 0x0C00, // [u32; 64]

        /// 0x0D00-0x0D7C - Interrupt Group Modifier Registers (GICD_IGRPMODR<n>)
        ///
        /// Modifies interrupt group behavior.
        /// Reset value: 0x00000000
        IGRPMODR = 0x0D00, // [u32; 32]

        /// 0x0E00-0x0EFC - Non-secure Access Control Registers (GICD_NSACR<n>)
        ///
        /// Controls non-secure access to secure interrupts.
        /// Reset value: 0x00000000
        NSACR = 0x0E00, // [u32; 64]

        /// 0x0F00 - Software Generated Interrupt Register (GICD_SGIR)
        ///
        /// Generates software interrupts.
        /// RES0 when affinity routing is enabled.
        SGIR = 0x0F00, // u32

        /// 0x0F10-0x0F1C - SGI Clear-Pending Registers (GICD_CPENDSGIR<n>)
        ///
        /// Clears pending state of SGIs. Reset value: 0x00000000
        /// RES0 when affinity routing is enabled.
        CPENDSGIR = 0x0F10, // [u32; 4]

        /// 0x0F20-0x0F2C - SGI Set-Pending Registers (GICD_SPENDSGIR<n>)
        ///
        /// Sets pending state of SGIs. Reset value: 0x00000000
        /// RES0 when affinity routing is enabled.
        SPENDSGIR = 0x0F20, // [u32; 4]

        /// 0x0F80-0x0FFC - Non-maskable Interrupt Registers (GICD_INMIR<n>)
        ///
        /// Controls non-maskable interrupts.
        /// Reset value: 0x00000000
        INMIR = 0x0F80, // [u32; 32]

        // Extended SPI registers (0x1000 onwards)

        /// 0x1000-0x107C - Interrupt Group Registers for extended SPI range (GICD_IGROUPR<n>E)
        ///
        /// Configures extended SPIs as Group 0 or Group 1.
        /// Reset value: 0x00000000
        IGROUPR_E = 0x1000, // [u32; 32]

        /// 0x1200-0x127C - Interrupt Set-Enable for extended SPI range (GICD_ISENABLER<n>E)
        ///
        /// Enables forwarding of extended SPI interrupts.
        /// Reset value: implementation defined.
        ISENABLER_E = 0x1200, // [u32; 32]

        /// 0x1400-0x147C - Interrupt Clear-Enable for extended SPI range (GICD_ICENABLER<n>E)
        ///
        /// Disables forwarding of extended SPI interrupts.
        /// Reset value: implementation defined.
        ICENABLER_E = 0x1400, // [u32; 32]

        /// 0x1600-0x167C - Interrupt Set-Pend for extended SPI range (GICD_ISPENDR<n>E)
        ///
        /// Sets the pending state of extended SPI interrupts.
        /// Reset value: 0x00000000
        ISPENDR_E = 0x1600, // [u32; 32]

        /// 0x1800-0x187C - Interrupt Clear-Pend for extended SPI range (GICD_ICPENDR<n>E)
        ///
        /// Clears the pending state of extended SPI interrupts.
        /// Reset value: 0x00000000
        ICPENDR_E = 0x1800, // [u32; 32]

        /// 0x1A00-0x1A7C - Interrupt Set-Active for extended SPI range (GICD_ISACTIVER<n>E)
        ///
        /// Sets the active state of extended SPI interrupts.
        /// Reset value: 0x00000000
        ISACTIVER_E = 0x1A00, // [u32; 32]

        /// 0x1C00-0x1C7C - Interrupt Clear-Active for extended SPI range (GICD_ICACTIVER<n>E)
        ///
        /// Clears the active state of extended SPI interrupts.
        /// Reset value: 0x00000000
        ICACTIVER_E = 0x1C00, // [u32; 32]

        /// 0x2000-0x23FC - Interrupt Priority for extended SPI range (GICD_IPRIORITYR<n>E)
        ///
        /// Configures the priority of extended SPI interrupts.
        /// Reset value: 0x00000000
        IPRIORITYR_E = 0x2000, // [u32; 256]

        /// 0x3000-0x30FC - Extended SPI Configuration Register (GICD_ICFGR<n>E)
        ///
        /// Configures extended SPI interrupts as level-sensitive or edge-triggered.
        /// Reset value: implementation defined.
        ICFGR_E = 0x3000, // [u32; 64]

        /// 0x3400-0x347C - Interrupt Group Modifier for extended SPI range (GICD_IGRPMODR<n>E)
        ///
        /// Modifies extended SPI interrupt group behavior.
        /// Reset value: 0x00000000
        IGRPMODR_E = 0x3400, // [u32; 32]

        /// 0x3600-0x36FC - Non-secure Access Control Registers for extended SPI range (GICD_NSACR<n>E)
        ///
        /// Controls non-secure access to secure extended SPI interrupts.
        /// Reset value: 0x00000000
        NSACR_E = 0x3600, // [u32; 64]

        /// 0x3B00-0x3B7C - Non-maskable Interrupt Registers for Extended SPIs (GICD_INMIR<n>Eg)
        ///
        /// Controls non-maskable extended SPI interrupts.
        /// Reset value: 0x00000000
        INMIR_E = 0x3B00, // [u32; 32]

        /// 0x6100-0x7FD8 - Interrupt Routing Registers (GICD_IROUTER<n>)
        ///
        /// Configures interrupt routing for affinity-based systems.
        /// Reset value: 0x00000000
        IROUTER0 = 0x6100, // [u32; 1984]

        /// 0x8000-0x9FFC - Interrupt Routing Registers for extended SPI range (GICD_IROUTER<n>E)
        ///
        /// Configures interrupt routing for extended SPI interrupts in affinity-based systems.
        /// Reset value: 0x00000000
        IROUTER_E = 0x8000, // [u32; 2048]

        /// 0xFFE8 - Distributor Peripheral ID2 Register (GICD_PIDR2)
        ///
        /// Provides version data about the distributor.
        ///
        PIDR2 = 0xFFE8, // u32
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

// GICR registers, "12.11 The GIC Redistributor register descriptions"

pub const GICR_SIZE: usize = 0x20000;
pub const GICR_FRAME_SIZE: usize = 0x10000;

open_enum! {
    /// GIC physical LPI Redistributor register map
    pub enum GicrRdRegister: u16 {
        /// 0x0000 - Redistributor Control Register (GICR_CTLR)
        CTLR = 0x0000,

        /// 0x0004 - Implementer Identification Register (GICR_IIDR)
        IIDR = 0x0004,

        /// 0x0008 - Redistributor Type Register (GICR_TYPER)
        TYPER = 0x0008,

        /// 0x0010 - Error Reporting Status Register (optional) (GICR_STATUSR)
        STATUSR = 0x0010,

        /// 0x0014 - Redistributor Wake Register (GICR_WAKER)
        WAKER = 0x0014,

        /// 0x0018 - Report maximum PARTID and PMG Register (GICR_MPAMIDR)
        MPAMIDR = 0x0018,

        /// 0x001C - Set PARTID and PMG Register (GICR_PARTIDR)
        PARTIDR = 0x001C,

        /// 0x0040 - Set LPI Pending Register (GICR_SETLPIR)
        SETLPIR = 0x0040,

        /// 0x0048 - Clear LPI Pending Register (GICR_CLRLPIR)
        CLRLPIR = 0x0048,

        /// 0x0070 - Redistributor Properties Base Address Register (GICR_PROPBASER)
        PROPBASER = 0x0070,

        /// 0x0078 - Redistributor LPI Pending Table Base Address Register (GICR_PENDBASER)
        PENDBASER = 0x0078,

        /// 0x00A0 - Redistributor Invalidate LPI Register (GICR_INVLPIR)
        INVLPIR = 0x00A0,

        /// 0x00B0 - Redistributor Invalidate All Register (GICR_INVALLR)
        INVALLR = 0x00B0,

        /// 0x00C0 - Redistributor Synchronize Register (GICR_SYNCR)
        SYNCR = 0x00C0,

        /// Distributor Peripheral ID2 (GICR_PIDR2)
        PIDR2 = 0xFFE8,
    }
}

open_enum! {
    /// GIC SGI and PPI Redistributor register map
    /// See "12.10 The GIC Redistributor register map"
    pub enum GicrSgiRegister: u16 {
        /// 0x0080 - Interrupt Group Register 0 (GICR_IGROUPR0)
        ///
        /// `1` means Group0, `0` means Secure if `GICD_CTRL.DS` == `1`.
        IGROUPR0 = 0x0080, // u32

        /// 0x0084-0x0088 - Interrupt Group Registers for extended PPI range (GICR_IGROUPR<n>E)
        IGROUPR_E = 0x0084, // [u32; 2]

        /// 0x0100 - Interrupt Set-Enable Register 0 (GICR_ISENABLER0)
        ISENABLER0 = 0x0100, // u32

        /// 0x0104-0x0108 - Interrupt Set-Enable for extended PPI range (GICR_ISENABLER<n>E)
        ISENABLER_E = 0x0104, // [u32; 2]

        /// 0x0180 - Interrupt Clear-Enable Register 0 (GICR_ICENABLER0)
        ICENABLER0 = 0x0180, // u32

        /// 0x0184-0x0188 - Interrupt Clear-Enable for extended PPI range (GICR_ICENABLER<n>E)
        ICENABLER_E = 0x0184, // [u32; 2]

        /// 0x0200 - Interrupt Set-Pend Register 0 (GICR_ISPENDR0)
        ISPENDR0 = 0x0200, // u32

        /// 0x0204-0x0208 - Interrupt Set-Pend for extended PPI range (GICR_ISPENDR<n>E)
        ISPENDR0_E = 0x0204, // [u32; 2]

        /// 0x0280 - Interrupt Clear-Pend Register 0 (GICR_ICPENDR0)
        ICPENDR0 = 0x0280, // u32

        /// 0x0284-0x0288 - Interrupt Clear-Pend for extended PPI range (GICR_ICPENDR<n>E)
        ICPENDR_E = 0x0284, // [u32; 2]

        /// 0x0300 - Interrupt Set-Active Register 0 (GICR_ISACTIVER0)
        ISACTIVER0 = 0x0300, // u32

        /// 0x0304-0x0308 - Interrupt Set-Active for extended PPI range (GICR_ISACTIVER<n>E)
        ISACTIVER0_E = 0x0304, // [u32; 2]

        /// 0x0380 - Interrupt Clear-Active Register 0 (GICR_ICACTIVER0)
        ICACTIVER0 = 0x0380, // u32

        /// 0x0384-0x0388 - Interrupt Clear-Active for extended PPI range (GICR_ICACTIVER<n>E)
        ICACTIVER0_E = 0x0384, // [u32; 2]

        /// 0x0400-0x041C - Interrupt Priority Registers (GICR_IPRIORITYR<n>)
        ///
        /// - GICR_IPRIORITYR0-GICR_IPRIORITYR3 store the priority of SGIs.
        /// - GICR_IPRIORITYR4-GICR_IPRIORITYR7 store the priority of PPIs.
        ///
        /// Interrupt priority value from an IMPLEMENTATION DEFINED range,
        /// takes 8 bits. Lower priority values correspond to greater priority
        /// of the interrupt. For an INTID configured as non-maskable, this field is RES0.
        IPRIORITYR0 = 0x0400, // [u32; 8]

        /// 0x0420-0x045C - Interrupt Priority for extended PPI range (GICR_IPRIORITYR<n>E)
        IPRIORITYR_E = 0x0420, // [u32; 16]

        /// 0x0C00 - SGI Configuration Register (GICR_ICFGR0)
        ICFGR0 = 0x0C00, // u32

        /// 0x0C04 - PPI Configuration Register (GICR_ICFGR1)
        ICFGR1 = 0x0C04, // u32

        /// 0x0C08-0x0C14 - Extended PPI Configuration Register (GICR_ICFGR<n>E)
        ICFGR_E = 0x0C08, // [u32; 4]

        /// 0x0D00 - Interrupt Group Modifier Register 0 (GICR_IGRPMODR0)
        IGRPMODR0 = 0x0D00, // u32

        /// 0x0D04-0x0D08 - Interrupt Group Modifier for extended PPI range (GICR_IGRPMODR<n>E)
        IGRPMODR_E = 0x0D04, // [u32; 2]

        /// 0x0E00 - Non-Secure Access Control Register (GICR_NSACR)
        NSACR = 0x0E00, // u32

        /// 0x0F80 - Non-maskable Interrupt Register for PPIs and SGIs (GICR_INMIR0)
        INMIR0 = 0x0F80, // u32

        /// 0x0F84-0x0FFC - Non-maskable Interrupt Registers for Extended PPIs (GICR_INMIR<n>E)
        INMIR_E = 0x0F84, // [u32; 31]
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
