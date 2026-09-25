// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM-specific Linux boot configuration.

use super::linux::Error as LinuxError;
use super::linux::KernelConfig;
use super::linux::KernelIsolationConfig;
use guestmem::GuestMemory;
use loader::importer::X86Register;
use loader::linux::InitrdAddressType;
use loader::linux::InitrdConfig;
use memory_range::MemoryRange;
use std::ffi::CString;
use std::io::Seek;
use thiserror::Error;
use vm_loader::InitialLoad;
use vm_loader::Loader;

const LAPIC_TIMER_HZ: &str = "lapic_timer_hz";
const MIN_APIC_FREQUENCY_HZ: u64 = 1_000_000;
const VIRTIO_MMIO_DEVICE: &str = "virtio_mmio.device=";

#[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
pub fn load_linux_x86_mptable(
    cfg: &KernelConfig<'_>,
    gm: &GuestMemory,
    apic_ids: &[u32],
    level_triggered_irqs: &[u32],
    reserved_memory_ranges: &[MemoryRange],
) -> Result<InitialLoad<X86Register>, LinuxError> {
    if !matches!(cfg.isolation, KernelIsolationConfig::None) {
        return Err(LinuxError::MpTableIsolation);
    }

    let mut kernel_file = cfg.kernel;
    let (mut initrd_reader, initrd_size) = if let Some(mut initrd_file) = cfg.initrd.as_ref() {
        initrd_file.rewind().map_err(LinuxError::InitRd)?;
        let size = initrd_file
            .seek(std::io::SeekFrom::End(0))
            .map_err(LinuxError::InitRd)?;
        (Some(initrd_file), size)
    } else {
        (None, 0)
    };
    let initrd_config = initrd_reader.as_mut().map(|reader| InitrdConfig {
        initrd_address: InitrdAddressType::AfterKernel,
        initrd: reader,
        size: initrd_size,
    });
    let cmdline = CString::new(cfg.cmdline).map_err(LinuxError::CommandLineNul)?;
    let mut loader = Loader::new(gm.clone(), cfg.mem_layout, hvdef::Vtl::Vtl0);
    loader::linux::microvm::load_x86_mptable(
        &mut loader,
        &mut kernel_file,
        initrd_config,
        &cmdline,
        cfg.mem_layout,
        &loader::mptable::MpTableConfig {
            apic_ids,
            level_triggered_irqs,
        },
        reserved_memory_ranges,
    )
    .map_err(LinuxError::Loader)?;
    Ok(loader.initial_regs_and_page_imports())
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum FrequencyParameterError {
    #[error("duplicate parameter")]
    Duplicate,
    #[error("malformed parameter: {0}")]
    Malformed(String),
    #[error("value {specified} does not match the backend-reported value {reported}")]
    Mismatch { specified: u64, reported: u64 },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum ApicFrequencyError {
    #[error("LAPIC frequency {0} Hz is outside the supported range of 1 MHz through 4294967295 Hz")]
    OutOfRange(u64),
    #[error("invalid lapic_timer_hz: {0}")]
    Parameter(#[from] FrequencyParameterError),
}

fn command_line_tokens(cmdline: &str) -> (Vec<(usize, &str)>, Option<usize>) {
    let mut tokens = Vec::new();
    let mut token_start = None;
    let mut in_quote = false;
    for (offset, character) in cmdline.char_indices() {
        if character == '"' {
            in_quote = !in_quote;
        }
        if character.is_ascii_whitespace() && !in_quote {
            if let Some(start) = token_start.take() {
                tokens.push((start, &cmdline[start..offset]));
            }
        } else if token_start.is_none() {
            token_start = Some(offset);
        }
    }
    let unterminated_quote_offset = if in_quote { token_start } else { None };
    if let Some(start) = token_start {
        tokens.push((start, &cmdline[start..]));
    }
    (tokens, unterminated_quote_offset)
}

fn linux_parameter_name_matches(actual: &str, expected: &str) -> bool {
    actual
        .bytes()
        .map(|byte| if byte == b'-' { b'_' } else { byte })
        .eq(expected
            .bytes()
            .map(|byte| if byte == b'-' { b'_' } else { byte }))
}

fn parse_linux_uint(value: &str) -> Option<u32> {
    let value = if let Some(value) = value.strip_prefix('"') {
        value.strip_suffix('"')?
    } else {
        if value.ends_with('"') {
            return None;
        }
        value
    };
    let value = value.strip_prefix('+').unwrap_or(value);
    if value.is_empty() {
        return None;
    }

    let (digits, radix) = if let Some(digits) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        (digits, 16)
    } else if value.starts_with('0') {
        (value, 8)
    } else {
        (value, 10)
    };
    u32::from_str_radix(digits, radix).ok()
}

pub(crate) fn propagate_apic_frequency(
    cmdline: &mut String,
    frequency_hz: u64,
) -> Result<(), ApicFrequencyError> {
    if !(MIN_APIC_FREQUENCY_HZ..=u64::from(u32::MAX)).contains(&frequency_hz) {
        return Err(ApicFrequencyError::OutOfRange(frequency_hz));
    }
    Ok(propagate_frequency_parameter(
        cmdline,
        LAPIC_TIMER_HZ,
        frequency_hz,
    )?)
}

fn propagate_frequency_parameter(
    cmdline: &mut String,
    parameter_name: &str,
    frequency: u64,
) -> Result<(), FrequencyParameterError> {
    let canonical_parameter = format!("{parameter_name}={frequency}");

    let mut parameter = None;
    let mut delimiter_offset = None;
    let mut discovery_offset = None;
    let (tokens, unterminated_quote_offset) = command_line_tokens(cmdline);
    for (offset, raw_token) in tokens {
        let token_length = raw_token.len();
        let token = raw_token.strip_prefix('"').map_or(raw_token, |token| {
            if token
                .split_once('=')
                .is_some_and(|(_name, value)| value.starts_with('"'))
            {
                token
            } else {
                token.strip_suffix('"').unwrap_or(token)
            }
        });
        if token == "--" {
            delimiter_offset = Some(offset);
            break;
        }
        if discovery_offset.is_none() && token.starts_with(VIRTIO_MMIO_DEVICE) {
            discovery_offset = Some(offset);
        }
        let name = token.split_once('=').map_or(token, |(name, _value)| name);
        if linux_parameter_name_matches(name, parameter_name) {
            if parameter.is_some() {
                return Err(FrequencyParameterError::Duplicate);
            }
            parameter = Some((offset..offset + token_length, token.to_owned()));
        }
    }

    if let Some((range, parameter)) = parameter {
        let Some((_name, value)) = parameter.split_once('=') else {
            return Err(FrequencyParameterError::Malformed(parameter));
        };
        let specified = parse_linux_uint(value)
            .map(u64::from)
            .ok_or_else(|| FrequencyParameterError::Malformed(parameter.clone()))?;
        if specified != frequency {
            return Err(FrequencyParameterError::Mismatch {
                specified,
                reported: frequency,
            });
        }
        cmdline.replace_range(range, &canonical_parameter);
        return Ok(());
    }

    if let Some(offset) = discovery_offset
        .or(delimiter_offset)
        .or(unterminated_quote_offset)
    {
        cmdline.insert_str(offset, &format!("{canonical_parameter} "));
        return Ok(());
    }
    if !cmdline.is_empty()
        && !cmdline
            .as_bytes()
            .last()
            .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        cmdline.push(' ');
    }
    cmdline.push_str(&canonical_parameter);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn propagates_backend_apic_frequency() {
        let mut cmdline = "console=hvc0 -- tenant".to_owned();
        propagate_apic_frequency(&mut cmdline, 200_000_000).unwrap();
        assert_eq!(cmdline, "console=hvc0 lapic_timer_hz=200000000 -- tenant");

        let mut discovery = "console=hvc0 virtio_mmio.device=0x1000@0xd0002000:7".to_owned();
        propagate_apic_frequency(&mut discovery, 200_000_000).unwrap();
        assert_eq!(
            discovery,
            "console=hvc0 lapic_timer_hz=200000000 virtio_mmio.device=0x1000@0xd0002000:7"
        );
    }

    #[test]
    fn canonicalizes_apic_frequency_using_linux_parameter_rules() {
        for parameter in [
            "lapic_timer_hz=200000000",
            "lapic-timer-hz=200000000",
            "lapic_timer_hz=0xbebc200",
            r#"lapic_timer_hz="200000000""#,
            r#""lapic_timer_hz=200000000""#,
        ] {
            let mut cmdline = format!("console=hvc0 {parameter} -- lapic_timer_hz=1");
            propagate_apic_frequency(&mut cmdline, 200_000_000).unwrap();
            assert_eq!(
                cmdline,
                "console=hvc0 lapic_timer_hz=200000000 -- lapic_timer_hz=1"
            );
        }
    }

    #[test]
    fn rejects_invalid_apic_frequency_without_mutating_command_line() {
        for parameter in [
            "lapic_timer_hz",
            "lapic_timer_hz=",
            "lapic_timer_hz=0",
            "lapic_timer_hz=199999999",
            "lapic_timer_hz=4294967296",
            "lapic_timer_hz=200000000 lapic-timer-hz=200000000",
        ] {
            let mut cmdline = parameter.to_owned();
            let error = propagate_apic_frequency(&mut cmdline, 200_000_000).unwrap_err();
            assert!(error.to_string().contains(LAPIC_TIMER_HZ));
            assert_eq!(cmdline, parameter);
        }
        for frequency in [0, MIN_APIC_FREQUENCY_HZ - 1, u64::from(u32::MAX) + 1] {
            let mut cmdline = "console=hvc0".to_owned();
            assert_eq!(
                propagate_apic_frequency(&mut cmdline, frequency),
                Err(ApicFrequencyError::OutOfRange(frequency))
            );
            assert_eq!(cmdline, "console=hvc0");
        }
        for frequency in [MIN_APIC_FREQUENCY_HZ, u64::from(u32::MAX)] {
            propagate_apic_frequency(&mut String::new(), frequency).unwrap();
        }
    }
}
