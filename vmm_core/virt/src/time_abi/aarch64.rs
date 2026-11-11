// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The ARM payload of the existing optional CPU/time contract.
//!
//! These primitives program and read back partition configuration. They do
//! not supply a canonical profile catalog or prove effective guest ID/cache
//! conformance; those remain admission requirements, not host-passthrough
//! defaults. ARM snapshots are not supported.

use super::TimeAbiCode;
use super::TimeAbiError;
use super::host::HostTimeSample;
pub use cpu_profile::aarch64::CachePolicy;
pub use cpu_profile::aarch64::CpuIdRegister;
pub use cpu_profile::aarch64::CpuProfile;
pub use cpu_profile::aarch64::Fingerprint;

/// ARM CPU and counter configuration, carried by
/// [`ProtoPartitionConfig::time_abi`](crate::ProtoPartitionConfig::time_abi).
///
/// No profile, counter rate, or host feature set is selected implicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeAbiConfig {
    /// The selected CPU profile.
    pub cpu_profile: CpuProfile,
    /// The declared, unscaled architectural counter rate, in Hz.
    pub counter_hz: u64,
    /// Native backend GPA width observed before profile programming, not the
    /// CPU's architectural address width. Excluded from the guest binding.
    pub backend_usable_gpa_bits: u32,
}

impl TimeAbiConfig {
    /// Validates the payload's shape, not its canonical digest or effective
    /// guest CPU surface.
    pub fn validate(&self) -> Result<(), TimeAbiError> {
        let id = self.cpu_profile.id.as_bytes();
        if id.is_empty()
            || id.len() > 32
            || !id.iter().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'_' | b'.')
            })
            || matches!(self.cpu_profile.id.as_str(), "auto" | "host")
        {
            return Err(TimeAbiError::new(
                TimeAbiCode::ProfileUnknown,
                "ARM requires an explicit lowercase ASCII CPU-profile ID of 1..32 bytes",
            ));
        }
        if self.cpu_profile.sha256 == [0; 32] {
            return Err(TimeAbiError::new(
                TimeAbiCode::ProfileDigest,
                "ARM CPU-profile digest is absent",
            ));
        }
        if self.counter_hz == 0 {
            return Err(TimeAbiError::new(
                TimeAbiCode::ProfileTimeBits,
                "ARM architectural counter frequency is zero",
            ));
        }
        for (index, entry) in self.cpu_profile.id_registers.iter().enumerate() {
            if entry.mask == 0
                || entry.value & !entry.mask != 0
                || matches!(
                    entry.register,
                    aarch64defs::SystemReg::CTR_EL0
                        | aarch64defs::SystemReg::DCZID_EL0
                        | aarch64defs::SystemReg::CLIDR_EL1
                )
                || self.cpu_profile.id_registers[..index]
                    .iter()
                    .any(|prior| prior.register == entry.register)
            {
                return Err(TimeAbiError::new(
                    TimeAbiCode::ProfileUnsupported,
                    format!("invalid or duplicate ARM architectural requirement: {entry:?}"),
                ));
            }
        }
        Ok(())
    }

    /// Encodes the approved draft `nvx-arm64-time-v1` identity. Encoding does
    /// not admit the CPU profile or establish guest-visible conformance.
    pub fn binding(&self, capacity: u32) -> Result<[u8; 96], TimeAbiError> {
        self.validate()?;
        if !matches!(capacity, 1 | 2 | 4 | 8) {
            return Err(TimeAbiError::new(
                TimeAbiCode::ProfileUnsupported,
                format!("ARM time binding does not support processor capacity {capacity}"),
            ));
        }
        let mut bytes = [0; 96];
        bytes[..4].copy_from_slice(b"NVT\0");
        bytes[4] = 1;
        bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
        bytes[12..16].copy_from_slice(&capacity.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.counter_hz.to_le_bytes());
        bytes[24..28].copy_from_slice(&20u32.to_le_bytes());
        let id = self.cpu_profile.id.as_bytes();
        bytes[28..32].copy_from_slice(&(id.len() as u32).to_le_bytes());
        bytes[32..32 + id.len()].copy_from_slice(id);
        bytes[64..].copy_from_slice(&self.cpu_profile.sha256);
        Ok(bytes)
    }
}

/// A reading of an active architectural counter, in `counter_hz` ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CounterReading {
    /// The actual guest `CNTVCT_EL0` value.
    pub ticks: u64,
    /// Changes when partition time resumes or is reset.
    pub epoch: u32,
}

