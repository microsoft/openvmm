// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The time ABI primitives a backend provides.

use super::DeclaredRates;
use super::HostTimeSample;
use super::RateCheck;
use super::TimeAbiError;
use super::TimeAbiTestHooks;
use super::rate;
use crate::CpuidLeaf;
use vm_topology::processor::VpIndex;

/// Time ABI primitives of a backend partition, returned by
/// [`Partition::time_abi`](crate::Partition::time_abi) when the partition
/// was built with [`TimeAbiConfig`](super::TimeAbiConfig).
///
/// Every method is called with all VPs stopped. No method falls back to
/// another mechanism: a backend that cannot meet an obligation returns the
/// failure code the specification names.
pub trait TimeAbiBackend: Send + Sync {
    /// Returns the native guest TSC rate `F_d`, in Hz. The guest TSC is never
    /// scaled. Fails with `E_TSC_RATE_UNAVAILABLE`.
    fn native_tsc_hz(&self) -> Result<u64, TimeAbiError>;

    /// Returns the LAPIC timer rate, in Hz: the backend constant. Fails with
    /// `E_LAPIC_RATE_UNAVAILABLE`.
    fn lapic_hz(&self) -> Result<u64, TimeAbiError>;

    /// Verifies, once after the partition is built, that identity routing is
    /// installed, that the synchronized TSC set is available, and that the
    /// guest TSC is not scaled. Fails with `E_IDENTITY_ROUTING`,
    /// `E_TSC_SYNC_UNSUPPORTED`, or `E_TSC_SCALING_ACTIVE`.
    ///
    /// Core calls it on cold boot and restore before any VP runs and before
    /// any VP state is restored, so it may briefly stop partition time (MSHV
    /// probes `TimeFreeze`) and read VP 0's reset state.
    fn preflight(&self) -> Result<BackendPreflight, TimeAbiError>;

    /// Returns the CPUID that VP 0 observes for the leaves of
    /// [`TimeAbiConfig::cpuid`](super::TimeAbiConfig::cpuid), or a superset,
    /// with VP 0's own APIC identity in the per-VP fields. Core compares it
    /// with the CPU profile's effective CPUID under its masks
    /// (`E_CPU_SURFACE`). Fails with `E_CPU_SURFACE`.
    ///
    /// A backend whose guests read the hypervisor's own values outside that
    /// table (MSHV, WHP) also returns VP 0's view at every entry the host's
    /// CPUID enumerates outside the profile's tables
    /// (`cpu_profile::unlisted_cpuid_candidates`), at subleaf 0 for a
    /// subleaf-independent entry. Core requires each to read zero
    /// (`E_CPU_UNLISTED`), so that no reserved entry exposes a host feature.
    ///
    /// Core calls it with all VPs stopped, before any VP runs and before any
    /// VP state is restored, on cold boot and restore. Core's first call to
    /// this method or to [`Self::preflight`] comes before any VP runs, so a
    /// backend that can only read VP 0's live view reads it then and keeps
    /// it.
    fn effective_cpuid(&self) -> Result<Vec<CpuidLeaf>, TimeAbiError>;

    /// Returns the CPU surface the backend supports on this host, for the CPU
    /// profile's support check (`E_PROFILE_UNSUPPORTED`), which core runs at
    /// every cold boot and restore. It must be cheap: capability queries and
    /// the host's CPUID, never a probe partition. A backend that cannot report
    /// its surface returns `None`, which fails every time ABI boot
    /// (`E_PROFILE_UNSUPPORTED`).
    fn supported_cpu_surface(
        &self,
    ) -> Result<Option<super::surface::SupportedCpuSurface>, TimeAbiError>;

    /// Takes the capture anchor: VP 0's TSC paired with
    /// [`sample_host_time`](super::host::sample_host_time). Fails with
    /// `E_TSC_ANCHOR` if the pair cannot be formed within the bound.
    fn capture_anchor(&self) -> Result<TscAnchor, TimeAbiError>;

    /// Performs the synchronized TSC set.
    ///
    /// The backend takes the restore anchor, one host instant, and calls
    /// `target` with the host sample of that instant. The orchestrator
    /// selects the downtime there and returns the TSC target, or fails. The
    /// backend then makes every instantiated VP hold that target as of the
    /// anchor and verifies it by read-back before returning
    /// (`E_TSC_SYNC_READBACK`). Creating a VP afterwards is an error
    /// (`E_VP_LATE_CREATION`).
    ///
    /// Guest time runs from the anchor: a backend that stops partition time
    /// for the set resumes it right after a successful read-back, before
    /// returning, never at the first VP run. A failed set may leave it
    /// stopped; the restore then fails.
    ///
    /// Core calls it once per restore, after every instantiated VP is bound
    /// (which creates it on MSHV) and its saved state is restored, and before
    /// any restored VP runs. A cold boot never calls it.
    fn set_synchronized_tsc(
        &self,
        target: &mut dyn FnMut(&HostTimeSample) -> Result<u64, TimeAbiError>,
    ) -> Result<TscSetReport, TimeAbiError>;
}

