// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The schema defined in this file must match the one defined in
//! `onecore/vm/schema/mars/Config/Config.Devices.Chipset.mars`.

use bitfield_struct::bitfield;
use guid::Guid;
use open_enum::open_enum;
use serde::Deserialize;
use serde::Serialize;

/// A type-alias to mark fields as _temporarily_ optional to preserve
/// build-to-compat compatibility during internal testing.
///
/// i.e: a newly added field should be marked as `DevLoopCompatOption` until
/// we're sure that all hosts that we expect this new underhill version to run
/// on are updated to send the new field.
///
/// It would be **very bad form** to ship a library/binary that includes
/// `DevLoopCompatOption` fields!
pub type DevLoopCompatOption<T> = Option<T>;

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct DevicePlatformSettingsV2Json {
    pub v1: HclDevicePlatformSettings,
    pub v2: HclDevicePlatformSettingsV2,
}

// The legacy DPS response's mars schema specifies all fields as [OmitEmpty],
// which we handle by setting `serde(default)` at the struct level.
//
// This is _not_ the case in the newer DPS packet, whereby all fields must be
// present, specifying "empty values" if the data is not set.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct HclDevicePlatformSettings {
    pub secure_boot_enabled: bool,
    pub secure_boot_template_id: HclSecureBootTemplateId,
    pub enable_battery: bool,
    pub enable_processor_idle: bool,
    pub enable_tpm: bool,
    pub com1: HclUartSettings,
    pub com2: HclUartSettings,
    #[serde(with = "serde_helpers::as_string")]
    pub bios_guid: Guid,
    pub console_mode: u8,
    pub enable_firmware_debugging: bool,
    pub enable_hibernation: bool,
    pub serial_number: String,
    pub base_board_serial_number: String,
    pub chassis_serial_number: String,
    pub chassis_asset_tag: String,
}

// requires a `Default` derive, due to [OmitEmpty] used in parent struct
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum HclSecureBootTemplateId {
    #[serde(rename = "None")]
    #[default]
    None,
    #[serde(rename = "MicrosoftWindows")]
    MicrosoftWindows,
    #[serde(rename = "MicrosoftUEFICertificateAuthority")]
    MicrosoftUEFICertificateAuthority,
}

