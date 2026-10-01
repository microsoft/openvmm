// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! How the Hyper-V backends, MSHV and WHP, present `IA32_ARCH_CAPABILITIES`.
//!
//! Neither backend lets the VMM intercept `IA32_ARCH_CAPABILITIES`: the
//! hypervisor answers it, deriving the immunity and capability bits from the
//! partition's processor feature banks. A profile therefore pins those bits
//! through the banks. The bank layout is the hypervisor's
//! `HV_PARTITION_PROCESSOR_FEATURES`, which WHP's `WHV_PROCESSOR_FEATURES`
//! and `WHV_PROCESSOR_FEATURES1` share.

use crate::fingerprint::BackendFingerprint;
use crate::fingerprint::IA32_ARCH_CAPABILITIES;
use crate::profile::CpuProfile;
use crate::surface::SupportedMsr;

/// The `IA32_ARCH_CAPABILITIES` bits that Hyper-V derives from the processor
/// feature banks, as (MSR bit, bank, bank bit).
pub const ARCH_CAPABILITIES_BANK_BITS: [(u32, usize, u32); 14] = [
    // RDCL_NO: RdclNo.
    (0, 0, 51),
    // IBRS_ALL: IbrsAllSupport.
    (1, 0, 52),
    // SKIP_L1DFL_VMENTRY: SkipL1df (reserved in WHP).
    (3, 0, 53),
    // SSB_NO: SsbNo.
    (4, 0, 54),
    // MDS_NO: MdsNoSupport.
    (5, 0, 59),
    // TSX_CTRL: TsxCtrlSupport.
    (7, 0, 62),
    // TAA_NO: TaaNoSupport.
    (8, 0, 61),
    // SBDR_SSDP_NO: SbdrSsdpNoSupport.
    (13, 1, 25),
    // FBSDP_NO: FbsdpNoSupport.
    (14, 1, 26),
    // PSDP_NO: PsdpNoSupport.
    (15, 1, 27),
    // FB_CLEAR: FbClearSupport.
    (17, 1, 28),
    // BHI_NO: BhiNoSupport.
    (20, 1, 43),
    // GDS_NO: GdsNoSupport.
    (26, 1, 39),
    // RFDS_NO: RfdsNoSupport.
    (27, 1, 48),
];

/// The `IA32_ARCH_CAPABILITIES` bits that the banks control.
pub const ARCH_CAPABILITIES_BANK_MASK: u64 = {
    let mut mask = 0;
    let mut i = 0;
    while i < ARCH_CAPABILITIES_BANK_BITS.len() {
        mask |= 1 << ARCH_CAPABILITIES_BANK_BITS[i].0;
        i += 1;
    }
    mask
};

/// Returns the `IA32_ARCH_CAPABILITIES` bits that a partition with processor
/// feature `banks` presents.
pub fn arch_capabilities_from_banks(banks: [u64; 2]) -> u64 {
    ARCH_CAPABILITIES_BANK_BITS
        .iter()
        .filter(|&&(_, bank, bank_bit)| banks[bank] & (1 << bank_bit) != 0)
        .fold(0, |value, &(bit, ..)| value | 1 << bit)
}

/// Returns what a Hyper-V backend whose host offers processor feature
/// `banks` can present in `IA32_ARCH_CAPABILITIES`.
///
/// Bits the banks do not derive are reported neither supported nor
/// controllable, so a profile can pin them only to zero.
pub fn arch_capabilities_msr(banks: [u64; 2]) -> SupportedMsr {
    SupportedMsr {
        index: IA32_ARCH_CAPABILITIES,
        supported: arch_capabilities_from_banks(banks),
        controllable: ARCH_CAPABILITIES_BANK_MASK,
    }
}