/// How a backend delivers the identity MSR range to [`super::TimeAbiMsrs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityMsrRoute {
    /// Every access in the range exits to OpenVMM.
    ExitToVmm,
    /// The hypervisor serves the range natively with values verified equal
    /// to the time ABI's (TBD(mshv)).
    NativeVerified,
}

/// How a backend performs the synchronized TSC set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TscSyncMethod {
    /// One TSC offset for every VP, computed from one host TSC read (KVM).
    CommonOffset,
    /// Partition time is frozen, the target is written to every VP and read
    /// back while frozen, and time resumes right after the read-back (MSHV,
    /// WHP).
    FrozenWrite,
}

/// The result of [`TimeAbiBackend::preflight`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendPreflight {
    /// The identity MSR route.
    pub msr_route: IdentityMsrRoute,
    /// The synchronized TSC set method.
    pub sync: TscSyncMethod,
}

/// The largest accepted anchor pairing, in nanoseconds. A backend samples the
/// capture anchor, and a restore anchor that pairs a TSC read with host time,
/// a bounded number of times and keeps the tightest pair; if none is within
/// this bound, the capture or restore fails with `E_TSC_ANCHOR`.
pub const MAX_ANCHOR_PAIRING_NS: u64 = 100_000;

/// The capture anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TscAnchor {
    /// `T_c`, VP 0's TSC.
    pub tsc: u64,
    /// The host sample paired with `tsc`.
    pub sample: HostTimeSample,
    /// The pairing uncertainty: half the width of the host-time bracket
    /// around the TSC read, in nanoseconds.
    pub pairing_ns: u64,
}

/// The result of [`TimeAbiBackend::set_synchronized_tsc`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TscSetReport {
    /// The TSC target every VP holds as of the anchor.
    pub target: u64,
    /// The host sample of the restore anchor.
    pub sample: HostTimeSample,
    /// The read-back evidence, one entry per instantiated VP: the TSC value,
    /// or the offset for [`TscSyncMethod::CommonOffset`].
    pub readback: Vec<(VpIndex, u64)>,
    /// The method used.
    pub method: TscSyncMethod,
}

/// The outcome of [`negotiate_rates`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiatedRates {
    /// The rates to declare to the guest.
    pub declared: DeclaredRates,
    /// The backend's native TSC rate `F_d`, after the rate test hook.
    pub native_tsc_hz: u64,
    /// On restore, the accepted rate check against the snapshot's rate.
    pub check: Option<RateCheck>,
}

