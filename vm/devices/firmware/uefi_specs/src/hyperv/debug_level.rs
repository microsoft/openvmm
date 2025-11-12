// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Debug level mappings defined by Project Mu's MU_BASECORE package,
//! used in the Hyper-V UEFI firmware.

#![warn(missing_docs)]

/// Initialization
pub const DEBUG_INIT: u32 = 0x0000_0001;
/// Warnings
pub const DEBUG_WARN: u32 = 0x0000_0002;
/// Load events
pub const DEBUG_LOAD: u32 = 0x0000_0004;
/// EFI File system
pub const DEBUG_FS: u32 = 0x0000_0008;
/// Alloc & Free (pool)
pub const DEBUG_POOL: u32 = 0x0000_0010;
/// Alloc & Free (page)
pub const DEBUG_PAGE: u32 = 0x0000_0020;
/// Informational debug messages
pub const DEBUG_INFO: u32 = 0x0000_0040;
/// PEI/DXE/SMM Dispatchers
pub const DEBUG_DISPATCH: u32 = 0x0000_0080;
/// Variable
pub const DEBUG_VARIABLE: u32 = 0x0000_0100;
/// Boot Manager
pub const DEBUG_BM: u32 = 0x0000_0400;
/// BlkIo Driver
pub const DEBUG_BLKIO: u32 = 0x0000_1000;
/// Network Io Driver
pub const DEBUG_NET: u32 = 0x0000_4000;
/// UNDI Driver
pub const DEBUG_UNDI: u32 = 0x0001_0000;
/// LoadFile
pub const DEBUG_LOADFILE: u32 = 0x0002_0000;
/// Event messages
pub const DEBUG_EVENT: u32 = 0x0008_0000;
/// Global Coherency Database changes
pub const DEBUG_GCD: u32 = 0x0010_0000;
/// Memory range cachability changes
pub const DEBUG_CACHE: u32 = 0x0020_0000;
/// Detailed debug messages that may significantly impact boot performance
pub const DEBUG_VERBOSE: u32 = 0x0040_0000;
/// Detailed debug and payload manageability messages related to modules such as Redfish, IPMI, MCTP etc.
pub const DEBUG_MANAGEABILITY: u32 = 0x0080_0000;
/// Error
pub const DEBUG_ERROR: u32 = 0x8000_0000;

/// Maps debug levels to their descriptive names.
pub const DEBUG_FLAG_NAMES: &[(u32, &str)] = &[
    (DEBUG_INIT, "INIT"),
    (DEBUG_WARN, "WARNING"),
    (DEBUG_LOAD, "LOAD"),
    (DEBUG_FS, "FILESYSTEM"),
    (DEBUG_POOL, "POOL"),
    (DEBUG_PAGE, "PAGE"),
    (DEBUG_INFO, "INFO"),
    (DEBUG_DISPATCH, "DISPATCH"),
    (DEBUG_VARIABLE, "VARIABLE"),
    (DEBUG_BM, "BOOTMANAGER"),
    (DEBUG_BLKIO, "BLOCKIO"),
    (DEBUG_NET, "NETWORK"),
    (DEBUG_UNDI, "UNDI"),
    (DEBUG_LOADFILE, "LOADFILE"),
    (DEBUG_EVENT, "EVENT"),
    (DEBUG_GCD, "GCD"),
    (DEBUG_CACHE, "CACHE"),
    (DEBUG_VERBOSE, "VERBOSE"),
    (DEBUG_MANAGEABILITY, "MANAGEABILITY"),
    (DEBUG_ERROR, "ERROR"),
];