/// A bounded pairing of host UTC with the actual guest counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CounterUtcPair {
    /// The paired counter value.
    pub counter: CounterReading,
    /// Host UTC and monotonic time sampled inside the counter bracket.
    pub time: HostTimeSample,
    /// The maximum counter/UTC pairing error, in nanoseconds.
    pub pairing_ns: u64,
}

/// Maximum accepted counter/UTC pairing uncertainty, in nanoseconds.
pub const MAX_COUNTER_PAIRING_NS: u64 = 100_000;

impl CounterUtcPair {
    /// Pairs a host sample with counter reads on both sides. Includes one
    /// counter tick for quantization and rejects a changed counter epoch,
    /// backwards counter, zero rate, or excessive uncertainty.
    pub fn from_bracket(
        before: CounterReading,
        after: CounterReading,
        time: HostTimeSample,
        counter_hz: u64,
    ) -> Result<Self, TimeAbiError> {
        let invalid = |message| TimeAbiError::new(TimeAbiCode::ProfileTimeBits, message);
        if counter_hz == 0 || before.epoch != after.epoch {
            return Err(invalid(
                "ARM counter rate is zero or its epoch changed during sampling",
            ));
        }
        let width = after
            .ticks
            .checked_sub(before.ticks)
            .ok_or_else(|| invalid("ARM counter moved backwards during sampling"))?;
        let error_ticks = u128::from(width).div_ceil(2) + 1;
        let pairing_ns = (error_ticks * 1_000_000_000).div_ceil(u128::from(counter_hz));
        if pairing_ns > u128::from(MAX_COUNTER_PAIRING_NS) {
            return Err(invalid("ARM counter/UTC pairing exceeds 100000 ns"));
        }
        Ok(Self {
            counter: CounterReading {
                ticks: before.ticks + width / 2,
                epoch: before.epoch,
            },
            time,
            pairing_ns: pairing_ns as u64,
        })
    }
}

/// ARM backend primitives returned through the existing optional
/// [`Partition::time_abi`](crate::Partition::time_abi) interface.
pub trait TimeAbiBackend: Send + Sync {
    /// The architecture-semantic payload requested by the caller.
    /// Backend configuration readback is not complete effective ID/cache proof.
    fn configuration(&self) -> &TimeAbiConfig;

    /// Reads an effective partition-wide architectural ID register using the
    /// backend's native provider, without executing a product VP. This is
    /// separate from per-VP topology and feature-bank configuration readback.
    /// Unsupported registers fail; a caller must not substitute a host
    /// capability or an expected profile value.
    fn read_cpu_id_register(&self, register: aarch64defs::SystemReg) -> Result<u64, TimeAbiError>;

    /// Reads the actual guest counter while partition time is active.
    /// Frozen time or a native API failure is an error, never a zero offset
    /// or a host-counter substitute.
    fn read_active_counter(&self) -> Result<CounterReading, TimeAbiError>;

    /// Brackets an actual host UTC sample with active guest-counter reads.
    /// A failed or excessively wide bracket is returned to the caller.
    fn counter_utc_pair(&self) -> Result<CounterUtcPair, TimeAbiError> {
        let before = self.read_active_counter()?;
        let time = super::host::sample_host_time()?;
        let after = self.read_active_counter()?;
        CounterUtcPair::from_bracket(before, after, time, self.configuration().counter_hz)
    }
}

