// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The TSC rate policy and the LAPIC rate rule.

use super::TimeAbiCode;
use super::TimeAbiError;

/// The accepted deviation of the destination TSC rate from the declared one.
pub const TSC_TOLERANCE_PPM: u32 = 250;
/// The lowest plausible TSC rate.
pub const MIN_TSC_HZ: u64 = 500_000_000;
/// The highest plausible TSC rate.
pub const MAX_TSC_HZ: u64 = 10_000_000_000;
/// The LAPIC timer rate on KVM.
pub const LAPIC_HZ_KVM: u64 = 1_000_000_000;
/// The LAPIC timer rate on MSHV and WHP.
pub const LAPIC_HZ_HYPERV: u64 = 200_000_000;
/// The largest LAPIC divide-configuration value.
pub const MAX_LAPIC_DIVIDE: u32 = 128;

/// Checks that `hz` is a plausible TSC rate.
pub fn check_plausible_tsc_hz(hz: u64) -> Result<(), TimeAbiError> {
    if !(MIN_TSC_HZ..=MAX_TSC_HZ).contains(&hz) {
        return Err(TimeAbiError::new(
            TimeAbiCode::TscRateImplausible,
            format!("TSC rate {hz} Hz is outside {MIN_TSC_HZ} to {MAX_TSC_HZ} Hz"),
        ));
    }
    Ok(())
}

/// Returns the LAPIC timer rate of the backend with hypervisor ID
/// `hypervisor` (`kvm`, `mshv`, or `whp`).
pub fn backend_lapic_hz(hypervisor: &str) -> Result<u64, TimeAbiError> {
    match hypervisor {
        "kvm" => Ok(LAPIC_HZ_KVM),
        "mshv" | "whp" => Ok(LAPIC_HZ_HYPERV),
        _ => Err(TimeAbiError::new(
            TimeAbiCode::LapicRateUnavailable,
            format!("backend '{hypervisor}' has no time ABI LAPIC rate"),
        )),
    }
}

/// Checks a backend's reported LAPIC rate against its constant and, on
/// restore, against the saved rate.
pub fn check_lapic_hz(
    hypervisor: &str,
    actual: u64,
    saved: Option<u64>,
) -> Result<(), TimeAbiError> {
    let expected = backend_lapic_hz(hypervisor)?;
    if actual != expected {
        return Err(TimeAbiError::new(
            TimeAbiCode::LapicRateMismatch,
            format!(
                "backend '{hypervisor}' reports LAPIC rate {actual} Hz, expected {expected} Hz"
            ),
        ));
    }
    if let Some(saved) = saved {
        if saved != actual {
            return Err(TimeAbiError::new(
                TimeAbiCode::LapicRateMismatch,
                format!(
                    "snapshot LAPIC rate {saved} Hz differs from the destination's {actual} Hz"
                ),
            ));
        }
    }
    Ok(())
}

/// The outcome of an accepted TSC rate check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateCheck {
    /// The declared rate `F_s`, in Hz.
    pub declared_hz: u64,
    /// The destination's native rate `F_d`, in Hz.
    pub native_hz: u64,
    /// `(F_d - F_s) / F_s` in parts per billion, rounded to nearest, for logs.
    pub deviation_ppb: i64,
    /// The restore packet's rate deviation: `(F_d - F_s) / F_s` in ppm
    /// scaled by 2^16, rounded to nearest.
    pub rate_deviation: i32,
}

