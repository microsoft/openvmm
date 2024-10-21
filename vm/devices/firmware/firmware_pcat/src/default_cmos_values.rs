// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to generate initial RTC CMOS ram state (as expected by the PCAT BIOS).
//!
//! NOTE: Technically speaking, this module isn't actually used by the
//! `firmware_pcat` helper device itself. Rather - upper-level init code in the
//! VMM stack must invoke [`default_cmos_values`] when initializing the CMOS RTC
//! device.
//!
//! So, why not stuff this code into its own crate then?
//!
//! Well... we could... but spinning off a whole crate for this seems a bit
//! overkill, given the fact that any VMM trying to use the PCAT helper device
//! will also require initializing the RTC with this data.
//!
//! As such, it seems reasonable to couple two bits of functionality in one
//! crate.

use vm_topology::memory::MemoryLayout;

const Q_RTC_SECONDS: u16 = 0;
const Q_RTC_SECONDS_ALARM: u16 = 1;
const Q_DATE: u16 = 2;
const Q_TIME: u16 = 3;
const Q_RTC_MINUTES: u16 = 4;
const Q_RTC_MINUTES_ALARM: u16 = 5;
const Q_RTC_HOURS: u16 = 6;
const Q_RTC_HOURS_ALARM: u16 = 7;
const Q_RTC_DAY_OF_WEEK: u16 = 8;
const Q_RTC_DAY: u16 = 9;
const Q_RTC_MONTH: u16 = 10;
const Q_RTC_YEAR: u16 = 11;
const Q_RTC_STATUS_A: u16 = 12;
const Q_RTC_STATUS_B: u16 = 13;
const Q_RTC_STATUS_C: u16 = 14;
const Q_RTC_STATUS_D: u16 = 15;
const Q_RTC_DIAG_STATUS: u16 = 16;
const Q_RTC_SHUTDOWN_STATUS: u16 = 17;
const Q_FLOPPY_B: u16 = 18;
const Q_FLOPPY_A: u16 = 19;
const Q_BOOT_FIRST_DEVICE: u16 = 20;
const Q_PM_E_TYPE: u16 = 21;
const Q_PS_E_TYPE: u16 = 22;
const Q_PS_TYPE: u16 = 23;
const Q_PM_TYPE: u16 = 24;
const Q_SM_E_TYPE: u16 = 25;
const Q_SS_E_TYPE: u16 = 26;
const Q_FLOPPY_SEEK: u16 = 27;
const Q_QUICK_BOOT: u16 = 28;
const Q_ADDON_ROM_DISPLAY: u16 = 29;
const Q_NUM_LOCK: u16 = 30;
const Q_FLOPPY_PRESENT: u16 = 31;
const Q_MATHCO_PRESENT: u16 = 32;
const Q_KEYBOARD_PRESENT: u16 = 33;
const Q_MOUSE_SUPPORT: u16 = 34;
const Q_TYPEMATIC_RATE: u16 = 35;
const Q_PARITY_ERROR: u16 = 36;
const Q_FLOPPY_COUNT: u16 = 37;
const Q_BASE_MEMORY_LSB: u16 = 38;
const Q_BASE_MEMORY_MSB: u16 = 39;
const Q_EXT_MEMORY_LSB: u16 = 40;
const Q_EXT_MEMORY_MSB: u16 = 41;
const Q_PM_EXT_TYPE: u16 = 42;
const Q_PS_EXT_TYPE: u16 = 43;
const Q_PM_CYL: u16 = 44;
const Q_PM_HD: u16 = 45;
const Q_PM_WPCOM: u16 = 46;
const Q_PM_SPT: u16 = 47;
const Q_PS_CYL: u16 = 48;
const Q_PS_HD: u16 = 49;
const Q_PS_WPCOM: u16 = 50;
const Q_PS_SPT: u16 = 51;
const Q_SM_TYPE: u16 = 52;
const Q_SS_TYPE: u16 = 53;
const Q_OS2_COMP_MODE: u16 = 54;
const Q_WAIT_FOR_F1: u16 = 55;
const Q_DISPLAY_HIT_DEL: u16 = 56;
const Q_PCI_IDE_CONTROLLER: u16 = 57;
const Q_PM_SMART: u16 = 58;
const Q_PM_LBA_MODE: u16 = 59;
const Q_PM_BLOCK_MODE: u16 = 60;
const Q_PM_32BIT_XFER: u16 = 61;
const Q_PM_PIO_MODE: u16 = 62;
const Q_PM_EMULATION_TYPE: u16 = 63;
const Q_PS_LBA_MODE: u16 = 64;
const Q_PS_BLOCK_MODE: u16 = 65;
const Q_PS_32BIT_XFER: u16 = 66;
const Q_PS_PIO_MODE: u16 = 67;
const Q_PS_SMART: u16 = 68;
const Q_SM_LBA_MODE: u16 = 69;
const Q_SM_BLOCK_MODE: u16 = 70;
const Q_SM_32BIT_XFER: u16 = 71;
const Q_SM_PIO_MODE: u16 = 72;
const Q_PS_EMULATION_TYPE: u16 = 73;
const Q_SS_LBA_MODE: u16 = 74;
const Q_SS_BLOCK_MODE: u16 = 75;
const Q_SS_32BIT_XFER: u16 = 76;
const Q_SS_PIO_MODE: u16 = 77;
const Q_SM_SMART: u16 = 78;
const Q_CHECKSUM_MSB: u16 = 79;
const Q_CHECKSUM_LSB: u16 = 80;
const Q_POST_EXT_MEMORY_LSB: u16 = 81;
const Q_POST_EXT_MEMORY_MSB: u16 = 82;
const Q_CENTURY: u16 = 83;
const Q_SCRATCH_33: u16 = 84;
const Q_SCRATCH_34: u16 = 85;
const Q_SCRATCH_35: u16 = 86;
const Q_SCRATCH_36: u16 = 87;
const Q_COLOR: u16 = 88;
const Q_PM_DMA_MODE: u16 = 89;
const Q_PS_DMA_MODE: u16 = 90;
const Q_SM_DMA_MODE: u16 = 91;
const Q_SM_EMULATION_TYPE: u16 = 92;
const Q_SS_DMA_MODE: u16 = 93;
const Q_SS_SMART: u16 = 94;
const Q_SS_EMULATION_TYPE: u16 = 95;
const Q_HDD_ACCESS_CONTROL: u16 = 96;
const Q_ATA_DETECT_TIME_OUT: u16 = 97;
const Q_ATA_80PIN_CABLE_DETECT: u16 = 98;
const Q_PNP_AWARE_OS: u16 = 99;
const Q_RESET_CONFIG_DATA: u16 = 100;
const Q_PCI_LATENCY_TIMER: u16 = 101;
const Q_PCI_VGA_IRQ: u16 = 102;
const Q_PCI_PALETTE_SNOOP: u16 = 103;
const Q_PCI_IDE_BUS_MASTER: u16 = 104;
const Q_PCI_OFFBOARD_IDE: u16 = 105;
const Q_PCI_OFFBOARD_IDE_PRI_IRQ: u16 = 106;
const Q_IRQ3_USED_BY_ISA: u16 = 107;
const Q_IRQ4_USED_BY_ISA: u16 = 108;
const Q_PCI_OFFBOARD_IDE_SEC_IRQ: u16 = 109;
const Q_IRQ5_USED_BY_ISA: u16 = 110;
const Q_IRQ7_USED_BY_ISA: u16 = 111;
const Q_IRQ9_USED_BY_ISA: u16 = 112;
const Q_IRQ10_USED_BY_ISA: u16 = 113;
const Q_IRQ11_USED_BY_ISA: u16 = 114;
const Q_EXT_CMOS_CHECKSUM_MSB: u16 = 115;
const Q_EXT_CMOS_CHECKSUM_LSB: u16 = 116;
const Q_SM_CYL: u16 = 117;
const Q_SM_HD: u16 = 118;
const Q_SM_WPCOM: u16 = 119;
const Q_SM_SPT: u16 = 120;
const Q_SS_CYL: u16 = 121;
const Q_SS_HD: u16 = 122;
const Q_SS_WPCOM: u16 = 123;
const Q_SS_SPT: u16 = 124;
const Q_IRQ14_USED_BY_ISA: u16 = 125;
const Q_IRQ15_USED_BY_ISA: u16 = 126;
const Q_DMA0_USED_BY_ISA: u16 = 127;
const Q_DMA1_USED_BY_ISA: u16 = 128;
const Q_DMA3_USED_BY_ISA: u16 = 129;
const Q_DMA5_USED_BY_ISA: u16 = 130;
const Q_DMA6_USED_BY_ISA: u16 = 131;
const Q_DMA7_USED_BY_ISA: u16 = 132;
const Q_MEM_SIZE_USED_BY_ISA: u16 = 133;
const Q_MEM_BASE_USED_BY_ISA: u16 = 134;
const Q_FLOPPY_CONTROLLER: u16 = 135;
const Q_COMB_IR_PIN_SELECT: u16 = 136;
const Q_COMA_PORT: u16 = 137;
const Q_COMB_PORT: u16 = 138;
const Q_COMB_MODE: u16 = 139;
const Q_COMB_DUPLEX_MODE: u16 = 140;
const Q_IR_PORT: u16 = 141;
const Q_IR_PORT_IRQ: u16 = 142;
const Q_LPT_MODE: u16 = 143;
const Q_LPT_PORT: u16 = 144;
const Q_LPT_EPPVER: u16 = 145;
const Q_LPT_ECP_DMA: u16 = 146;
const Q_LPT_IRQ: u16 = 147;
const Q_CPU_SERIAL: u16 = 148;
const Q_BU_UPDATE: u16 = 149;
const Q_CACHE_BUS_ECC: u16 = 150;
const Q_INTERNAL_CACHE: u16 = 151;
const Q_C0000_SHADOW: u16 = 152;
const Q_C4000_SHADOW: u16 = 153;
const Q_CPU_FREQ_STRAP: u16 = 154;
const Q_C8000_SHADOW: u16 = 155;
const Q_NB_SERR: u16 = 156;
const Q_CC000_SHADOW: u16 = 157;
const Q_D0000_SHADOW: u16 = 158;
const Q_D4000_SHADOW: u16 = 159;
const Q_D8000_SHADOW: u16 = 160;
const Q_DC000_SHADOW: u16 = 161;
const Q_NB_PERR: u16 = 162;
const Q_NB_USWC_WRITE_POST: u16 = 163;
const Q_NB_MLT: u16 = 164;
const Q_NB_PCI1_TO_PCI0_ACCESS: u16 = 165;
const Q_NB_MTT: u16 = 166;
const Q_NB_APERTURE_ACCESS_GLOBAL: u16 = 167;
const Q_NB_PCI0_TO_APERTURE_ACCESS: u16 = 168;
const Q_NB_DATA_INTEGRITY_MODE: u16 = 169;
const Q_NB_EDO_RASX_WAIT_STATE: u16 = 170;
const Q_NB_DRAM_REFRESH_RATE: u16 = 171;
const Q_NB_EDO_CASX_WAIT_STATE: u16 = 172;
const Q_NB_SUSPEND_REFRESH: u16 = 173;
const Q_NB_SDRAM_RAS_TO_CAS_DELAY: u16 = 174;
const Q_NB_POWER_DOWN_SDRAM: u16 = 175;
const Q_NB_SDRAM_RAS_PRECHARGE: u16 = 176;
const Q_NB_SDRAM_PRECHARGE_CONTROL: u16 = 177;
const Q_NB_ACPI_CONTROL: u16 = 178;
const Q_NB_GATED_CLOCK: u16 = 179;
const Q_NB_SEARCH_MDA: u16 = 180;
const Q_NB_AGP_SYNC: u16 = 181;
const Q_NB_APERTURE_SIZE: u16 = 182;
const Q_NB_AGP_SNOOP: u16 = 183;
const Q_NB_AMTT: u16 = 184;
const Q_AGP_SERR: u16 = 185;
const Q_NB_LPTT: u16 = 186;
const Q_AGP_PARITY_ERROR_RESPONSE: u16 = 187;
const Q_EXTERNAL_CACHE: u16 = 188;
const Q_SBH_SERR: u16 = 189;
const Q_8BIT_IO_RECOVERY: u16 = 190;
const Q_16BIT_IO_RECOVERY: u16 = 191;
const Q_SBH_USB_PASSIVE_RELEASE: u16 = 192;
const Q_SBH_PASSIVE_RELEASE: u16 = 193;
const Q_SBH_DELAYED_TRANSACTION: u16 = 194;
const Q_SBH_ROUTING1_DMA_TYPEF: u16 = 195;
const Q_SBH_ROUTING2_DMA_TYPEF: u16 = 196;
const Q_SBH_DMA0_TYPE: u16 = 197;
const Q_SBH_DMA1_TYPE: u16 = 198;
const Q_SBH_DMA2_TYPE: u16 = 199;
const Q_SBH_DMA3_TYPE: u16 = 200;
const Q_SBH_DMA5_TYPE: u16 = 201;
const Q_SBH_DMA6_TYPE: u16 = 202;
const Q_SBH_DMA7_TYPE: u16 = 203;
const Q_ACPI_OS: u16 = 204;
const Q_ACPI_2: u16 = 205;
const Q_ACPI_APIC_TBL: u16 = 206;
const Q_ACPI_EXCH_BUF: u16 = 207;
const Q_ACPI_HEADLESS_MODE: u16 = 208;
const Q_LANGUAGE: u16 = 209;
const Q_FLOPPY_SWAP: u16 = 210;
const Q_I19_TRAP_ALLOW: u16 = 211;
const Q_PCI_SLOT1_IRQ: u16 = 212;
const Q_PCI_SLOT2_IRQ: u16 = 213;
const Q_PCI_SLOT3_IRQ: u16 = 214;
const Q_PCI_SLOT4_IRQ: u16 = 215;
const Q_FIRST_BOOT_DRIVE: u16 = 216;
const Q_SECOND_BOOT_DRIVE: u16 = 217;
const Q_RD_FIRST_BOOT_DRIVE: u16 = 218;
const Q_THIRD_BOOT_DRIVE: u16 = 219;
const Q_FOURTH_BOOT_DRIVE: u16 = 220;
const Q_RD_SECOND_BOOT_DRIVE: u16 = 221;
const Q_FIFTH_BOOT_DRIVE: u16 = 222;
const Q_SIXTH_BOOT_DRIVE: u16 = 223;
const Q_RD_THIRD_BOOT_DRIVE: u16 = 224;
const Q_SEVENTH_BOOT_DRIVE: u16 = 225;
const Q_EIGHTH_BOOT_DRIVE: u16 = 226;
const Q_RD_FOURTH_BOOT_DRIVE: u16 = 227;
const Q_HD_FIRST_BOOT_DRIVE: u16 = 228;
const Q_HD_SECOND_BOOT_DRIVE: u16 = 229;
const Q_HD_THIRD_BOOT_DRIVE: u16 = 230;
const Q_HD_FOURTH_BOOT_DRIVE: u16 = 231;
const Q_HD_FIFTH_BOOT_DRIVE: u16 = 232;
const Q_HD_SIXTH_BOOT_DRIVE: u16 = 233;
const Q_HD_SEVENTH_BOOT_DRIVE: u16 = 234;
const Q_HD_EIGHTH_BOOT_DRIVE: u16 = 235;
const Q_HD_NINTH_BOOT_DRIVE: u16 = 236;
const Q_HD_TENTH_BOOT_DRIVE: u16 = 237;
const Q_HD_ELEVENTH_BOOT_DRIVE: u16 = 238;
const Q_HD_TWELVETH_BOOT_DRIVE: u16 = 239;
const Q_CD_FIRST_BOOT_DRIVE: u16 = 240;
const Q_CD_SECOND_BOOT_DRIVE: u16 = 241;
const Q_CD_THIRD_BOOT_DRIVE: u16 = 242;
const Q_CD_FOURTH_BOOT_DRIVE: u16 = 243;
const Q_SUPERVISOR_PASSWORD: u16 = 244;
const Q_DENY_SETUP: u16 = 245;
const Q_PASSWORD_CHECK: u16 = 246;
const Q_VIRUS_PROTECTION: u16 = 247;
const Q_USER_PASSWORD: u16 = 248;
const VMBIOS_ENDTOKEN: u16 = 249;

