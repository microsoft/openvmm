// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for constructing VM configurations.

use openvmm_defs::config::LinuxDirectBootMode;
use openvmm_defs::config::LoadMode;

/// Whether this boot path exposes the host's ACPI power button to the guest.
///
/// IGVM images supply their own firmware tables, and device-tree-only and
/// MP-table Linux boot have no ACPI button description. `None` carries no
/// firmware information; snapshot restore must reconstruct the original device
/// configuration.
pub fn supports_acpi_power_button(load_mode: &LoadMode) -> bool {
    match load_mode {
        LoadMode::Linux { boot_mode, .. } => match boot_mode {
            LinuxDirectBootMode::Acpi => true,
            LinuxDirectBootMode::DeviceTree | LinuxDirectBootMode::MpTable => false,
        },
        LoadMode::Uefi { .. } | LoadMode::Pcat { .. } => true,
        LoadMode::Igvm { .. } | LoadMode::None => false,
    }
}
