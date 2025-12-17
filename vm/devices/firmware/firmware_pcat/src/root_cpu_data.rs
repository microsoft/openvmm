// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Subset of functionality from `RootCpuData.cpp/h` in the OS repo.
//!
//! The original code mixes a few different concepts:
//! - Reading raw SMBIOS tables from the host via Windows APIs
//! - Extracting interesting SMBIOS strings from said tables
//! - Constructing _both_ UEFI structures _and_ PCAT SMBIOS structures
//!
//! This code only deals with that latter-most usecase, and only for PCAT,
//! deferring the act of obtaining interesting SMBIOS information up-the-stack.
//!
//! On Underhill, this information is generated host-side, and sent over the
//! GET, whereas on OpenVMM, it will need to be fetched from the Host itself.

use super::config::SmbiosProcessorInfoBundle;
use core::mem::size_of;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// reSearch query: `RootCpuData::GetVpDmiInfo`
pub(crate) fn get_vp_dmi_info(
    processor_info: Option<&SmbiosProcessorInfoBundle>,
    processor_manufacturer: &[u8],
    processor_version: &[u8],
) -> SmbiosCpuInfoLegacy {
    const SMBIOS_CPU_STRUCT_TYPE: u8 = 4;
    const SMBIOS_PROCESSOR_CENTRAL: u8 = 0x3;
    const SMBIOS_PROCESSOR_UNKNOWN: u8 = 0x2;
    const SMBIOS_PROCESSOR_ID_UNKNOWN: u64 = 0;
    const SMBIOS_PROCESSOR_VOLTAGE_5V: u8 = 1;
    const SMBIOS_PROCESSOR_NO_UPGRADE: u8 = 0x6;
    const SMBIOS_PROCESSOR_NO_SPEED: u16 = 0;
    const SMBIOS_PROCESSOR_ENABLED: u8 = 0x41;
    const SMBIOS_CPU_NO_CACHE_INFO: u16 = 0xFFFF;

    let processor_info = processor_info.unwrap_or(&SmbiosProcessorInfoBundle {
        processor_family: SMBIOS_PROCESSOR_UNKNOWN,
        voltage: SMBIOS_PROCESSOR_VOLTAGE_5V,
        external_clock: SMBIOS_PROCESSOR_NO_SPEED,
        max_speed: SMBIOS_PROCESSOR_NO_SPEED,
        current_speed: SMBIOS_PROCESSOR_NO_SPEED,
    });

    SmbiosCpuInfoLegacy {
        formatted: SmbiosCpuInfoFormatted {
            header: SmbiosHeader {
                structure_type: SMBIOS_CPU_STRUCT_TYPE,
                length: size_of::<SmbiosCpuInfoFormatted>() as u8,
                handle: 0,
            },
            socket_designation: 1, // hardcoded to SMBIOS_NONE_STRING
            processor_type: SMBIOS_PROCESSOR_CENTRAL,
            processor_family: processor_info.processor_family,
            processor_manufacturer: 2,
            processor_id: SMBIOS_PROCESSOR_ID_UNKNOWN,
            processor_version: 3,
            voltage: processor_info.voltage,
            external_clock: processor_info.external_clock,
            max_speed: processor_info.max_speed,
            current_speed: processor_info.current_speed,
            status: SMBIOS_PROCESSOR_ENABLED,
            upgrade: SMBIOS_PROCESSOR_NO_UPGRADE,
            l1_handle: SMBIOS_CPU_NO_CACHE_INFO,
            l2_handle: SMBIOS_CPU_NO_CACHE_INFO,
            l3_handle: SMBIOS_CPU_NO_CACHE_INFO,
            serial_number: 1, // hardcoded to SMBIOS_NONE_STRING
            asset_tag: 1,     // hardcoded to SMBIOS_NONE_STRING
            part_number: 1,   // hardcoded to SMBIOS_NONE_STRING
        },
        unformatted: SmbiosCpuInfoStringsLegacy {
            string_table: {
                let mut string_table = [0x20; MAX_SMBIOS_STRING_TABLE_LEGACY_LENGTH];
                string_table[MAX_SMBIOS_STRING_TABLE_LEGACY_LENGTH - 2..].fill(0);
                {
                    fn write_to_prefix<'a>(buf: &'a mut [u8], src: &[u8]) -> &'a mut [u8] {
                        let (dst, rest) = buf.split_at_mut(src.len());
                        dst.copy_from_slice(src);
                        rest
                    }

                    fn write_string<'a>(mut buf: &'a mut [u8], src: &[u8]) -> &'a mut [u8] {
                        if src.is_empty() {
                            buf = write_to_prefix(buf, SMBIOS_NONE_STRING)
                        } else {
                            buf = write_to_prefix(buf, src);
                            if !src.ends_with(&[0]) {
                                buf = write_to_prefix(buf, &[0]);
                            }
                        }
                        buf
                    }

                    let mut s = &mut string_table[..];
                    s = write_to_prefix(s, SMBIOS_NONE_STRING);
                    s = write_string(s, processor_manufacturer);
                    write_string(s, processor_version);
                }
                string_table
            },
        },
    }
}

