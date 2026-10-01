// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows host OS information from the registry.

// UNSAFETY: Calling the Win32 registry API.
#![expect(unsafe_code)]

use super::HostOs;
use super::OsInfo;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;
use windows_sys::Win32::System::Registry::RRF_RT_REG_BINARY;
use windows_sys::Win32::System::Registry::RRF_RT_REG_DWORD;
use windows_sys::Win32::System::Registry::RRF_RT_REG_SZ;
use windows_sys::Win32::System::Registry::RegGetValueW;

const CURRENT_VERSION_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
const PROCESSOR0_KEY: &str = r"HARDWARE\DESCRIPTION\System\CentralProcessor\0";

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// Reads the value `value` of the `HKEY_LOCAL_MACHINE` subkey `subkey`, if it
/// exists and has a type that `flags` allows.
fn read_value(subkey: &str, value: &str, flags: u32) -> Option<Vec<u8>> {
    let subkey = wide(subkey);
    let value = wide(value);
    let mut len = 0_u32;
    // SAFETY: The key and value names are NUL-terminated UTF-16 strings that
    // outlive the call, and a null data pointer queries only the size.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            flags,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut len,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let mut data = vec![0_u8; len as usize];
    // SAFETY: `data` is writable for `len` bytes, and the names are as
    // above.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            flags,
            std::ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    data.truncate(len as usize);
    Some(data)
}

fn read_string(subkey: &str, value: &str) -> Option<String> {
    let data = read_value(subkey, value, RRF_RT_REG_SZ)?;
    let wide = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .take_while(|&unit| unit != 0)
        .collect::<Vec<_>>();
    let text = String::from_utf16_lossy(&wide).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

fn read_dword(subkey: &str, value: &str) -> Option<u32> {
    let data = read_value(subkey, value, RRF_RT_REG_DWORD)?;
    Some(u32::from_le_bytes(data.get(..4)?.try_into().ok()?))
}

pub(super) fn os_info() -> OsInfo {
    let major = read_dword(CURRENT_VERSION_KEY, "CurrentMajorVersionNumber");
    let minor = read_dword(CURRENT_VERSION_KEY, "CurrentMinorVersionNumber");
    let build = read_string(CURRENT_VERSION_KEY, "CurrentBuildNumber");
    let revision = read_dword(CURRENT_VERSION_KEY, "UBR");
    let release = match (major, minor, build) {
        (Some(major), Some(minor), Some(build)) => Some(match revision {
            Some(revision) => format!("{major}.{minor}.{build}.{revision}"),
            None => format!("{major}.{minor}.{build}"),
        }),
        _ => None,
    };
    let version = match (
        read_string(CURRENT_VERSION_KEY, "ProductName"),
        read_string(CURRENT_VERSION_KEY, "DisplayVersion"),
    ) {
        (Some(product), Some(display)) => Some(format!("{product} {display}")),
        (product, display) => product.or(display),
    };
    let microcode = read_value(PROCESSOR0_KEY, "Update Revision", RRF_RT_REG_BINARY)
        .and_then(|data| microcode_revision(&data))
        .into_iter()
        .collect();
    OsInfo {
        os: Some(HostOs {
            kind: std::env::consts::OS.to_owned(),
            release,
            version,
            cpu_flags: Vec::new(),
            clocksource: None,
            available_clocksources: Vec::new(),
        }),
        microcode,
    }
}

/// Decodes the processor's `Update Revision` registry value. Older Windows
/// releases store the 64-bit microcode signature MSR, with the Intel revision
/// in the high half and the AMD revision in the low half; newer releases
/// store the 32-bit revision itself.
fn microcode_revision(data: &[u8]) -> Option<String> {
    let revision = match data.len() {
        4 => u64::from(u32::from_le_bytes(data.try_into().ok()?)),
        8 => {
            let signature = u64::from_le_bytes(data.try_into().ok()?);
            match signature >> 32 {
                0 => signature & 0xffff_ffff,
                high => high,
            }
        }
        _ => return None,
    };
    Some(format!("{revision:#x}"))
}

#[cfg(test)]
mod tests {
    use super::microcode_revision;
    use test_with_tracing::test;

    #[test]
    fn decodes_microcode_revision() {
        let intel = 0x0200_7006_0000_0000_u64.to_le_bytes();
        assert_eq!(microcode_revision(&intel).as_deref(), Some("0x2007006"));
        let amd = 0x0a10_1148_u64.to_le_bytes();
        assert_eq!(microcode_revision(&amd).as_deref(), Some("0xa101148"));
        assert_eq!(
            microcode_revision(&[0x3b, 0x04, 0, 0]).as_deref(),
            Some("0x43b")
        );
        assert_eq!(microcode_revision(&[1, 2, 3]), None);
        assert_eq!(microcode_revision(&[0; 9]), None);
    }
}
