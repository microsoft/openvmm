// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command line arguments and parsing for openhcl_boot.

use crate::boot_logger::log;
use underhill_confidentiality::OPENHCL_CONFIDENTIAL_DEBUG_ENV_VAR_NAME;

/// Enable the private VTL2 GPA pool for page allocations.
///
/// Possible values:
/// * `release`: Use the release version of the lookup table (default), or device tree.
/// * `debug`: Use the debug version of the lookup table, or device tree.
/// * `off`: Disable the VTL2 GPA pool.
/// * `<num_pages>`: Explicitly specify the size of the VTL2 GPA pool.
///
/// See `Vtl2GpaPoolConfig` for more details.
const ENABLE_VTL2_GPA_POOL: &str = "OPENHCL_ENABLE_VTL2_GPA_POOL=";

/// Options controlling sidecar.
///
/// * `off`: Disable sidecar support.
/// * `on`: Enable sidecar support. Sidecar will still only be started if
///   sidecar is present in the binary and supported on the platform. This
///   is the default.
/// * `log`: Enable sidecar logging.
const SIDECAR: &str = "OPENHCL_SIDECAR=";

/// Disable NVME keep alive regardless if the host supports it.
const DISABLE_NVME_KEEP_ALIVE: &str = "OPENHCL_DISABLE_NVME_KEEP_ALIVE=";

/// Lookup table to use for VTL2 GPA pool size heuristics.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Vtl2GpaPoolLookupTable {
    Release,
    Debug,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Vtl2GpaPoolConfig {
    /// Use heuristics to determine the VTL2 GPA pool size.
    /// Reserve a default size based on the amount of VTL2 ram and
    /// number of vCPUs. The point of this method is to account for cases where
    /// we retrofit the private pool into existing deployments that do not
    /// specify it explicitly.
    ///
    /// If the host specifies a size via the device tree, that size will be used
    /// instead.
    ///
    /// The lookup table specifies whether to use the debug or release
    /// heuristics (as the dev manifests provide different amounts of VTL2 RAM).
    Heuristics(Vtl2GpaPoolLookupTable),

    /// Explicitly disable the VTL2 private pool.
    Off,

    /// Explicitly specify the size of the VTL2 GPA pool in pages.
    Pages(u64),
}

#[derive(Debug, PartialEq)]
pub struct BootCommandLineOptions {
    pub confidential_debug: bool,
    pub enable_vtl2_gpa_pool: Vtl2GpaPoolConfig,
    pub sidecar: bool,
    pub sidecar_logging: bool,
    pub disable_nvme_keep_alive: bool,
}

impl BootCommandLineOptions {
    pub const fn new() -> Self {
        BootCommandLineOptions {
            confidential_debug: false,
            enable_vtl2_gpa_pool: Vtl2GpaPoolConfig::Heuristics(Vtl2GpaPoolLookupTable::Release), // use the release config by default
            sidecar: true, // sidecar is enabled by default
            sidecar_logging: false,
            disable_nvme_keep_alive: false,
        }
    }
}

