// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Virtualization Based Security (VBS) platform definitions defined by Hyper-V

#![allow(non_camel_case_types)]

use bitfield_struct::bitfield;
use igvm_defs::PAGE_SIZE_4K;
use open_enum::open_enum;
use static_assertions::const_assert;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

pub const VBS_VP_CHUNK_SIZE_BYTES: usize = PAGE_SIZE_4K as usize + size_of::<VpGpaPageChunk>();

/// Structure containing the completed VBS boot measurement of the IGVM file.
/// The signature of the hash of this struct is the signature for [`igvm_defs::IGVM_VHS_VBS_MEASUREMENT`]
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, Debug)]
pub struct VBS_VM_BOOT_MEASUREMENT_SIGNED_DATA {
    /// The version of the signature structure
    pub version: u32,
    /// The user supplied product id
    pub product_id: u32,
    /// The uesr supplied module id
    pub module_id: u32,
    /// The user supplied svn
    pub security_version: u32,
    /// Security policy for the guest
    pub security_policy: VBS_POLICY_FLAGS,
    /// Algorithm that created the boot digest hash
    pub boot_digest_algo: u32,
    /// Algorithm that produces the signature
    pub signing_algo: u32,
    /// VBS Boot digest
    pub boot_measurement_digest: [u8; 32],
}

/// Chunk that is measured to generate digest. These consist of a 16 byte header followed by data.
/// This needs c style alignment to generate a consistent measurement.
/// Defined by the following struct in C:
/// ``` ignore
/// typedef struct _VBS_VM_BOOT_MEASUREMENT_CHUNK
/// {
///     UINT32 ByteCount;
///     VBS_VM_BOOT_MEASUREMENT_CHUNK_TYPE Type;
///     UINT64 Reserved;
///
///     union
///     {
///         VBS_VM_BOOT_MEASUREMENT_CHUNK_VP_REGISTER VpRegister;
///         VBS_VM_BOOT_MEASUREMENT_CHUNK_VP_VTL_ENABLED VpVtlEnabled;
///         VBS_VM_BOOT_MEASUREMENT_CHUNK_GPA_PAGE GpaPage;
///     } u;
/// } VBS_VM_BOOT_MEASUREMENT_CHUNK, *PVBS_VM_BOOT_MEASUREMENT_CHUNK;
/// ```
///
/// Structure describing the chunk to be measured
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout)]
pub struct VbsChunkHeader {
    /// The full size to be measured
    pub byte_count: u32,
    pub chunk_type: BootMeasurementType,
    pub reserved: u64,
}

/// Structure describing the register being measured. Will be padded to [`VBS_VP_CHUNK_SIZE_BYTES`] when hashed to generate digest
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout)]
pub struct VbsRegisterChunk {
    pub header: VbsChunkHeader,
    pub reserved: u32,
    pub vtl: u8,
    pub reserved2: u8,
    pub reserved3: u16,
    pub reserved4: u32,
    pub name: u32,
    pub value: [u8; 16],
}
const_assert!(size_of::<VbsRegisterChunk>() <= VBS_VP_CHUNK_SIZE_BYTES);

/// Structure describing the page to be measured.
/// Page data is hashed after struct to generate digest, if not a full page, measurable data will be padded to [`VBS_VP_CHUNK_SIZE_BYTES`]
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout)]
pub struct VpGpaPageChunk {
    pub header: VbsChunkHeader,
    pub metadata: u64,
    pub page_number: u64,
}

open_enum! {
#[derive(IntoBytes, Immutable, KnownLayout)]
pub enum BootMeasurementType: u32 {
    VP_REGISTER = 0,
    VP_VTL_ENABLED = 1,
    VP_GPA_PAGE = 2,
}
}

/// Flags indicating read and write acceptance of a GPA Page and whether it is
/// to be measured in the digest
#[bitfield(u64)]
pub struct VBS_VM_GPA_PAGE_BOOT_METADATA {
    #[bits(2)]
    pub acceptance: u64,
    #[bits(1)]
    pub data_unmeasured: bool,
    #[bits(61)]
    reserved: u64,
}

/// Flags defining the security policy for the guest
#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout)]
pub struct VBS_POLICY_FLAGS {
    /// Guest supports debugging
    #[bits(1)]
    pub debug: bool,
    #[bits(31)]
    reserved: u32,
}
pub const VM_GPA_PAGE_READABLE: u64 = 0x1;
pub const VM_GPA_PAGE_WRITABLE: u64 = 0x2;
