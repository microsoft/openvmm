// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Nvram types defined in `BiosInterface.h`

use self::packed_nums::*;
use crate::hyperv::common::EfiStatus64NoErrorBit;
use bitfield_struct::bitfield;
use guid::Guid;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[allow(non_camel_case_types)]
mod packed_nums {
    pub type u64_ne = zerocopy::U64<zerocopy::NativeEndian>;
}

open_enum! {
    /// Command types for NVRAM_COMMAND_DESCRIPTOR.
    ///
    /// These correlate with the semantics of the UEFI runtime variable services.
    ///
    /// MsvmPkg: `NVRAM_COMMAND`
    #[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
    pub enum NvramCommand: u32 {
        GET_VARIABLE = 0,
        SET_VARIABLE = 1,
        GET_FIRST_VARIABLE_NAME = 2,
        GET_NEXT_VARIABLE_NAME = 3,
        QUERY_INFO = 4,
        SIGNAL_RUNTIME = 5,
        DEBUG_STRING = 6,
    }
}

/// MsvmPkg: `NVRAM_COMMAND_DESCRIPTOR`
#[repr(C)]
#[derive(Debug, Clone, Copy, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct NvramCommandDescriptor {
    pub command: NvramCommand,
    pub status: EfiStatus64NoErrorBit,
}

/// MsvmPkg: `NVRAM_COMMAND_DESCRIPTOR`
#[repr(C)]
#[derive(Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct NvramDebugStringCommand {
    pub padding: u32,
    pub address: u64_ne,
    pub len: u32,
}

/// MsvmPkg: `NVRAM_COMMAND_DESCRIPTOR`
#[repr(C)]
#[derive(Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct NvramVariableCommand {
    /// UEFI variable attributes associated with the variable: access rights
    /// (RT/BS).
    ///
    /// Used as input for the SetVariable command. Used as output for the
    /// GetVariable command.
    pub attributes: u32,

    /// GPA of the buffer containing a 16-bit unicode variable name.
    ///
    /// Memory at this location is read for the GetVariable, SetVariable,
    /// GetNextVariable command. Memory at this location is written to for the
    /// GetNextVariable command.
    pub name_address: u64_ne,

    /// Size in bytes of the buffer at VariableNameAddress.
    ///
    /// Used as input for GetVariable, SetVariable, and GetNextVariable
    /// commands. Used as output for the GetNextVariable command.
    pub name_bytes: u32,

    /// A GUID comprising the other half of the variable name.
    ///
    /// Used as input for GetVariable, SetVariable, and GetNextVariable
    /// commands. Used as output for the GetNextVariable command.
    pub vendor_guid: Guid,

    /// GPA of the buffer containing variable data. Memory at this location is
    /// written to for the GetVariable command.
    ///
    /// Memory at this location is read for the SetVariable command.
    pub data_address: u64_ne,

    /// Size of the buffer at VariableDataAddress.
    ///
    /// Used as input for the GetVariable command. Used as output for the
    /// GetVariable and SetVariable commands.
    pub data_bytes: u32,
}

/// MsvmPkg: `NVRAM_COMMAND_DESCRIPTOR`
#[repr(C)]
#[derive(Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct NvramQueryInfo {
    /// Attribute mask, controls variable type for which the information is
    /// returned.
    ///
    /// Used as an input for the QueryInfo command.
    pub attributes: u32,

    // These are outputs for the QueryInfo command.
    pub maximum_variable_storage: u64_ne,
    pub remaining_variable_storage: u64_ne,
    pub maximum_variable_size: u64_ne,
}

/// MsvmPkg: `NVRAM_COMMAND_DESCRIPTOR`
#[bitfield(u64)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct SignalRuntimeCommandFlags {
    pub vsm_aware: bool,
    #[bits(63)]
    _reserved: u64,
}

/// MsvmPkg: `NVRAM_COMMAND_DESCRIPTOR`
///
/// ```text
/// union
/// {
///     struct
///     {
///         UINT64 VsmAware : 1;
///         UINT64 Unused   : 63;
///     } S;
///     UINT64 AsUINT64;
/// } SignalRuntimeCommand;
/// ```
#[repr(C)]
#[derive(Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct NvramSignalRuntimeCommand {
    pub flags: SignalRuntimeCommandFlags,
}

pub mod vars {
    use guid::Guid;

    const SECURE_BOOT_ENABLE_GUID: Guid =
        Guid::from_static_str("f0a30bc7-af08-4556-99c4-001009c93a44");

    pub const MSFT_SECURE_BOOT_PRODUCTION_GUID: Guid =
        Guid::from_static_str("77fa9abd-0359-4d32-bd60-28f4e78f784b");

    const EFI_HYPERV_PRIVATE_GUID: Guid =
        Guid::from_static_str("610b9e98-c6f6-47f8-8b47-2d2da0d52a91");

    defn_nvram_var!(SECURE_BOOT_ENABLE = (SECURE_BOOT_ENABLE_GUID, "SecureBootEnable"));
    defn_nvram_var!(CURRENT_POLICY = (MSFT_SECURE_BOOT_PRODUCTION_GUID, "CurrentPolicy"));
    defn_nvram_var!(OS_LOADER_INDICATIONS = (EFI_HYPERV_PRIVATE_GUID, "OsLoaderIndications"));
    defn_nvram_var!(
        OS_LOADER_INDICATIONS_SUPPORTED = (EFI_HYPERV_PRIVATE_GUID, "OsLoaderIndicationsSupported")
    );
}