#[derive(Debug, Default, Copy, Clone)]
struct VmBiosTokenEntry {
    pub token: u16,
    pub bit_offset: u16,
    pub bit_size: u16,
    pub default_value: u16,
}

impl VmBiosTokenEntry {
    const fn new(
        token: u16,
        bit_offset: u16,
        bit_size: u16,
        default_value: u16,
    ) -> VmBiosTokenEntry {
        VmBiosTokenEntry {
            token,
            bit_offset,
            bit_size,
            default_value,
        }
    }
}

const S_VM_BIOS_TOKEN_TABLE: &[VmBiosTokenEntry] = &[
    VmBiosTokenEntry::new(Q_RTC_SECONDS, 0x0000, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_RTC_SECONDS_ALARM, 0x0008, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_TIME, 0x0008, 0x00, 0x0000),
    VmBiosTokenEntry::new(Q_DATE, 0x0008, 0x00, 0x0000),
    VmBiosTokenEntry::new(Q_RTC_MINUTES, 0x0010, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_RTC_MINUTES_ALARM, 0x0018, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_RTC_HOURS, 0x0020, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_RTC_HOURS_ALARM, 0x0028, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_RTC_DAY_OF_WEEK, 0x0030, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_RTC_DAY, 0x0038, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_RTC_MONTH, 0x0040, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_RTC_YEAR, 0x0048, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_RTC_STATUS_A, 0x0050, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_RTC_STATUS_B, 0x0058, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_RTC_STATUS_C, 0x0060, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_RTC_STATUS_D, 0x0068, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_RTC_DIAG_STATUS, 0x0070, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_RTC_SHUTDOWN_STATUS, 0x0078, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_FLOPPY_B, 0x0080, 0x04, 0x0000),
    VmBiosTokenEntry::new(Q_FLOPPY_A, 0x0084, 0x04, 0x0004),
    VmBiosTokenEntry::new(Q_BOOT_FIRST_DEVICE, 0x0088, 0x04, 0x0000),
    VmBiosTokenEntry::new(Q_PM_E_TYPE, 0x008c, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_PS_E_TYPE, 0x008e, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_PS_TYPE, 0x0090, 0x04, 0x0000),
    VmBiosTokenEntry::new(Q_PM_TYPE, 0x0094, 0x04, 0x0000),
    VmBiosTokenEntry::new(Q_SM_E_TYPE, 0x0098, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_SS_E_TYPE, 0x009a, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_FLOPPY_SEEK, 0x009c, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_QUICK_BOOT, 0x009d, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_ADDON_ROM_DISPLAY, 0x009e, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_NUM_LOCK, 0x009f, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_FLOPPY_PRESENT, 0x00a0, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_MATHCO_PRESENT, 0x00a1, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_KEYBOARD_PRESENT, 0x00a2, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_MOUSE_SUPPORT, 0x00a3, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_TYPEMATIC_RATE, 0x00a4, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_PARITY_ERROR, 0x00a5, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_FLOPPY_COUNT, 0x00a6, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_BASE_MEMORY_LSB, 0x00a8, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_BASE_MEMORY_MSB, 0x00b0, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_EXT_MEMORY_LSB, 0x00b8, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_EXT_MEMORY_MSB, 0x00c0, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_PM_EXT_TYPE, 0x00c8, 0x08, 0x0030),
    VmBiosTokenEntry::new(Q_PS_EXT_TYPE, 0x00d0, 0x08, 0x0030),
    VmBiosTokenEntry::new(Q_PM_CYL, 0x00d8, 0x10, 0x0000),
    VmBiosTokenEntry::new(Q_PM_HD, 0x00e8, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_PM_WPCOM, 0x00f0, 0x10, 0x0000),
    VmBiosTokenEntry::new(Q_PM_SPT, 0x0100, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_PS_CYL, 0x0108, 0x10, 0x0000),
    VmBiosTokenEntry::new(Q_PS_HD, 0x0118, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_PS_WPCOM, 0x0120, 0x10, 0x0000),
    VmBiosTokenEntry::new(Q_PS_SPT, 0x0130, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_SM_TYPE, 0x0138, 0x08, 0x0030),
    VmBiosTokenEntry::new(Q_SS_TYPE, 0x0140, 0x08, 0x0030),
    VmBiosTokenEntry::new(Q_OS2_COMP_MODE, 0x0148, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_WAIT_FOR_F1, 0x0149, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_DISPLAY_HIT_DEL, 0x014a, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_PCI_IDE_CONTROLLER, 0x014b, 0x03, 0x0004),
    VmBiosTokenEntry::new(Q_PM_SMART, 0x014e, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_PM_LBA_MODE, 0x0150, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_PM_BLOCK_MODE, 0x0151, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_PM_32BIT_XFER, 0x0152, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_PM_PIO_MODE, 0x0153, 0x03, 0x0000),
    VmBiosTokenEntry::new(Q_PM_EMULATION_TYPE, 0x0156, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_PS_LBA_MODE, 0x0158, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_PS_BLOCK_MODE, 0x0159, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_PS_32BIT_XFER, 0x015a, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_PS_PIO_MODE, 0x015b, 0x03, 0x0000),
    VmBiosTokenEntry::new(Q_PS_SMART, 0x015e, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_SM_LBA_MODE, 0x0160, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_SM_BLOCK_MODE, 0x0161, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_SM_32BIT_XFER, 0x0162, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_SM_PIO_MODE, 0x0163, 0x03, 0x0000),
    VmBiosTokenEntry::new(Q_PS_EMULATION_TYPE, 0x0166, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_SS_LBA_MODE, 0x0168, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_SS_BLOCK_MODE, 0x0169, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_SS_32BIT_XFER, 0x016a, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_SS_PIO_MODE, 0x016b, 0x03, 0x0000),
    VmBiosTokenEntry::new(Q_SM_SMART, 0x016e, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_CHECKSUM_MSB, 0x0170, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_CHECKSUM_LSB, 0x0178, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_POST_EXT_MEMORY_LSB, 0x0180, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_POST_EXT_MEMORY_MSB, 0x0188, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_CENTURY, 0x0190, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_SCRATCH_33, 0x0198, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_SCRATCH_34, 0x01a0, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_SCRATCH_35, 0x01a8, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_SCRATCH_36, 0x01b0, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_COLOR, 0x01b8, 0x04, 0x0000),
    VmBiosTokenEntry::new(Q_PM_DMA_MODE, 0x01bc, 0x04, 0x0000),
    VmBiosTokenEntry::new(Q_PS_DMA_MODE, 0x01c0, 0x04, 0x0000),
    VmBiosTokenEntry::new(Q_SM_DMA_MODE, 0x01c4, 0x04, 0x0000),
    VmBiosTokenEntry::new(Q_SM_EMULATION_TYPE, 0x01c8, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_SS_DMA_MODE, 0x01ca, 0x04, 0x0000),
    VmBiosTokenEntry::new(Q_SS_SMART, 0x01ce, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_SS_EMULATION_TYPE, 0x01d0, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_HDD_ACCESS_CONTROL, 0x01d2, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_ATA_DETECT_TIME_OUT, 0x01d3, 0x03, 0x0007),
    VmBiosTokenEntry::new(Q_ATA_80PIN_CABLE_DETECT, 0x01d6, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_PNP_AWARE_OS, 0x01d8, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_RESET_CONFIG_DATA, 0x01d9, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_PCI_LATENCY_TIMER, 0x01da, 0x03, 0x0001),
    VmBiosTokenEntry::new(Q_PCI_VGA_IRQ, 0x01dd, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_PCI_PALETTE_SNOOP, 0x01de, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_PCI_IDE_BUS_MASTER, 0x01df, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_PCI_OFFBOARD_IDE, 0x01e0, 0x03, 0x0000),
    VmBiosTokenEntry::new(Q_PCI_OFFBOARD_IDE_PRI_IRQ, 0x01e3, 0x03, 0x0000),
    VmBiosTokenEntry::new(Q_IRQ3_USED_BY_ISA, 0x01e6, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_IRQ4_USED_BY_ISA, 0x01e7, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_PCI_OFFBOARD_IDE_SEC_IRQ, 0x01e8, 0x03, 0x0000),
    VmBiosTokenEntry::new(Q_IRQ5_USED_BY_ISA, 0x01eb, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_IRQ7_USED_BY_ISA, 0x01ec, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_IRQ9_USED_BY_ISA, 0x01ed, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_IRQ10_USED_BY_ISA, 0x01ee, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_IRQ11_USED_BY_ISA, 0x01ef, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_EXT_CMOS_CHECKSUM_MSB, 0x01f0, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_EXT_CMOS_CHECKSUM_LSB, 0x01f8, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_SM_CYL, 0x0200, 0x10, 0x0000),
    VmBiosTokenEntry::new(Q_SM_HD, 0x0210, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_SM_WPCOM, 0x0218, 0x10, 0x0000),
    VmBiosTokenEntry::new(Q_SM_SPT, 0x0228, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_SS_CYL, 0x0230, 0x10, 0x0000),
    VmBiosTokenEntry::new(Q_SS_HD, 0x0240, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_SS_WPCOM, 0x0248, 0x10, 0x0000),
    VmBiosTokenEntry::new(Q_SS_SPT, 0x0258, 0x08, 0x0000),
    VmBiosTokenEntry::new(Q_IRQ14_USED_BY_ISA, 0x0260, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_IRQ15_USED_BY_ISA, 0x0261, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_DMA0_USED_BY_ISA, 0x0262, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_DMA1_USED_BY_ISA, 0x0263, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_DMA3_USED_BY_ISA, 0x0264, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_DMA5_USED_BY_ISA, 0x0265, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_DMA6_USED_BY_ISA, 0x0266, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_DMA7_USED_BY_ISA, 0x0267, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_MEM_SIZE_USED_BY_ISA, 0x0268, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_MEM_BASE_USED_BY_ISA, 0x026a, 0x03, 0x0002),
    VmBiosTokenEntry::new(Q_FLOPPY_CONTROLLER, 0x026d, 0x02, 0x0002),
    VmBiosTokenEntry::new(Q_COMB_IR_PIN_SELECT, 0x026f, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_COMA_PORT, 0x0270, 0x03, 0x0002),
    VmBiosTokenEntry::new(Q_COMB_PORT, 0x0273, 0x03, 0x0003),
    VmBiosTokenEntry::new(Q_COMB_MODE, 0x0276, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_COMB_DUPLEX_MODE, 0x0278, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_IR_PORT, 0x0279, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_IR_PORT_IRQ, 0x027b, 0x03, 0x0003),
    VmBiosTokenEntry::new(Q_LPT_MODE, 0x027e, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_LPT_PORT, 0x0280, 0x03, 0x0002),
    VmBiosTokenEntry::new(Q_LPT_EPPVER, 0x0283, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_LPT_ECP_DMA, 0x0284, 0x02, 0x0003),
    VmBiosTokenEntry::new(Q_LPT_IRQ, 0x0286, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_CPU_SERIAL, 0x0287, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_BU_UPDATE, 0x0288, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_CACHE_BUS_ECC, 0x0289, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_INTERNAL_CACHE, 0x028a, 0x02, 0x0002),
    VmBiosTokenEntry::new(Q_C0000_SHADOW, 0x028c, 0x02, 0x0002),
    VmBiosTokenEntry::new(Q_C4000_SHADOW, 0x028e, 0x02, 0x0002),
    VmBiosTokenEntry::new(Q_CPU_FREQ_STRAP, 0x0290, 0x05, 0x0018),
    VmBiosTokenEntry::new(Q_C8000_SHADOW, 0x0295, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_NB_SERR, 0x0297, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_CC000_SHADOW, 0x0298, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_D0000_SHADOW, 0x029a, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_D4000_SHADOW, 0x029c, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_D8000_SHADOW, 0x029e, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_DC000_SHADOW, 0x02a0, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_NB_PERR, 0x02a2, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_NB_USWC_WRITE_POST, 0x02a3, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_NB_MLT, 0x02a4, 0x03, 0x0002),
    VmBiosTokenEntry::new(Q_NB_PCI1_TO_PCI0_ACCESS, 0x02a7, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_NB_MTT, 0x02a8, 0x03, 0x0001),
    VmBiosTokenEntry::new(Q_NB_APERTURE_ACCESS_GLOBAL, 0x02ab, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_NB_PCI0_TO_APERTURE_ACCESS, 0x02ac, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_NB_DATA_INTEGRITY_MODE, 0x02ad, 0x02, 0x0003),
    VmBiosTokenEntry::new(Q_NB_EDO_RASX_WAIT_STATE, 0x02af, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_NB_DRAM_REFRESH_RATE, 0x02b0, 0x03, 0x0001),
    VmBiosTokenEntry::new(Q_NB_EDO_CASX_WAIT_STATE, 0x02b3, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_NB_SUSPEND_REFRESH, 0x02b4, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_NB_SDRAM_RAS_TO_CAS_DELAY, 0x02b5, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_NB_POWER_DOWN_SDRAM, 0x02b7, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_NB_SDRAM_RAS_PRECHARGE, 0x02b8, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_NB_SDRAM_PRECHARGE_CONTROL, 0x02ba, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_NB_ACPI_CONTROL, 0x02bc, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_NB_GATED_CLOCK, 0x02bd, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_NB_SEARCH_MDA, 0x02be, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_NB_AGP_SYNC, 0x02bf, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_NB_APERTURE_SIZE, 0x02c0, 0x03, 0x0004),
    VmBiosTokenEntry::new(Q_NB_AGP_SNOOP, 0x02c3, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_NB_AMTT, 0x02c4, 0x03, 0x0000),
    VmBiosTokenEntry::new(Q_AGP_SERR, 0x02c7, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_NB_LPTT, 0x02c8, 0x04, 0x0000),
    VmBiosTokenEntry::new(Q_AGP_PARITY_ERROR_RESPONSE, 0x02cc, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_EXTERNAL_CACHE, 0x02cd, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_SBH_SERR, 0x02cf, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_8BIT_IO_RECOVERY, 0x02d0, 0x04, 0x0000),
    VmBiosTokenEntry::new(Q_16BIT_IO_RECOVERY, 0x02d4, 0x03, 0x0000),
    VmBiosTokenEntry::new(Q_SBH_USB_PASSIVE_RELEASE, 0x02d7, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_SBH_PASSIVE_RELEASE, 0x02d8, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_SBH_DELAYED_TRANSACTION, 0x02d9, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_SBH_ROUTING1_DMA_TYPEF, 0x02da, 0x03, 0x0004),
    VmBiosTokenEntry::new(Q_SBH_ROUTING2_DMA_TYPEF, 0x02dd, 0x03, 0x0004),
    VmBiosTokenEntry::new(Q_SBH_DMA0_TYPE, 0x02e0, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_SBH_DMA1_TYPE, 0x02e2, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_SBH_DMA2_TYPE, 0x02e4, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_SBH_DMA3_TYPE, 0x02e6, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_SBH_DMA5_TYPE, 0x02e8, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_SBH_DMA6_TYPE, 0x02ea, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_SBH_DMA7_TYPE, 0x02ec, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_ACPI_OS, 0x02ee, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_ACPI_2, 0x02ef, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_ACPI_APIC_TBL, 0x02f0, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_ACPI_EXCH_BUF, 0x02f1, 0x01, 0x0001),
    VmBiosTokenEntry::new(Q_ACPI_HEADLESS_MODE, 0x02f2, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_LANGUAGE, 0x02f3, 0x03, 0x0000),
    VmBiosTokenEntry::new(Q_FLOPPY_SWAP, 0x02f6, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_I19_TRAP_ALLOW, 0x02f7, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_PCI_SLOT1_IRQ, 0x02f8, 0x04, 0x0000),
    VmBiosTokenEntry::new(Q_PCI_SLOT2_IRQ, 0x02fc, 0x04, 0x0000),
    VmBiosTokenEntry::new(Q_PCI_SLOT3_IRQ, 0x0300, 0x04, 0x0000),
    VmBiosTokenEntry::new(Q_PCI_SLOT4_IRQ, 0x0304, 0x04, 0x0000),
    VmBiosTokenEntry::new(Q_FIRST_BOOT_DRIVE, 0x0308, 0x03, 0x0000),
    VmBiosTokenEntry::new(Q_SECOND_BOOT_DRIVE, 0x030b, 0x03, 0x0001),
    VmBiosTokenEntry::new(Q_RD_FIRST_BOOT_DRIVE, 0x030e, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_THIRD_BOOT_DRIVE, 0x0310, 0x03, 0x0002),
    VmBiosTokenEntry::new(Q_FOURTH_BOOT_DRIVE, 0x0313, 0x03, 0x0003),
    VmBiosTokenEntry::new(Q_RD_SECOND_BOOT_DRIVE, 0x0316, 0x02, 0x0001),
    VmBiosTokenEntry::new(Q_FIFTH_BOOT_DRIVE, 0x0318, 0x03, 0x0004),
    VmBiosTokenEntry::new(Q_SIXTH_BOOT_DRIVE, 0x031b, 0x03, 0x0005),
    VmBiosTokenEntry::new(Q_RD_THIRD_BOOT_DRIVE, 0x031e, 0x02, 0x0002),
    VmBiosTokenEntry::new(Q_SEVENTH_BOOT_DRIVE, 0x0320, 0x03, 0x0006),
    VmBiosTokenEntry::new(Q_EIGHTH_BOOT_DRIVE, 0x0323, 0x03, 0x0007),
    VmBiosTokenEntry::new(Q_RD_FOURTH_BOOT_DRIVE, 0x0326, 0x02, 0x0003),
    VmBiosTokenEntry::new(Q_HD_FIRST_BOOT_DRIVE, 0x0328, 0x04, 0x0000),
    VmBiosTokenEntry::new(Q_HD_SECOND_BOOT_DRIVE, 0x032c, 0x04, 0x0001),
    VmBiosTokenEntry::new(Q_HD_THIRD_BOOT_DRIVE, 0x0330, 0x04, 0x0002),
    VmBiosTokenEntry::new(Q_HD_FOURTH_BOOT_DRIVE, 0x0334, 0x04, 0x0003),
    VmBiosTokenEntry::new(Q_HD_FIFTH_BOOT_DRIVE, 0x0338, 0x04, 0x0004),
    VmBiosTokenEntry::new(Q_HD_SIXTH_BOOT_DRIVE, 0x033c, 0x04, 0x0005),
    VmBiosTokenEntry::new(Q_HD_SEVENTH_BOOT_DRIVE, 0x0340, 0x04, 0x0006),
    VmBiosTokenEntry::new(Q_HD_EIGHTH_BOOT_DRIVE, 0x0344, 0x04, 0x0007),
    VmBiosTokenEntry::new(Q_HD_NINTH_BOOT_DRIVE, 0x0348, 0x04, 0x0008),
    VmBiosTokenEntry::new(Q_HD_TENTH_BOOT_DRIVE, 0x034c, 0x04, 0x0009),
    VmBiosTokenEntry::new(Q_HD_ELEVENTH_BOOT_DRIVE, 0x0350, 0x04, 0x000a),
    VmBiosTokenEntry::new(Q_HD_TWELVETH_BOOT_DRIVE, 0x0354, 0x04, 0x000b),
    VmBiosTokenEntry::new(Q_CD_FIRST_BOOT_DRIVE, 0x0358, 0x02, 0x0000),
    VmBiosTokenEntry::new(Q_CD_SECOND_BOOT_DRIVE, 0x035a, 0x02, 0x0001),
    VmBiosTokenEntry::new(Q_CD_THIRD_BOOT_DRIVE, 0x035c, 0x02, 0x0002),
    VmBiosTokenEntry::new(Q_CD_FOURTH_BOOT_DRIVE, 0x035e, 0x02, 0x0003),
    VmBiosTokenEntry::new(Q_SUPERVISOR_PASSWORD, 0x0360, 0x06, 0x0000),
    VmBiosTokenEntry::new(Q_DENY_SETUP, 0x0390, 0x02, 0x0003),
    VmBiosTokenEntry::new(Q_PASSWORD_CHECK, 0x0392, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_VIRUS_PROTECTION, 0x0393, 0x01, 0x0000),
    VmBiosTokenEntry::new(Q_USER_PASSWORD, 0x0398, 0x06, 0x0000),
    VmBiosTokenEntry::new(VMBIOS_ENDTOKEN, 0, 0, 0),
];

