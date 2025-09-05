// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VBS (Virtualization-Based Security) attestation report structures.

use bitfield_struct::bitfield;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Size of the [`VbsReport`].
pub const VBS_REPORT_SIZE: usize = 0x230;

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VbsReportPackageHeader {
    /// Total size of the VBS report package, including this header.
    pub package_size: u32,
    /// Version of the VBS report package format.
    pub version: u32,
    /// Signature scheme used for the report.
    pub signature_scheme: u32,
    /// Size of the signature in bytes.
    pub signature_size: u32,
    /// Reserved for future use.
    pub _reserved: u32,
}

/// VBS VM identity structure.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VbsVmIdentity {
    /// Owner ID of the VM.
    pub owner_id: [u8; 32],
    /// Measurement of the VM.
    pub measurement: [u8; 32],
    /// Signer of the VM.
    pub signer: [u8; 32],
    /// Host-specific data.
    pub host_data: [u8; 32],
    /// Enabled Virtual Trust Levels bitmap.
    pub enabled_vtl: VtlBitMap,
    /// Security policy attributes.
    pub policy: SecurityAttributes,
    /// Guest Virtual Trust Level.
    pub guest_vtl: u32,
    /// Guest Security Version Number.
    pub guest_svn: u32,
    /// Guest Product ID.
    pub guest_product_id: u32,
    /// Guest Module ID.
    pub guest_module_id: u32,
    /// Reserved for future use.
    pub _reserved: [u8; 64],
}

/// VBS report structure.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VbsReport {
    /// Package header containing metadata about the report.
    pub header: VbsReportPackageHeader,
    /// Version of the VBS report.
    pub version: u32,
    /// Report data that is provided at the runtime.
    pub report_data: [u8; 64],
    /// Identity information of the VM.
    pub identity: VbsVmIdentity,
    /// Signature of the report.
    pub signature: [u8; 256],
}

static_assertions::const_assert_eq!(VBS_REPORT_SIZE, size_of::<VbsReport>());

/// Virtual Trust Level bitmap.
#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct VtlBitMap {
    /// Indicates if Virtual Trust Level 0 is enabled.
    pub vtl0: bool,
    /// Indicates if Virtual Trust Level 1 is enabled.
    pub vtl1: bool,
    /// Indicates if Virtual Trust Level 2 is enabled.
    pub vtl2: bool,
    #[bits(29)]
    pub _reserved: u32,
}

/// Security attributes for the VM.
#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct SecurityAttributes {
    /// Indicates if debugging is allowed on the VM.
    pub debug_allowed: bool,
    #[bits(31)]
    pub _reserved: u32,
}
