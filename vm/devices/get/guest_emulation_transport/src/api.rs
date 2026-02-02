// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Friendly Rust representations of the data sent over the GET.

// These types are re-exported as-is, in order to avoid requiring consumers of
// the GET and GED to also import get_protocol.
pub use get_protocol::CreateRamGpaRangeFlags;
pub use get_protocol::EventLogId;
pub use get_protocol::GSP_CIPHERTEXT_MAX;
pub use get_protocol::GspCiphertextContent;
pub use get_protocol::GspCleartextContent;
pub use get_protocol::GspExtendedStatusFlags;
pub use get_protocol::IGVM_ATTEST_MSG_REQ_AGENT_DATA_MAX_SIZE;
pub use get_protocol::MAX_TRANSFER_SIZE;
pub use get_protocol::NUMBER_GSP;
pub use get_protocol::ProtocolVersion;
pub use get_protocol::SaveGuestVtl2StateFlags;
pub use get_protocol::VmgsIoStatus;

use guid::Guid;
use mesh::MeshPayload;
use std::time::Duration;
use zerocopy::FromZeros;

/// Device platform settings.
#[expect(missing_docs)]
pub mod platform_settings {
    pub use get_protocol::dps_json::PcatBootDevice;

    use get_protocol::dps_json::EfiDiagnosticsLogLevelType;
    use get_protocol::dps_json::GuestStateEncryptionPolicy;
    use get_protocol::dps_json::GuestStateLifetime;
    use get_protocol::dps_json::ManagementVtlFeatures;
    use guid::Guid;
    use inspect::Inspect;

    /// All available device platform settings.
    #[derive(Debug, Inspect)]
    pub struct DevicePlatformSettings {
        pub smbios: Smbios,
        pub general: General,
        #[inspect(with = "inspect::iter_by_index")]
        pub acpi_tables: Vec<Vec<u8>>,
    }

    /// All available SMBIOS related config.
    #[derive(Debug, Inspect)]
    pub struct Smbios {
        pub serial_number: Vec<u8>,
        pub base_board_serial_number: Vec<u8>,
        pub chassis_serial_number: Vec<u8>,
        pub chassis_asset_tag: Vec<u8>,

        pub system_manufacturer: Vec<u8>,
        pub system_product_name: Vec<u8>,
        pub system_version: Vec<u8>,
        pub system_sku_number: Vec<u8>,
        pub system_family: Vec<u8>,
        pub bios_lock_string: Vec<u8>,
        pub memory_device_serial_number: Vec<u8>,

        pub processor_manufacturer: Vec<u8>,
        pub processor_version: Vec<u8>,
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

    /// All available general device platform configuration.
    // DEVNOTE: "general" is code for "not well organized", so if you've got a
    // better way to organize these settings, do consider cleaning this up a bit!
    #[derive(Debug, Inspect)]
    pub struct General {
        pub secure_boot_enabled: bool,
        pub secure_boot_template: SecureBootTemplateType,
        pub bios_guid: Guid,
        pub console_mode: UefiConsoleMode,
        pub battery_enabled: bool,
        pub processor_idle_enabled: bool,
        pub tpm_enabled: bool,
        pub com1_enabled: bool,
        pub com1_debugger_mode: bool,
        pub com1_vmbus_redirector: bool,
        pub com2_enabled: bool,
        pub com2_debugger_mode: bool,
        pub com2_vmbus_redirector: bool,
        pub firmware_debugging_enabled: bool,
        pub hibernation_enabled: bool,

        pub suppress_attestation: Option<bool>,
        pub generation_id: Option<[u8; 16]>,

        pub legacy_memory_map: bool,
        pub pause_after_boot_failure: bool,
        pub pxe_ip_v6: bool,
        pub measure_additional_pcrs: bool,
        pub disable_frontpage: bool,
        pub disable_sha384_pcr: bool,
        pub media_present_enabled_by_default: bool,
        pub vpci_boot_enabled: bool,
        pub memory_protection_mode: MemoryProtectionMode,
        pub default_boot_always_attempt: bool,
        pub num_lock_enabled: bool,
        #[inspect(with = "|x| inspect::iter_by_index(x).map_value(inspect::AsDebug)")]
        pub pcat_boot_device_order: [PcatBootDevice; 4],

        pub vpci_instance_filter: Option<Guid>,
        pub nvdimm_count: u16,
        pub psp_enabled: bool,

        pub vmbus_redirection_enabled: bool,
        pub always_relay_host_mmio: bool,
        pub vtl2_settings: Option<underhill_config::Vtl2Settings>,

