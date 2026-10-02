// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Host time samples and host identities.

// UNSAFETY: Calling host clock and Windows registry APIs.
#![cfg_attr(any(target_os = "linux", windows), expect(unsafe_code))]

use super::TimeAbiCode;
use super::TimeAbiError;
use mesh_protobuf::Protobuf;

/// The host monotonic clock a sample was taken from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Protobuf)]
#[mesh(package = "virt.time_abi")]
pub enum HostClockKind {
    /// Linux `CLOCK_BOOTTIME`.
    #[mesh(1)]
    LinuxBoottime,
    /// Windows `QueryInterruptTimePrecise`, in nanoseconds.
    #[mesh(2)]
    WindowsInterruptTime,
}

impl HostClockKind {
    /// Returns the manifest spelling of the clock kind.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LinuxBoottime => "linux-boottime",
            Self::WindowsInterruptTime => "windows-interrupt-time",
        }
    }

    /// Parses the manifest spelling of a clock kind.
    pub fn from_manifest(value: &str) -> Option<Self> {
        match value {
            "linux-boottime" => Some(Self::LinuxBoottime),
            "windows-interrupt-time" => Some(Self::WindowsInterruptTime),
            _ => None,
        }
    }
}

/// The identity of a host and of its current boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Protobuf)]
#[mesh(package = "virt.time_abi")]
pub struct HostIdentity {
    /// The host identity: `/etc/machine-id` on Linux, `MachineGuid` on
    /// Windows.
    #[mesh(1)]
    pub host_id: [u8; 16],
    /// The boot identity: `/proc/sys/kernel/random/boot_id` on Linux,
    /// `PrefetchParameters\BootId` (zero-extended) on Windows.
    #[mesh(2)]
    pub boot_id: [u8; 16],
    /// The host monotonic clock.
    #[mesh(3)]
    pub clock: HostClockKind,
}

/// A pair of host clock readings taken back to back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostTimeSample {
    /// Host UTC, in nanoseconds since the Unix epoch.
    pub utc_ns: u64,
    /// Host monotonic time, in nanoseconds, from the clock of
    /// [`HostIdentity::clock`].
    pub monotonic_ns: u64,
}

/// Samples host UTC and host monotonic time back to back.
///
/// The monotonic clock is read on both sides of the UTC read and the midpoint
/// is returned, so the pair is consistent to within half the read time.
pub fn sample_host_time() -> Result<HostTimeSample, TimeAbiError> {
    let monotonic_before = sys::monotonic_ns()?;
    let utc_ns = sys::utc_ns()?;
    let monotonic_after = sys::monotonic_ns()?;
    if monotonic_after < monotonic_before {
        return Err(host_error("the host monotonic clock went backwards"));
    }
    Ok(HostTimeSample {
        utc_ns,
        monotonic_ns: monotonic_before + (monotonic_after - monotonic_before) / 2,
    })
}

/// Returns the identity of this host and of its current boot.
pub fn host_identity() -> Result<HostIdentity, TimeAbiError> {
    sys::host_identity()
}

fn host_error(message: impl Into<String>) -> TimeAbiError {
    TimeAbiError::new(TimeAbiCode::HostIdentity, message)
}

