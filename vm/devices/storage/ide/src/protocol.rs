// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use bitfield_struct::bitfield;
use bitflags::bitflags;
use inspect::Inspect;
use open_enum::open_enum;
use zerocopy::AsBytes;
use zerocopy::FromBytes;
use zerocopy::FromZeroes;

#[allow(non_camel_case_types)]
mod packed_nums {
    pub type u32_le = zerocopy::U32<zerocopy::LittleEndian>;
    pub type u64_le = zerocopy::U64<zerocopy::LittleEndian>;
}

// status register flags
bitflags! {
    #[derive(Inspect)]
    #[inspect(debug)]
    pub struct Status: u8 {
        const ERR =  0b0000_0001; // Error occurred on last transaction (see error reg for details)
        const HIT =  0b0000_0010; // Set once per rotation (we don't support it)
        const CORR =  0b0000_0100; // Correctable read error was hit (we don't support it)
        const DRQ =  0b0000_1000; // Drive wants to exchange data with host
        const DSC =  0b0001_0000; // Heads are positioned over desired cylinder
        const DF =  0b0010_0000; // Drive fault - major hardware error (we don't support it)
        const DRDY = 0b0100_0000; // Drive is ready for next command
        const BSY =  0b1000_0000; // Host cannot access any IDE registers at this time
    }
}

// ide commands
open_enum! {
    #[derive(AsBytes, FromBytes, FromZeroes, Inspect)]
    #[inspect(debug)]
    pub enum IdeCommand: u8 {
        DEVICE_RESET = 0x08,
        SOFT_RESET = 0x04,
        RECALIBRATE_START = 0x10,
        RECALIBRATE_END = 0x1F,
        READ_SECTORS = 0x20,
        READ_SECTORS_ALT = 0x21,
        READ_ONE_SECTOR = 0x22,
        READ_ONE_SECTOR_ALT = 0x23,
        READ_MULTI_SECTORS_EXT = 0x24,
        READ_DMA_EXT = 0x25,
        READ_MULTI_BLOCKS_EXT = 0x29,
        WRITE_SECTORS = 0x30,
        WRITE_SECTORS_ALT = 0x31,
        WRITE_ONE_SECTOR = 0x32,
        WRITE_ONE_SECTOR_ALT = 0x33,
        WRITE_MULTI_SECTORS_EXT = 0x34,
        WRITE_DMA_EXT = 0x35,
        WRITE_MULTI_BLOCKS_EXT = 0x39,
        WRITE_DMA_FUA_EXT = 0x3D,
        VERIFY_MULTI_SECTORS = 0x40,
        VERIFY_MULTI_SECTORS_ALT = 0x41,
        VERIFY_MULTI_SECTORS_EXT = 0x42,
        FORMAT_TRACK = 0x50,
        SEEK_START = 0x70,
        SEEK_END = 0x7F,
        EXECUTE_DEVICE_DIAGNOSTIC = 0x90,
        INIT_DRIVE_PARAMETERS = 0x91,
        PACKET_COMMAND = 0xA0,
        IDENTIFY_PACKET_DEVICE = 0xA1,
        SET_MULTI_BLOCK_MODE = 0xC6,
        READ_DMA = 0xC8,
        READ_DMA_ALT = 0xC9,
        WRITE_DMA = 0xCA,
        WRITE_DMA_ALT = 0xCB,
        WRITE_MULTIPLE_BLOCKS_EXT_FUA = 0xCE,
        STANDBY_IMMEDIATE = 0xE0,
        IDLE_IMMEDIATE = 0xE1,
        STANDBY = 0xE2,
        IDLE = 0xE3,
        CHECK_POWER_MODE = 0xE5,
        SLEEP = 0xE6,
        FLUSH_CACHE = 0xE7,
        FLUSH_CACHE_EXT = 0xEA,
        IDENTIFY_DEVICE = 0xEC,
        SET_FEATURES = 0xEF,
    }
}

// errors
bitflags! {
    #[repr(C)]
    #[derive(Inspect, AsBytes, FromBytes, FromZeroes)]
    pub struct ErrorReg: u8 {
        const ERR_NONE = 0x00;
        const ERR_AMNF_ILI_DEFAULT = 0x01; // no address mark or illegal length indication, register default values
        const ERR_TK0NF_EOM = 0x02; // track 0 not found or end of media detected
        const ERR_UNKNOWN_COMMAND = 0x04; // Command aborted
        const ERR_MCR = 0x08; // media change request
        const ERR_BAD_LOCATION = 0x10; // IDNF, ID mark not found
        const ERR_MEDIA_CHANGED = 0x20; // mc, media changed
        const ERR_UNC = 0x40; // uncorrectable data
        const ERR_BAD_SECTOR = 0x80; // bbk, bad block
    }
}