// requires a `Default` derive, due to [OmitEmpty] used in parent struct
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct HclUartSettings {
    pub enable_port: bool,
    pub debugger_mode: bool,
    pub enable_vmbus_redirector: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct HclDevicePlatformSettingsV2 {
    pub r#static: HclDevicePlatformSettingsV2Static,
    pub dynamic: HclDevicePlatformSettingsV2Dynamic,
}

/// Boot device order entry used by the PCAT Bios.
#[derive(Debug, Copy, Clone, Deserialize, Serialize)]
pub enum PcatBootDevice {
    Floppy,
    Optical,
    HardDrive,
    Network,
}

/// Guest state lifetime
#[derive(Debug, Copy, Clone, Deserialize, Serialize, Default)]
pub enum GuestStateLifetime {
    #[default]
    Default,
    ReprovisionOnFailure,
    Reprovision,
    Ephemeral,
}

/// Guest state encryption policy
#[derive(Debug, Copy, Clone, Deserialize, Serialize, Default)]
pub enum GuestStateEncryptionPolicy {
    /// Use the best encryption available, allowing fallback.
    ///
    /// VMs will be created using the best encryption available,
    /// attempting GspKey, then GspById, and finally leaving the data
    /// unencrypted if neither are available. VMs will not be migrated
    /// to a different encryption method.
    #[default]
    Auto,
    /// Prefer (or require, if strict) no encryption.
    ///
    /// Do not encrypt the guest state unless it is already encrypted and
    /// strict encryption policy is disabled.
    None,
    /// Prefer (or require, if strict) GspById.
    ///
    /// This prevents a VM from being created as or migrated to GspKey even
    /// if it is available. Exisiting GspKey encryption will be used unless
    /// strict encryption policy is enabled. Fails if the data cannot be
    /// encrypted.
    GspById,
    /// Prefer (or require, if strict) GspKey.
    ///
    /// VMs will be created as or migrated to GspKey. GspById encryption will
    /// be used if GspKey is unavailable unless strict encryption policy is
    /// enabled. Fails if the data cannot be encrypted.
    GspKey,
    /// Use hardware sealing
    // TODO: update this doc comment once hardware sealing is implemented
    HardwareSealing,
}

open_enum! {
    /// EFI Diagnostics Log Level Filter
    #[derive(Default, Deserialize, Serialize)]
    pub enum EfiDiagnosticsLogLevelType: u32 {
        /// Default log level
        DEFAULT = 0,
        /// Include INFO logs
        INFO = 1,
        /// All logs
        FULL = 2,
    }
}

/// Management VTL Feature Flags
#[bitfield(u64)]
#[derive(Deserialize, Serialize)]
#[serde(transparent)]
pub struct ManagementVtlFeatures {
    pub strict_encryption_policy: bool,
    pub _reserved1: bool,
    pub control_ak_cert_provisioning: bool,
    pub attempt_ak_cert_callback: bool,
    pub tx_only_serial_port: bool,
    #[bits(59)]
    pub _reserved2: u64,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct HclDevicePlatformSettingsV2Static {
    //UEFI flags
    pub legacy_memory_map: bool,
    pub pause_after_boot_failure: bool,
    pub pxe_ip_v6: bool,
    pub measure_additional_pcrs: bool,
    pub disable_frontpage: bool,
    pub disable_sha384_pcr: bool,
    pub media_present_enabled_by_default: bool,
    pub memory_protection_mode: u8,
    #[serde(default)]
    pub default_boot_always_attempt: bool,

    // UEFI info
    pub vpci_boot_enabled: bool,
    #[serde(default)]
    #[serde(with = "serde_helpers::opt_guid_str")]
    pub vpci_instance_filter: Option<Guid>,

    // PCAT info
    pub num_lock_enabled: bool,
    pub pcat_boot_device_order: Option<[PcatBootDevice; 4]>,

    pub smbios: HclDevicePlatformSettingsV2StaticSmbios,

    // Per field serde(default) is required here because that
    // we can't reply on serde's normal behavior for optional
    // fields (put None if not present in json) because we're
    // using custom serialize/deserialize methods
    #[serde(default)]
    #[serde(with = "serde_helpers::opt_base64_vec")]
    pub vtl2_settings: Option<Vec<u8>>,

    pub vmbus_redirection_enabled: bool,
    pub no_persistent_secrets: bool,
    pub watchdog_enabled: bool,
    // this `#[serde(default)]` shouldn't have been necessary, but we let a
    // `[OmitEmpty]` marker slip past in code review...
    #[serde(default)]
    pub firmware_mode_is_pcat: bool,
    #[serde(default)]
    pub always_relay_host_mmio: bool,
    #[serde(default)]
    pub imc_enabled: bool,
    #[serde(default)]
    pub cxl_memory_enabled: bool,
    #[serde(default)]
    pub guest_state_lifetime: GuestStateLifetime,
    #[serde(default)]
    pub guest_state_encryption_policy: GuestStateEncryptionPolicy,
    #[serde(default)]
    pub efi_diagnostics_log_level: EfiDiagnosticsLogLevelType,
    #[serde(default)]
    pub management_vtl_features: ManagementVtlFeatures,
    #[serde(default)]
    pub hv_sint_enabled: bool,
    #[serde(default)]
    pub azi_hsm_enabled: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct HclDevicePlatformSettingsV2StaticSmbios {
    pub system_manufacturer: String,
    pub system_product_name: String,
    pub system_version: String,
    #[serde(rename = "SystemSKUNumber")]
    pub system_sku_number: String,
    pub system_family: String,
    pub bios_lock_string: String,
    pub memory_device_serial_number: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct HclDevicePlatformSettingsV2Dynamic {
    pub nvdimm_count: u16,
    pub enable_psp: bool,
    pub generation_id_low: u64,
    pub generation_id_high: u64,
    pub smbios: HclDevicePlatformSettingsV2DynamicSmbios,
    pub is_servicing_scenario: bool,

    #[serde(default)]
    #[serde(with = "serde_helpers::vec_base64_vec")]
    pub acpi_tables: Vec<Vec<u8>>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct HclDevicePlatformSettingsV2DynamicSmbios {
    #[serde(with = "serde_helpers::base64_vec")]
    pub processor_manufacturer: Vec<u8>,
    #[serde(with = "serde_helpers::base64_vec")]
    pub processor_version: Vec<u8>,

    #[serde(rename = "ProcessorID")]
    pub processor_id: u64,
    pub external_clock: u16,
    pub max_speed: u16,
    pub current_speed: u16,
    pub processor_characteristics: u16,
    pub processor_family2: u16,
    pub processor_type: u8,
    pub voltage: u8,
    pub status: u8,
    pub processor_upgrade: u8,
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn smoke_test_sample() {
        serde_json::from_slice::<DevicePlatformSettingsV2Json>(include_bytes!(
            "dps_test_json.json"
        ))
        .unwrap();
    }

    #[test]
    fn smoke_test_sample_with_vtl2settings() {
        serde_json::from_slice::<DevicePlatformSettingsV2Json>(include_bytes!(
            "dps_test_json_with_vtl2settings.json"
        ))
        .unwrap();
    }
}
