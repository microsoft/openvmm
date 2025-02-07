// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use bitfield_struct::bitfield;
use inspect::Inspect;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[allow(non_camel_case_types)]
mod packed_nums {
    pub type u32_le = zerocopy::U32<zerocopy::LittleEndian>;
    pub type u64_le = zerocopy::U64<zerocopy::LittleEndian>;
}

// status register flags
#[derive(Inspect)]
#[bitfield(u8)]
pub struct Status {
    /// Error occurred on last transaction (see error reg for details)
    pub err: bool,
    /// Set once per rotation (we don't support it)
    pub hit: bool,
    /// Correctable read error was hit (we don't support it)
    pub corr: bool,
    /// Drive wants to exchange data with host
    pub drq: bool,
    /// Heads are positioned over desired cylinder
    pub dsc: bool,
    /// Drive fault - major hardware error (we don't support it)
    pub df: bool,
    /// Drive is ready for next command
    pub drdy: bool,
    /// Host cannot access any IDE registers at this time
    pub bsy: bool,
}

// ide commands
open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
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
#[derive(Inspect)]
#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct ErrorReg {
    /// no address mark or illegal length indication, register default values
    pub amnf_ili_default: bool,
    /// track 0 not found or end of media detected
    pub tk0nf_eom: bool,
    /// Command aborted
    pub unknown_command: bool,
    /// media change request
    pub mcr: bool,
    /// IDNF, ID mark not found
    pub bad_location: bool,
    /// mc, media changed
    pub media_changed: bool,
    /// uncorrectable data
    pub unc: bool,
    /// bbk, bad block
    pub bad_sector: bool,
}

pub const MAX_SECTORS_MULT_TRANSFER_DEFAULT: u16 = 128;
pub const MAX_SECTORS_48BIT_LBA: u64 = 0x0000FFFFFFFFFFFF;
pub const MAX_BYTES_48BIT_LBA: u64 = MAX_SECTORS_48BIT_LBA * 512;
pub const MAX_48BIT_SECTOR_COUNT: u32 = 0x10000;

pub const DEVICE_ACTIVE_OR_IDLE: u8 = 0xFF;

#[derive(Inspect)]
#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
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
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
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

#[derive(Inspect)]
#[bitfield(u32)]
pub struct BusMasterCommandReg {
    pub start: bool,
    #[bits(2)]
    reserved: u32,
    pub write: bool,
    #[bits(28)]
    reserved2: u32,
}

impl BusMasterCommandReg {
    pub fn from_bits_truncate(bits: u32) -> Self {
        Self::from_bits(bits).with_reserved(0).with_reserved2(0)
    }
}

#[derive(Inspect)]
#[bitfield(u32)]
pub struct BusMasterStatusReg {
    pub active: bool,
    pub dma_error: bool,
    pub interrupt: bool,
    #[bits(2)]
    reserved: u32,
    /// Don't know what these bits mean, but they can be set by the guest.
    #[bits(2)]
    pub settable: u32,
    #[bits(25)]
    reserved2: u32,
}

impl BusMasterStatusReg {
    pub fn from_bits_truncate(bits: u32) -> Self {
        Self::from_bits(bits).with_reserved(0).with_reserved2(0)
    }
}

#[derive(Inspect)]
#[bitfield(u8)]
pub struct DeviceControlReg {
    reserved: bool,
    /// nIEN
    pub interrupt_mask: bool,
    /// SRST
    pub reset: bool,
    #[bits(4)]
    reserved2: u8,
    /// HOB
    pub high_order_byte: bool,
}

impl DeviceControlReg {
    pub fn from_bits_truncate(bits: u8) -> Self {
        Self::from_bits(bits).with_reserved(false).with_reserved2(0)
    }
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct BusMasterDmaDesc {
    pub mem_physical_base: u32,
    pub byte_count: u16,
    pub unused: u8,
    pub end_of_table: u8,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
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