impl From<u8> for ErrorReg {
    fn from(v: u8) -> Self {
        Self::from_bits(v).unwrap()
    }
}

pub const MAX_SECTORS_MULT_TRANSFER_DEFAULT: u16 = 128;
pub const MAX_SECTORS_48BIT_LBA: u64 = 0x0000FFFFFFFFFFFF;
pub const MAX_BYTES_48BIT_LBA: u64 = MAX_SECTORS_48BIT_LBA * 512;
pub const MAX_48BIT_SECTOR_COUNT: u32 = 0x10000;

pub const DEVICE_ACTIVE_OR_IDLE: u8 = 0xFF;

#[derive(Inspect)]
#[bitfield(u8)]
#[derive(AsBytes, FromBytes, FromZeroes)]
pub struct DeviceHeadReg {
    #[bits(4)]
    pub head: u8,
    pub dev: bool,
    pub obs1: bool,
    pub lba: bool,
    pub obs2: bool,
}

pub const HARD_DRIVE_SECTOR_BYTES: u32 = 512;
pub const CD_DRIVE_SECTOR_BYTES: u32 = 2048;
pub const LBA_28BIT_MAX_SECTORS: u32 = 0x0FFFFFFF;
pub const MAX_CHS_SECTORS: u32 = 0xFC0000;

pub const ATAPI_RESET_LBA_MID: u8 = 0x14;
pub const ATAPI_RESET_LBA_HIGH: u8 = 0xeb;

// These values go into the cylinder count field
// as additional status for ATAPI devices.
pub const ATAPI_READY_FOR_PACKET_DEFAULT: u8 = 0x01;
pub const ATAPI_DATA_FOR_HOST: u8 = 0x02;
pub const ATAPI_COMMAND_COMPLETE: u8 = 0x03;

#[repr(C)]
#[derive(AsBytes, FromBytes, FromZeroes)]
pub struct IdeFeatures {
    pub config_bits: u16,                              // word 0
    pub cylinders: u16,                                // word 1
    pub reserved2: u16,                                // word 2
    pub heads: u16,                                    // word 3
    pub unformatted_sectors_per_track: u16,            // word 4, ATA1 only
    pub unformatted_bytes_per_sector: u16,             // word 5, ATA1 only
    pub sectors_per_track: u16,                        // word 6
    pub compact_flash: [u16; 2], // words 7-8, reserved for assignment by CompactFlash association starting in ATA5
    pub vendor0: u16,            // word 9, vendor specific, marked as retired in ATA4
    pub serial_no: [u8; 20],     // words 10-19, 20 ASCII characters, all spaces
    pub buffer_type: u16,        // word 20, ATA1 only
    pub buffer_size: u16,        // word 21, ATA1 only, size in 512B increments
    pub reserved22: u16,         // word 22, obsolete
    pub firmware_revision: [u8; 8], // words 23-26, 8 ASCII characters, all spaces
    pub model_number: [u8; 40],  // words 27-46, 40 ASCII characters, "iVtrau lDH" padded by spaces
    pub max_sectors_mult_transfer: u16, // word 47, maximum number of sectors that shall be transferred per interrupt on READ/WRITE MULTIPLE commands
    pub reserved48: u16,                // word 48, reserved
    pub capabilities: u16,              // word 49, default hard drive capabilities
    pub reserved50: u16, // word 50, unused, capabilities (standby timer value minimum)
    pub pio_cycle_times: u16, // word 51
    pub dma_cycle_times: u16, // word 52
    pub new_words_valid_flags: u16, // word 53
    pub log_cylinders: u16, // word 54
    pub log_heads: u16,  // word 55
    pub log_sectors_per_track: u16, // word 56
    pub log_total_sectors: packed_nums::u32_le, // word 57-58
    pub multi_sector_capabilities: u16, // word 59
    pub user_addressable_sectors: packed_nums::u32_le, // word 60
    pub single_word_dma_mode: u16, // word 62, first marked as obsolete in ATA3
    pub multi_word_dma_mode: u16, // word 63
    pub enhanced_pio_mode: u16, // word 64
    pub min_multi_dma_time: u16, // word 65
    pub recommended_multi_dma_time: u16, // word 66
    pub min_pio_cycle_time_no_flow: u16, // word 67
    pub min_pio_cycle_time_flow: u16, // word 68
    pub reserved69_79: [u16; 11], // words 69-79, unused
    pub major_version_number: u16, // word 80, ATA protocols supported
    pub minor_version_number: u16, // word 81, unused
    pub command_set_supported: u16, // word 82
    pub command_sets_supported: u16, // word 83
    pub command_set_supported_ext: u16, // word 84
    pub command_set_enabled1: u16, // word 85
    pub command_set_enabled2: u16, // word 86
    pub command_set_default: u16, // word 87
    pub reserved88_99: [u16; 12], // words 88-99, unused
    pub total_sectors_48_bit: packed_nums::u64_le, // words 100-103
    pub reserved104_105: [u16; 2], // words 104-105, reserved
    pub default_sector_size_config: u16, // word 106
    pub reserved107_208: [u16; 102], // words 107-208, unused
    pub logical_block_alignment: u16, // word 209
    pub reserved210_255: [u16; 46], // words 210-255, unused
}

