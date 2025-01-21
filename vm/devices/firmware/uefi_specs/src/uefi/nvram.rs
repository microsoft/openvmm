// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! UEFI Nvram Variable Services

use crate::uefi::signing::WIN_CERTIFICATE_UEFI_GUID;
use crate::uefi::time::EFI_TIME;
use bitfield_struct::bitfield;
use guid::Guid;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// UEFI spec 8.2 - Variable Services
#[bitfield(u32)]
#[derive(Eq, PartialEq)]
pub struct EfiVariableAttributes {
    pub non_volatile: bool,
    pub bootservice_access: bool,
    pub runtime_access: bool,
    pub hardware_error_record: bool,
    pub authenticated_write_access: bool,
    pub time_based_authenticated_write_access: bool,
    pub append_write: bool,
    pub enhanced_authenticated_access: bool,

    #[bits(24)]
    _reserved: u32,
}

impl EfiVariableAttributes {
    pub const DEFAULT_ATTRIBUTES: EfiVariableAttributes = EfiVariableAttributes::new()
        .with_non_volatile(true)
        .with_bootservice_access(true)
        .with_runtime_access(true);
    pub const DEFAULT_ATTRIBUTES_VOLATILE: EfiVariableAttributes = EfiVariableAttributes::new()
        .with_bootservice_access(true)
        .with_runtime_access(true);
    pub const DEFAULT_ATTRIBUTES_TIME_BASED_AUTH: EfiVariableAttributes =
        Self::DEFAULT_ATTRIBUTES.with_time_based_authenticated_write_access(true);

    pub fn contains_unsupported_bits(&self) -> bool {
        u32::from(*self)
            & !u32::from(
                Self::new()
                    .with_non_volatile(true)
                    .with_bootservice_access(true)
                    .with_runtime_access(true)
                    .with_hardware_error_record(true)
                    .with_authenticated_write_access(true)
                    .with_time_based_authenticated_write_access(true)
                    .with_append_write(true)
                    .with_enhanced_authenticated_access(true),
            )
            != 0
    }
}

/// UEFI spec 8.2
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct EFI_VARIABLE_AUTHENTICATION_2 {
    /// Components Pad1, Nanosecond, TimeZone, Daylight and Pad2 shall be set to
    /// 0. This means that the time shall always be expressed in GMT.
    pub timestamp: EFI_TIME,
    /// Provides the authorization for the variable access. Only a CertType of
    /// EFI_CERT_TYPE_PKCS7_GUID is accepted.
    pub auth_info: WIN_CERTIFICATE_UEFI_GUID,
}

impl EFI_VARIABLE_AUTHENTICATION_2 {
    /// A "dummy" header that doesn't actually include a valid cert.
    ///
    /// This header is used during parts of the pre-boot setup to inject
    /// `TIME_BASED_AUTHENTICATED_WRITE_ACCESS` from within the UEFI device
    /// itself.
    pub const DUMMY: Self = {
        use crate::uefi::signing::EFI_CERT_TYPE_PKCS7_GUID;
        use crate::uefi::signing::WIN_CERTIFICATE;
        use crate::uefi::signing::WIN_CERT_TYPE_EFI_GUID;

        EFI_VARIABLE_AUTHENTICATION_2 {
            timestamp: EFI_TIME::ZEROED,
            auth_info: WIN_CERTIFICATE_UEFI_GUID {
                header: WIN_CERTIFICATE {
                    // `length` includes both the header itself, and the cert
                    // payload (which has len 0 in the dummy header)
                    length: size_of::<WIN_CERTIFICATE_UEFI_GUID>() as u32,
                    revision: 0x0200,
                    certificate_type: WIN_CERT_TYPE_EFI_GUID,
                },
                cert_type: EFI_CERT_TYPE_PKCS7_GUID,
            },
        }
    };
}

