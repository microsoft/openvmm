// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command line arguments and parsing for openhcl_boot.

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
const IGVM_VTL2_GPA_POOL_CONFIG: &str = "OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG=";

/// Test-legacy/test-compat override for `OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG`.
/// (otherwise, tests cannot modify the VTL2 GPA pool config different from what
/// may be in the manifest).
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

impl<S: AsRef<str>> From<S> for Vtl2GpaPoolConfig {
    fn from(arg: S) -> Self {
        match arg.as_ref() {
            "debug" => Vtl2GpaPoolConfig::Heuristics(Vtl2GpaPoolLookupTable::Debug),
            "release" => Vtl2GpaPoolConfig::Heuristics(Vtl2GpaPoolLookupTable::Release),
            "off" => Vtl2GpaPoolConfig::Off,
            _ => {
                let num = arg.as_ref().parse::<u64>().unwrap_or(0);
                // A size of 0 or failure to parse is treated as disabling
                // the pool.
                if num == 0 {
                    Vtl2GpaPoolConfig::Off
                } else {
                    Vtl2GpaPoolConfig::Pages(num)
                }
            }
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum SidecarOptions {
    /// Sidecar is enabled (either via command line or by default),
    /// but should be ignored if this is a restore and the host has
    /// devices and the number of VPs below the threshold.
    Enabled {
        enable_logging: bool,
        cpu_threshold: Option<u32>,
    },
    /// Sidecar is disabled because this is a restore from save state (during servicing),
    /// and sidecar will not benefit this specific scenario.
    DisabledServicing,
    /// Sidecar is explicitly disabled via command line.
    DisabledCommandLine,
}

impl SidecarOptions {
    pub const DEFAULT_CPU_THRESHOLD: Option<u32> = Some(100);
    pub const fn default() -> Self {
        SidecarOptions::Enabled {
            enable_logging: false,
            cpu_threshold: Self::DEFAULT_CPU_THRESHOLD,
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct BootCommandLineOptions {
    pub confidential_debug: bool,
    pub enable_vtl2_gpa_pool: Vtl2GpaPoolConfig,
    pub sidecar: SidecarOptions,
    pub disable_nvme_keep_alive: bool,
}

impl BootCommandLineOptions {
    pub const fn new() -> Self {
        BootCommandLineOptions {
            confidential_debug: false,
            enable_vtl2_gpa_pool: Vtl2GpaPoolConfig::Heuristics(Vtl2GpaPoolLookupTable::Release), // use the release config by default
            sidecar: SidecarOptions::default(),
            disable_nvme_keep_alive: false,
        }
    }
}

impl BootCommandLineOptions {
    /// Parse arguments from a command line.
    pub fn parse(&mut self, cmdline: &str) {
        let mut override_vtl2_gpa_pool: Option<Vtl2GpaPoolConfig> = None;
        for arg in cmdline.split_whitespace() {
            if arg.starts_with(OPENHCL_CONFIDENTIAL_DEBUG_ENV_VAR_NAME) {
                let arg = arg.split_once('=').map(|(_, arg)| arg);
                if arg.is_some_and(|a| a != "0") {
                    self.confidential_debug = true;
                }
            } else if arg.starts_with(IGVM_VTL2_GPA_POOL_CONFIG) {
                if let Some((_, arg)) = arg.split_once('=') {
                    self.enable_vtl2_gpa_pool = Vtl2GpaPoolConfig::from(arg);
                } else {
                    log::warn!("WARNING: Missing value for IGVM_VTL2_GPA_POOL_CONFIG argument");
                }
            } else if arg.starts_with(ENABLE_VTL2_GPA_POOL) {
                if let Some((_, arg)) = arg.split_once('=') {
                    override_vtl2_gpa_pool = Some(Vtl2GpaPoolConfig::from(arg));
                } else {
                    log::warn!("WARNING: Missing value for ENABLE_VTL2_GPA_POOL argument");
                }
            } else if arg.starts_with(SIDECAR) {
                if let Some((_, arg)) = arg.split_once('=') {
                    for arg in arg.split(',') {
                        match arg {
                            "off" => self.sidecar = SidecarOptions::DisabledCommandLine,
                            "on" => {
                                self.sidecar = SidecarOptions::Enabled {
                                    enable_logging: false,
                                    cpu_threshold: SidecarOptions::DEFAULT_CPU_THRESHOLD,
                                }
                            }
                            "log" => {
                                self.sidecar = SidecarOptions::Enabled {
                                    enable_logging: true,
                                    cpu_threshold: SidecarOptions::DEFAULT_CPU_THRESHOLD,
                                }
                            }
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

        if let Some(override_config) = override_vtl2_gpa_pool {
            self.enable_vtl2_gpa_pool = override_config;
            log::info!(
                "INFO: Overriding VTL2 GPA pool config to {:?} from command line",
                override_config
            );
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
        for (cmdline, expected) in [
            (
                // default
                "",
                Vtl2GpaPoolConfig::Heuristics(Vtl2GpaPoolLookupTable::Release),
            ),
            (
                "OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG=1",
                Vtl2GpaPoolConfig::Pages(1),
            ),
            (
                "OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG=0",
                Vtl2GpaPoolConfig::Off,
            ),
            (
                "OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG=asdf",
                Vtl2GpaPoolConfig::Off,
            ),
            (
                "OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG=512",
                Vtl2GpaPoolConfig::Pages(512),
            ),
            (
                "OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG=off",
                Vtl2GpaPoolConfig::Off,
            ),
            (
                "OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG=debug",
                Vtl2GpaPoolConfig::Heuristics(Vtl2GpaPoolLookupTable::Debug),
            ),
            (
                "OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG=release",
                Vtl2GpaPoolConfig::Heuristics(Vtl2GpaPoolLookupTable::Release),
            ),
            (
                // OPENHCL_ENABLE_VTL2_GPA_POOL= takes precedence over OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG=
                "OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG=release OPENHCL_ENABLE_VTL2_GPA_POOL=debug",
                Vtl2GpaPoolConfig::Heuristics(Vtl2GpaPoolLookupTable::Debug),
            ),
        ] {
            assert_eq!(
                parse_boot_command_line(cmdline).enable_vtl2_gpa_pool,
                expected,
                "Failed parsing VTL2 GPA pool config from command line: {}",
                cmdline
            );
        }
    }

    #[test]
    fn test_sidecar_parsing() {
        assert_eq!(
            parse_boot_command_line("OPENHCL_SIDECAR=on"),
            BootCommandLineOptions {
                sidecar: SidecarOptions::Enabled {
                    enable_logging: false,
                    cpu_threshold: SidecarOptions::DEFAULT_CPU_THRESHOLD,
                },
                ..BootCommandLineOptions::new()
            }
        );
        assert_eq!(
            parse_boot_command_line("OPENHCL_SIDECAR=off"),
            BootCommandLineOptions {
                sidecar: SidecarOptions::DisabledCommandLine,
                ..BootCommandLineOptions::new()
            }
        );
        assert_eq!(
            parse_boot_command_line("OPENHCL_SIDECAR=on,off"),
            BootCommandLineOptions {
                sidecar: SidecarOptions::DisabledCommandLine,
                ..BootCommandLineOptions::new()
            }
        );
        assert_eq!(
            parse_boot_command_line("OPENHCL_SIDECAR=on,log"),
            BootCommandLineOptions {
                sidecar: SidecarOptions::Enabled {
                    enable_logging: true,
                    cpu_threshold: SidecarOptions::DEFAULT_CPU_THRESHOLD,
                },
                ..BootCommandLineOptions::new()
            }
        );
        assert_eq!(
            parse_boot_command_line("OPENHCL_SIDECAR=log"),
            BootCommandLineOptions {
                sidecar: SidecarOptions::Enabled {
                    enable_logging: true,
                    cpu_threshold: SidecarOptions::DEFAULT_CPU_THRESHOLD,
                },
                ..BootCommandLineOptions::new()
            }
        );
    }
}