// This routine sets the value of a specific bit field within the CMOS
// (non-volatile) memory image. The token number is used to look up the
// token field position within a table that is generated as part of the
// AMI BIOS build process.
fn set_token_value(token: u16, value: u16, cmos: &mut [u8; 256]) {
    let mut target: VmBiosTokenEntry = Default::default();
    for entry in S_VM_BIOS_TOKEN_TABLE {
        if entry.token == token {
            target = *entry;
            break;
        }
    }

    if target.token == VMBIOS_ENDTOKEN
        || target.token <= Q_RTC_STATUS_D
        || target.token == Q_CENTURY
    {
        return;
    }

    //
    // Determine the byte and bit offset of the field being written
    // to the CMOS memory array.
    //
    let mut byte_offset = target.bit_offset / 8;
    let mut bit_offset = target.bit_offset % 8;
    let mut bits_left = target.bit_size;
    assert!(bits_left <= 16);

    //
    // Make sure the value is reasonable.
    //
    let temp_mask = if bits_left == 16 {
        0xFFFF
    } else {
        (1 << bits_left) - 1
    };

    let new_value = value & temp_mask;
    assert_eq!(value, new_value);
    while bits_left > 0 {
        //
        // Compute the mask for this byte.
        //
        let real_mask = (0xFF & ((0xFF & temp_mask) << bit_offset)) as u8;

        //
        // Do not touch any of the hardware CMOS/RTC values (byte offsets
        // 0 through 13 and 0x32).
        //
        if byte_offset > 0x0D && byte_offset != 0x32 {
            // Clear the old bits and OR in the new ones.
            cmos[byte_offset as usize] &= !real_mask;
            cmos[byte_offset as usize] |= (new_value << bit_offset) as u8;
        }

        //
        // Advance to the next byte.
        //
        if bits_left > 8 - bit_offset {
            bits_left -= 8 - bit_offset;
            byte_offset += 1;
            bit_offset = 0;
        } else {
            bits_left = 0;
        }
    }
}