/// Verifies CPU-owned architectural fields against the target partition.
///
/// This is the effective-ID portion of admission, not canonical-profile,
/// generation, topology, or complete cache/time validation. A missing native
/// provider is a failure, never an expected-value or host-capability substitute.
pub fn verify_cpu_id_requirements(backend: &dyn TimeAbiBackend) -> Result<(), TimeAbiError> {
    let config = backend.configuration();
    config.validate()?;
    if config.cpu_profile.id_registers.is_empty() {
        return Err(TimeAbiError::new(
            TimeAbiCode::CpuSurface,
            "ARM profile has no effective architectural ID requirements",
        ));
    }
    let mut violations = Vec::new();
    for requirement in &config.cpu_profile.id_registers {
        match backend.read_cpu_id_register(requirement.register) {
            Ok(actual) if actual & requirement.mask == requirement.value => {}
            Ok(actual) => violations.push(format!(
                "{:?} mask={:#018x} expected={:#018x} actual={actual:#018x}",
                requirement.register, requirement.mask, requirement.value,
            )),
            Err(error) => violations.push(format!("{:?}: {error}", requirement.register)),
        }
    }
    if !violations.is_empty() {
        return Err(TimeAbiError::new(
            TimeAbiCode::CpuSurface,
            format!(
                "ARM effective CPU fields failed:\n{}",
                violations.join("\n")
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn config() -> TimeAbiConfig {
        TimeAbiConfig {
            cpu_profile: CpuProfile {
                id: "arm64-test".into(),
                sha256: [0x5a; 32],
                model: cpu_profile::aarch64::CpuModel {
                    mask: 0xff0f_fff0,
                    value: 0x410f_d030,
                },
                physical_address_bits: 36,
                cache_policy: CachePolicy::NativeInformational,
                id_registers: vec![],
            },
            counter_hz: 19_200_000,
            backend_usable_gpa_bits: 39,
        }
    }

    #[test]
    fn binding_has_the_exact_draft_layout() {
        for capacity in [1u32, 2, 4, 8] {
            let bytes = config().binding(capacity).unwrap();
            assert_eq!(&bytes[..12], b"NVT\0\x01\0\0\0\x01\0\0\0");
            assert_eq!(&bytes[12..16], &capacity.to_le_bytes());
            assert_eq!(&bytes[16..24], &19_200_000u64.to_le_bytes());
            assert_eq!(&bytes[24..32], &[20, 0, 0, 0, 10, 0, 0, 0]);
            assert_eq!(&bytes[32..42], b"arm64-test");
            assert_eq!(&bytes[42..64], &[0; 22]);
            assert_eq!(&bytes[64..], &[0x5a; 32]);
        }
        for capacity in [0, 3, 5, 16, u32::MAX] {
            assert!(config().binding(capacity).is_err());
        }
    }

    #[test]
    fn configuration_has_no_implicit_profile_or_rate() {
        for id in ["", "auto", "host", "ARM64", "arm64 bad", &"a".repeat(33)] {
            let mut cfg = config();
            cfg.cpu_profile.id = id.into();
            assert_eq!(
                cfg.validate().unwrap_err().code,
                TimeAbiCode::ProfileUnknown
            );
        }
        let mut cfg = config();
        cfg.counter_hz = 0;
        assert_eq!(
            cfg.validate().unwrap_err().code,
            TimeAbiCode::ProfileTimeBits
        );
        cfg = config();
        cfg.cpu_profile.sha256 = [0; 32];
        assert_eq!(cfg.validate().unwrap_err().code, TimeAbiCode::ProfileDigest);
    }

    #[test]
    fn time_binding_uses_the_selected_identity_and_actual_frequency() {
        for (id, hz, digest) in [
            ("arm.test-model.v1", 24_000_000u64, [0x12; 32]),
            ("another.host.v1", 100_000_000u64, [0x34; 32]),
        ] {
            let mut cfg = config();
            cfg.cpu_profile.id = id.into();
            cfg.cpu_profile.sha256 = digest;
            cfg.counter_hz = hz;
            let binding = cfg.binding(4).unwrap();
            assert_eq!(&binding[16..24], &hz.to_le_bytes());
            assert_eq!(&binding[32..32 + id.len()], id.as_bytes());
            assert_eq!(&binding[64..], &digest);
        }
    }

    #[test]
    fn backend_gpa_metadata_does_not_change_cpu_or_time_binding() {
        let mut cfg = config();
        let binding = cfg.binding(4).unwrap();
        let profile = cfg.cpu_profile.clone();
        cfg.backend_usable_gpa_bits = 48;
        assert_eq!(cfg.cpu_profile, profile);
        assert_eq!(cfg.binding(4).unwrap(), binding);
        assert_eq!(cfg.cpu_profile.physical_address_bits, 36);
    }

    #[test]
    fn architectural_requirements_reject_duplicate_and_unowned_values() {
        let entry = CpuIdRegister {
            register: aarch64defs::SystemReg::ID_AA64ISAR0_EL1,
            mask: 0xf0,
            value: 0x20,
        };
        let mut cfg = config();
        cfg.cpu_profile.id_registers = vec![entry];
        cfg.validate().unwrap();
        for entries in [
            vec![entry, entry],
            vec![CpuIdRegister { mask: 0, ..entry }],
            vec![CpuIdRegister { value: 1, ..entry }],
        ] {
            cfg.cpu_profile.id_registers = entries;
            assert_eq!(
                cfg.validate().unwrap_err().code,
                TimeAbiCode::ProfileUnsupported
            );
        }
        for register in [
            aarch64defs::SystemReg::CTR_EL0,
            aarch64defs::SystemReg::DCZID_EL0,
            aarch64defs::SystemReg::CLIDR_EL1,
        ] {
            cfg.cpu_profile.id_registers = vec![CpuIdRegister {
                register,
                mask: 0xf,
                value: 4,
            }];
            assert_eq!(
                cfg.validate().unwrap_err().code,
                TimeAbiCode::ProfileUnsupported
            );
        }
    }

    #[test]
    fn effective_cpu_check_uses_native_values_masks_and_reports_all_failures() {
        struct Backend {
            config: TimeAbiConfig,
            value: u64,
        }
        impl TimeAbiBackend for Backend {
            fn configuration(&self) -> &TimeAbiConfig {
                &self.config
            }
            fn read_cpu_id_register(
                &self,
                register: aarch64defs::SystemReg,
            ) -> Result<u64, TimeAbiError> {
                if register == aarch64defs::SystemReg::ID_AA64ISAR0_EL1 {
                    Ok(self.value)
                } else {
                    Err(TimeAbiError::new(
                        TimeAbiCode::ProfileUnsupported,
                        "no native provider",
                    ))
                }
            }
            fn read_active_counter(&self) -> Result<CounterReading, TimeAbiError> {
                Err(TimeAbiError::new(
                    TimeAbiCode::ProfileTimeBits,
                    "not sampled",
                ))
            }
        }
        let mut backend = Backend {
            config: config(),
            value: 0xffff_ffff_ffff_ff2f,
        };
        assert!(verify_cpu_id_requirements(&backend).is_err());
        backend.config.cpu_profile.id_registers.push(CpuIdRegister {
            register: aarch64defs::SystemReg::ID_AA64ISAR0_EL1,
            mask: 0xf0,
            value: 0x20,
        });
        verify_cpu_id_requirements(&backend).unwrap();
        backend.value = 0x30;
        backend.config.cpu_profile.id_registers.push(CpuIdRegister {
            register: aarch64defs::SystemReg::ID_AA64MMFR0_EL1,
            mask: 0xf,
            value: 1,
        });
        let error = verify_cpu_id_requirements(&backend).unwrap_err();
        let message = error.to_string();
        assert_eq!(error.code, TimeAbiCode::CpuSurface);
        assert!(message.contains("ID_AA64ISAR0_EL1"));
        assert!(message.contains("ID_AA64MMFR0_EL1"));
        assert!(message.contains("actual=0x0000000000000030"));
        assert!(message.contains("no native provider"));
    }

    #[test]
    fn bracket_uses_counter_ticks_not_host_tsc_units() {
        let time = HostTimeSample {
            utc_ns: 123,
            monotonic_ns: 456,
        };
        let read = |ticks| CounterReading { ticks, epoch: 7 };
        let pair = CounterUtcPair::from_bracket(read(100), read(105), time, 19_200_000).unwrap();
        assert_eq!(pair.counter, read(102));
        assert_eq!(pair.pairing_ns, 209);
        assert_eq!(pair.time, time);
        assert_eq!(
            CounterUtcPair::from_bracket(read(100), read(100), time, 19_200_000)
                .unwrap()
                .pairing_ns,
            53
        );
        assert_eq!(
            CounterUtcPair::from_bracket(read(u64::MAX - 4), read(u64::MAX), time, 19_200_000)
                .unwrap()
                .counter
                .ticks,
            u64::MAX - 2
        );
    }

    #[test]
    fn invalid_and_overwide_brackets_fail_closed() {
        let time = HostTimeSample {
            utc_ns: 123,
            monotonic_ns: 456,
        };
        let read = |ticks, epoch| CounterReading { ticks, epoch };
        for (before, after, hz) in [
            (read(5, 0), read(4, 0), 19_200_000),
            (read(5, 0), read(6, 1), 19_200_000),
            (read(5, 0), read(6, 0), 0),
            (read(0, 0), read(u64::MAX, 0), 1),
        ] {
            assert!(CounterUtcPair::from_bracket(before, after, time, hz).is_err());
        }
        assert_eq!(
            CounterUtcPair::from_bracket(read(0, 0), read(199_998, 0), time, 1_000_000_000)
                .unwrap()
                .pairing_ns,
            MAX_COUNTER_PAIRING_NS
        );
        assert!(
            CounterUtcPair::from_bracket(read(0, 0), read(199_999, 0), time, 1_000_000_000)
                .is_err()
        );
    }
}
