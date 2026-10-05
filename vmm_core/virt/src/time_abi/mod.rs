// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVX time ABI v1.
//!
//! The guest-visible time contract of the microVM: a minimal Hyper-V
//! frequency identity, the identity MSRs, the TSC rate policy, downtime
//! selection, and the backend primitives that snapshot restore needs. The
//! contract is specified in the NVX repository, `doc/design/time-abi.md`.

#[cfg(guest_arch = "x86_64")]
pub mod backend;
#[cfg(guest_arch = "x86_64")]
pub mod cpuid;
pub mod downtime;
pub mod host;
#[cfg(guest_arch = "x86_64")]
pub mod identity;
#[cfg(guest_arch = "x86_64")]
pub mod msr;
pub mod rate;
pub mod surface;

#[cfg(guest_arch = "x86_64")]
pub use backend::BackendPreflight;
#[cfg(guest_arch = "x86_64")]
pub use backend::IdentityMsrRoute;
#[cfg(guest_arch = "x86_64")]
pub use backend::MAX_ANCHOR_PAIRING_NS;
#[cfg(guest_arch = "x86_64")]
pub use backend::NegotiatedRates;
#[cfg(guest_arch = "x86_64")]
pub use backend::TimeAbiBackend;
#[cfg(guest_arch = "x86_64")]
pub use backend::TscAnchor;
#[cfg(guest_arch = "x86_64")]
pub use backend::TscSetReport;
#[cfg(guest_arch = "x86_64")]
pub use backend::TscSyncMethod;
#[cfg(guest_arch = "x86_64")]
pub use backend::negotiate_rates;
pub use downtime::CaptureTimeRecord;
pub use downtime::Downtime;
pub use downtime::DowntimeSource;
pub use host::HostClockKind;
pub use host::HostIdentity;
pub use host::HostTimeSample;
#[cfg(guest_arch = "x86_64")]
pub use msr::TimeAbiMsrs;
pub use rate::RateCheck;

use mesh_protobuf::Protobuf;
use std::fmt;

/// The time ABI version this OpenVMM implements.
pub const TIME_ABI_VERSION: u32 = 1;

macro_rules! time_abi_codes {
    ($($(#[doc = $doc:literal])* $variant:ident => $name:literal,)*) => {
        /// A stable failure code of the time ABI.
        ///
        /// The spelling returned by [`TimeAbiCode::as_str`] is part of the
        /// ABI: tests and orchestrators match it.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum TimeAbiCode {
            $($(#[doc = $doc])* $variant,)*
        }

        impl TimeAbiCode {
            /// Every code, in specification order.
            pub const ALL: &[TimeAbiCode] = &[$(TimeAbiCode::$variant,)*];

            /// Returns the stable name of the code, for example
            /// `E_TSC_RATE_TOLERANCE`.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(TimeAbiCode::$variant => $name,)*
                }
            }

            /// Returns the code with the stable name `name`, if any.
            pub fn from_name(name: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|code| code.as_str() == name)
            }
        }
    };
}