/// Returns the default values PCAT BIOS expects to find in RTC CMOS memory.
///
/// By pre-initializing the CMOS values the first time the VM is booted, we
/// prevent the BIOS from reporting an error to the user.
pub fn default_cmos_values(mem_layout: &MemoryLayout) -> [u8; 256] {
    let mut cmos_data = [0; 256];
    let cmos = &mut cmos_data;

    // The default value for the "base memory" CMOS value is 640K (0x280).
    const CMOS_BASE_MEMORY_LOW_BYTE_DEFAULT: u8 = 0x80;
    const CMOS_BASE_MEMORY_HIGH_BYTE_DEFAULT: u8 = 0x02;

    // Parse all the entries in the BIOS token table and fill in the defaults.
    for entry in S_VM_BIOS_TOKEN_TABLE {
        set_token_value(entry.token, entry.default_value, cmos);
    }

    // Provide specific settings that correspond to the VM's initial hardware state.
    //
    // Set base memory to 640k, and extended memory to the top of the first
    // memory block.
    let total_extended_mem = mem_layout.ram()[0].range.len() >> 20;
    assert_eq!(total_extended_mem & !0xFFFF, 0);
    set_token_value(Q_EXT_MEMORY_LSB, (total_extended_mem & 0xFF) as u16, cmos);
    set_token_value(
        Q_EXT_MEMORY_MSB,
        ((total_extended_mem >> 8) & 0xFF) as u16,
        cmos,
    );

    set_token_value(
        Q_BASE_MEMORY_LSB,
        CMOS_BASE_MEMORY_LOW_BYTE_DEFAULT.into(),
        cmos,
    );
    set_token_value(
        Q_BASE_MEMORY_MSB,
        CMOS_BASE_MEMORY_HIGH_BYTE_DEFAULT.into(),
        cmos,
    );

    // Enable the math coprocessor.
    set_token_value(Q_MATHCO_PRESENT, 1, cmos);

    // Enable ACPI.
    set_token_value(Q_ACPI_OS, 1, cmos);

    // Turn the Parallel Port off for Viridian
    set_token_value(Q_LPT_PORT, 1, cmos);

    // Indicate the number and types of floppy drives.
    //
    // Hard coded for 1 floppy drive as the "A" drive.
    set_token_value(Q_FLOPPY_A, 4, cmos); // 4 means A drive present
    set_token_value(Q_FLOPPY_B, 0, cmos); // 0 means B drive not present
    set_token_value(Q_FLOPPY_PRESENT, 1, cmos); // 1 means some drives are present
    set_token_value(Q_FLOPPY_COUNT, 0, cmos); // 0 means 1 drive, 1 means 2 drives

    // Force a reset of the IDE configuration data.
    set_token_value(Q_RESET_CONFIG_DATA, 1, cmos);

    // The remaining settings are just configured to equal the "optimum"
    // settings, even though they don't really apply to us.
    set_token_value(Q_ATA_DETECT_TIME_OUT, 2, cmos);
    set_token_value(Q_NB_SDRAM_RAS_TO_CAS_DELAY, 2, cmos);
    set_token_value(Q_NB_SDRAM_RAS_PRECHARGE, 2, cmos);
    set_token_value(Q_NB_SDRAM_PRECHARGE_CONTROL, 1, cmos);
    set_token_value(Q_NB_AMTT, 1, cmos); // AGP Multi-Trans Timer
    set_token_value(Q_NB_LPTT, 1, cmos); // AGP Low-Priority Timer

    // Compute checksums so the BIOS doesn't think that the CMOS has been corrupted.
    //
    // Compute and set checksum for addresses 0x10 through 0x2D (right up to the checksum word).
    let mut basic_checksum = 0;
    for item in cmos[0x10..=0x2D].iter() {
        basic_checksum += *item as u16;
    }

    set_token_value(Q_CHECKSUM_LSB, basic_checksum & 0xFF, cmos);
    set_token_value(Q_CHECKSUM_MSB, basic_checksum >> 8, cmos);

    // Compute the extended checksum for addresses 0x42 to 0x7F (starting at the
    // extended checksum word through to the end).
    let mut ext_checksum = 0;
    for item in cmos[0x37..=0x3D].iter() {
        ext_checksum += *item as u16;
    }
    for item in cmos[0x40..=0x7F].iter() {
        ext_checksum += *item as u16;
    }

    set_token_value(Q_EXT_CMOS_CHECKSUM_LSB, ext_checksum & 0xFF, cmos);
    set_token_value(Q_EXT_CMOS_CHECKSUM_MSB, ext_checksum >> 8, cmos);

    // For the record, after rebooting a machine and forcing defaults in the CMOS,
    // the following is a list of entries that end up with different values after
    // the first boot.
    //
    // Their     Our
    // Value     Value    Token
    //  -----     -----    ----------------------------
    //   01        00       Q_PM_E_TYPE (01 = auto detect LBA/32-bit/etc settings)
    //   01        00       Q_PS_E_TYPE (01 = auto detect LBA/32-bit/etc settings)
    //   01        00       Q_SM_E_TYPE (01 = auto detect LBA/32-bit/etc settings)
    //   01        00       Q_SS_E_TYPE (01 = auto detect LBA/32-bit/etc settings)
    //   00        01       Q_RESET_CONFIG_DATA

    cmos_data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_cmos_values() {
        use memory_range::MemoryRange;
        use vm_topology::memory::MemoryLayout;
        use vm_topology::memory::MemoryRangeWithNode;

        const KB: u64 = 1024;
        const MB: u64 = 1024 * KB;
        const GB: u64 = 1024 * MB;
        const TB: u64 = 1024 * GB;

        let mmio = &[
            MemoryRange::new(GB..2 * GB),
            MemoryRange::new(3 * GB..4 * GB),
        ];
        let ram = &[
            MemoryRangeWithNode {
                range: MemoryRange::new(0..GB),
                vnode: 0,
            },
            MemoryRangeWithNode {
                range: MemoryRange::new(2 * GB..3 * GB),
                vnode: 0,
            },
            MemoryRangeWithNode {
                range: MemoryRange::new(4 * GB..TB + 2 * GB),
                vnode: 0,
            },
        ];
        let layout = MemoryLayout::new_from_ranges(42, ram, mmio).unwrap();

        const CMOS_DEFAULT: [u8; 256] = [
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x40, 0x00, 0x00, 0x20, 0x1f, 0x80, 0x02, 0x00, 0x04, 0x30, 0x30, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x30, 0x26,
            0x07, 0x07, 0x07, 0x07, 0x02, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x10, 0x86, 0x00, 0x40, 0x0b, 0x97, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x48, 0x1a, 0x19, 0x71, 0xa9, 0x18, 0x00,
            0xa8, 0xf1, 0x49, 0x76, 0x14, 0x01, 0x80, 0x93, 0x00, 0x40, 0x03, 0x00, 0x00, 0x08,
            0x5a, 0xac, 0xfe, 0x10, 0x32, 0x54, 0x76, 0x98, 0xba, 0xe4, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        assert_eq!(CMOS_DEFAULT, default_cmos_values(&layout));
    }
}
