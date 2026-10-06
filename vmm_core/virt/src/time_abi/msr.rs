// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The identity MSR handler.
//!
//! OpenVMM serves the whole identity MSR range itself on every backend. Each
//! backend routes accesses in [`IDENTITY_MSR_RANGE`] to one shared
//! [`TimeAbiMsrs`] before any other MSR handling. On a partition with an AMD
//! CPU profile, each backend also serves AMD's configuration MSRs
//! ([`AMD_CONFIG_MSRS`]) with fixed values.

use super::DeclaredRates;
use crate::x86::MsrError;
use inspect::Inspect;
use mesh_protobuf::Protobuf;
use std::ops::RangeInclusive;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use thiserror::Error;
use vm_topology::processor::VpIndex;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SavedStateRoot;

/// The identity MSR range. Every MSR in it that the table below does not
/// define raises #GP.
pub const IDENTITY_MSR_RANGE: RangeInclusive<u32> = 0x4000_0000..=0x4000_01ff;
/// `HV_X64_MSR_VP_INDEX`: the VP index, read only.
pub const MSR_VP_INDEX: u32 = 0x4000_0002;
/// `HV_X64_MSR_TSC_FREQUENCY`: the declared TSC rate, read only.
pub const MSR_TSC_FREQUENCY: u32 = 0x4000_0022;
/// `HV_X64_MSR_APIC_FREQUENCY`: the LAPIC timer rate, read only.
pub const MSR_APIC_FREQUENCY: u32 = 0x4000_0023;
/// `HV_X64_MSR_TSC_INVARIANT_CONTROL`: 0 or 1, partition-wide guest state.
pub const MSR_TSC_INVARIANT_CONTROL: u32 = 0x4000_0118;

/// `MSR_K7_HWCR`, AMD's hardware configuration register.
pub const MSR_AMD_HWCR: u32 = 0xc001_0015;
/// HWCR's `TscFreqSel`, bit 24: the TSC counts at the P0 frequency.
pub const HWCR_TSC_FREQ_SEL: u64 = 1 << 24;
/// `MSR_AMD64_DE_CFG`, AMD's decode configuration register.
pub const MSR_AMD_DE_CFG: u32 = 0xc001_1029;
/// DE_CFG's `LFENCE` serialization bit, bit 1: `LFENCE` is dispatch
/// serializing.
pub const DE_CFG_LFENCE_SERIALIZE: u64 = 1 << 1;

/// The AMD configuration MSRs that a partition with an AMD CPU profile
/// presents on every backend, with the value that every read returns:
///
/// - HWCR sets only `TscFreqSel`. Linux reads HWCR with an unchecked
///   `rdmsr` on every AMD CPU with an invariant TSC, which the time ABI's
///   CPUID always sets: a fault would fail the guest's `G_UNCHECKED_MSR`
///   check, and a clear `TscFreqSel` makes Linux warn that the TSC does not
///   count at the P0 frequency.
/// - DE_CFG sets only `LFENCE` serialization, which Linux otherwise tries to
///   set itself on a CPU whose CPUID does not enumerate it.
///
/// A write of the value that a read returns succeeds and changes nothing;
/// any other write raises #GP. Linux writes neither MSR on a CPU whose CPUID
/// clears the instructions-retired counter and CPUID faulting, as the
/// profiles' derivation policy does.
pub const AMD_CONFIG_MSRS: [(u32, u64); 2] = [
    (MSR_AMD_HWCR, HWCR_TSC_FREQ_SEL),
    (MSR_AMD_DE_CFG, DE_CFG_LFENCE_SERIALIZE),
];

/// Handles a guest read of `msr` on a partition with an AMD CPU profile:
/// the value of one of the [`AMD_CONFIG_MSRS`], or `None` for any other MSR,
/// which the backend handles as before.
pub fn amd_config_msr_read(msr: u32) -> Option<u64> {
    AMD_CONFIG_MSRS
        .iter()
        .find(|&&(index, _)| index == msr)
        .map(|&(_, value)| value)
}

/// Handles a guest write of `value` to `msr` on a partition with an AMD CPU
/// profile, with the same contract as [`amd_config_msr_read`]: writing the
/// value that a read returns succeeds, and any other value raises #GP.
pub fn amd_config_msr_write(msr: u32, value: u64) -> Option<Result<(), MsrError>> {
    let fixed = amd_config_msr_read(msr)?;
    Some(if value == fixed {
        Ok(())
    } else {
        Err(MsrError::InvalidAccess)
    })
}

/// The rates were declared before, with different values.
#[derive(Debug, Error)]
#[error("time ABI rates are already declared as {0:?}")]
pub struct AlreadyDeclared(pub DeclaredRates);

/// The identity MSR handler of one partition.
///
/// It is shared between the backend's MSR exit path and the worker's
/// `time-abi` state unit.
#[derive(Debug, Default)]
pub struct TimeAbiMsrs {
    rates: OnceLock<DeclaredRates>,
    tsc_invariant_control: AtomicU64,
}