time_abi_codes! {
    /// The manifest version is not the supported one.
    SnapshotVersion => "E_SNAPSHOT_VERSION",
    /// The time contract is missing or malformed.
    ManifestTime => "E_MANIFEST_TIME",
    /// The snapshot was taken on another backend.
    BackendMismatch => "E_BACKEND_MISMATCH",
    /// The CPU profile is not pinned in this OpenVMM.
    ProfileUnknown => "E_PROFILE_UNKNOWN",
    /// A CPU profile or effective-CPUID digest does not verify.
    ProfileDigest => "E_PROFILE_DIGEST",
    /// `--cpu-profile auto` maps the host to no profile.
    ProfileHostUnknown => "E_PROFILE_HOST_UNKNOWN",
    /// The CPU profile violates the CPU time bits.
    ProfileTimeBits => "E_PROFILE_TIME_BITS",
    /// The host CPU generation is not in the profile.
    CpuGeneration => "E_CPU_GENERATION",
    /// The backend lacks a feature, limit, or value of the profile.
    ProfileUnsupported => "E_PROFILE_UNSUPPORTED",
    /// The recomputed effective CPUID differs from the recorded one.
    CpuSurface => "E_CPU_SURFACE",
    /// The hypervisor presents a non-zero CPUID entry outside the profile's
    /// tables, which would expose a host feature in a reserved entry.
    CpuUnlisted => "E_CPU_UNLISTED",
    /// The backend cannot deliver the identity CPUID or MSRs.
    IdentityRouting => "E_IDENTITY_ROUTING",
    /// The backend lacks the synchronized TSC set.
    TscSyncUnsupported => "E_TSC_SYNC_UNSUPPORTED",
    /// The guest TSC would be scaled.
    TscScalingActive => "E_TSC_SCALING_ACTIVE",
    /// The backend cannot report its native TSC rate.
    TscRateUnavailable => "E_TSC_RATE_UNAVAILABLE",
    /// A TSC rate is outside the plausible range.
    TscRateImplausible => "E_TSC_RATE_IMPLAUSIBLE",
    /// The destination TSC rate is outside the tolerance.
    TscRateTolerance => "E_TSC_RATE_TOLERANCE",
    /// The backend cannot report its LAPIC timer rate.
    LapicRateUnavailable => "E_LAPIC_RATE_UNAVAILABLE",
    /// The LAPIC timer rate is not the backend constant or the saved rate.
    LapicRateMismatch => "E_LAPIC_RATE_MISMATCH",
    /// The host identity, boot identity, or clocks are unavailable.
    HostIdentity => "E_HOST_IDENTITY",
    /// The downtime is negative.
    DowntimeNegative => "E_DOWNTIME_NEGATIVE",
    /// The downtime exceeds the maximum.
    DowntimeExcessive => "E_DOWNTIME_EXCESSIVE",
    /// The capture anchor is unavailable or not paired closely enough.
    TscAnchor => "E_TSC_ANCHOR",
    /// The restore TSC target exceeds 64 bits.
    TscTargetOverflow => "E_TSC_TARGET_OVERFLOW",
    /// A VP does not hold the synchronized TSC value.
    TscSyncReadback => "E_TSC_SYNC_READBACK",
    /// A VP was instantiated after the synchronized TSC set.
    VpLateCreation => "E_VP_LATE_CREATION",
    /// A periodic LAPIC timer is armed.
    LapicPeriodic => "E_LAPIC_PERIODIC",
    /// TSC-deadline LAPIC timer mode or state is present.
    LapicTscDeadline => "E_LAPIC_TSC_DEADLINE",
    /// PIT channel 0 is counting in a periodic mode.
    PitActive => "E_PIT_ACTIVE",
    /// The generation counter would overflow.
    GenerationExhausted => "E_GENERATION_EXHAUSTED",
    /// A clock token is present in the kernel command line.
    CmdlineClockToken => "E_CMDLINE_CLOCK_TOKEN",
    /// A test hook is malformed or unknown.
    TestHook => "E_TEST_HOOK",
}

impl fmt::Display for TimeAbiCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A time ABI failure.
///
/// It displays as `[E_CODE] message`, so the code survives conversion to a
/// string across the worker boundary and error wrapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeAbiError {
    /// The stable code.
    pub code: TimeAbiCode,
    /// A human-readable description with the relevant values.
    pub message: String,
}

