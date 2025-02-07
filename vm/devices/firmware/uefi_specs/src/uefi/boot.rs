// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Definitions related to UEFI boot entries

use guid::Guid;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

open_enum::open_enum! {
    /// From UEFI spec 7.2.1
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum EfiMemoryType: u32 {
        EFI_RESERVED_MEMORY_TYPE = 0,
        EFI_LOADER_CODE = 1,
        EFI_LOADER_DATA = 2,
        EFI_BOOT_SERVICES_CODE = 3,
        EFI_BOOT_SERVICES_DATA = 4,
        EFI_RUNTIME_SERVICES_CODE = 5,
        EFI_RUNTIME_SERVICES_DATA = 6,
        EFI_CONVENTIONAL_MEMORY = 7,
        EFI_UNUSABLE_MEMORY = 8,
        EFI_ACPI_RECLAIM_MEMORY = 9,
        EFI_ACPI_MEMORY_NVS = 10,
        EFI_MEMORY_MAPPED_IO = 11,
        EFI_MEMORY_MAPPED_IOPORT_SPACE = 12,
        EFI_PAL_CODE = 13,
        EFI_PERSISTENT_MEMORY = 14,
        EFI_UNACCEPTED_MEMORY_TYPE = 15,
        EFI_MAX_MEMORY_TYPE = 16,
    }
}

/// From UEFI spec 10.2
#[repr(C, packed)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct EfiDevicePathProtocol {
    pub device_type: EfiDeviceType,
    pub sub_type: u8,
    pub length: [u8; 2],
}

/// From UEFI spec 3.1.3
#[repr(C, packed)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct EfiLoadOption {
    pub attributes: u32,
    pub file_path_list_length: u16,
}

open_enum::open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum EfiDeviceType: u8 {
        HARDWARE = 0x01,
        ACPI = 0x02,
        MESSAGING = 0x03,
        MEDIA = 0x04,
        BIOS_BOOT_SPEC = 0x05,
        END = 0x7F,
    }
}

open_enum::open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum EfiEndDeviceSubType: u8 {
        INSTANCE = 0x01,
        ENTIRE = 0xFF,
    }
}

open_enum::open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum EfiHardwareDeviceSubType: u8 {
        PCI = 1,
        PCCARD = 2,
        MEMORY_MAPPED = 3,
        VENDOR = 4,
        CONTROLLER = 5,
        BMC = 6,
    }
}

open_enum::open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum EfiAcpiDeviceSubType: u8 {
        ACPI = 1,
        EXPANDED_ACPI = 2,
        ADR = 3,
        NVDIMM = 4,
    }
}

open_enum::open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum EfiMessagingDeviceSubType: u8 {
        ATAPI = 1,
        SCSI = 2,
        FIBRE_CHANNEL = 3,
        FIBRE_CHANNEL_EX = 21,
        IEEE_1394 = 4,
        USB = 5,
        SATA = 18,
        USB_WWID = 16,
        LOGICAL_UNIT =  17,
        USB_CLASS = 15,
        I20_RANDOM_BLOCK_STORAGE_CLASS = 6,
        MAC_ADDRESS = 11,
        IPV4 = 12,
        IPV6 = 13,
        VLAN = 20,
        INFINIBAND = 9,
        UART = 14,
        SAS = 10,
        SAS_EX = 22,
        ISCSI = 19,
        NVME_NAMESPACE = 23,
        URI = 24,
        UFS = 25,
        SD = 26,
        BLUETOOTH = 27,
        WIFI = 28,
        EMMC = 29,
        BLUETOOTH_LE = 30,
        DNS = 31,
        NVDIMM = 32,
        REST_SERVICE = 33,
        NVME_OF = 34,
    }
}

open_enum::open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum EfiMediaDeviceSubType: u8 {
        HARD_DRIVE = 0x01,
        CD_ROM = 0x02,
        VENDOR = 0x03,
        FILE = 0x04,
        MEDIA_PROTOCOL = 0x05,
        PIWG_FIRMWARE_FILE = 0x06,
        PIWG_FIRMWARE_VOLUME = 0x07,
        RELATIVE_OFFSET_RANGE = 0x08,
        RAM_DISK = 0x09,
    }
}

open_enum::open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum EfiPartitionFormat: u8 {
        MBR = 0x01,
        GUID = 0x02,
    }
}

open_enum::open_enum! {
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum EfiSignatureType: u8 {
        NONE = 0x00,
        MBR = 0x01,
        GUID = 0x02,
    }
}

#[repr(C, packed)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout, Debug, PartialEq)]
pub struct EfiHardDriveDevice {
    pub partition_number: u32,
    pub partition_start: u64,
    pub partition_size: u64,
    pub partition_signature: Guid,
    pub partition_format: EfiPartitionFormat,
    pub partition_type: EfiSignatureType,
}

#[repr(C, packed)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout, Debug, PartialEq)]
pub struct EfiScsiDevice {
    pub target_id: u16,
    pub logical_unit_num: u16,
}

#[repr(C, packed)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout, Debug, PartialEq)]
pub struct EfiMemoryMappedDevice {
    pub memory_type: EfiMemoryType,
    pub start_address: u64,
    pub end_address: u64,
}

#[repr(C, packed)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout, Debug, PartialEq)]
pub struct EfiExpandedAcpiDevice {
    pub hid: u32,
    pub cid: u32,
    pub uid: u32,
}
