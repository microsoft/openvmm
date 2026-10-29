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

    /// Returns whether the clock runs at host UTC's corrected rate, so that
    /// its difference from host UTC changes only when host UTC steps.
    ///
    /// Linux's corrections of `CLOCK_REALTIME`'s rate also apply to
    /// `CLOCK_BOOTTIME`; Windows never corrects its interrupt time.
    pub const fn follows_utc_rate(self) -> bool {
        match self {
            Self::LinuxBoottime => true,
            Self::WindowsInterruptTime => false,
        }
    }
}

/// The host clock kind of this host OS, as [`host_identity`] reports it.
pub const fn host_clock_kind() -> HostClockKind {
    if cfg!(windows) {
        HostClockKind::WindowsInterruptTime
    } else {
        HostClockKind::LinuxBoottime
    }
}

/// The status of host UTC, as the host's time service reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostClockStatus {
    /// The host does not report its clock's status.
    Unknown,
    /// A time service synchronizes host UTC with a reference.
    Synchronized,
    /// No time service synchronizes host UTC, which runs on its own.
    FreeRunning,
    /// The host reports a fault of its clock.
    Unreliable,
}

/// How the host converts its TSC to UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostClockBasis {
    /// The TSC at the rate that the host kernel calibrated, to the kHz: the
    /// `tsc` clocksource of Linux.
    KernelTsc,
    /// The TSC at the hypervisor's rate, to the Hz: the Hyper-V TSC page
    /// clocksource of Linux, and the performance counter of Windows.
    HypervisorTsc,
    /// Another clock, which may not follow the TSC.
    Other,
}

/// What the host's time service makes host UTC do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostClockDiscipline {
    /// The status of host UTC.
    pub status: HostClockStatus,
    /// How much faster than the host's uncorrected clock host UTC runs, in
    /// parts per million scaled by 2^16, the unit of Linux's frequency
    /// correction.
    pub correction: i64,
    /// The time service's bound on the error of host UTC, in microseconds.
    pub max_error_us: Option<u64>,
    /// The clock from which the host derives UTC.
    pub basis: HostClockBasis,
}

/// Reads the discipline of host UTC: on Linux, the kernel's frequency
/// correction and status that `adjtimex` reports, which any time service
/// sets; on Windows, the time adjustment that `GetSystemTimeAdjustmentPrecise`
/// reports. A host that cannot report it reports
/// [`HostClockStatus::Unknown`] and no correction.
pub fn host_clock_discipline() -> HostClockDiscipline {
    sys::clock_discipline()
}