impl TimeAbiError {
    /// Returns a new error.
    pub fn new(code: TimeAbiCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for TimeAbiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for TimeAbiError {}

impl From<cpu_profile::ProfileError> for TimeAbiError {
    /// Keeps the code: every CPU profile code is a time ABI code.
    fn from(error: cpu_profile::ProfileError) -> Self {
        let code = TimeAbiCode::from_name(error.code.as_str())
            .expect("every CPU profile code is a time ABI code");
        Self::new(code, error.message)
    }
}

/// The rates the guest reads from MSRs `0x40000022` and `0x40000023`.
///
/// They are declared once per VM process: a cold boot declares the native TSC
/// rate and a restore declares the snapshot's rates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Protobuf)]
#[mesh(package = "virt.time_abi")]
pub struct DeclaredRates {
    /// The declared TSC rate `F`, in Hz.
    #[mesh(1)]
    pub tsc_hz: u64,
    /// The LAPIC timer rate `L`, in Hz.
    #[mesh(2)]
    pub apic_hz: u64,
}

impl DeclaredRates {
    /// Returns validated rates: a plausible TSC rate and a LAPIC rate that is
    /// one of the backend constants.
    pub fn new(tsc_hz: u64, apic_hz: u64) -> Result<Self, TimeAbiError> {
        rate::check_plausible_tsc_hz(tsc_hz)?;
        if apic_hz != rate::LAPIC_HZ_KVM && apic_hz != rate::LAPIC_HZ_HYPERV {
            return Err(TimeAbiError::new(
                TimeAbiCode::LapicRateMismatch,
                format!("LAPIC timer rate {apic_hz} Hz is not a backend constant"),
            ));
        }
        Ok(Self { tsc_hz, apic_hz })
    }
}

/// Test hooks selected with the hidden `--x-time-abi-test-hook` option.
///
/// Every active hook is logged and sets the `TEST_HOOKS` flag in the restore
/// packet and in every time sample.
#[derive(Debug, Clone, Default, PartialEq, Eq, Protobuf)]
#[mesh(package = "virt.time_abi")]
pub struct TimeAbiTestHooks {
    /// Select the UTC downtime source even on the same host boot.
    #[mesh(1)]
    pub force_utc_downtime: bool,
    /// Treat the destination boot identity as different.
    #[mesh(2)]
    pub boot_id_mismatch: bool,
    /// Perturb the measured destination TSC rate by this many ppm.
    #[mesh(3)]
    pub dest_rate_offset_ppm: i32,
    /// Add this many seconds to the measured downtime.
    #[mesh(4)]
    pub downtime_add_s: u64,
    /// Add this many milliseconds to every destination UTC reading.
    #[mesh(5)]
    pub utc_offset_ms: i64,
    /// Delay the handling of time-sample selections by this many
    /// microseconds.
    #[mesh(6)]
    pub sample_delay_us: u32,
}

/// The largest accepted `dest-rate-offset-ppm` magnitude.
pub const MAX_HOOK_RATE_OFFSET_PPM: i32 = 10_000;
/// The largest accepted `downtime-add-s` value: ten years.
pub const MAX_HOOK_DOWNTIME_ADD_S: u64 = 10 * 365 * 24 * 60 * 60;
/// The largest accepted `utc-offset-ms` magnitude: ten years.
pub const MAX_HOOK_UTC_OFFSET_MS: i64 = 10 * 365 * 24 * 60 * 60 * 1000;
/// The largest accepted `sample-delay-us` value.
pub const MAX_HOOK_SAMPLE_DELAY_US: u32 = 100_000;

impl TimeAbiTestHooks {
    /// Parses hook values such as `force-utc-downtime` or
    /// `dest-rate-offset-ppm=-200`. A hook may appear at most once.
    pub fn parse<S: AsRef<str>>(values: &[S]) -> Result<Self, TimeAbiError> {
        fn invalid(value: &str, reason: &str) -> TimeAbiError {
            TimeAbiError::new(
                TimeAbiCode::TestHook,
                format!("invalid time ABI test hook '{value}': {reason}"),
            )
        }

        fn number<T: std::str::FromStr + PartialOrd>(
            value: &str,
            argument: Option<&str>,
            min: T,
            max: T,
        ) -> Result<T, TimeAbiError> {
            let argument = argument.ok_or_else(|| invalid(value, "missing value"))?;
            let parsed: T = argument
                .parse()
                .map_err(|_| invalid(value, "malformed number"))?;
            if parsed < min || parsed > max {
                return Err(invalid(value, "value out of range"));
            }
            Ok(parsed)
        }

        let mut hooks = Self::default();
        let mut seen = Vec::new();
        for value in values {
            let value = value.as_ref();
            let (name, argument) = match value.split_once('=') {
                Some((name, argument)) => (name, Some(argument)),
                None => (value, None),
            };
            if seen.contains(&name) {
                return Err(invalid(value, "repeated hook"));
            }
            seen.push(name);
            match name {
                "force-utc-downtime" | "boot-id-mismatch" if argument.is_some() => {
                    return Err(invalid(value, "the hook takes no value"));
                }
                "force-utc-downtime" => hooks.force_utc_downtime = true,
                "boot-id-mismatch" => hooks.boot_id_mismatch = true,
                "dest-rate-offset-ppm" => {
                    hooks.dest_rate_offset_ppm = number(
                        value,
                        argument,
                        -MAX_HOOK_RATE_OFFSET_PPM,
                        MAX_HOOK_RATE_OFFSET_PPM,
                    )?;
                }
                "downtime-add-s" => {
                    hooks.downtime_add_s = number(value, argument, 0, MAX_HOOK_DOWNTIME_ADD_S)?;
                }
                "utc-offset-ms" => {
                    hooks.utc_offset_ms = number(
                        value,
                        argument,
                        -MAX_HOOK_UTC_OFFSET_MS,
                        MAX_HOOK_UTC_OFFSET_MS,
                    )?;
                }
                "sample-delay-us" => {
                    hooks.sample_delay_us = number(value, argument, 0, MAX_HOOK_SAMPLE_DELAY_US)?;
                }
                _ => return Err(invalid(value, "unknown hook")),
            }
        }
        Ok(hooks)
    }

    /// Returns whether any hook alters time ABI behavior.
    pub fn active(&self) -> bool {
        *self != Self::default()
    }
}

/// Kernel parameters that configure guest clocks and that the time ABI
/// forbids.
pub const CLOCK_TOKENS: [&str; 2] = ["tsc_early_khz", "lapic_timer_hz"];

/// Rejects a kernel command line that sets a [`CLOCK_TOKENS`] parameter
/// (`E_CMDLINE_CLOCK_TOKEN`).
///
/// It follows the kernel's parsing: parameters are separated by unquoted
/// whitespace, `-` and `_` are equivalent in names, a parameter may be
/// quoted as a whole, and parsing stops at the `--` delimiter, after which
/// arguments belong to init.
pub fn check_command_line_clock_tokens(cmdline: &str) -> Result<(), TimeAbiError> {
    fn normalized(name: &str) -> impl Iterator<Item = u8> + '_ {
        name.bytes().map(|b| if b == b'-' { b'_' } else { b })
    }

    let mut tokens = Vec::new();
    let mut start = None;
    let mut in_quote = false;
    for (offset, c) in cmdline.char_indices() {
        if c == '"' {
            in_quote = !in_quote;
        }
        if c.is_ascii_whitespace() && !in_quote {
            if let Some(start) = start.take() {
                tokens.push(&cmdline[start..offset]);
            }
        } else if start.is_none() {
            start = Some(offset);
        }
    }
    if let Some(start) = start {
        tokens.push(&cmdline[start..]);
    }

    for token in tokens {
        let unquoted = token
            .strip_prefix('"')
            .map_or(token, |rest| rest.strip_suffix('"').unwrap_or(rest));
        if unquoted == "--" {
            break;
        }
        let name = unquoted.split_once('=').map_or(unquoted, |(name, _)| name);
        if CLOCK_TOKENS
            .iter()
            .any(|clock| normalized(name).eq(normalized(clock)))
        {
            return Err(TimeAbiError::new(
                TimeAbiCode::CmdlineClockToken,
                format!("kernel command line sets the clock parameter '{token}'"),
            ));
        }
    }
    Ok(())
}

/// Time ABI configuration of a partition, set in
/// [`ProtoPartitionConfig::time_abi`](crate::ProtoPartitionConfig::time_abi).
///
/// A backend that receives it must program `cpuid` after every other CPUID
/// source, remove every hypervisor-range leaf of its own, derive its
/// processor features from the CPU profile, route the identity MSR range to
/// `msrs`, derive its partition capabilities through
/// [`identity::capabilities_cpuid`], and implement [`TimeAbiBackend`].
#[cfg(guest_arch = "x86_64")]
#[derive(Debug, Clone)]
pub struct TimeAbiConfig {
    /// The complete CPUID of the partition, which the backend programs on
    /// every VP: the effective CPUID of the CPU profile (every leaf and
    /// subleaf of the profile, the topology leaves, and the identity and
    /// explicit zero leaves). Bits it leaves unmasked are not the profile's:
    /// the runtime-owned bits, and the per-VP fields (the initial APIC ID in
    /// leaf 1, the x2APIC IDs in leaves 0xB and 0x1F, and the IDs in leaf
    /// 0x8000001E), which the backend sets for each VP.
    pub cpuid: std::sync::Arc<crate::CpuidLeafSet>,
    /// The identity MSR handler, shared with the worker's `time-abi` state
    /// unit.
    pub msrs: std::sync::Arc<TimeAbiMsrs>,
    /// The partition's CPU profile, selected or restored before the partition
    /// is created: a pinned profile or a host profile, from which a backend
    /// derives its processor features. Only the partitions that use a host
    /// profile hold it.
    pub cpu_profile: cpu_profile::PartitionProfile,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_errors_keep_their_codes() {
        for code in cpu_profile::ProfileErrorCode::ALL {
            let error = TimeAbiError::from(cpu_profile::ProfileError::new(code, "message"));
            assert_eq!(error.code.as_str(), code.as_str());
            assert_eq!(error.message, "message");
        }
    }

