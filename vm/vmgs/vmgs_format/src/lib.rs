// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VMGS format definitions

#![expect(missing_docs)]
#![forbid(unsafe_code)]
#![no_std]

extern crate alloc;

use alloc::string::String;
use bitfield_struct::bitfield;
use core::fmt::Display;
use core::ops::Index;
use core::ops::IndexMut;
#[cfg(feature = "inspect")]
use inspect::Inspect;
use open_enum::open_enum;
use serde::Deserialize;
use serde::Serialize;
use static_assertions::const_assert;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// The suggested default capacity of a VMGS disk in bytes, 4MB.
///
/// In some sense, this is not part of the VMGS format, but all known
/// implementations default to this capacity (with an optional user-provided
/// override), so it is useful to have it here. But an implementation is not
/// _required_ to use this capacity, and the VMGS parser cannot assume that the
/// disk is this size.
pub const VMGS_DEFAULT_CAPACITY: u64 = 0x400000;

open_enum! {
    /// VMGS fixed file IDs
    #[cfg_attr(feature = "inspect", derive(Inspect))]
    #[cfg_attr(feature = "inspect", inspect(debug))]
    pub enum FileId: u32 {
        FILE_TABLE     = 0,

        BIOS_NVRAM     = 1,
        TPM_PPI        = 2,
        TPM_NVRAM      = 3,
        RTC_SKEW       = 4,
        ATTEST         = 5,
        KEY_PROTECTOR  = 6,
        VM_UNIQUE_ID   = 7,
        GUEST_FIRMWARE = 8,
        CUSTOM_UEFI    = 9,
        GUEST_WATCHDOG = 10,
        HW_KEY_PROTECTOR = 11,
        GUEST_SECRET_KEY = 13,
        HIBERNATION_FIRMWARE = 14,
        PLATFORM_SEED = 15,
        PROVENANCE_DOC = 16,
        TPM_NVRAM_BACKUP = 17,
        PROVISIONING_MARKER = 18,

        EXTENDED_FILE_TABLE = 63,
    }
}

impl Display for FileId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_fmt(format_args!("File ID {} ({:?})", self.0, self))
    }
}

pub const VMGS_VERSION_2_0: u32 = 0x00020000;
pub const VMGS_VERSION_3_0: u32 = 0x00030000;

pub const VMGS_SIGNATURE: u64 = u64::from_le_bytes(*b"GUESTRTS"); // identical to the V1 format signature

pub const VMGS_BYTES_PER_BLOCK: u32 = 4096;

const VMGS_MAX_CAPACITY_BLOCKS: u64 = 0x100000000;
pub const VMGS_MAX_CAPACITY_BYTES: u64 = VMGS_MAX_CAPACITY_BLOCKS * VMGS_BYTES_PER_BLOCK as u64;

pub const VMGS_MIN_FILE_BLOCK_OFFSET: u32 = 2;
pub const VMGS_FILE_COUNT: usize = 64;
pub const VMGS_MAX_FILE_SIZE_BLOCKS: u64 = 0xFFFFFFFF;
pub const VMGS_MAX_FILE_SIZE_BYTES: u64 = VMGS_MAX_FILE_SIZE_BLOCKS * VMGS_BYTES_PER_BLOCK as u64;

pub const VMGS_NONCE_SIZE: usize = 12; // Each nonce includes a 4-byte random seed and a 8-byte counter.
pub const VMGS_NONCE_RANDOM_SEED_SIZE: usize = 4;
pub const VMGS_AUTHENTICATION_TAG_SIZE: usize = 16;
pub const VMGS_ENCRYPTION_KEY_SIZE: usize = 32;

pub type VmgsNonce = [u8; VMGS_NONCE_SIZE];
pub type VmgsAuthTag = [u8; VMGS_AUTHENTICATION_TAG_SIZE];
pub type VmgsDatastoreKey = [u8; VMGS_ENCRYPTION_KEY_SIZE];

#[repr(C)]
#[derive(Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VmgsFileEntry {
    // V2 fields
    pub offset: u32,
    pub allocation_size: u32,
    pub valid_data_size: u64,

    // V3 fields
    pub nonce: VmgsNonce,
    pub authentication_tag: VmgsAuthTag,

    // Store a copy of the extended attributes here so we can check if a FileId
    // is encrypted without unlocking the VMGS.
    pub attributes: FileAttribute,

    pub reserved: [u8; 16],
}

const_assert!(size_of::<VmgsFileEntry>() == 64);

impl Index<FileId> for [VmgsFileEntry] {
    type Output = VmgsFileEntry;

    fn index(&self, file_id: FileId) -> &Self::Output {
        &self[file_id.0 as usize]
    }
}

impl IndexMut<FileId> for [VmgsFileEntry] {
    fn index_mut(&mut self, file_id: FileId) -> &mut Self::Output {
        &mut self[file_id.0 as usize]
    }
}

#[repr(C)]
#[derive(Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VmgsExtendedFileEntry {
    pub attributes: FileAttribute,
    pub encryption_key: VmgsDatastoreKey,

    pub reserved: [u8; 28],
}

