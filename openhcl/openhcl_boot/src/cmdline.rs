// Copyright (C) Microsoft Corporation. All rights reserved.

//! Command line arguments and parsing for openhcl_boot.

use crate::boot_logger::LoggerType;
use underhill_confidentiality::UNDERHILL_CONFIDENTIAL_DEBUG_ENV_VAR_NAME;

/// Enable boot logging in the bootloader.
///
/// Format of `OPENHCL_BOOT_LOG=<logger>`, with valid loggers being:
///     - `com3`: use the com3 serial port, available on no isolation or Tdx.
const BOOT_LOG: &str = "OPENHCL_BOOT_LOG=";
const SERIAL_LOGGER: &str = "com3";

#[derive(Debug, PartialEq)]
pub struct BootCommandLineOptions {
    pub logger: Option<LoggerType>,
    pub confidential_debug: bool,
}

/// Parse arguments from a command line.
pub fn parse_boot_command_line(cmdline: &str) -> BootCommandLineOptions {
    let mut result = BootCommandLineOptions {
        logger: None,
        confidential_debug: false,
    };

    for arg in cmdline.split_whitespace() {
        if arg.starts_with(BOOT_LOG) {
            let arg = arg.split_once('=').map(|(_, arg)| arg);
            if let Some(SERIAL_LOGGER) = arg {
                result.logger = Some(LoggerType::Serial)
            }
        } else if arg.starts_with(UNDERHILL_CONFIDENTIAL_DEBUG_ENV_VAR_NAME) {
            let arg = arg.split_once('=').map(|(_, arg)| arg);
            if arg.is_some_and(|a| a != "0") {
                result.confidential_debug = true;
                // Explicit logger specification overrides this default.
                if result.logger.is_none() {
                    result.logger = Some(LoggerType::Serial);
                }
            }
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
                confidential_debug: false
            }
        );

        assert_eq!(
            parse_boot_command_line("OPENHCL_BOOT_LOG=1"),
            BootCommandLineOptions {
                logger: None,
                confidential_debug: false
            }
        );

        assert_eq!(
            parse_boot_command_line("OPENHCL_BOOT_LOG=random"),
            BootCommandLineOptions {
                logger: None,
                confidential_debug: false
            }
        );

        assert_eq!(
            parse_boot_command_line("OPENHCL_BOOT_LOG==com3"),
            BootCommandLineOptions {
                logger: None,
                confidential_debug: false
            }
        );

        assert_eq!(
            parse_boot_command_line("OPENHCL_BOOT_LOGserial"),
            BootCommandLineOptions {
                logger: None,
                confidential_debug: false
            }
        );

        let cmdline = format!("{UNDERHILL_CONFIDENTIAL_DEBUG_ENV_VAR_NAME}=1");
        assert_eq!(
            parse_boot_command_line(&cmdline),
            BootCommandLineOptions {
                logger: Some(LoggerType::Serial),
                confidential_debug: true
            }
        );
    }
}