/// UEFI spec 32.4.1
pub mod signature_list {
    use guid::Guid;
    use zerocopy::FromBytes;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    #[derive(Debug, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
    #[repr(C)]
    pub struct EFI_SIGNATURE_LIST {
        /// Type of the signature. GUID signature types are defined in "Related
        /// Definitions" below.
        pub signature_type: Guid,
        /// Total size of the signature list, including this header.
        pub signature_list_size: u32,
        /// Size of the signature header which precedes the array of signatures.
        ///
        /// > NOTE: a careful reading of the UEFI spec uncovers that this field
        /// > is _always_ zero. Why? Excellent question.
        pub signature_header_size: u32,
        /// Size of each signature. Must be at least the size of EFI_SIGNATURE_DATA.
        pub signature_size: u32,
        // Header before the array of signatures. The format of this header is
        // specified by the SignatureType.
        //
        // > NOTE: because SignatureHeaderSize is always zero, this array is
        // > always zero sized...
        //
        // UINT8 SignatureHeader[SignatureHeaderSize];
        //
        // An array of signatures. Each signature is SignatureSize bytes in
        // length. The format of the signature is defined by the SignatureType.
        //
        // EFI_SIGNATURE_DATA Signatures[…][SignatureSize];
    }

    #[derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        IntoBytes,
        FromBytes,
        Immutable,
        KnownLayout,
    )]
    #[repr(C)]
    pub struct EFI_SIGNATURE_DATA {
        /// An identifier which identifies the agent which added the signature to
        /// the list.
        pub signature_owner: Guid,
        // UINT8 SignatureData[…];
    }

    pub const EFI_CERT_SHA256_GUID: Guid =
        Guid::from_static_str("c1c41626-504c-4092-aca9-41f936934328");

    pub const EFI_CERT_X509_GUID: Guid =
        Guid::from_static_str("a5c059a1-94e4-4aa7-87b5-ab155c2bf072");
}

/// Check if the specified variable is a secure boot policy variable, as
/// specified by the UEFI spec in section 3.3 Globally Defined Variables, under
/// the details of `SetupMode`.
pub fn is_secure_boot_policy_var(vendor: Guid, name: &ucs2::Ucs2LeSlice) -> bool {
    let secure_boot_policy_vars = [
        vars::PK(),
        vars::KEK(),
        // TODO: add OsRecoveryOrder, OsRecovery####
    ];

    let is_secure_boot_policy_var = secure_boot_policy_vars
        .into_iter()
        .any(|v| v == (vendor, name));

    is_secure_boot_policy_var || vendor == vars::IMAGE_SECURITY_DATABASE_GUID
}

/// UEFI spec 3.3 - Table 3-1
///
/// Due to the Rust compiler not having built-in support for defining
/// wide-string literals, and me not wanting to yak-shave a proc macro
/// implementation that emits valid Utf16LeSlices at compile time, these
/// "constants" are actually methods that can only be called at runtime.
#[allow(dead_code)] // no live code - just a bunch of constants
pub mod vars {
    use guid::Guid;

    /// UEFI spec 3.3 - Globally Defined Variables
    pub const EFI_GLOBAL_VARIABLE: Guid =
        Guid::from_static_str("8BE4DF61-93CA-11D2-AA0D-00E098032B8C");

    /// UEFI spec 32.6.1 - UEFI Image Variable GUID & Variable Name
    pub const IMAGE_SECURITY_DATABASE_GUID: Guid =
        Guid::from_static_str("d719b2cb-3d3a-4596-a3bc-dad00e67656f");

    defn_nvram_var!(SECURE_BOOT = (EFI_GLOBAL_VARIABLE, "SecureBoot"));
    defn_nvram_var!(SETUP_MODE = (EFI_GLOBAL_VARIABLE, "SetupMode"));

    defn_nvram_var!(PK = (EFI_GLOBAL_VARIABLE, "PK"));
    defn_nvram_var!(KEK = (EFI_GLOBAL_VARIABLE, "KEK"));
    defn_nvram_var!(DBDEFAULT = (EFI_GLOBAL_VARIABLE, "dbDefault"));

    defn_nvram_var!(DB = (IMAGE_SECURITY_DATABASE_GUID, "db"));
    defn_nvram_var!(DBX = (IMAGE_SECURITY_DATABASE_GUID, "dbx"));
}
