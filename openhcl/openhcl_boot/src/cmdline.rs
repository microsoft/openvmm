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

#[derive(Debug, PartialEq)]
pub struct BootCommandLineOptions {
    pub logger: Option<LoggerType>,
    pub confidential_debug: bool,
    pub enable_vtl2_gpa_pool: Option<u64>,
}

/// Parse arguments from a command line.
pub fn parse_boot_command_line(cmdline: &str) -> BootCommandLineOptions {
    let mut result = BootCommandLineOptions {
        logger: None,
        confidential_debug: false,
        enable_vtl2_gpa_pool: None,
    };

    for arg in cmdline.split_whitespace() {
        if arg.starts_with(BOOT_LOG) {
            let arg = arg.split_once('=').map(|(_, arg)| arg);
            if let Some(SERIAL_LOGGER) = arg {
                result.logger = Some(LoggerType::Serial)
            }
        } else if arg.starts_with(OPENHCL_CONFIDENTIAL_DEBUG_ENV_VAR_NAME) {
            let arg = arg.split_once('=').map(|(_, arg)| arg);
            if arg.is_some_and(|a| a != "0") {
                result.confidential_debug = true;
                // Explicit logger specification overrides this default.
                if result.logger.is_none() {
                    result.logger = Some(LoggerType::Serial);
                }
            }
        } else if arg.starts_with(ENABLE_VTL2_GPA_POOL) {
            result.enable_vtl2_gpa_pool = arg.split_once('=').and_then(|(_, arg)| {
                let num = arg.parse::<u64>().unwrap_or(0);
                // A size of 0 or failure to parse is treated as disabling
                // the pool.
                if num == 0 {
                    None
                } else {
                    Some(num)
                }
            });
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_console_parsing() {
        assert_eq!(
            parse_boot_command_line("OPENHCL_BOOT_LOG=com3"),
            BootCommandLineOptions {
                logger: Some(LoggerType::Serial),
                confidential_debug: false,
                enable_vtl2_gpa_pool: None
            }
        );

        assert_eq!(
            parse_boot_command_line("OPENHCL_BOOT_LOG=1"),
            BootCommandLineOptions {
                logger: None,
                confidential_debug: false,
                enable_vtl2_gpa_pool: None
            }
        );

        assert_eq!(
            parse_boot_command_line("OPENHCL_BOOT_LOG=random"),
            BootCommandLineOptions {
                logger: None,
                confidential_debug: false,
                enable_vtl2_gpa_pool: None
            }
        );

        assert_eq!(
            parse_boot_command_line("OPENHCL_BOOT_LOG==com3"),
            BootCommandLineOptions {
                logger: None,
                confidential_debug: false,
                enable_vtl2_gpa_pool: None
            }
        );

        assert_eq!(
            parse_boot_command_line("OPENHCL_BOOT_LOGserial"),
            BootCommandLineOptions {
                logger: None,
                confidential_debug: false,
                enable_vtl2_gpa_pool: None
            }
        );

        let cmdline = format!("{OPENHCL_CONFIDENTIAL_DEBUG_ENV_VAR_NAME}=1");
        assert_eq!(
            parse_boot_command_line(&cmdline),
            BootCommandLineOptions {
                logger: Some(LoggerType::Serial),
                confidential_debug: true,
                enable_vtl2_gpa_pool: None
            }
        );
    }

    #[test]
    fn test_vtl2_gpa_pool_parsing() {
        assert_eq!(
            parse_boot_command_line("OPENHCL_ENABLE_VTL2_GPA_POOL=1"),
            BootCommandLineOptions {
                logger: None,
                confidential_debug: false,
                enable_vtl2_gpa_pool: Some(1),
            }
        );
        assert_eq!(
            parse_boot_command_line("OPENHCL_ENABLE_VTL2_GPA_POOL=0"),
            BootCommandLineOptions {
                logger: None,
                confidential_debug: false,
                enable_vtl2_gpa_pool: None,
            }
        );
        assert_eq!(
            parse_boot_command_line("OPENHCL_ENABLE_VTL2_GPA_POOL=asdf"),
            BootCommandLineOptions {
                logger: None,
                confidential_debug: false,
                enable_vtl2_gpa_pool: None,
            }
        );
        assert_eq!(
            parse_boot_command_line("OPENHCL_ENABLE_VTL2_GPA_POOL=1024"),
            BootCommandLineOptions {
                logger: None,
                confidential_debug: false,
                enable_vtl2_gpa_pool: Some(1024),
            }
        );
    }
}