/// Saved state of the identity MSRs.
#[derive(Debug, Clone, PartialEq, Eq, Protobuf, SavedStateRoot)]
#[mesh(package = "virt.time_abi")]
pub struct TimeAbiSavedState {
    /// `HV_X64_MSR_TSC_INVARIANT_CONTROL`.
    #[mesh(1)]
    pub tsc_invariant_control: u64,
}

impl TimeAbiMsrs {
    /// Returns a handler with no declared rates.
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares the rates. The worker calls this once, before any VP runs;
    /// declaring the same rates again is accepted.
    pub fn declare(&self, rates: DeclaredRates) -> Result<(), AlreadyDeclared> {
        let declared = *self.rates.get_or_init(|| rates);
        if declared != rates {
            return Err(AlreadyDeclared(declared));
        }
        Ok(())
    }

    /// Returns the declared rates, if any.
    pub fn declared(&self) -> Option<DeclaredRates> {
        self.rates.get().copied()
    }

    /// Handles a read of `msr` by `vp`.
    ///
    /// Returns `None` if `msr` is outside [`IDENTITY_MSR_RANGE`], so the
    /// backend handles it as before. `Some(Err(_))` means the backend must
    /// inject #GP; in-range MSRs never fall through to other handlers.
    pub fn read(&self, vp: VpIndex, msr: u32) -> Option<Result<u64, MsrError>> {
        if !IDENTITY_MSR_RANGE.contains(&msr) {
            return None;
        }
        let value = match msr {
            MSR_VP_INDEX => Ok(vp.index().into()),
            MSR_TSC_FREQUENCY | MSR_APIC_FREQUENCY => match self.declared() {
                Some(rates) if msr == MSR_TSC_FREQUENCY => Ok(rates.tsc_hz),
                Some(rates) => Ok(rates.apic_hz),
                None => {
                    tracelimit::warn_ratelimited!(
                        vp = vp.index(),
                        msr,
                        "identity frequency MSR read before the rates were declared"
                    );
                    Err(MsrError::InvalidAccess)
                }
            },
            MSR_TSC_INVARIANT_CONTROL => Ok(self.tsc_invariant_control()),
            _ => Err(MsrError::InvalidAccess),
        };
        Some(value)
    }

    /// Handles a write of `value` to `msr` by `vp`, with the same contract as
    /// [`Self::read`].
    pub fn write(&self, vp: VpIndex, msr: u32, value: u64) -> Option<Result<(), MsrError>> {
        let _ = vp;
        if !IDENTITY_MSR_RANGE.contains(&msr) {
            return None;
        }
        let result = match (msr, value) {
            (MSR_TSC_INVARIANT_CONTROL, 0 | 1) => {
                self.tsc_invariant_control.store(value, Ordering::Relaxed);
                Ok(())
            }
            _ => Err(MsrError::InvalidAccess),
        };
        Some(result)
    }

    /// Returns `HV_X64_MSR_TSC_INVARIANT_CONTROL`.
    pub fn tsc_invariant_control(&self) -> u64 {
        self.tsc_invariant_control.load(Ordering::Relaxed)
    }

    /// Resets the guest state to its power-on value. The declared rates are
    /// kept: they belong to the VM process.
    pub fn reset(&self) {
        self.tsc_invariant_control.store(0, Ordering::Relaxed);
    }

    /// Saves the guest state.
    pub fn save(&self) -> TimeAbiSavedState {
        TimeAbiSavedState {
            tsc_invariant_control: self.tsc_invariant_control(),
        }
    }

    /// Restores the guest state.
    pub fn restore(&self, state: TimeAbiSavedState) -> Result<(), RestoreError> {
        let TimeAbiSavedState {
            tsc_invariant_control,
        } = state;
        if tsc_invariant_control > 1 {
            return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                "HV_X64_MSR_TSC_INVARIANT_CONTROL value {tsc_invariant_control} is not 0 or 1"
            )));
        }
        self.tsc_invariant_control
            .store(tsc_invariant_control, Ordering::Relaxed);
        Ok(())
    }
}

impl Inspect for TimeAbiMsrs {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        if let Some(rates) = self.declared() {
            resp.field("tsc_hz", rates.tsc_hz)
                .field("apic_hz", rates.apic_hz);
        }
        resp.field("tsc_invariant_control", self.tsc_invariant_control());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time_abi::rate::LAPIC_HZ_HYPERV;

    const RATES: DeclaredRates = DeclaredRates {
        tsc_hz: 2_100_000_000,
        apic_hz: LAPIC_HZ_HYPERV,
    };

    fn gp<T: std::fmt::Debug>(result: Option<Result<T, MsrError>>) -> bool {
        matches!(result, Some(Err(MsrError::InvalidAccess)))
    }

