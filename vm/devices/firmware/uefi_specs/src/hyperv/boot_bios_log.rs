// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types and constants defined in `BootBiosLogInterface.h`

use crate::uefi::common::EfiStatus64;
use open_enum::open_enum;
use static_assertions::const_assert_eq;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Event Id for Device Boot Attempts
pub const BOOT_DEVICE_EVENT_ID: u32 = 1;

// Device status code groups
const DEVICE_STATUS_BOOT_GROUP: u32 = 0x00010000;
const DEVICE_STATUS_SECURE_BOOT_GROUP: u32 = 0x00020000;
const DEVICE_STATUS_NETWORK_GROUP: u32 = 0x00030000;

open_enum! {
    /// Device failure reason codes
    ///
    /// Status codes are made up of a group ID in the high word and a
    /// status code in the low word
    ///
    /// If items are added to this enum the UEFI string mapping function
    /// PlatformConsoleDeviceStatusString and the corresponding string table
    /// in PlatformBdsString.uni must be updated
    ///
    /// reSearch query: `BOOT_DEVICE_STATUS`
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum BootDeviceStatus: u32 {
        BOOT_PENDING = 0,
        BOOT_DEVICE_NO_FILESYSTEM               = DEVICE_STATUS_BOOT_GROUP,
        BOOT_DEVICE_NO_LOADER                   = DEVICE_STATUS_BOOT_GROUP + 1,
        BOOT_DEVICE_INCOMPATIBLE_LOADER         = DEVICE_STATUS_BOOT_GROUP + 2,
        BOOT_DEVICE_RETURNED_FAILURE            = DEVICE_STATUS_BOOT_GROUP + 3,
        BOOT_DEVICE_OS_NOT_LOADED               = DEVICE_STATUS_BOOT_GROUP + 4,
        BOOT_DEVICE_OS_LOADED                   = DEVICE_STATUS_BOOT_GROUP + 5,
        BOOT_DEVICE_NO_DEVICES                  = DEVICE_STATUS_BOOT_GROUP + 6,
        BOOT_DEVICE_LOAD_ERROR                  = DEVICE_STATUS_BOOT_GROUP + 7,
        SECURE_BOOT_FAILED                      = DEVICE_STATUS_SECURE_BOOT_GROUP,
        SECURE_BOOT_POLICY_DENIED               = DEVICE_STATUS_SECURE_BOOT_GROUP + 1,
        SECURE_BOOT_HASH_DENIED                 = DEVICE_STATUS_SECURE_BOOT_GROUP + 2,
        SECURE_BOOT_CERT_DENIED                 = DEVICE_STATUS_SECURE_BOOT_GROUP + 3,
        SECURE_BOOT_INVALID_IMAGE               = DEVICE_STATUS_SECURE_BOOT_GROUP + 4,
        SECURE_BOOT_UNSIGNED_HAS_NOT_IN_DB      = DEVICE_STATUS_SECURE_BOOT_GROUP + 5,
        SECURE_BOOT_SIGNED_HASH_NOT_FOUND       = DEVICE_STATUS_SECURE_BOOT_GROUP + 6,
        SECURE_BOOT_NEITHER_CERT_NOR_HASH_IN_DB = DEVICE_STATUS_SECURE_BOOT_GROUP + 7,
        NETWORK_BOOT_MEDIA_DISCONNECTED         = DEVICE_STATUS_NETWORK_GROUP,
        NETWORK_BOOT_DHCP_FAILED                = DEVICE_STATUS_NETWORK_GROUP + 1,
        NETWORK_BOOT_NO_RESPONSE                = DEVICE_STATUS_NETWORK_GROUP + 2,
        NETWORK_BOOT_BUFFER_TOO_SMALL           = DEVICE_STATUS_NETWORK_GROUP + 3,
        NETWORK_BOOT_DEVICE_ERROR               = DEVICE_STATUS_NETWORK_GROUP + 4,
        NETWORK_BOOT_NO_RESOURCES               = DEVICE_STATUS_NETWORK_GROUP + 5,
        NETWORK_BOOT_SERVER_TIMEOUT             = DEVICE_STATUS_NETWORK_GROUP + 6,
        NETWORK_BOOT_CANCELLED                  = DEVICE_STATUS_NETWORK_GROUP + 7,
        NETWORK_BOOT_ICMP_ERROR                 = DEVICE_STATUS_NETWORK_GROUP + 8,
        NETWORK_BOOT_TFTP_ERROR                 = DEVICE_STATUS_NETWORK_GROUP + 9,
        NETWORK_BOOT_NO_BOOT_FILE               = DEVICE_STATUS_NETWORK_GROUP + 10,
        NETWORK_BOOT_UNEXPECTED_FAILURE         = DEVICE_STATUS_NETWORK_GROUP + 11,
    }
}

impl Default for BootDeviceStatus {
    fn default() -> Self {
        Self::BOOT_PENDING
    }
}

impl BootDeviceStatus {
    pub fn get_boot_device_status_group(&self) -> u32 {
        self.0 & 0xFFFF0000
    }
}

/// reSearch query: `BOOTEVENT_DEVICE_ENTRY`
#[repr(C)]
#[derive(Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct BootEventDeviceEntry {
    pub status: BootDeviceStatus,
    pub pad: u32,
    pub extended_status: EfiStatus64,
    pub boot_variable_number: u16,
    pub pad1: u16,
    pub device_path_size: u32,
    // Variable device_path payload
}

const_assert_eq!(size_of::<BootEventDeviceEntry>(), 24);
