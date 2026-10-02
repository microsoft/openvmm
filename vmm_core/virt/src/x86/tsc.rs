// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CPUID leaves that expose an exact TSC frequency, for partitions without
//! the NVX time ABI.

use thiserror::Error;
use x86defs::cpuid::CpuidFunction;

/// The backend TSC frequency cannot be represented by CPUID leaf `0x15`.
#[derive(Debug, Error)]
pub enum TscFrequencyCpuidError {
    #[error("TSC frequency must be nonzero")]
    Zero,
    #[error("TSC frequency {0} Hz cannot be represented by CPUID leaf 0x15")]
    Unrepresentable(u64),
}

/// Returns CPUID overrides that expose an exact virtual TSC frequency.
pub fn tsc_frequency_cpuid_leaves(
    frequency_hz: u64,
    current_max_basic_leaf: u32,
) -> Result<[crate::CpuidLeaf; 2], TscFrequencyCpuidError> {
    fn gcd(mut left: u64, mut right: u64) -> u64 {
        while right != 0 {
            (left, right) = (right, left % right);
        }
        left
    }

    if frequency_hz == 0 {
        return Err(TscFrequencyCpuidError::Zero);
    }
    const FALLBACK_CRYSTAL_FREQUENCY_HZ: u64 = 1_000_000_000;
    let (denominator, numerator, crystal_frequency_hz) =
        if let Ok(frequency_hz) = u32::try_from(frequency_hz) {
            (1, 1, frequency_hz)
        } else {
            let divisor = gcd(frequency_hz, FALLBACK_CRYSTAL_FREQUENCY_HZ);
            let numerator = u32::try_from(frequency_hz / divisor)
                .map_err(|_| TscFrequencyCpuidError::Unrepresentable(frequency_hz))?;
            let denominator = u32::try_from(FALLBACK_CRYSTAL_FREQUENCY_HZ / divisor)
                .map_err(|_| TscFrequencyCpuidError::Unrepresentable(frequency_hz))?;
            (denominator, numerator, FALLBACK_CRYSTAL_FREQUENCY_HZ as u32)
        };

    Ok([
        crate::CpuidLeaf::new(
            CpuidFunction::VendorAndMaxFunction.0,
            [
                current_max_basic_leaf.max(CpuidFunction::CoreCrystalClockInformation.0),
                0,
                0,
                0,
            ],
        )
        .masked([u32::MAX, 0, 0, 0]),
        crate::CpuidLeaf::new(
            CpuidFunction::CoreCrystalClockInformation.0,
            [denominator, numerator, crystal_frequency_hz, 0],
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::tsc_frequency_cpuid_leaves;
    use test_with_tracing::test;

    #[test]
    fn tsc_frequency_cpuid_avoids_kernel_overflow() {
        let frequency_hz = 2_194_843_733;
        let leaves = tsc_frequency_cpuid_leaves(frequency_hz, 0).unwrap();
        let [denominator, numerator, crystal_frequency_hz, _] = leaves[1].result;

        assert_eq!(
            u64::from(crystal_frequency_hz) * u64::from(numerator) / u64::from(denominator),
            frequency_hz
        );
        assert!(
            (crystal_frequency_hz / 1000)
                .checked_mul(numerator)
                .is_some()
        );
    }

    #[test]
    fn tsc_frequency_cpuid_supports_large_round_frequency() {
        let frequency_hz = 5_000_000_000;
        let leaves = tsc_frequency_cpuid_leaves(frequency_hz, 0).unwrap();
        let [denominator, numerator, crystal_frequency_hz, _] = leaves[1].result;

        assert_eq!(
            u64::from(crystal_frequency_hz) * u64::from(numerator) / u64::from(denominator),
            frequency_hz
        );
    }
}