// reSearch query: `SMBIOS_HEADER`
#[repr(C, packed)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
struct SmbiosHeader {
    structure_type: u8,
    length: u8,
    handle: u16,
}

/// SMBIOS v2.4 CPU Information structure.
///
/// See <https://www.dmtf.org/sites/default/files/standards/documents/DSP0134_3.2.0.pdf>,
/// section 7.5 Processor Information (Type 4)
///
/// reSearch query: `SMBIOS_CPU_INFO_FORMATTED`
#[repr(C, packed)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
struct SmbiosCpuInfoFormatted {
    header: SmbiosHeader,
    socket_designation: u8,
    processor_type: u8,
    processor_family: u8,
    processor_manufacturer: u8,
    processor_id: u64,
    processor_version: u8,
    voltage: u8,
    external_clock: u16,
    max_speed: u16,
    current_speed: u16,
    status: u8,
    upgrade: u8,
    l1_handle: u16,
    l2_handle: u16,
    l3_handle: u16,
    serial_number: u8,
    asset_tag: u8,
    part_number: u8,
}

// reSearch query: _name is identical_
const SMBIOS_NONE_STRING: &[u8] = b"None\0";
const MAX_SMBIOS_STRING_LENGTH: usize = 64;
const MAX_SMBIOS_STRING_LENGTH_LEGACY_MFR: usize = 16;

/// CPU Information structure string table for legacy BIOS.
///
/// Sized for:
///  1 "None" strings.
///  2 strings obtained from host; the mfr string is max 16 chars
///   the version string is max 64 chars.
///  1 empty string to terminate the table.
const MAX_SMBIOS_STRING_TABLE_LEGACY_LENGTH: usize = SMBIOS_NONE_STRING.len()
    + (MAX_SMBIOS_STRING_LENGTH_LEGACY_MFR + 1)
    + (MAX_SMBIOS_STRING_LENGTH + 1)
    + 1;

/// CPU Information structure string table.
///
/// Sized for:
///  4 "None" strings.
///  2 strings obtained from host that are max 64 chars each.
///  1 empty string to terminate the table.
const MAX_SMBIOS_STRING_TABLE_LENGTH: usize =
    (4 * SMBIOS_NONE_STRING.len()) + ((MAX_SMBIOS_STRING_LENGTH + 1) * 2) + 1;

/// reSearch query: `SMBIOS_CPU_INFO_STRINGS_LEGACY`
#[repr(C, packed)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
// TODO: zerocopy: remove `pub(crate)` once this issue is resolved: https://github.com/google/zerocopy/issues/2177 (https://github.com/microsoft/openvmm/issues/759)
pub(crate) struct SmbiosCpuInfoStringsLegacy {
    string_table: [u8; MAX_SMBIOS_STRING_TABLE_LEGACY_LENGTH],
}

/// reSearch query: `SMBIOS_CPU_INFO_STRINGS`
#[repr(C, packed)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
struct SmbiosCpuInfoStrings {
    string_table: [u8; MAX_SMBIOS_STRING_TABLE_LENGTH],
}

static_assertions::const_assert!(
    size_of::<SmbiosCpuInfoStrings>() > size_of::<SmbiosCpuInfoStringsLegacy>()
);

/// reSearch query: `SMBIOS_CPU_INFORMATION_LEGACY`
#[repr(C, packed)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
pub(crate) struct SmbiosCpuInfoLegacy {
    formatted: SmbiosCpuInfoFormatted,
    unformatted: SmbiosCpuInfoStringsLegacy,
}

static_assertions::const_assert_eq!(size_of::<SmbiosCpuInfoLegacy>(), 123);