/// Applies the rate policy: accepts iff
/// `|F_d - F_s| * 1_000_000 <= TSC_TOLERANCE_PPM * F_s`, boundary included.
pub fn check_tsc_rate(declared_hz: u64, native_hz: u64) -> Result<RateCheck, TimeAbiError> {
    check_plausible_tsc_hz(declared_hz)?;
    check_plausible_tsc_hz(native_hz)?;
    let declared = i128::from(declared_hz);
    let difference = i128::from(native_hz) - declared;
    if difference.abs() * 1_000_000 > i128::from(TSC_TOLERANCE_PPM) * declared {
        return Err(TimeAbiError::new(
            TimeAbiCode::TscRateTolerance,
            format!(
                "destination TSC rate {native_hz} Hz deviates from the declared {declared_hz} Hz by {} ppb, beyond {TSC_TOLERANCE_PPM} ppm",
                div_round_nearest(difference * 1_000_000_000, declared)
            ),
        ));
    }
    // Both values are bounded by the tolerance check above.
    let deviation_ppb = div_round_nearest(difference * 1_000_000_000, declared) as i64;
    let rate_deviation = div_round_nearest(difference * 1_000_000 * 65_536, declared) as i32;
    Ok(RateCheck {
        declared_hz,
        native_hz,
        deviation_ppb,
        rate_deviation,
    })
}

/// Applies the `dest-rate-offset-ppm` test hook to a measured native rate.
pub fn apply_rate_offset(native_hz: u64, offset_ppm: i32) -> Result<u64, TimeAbiError> {
    let native = i128::from(native_hz);
    let offset = div_round_nearest(native * i128::from(offset_ppm), 1_000_000);
    u64::try_from(native + offset).map_err(|_| {
        TimeAbiError::new(
            TimeAbiCode::TestHook,
            format!("rate offset {offset_ppm} ppm cannot apply to {native_hz} Hz"),
        )
    })
}

/// Returns the LAPIC ticks of a counting-mode one-shot timer over
/// `downtime_ns`: `floor(D * L / 1_000_000_000 / divide)`.
///
/// `divide` is the divide-configuration value, a power of two from 1 to
/// [`MAX_LAPIC_DIVIDE`]; other values return `None`.
pub fn lapic_ticks(downtime_ns: u64, apic_hz: u64, divide: u32) -> Option<u64> {
    if !divide.is_power_of_two() || divide > MAX_LAPIC_DIVIDE {
        return None;
    }
    let ticks =
        u128::from(downtime_ns) * u128::from(apic_hz) / (1_000_000_000 * u128::from(divide));
    u64::try_from(ticks).ok()
}