/// Parses 32 hexadecimal digits, ignoring dashes and a trailing newline, as
/// in `/etc/machine-id`, the Linux boot ID, and the Windows machine GUID.
fn parse_hex_id(text: &str) -> Option<[u8; 16]> {
    let digits: Vec<u8> = text
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .bytes()
        .filter(|&b| b != b'-')
        .collect();
    if digits.len() != 32 {
        return None;
    }
    let mut id = [0; 16];
    for (i, byte) in id.iter_mut().enumerate() {
        let pair = std::str::from_utf8(&digits[2 * i..2 * i + 2]).ok()?;
        *byte = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(id)
}

/// Returns a 16-byte identity holding `value` little-endian, zero-extended.
#[cfg_attr(not(any(windows, test)), expect(dead_code))]
fn zero_extended_id(value: u32) -> [u8; 16] {
    let mut id = [0; 16];
    id[..4].copy_from_slice(&value.to_le_bytes());
    id
}

#[cfg(target_os = "linux")]
mod sys {
    use super::HostClockKind;
    use super::HostIdentity;
    use super::host_error;
    use super::parse_hex_id;
    use crate::time_abi::TimeAbiError;

    fn clock_ns(clock: libc::clockid_t, name: &str) -> Result<u64, TimeAbiError> {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, writable timespec for the duration of the
        // call.
        let r = unsafe { libc::clock_gettime(clock, &mut ts) };
        if r != 0 {
            return Err(host_error(format!(
                "cannot read {name}: {}",
                std::io::Error::last_os_error()
            )));
        }
        let secs =
            u64::try_from(ts.tv_sec).map_err(|_| host_error(format!("{name} is negative")))?;
        let nanos =
            u64::try_from(ts.tv_nsec).map_err(|_| host_error(format!("{name} is malformed")))?;
        secs.checked_mul(1_000_000_000)
            .and_then(|ns| ns.checked_add(nanos))
            .ok_or_else(|| host_error(format!("{name} overflows")))
    }

    pub(super) fn utc_ns() -> Result<u64, TimeAbiError> {
        clock_ns(libc::CLOCK_REALTIME, "CLOCK_REALTIME")
    }

    pub(super) fn monotonic_ns() -> Result<u64, TimeAbiError> {
        clock_ns(libc::CLOCK_BOOTTIME, "CLOCK_BOOTTIME")
    }

    fn read_id(path: &str) -> Result<[u8; 16], TimeAbiError> {
        use std::io::Read;

        // The identity files hold 33 to 37 bytes, which one read returns. Read
        // them into a stack buffer with no metadata query: every system call
        // is costly in a Hyper-V root partition.
        let mut buf = [0; 64];
        let mut file = std::fs::File::open(path)
            .map_err(|err| host_error(format!("cannot read {path}: {err}")))?;
        let len = loop {
            match file.read(&mut buf) {
                Ok(len) => break len,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => return Err(host_error(format!("cannot read {path}: {err}"))),
            }
        };
        std::str::from_utf8(&buf[..len])
            .ok()
            .and_then(parse_hex_id)
            .ok_or_else(|| host_error(format!("{path} is malformed")))
    }

    pub(super) fn host_identity() -> Result<HostIdentity, TimeAbiError> {
        Ok(HostIdentity {
            host_id: read_id("/etc/machine-id")?,
            boot_id: read_id("/proc/sys/kernel/random/boot_id")?,
            clock: HostClockKind::LinuxBoottime,
        })
    }
}

#[cfg(windows)]
mod sys {
    use super::HostClockKind;
    use super::HostIdentity;
    use super::host_error;
    use super::parse_hex_id;
    use super::zero_extended_id;
    use crate::time_abi::TimeAbiError;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;
    use windows_sys::Win32::System::Registry::RRF_RT_REG_DWORD;
    use windows_sys::Win32::System::Registry::RRF_RT_REG_SZ;
    use windows_sys::Win32::System::Registry::RegGetValueW;
    use windows_sys::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
    use windows_sys::Win32::System::WindowsProgramming::QueryInterruptTimePrecise;

    /// 100 ns intervals from 1601-01-01 to 1970-01-01.
    const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

    pub(super) fn utc_ns() -> Result<u64, TimeAbiError> {
        let mut time = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        // SAFETY: `time` is a valid, writable FILETIME for the duration of
        // the call.
        unsafe { GetSystemTimePreciseAsFileTime(&mut time) };
        let intervals = (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
        intervals
            .checked_sub(FILETIME_UNIX_EPOCH)
            .and_then(|intervals| intervals.checked_mul(100))
            .ok_or_else(|| host_error("host UTC is outside the supported range"))
    }

    pub(super) fn monotonic_ns() -> Result<u64, TimeAbiError> {
        let mut intervals = 0;
        // SAFETY: `intervals` is a valid, writable u64 for the duration of
        // the call.
        unsafe { QueryInterruptTimePrecise(&mut intervals) };
        intervals
            .checked_mul(100)
            .ok_or_else(|| host_error("host interrupt time overflows"))
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    fn read_value(
        key: &str,
        value: &str,
        flags: u32,
        data: &mut [u8],
    ) -> Result<usize, TimeAbiError> {
        let key_wide = wide(key);
        let value_wide = wide(value);
        let mut len = data.len() as u32;
        // SAFETY: the key and value names are NUL-terminated UTF-16 strings,
        // and `data` is writable for `len` bytes, all for the duration of the
        // call.
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key_wide.as_ptr(),
                value_wide.as_ptr(),
                flags,
                std::ptr::null_mut(),
                data.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(host_error(format!(
                "cannot read registry value HKLM\\{key}\\{value}: {}",
                std::io::Error::from_raw_os_error(status as i32)
            )));
        }
        Ok(len as usize)
    }

    fn machine_guid() -> Result<[u8; 16], TimeAbiError> {
        const KEY: &str = r"SOFTWARE\Microsoft\Cryptography";
        let mut data = [0_u8; 128];
        let len = read_value(KEY, "MachineGuid", RRF_RT_REG_SZ, &mut data)?;
        let units: Vec<u16> = (0..len.min(data.len()) / 2)
            .map(|i| u16::from_le_bytes([data[2 * i], data[2 * i + 1]]))
            .take_while(|&unit| unit != 0)
            .collect();
        let text = String::from_utf16_lossy(&units);
        parse_hex_id(&text).ok_or_else(|| host_error("MachineGuid is malformed"))
    }

    fn boot_id() -> Result<[u8; 16], TimeAbiError> {
        const KEY: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management\PrefetchParameters";
        let mut data = [0_u8; 4];
        let len = read_value(KEY, "BootId", RRF_RT_REG_DWORD, &mut data)?;
        if len != data.len() {
            return Err(host_error("BootId is malformed"));
        }
        Ok(zero_extended_id(u32::from_le_bytes(data)))
    }

    pub(super) fn host_identity() -> Result<HostIdentity, TimeAbiError> {
        Ok(HostIdentity {
            host_id: machine_guid()?,
            boot_id: boot_id()?,
            clock: HostClockKind::WindowsInterruptTime,
        })
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod sys {
    use super::HostIdentity;
    use super::host_error;
    use crate::time_abi::TimeAbiError;

    pub(super) fn utc_ns() -> Result<u64, TimeAbiError> {
        Err(host_error(
            "host time sampling is not supported on this host",
        ))
    }

    pub(super) fn monotonic_ns() -> Result<u64, TimeAbiError> {
        Err(host_error(
            "host time sampling is not supported on this host",
        ))
    }

    pub(super) fn host_identity() -> Result<HostIdentity, TimeAbiError> {
        Err(host_error("host identity is not supported on this host"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_ids() {
        assert_eq!(
            parse_hex_id("0123456789abcdef0123456789ABCDEF\n"),
            Some([
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
                0xcd, 0xef
            ])
        );
        assert_eq!(
            parse_hex_id("01234567-89ab-cdef-0123-456789abcdef"),
            parse_hex_id("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            parse_hex_id("{01234567-89ab-cdef-0123-456789abcdef}"),
            parse_hex_id("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(parse_hex_id("0123"), None);
        assert_eq!(parse_hex_id("0123456789abcdef0123456789abcdeg"), None);
        assert_eq!(parse_hex_id("0123456789abcdef0123456789abcdef00"), None);
    }

    #[test]
    fn zero_extension() {
        assert_eq!(
            zero_extended_id(6),
            [6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn clock_kind_spelling() {
        for kind in [
            HostClockKind::LinuxBoottime,
            HostClockKind::WindowsInterruptTime,
        ] {
            assert_eq!(HostClockKind::from_manifest(kind.as_str()), Some(kind));
        }
        assert_eq!(HostClockKind::from_manifest("utc"), None);
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn host_time_samples_advance() {
        let first = sample_host_time().unwrap();
        let second = sample_host_time().unwrap();
        // After 2020-01-01.
        assert!(first.utc_ns > 1_577_836_800_000_000_000);
        assert!(second.monotonic_ns >= first.monotonic_ns);
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    #[ignore = "reads the host identity, which containers may lack"]
    fn host_identity_is_available() {
        let first = host_identity().unwrap();
        assert_ne!(first.host_id, [0; 16]);
        assert_eq!(host_identity().unwrap(), first);
    }
}