pub const IDENTIFY_DEVICE_BYTES: usize = 512;
static_assertions::assert_eq_size!(IdeFeatures, [u8; IDENTIFY_DEVICE_BYTES]);

// PCI
pub const BX_PCI_ISA_BRIDGE_IDE_IDREG_VALUE: u32 = 0x71118086;
pub const BX_PCI_IDE_CLASS_WORD: u32 = 0x01018001;

pub const PCI_CONFIG_STATUS_IO_SPACE_ENABLE_MASK: u32 = 0x01;
pub const PCI_CONFIG_STATUS_BUS_MASTER_ENABLE_MASK: u32 = 0x04;
pub const CFCS_BUS_MASTER_IO_ENABLE_MASK: u32 =
    PCI_CONFIG_STATUS_IO_SPACE_ENABLE_MASK | PCI_CONFIG_STATUS_BUS_MASTER_ENABLE_MASK;

open_enum! {
    pub enum IdeConfigSpace: u16 {
        PRIMARY_TIMING_REG_ADDR = 0x40,
        SECONDARY_TIMING_REG_ADDR = 0x44,
        UDMA_CTL_REG_ADDR = 0x48,
        MANUFACTURE_ID_REG_ADDR = 0xF8,
    }
}

open_enum! {
    pub enum BusMasterReg: u16 {
        COMMAND = 0,
        STATUS = 2,
        TABLE_PTR = 4,
        TABLE_PTR2 = 6,
    }
}

bitflags! {
    #[derive(Default, Inspect)]
    #[inspect(debug)]
    pub struct BusMasterCommandReg: u32 {
        const START = 0x01;
        const WRITE = 0x08;
    }

    #[derive(Default, Inspect)]
    #[inspect(debug)]
    pub struct BusMasterStatusReg: u32 {
        const ACTIVE = 0x01;
        const DMA_ERROR = 0x02;
        const INTERRUPT = 0x04;
        const SETTABLE = 0x60; // Don't know what these bits mean, but they can be set by the guest.
    }
}

bitflags! {
    #[derive(Inspect)]
    #[inspect(debug)]
    pub struct DeviceControlReg: u8 {
        const HIGH_ORDER_BYTE = 0x80;   // HOB
        const RESET = 0x4;              // SRST
        const INTERRUPT_MASK = 0x2;     // nIEN
    }
}

#[repr(C)]
#[derive(Debug, AsBytes, FromBytes, FromZeroes)]
pub struct BusMasterDmaDesc {
    pub mem_physical_base: u32,
    pub byte_count: u16,
    pub unused: u8,
    pub end_of_table: u8,
}

#[repr(C)]
#[derive(Debug, AsBytes, FromBytes, FromZeroes)]
pub struct EnlightenedInt13Command {
    pub command: IdeCommand,
    pub device_head: DeviceHeadReg,
    pub flags: u8,
    pub result_status: u8,
    pub lba_low: u32,
    pub lba_high: u16,
    pub block_count: u16,
    pub byte_count: u32,
    pub data_buffer: u32,
    pub skip_bytes_head: u16,
    pub skip_bytes_tail: u16,
}