    #[test]
    fn codes_match_the_specification() {
        let names: Vec<_> = TimeAbiCode::ALL.iter().map(|code| code.as_str()).collect();
        assert_eq!(
            names,
            [
                "E_SNAPSHOT_VERSION",
                "E_MANIFEST_TIME",
                "E_BACKEND_MISMATCH",
                "E_PROFILE_UNKNOWN",
                "E_PROFILE_DIGEST",
                "E_PROFILE_HOST_UNKNOWN",
                "E_PROFILE_TIME_BITS",
                "E_CPU_GENERATION",
                "E_PROFILE_UNSUPPORTED",
                "E_CPU_SURFACE",
                "E_CPU_UNLISTED",
                "E_IDENTITY_ROUTING",
                "E_TSC_SYNC_UNSUPPORTED",
                "E_TSC_SCALING_ACTIVE",
                "E_TSC_RATE_UNAVAILABLE",
                "E_TSC_RATE_IMPLAUSIBLE",
                "E_TSC_RATE_TOLERANCE",
                "E_LAPIC_RATE_UNAVAILABLE",
                "E_LAPIC_RATE_MISMATCH",
                "E_HOST_IDENTITY",
                "E_DOWNTIME_NEGATIVE",
                "E_DOWNTIME_EXCESSIVE",
                "E_TSC_ANCHOR",
                "E_TSC_TARGET_OVERFLOW",
                "E_TSC_SYNC_READBACK",
                "E_VP_LATE_CREATION",
                "E_LAPIC_PERIODIC",
                "E_LAPIC_TSC_DEADLINE",
                "E_PIT_ACTIVE",
                "E_GENERATION_EXHAUSTED",
                "E_CMDLINE_CLOCK_TOKEN",
                "E_TEST_HOOK",
            ]
        );
    }

