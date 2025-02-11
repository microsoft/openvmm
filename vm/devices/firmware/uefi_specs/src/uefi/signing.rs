// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use guid::Guid;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// UEFI spec 32.2.4
///
/// This structure is the certificate header.
/// There may be zero or more certificates.
#[derive(Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct WIN_CERTIFICATE {
    /// The length of the entire certificate, including the length of the header,
    /// in bytes
    pub length: u32,
    /// The revision level of the WIN_CERTIFICATE structure.
    /// The current revision level is 0x0200
    pub revision: u16,
    /// The certificate type. See WIN_CERT_TYPE_xxx for the UEFI certificate
    /// types. The UEFI specification reserves the range of certificate type
    /// values from 0x0EF0 to 0x0EFF.
    pub certificate_type: u16,
    // The actual certificate. The format of the certificate depends on
    // certificate_type.
    //
    // UINT8 bCertificate[ANYSIZE_ARRAY];
}

/// UEFI spec 32.2.4 - WIN_CERTIFICATE_UEFI_GUID
#[derive(Debug, IntoBytes, FromBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct WIN_CERTIFICATE_UEFI_GUID {
    /// certificate_type is set to WIN_CERT_TYPE_EFI_GUID.
    pub header: WIN_CERTIFICATE,
    /// This is the unique id which determines the format of the CertData.
    pub cert_type: Guid,
    // This is the certificate data. The format of the data is determined by the
    // CertType.
    //
    // UINT8 CertData[ANYSIZE_ARRAY];
}

/// UEFI spec 32.2.4 - WIN_CERTIFICATE_UEFI_GUID
pub const EFI_CERT_TYPE_PKCS7_GUID: Guid =
    Guid::from_static_str("4aafd29d-68df-49ee-8aa9-347d375665a7");

// UEFI spec 32.2.4 - WIN_CERTIFICATE

// pub const WIN_CERT_TYPE_PKCS_SIGNED_DATA: u16 = 0x0002;
// pub const WIN_CERT_TYPE_EFI_PKCS115: u16 = 0x0EF0;//
pub const WIN_CERT_TYPE_EFI_GUID: u16 = 0x0EF1;
