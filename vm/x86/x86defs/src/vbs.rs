// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

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
    pub package_size: u32,

    pub version: u32,

    pub signature_scheme: u32,

    pub signature_size: u32,

    pub _reserved: u32,
}

/// VBS VM identity structure.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VbsVmIdentity {
    pub onwer_id: [u8; 32],

    pub measurement: [u8; 32],

    pub signer: [u8; 32],

    pub host_data: [u8; 32],

    pub enabled_vtl: VtlBitMap,

    pub policy: SecurityAttributes,

    pub guest_vtl: u32,

    pub guest_svn: u32,

    pub guest_product_id: u32,

    pub guest_module_id: u32,

    pub _reserved: [u8; 64],
}

/// VBS report structure.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VbsReport {
    pub header: VbsReportPackageHeader,

    pub version: u32,

    pub report_data: [u8; 64],

    pub identity: VbsVmIdentity,

    pub signature: [u8; 256],
}

static_assertions::const_assert_eq!(VBS_REPORT_SIZE, size_of::<VbsReport>());

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct VtlBitMap {
    pub vtl0: bool,
    pub vtl1: bool,
    pub vtl2: bool,
    #[bits(29)]
    pub _reserved: u32,
}

#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct SecurityAttributes {
    pub debug_allowed: bool,
    #[bits(31)]
    pub _reserved: u32,
}
