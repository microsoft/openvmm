// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared SMBIOS (DMI) identity configuration.
//!
//! These types describe the SMBIOS identity overrides a host can apply to a
//! guest. They live in their own crate so that both the OpenVMM configuration
//! (`openvmm_defs`) and the Guest Emulation Transport resources
//! (`get_resources`) can share a single representation rather than each
//! defining a near-duplicate type.
//!
//! Each string field is an override: `None` means "use the loader/firmware's
//! built-in default identity". The default string literals themselves live with
//! the code that builds the tables (e.g. the OpenVMM and OpenHCL direct
//! loaders), not here, so the defaults are defined in a single place.

use guid::Guid;
use mesh::MeshPayload;

/// SMBIOS (DMI) identity configuration overrides.
///
/// Grouped by SMBIOS structure type to mirror the borrowed table view in the
/// loader: one sub-struct per type.
#[derive(Debug, Clone, Default, MeshPayload)]
pub struct SmbiosConfig {
    /// Type 0 (BIOS Information) overrides.
    pub bios: SmbiosBiosOverrides,
    /// Type 1 (System Information) overrides.
    pub system: SmbiosSystemOverrides,
}

/// SMBIOS Type 0 (BIOS Information) overrides. `None` = use the loader default.
#[derive(Debug, Clone, Default, MeshPayload)]
pub struct SmbiosBiosOverrides {
    /// BIOS vendor string.
    pub vendor: Option<String>,
    /// BIOS version string.
    pub version: Option<String>,
    /// BIOS release date string.
    pub release_date: Option<String>,
    /// System BIOS Major/Minor Release (`release=MAJOR.MINOR`).
    pub release: Option<(u8, u8)>,
}

/// SMBIOS Type 1 (System Information) overrides. `None` string fields use the
/// loader default; `uuid` is concrete (generated per-VM at runtime, `Guid::ZERO`
/// by default).
#[derive(Debug, Clone, Default, MeshPayload)]
pub struct SmbiosSystemOverrides {
    /// System manufacturer string.
    pub manufacturer: Option<String>,
    /// System product name string.
    pub product_name: Option<String>,
    /// System version string.
    pub version: Option<String>,
    /// System serial number string.
    pub serial_number: Option<String>,
    /// System SKU number string.
    pub sku_number: Option<String>,
    /// System family string.
    pub family: Option<String>,
    /// System UUID (the VM's BIOS GUID). Stored as raw mixed-endian EFI GUID
    /// bytes when delivered to the guest.
    pub uuid: Guid,
}
