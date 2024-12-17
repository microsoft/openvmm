// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The schema defined in this file must match the one defined in
//! `onecore/vm/schema/mars/Config/Config.Devices.Chipset.mars`.

use guid::Guid;
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

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct HclDevicePlatformSettingsV2Static {
    pub legacy_memory_map: bool,
    pub pause_after_boot_failure: bool,
    pub pxe_ip_v6: bool,
    pub measure_additional_pcrs: bool,
    pub disable_frontpage: bool,
    pub disable_sha384_pcr: bool,
    pub media_present_enabled_by_default: bool,
    pub memory_protection_mode: u8,

    pub vpci_boot_enabled: bool,
    #[serde(default)]
    #[serde(with = "serde_helpers::opt_guid_str")]
    pub vpci_instance_filter: Option<Guid>,

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