/// Returns how long the host has been suspended since it booted, in
/// nanoseconds: how far the host monotonic clock of [`host_clock_kind`],
/// which counts suspended time, has run ahead of one that does not.
pub fn host_suspended_ns() -> Result<u64, TimeAbiError> {
    sys::suspended_ns()
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

/// One part per million in Linux's frequency correction, which scales it by
/// 2^16: the unit of [`HostClockDiscipline::correction`].
pub const SCALED_PPM: i64 = 1 << 16;

/// Returns the correction of a clock that counts a second for every
/// `adjustment` ticks of a counter that ticks `increment` times a second, as
/// Windows reports its precise time adjustment, in parts per million scaled
/// by 2^16, rounded to the nearest: the clock runs at `increment /
/// adjustment` of the counter's rate, so a larger adjustment slows it.
#[cfg(any(windows, test))]
fn adjustment_correction(adjustment: u64, increment: u64) -> i64 {
    let scale = i128::from(SCALED_PPM) * 1_000_000;
    let numerator = (i128::from(increment) - i128::from(adjustment)) * scale;
    let adjustment = i128::from(adjustment.max(1));
    let rounded = if numerator >= 0 {
        (numerator + adjustment / 2) / adjustment
    } else {
        (numerator - adjustment / 2) / adjustment
    };
    rounded.clamp(i64::MIN.into(), i64::MAX.into()) as i64
}

/// Parses 32 hexadecimal digits, ignoring dashes and a trailing newline, as
/// in `/etc/machine-id`, the Linux boot ID, and the Windows machine GUID.
#[cfg(any(target_os = "linux", windows, test))]
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
    use super::HostClockBasis;
    use super::HostClockDiscipline;
    use super::HostClockKind;
    use super::HostClockStatus;
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

    /// The file that names the clocksource from which Linux derives UTC.
    const CURRENT_CLOCKSOURCE: &str =
        "/sys/devices/system/clocksource/clocksource0/current_clocksource";

    /// Linux's `USER_HZ`, whose ticks `adjtimex` measures: 100 on x86-64.
    const USER_HZ: i64 = 100;

    pub(super) fn clock_discipline() -> HostClockDiscipline {
        let basis = match std::fs::read_to_string(CURRENT_CLOCKSOURCE)
            .as_deref()
            .map(str::trim)
        {
            Ok("tsc") => HostClockBasis::KernelTsc,
            Ok("hyperv_clocksource_tsc_page") => HostClockBasis::HypervisorTsc,
            _ => HostClockBasis::Other,
        };
        // SAFETY: `timex` holds only integers, for which zero is valid.
        let mut timex: libc::timex = unsafe { std::mem::zeroed() };
        // SAFETY: `timex` is a valid, writable timex for the duration of the
        // call, and its zero `modes` only reads the kernel's state.
        if unsafe { libc::adjtimex(&mut timex) } < 0 {
            return HostClockDiscipline {
                status: HostClockStatus::Unknown,
                correction: 0,
                max_error_us: None,
                basis,
            };
        }
        let status = if timex.status & libc::STA_CLOCKERR != 0 {
            HostClockStatus::Unreliable
        } else if timex.status & libc::STA_UNSYNC != 0 {
            HostClockStatus::FreeRunning
        } else {
            HostClockStatus::Synchronized
        };
        // A second of host UTC lasts `tick` microseconds per `USER_HZ` tick,
        // plus the frequency correction.
        let tick_correction = (timex.tick * USER_HZ - 1_000_000) * super::SCALED_PPM;
        HostClockDiscipline {
            status,
            correction: timex.freq.saturating_add(tick_correction),
            max_error_us: u64::try_from(timex.maxerror).ok(),
            basis,
        }
    }

    pub(super) fn suspended_ns() -> Result<u64, TimeAbiError> {
        let monotonic = clock_ns(libc::CLOCK_MONOTONIC, "CLOCK_MONOTONIC")?;
        let boottime = clock_ns(libc::CLOCK_BOOTTIME, "CLOCK_BOOTTIME")?;
        Ok(boottime.saturating_sub(monotonic))
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
    use super::HostClockBasis;
    use super::HostClockDiscipline;
    use super::HostClockKind;
    use super::HostClockStatus;
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
    use windows_sys::Win32::System::SystemInformation::GetSystemTimeAdjustmentPrecise;
    use windows_sys::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
    use windows_sys::Win32::System::WindowsProgramming::QueryInterruptTimePrecise;
    use windows_sys::Win32::System::WindowsProgramming::QueryUnbiasedInterruptTimePrecise;

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

    pub(super) fn clock_discipline() -> HostClockDiscipline {
        let mut adjustment = 0;
        let mut increment = 0;
        let mut disabled = 0;
        // SAFETY: the three outputs are valid, writable values for the
        // duration of the call.
        let read = unsafe {
            GetSystemTimeAdjustmentPrecise(&mut adjustment, &mut increment, &mut disabled)
        };
        let (status, correction) = if read == 0 || increment == 0 {
            (HostClockStatus::Unknown, 0)
        } else if disabled != 0 {
            // No time service adjusts UTC, which follows the performance
            // counter.
            (HostClockStatus::FreeRunning, 0)
        } else if adjustment == 0 {
            (HostClockStatus::Unknown, 0)
        } else {
            // The increment is the performance counter's frequency, and UTC
            // counts a second for every adjustment of its ticks.
            (
                HostClockStatus::Synchronized,
                super::adjustment_correction(adjustment, increment),
            )
        };
        // Under Hyper-V, the performance counter runs at the TSC rate that
        // the hypervisor reports.
        HostClockDiscipline {
            status,
            correction,
            max_error_us: None,
            basis: HostClockBasis::HypervisorTsc,
        }
    }

    pub(super) fn suspended_ns() -> Result<u64, TimeAbiError> {
        let mut unbiased = 0;
        let mut interrupt = 0;
        // SAFETY: `unbiased` and `interrupt` are valid, writable u64 values
        // for the duration of the calls.
        unsafe {
            QueryUnbiasedInterruptTimePrecise(&mut unbiased);
            QueryInterruptTimePrecise(&mut interrupt);
        }
        interrupt
            .saturating_sub(unbiased)
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
    use super::HostClockBasis;
    use super::HostClockDiscipline;
    use super::HostClockStatus;
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

    pub(super) fn clock_discipline() -> HostClockDiscipline {
        HostClockDiscipline {
            status: HostClockStatus::Unknown,
            correction: 0,
            max_error_us: None,
            basis: HostClockBasis::Other,
        }
    }

    pub(super) fn suspended_ns() -> Result<u64, TimeAbiError> {
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

    #[test]
    fn only_linux_boottime_follows_utc_rate() {
        assert!(HostClockKind::LinuxBoottime.follows_utc_rate());
        assert!(!HostClockKind::WindowsInterruptTime.follows_utc_rate());
    }

    #[test]
    fn windows_adjustments_are_scaled_ppm() {
        // Windows's nominal rate: a second of UTC per second of the
        // performance counter.
        assert_eq!(adjustment_correction(10_000_000, 10_000_000), 0);
        assert_eq!(
            adjustment_correction(10_000_000, 10_000_100),
            10 * SCALED_PPM
        );
        assert_eq!(
            adjustment_correction(10_000_000, 9_999_900),
            -10 * SCALED_PPM
        );
        // A larger adjustment counts more of the counter's ticks as a second,
        // so UTC runs slow: a Windows host that reported 10,000,501 for its
        // 10 MHz counter ran UTC 53 ppm slow against the counter, between the
        // -50.1 and -58.8 ppm of its adjustments over the 20 s measured.
        assert_eq!(adjustment_correction(10_000_501, 10_000_000), -3_283_189);
        assert_eq!(
            adjustment_correction(10_000_005, 10_000_000),
            -SCALED_PPM / 2
        );
        // The correction rounds to the nearest scaled unit, and half a unit
        // away from zero.
        assert_eq!(adjustment_correction(1_000_000, 1_000_001), SCALED_PPM);
        assert_eq!(adjustment_correction(156_250, 156_251), 419_430);
        assert_eq!(adjustment_correction(156_250, 156_249), -419_430);
        assert_eq!(adjustment_correction(131_072_000_000, 131_072_000_001), 1);
        assert_eq!(adjustment_correction(131_072_000_000, 131_071_999_999), -1);
        // A zero adjustment, which the caller rejects, does not divide by
        // zero.
        assert_eq!(adjustment_correction(0, 1), SCALED_PPM * 1_000_000);
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn host_clock_discipline_is_available() {
        let discipline = host_clock_discipline();
        assert_ne!(
            discipline.status,
            HostClockStatus::Unknown,
            "{discipline:?}"
        );
        // Linux limits the correction to its tick range and ±500 ppm.
        assert!(
            discipline.correction.unsigned_abs() < 200_000 * SCALED_PPM.unsigned_abs(),
            "{discipline:?}"
        );
        if cfg!(windows) {
            assert_eq!(discipline.basis, HostClockBasis::HypervisorTsc);
            assert_eq!(discipline.max_error_us, None);
        }
        host_suspended_ns().unwrap();
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