const_assert!(size_of::<VmgsExtendedFileEntry>() == 64);

impl Index<FileId> for [VmgsExtendedFileEntry] {
    type Output = VmgsExtendedFileEntry;

    fn index(&self, file_id: FileId) -> &Self::Output {
        &self[file_id.0 as usize]
    }
}

impl IndexMut<FileId> for [VmgsExtendedFileEntry] {
    fn index_mut(&mut self, file_id: FileId) -> &mut Self::Output {
        &mut self[file_id.0 as usize]
    }
}

#[repr(C)]
#[derive(Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[cfg_attr(feature = "inspect", derive(Inspect))]
pub struct VmgsEncryptionKey {
    pub nonce: VmgsNonce,
    pub reserved: u32,
    pub authentication_tag: VmgsAuthTag,
    pub encryption_key: VmgsDatastoreKey,
}

const_assert!(size_of::<VmgsEncryptionKey>() == 64);

#[repr(C)]
#[derive(Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VmgsHeader {
    // V1 compatible fields
    pub signature: u64,
    pub version: u32,
    pub checksum: u32,
    pub sequence: u32,
    pub header_size: u32,

    // V2 fields
    pub file_table_offset: u32,
    pub file_table_size: u32,

    // V3 fields
    pub encryption_algorithm: EncryptionAlgorithm,
    pub markers: VmgsMarkers,
    pub metadata_keys: [VmgsEncryptionKey; 2],
    pub reserved_1: u32,
}

const_assert!(size_of::<VmgsHeader>() == 168);

#[repr(C)]
#[derive(Clone, IntoBytes, Immutable, KnownLayout, FromBytes, Debug)]
pub struct VmgsFileTable {
    pub entries: [VmgsFileEntry; VMGS_FILE_COUNT],
}

const_assert!(size_of::<VmgsFileTable>() == 4096);
const_assert!((size_of::<VmgsFileTable>() as u32).is_multiple_of(VMGS_BYTES_PER_BLOCK));
pub const VMGS_FILE_TABLE_BLOCK_SIZE: u32 =
    size_of::<VmgsFileTable>() as u32 / VMGS_BYTES_PER_BLOCK;

#[repr(C)]
#[derive(Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VmgsExtendedFileTable {
    pub entries: [VmgsExtendedFileEntry; VMGS_FILE_COUNT],
}

const_assert!(size_of::<VmgsExtendedFileTable>() == 4096);
const_assert!((size_of::<VmgsExtendedFileTable>() as u32).is_multiple_of(VMGS_BYTES_PER_BLOCK));
pub const VMGS_EXTENDED_FILE_TABLE_BLOCK_SIZE: u32 =
    size_of::<VmgsExtendedFileTable>() as u32 / VMGS_BYTES_PER_BLOCK;

/// File attribute for VMGS files
#[cfg_attr(feature = "inspect", derive(Inspect))]
#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct FileAttribute {
    pub encrypted: bool,
    pub authenticated: bool,
    #[bits(30)]
    _reserved: u32,
}

open_enum! {
    /// Encryption algorithm used to encrypt VMGS file
    #[cfg_attr(feature = "inspect", derive(Inspect))]
    #[cfg_attr(feature = "inspect", inspect(debug))]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum EncryptionAlgorithm: u16 {
        /// No encryption algorithm
        NONE = 0,
        /// AES 256 GCM encryption
        AES_GCM = 1,
    }
}

/// Markers used internally to indicate how the VMGS should be treated
#[cfg_attr(feature = "inspect", derive(Inspect))]
#[bitfield(u16)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct VmgsMarkers {
    pub reprovisioned: bool,
    #[bits(15)]
    _reserved: u16,
}

/// Entities that can provision a VMGS file.
#[cfg_attr(feature = "inspect", derive(Inspect))]
#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VmgsProvisioner {
    Unknown,
    Hcl,
    OpenHcl,
    CpsVmgstoolCvm,
    HaVmgstoolTvm,
    HclPostProvisioning,
}

/// Reasons that OpenHCL will provision a VMGS file.
#[cfg_attr(feature = "inspect", derive(Inspect))]
#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VmgsProvisioningReason {
    /// VMGS file was empty.
    Empty,
    /// VMGS file was corrupt or OpenHCL failed to read it.
    Failure,
    /// Host requested that OpenHCL reprovision the VMGS.
    Request,
    /// Unknown reason.
    Unknown,
}

/// Diagnostic marker that contains information about the VMGS's provisioning.
/// This marker is written once when a VMGS file is created, leaving a trace of
/// where and how it originated (e.g., that it was created by OpenHCL). Adding
/// new fields is safe, as it is not read by OpenHCL for any behavioral purpose.
#[cfg_attr(feature = "inspect", derive(Inspect))]
#[derive(Debug, Serialize, Deserialize)]
pub struct VmgsProvisioningMarker {
    pub provisioner: VmgsProvisioner,
    pub reason: VmgsProvisioningReason,
    pub tpm_version: String,
    pub tpm_nvram_size: usize,
    pub akcert_size: usize,
    pub akcert_attrs: String,
    pub provisioner_version: String,
}