/// Clears from `banks` every bank bit whose `IA32_ARCH_CAPABILITIES` bit
/// `profile` pins clear, so the hypervisor presents the profile's value.
///
/// The backend must also verify the profile (see
/// [`verify_support`](crate::verify_support)), which ensures the banks
/// already have every bit the profile pins set.
pub fn restrict_banks_to_profile(profile: &CpuProfile, banks: &mut [u64; 2]) {
    let Some((value, mask)) = profile.msr(IA32_ARCH_CAPABILITIES) else {
        return;
    };
    for &(bit, bank, bank_bit) in &ARCH_CAPABILITIES_BANK_BITS {
        if mask & !value & (1 << bit) != 0 {
            banks[bank] &= !(1 << bank_bit);
        }
    }
}

/// Returns the processor feature banks 0 and 1 a Hyper-V fingerprint
/// recorded, if any.
pub(crate) fn fingerprint_banks(backend: &BackendFingerprint) -> Option<[u64; 2]> {
    let bank = |name: &str| backend.feature_banks.get(name).map(|value| value.0);
    match backend.name.as_str() {
        "mshv" => Some([
            bank("mshv.host.ProcessorFeatures0")?,
            bank("mshv.host.ProcessorFeatures1").unwrap_or(0),
        ]),
        "whp" => Some(
            match (
                bank("whp.capability.ProcessorFeaturesBanks.bank0"),
                bank("whp.capability.ProcessorFeaturesBanks.bank1"),
            ) {
                (Some(bank0), bank1) => [bank0, bank1.unwrap_or(0)],
                (None, _) => [bank("whp.capability.ProcessorFeatures")?, 0],
            },
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn derives_arch_capabilities_from_azure_banks() {
        // Ice Lake on Azure WHP: RdclNo, MdsNo, TaaNo; RfdsNo in bank 1.
        let icelake_whp = [0x2e0a_8bff_e7f7_859f, 0x0001_000e_0000_00f1];
        assert_eq!(arch_capabilities_from_banks(icelake_whp), 0x0800_0121);
        // Emerald Rapids on Azure MSHV: RdclNo, SkipL1df, and MdsNo.
        let emeraldrapids_mshv = [0x0e2a_8bff_fff7_859f, 0x0000_000c_0000_0051];
        assert_eq!(arch_capabilities_from_banks(emeraldrapids_mshv), 0x29);
        // WHP reserves SkipL1df.
        let emeraldrapids_whp = [0x0e0a_8bff_e7f7_859f, 0x0000_000c_0000_0051];
        assert_eq!(arch_capabilities_from_banks(emeraldrapids_whp), 0x21);
    }

    #[test]
    fn the_mask_covers_every_mapped_bit_once() {
        assert_eq!(
            ARCH_CAPABILITIES_BANK_MASK.count_ones() as usize,
            ARCH_CAPABILITIES_BANK_BITS.len()
        );
        assert_eq!(
            arch_capabilities_from_banks([!0, !0]),
            ARCH_CAPABILITIES_BANK_MASK
        );
        let msr = arch_capabilities_msr([0, 0]);
        assert_eq!(
            (msr.supported, msr.controllable),
            (0, ARCH_CAPABILITIES_BANK_MASK)
        );
    }

    #[test]
    fn restricts_the_banks_to_the_profile() {
        let icelake = crate::test_support::profile("intel.icelake-sp.v1");
        let mut banks = [!0, !0];
        restrict_banks_to_profile(icelake, &mut banks);
        assert_eq!(arch_capabilities_from_banks(banks), 0x0800_0121);
        // Bits that derive no IA32_ARCH_CAPABILITIES bit are untouched.
        let mapped = |bank| {
            ARCH_CAPABILITIES_BANK_BITS
                .iter()
                .filter(|&&(_, b, _)| b == bank)
                .fold(0u64, |mask, &(.., bit)| mask | 1 << bit)
        };
        assert_eq!(banks[0] | mapped(0), !0);
        assert_eq!(banks[1] | mapped(1), !0);

        let skylake = crate::test_support::profile("intel.skylake-sp.v1");
        let mut banks = [!0, !0];
        restrict_banks_to_profile(skylake, &mut banks);
        assert_eq!(arch_capabilities_from_banks(banks), 0);
    }
}
