// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types and constants specific to Hyper-V's firmware implementation.
//!
//! These values are all bespoke implementation details, and will not be found
//! in any public spec. Instead, they are transcribed directly from the Hyper-V
//! C/C++ sources.

pub mod bios_event_log;
pub mod boot_bios_log;
pub mod common;
pub mod crypto;
pub mod nvram;
pub mod time;

use guid::Guid;

/// MsvmPkg: `gEfiVmbusChannelDevicePathGuid`
pub const VM_HW_VENDOR_VMBUS_GUID: Guid =
    guid::guid!("9b17e5a2-0891-42dd-b653-80b5c22809ba");

/// MsvmPkg: `gSyntheticStorageClassGuid`
pub const VM_DISK_VMBUS_CHILD_GUID: Guid =
    guid::guid!("ba6163d9-04a1-4d29-b605-72e2ffb1dc7f");
