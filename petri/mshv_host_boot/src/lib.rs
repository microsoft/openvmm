// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! On-disk configuration for the development-only mshv UEFI host boot application.

#![no_std]

/// Linux EFI stub load options, relative to the EFI System Partition root.
pub const KERNEL_CMDLINE_FILE: &str = "linux.cmdline";
/// HvLoader load options, relative to the EFI System Partition root.
pub const HYPERVISOR_CMDLINE_FILE: &str = "hvloader.cmdline";
/// Maximum size of either command-line file, in bytes.
pub const MAX_CMDLINE_SIZE: usize = 16 * 1024;
/// Validate literal, single-line UCS-2 load options, allowing a final LF or CRLF.
pub fn command_line(text: &str) -> Result<&str, &'static str> {
    if text.len() > MAX_CMDLINE_SIZE {
        return Err("command-line file exceeds 16 KiB");
    }
    let text = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    if text
        .chars()
        .any(|c| c.is_control() || u32::from(c) > 0xffff)
    {
        return Err("command line contains a control or non-UCS-2 character");
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_options_and_line_endings() {
        let options = r#"console=ttyS0 key="two words" x=$root;halt"#;
        assert_eq!(command_line(options), Ok(options));
        assert_eq!(command_line("MSHV_ENABLE=TRUE\r\n"), Ok("MSHV_ENABLE=TRUE"));
        assert_eq!(command_line("console=ttyS0\n"), Ok("console=ttyS0"));
        assert_eq!(command_line(""), Ok(""));
    }

    #[test]
    fn rejects_malformed_options() {
        for text in ["x\0y", "x\ny", "x\n\n", "x\r", "x\ty", "x\u{10000}"] {
            assert!(command_line(text).is_err(), "{text:?}");
        }
        assert!(
            command_line(core::str::from_utf8(&[b'x'; MAX_CMDLINE_SIZE + 1]).unwrap()).is_err()
        );
    }
}