    #[test]
    fn codes_parse_from_their_names() {
        for &code in TimeAbiCode::ALL {
            assert_eq!(TimeAbiCode::from_name(code.as_str()), Some(code));
        }
        assert_eq!(TimeAbiCode::from_name("E_NOT_A_CODE"), None);
        assert_eq!(TimeAbiCode::from_name("e_test_hook"), None);
    }

    #[test]
    fn error_display_leads_with_the_code() {
        let error = TimeAbiError::new(TimeAbiCode::TscRateTolerance, "rate too far");
        assert_eq!(error.to_string(), "[E_TSC_RATE_TOLERANCE] rate too far");
        let wrapped = anyhow::Error::new(error).context("restore failed");
        assert!(format!("{wrapped:#}").contains("[E_TSC_RATE_TOLERANCE] rate too far"));
    }

    #[test]
    fn declared_rates_are_validated() {
        DeclaredRates::new(2_000_000_000, rate::LAPIC_HZ_KVM).unwrap();
        DeclaredRates::new(2_000_000_000, rate::LAPIC_HZ_HYPERV).unwrap();
        assert_eq!(
            DeclaredRates::new(2_000_000_000, 100_000_000)
                .unwrap_err()
                .code,
            TimeAbiCode::LapicRateMismatch
        );
        assert_eq!(
            DeclaredRates::new(100, rate::LAPIC_HZ_KVM)
                .unwrap_err()
                .code,
            TimeAbiCode::TscRateImplausible
        );
    }

