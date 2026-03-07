// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Header;
use super::Table;
use core::mem::size_of;
use static_assertions::const_assert_eq;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::Unaligned;

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, Default, IntoBytes, Immutable, KnownLayout, Unaligned)]
pub struct Fadt {
    // 36
    pub facs: u32,
    pub dsdt: u32,

    // 44
    pub rsvd: u8,
    pub preferred_pm_profile: u8,
    pub sci_int: u16,

    // 48
    pub smi_cmd: u32,
    pub acpi_enable: u8,
    pub acpi_disable: u8,
    pub s4bios_req: u8,
    pub pstate_cnt: u8,

    // 56
    pub pm1a_evt_blk: u32,
    pub pm1b_evt_blk: u32,
    pub pm1a_cnt_blk: u32,
    pub pm1b_cnt_blk: u32,
    pub pm2_cnt_blk: u32,
    pub pm_tmr_blk: u32,
    pub gpe0_blk: u32,
    pub gpe1_blk: u32,

    // 88
    pub pm1_evt_len: u8,
    pub pm1_cnt_len: u8,
    pub pm2_cnt_len: u8,
    pub pm_tmr_len: u8,
    pub gpe0_blk_len: u8,
    pub gpe1_blk_len_len: u8,
    pub gpe1_base: u8,
    pub cst_cnt: u8,

    // 96
    pub p_lvl2_lat: u16,
    pub p_lvl3_lat: u16,
    pub flush_size: u16,
    pub flush_stride: u16,

    // 104
    pub duty_offset: u8,
    pub duty_width: u8,
    pub day_alrm: u8,
    pub mon_alrm: u8,
    pub century: u8,
    pub iapc_boot_arch: u16,
    pub rsvd2: u8,

    // 112
    pub flags: u32,

    // 116
    pub reset_reg: GenericAddress,
    pub reset_value: u8,
    pub arm_boot_arch: u16,
    pub minor_version: u8,

    // 132
    pub x_firmware_ctrl: u64,
    pub x_dsdt: u64,

    // 148
    pub x_pm1a_evt_blk: GenericAddress,
    pub x_pm1b_evt_blk: GenericAddress,
    pub x_pm1a_cnt_blk: GenericAddress,
    pub x_pm1b_cnt_blk: GenericAddress,
    pub x_pm2_cnt_blk: GenericAddress,
    pub x_pm_tmr_blk: GenericAddress,
    pub x_gpe0_blk: GenericAddress,
    pub x_gpe1_blk: GenericAddress,

    pub sleep_control_reg: GenericAddress,
    pub sleep_status_reg: GenericAddress,
    pub hypervisor_vendor_identity: u64,
}

const_assert_eq!(size_of::<Fadt>(), 276 - size_of::<Header>());

impl Table for Fadt {
    const SIGNATURE: [u8; 4] = *b"FACP";
}

pub const FADT_WBINVD: u32 = 1 << 0;
pub const FADT_WBINVD_FLUSH: u32 = 1 << 1;
pub const FADT_PROC_C1: u32 = 1 << 2;
pub const FADT_LVL2_UP: u32 = 1 << 3;
pub const FADT_PWR_BUTTON: u32 = 1 << 4;
pub const FADT_SLP_BUTTON: u32 = 1 << 5;
pub const FADT_FIX_RTC: u32 = 1 << 6;
pub const FADT_RTC_S4: u32 = 1 << 7;
pub const FADT_TMR_VAL_EXT: u32 = 1 << 8;
pub const FADT_DCK_CAP: u32 = 1 << 9;
pub const FADT_RESET_REG_SUP: u32 = 1 << 10;
pub const FADT_SEALED_CASE: u32 = 1 << 11;
pub const FADT_HEADLESS: u32 = 1 << 12;
pub const FADT_CPU_SW_SLP: u32 = 1 << 13;
pub const FADT_PCI_EXP_WAK: u32 = 1 << 14;
pub const FADT_USE_PLATFORM_CLOCK: u32 = 1 << 15;
pub const FADT_S4_RTC_STS_VALID: u32 = 1 << 16;
pub const FADT_REMOTE_POWER_ON_CAPABLE: u32 = 1 << 17;
pub const FADT_FORCE_APIC_CLUSTER_MODE: u32 = 1 << 18;
pub const FADT_FORCE_APIC_PHYSICAL_DESTINATION_MODE: u32 = 1 << 19;
pub const FADT_HW_REDUCED_ACPI: u32 = 1 << 20;
pub const FADT_LOW_POWER_S0_IDLE_CAPABLE: u32 = 1 << 21;

#[repr(u8)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, Default)]
pub enum AddressSpaceId {
    #[default]
    SystemMemory = 0,
    SystemIo = 1,
    PciConfigurationSpace = 2,
    EmbeddedController = 3,
    Smbus = 4,
    PlatformCommunicationChannel = 0x0A,
    FunctionalFixedHardware = 0x7F,
}

#[repr(u8)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, Default)]
pub enum AddressWidth {
    #[default]
    Undefined = 0,
    Byte = 1,
    Word = 2,
    Dword = 3,
    Qword = 4,
}

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, Default, IntoBytes, Immutable, KnownLayout)]
pub struct GenericAddress {
    pub addr_space_id: AddressSpaceId,
    pub register_bit_width: u8,
    pub register_bit_offset: u8,
    pub access_size: AddressWidth,
    pub address: u64,
}

const_assert_eq!(size_of::<GenericAddress>(), 12);
