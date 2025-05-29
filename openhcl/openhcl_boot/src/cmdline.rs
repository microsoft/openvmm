// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command line arguments and parsing for openhcl_boot.

use crate::boot_logger::LoggerType;
use underhill_confidentiality::OPENHCL_CONFIDENTIAL_DEBUG_ENV_VAR_NAME;

/// Enable boot logging in the bootloader.
///
/// Format of `OPENHCL_BOOT_LOG=<logger>`, with valid loggers being:
///     - `com3`: use the com3 serial port, available on no isolation or Tdx.
const BOOT_LOG: &str = "OPENHCL_BOOT_LOG=";
const SERIAL_LOGGER: &str = "com3";

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

/// Options controlling sidecar.
///
/// * `off`: Disable sidecar support.
/// * `on`: Enable sidecar support. Sidecar will still only be started if
///   sidecar is present in the binary and supported on the platform. This
///   is the default.
/// * `log`: Enable sidecar logging.
const SIDECAR: &str = "OPENHCL_SIDECAR=";

#[derive(Debug, PartialEq)]
pub struct BootCommandLineOptions {
    pub logger: Option<LoggerType>,
    pub confidential_debug: bool,
    pub enable_vtl2_gpa_pool: Option<u64>,
    pub sidecar: bool,
    pub sidecar_logging: bool,
}

impl BootCommandLineOptions {
    pub const fn new() -> Self {
        BootCommandLineOptions {
            logger: None,
            confidential_debug: false,
            enable_vtl2_gpa_pool: None,
            sidecar: true, // sidecar is enabled by default
            sidecar_logging: false,
        }
    }
}

impl BootCommandLineOptions {
    /// Parse arguments from a command line.
    pub fn parse(&mut self, cmdline: &str) {
        for arg in cmdline.split_whitespace() {
            if arg.starts_with(BOOT_LOG) {
                if let Some(SERIAL_LOGGER) = arg.split_once('=').map(|(_, arg)| arg) {
                    self.logger = Some(LoggerType::Serial)
                }
            } else if arg.starts_with(OPENHCL_CONFIDENTIAL_DEBUG_ENV_VAR_NAME) {
                let arg = arg.split_once('=').map(|(_, arg)| arg);
                if arg.is_some_and(|a| a != "0") {
                    self.confidential_debug = true;
                    // Explicit logger specification overrides this default.
                    if self.logger.is_none() {
                        self.logger = Some(LoggerType::Serial);
                    }
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
    fn test_console_parsing() {
        assert_eq!(
            parse_boot_command_line("OPENHCL_BOOT_LOG=com3"),
            BootCommandLineOptions {
                logger: Some(LoggerType::Serial),
                ..BootCommandLineOptions::new()
            }
        );

        assert_eq!(
            parse_boot_command_line("OPENHCL_BOOT_LOG=1"),
            BootCommandLineOptions {
                logger: None,
                ..BootCommandLineOptions::new()
            }
        );

        assert_eq!(
            parse_boot_command_line("OPENHCL_BOOT_LOG=random"),
            BootCommandLineOptions {
                logger: None,
                ..BootCommandLineOptions::new()
            }
        );

        assert_eq!(
            parse_boot_command_line("OPENHCL_BOOT_LOG==com3"),
            BootCommandLineOptions {
                logger: None,
                ..BootCommandLineOptions::new()
            }
        );

        assert_eq!(
            parse_boot_command_line("OPENHCL_BOOT_LOGserial"),
            BootCommandLineOptions {
                logger: None,
                ..BootCommandLineOptions::new()
            }
        );

        let cmdline = format!("{OPENHCL_CONFIDENTIAL_DEBUG_ENV_VAR_NAME}=1");
        assert_eq!(
            parse_boot_command_line(&cmdline),
            BootCommandLineOptions {
                logger: Some(LoggerType::Serial),
                confidential_debug: true,
                ..BootCommandLineOptions::new()
            }
        );
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
