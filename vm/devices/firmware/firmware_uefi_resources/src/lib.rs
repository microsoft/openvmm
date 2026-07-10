// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for the Hyper-V UEFI helper device
//! ([`firmware_uefi`](../firmware_uefi/index.html)).
//!
//! This crate exists so that crates which need to construct a UEFI device
//! handle (e.g., `vm_manifest_builder`) or register platform-specific
//! resolvers (e.g., `openvmm_core`, `underhill_core`) do not need to take a
//! dependency on the full `firmware_uefi` device implementation.

#![forbid(unsafe_code)]
#![expect(missing_docs)]

pub use hcl_compat_uefi_nvram_resources::HclCompatNvramQuirks;

use chipset_resources::CmosRtcTimeSourceHandleKind;
use firmware_uefi_custom_vars::CustomVars;
use inspect::Inspect;
use mesh::MeshPayload;
use mesh_protobuf::Protobuf;
use std::borrow::Cow;
use uefi_specs::hyperv::debug_level::DEBUG_ERROR;
use uefi_specs::hyperv::debug_level::DEBUG_FLAG_NAMES;
use uefi_specs::hyperv::debug_level::DEBUG_INFO;
use uefi_specs::hyperv::debug_level::DEBUG_WARN;
use vm_resource::CanResolveTo;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::ResourceKind;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vm_resource::kind::NonVolatileStoreKind;
use watchdog_core::platform::WatchdogPlatform;

/// A centralized place to expose various service-specific interface traits that
/// must be implemented by the "platform" hosting the UEFI device.
///
/// This layer of abstraction allows the re-using the same UEFI emulator between
/// multiple VMMs (OpenVMM, Underhill, etc...), without tying the emulator to any
/// VMM specific infrastructure (via some kind of compile-time feature flag
/// infrastructure).
pub mod platform {
    /// A UEFI event that should be surfaced to the host.
    #[derive(Debug)]
    pub enum UefiEvent {
        BootSuccess(BootInfo),
        BootFailure(BootInfo),
        NoBootDevice,
    }

    /// Information about a boot attempt.
    #[derive(Debug)]
    pub struct BootInfo {
        pub secure_boot_succeeded: bool,
    }

    /// Interface to log UEFI events.
    pub trait UefiLogger: Send {
        fn log_event(&self, event: UefiEvent);
    }

    /// Callbacks that enable nvram services to revoke VSM on
    /// `ExitBootServices` if requested by the guest.
    pub trait VsmConfig: Send {
        fn revoke_guest_vsm(&self);
    }
}

/// The UEFI command set understood by the device.
#[derive(Debug, Inspect, PartialEq, Clone, Protobuf)]
pub enum UefiCommandSet {
    X64,
    Aarch64,
}

/// Log level configuration - encapsulates a `u32` mask where [`u32::MAX`] means
/// "log everything".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Protobuf)]
#[mesh(transparent)]
pub struct LogLevel(u32);

impl LogLevel {
    /// Create default log level configuration (ERROR and WARN only)
    pub const fn make_default() -> Self {
        Self(DEBUG_ERROR | DEBUG_WARN)
    }

    /// Create info log level configuration (ERROR, WARN, and INFO)
    pub const fn make_info() -> Self {
        Self(DEBUG_ERROR | DEBUG_WARN | DEBUG_INFO)
    }

    /// Create full log level configuration (all levels)
    pub const fn make_full() -> Self {
        Self(u32::MAX)
    }

    /// Checks if a raw debug level should be logged based on this log level
    /// configuration.
    pub fn should_log(self, raw_debug_level: u32) -> bool {
        if self.0 == u32::MAX {
            true
        } else {
            (raw_debug_level & self.0) != 0
        }
    }

    /// Returns the raw u32 mask.
    pub fn as_u32(self) -> u32 {
        self.0
    }
}

impl Default for LogLevel {
    fn default() -> Self {
        Self::make_default()
    }
}

impl Inspect for LogLevel {
    fn inspect(&self, req: inspect::Request<'_>) {
        let human_readable = debug_level_to_string(self.0);
        req.respond()
            .field("raw_value", self.0)
            .field("debug_levels", human_readable.as_ref());
    }
}