impl BootCommandLineOptions {
    /// Parse arguments from a command line.
    pub fn parse(&mut self, cmdline: &str) {
        for arg in cmdline.split_whitespace() {
            if arg.starts_with(OPENHCL_CONFIDENTIAL_DEBUG_ENV_VAR_NAME) {
                let arg = arg.split_once('=').map(|(_, arg)| arg);
                if arg.is_some_and(|a| a != "0") {
                    self.confidential_debug = true;
                }
            } else if arg.starts_with(ENABLE_VTL2_GPA_POOL) {
                if let Some((_, arg)) = arg.split_once('=') {
                    self.enable_vtl2_gpa_pool = match arg {
                        "debug" => Vtl2GpaPoolConfig::Heuristics(Vtl2GpaPoolLookupTable::Debug),
                        "release" => Vtl2GpaPoolConfig::Heuristics(Vtl2GpaPoolLookupTable::Release),
                        "off" => Vtl2GpaPoolConfig::Off,
                        _ => {
                            let num = arg.parse::<u64>().unwrap_or(0);
                            // A size of 0 or failure to parse is treated as disabling
                            // the pool.
                            if num == 0 {
                                Vtl2GpaPoolConfig::Off
                            } else {
                                Vtl2GpaPoolConfig::Pages(num)
                            }
                        }
                    }
                } else {
                    log!("WARNING: Missing value for ENABLE_VTL2_GPA_POOL argument");
                }
            } else if arg.starts_with(SIDECAR) {
                if let Some((_, arg)) = arg.split_once('=') {
                    for arg in arg.split(',') {
                        match arg {
                            "off" => self.sidecar = false,
                            "on" => self.sidecar = true,
                            "log" => self.sidecar_logging = true,
                            _ => {}
                        }
                    }
                }
            } else if arg.starts_with(DISABLE_NVME_KEEP_ALIVE) {
                let arg = arg.split_once('=').map(|(_, arg)| arg);
                if arg.is_some_and(|a| a != "0") {
                    self.disable_nvme_keep_alive = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_boot_command_line(cmdline: &str) -> BootCommandLineOptions {
        let mut options = BootCommandLineOptions::new();
        options.parse(cmdline);
        options
    }

    #[test]
    fn test_vtl2_gpa_pool_parsing() {
        assert_eq!(
            parse_boot_command_line("OPENHCL_ENABLE_VTL2_GPA_POOL=1"),
            BootCommandLineOptions {
                enable_vtl2_gpa_pool: Vtl2GpaPoolConfig::Pages(1),
                ..BootCommandLineOptions::new()
            }
        );
        assert_eq!(
            parse_boot_command_line("OPENHCL_ENABLE_VTL2_GPA_POOL=0"),
            BootCommandLineOptions {
                enable_vtl2_gpa_pool: Vtl2GpaPoolConfig::Off,
                ..BootCommandLineOptions::new()
            }
        );
        assert_eq!(
            parse_boot_command_line("OPENHCL_ENABLE_VTL2_GPA_POOL=asdf"),
            BootCommandLineOptions {
                enable_vtl2_gpa_pool: Vtl2GpaPoolConfig::Off,
                ..BootCommandLineOptions::new()
            }
        );
        assert_eq!(
            parse_boot_command_line("OPENHCL_ENABLE_VTL2_GPA_POOL=512"),
            BootCommandLineOptions {
                enable_vtl2_gpa_pool: Vtl2GpaPoolConfig::Pages(512),
                ..BootCommandLineOptions::new()
            }
        );
        assert_eq!(
            parse_boot_command_line("OPENHCL_ENABLE_VTL2_GPA_POOL=off"),
            BootCommandLineOptions {
                enable_vtl2_gpa_pool: Vtl2GpaPoolConfig::Off,
                ..BootCommandLineOptions::new()
            }
        );
        assert_eq!(
            parse_boot_command_line("OPENHCL_ENABLE_VTL2_GPA_POOL=debug"),
            BootCommandLineOptions {
                enable_vtl2_gpa_pool: Vtl2GpaPoolConfig::Heuristics(Vtl2GpaPoolLookupTable::Debug),
                ..BootCommandLineOptions::new()
            }
        );
        assert_eq!(
            parse_boot_command_line("OPENHCL_ENABLE_VTL2_GPA_POOL=release"),
            BootCommandLineOptions {
                enable_vtl2_gpa_pool: Vtl2GpaPoolConfig::Heuristics(
                    Vtl2GpaPoolLookupTable::Release
                ),
                ..BootCommandLineOptions::new()
            }
        );
    }

    #[test]
    fn test_sidecar_parsing() {
        assert_eq!(
            parse_boot_command_line("OPENHCL_SIDECAR=on"),
            BootCommandLineOptions {
                sidecar: true,
                ..BootCommandLineOptions::new()
            }
        );
        assert_eq!(
            parse_boot_command_line("OPENHCL_SIDECAR=off"),
            BootCommandLineOptions {
                sidecar: false,
                ..BootCommandLineOptions::new()
            }
        );
        assert_eq!(
            parse_boot_command_line("OPENHCL_SIDECAR=on,off"),
            BootCommandLineOptions {
                sidecar: false,
                ..BootCommandLineOptions::new()
            }
        );
        assert_eq!(
            parse_boot_command_line("OPENHCL_SIDECAR=on,log"),
            BootCommandLineOptions {
                sidecar: true,
                sidecar_logging: true,
                ..BootCommandLineOptions::new()
            }
        );
        assert_eq!(
            parse_boot_command_line("OPENHCL_SIDECAR=log"),
            BootCommandLineOptions {
                sidecar: true,
                sidecar_logging: true,
                ..BootCommandLineOptions::new()
            }
        );
    }
}