    #[test]
    fn clock_tokens_are_rejected() {
        for cmdline in [
            "console=hvc0 tsc_early_khz=2500000",
            "lapic_timer_hz=200000000 console=hvc0",
            "console=hvc0 tsc-early-khz=1",
            "console=hvc0 lapic-timer_hz",
            r#"console=hvc0 "tsc_early_khz=1""#,
            "console=hvc0\ttsc_early_khz=1\n",
        ] {
            assert_eq!(
                check_command_line_clock_tokens(cmdline).unwrap_err().code,
                TimeAbiCode::CmdlineClockToken,
                "{cmdline}"
            );
        }
    }

    #[test]
    fn clock_token_check_follows_kernel_parsing() {
        for cmdline in [
            "",
            "console=hvc0 clocksource=tsc",
            "console=hvc0 -- tsc_early_khz=1",
            r#"console=hvc0 "--" lapic_timer_hz=1"#,
            r#"console=hvc0 note="tsc_early_khz=1 lapic_timer_hz=1""#,
            "xtsc_early_khz=1 tsc_early_khzx=1",
        ] {
            check_command_line_clock_tokens(cmdline).unwrap();
        }
    }

    #[test]
    fn test_hooks_parse() {
        let hooks = TimeAbiTestHooks::parse(&[
            "force-utc-downtime",
            "boot-id-mismatch",
            "dest-rate-offset-ppm=-200",
            "downtime-add-s=2592001",
            "utc-offset-ms=-5000",
            "sample-delay-us=3000",
        ])
        .unwrap();
        assert_eq!(
            hooks,
            TimeAbiTestHooks {
                force_utc_downtime: true,
                boot_id_mismatch: true,
                dest_rate_offset_ppm: -200,
                downtime_add_s: 2_592_001,
                utc_offset_ms: -5000,
                sample_delay_us: 3000,
            }
        );
        assert!(hooks.active());
        assert!(!TimeAbiTestHooks::parse::<&str>(&[]).unwrap().active());
    }

    #[test]
    fn test_hooks_reject_malformed_values() {
        for value in [
            "unknown",
            "force-utc-downtime=1",
            "dest-rate-offset-ppm",
            "dest-rate-offset-ppm=",
            "dest-rate-offset-ppm=x",
            "dest-rate-offset-ppm=10001",
            "downtime-add-s=-1",
            "downtime-add-s=315360001",
            "utc-offset-ms=315360000001",
            "sample-delay-us=100001",
        ] {
            assert_eq!(
                TimeAbiTestHooks::parse(&[value]).unwrap_err().code,
                TimeAbiCode::TestHook,
                "{value}"
            );
        }
        assert_eq!(
            TimeAbiTestHooks::parse(&["downtime-add-s=1", "downtime-add-s=2"])
                .unwrap_err()
                .code,
            TimeAbiCode::TestHook
        );
    }
}