/// Converts a debug level mask to a human-readable string.
pub fn debug_level_to_string(debug_level: u32) -> Cow<'static, str> {
    if debug_level.count_ones() == 1 {
        if let Some(&(_, name)) = DEBUG_FLAG_NAMES
            .iter()
            .find(|&&(flag, _)| flag == debug_level)
        {
            return Cow::Borrowed(name);
        }
    }

    let flags: Vec<&str> = DEBUG_FLAG_NAMES
        .iter()
        .filter(|&&(flag, _)| debug_level & flag != 0)
        .map(|&(_, name)| name)
        .collect();

    if flags.is_empty() {
        Cow::Borrowed("UNKNOWN")
    } else {
        Cow::Owned(flags.join("+"))
    }
}

/// Static configuration for the UEFI device.
#[derive(Clone, Protobuf)]
pub struct UefiConfig {
    pub custom_uefi_vars: CustomVars,
    pub secure_boot: bool,
    pub initial_generation_id: [u8; 16],
    pub use_mmio: bool,
    pub command_set: UefiCommandSet,
    pub diagnostics_log_level: LogLevel,
    pub diagnostics_rate_limit: Option<u32>,
}

/// Resource kind for the platform-provided UEFI logger.
pub enum UefiLoggerHandleKind {}

impl ResourceKind for UefiLoggerHandleKind {
    const NAME: &'static str = "uefi_logger";
}

/// Resolved UEFI logger.
pub struct ResolvedUefiLogger(pub Box<dyn platform::UefiLogger>);

impl CanResolveTo<ResolvedUefiLogger> for UefiLoggerHandleKind {
    type Input<'a> = ();
}

/// Resource kind for the UEFI watchdog platform implementation.
pub enum UefiWatchdogPlatformHandleKind {}

impl ResourceKind for UefiWatchdogPlatformHandleKind {
    const NAME: &'static str = "uefi_watchdog_platform";
}

/// Resolved UEFI watchdog platform, including the receiver used by the device
/// to wake up on watchdog timeout notifications.
pub struct ResolvedUefiWatchdogPlatform {
    pub platform: Box<dyn WatchdogPlatform>,
    pub watchdog_recv: mesh::Receiver<()>,
}

impl CanResolveTo<ResolvedUefiWatchdogPlatform> for UefiWatchdogPlatformHandleKind {
    type Input<'a> = &'a ();
}

/// Resource kind for the platform VSM configuration callbacks.
pub enum UefiVsmConfigHandleKind {}

impl ResourceKind for UefiVsmConfigHandleKind {
    const NAME: &'static str = "uefi_vsm_config";
}

/// Resolved VSM configuration callbacks.
pub struct ResolvedUefiVsmConfig(pub Box<dyn platform::VsmConfig>);

impl CanResolveTo<ResolvedUefiVsmConfig> for UefiVsmConfigHandleKind {
    type Input<'a> = ();
}

/// A handle to the Hyper-V UEFI helper chipset device.
#[derive(MeshPayload)]
pub struct UefiDeviceHandle {
    /// Static configuration data.
    pub config: UefiConfig,
    /// Quirks for the NVRAM storage.
    pub storage_quirks: Option<HclCompatNvramQuirks>,
    /// Channel receiver for updated generation ID values.
    pub generation_id_recv: mesh::Receiver<[u8; 16]>,
    /// Platform-provided UEFI event logger.
    pub logger: Resource<UefiLoggerHandleKind>,
    /// UEFI NVRAM backing storage.
    pub nvram_storage: Resource<NonVolatileStoreKind>,
    /// Platform-provided UEFI watchdog hooks (NMI on x64, halt on aarch64,
    /// etc.).
    pub watchdog_platform: Resource<UefiWatchdogPlatformHandleKind>,
    /// Optional platform-provided VSM revocation callbacks. Only used by
    /// platforms that support guest VSM.
    pub vsm_config: Option<Resource<UefiVsmConfigHandleKind>>,
    /// Real-time clock time source used for UEFI time services.
    pub time_source: Resource<CmosRtcTimeSourceHandleKind>,
}

impl ResourceId<ChipsetDeviceHandleKind> for UefiDeviceHandle {
    const ID: &'static str = "hyperv_firmware_uefi";
}