    #[test]
    fn reads() {
        let msrs = TimeAbiMsrs::new();
        let vp = VpIndex::new(3);
        assert!(gp(msrs.read(vp, MSR_TSC_FREQUENCY)));
        assert!(gp(msrs.read(vp, MSR_APIC_FREQUENCY)));
        msrs.declare(RATES).unwrap();
        assert_eq!(msrs.read(vp, MSR_VP_INDEX).unwrap().unwrap(), 3);
        assert_eq!(
            msrs.read(vp, MSR_TSC_FREQUENCY).unwrap().unwrap(),
            2_100_000_000
        );
        assert_eq!(
            msrs.read(vp, MSR_APIC_FREQUENCY).unwrap().unwrap(),
            200_000_000
        );
        assert_eq!(
            msrs.read(vp, MSR_TSC_INVARIANT_CONTROL).unwrap().unwrap(),
            0
        );
        // Hyper-V MSRs the time ABI does not define raise #GP.
        for msr in [
            0x4000_0000,
            0x4000_0001,
            0x4000_0020,
            0x4000_0021,
            0x4000_0073,
            0x4000_01ff,
        ] {
            assert!(gp(msrs.read(vp, msr)), "{msr:#x}");
        }
        for msr in [0x3fff_ffff, 0x4000_0200, 0x10] {
            assert!(msrs.read(vp, msr).is_none(), "{msr:#x}");
        }
    }

    #[test]
    fn writes() {
        let msrs = TimeAbiMsrs::new();
        let vp = VpIndex::BSP;
        msrs.write(vp, MSR_TSC_INVARIANT_CONTROL, 1)
            .unwrap()
            .unwrap();
        assert_eq!(msrs.tsc_invariant_control(), 1);
        msrs.write(vp, MSR_TSC_INVARIANT_CONTROL, 0)
            .unwrap()
            .unwrap();
        assert_eq!(msrs.tsc_invariant_control(), 0);
        assert!(gp(msrs.write(vp, MSR_TSC_INVARIANT_CONTROL, 2)));
        for msr in [
            MSR_VP_INDEX,
            MSR_TSC_FREQUENCY,
            MSR_APIC_FREQUENCY,
            0x4000_0001,
        ] {
            assert!(gp(msrs.write(vp, msr, 0)), "{msr:#x}");
        }
        assert!(msrs.write(vp, 0x4000_0200, 0).is_none());
    }

    #[test]
    fn amd_config_msrs_read_fixed_values_and_accept_only_them() {
        assert_eq!(amd_config_msr_read(MSR_AMD_HWCR), Some(1 << 24));
        assert_eq!(amd_config_msr_read(MSR_AMD_DE_CFG), Some(1 << 1));
        for msr in [0xc001_0010, 0xc001_0114, 0x4000_0022, 0x10] {
            assert_eq!(amd_config_msr_read(msr), None, "{msr:#x}");
            assert!(amd_config_msr_write(msr, 0).is_none(), "{msr:#x}");
        }
        // Linux's msr_set_bit writes nothing when the bit is set already.
        assert!(matches!(
            amd_config_msr_write(MSR_AMD_HWCR, 1 << 24),
            Some(Ok(()))
        ));
        assert!(matches!(
            amd_config_msr_write(MSR_AMD_DE_CFG, 1 << 1),
            Some(Ok(()))
        ));
        // Setting IRPERF_EN or McStatusWrEn, or clearing a fixed bit.
        assert!(gp(amd_config_msr_write(MSR_AMD_HWCR, 1 << 24 | 1 << 30)));
        assert!(gp(amd_config_msr_write(MSR_AMD_HWCR, 1 << 24 | 1 << 18)));
        assert!(gp(amd_config_msr_write(MSR_AMD_HWCR, 0)));
        assert!(gp(amd_config_msr_write(MSR_AMD_DE_CFG, 0)));
    }

    #[test]
    fn declare_once() {
        let msrs = TimeAbiMsrs::new();
        assert_eq!(msrs.declared(), None);
        msrs.declare(RATES).unwrap();
        msrs.declare(RATES).unwrap();
        let other = DeclaredRates {
            tsc_hz: 2_000_000_000,
            ..RATES
        };
        assert_eq!(msrs.declare(other).unwrap_err().0, RATES);
        assert_eq!(msrs.declared(), Some(RATES));
    }

    #[test]
    fn save_restore_reset() {
        let msrs = TimeAbiMsrs::new();
        msrs.write(VpIndex::BSP, MSR_TSC_INVARIANT_CONTROL, 1)
            .unwrap()
            .unwrap();
        let saved = msrs.save();
        assert_eq!(saved.tsc_invariant_control, 1);

        let restored = TimeAbiMsrs::new();
        restored.restore(saved).unwrap();
        assert_eq!(restored.tsc_invariant_control(), 1);
        assert!(
            restored
                .restore(TimeAbiSavedState {
                    tsc_invariant_control: 2,
                })
                .is_err()
        );
        assert_eq!(restored.tsc_invariant_control(), 1);

        restored.declare(RATES).unwrap();
        restored.reset();
        assert_eq!(restored.tsc_invariant_control(), 0);
        assert_eq!(restored.declared(), Some(RATES));
    }
}