/// Applies the rate policy to a backend: restore step 9 of the
/// specification, and its cold-boot equivalent when `saved` is `None`.
///
/// A cold boot declares the native TSC rate. A restore declares the saved
/// rates after checking the native rate, perturbed by the
/// `dest-rate-offset-ppm` hook, against the tolerance, and the LAPIC rate
/// against the saved one. `hypervisor` is the backend ID (`kvm`, `mshv`, or
/// `whp`).
pub fn negotiate_rates(
    backend: &dyn TimeAbiBackend,
    hypervisor: &str,
    saved: Option<DeclaredRates>,
    hooks: &TimeAbiTestHooks,
) -> Result<NegotiatedRates, TimeAbiError> {
    let native_tsc_hz = backend.native_tsc_hz()?;
    let lapic_hz = backend.lapic_hz()?;
    match saved {
        None => {
            rate::check_lapic_hz(hypervisor, lapic_hz, None)?;
            Ok(NegotiatedRates {
                declared: DeclaredRates::new(native_tsc_hz, lapic_hz)?,
                native_tsc_hz,
                check: None,
            })
        }
        Some(saved) => {
            let native_tsc_hz = rate::apply_rate_offset(native_tsc_hz, hooks.dest_rate_offset_ppm)?;
            let check = rate::check_tsc_rate(saved.tsc_hz, native_tsc_hz)?;
            rate::check_lapic_hz(hypervisor, lapic_hz, Some(saved.apic_hz))?;
            Ok(NegotiatedRates {
                declared: DeclaredRates::new(saved.tsc_hz, saved.apic_hz)?,
                native_tsc_hz,
                check: Some(check),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time_abi::TimeAbiCode;
    use crate::time_abi::rate::LAPIC_HZ_HYPERV;
    use crate::time_abi::rate::LAPIC_HZ_KVM;

    struct FakeBackend {
        tsc_hz: Result<u64, TimeAbiError>,
        lapic_hz: u64,
    }

    impl TimeAbiBackend for FakeBackend {
        fn native_tsc_hz(&self) -> Result<u64, TimeAbiError> {
            self.tsc_hz.clone()
        }

        fn lapic_hz(&self) -> Result<u64, TimeAbiError> {
            Ok(self.lapic_hz)
        }

        fn preflight(&self) -> Result<BackendPreflight, TimeAbiError> {
            unreachable!()
        }

        fn effective_cpuid(&self) -> Result<Vec<CpuidLeaf>, TimeAbiError> {
            unreachable!()
        }

        fn supported_cpu_surface(
            &self,
        ) -> Result<Option<crate::time_abi::surface::SupportedCpuSurface>, TimeAbiError> {
            unreachable!()
        }

        fn capture_anchor(&self) -> Result<TscAnchor, TimeAbiError> {
            unreachable!()
        }

        fn set_synchronized_tsc(
            &self,
            _target: &mut dyn FnMut(&HostTimeSample) -> Result<u64, TimeAbiError>,
        ) -> Result<TscSetReport, TimeAbiError> {
            unreachable!()
        }
    }

    fn backend(tsc_hz: u64, lapic_hz: u64) -> FakeBackend {
        FakeBackend {
            tsc_hz: Ok(tsc_hz),
            lapic_hz,
        }
    }

    const NO_HOOKS: TimeAbiTestHooks = TimeAbiTestHooks {
        force_utc_downtime: false,
        boot_id_mismatch: false,
        dest_rate_offset_ppm: 0,
        downtime_add_s: 0,
        utc_offset_ms: 0,
        sample_delay_us: 0,
    };

    #[test]
    fn cold_boot_declares_the_native_rate() {
        let rates = negotiate_rates(
            &backend(2_194_804_000, LAPIC_HZ_KVM),
            "kvm",
            None,
            &NO_HOOKS,
        )
        .unwrap();
        assert_eq!(
            rates,
            NegotiatedRates {
                declared: DeclaredRates {
                    tsc_hz: 2_194_804_000,
                    apic_hz: LAPIC_HZ_KVM,
                },
                native_tsc_hz: 2_194_804_000,
                check: None,
            }
        );
        assert_eq!(
            negotiate_rates(
                &backend(2_194_804_000, LAPIC_HZ_HYPERV),
                "kvm",
                None,
                &NO_HOOKS
            )
            .unwrap_err()
            .code,
            TimeAbiCode::LapicRateMismatch
        );
        assert_eq!(
            negotiate_rates(&backend(1_000, LAPIC_HZ_HYPERV), "whp", None, &NO_HOOKS)
                .unwrap_err()
                .code,
            TimeAbiCode::TscRateImplausible
        );
        let unavailable = FakeBackend {
            tsc_hz: Err(TimeAbiError::new(
                TimeAbiCode::TscRateUnavailable,
                "no rate",
            )),
            lapic_hz: LAPIC_HZ_HYPERV,
        };
        assert_eq!(
            negotiate_rates(&unavailable, "mshv", None, &NO_HOOKS)
                .unwrap_err()
                .code,
            TimeAbiCode::TscRateUnavailable
        );
    }

    #[test]
    fn restore_declares_the_saved_rates() {
        let saved = DeclaredRates {
            tsc_hz: 2_000_000_000,
            apic_hz: LAPIC_HZ_HYPERV,
        };
        let rates = negotiate_rates(
            &backend(2_000_400_000, LAPIC_HZ_HYPERV),
            "mshv",
            Some(saved),
            &NO_HOOKS,
        )
        .unwrap();
        assert_eq!(rates.declared, saved);
        assert_eq!(rates.native_tsc_hz, 2_000_400_000);
        assert_eq!(rates.check.unwrap().rate_deviation, 200 * 65_536);

        assert_eq!(
            negotiate_rates(
                &backend(2_000_600_000, LAPIC_HZ_HYPERV),
                "mshv",
                Some(saved),
                &NO_HOOKS
            )
            .unwrap_err()
            .code,
            TimeAbiCode::TscRateTolerance
        );
        assert_eq!(
            negotiate_rates(
                &backend(2_000_000_000, LAPIC_HZ_KVM),
                "kvm",
                Some(saved),
                &NO_HOOKS
            )
            .unwrap_err()
            .code,
            TimeAbiCode::LapicRateMismatch
        );
    }

    #[test]
    fn restore_applies_the_rate_hook() {
        let saved = DeclaredRates {
            tsc_hz: 2_000_000_000,
            apic_hz: LAPIC_HZ_HYPERV,
        };
        let hooks = TimeAbiTestHooks {
            dest_rate_offset_ppm: -200,
            ..NO_HOOKS
        };
        let rates = negotiate_rates(
            &backend(2_000_000_000, LAPIC_HZ_HYPERV),
            "whp",
            Some(saved),
            &hooks,
        )
        .unwrap();
        assert_eq!(rates.native_tsc_hz, 1_999_600_000);
        assert_eq!(rates.check.unwrap().rate_deviation, -200 * 65_536);
        let hooks = TimeAbiTestHooks {
            dest_rate_offset_ppm: 251,
            ..NO_HOOKS
        };
        assert_eq!(
            negotiate_rates(
                &backend(2_000_000_000, LAPIC_HZ_HYPERV),
                "whp",
                Some(saved),
                &hooks
            )
            .unwrap_err()
            .code,
            TimeAbiCode::TscRateTolerance
        );
        // The hook never changes a cold boot's declared rate.
        let rates = negotiate_rates(
            &backend(2_000_000_000, LAPIC_HZ_HYPERV),
            "whp",
            None,
            &hooks,
        )
        .unwrap();
        assert_eq!(rates.declared.tsc_hz, 2_000_000_000);
    }
}
