// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command line arguments and parsing for openhcl_boot.

use underhill_confidentiality::OPENHCL_CONFIDENTIAL_DEBUG_ENV_VAR_NAME;

/// Enable the private VTL2 GPA pool for page allocations. This is only enabled
/// via the command line, because in order to support the VTL2 GPA pool
/// generically, the boot shim must read serialized data from the previous
/// OpenHCL instance on a servicing boot in order to guarantee the same memory
/// layout is presented.
///
/// The value specified is the number of 4K pages to reserve for the pool.
///
/// TODO: Remove this commandline once support for reading saved state is
/// supported in openhcl_boot.
const ENABLE_VTL2_GPA_POOL: &str = "OPENHCL_ENABLE_VTL2_GPA_POOL=";

/// Lookup table for calculating VTL2 GPA pool size.
///
/// * `release`: Use the release version of the lookup table.
/// * `debug`: Use the debug version of the lookup table.
///
/// The debug profiles require more VTL2 memory, and thus the heuristics in the
/// lookup table are different.
///
/// If not specified, then `release` is used.
const VTL2_GPA_POOL_LOOKUP_TABLE: &str = "OPENHCL_VTL2_GPA_POOL_LOOKUP_TABLE=";

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

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Vtl2GpaPoolLookupTable {
    Release,
    Debug,
}

#[derive(Debug, PartialEq)]
pub struct BootCommandLineOptions {
    pub confidential_debug: bool,
    pub enable_vtl2_gpa_pool: Option<u64>,
    pub sidecar: bool,
    pub sidecar_logging: bool,
    pub disable_nvme_keep_alive: bool,
    pub vtl2_gpa_pool_lookup_table: Vtl2GpaPoolLookupTable,
}

impl BootCommandLineOptions {
    pub const fn new() -> Self {
        BootCommandLineOptions {
            confidential_debug: false,
            enable_vtl2_gpa_pool: None,
            sidecar: true, // sidecar is enabled by default
            sidecar_logging: false,
            disable_nvme_keep_alive: false,
            vtl2_gpa_pool_lookup_table: Vtl2GpaPoolLookupTable::Release,
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
                self.enable_vtl2_gpa_pool = arg.split_once('=').and_then(|(_, arg)| {
                    let num = arg.parse::<u64>().unwrap_or(0);
                    // A size of 0 or failure to parse is treated as disabling
                    // the pool.
                    if num == 0 { None } else { Some(num) }
                });
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
            } else if arg.starts_with(VTL2_GPA_POOL_LOOKUP_TABLE) {
                if let Some((_, arg)) = arg.split_once('=') {
                    for arg in arg.split(',') {
                        match arg {
                            "debug" => {
                                self.vtl2_gpa_pool_lookup_table = Vtl2GpaPoolLookupTable::Debug
                            }
                            "release" => {
                                self.vtl2_gpa_pool_lookup_table = Vtl2GpaPoolLookupTable::Release
                            }
                            _ => {}
                        }
                    }
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
                enable_vtl2_gpa_pool: Some(1),
                ..BootCommandLineOptions::new()
            }
        );
        assert_eq!(
            parse_boot_command_line("OPENHCL_ENABLE_VTL2_GPA_POOL=0"),
            BootCommandLineOptions {
                enable_vtl2_gpa_pool: None,
                ..BootCommandLineOptions::new()
            }
        );
        assert_eq!(
            parse_boot_command_line("OPENHCL_ENABLE_VTL2_GPA_POOL=asdf"),
            BootCommandLineOptions {
                enable_vtl2_gpa_pool: None,
                ..BootCommandLineOptions::new()
            }
        );
        assert_eq!(
            parse_boot_command_line("OPENHCL_ENABLE_VTL2_GPA_POOL=512"),
            BootCommandLineOptions {
                enable_vtl2_gpa_pool: Some(512),
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