/// Divides with rounding to nearest, ties away from zero. `den` must be
/// positive.
fn div_round_nearest(num: i128, den: i128) -> i128 {
    debug_assert!(den > 0);
    let magnitude = (num.abs() + den / 2) / den;
    if num < 0 { -magnitude } else { magnitude }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plausibility_bounds_are_inclusive() {
        check_plausible_tsc_hz(MIN_TSC_HZ).unwrap();
        check_plausible_tsc_hz(MAX_TSC_HZ).unwrap();
        for hz in [0, MIN_TSC_HZ - 1, MAX_TSC_HZ + 1] {
            assert_eq!(
                check_plausible_tsc_hz(hz).unwrap_err().code,
                TimeAbiCode::TscRateImplausible
            );
        }
    }

    #[test]
    fn tolerance_boundary_is_accepted() {
        let declared = 2_000_000_000;
        // 250 ppm of 2 GHz is exactly 500 kHz.
        let fast = check_tsc_rate(declared, declared + 500_000).unwrap();
        assert_eq!(fast.rate_deviation, 250 * 65_536);
        assert_eq!(fast.deviation_ppb, 250_000);
        let slow = check_tsc_rate(declared, declared - 500_000).unwrap();
        assert_eq!(slow.rate_deviation, -250 * 65_536);
        for native in [declared + 500_001, declared - 500_001] {
            assert_eq!(
                check_tsc_rate(declared, native).unwrap_err().code,
                TimeAbiCode::TscRateTolerance
            );
        }
    }

    #[test]
    fn rate_deviation_rounds_to_nearest() {
        let declared = 2_100_000_000;
        let same = check_tsc_rate(declared, declared).unwrap();
        assert_eq!((same.rate_deviation, same.deviation_ppb), (0, 0));
        // 1 Hz of 2.1 GHz is 31.2076... in units of 2^-16 ppm.
        assert_eq!(
            check_tsc_rate(declared, declared + 1)
                .unwrap()
                .rate_deviation,
            31
        );
        assert_eq!(
            check_tsc_rate(declared, declared - 1)
                .unwrap()
                .rate_deviation,
            -31
        );
        // 200 ppm of 2.1 GHz.
        let hooked = check_tsc_rate(declared, declared + 420_000).unwrap();
        assert_eq!(hooked.rate_deviation, 200 * 65_536);
    }

    #[test]
    fn rate_check_rejects_implausible_rates() {
        assert_eq!(
            check_tsc_rate(1_000, 1_000).unwrap_err().code,
            TimeAbiCode::TscRateImplausible
        );
    }

    #[test]
    fn rate_offset_hook() {
        assert_eq!(
            apply_rate_offset(2_000_000_000, 251).unwrap(),
            2_000_502_000
        );
        assert_eq!(
            apply_rate_offset(2_000_000_000, -200).unwrap(),
            1_999_600_000
        );
        assert_eq!(apply_rate_offset(2_000_000_000, 0).unwrap(), 2_000_000_000);
        let rejected = apply_rate_offset(2_000_000_000, 251).unwrap();
        assert_eq!(
            check_tsc_rate(2_000_000_000, rejected).unwrap_err().code,
            TimeAbiCode::TscRateTolerance
        );
    }

    #[test]
    fn lapic_rates() {
        assert_eq!(backend_lapic_hz("kvm").unwrap(), LAPIC_HZ_KVM);
        assert_eq!(backend_lapic_hz("mshv").unwrap(), LAPIC_HZ_HYPERV);
        assert_eq!(backend_lapic_hz("whp").unwrap(), LAPIC_HZ_HYPERV);
        assert_eq!(
            backend_lapic_hz("hvf").unwrap_err().code,
            TimeAbiCode::LapicRateUnavailable
        );
        check_lapic_hz("kvm", LAPIC_HZ_KVM, None).unwrap();
        check_lapic_hz("whp", LAPIC_HZ_HYPERV, Some(LAPIC_HZ_HYPERV)).unwrap();
        assert_eq!(
            check_lapic_hz("whp", LAPIC_HZ_KVM, None).unwrap_err().code,
            TimeAbiCode::LapicRateMismatch
        );
        assert_eq!(
            check_lapic_hz("mshv", LAPIC_HZ_HYPERV, Some(LAPIC_HZ_KVM))
                .unwrap_err()
                .code,
            TimeAbiCode::LapicRateMismatch
        );
    }

    #[test]
    fn lapic_tick_rule() {
        // One second at 200 MHz with divide 1 and 16.
        assert_eq!(
            lapic_ticks(1_000_000_000, LAPIC_HZ_HYPERV, 1),
            Some(200_000_000)
        );
        assert_eq!(
            lapic_ticks(1_000_000_000, LAPIC_HZ_HYPERV, 16),
            Some(12_500_000)
        );
        // Floors partial ticks.
        assert_eq!(lapic_ticks(4, LAPIC_HZ_HYPERV, 1), Some(0));
        assert_eq!(lapic_ticks(5, LAPIC_HZ_HYPERV, 1), Some(1));
        assert_eq!(lapic_ticks(1_999, LAPIC_HZ_KVM, 2), Some(999));
        // Thirty days at 1 GHz.
        let thirty_days = 30 * 24 * 60 * 60 * 1_000_000_000;
        assert_eq!(lapic_ticks(thirty_days, LAPIC_HZ_KVM, 1), Some(thirty_days));
        assert_eq!(lapic_ticks(1, LAPIC_HZ_KVM, 3), None);
        assert_eq!(lapic_ticks(1, LAPIC_HZ_KVM, 256), None);
        assert_eq!(lapic_ticks(1, LAPIC_HZ_KVM, 0), None);
    }
}