        pub is_servicing_scenario: bool,
        pub watchdog_enabled: bool,
        pub firmware_mode_is_pcat: bool,
        pub imc_enabled: bool,
        pub cxl_memory_enabled: bool,
        #[inspect(debug)]
        pub efi_diagnostics_log_level: EfiDiagnosticsLogLevelType,
        #[inspect(debug)]
        pub guest_state_lifetime: GuestStateLifetime,
        #[inspect(debug)]
        pub guest_state_encryption_policy: GuestStateEncryptionPolicy,
        #[inspect(debug)]
        pub management_vtl_features: ManagementVtlFeatures,
        pub hv_sint_enabled: bool,
    }

    #[derive(Copy, Clone, Debug, Inspect)]
    pub enum MemoryProtectionMode {
        Disabled = 0,
        Default = 1,
        Strict = 2,
        Relaxed = 3,
    }

    #[derive(Debug, Inspect)]
    pub enum UefiConsoleMode {
        /// video+kbd (having a head)
        Default = 0,
        /// headless with COM1 serial console
        COM1 = 1,
        /// headless with COM2 serial console
        COM2 = 2,
        /// headless
        None = 3,
    }

    #[derive(Debug, Inspect)]
    pub enum SecureBootTemplateType {
        /// No template to apply.
        None,
        /// Apply the Windows only CA.
        MicrosoftWindows,
        /// Apply the Microsoft UEFI CA.
        MicrosoftUefiCertificateAuthority,
    }
}

/// Response fields for Guest State Protection sent from the host
pub struct GuestStateProtection {
    /// Guest State Protection ciphertext content
    pub encrypted_gsp: GspCiphertextContent,
    /// Guest State Protection cleartext content
    pub decrypted_gsp: [GspCleartextContent; NUMBER_GSP as usize],
    /// Extended status flags
    pub extended_status_flags: GspExtendedStatusFlags,
    /// Randomized new_gsp sent in the GuestStateProtectionRequest message to
    /// the host
    pub new_gsp: GspCleartextContent,
}

/// Response fields for Guest State Protection by ID from the host
#[derive(Copy, Clone)]
pub struct GuestStateProtectionById {
    /// Guest State Protection cleartext content
    pub seed: GspCleartextContent,
    /// Extended status flags
    pub extended_status_flags: GspExtendedStatusFlags,
}

impl GuestStateProtectionById {
    /// Construct a blank instance of `GuestStateProtectionById`
    pub fn new_zeroed() -> GuestStateProtectionById {
        GuestStateProtectionById {
            seed: GspCleartextContent::new_zeroed(),
            extended_status_flags: GspExtendedStatusFlags::new_zeroed(),
        }
    }
}

/// Response for IGVM Attest from the host
#[derive(Clone)]
pub struct IgvmAttest {
    /// Response data
    pub response: Vec<u8>,
}

/// Response fields for VMGS Get Device Info from the host
pub struct VmgsGetDeviceInfo {
    /// Status of the request
    pub status: VmgsIoStatus,
    /// Logical sectors
    pub capacity: u64,
    /// Bytes per logical sector
    pub bytes_per_logical_sector: u16,
    /// Bytes per physical sector
    pub bytes_per_physical_sector: u16,
    /// Maximum transfer size bytes
    pub maximum_transfer_size_bytes: u32,
}

/// Response fields from Time from the host
#[derive(Debug, Copy, Clone)]
pub struct Time {
    /// UTC, in 100ns units since Jan 1 1601.
    ///
    /// (corresponds to `RtlGetSystemTime()` on the Host)
    pub utc: i64,
    /// Time zone (as minutes from UTC)
    pub time_zone: i16,
}

/// A handle returned by `CreateRamGpaRange`, which can be passed to
/// `ResetRamGpaRange` in order to reset the associated range.
#[derive(Debug)]
pub struct RemoteRamGpaRangeHandle(u32);

impl RemoteRamGpaRangeHandle {
    /// Return a raw u32 that represents this handle
    pub fn as_raw(&self) -> u32 {
        self.0
    }

    /// Create a new [`RemoteRamGpaRangeHandle`] from a raw u32 previously
    /// returned from `into_raw`.
    pub fn from_raw(handle: u32) -> Self {
        RemoteRamGpaRangeHandle(handle)
    }
}

/// Request to save Guest state during servicing.
#[derive(MeshPayload)]
pub struct GuestSaveRequest {
    /// GUID associated with the request.
    pub correlation_id: Guid,
    /// When to complete the request.
    pub timeout_hint: Duration,
    /// Flags bitfield.
    #[mesh(encoding = "mesh::payload::encoding::ZeroCopyEncoding")]
    pub capabilities_flags: SaveGuestVtl2StateFlags,
}
