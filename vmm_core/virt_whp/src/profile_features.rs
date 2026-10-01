// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! WHP processor features derived from a CPU profile.
//!
//! A WHP partition presents a CPU feature only when the feature's processor
//! feature bank bit (or XSAVE feature bit) is set, and some of those bits also
//! decide which MSRs the guest may use (for example `IA32_SPEC_CTRL`).
//! [`profile_features`] starts from the features WHP offers, clears every bit
//! whose CPUID feature the profile clears, and restricts the
//! `IA32_ARCH_CAPABILITIES` bits that the banks derive to the profile's pinned
//! value. The partition then behaves as the profile's CPUID describes.
//!
//! Bits without a CPUID feature, such as the nested-paging details, keep WHP's
//! capability. Features that no bit controls (for example PKU), the
//! descriptive leaves (caches, the brand string), and the time policy bits a
//! host lacks (ARAT and invariant TSC on Azure) reach the guest through the
//! CPUID results ([`profile_cpuid_results`] and the time ABI CPUID).
//!
//! The hardware test `features_control_the_mapped_cpuid_bits` checks the table
//! against what WHP presents on a host.

use cpu_profile::CpuProfile;
use cpu_profile::TIME_POLICY_BITS;
use cpu_profile::hv_banks;
use hvdef::HvX64PartitionProcessorFeatures as Bank0;
use hvdef::HvX64PartitionProcessorFeatures1 as Bank1;
use hvdef::HvX64PartitionProcessorXsaveFeatures as XsaveBank;
use virt::time_abi::TimeAbiCode;
use virt::time_abi::TimeAbiError;
use whp::abi::WHV_CPUID_OUTPUT;
use whp::abi::WHV_X64_CPUID_RESULT2;
use whp::abi::WHV_X64_CPUID_RESULT2_FLAGS;
use whp::abi::WHvX64CpuidResult2FlagSubleafSpecific;

/// `IA32_ARCH_CAPABILITIES`.
const MSR_ARCH_CAPABILITIES: u32 = 0x10a;

const EAX: usize = 0;
const EBX: usize = 1;
const ECX: usize = 2;
const EDX: usize = 3;

/// The extended feature leaf.
const X1: u32 = 0x8000_0001;
/// The advanced power management leaf.
const X7: u32 = 0x8000_0007;
/// The AMD extended feature identifiers leaf.
const X8: u32 = 0x8000_0008;

/// WHP processor features: feature banks 0 and 1, and the XSAVE features.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct WhpFeatures {
    pub banks: [u64; 2],
    pub xsave: u64,
}

impl WhpFeatures {
    fn word_mut(&mut self, bank: Bank) -> &mut u64 {
        match bank {
            Bank::Features0 => &mut self.banks[0],
            Bank::Features1 => &mut self.banks[1],
            Bank::Xsave => &mut self.xsave,
        }
    }

    #[cfg(test)]
    fn word(&self, bank: Bank) -> u64 {
        match bank {
            Bank::Features0 => self.banks[0],
            Bank::Features1 => self.banks[1],
            Bank::Xsave => self.xsave,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Bank {
    Features0,
    Features1,
    Xsave,
}

#[cfg(test)]
impl Bank {
    fn name(self) -> &'static str {
        match self {
            Bank::Features0 => "bank0",
            Bank::Features1 => "bank1",
            Bank::Xsave => "xsave",
        }
    }
}

/// A CPUID feature bit: leaf, subleaf, register (EAX through EDX), and bit.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct CpuidBit {
    leaf: u32,
    subleaf: u32,
    register: usize,
    bit: u32,
}

impl std::fmt::Display for CpuidBit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let register = ["eax", "ebx", "ecx", "edx"][self.register];
        write!(
            f,
            "{:#x}.{}:{}[{}]",
            self.leaf, self.subleaf, register, self.bit
        )
    }
}

const fn cpuid(leaf: u32, subleaf: u32, register: usize, bit: u32) -> CpuidBit {
    CpuidBit {
        leaf,
        subleaf,
        register,
        bit,
    }
}

/// A WHP feature bit and the CPUID feature bit it controls.
struct Feature {
    name: &'static str,
    bank: Bank,
    mask: u64,
    cpuid: CpuidBit,
}

impl Feature {
    fn name(&self) -> &'static str {
        self.name.trim_start_matches("with_")
    }

    fn is_time_policy(&self) -> bool {
        let bit = self.cpuid;
        TIME_POLICY_BITS.contains(&(bit.leaf, bit.subleaf, bit.register, 1 << bit.bit))
    }
}

macro_rules! feature {
    ($bank:ident, $ty:ident, $with:ident, $leaf:expr, $subleaf:expr, $register:expr, $bit:expr) => {
        Feature {
            name: stringify!($with),
            bank: Bank::$bank,
            mask: $ty::new().$with(true).into_bits(),
            cpuid: cpuid($leaf, $subleaf, $register, $bit),
        }
    };
}

macro_rules! bank0 {
    ($with:ident, $($cpuid:expr),+) => {
        feature!(Features0, Bank0, $with, $($cpuid),+)
    };
}

macro_rules! bank1 {
    ($with:ident, $($cpuid:expr),+) => {
        feature!(Features1, Bank1, $with, $($cpuid),+)
    };
}

macro_rules! xsave {
    ($with:ident, $($cpuid:expr),+) => {
        feature!(Xsave, XsaveBank, $with, $($cpuid),+)
    };
}

/// The WHP feature bits with a CPUID feature, as leaf, subleaf, register, and
/// bit. The leaves follow the Intel SDM and the AMD APM. The bits that the
/// banks derive into `IA32_ARCH_CAPABILITIES` are [`hv_banks`]'s.
const FEATURES: &[Feature] = &[
    // Bank 0.
    bank0!(with_sse3_support, 1, 0, ECX, 0),
    bank0!(with_lahf_sahf_support, X1, 0, ECX, 0),
    bank0!(with_ssse3_support, 1, 0, ECX, 9),
    bank0!(with_sse4_1_support, 1, 0, ECX, 19),
    bank0!(with_sse4_2_support, 1, 0, ECX, 20),
    bank0!(with_sse4a_support, X1, 0, ECX, 6),
    bank0!(with_xop_support, X1, 0, ECX, 11),
    bank0!(with_pop_cnt_support, 1, 0, ECX, 23),
    bank0!(with_cmpxchg16b_support, 1, 0, ECX, 13),
    bank0!(with_altmovcr8_support, X1, 0, ECX, 4),
    bank0!(with_lzcnt_support, X1, 0, ECX, 5),
    bank0!(with_mis_align_sse_support, X1, 0, ECX, 7),
    bank0!(with_mmx_ext_support, X1, 0, EDX, 22),
    bank0!(with_amd3d_now_support, X1, 0, EDX, 31),
    bank0!(with_extended_amd3d_now_support, X1, 0, EDX, 30),
    bank0!(with_page_1gb_support, X1, 0, EDX, 26),
    bank0!(with_aes_support, 1, 0, ECX, 25),
    bank0!(with_pclmulqdq_support, 1, 0, ECX, 1),
    bank0!(with_pcid_support, 1, 0, ECX, 17),
    bank0!(with_fma4_support, X1, 0, ECX, 16),
    bank0!(with_f16c_support, 1, 0, ECX, 29),
    bank0!(with_rd_rand_support, 1, 0, ECX, 30),
    bank0!(with_rd_wr_fs_gs_support, 7, 0, EBX, 0),
    bank0!(with_smep_support, 7, 0, EBX, 7),
    bank0!(with_enhanced_fast_string_support, 7, 0, EBX, 9),
    bank0!(with_bmi1_support, 7, 0, EBX, 3),
    bank0!(with_bmi2_support, 7, 0, EBX, 8),
    bank0!(with_movbe_support, 1, 0, ECX, 22),
    bank0!(with_dep_x87_fpu_save_support, 7, 0, EBX, 13),
    bank0!(with_rd_seed_support, 7, 0, EBX, 18),
    bank0!(with_adx_support, 7, 0, EBX, 19),
    bank0!(with_intel_prefetch_support, X1, 0, ECX, 8),
    bank0!(with_smap_support, 7, 0, EBX, 20),
    bank0!(with_hle_support, 7, 0, EBX, 4),
    bank0!(with_rtm_support, 7, 0, EBX, 11),
    bank0!(with_rdtscp_support, X1, 0, EDX, 27),
    bank0!(with_clflushopt_support, 7, 0, EBX, 23),
    bank0!(with_clwb_support, 7, 0, EBX, 24),
    bank0!(with_sha_support, 7, 0, EBX, 29),
    bank0!(with_x87_pointers_saved_support, X8, 0, EBX, 2),
    bank0!(with_invpcid_support, 7, 0, EBX, 10),
    bank0!(with_ibrs_support, 7, 0, EDX, 26),
    bank0!(with_stibp_support, 7, 0, EDX, 27),
    // CPUID enumerates IBPB with IBRS; WHP derives the bit from IBRS.
    bank0!(with_ibpb_support, 7, 0, EDX, 26),
    bank0!(with_mdd_support, 7, 0, EDX, 31),
    bank0!(with_fast_short_rep_mov_support, 7, 0, EDX, 4),
    bank0!(with_l1d_cache_flush_support, 7, 0, EDX, 28),
    bank0!(with_rd_pid_support, 7, 0, ECX, 22),
    bank0!(with_umip_support, 7, 0, ECX, 2),
    bank0!(with_mb_clear_support, 7, 0, EDX, 10),
    // Bank 1.
    bank1!(with_a_count_m_count_support, 6, 0, ECX, 0),
    bank1!(with_tsc_invariant_support, X7, 0, EDX, 8),
    bank1!(with_cl_zero_support, X8, 0, EBX, 0),
    bank1!(with_rdpru_support, X8, 0, EBX, 4),
    bank1!(with_la57_support, 7, 0, ECX, 16),
    bank1!(with_nested_virt_support, 1, 0, ECX, 5),
    bank1!(with_psfd_support, 7, 2, EDX, 0),
    bank1!(with_cet_ss_support, 7, 0, ECX, 7),
    bank1!(with_cet_ibt_support, 7, 0, EDX, 20),
    bank1!(with_enqcmd_support, 7, 0, ECX, 29),
    bank1!(with_umwait_tpause_support, 7, 0, ECX, 5),
    bank1!(with_movdiri_support, 7, 0, ECX, 27),
    bank1!(with_movdir64b_support, 7, 0, ECX, 28),
    bank1!(with_cldemote_support, 7, 0, ECX, 25),
    bank1!(with_serialize_support, 7, 0, EDX, 14),
    bank1!(with_tsc_deadline_tmr_support, 1, 0, ECX, 24),
    bank1!(with_tsc_adjust_support, 7, 0, EBX, 1),
    bank1!(with_fz_l_rep_movsb, 7, 1, EAX, 10),
    bank1!(with_fs_rep_stosb, 7, 1, EAX, 11),
    bank1!(with_fs_rep_cmpsb, 7, 1, EAX, 12),
    bank1!(with_tsx_ld_trk_support, 7, 0, EDX, 16),
    bank1!(with_cmpccxadd_support, 7, 1, EAX, 7),
    bank1!(with_bhi_dis_support, 7, 2, EDX, 4),
    bank1!(with_prefetch_i_support, 7, 1, EDX, 14),
    bank1!(with_sha512_support, 7, 1, EAX, 0),
    bank1!(with_sm3_support, 7, 1, EAX, 1),
    bank1!(with_sm4_support, 7, 1, EAX, 2),
    bank1!(with_lass_support, 7, 1, EAX, 6),
    // XSAVE features.
    xsave!(with_xsave_support, 1, 0, ECX, 26),
    xsave!(with_xsaveopt_support, 0xd, 1, EAX, 0),
    xsave!(with_avx_support, 1, 0, ECX, 28),
    xsave!(with_avx2_support, 7, 0, EBX, 5),
    xsave!(with_fma_support, 1, 0, ECX, 12),
    xsave!(with_mpx_support, 7, 0, EBX, 14),
    xsave!(with_avx512_support, 7, 0, EBX, 16),
    xsave!(with_avx512_dq_support, 7, 0, EBX, 17),
    xsave!(with_avx512_cd_support, 7, 0, EBX, 28),
    xsave!(with_avx512_bw_support, 7, 0, EBX, 30),
    xsave!(with_avx512_vl_support, 7, 0, EBX, 31),
    xsave!(with_xsave_comp_support, 0xd, 1, EAX, 1),
    xsave!(with_xsave_supervisor_support, 0xd, 1, EAX, 3),
    xsave!(with_xcr1_support, 0xd, 1, EAX, 2),
    xsave!(with_avx512_bitalg_support, 7, 0, ECX, 12),
    xsave!(with_avx512_ifma_support, 7, 0, EBX, 21),
    xsave!(with_avx512_vbmi_support, 7, 0, ECX, 1),
    xsave!(with_avx512_vbmi2_support, 7, 0, ECX, 6),
    xsave!(with_avx512_vnni_support, 7, 0, ECX, 11),
    xsave!(with_gfni_support, 7, 0, ECX, 8),
    xsave!(with_vaes_support, 7, 0, ECX, 9),
    xsave!(with_avx512_vpopcntdq_support, 7, 0, ECX, 14),
    xsave!(with_vpclmulqdq_support, 7, 0, ECX, 10),
    xsave!(with_avx512_bf16_support, 7, 1, EAX, 5),
    xsave!(with_avx512_vp2_intersect_support, 7, 0, EDX, 8),
    xsave!(with_avx512_fp16_support, 7, 0, EDX, 23),
    xsave!(with_xfd_support, 0xd, 1, EAX, 4),
    xsave!(with_amx_tile_support, 7, 0, EDX, 24),
    xsave!(with_amx_bf16_support, 7, 0, EDX, 22),
    xsave!(with_amx_int8_support, 7, 0, EDX, 25),
    xsave!(with_avx_vnni_support, 7, 1, EAX, 4),
    xsave!(with_avx_ifma_support, 7, 1, EAX, 23),
    xsave!(with_avx_ne_convert_support, 7, 1, EDX, 5),
    xsave!(with_avx_vnni_int8_support, 7, 1, EDX, 4),
    xsave!(with_avx_vnni_int16_support, 7, 1, EDX, 10),
    // CPUID enumerates AVX10 once; leaf 0x24 reports its vector lengths.
    xsave!(with_avx10_1_256_support, 7, 1, EDX, 19),
    xsave!(with_avx10_1_512_support, 7, 1, EDX, 19),
    xsave!(with_amx_fp16_support, 7, 1, EAX, 21),
];

/// How a profile pins a CPUID feature bit.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Pinned {
    Set,
    Clear,
    Unpinned,
}

/// Returns how `profile` pins `bit`. A leaf the profile does not list reads
/// zero, pinned.
fn pinned(profile: &CpuProfile, bit: CpuidBit) -> Pinned {
    let (value, mask) = profile
        .cpuid()
        .iter()
        .find(|entry| {
            let (leaf, subleaf) = entry.key();
            leaf == bit.leaf && subleaf.unwrap_or(0) == bit.subleaf
        })
        .map_or((0, !0), |entry| {
            (entry.values()[bit.register], entry.masks()[bit.register])
        });
    let flag = 1 << bit.bit;
    if mask & flag == 0 {
        Pinned::Unpinned
    } else if value & flag != 0 {
        Pinned::Set
    } else {
        Pinned::Clear
    }
}

/// Returns the WHP features for `profile`, given the features WHP offers.
///
/// Every feature bit whose CPUID feature the profile clears is cleared, and
/// the `IA32_ARCH_CAPABILITIES` bits the banks derive are restricted to the
/// profile's pinned value. Fails with `E_PROFILE_UNSUPPORTED` when the profile
/// sets a CPUID feature whose bit WHP does not offer (except the time policy
/// bits, which the CPUID results supply), or when the banks cannot present
/// the profile's `IA32_ARCH_CAPABILITIES`.
pub(crate) fn profile_features(
    profile: &CpuProfile,
    available: WhpFeatures,
) -> Result<WhpFeatures, TimeAbiError> {
    let mut features = available;
    let mut missing = Vec::new();
    for feature in FEATURES {
        let word = features.word_mut(feature.bank);
        match pinned(profile, feature.cpuid) {
            Pinned::Clear => *word &= !feature.mask,
            Pinned::Set if *word & feature.mask == 0 && !feature.is_time_policy() => {
                missing.push(feature.name().to_owned());
            }
            Pinned::Set | Pinned::Unpinned => {}
        }
    }
    hv_banks::restrict_banks_to_profile(profile, &mut features.banks);
    if let Some((value, mask)) = profile.msr(MSR_ARCH_CAPABILITIES) {
        let presented = hv_banks::arch_capabilities_from_banks(features.banks);
        if presented & mask != value & mask {
            missing.push(format!(
                "IA32_ARCH_CAPABILITIES {:#x} (the banks present {:#x} under mask {mask:#x})",
                value & mask,
                presented & mask
            ));
        }
    }
    if !missing.is_empty() {
        return Err(TimeAbiError::new(
            TimeAbiCode::ProfileUnsupported,
            format!(
                "WHP cannot present CPU profile {}: it lacks {}",
                profile.id(),
                missing.join(", ")
            ),
        ));
    }
    Ok(features)
}

/// Returns the CPUID results that present `profile`'s pinned bits: one
/// `CpuidResultList2` entry per profile leaf, with the profile's mask, so the
/// pinned bits (features, descriptors, the brand string) come from the
/// profile and the rest (runtime state such as OSXSAVE, and the VM-owned
/// topology fields) from WHP and OpenVMM. The VM-owned topology leaves are
/// left to OpenVMM's CPUID exits.
///
/// WHP returns these results for leaves without an exit, and as the default
/// result of a CPUID exit, on which OpenVMM applies the time ABI CPUID.
pub(crate) fn profile_cpuid_results(profile: &CpuProfile) -> Vec<WHV_X64_CPUID_RESULT2> {
    profile
        .cpuid()
        .iter()
        .filter_map(|entry| {
            let (leaf, subleaf) = entry.key();
            let mask = entry.masks();
            if cpu_profile::VM_OWNED_LEAVES.contains(&leaf) || mask == [0; 4] {
                return None;
            }
            let values = entry.values();
            let output = |registers: [u32; 4]| WHV_CPUID_OUTPUT {
                Eax: registers[EAX],
                Ebx: registers[EBX],
                Ecx: registers[ECX],
                Edx: registers[EDX],
            };
            Some(WHV_X64_CPUID_RESULT2 {
                Function: leaf,
                Index: subleaf.unwrap_or(0),
                VpIndex: 0,
                Flags: if subleaf.is_some() {
                    WHvX64CpuidResult2FlagSubleafSpecific
                } else {
                    WHV_X64_CPUID_RESULT2_FLAGS(0)
                },
                Output: output(std::array::from_fn(|i| values[i] & mask[i])),
                Mask: output(mask),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feature capabilities that WHP reported on the fleet's hosts (CPU
    /// fingerprints): prometheus28 (Skylake-SP, bare metal), the 8370C
    /// runners (Ice Lake, nested), and the 8573C runners (Emerald Rapids,
    /// nested).
    const PROMETHEUS28: WhpFeatures = WhpFeatures {
        banks: [0x1001_f9ff_e7f7_859f, 0x0000_000f_1086_0063],
        xsave: 0x3fff,
    };
    const AZURE_8370C: WhpFeatures = WhpFeatures {
        banks: [0x2e0a_8bff_e7f7_859f, 0x0001_000e_0000_00f1],
        xsave: 0x7f_ffdf,
    };
    const AZURE_8573C: WhpFeatures = WhpFeatures {
        banks: [0x0e0a_8bff_e7f7_859f, 0x0000_000c_0000_0051],
        xsave: 0x7f_ffdf,
    };

    const HOSTS: [(&str, WhpFeatures); 3] = [
        ("intel.skylake-sp.v1", PROMETHEUS28),
        ("intel.icelake-sp.v1", AZURE_8370C),
        ("intel.emeraldrapids.v1", AZURE_8573C),
    ];

    fn profile(id: &str) -> &'static CpuProfile {
        cpu_profile::pinned(id).unwrap_or_else(|| panic!("profile {id} is not pinned"))
    }

    fn has(features: &WhpFeatures, bank: Bank, mask: u64) -> bool {
        features.word(bank) & mask != 0
    }

    #[test]
    fn every_feature_is_one_distinct_bit() {
        let mut seen = Vec::new();
        for feature in FEATURES {
            assert_eq!(feature.mask.count_ones(), 1, "{}", feature.name());
            assert!(
                !seen.contains(&(feature.bank, feature.mask)),
                "{} repeats a bit",
                feature.name()
            );
            seen.push((feature.bank, feature.mask));
        }
    }

    #[test]
    fn pinned_profiles_derive_on_their_hosts() {
        for (id, available) in HOSTS {
            let profile = profile(id);
            let features = profile_features(profile, available).unwrap();
            for bank in [Bank::Features0, Bank::Features1, Bank::Xsave] {
                assert_eq!(
                    features.word(bank) & !available.word(bank),
                    0,
                    "{id}: {} gained bits",
                    bank.name()
                );
            }
            // Every mapped bit follows the profile's CPUID.
            for feature in FEATURES {
                let offered = has(&available, feature.bank, feature.mask);
                let kept = has(&features, feature.bank, feature.mask);
                match pinned(profile, feature.cpuid) {
                    Pinned::Clear => assert!(!kept, "{id}: {}", feature.name()),
                    Pinned::Set | Pinned::Unpinned => {
                        assert_eq!(kept, offered, "{id}: {}", feature.name())
                    }
                }
            }
            // The time policy: no TSC-deadline, TSC_ADJUST, or APERF/MPERF.
            for mask in [
                Bank1::new().with_tsc_deadline_tmr_support(true).into_bits(),
                Bank1::new().with_tsc_adjust_support(true).into_bits(),
                Bank1::new().with_a_count_m_count_support(true).into_bits(),
            ] {
                assert!(!has(&features, Bank::Features1, mask), "{id}: {mask:#x}");
            }
            let (value, mask) = profile.msr(MSR_ARCH_CAPABILITIES).unwrap();
            assert_eq!(
                hv_banks::arch_capabilities_from_banks(features.banks) & mask,
                value & mask,
                "{id}"
            );
        }
    }

    #[test]
    fn skylake_keeps_the_speculation_controls() {
        // The Skylake-SP profile pins SPEC_CTRL, STIBP, SSBD, and MD_CLEAR
        // (CPUID 7.0 EDX 0xac000400), which WHP's default features omit. The
        // Azure profiles pin none of them (EDX 0x20000010), as Azure's WHP
        // offers none.
        let features = profile_features(profile("intel.skylake-sp.v1"), PROMETHEUS28).unwrap();
        for mask in [
            Bank0::new().with_ibrs_support(true).into_bits(),
            Bank0::new().with_ibpb_support(true).into_bits(),
            Bank0::new().with_stibp_support(true).into_bits(),
            Bank0::new().with_mdd_support(true).into_bits(),
            Bank0::new().with_mb_clear_support(true).into_bits(),
        ] {
            assert!(has(&features, Bank::Features0, mask), "{mask:#x}");
        }
    }

    #[test]
    fn invariant_tsc_follows_the_host_and_the_cpuid_results_supply_it() {
        let invariant = Bank1::new().with_tsc_invariant_support(true).into_bits();
        let skylake = profile_features(profile("intel.skylake-sp.v1"), PROMETHEUS28).unwrap();
        assert!(has(&skylake, Bank::Features1, invariant));
        // Azure's WHP offers no invariant TSC; the profile still pins it, and
        // the derivation leaves it to the CPUID results.
        let icelake = profile_features(profile("intel.icelake-sp.v1"), AZURE_8370C).unwrap();
        assert!(!has(&icelake, Bank::Features1, invariant));
    }

    #[test]
    fn a_missing_feature_is_unsupported() {
        let mut available = PROMETHEUS28;
        available.xsave &= !XsaveBank::new().with_avx512_support(true).into_bits();
        available.banks[0] &= !Bank0::new().with_ibrs_support(true).into_bits();
        let error = profile_features(profile("intel.skylake-sp.v1"), available).unwrap_err();
        assert_eq!(error.code, TimeAbiCode::ProfileUnsupported);
        assert!(error.message.contains("avx512_support"), "{error}");
        assert!(error.message.contains("ibrs_support"), "{error}");
    }

    #[test]
    fn a_newer_profile_is_unsupported_on_an_older_host() {
        let error = profile_features(profile("intel.emeraldrapids.v1"), PROMETHEUS28).unwrap_err();
        assert_eq!(error.code, TimeAbiCode::ProfileUnsupported);
    }

    #[test]
    fn unlisted_leaves_read_zero_and_unmasked_bits_are_unpinned() {
        let skylake = profile("intel.skylake-sp.v1");
        // Skylake-SP lists no leaf 7.1, so its features are pinned clear.
        assert_eq!(pinned(skylake, cpuid(7, 1, EAX, 5)), Pinned::Clear);
        // OSXSAVE is guest state: the profile leaves it unpinned.
        assert_eq!(pinned(skylake, cpuid(1, 0, ECX, 27)), Pinned::Unpinned);
        assert_eq!(pinned(skylake, cpuid(1, 0, ECX, 26)), Pinned::Set);
    }

    #[test]
    fn profile_results_present_every_pinned_leaf() {
        for (id, _) in HOSTS {
            let profile = profile(id);
            let results = profile_cpuid_results(profile);
            let pinned_entries = profile
                .cpuid()
                .iter()
                .filter(|entry| {
                    !cpu_profile::VM_OWNED_LEAVES.contains(&entry.key().0)
                        && entry.masks() != [0; 4]
                })
                .count();
            assert_eq!(results.len(), pinned_entries, "{id}");
            for result in &results {
                assert!(!cpu_profile::VM_OWNED_LEAVES.contains(&result.Function));
                let entry = profile
                    .cpuid()
                    .iter()
                    .find(|entry| {
                        let (leaf, subleaf) = entry.key();
                        leaf == result.Function
                            && subleaf.unwrap_or(0) == result.Index
                            && subleaf.is_some()
                                == result.Flags.is_set(WHvX64CpuidResult2FlagSubleafSpecific)
                    })
                    .unwrap();
                let output = [
                    result.Output.Eax,
                    result.Output.Ebx,
                    result.Output.Ecx,
                    result.Output.Edx,
                ];
                let mask = [
                    result.Mask.Eax,
                    result.Mask.Ebx,
                    result.Mask.Ecx,
                    result.Mask.Edx,
                ];
                assert_eq!(mask, entry.masks(), "{id} {:#x}", result.Function);
                for register in 0..4 {
                    assert_eq!(
                        output[register],
                        entry.values()[register] & mask[register],
                        "{id} {:#x}",
                        result.Function
                    );
                }
            }
            // The brand string is the profile's generic one, not the host's.
            let brand: String = [0x8000_0002, 0x8000_0003, 0x8000_0004]
                .iter()
                .flat_map(|&leaf| {
                    let result = results.iter().find(|r| r.Function == leaf).unwrap();
                    [
                        result.Output.Eax,
                        result.Output.Ebx,
                        result.Output.Ecx,
                        result.Output.Edx,
                    ]
                })
                .flat_map(u32::to_le_bytes)
                .take_while(|&byte| byte != 0)
                .map(char::from)
                .collect();
            assert!(
                brand.starts_with("Intel(R) Xeon(R) Processor ("),
                "{id}: {brand}"
            );
            // Runtime state stays with the hypervisor: OSXSAVE is unpinned.
            let leaf1 = results.iter().find(|r| r.Function == 1).unwrap();
            assert_eq!(leaf1.Mask.Ecx & (1 << 27), 0, "{id}");
        }
    }
}

#[cfg(test)]
mod whp_tests {
    //! Tests that need WHP. Run them on a host with
    //! `cargo test -p virt_whp -- --ignored`.

    use super::*;
    use cpu_profile::HostCpuSignature;
    use whp::abi::WHV_PROCESSOR_FEATURES;
    use whp::abi::WHV_PROCESSOR_FEATURES1;
    use whp::abi::WHV_PROCESSOR_XSAVE_FEATURES;

    /// The leaves whose feature bits the WHP features control.
    const FEATURE_LEAVES: [(u32, u32); 14] = [
        (1, 0),
        (6, 0),
        (7, 0),
        (7, 1),
        (7, 2),
        (0xd, 0),
        (0xd, 1),
        (0x14, 0),
        (0x19, 0),
        (0x24, 0),
        (X1, 0),
        (X7, 0),
        (X8, 0),
        (0x8000_0021, 0),
    ];

    fn available() -> WhpFeatures {
        let banks = whp::capabilities::processor_features().unwrap();
        let xsave = whp::capabilities::processor_xsave_features().unwrap();
        WhpFeatures {
            banks: [banks.bank0.0, banks.bank1.0],
            xsave: xsave.0,
        }
    }

    /// Creates a partition with `vp_count` VPs, `features`, and the CPUID
    /// `results`.
    fn probe_partition_with(
        features: WhpFeatures,
        vp_count: u32,
        results: &[WHV_X64_CPUID_RESULT2],
    ) -> Result<whp::Partition, whp::WHvError> {
        let mut config = whp::PartitionConfig::new()?;
        config.set_property(whp::PartitionProperty::ProcessorCount(vp_count))?;
        config.set_property(whp::PartitionProperty::LocalApicEmulationMode(
            whp::abi::WHvX64LocalApicEmulationModeXApic,
        ))?;
        let mut banks = whp::capabilities::processor_features()?;
        banks.bank0 = WHV_PROCESSOR_FEATURES(features.banks[0]);
        banks.bank1 = WHV_PROCESSOR_FEATURES1(features.banks[1]);
        config.set_property(whp::PartitionProperty::ProcessorFeaturesBanks(banks))?;
        config.set_property(whp::PartitionProperty::ProcessorXsaveFeatures(
            WHV_PROCESSOR_XSAVE_FEATURES(features.xsave),
        ))?;
        if !results.is_empty() {
            config.set_property(whp::PartitionProperty::CpuidResultList2(results))?;
        }
        let partition = config.create()?;
        for vp in 0..vp_count {
            partition.create_vp(vp).create()?;
        }
        Ok(partition)
    }

    /// Creates a one-VP partition with `features`.
    fn probe_partition(features: WhpFeatures) -> Result<whp::Partition, whp::WHvError> {
        probe_partition_with(features, 1, &[])
    }

    fn read_vp(partition: &whp::Partition, vp: u32, leaf: u32, subleaf: u32) -> [u32; 4] {
        let output = partition.vp(vp).get_cpuid_output(leaf, subleaf).unwrap();
        [output.Eax, output.Ebx, output.Ecx, output.Edx]
    }

    fn read(partition: &whp::Partition, leaf: u32, subleaf: u32) -> [u32; 4] {
        read_vp(partition, 0, leaf, subleaf)
    }

    fn feature_cpuid(partition: &whp::Partition) -> Vec<[u32; 4]> {
        FEATURE_LEAVES
            .iter()
            .map(|&(leaf, subleaf)| read(partition, leaf, subleaf))
            .collect()
    }

    /// The CPUID bits set in `base` and clear in `probe`.
    fn removed(base: &[[u32; 4]], probe: &[[u32; 4]]) -> Vec<CpuidBit> {
        let mut bits = Vec::new();
        for (index, &(leaf, subleaf)) in FEATURE_LEAVES.iter().enumerate() {
            for register in 0..4 {
                // Leaf 0xD.0 reports XSAVE sizes and components, and leaf 7.0
                // EAX the last subleaf; neither holds features.
                if (leaf, subleaf) == (0xd, 0) || (leaf, subleaf, register) == (7, 0, EAX) {
                    continue;
                }
                let lost = base[index][register] & !probe[index][register];
                for bit in 0..32 {
                    if lost & (1 << bit) != 0 {
                        bits.push(cpuid(leaf, subleaf, register, bit));
                    }
                }
            }
        }
        bits
    }

    /// Whether another feature maps the same CPUID bit (as IBRS and IBPB
    /// do): WHP may derive the bit from either.
    fn shared(feature: &Feature) -> bool {
        FEATURES
            .iter()
            .any(|other| !std::ptr::eq(other, feature) && other.cpuid == feature.cpuid)
    }

    fn list(bits: &[CpuidBit]) -> String {
        if bits.is_empty() {
            return "none".to_owned();
        }
        bits.iter()
            .map(|bit| bit.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Clears each feature bit WHP offers, one at a time, and checks that the
    /// CPUID bit the table maps to it disappears. Prints what every bit
    /// controls on this host.
    #[test]
    #[ignore = "requires WHP"]
    fn features_control_the_mapped_cpuid_bits() {
        let available = available();
        let base = feature_cpuid(&probe_partition(available).unwrap());
        println!(
            "available: bank0 {:#x} bank1 {:#x} xsave {:#x}",
            available.banks[0], available.banks[1], available.xsave
        );
        let mut failures = Vec::new();
        for bank in [Bank::Features0, Bank::Features1, Bank::Xsave] {
            let word = available.word(bank);
            for bit in (0..64).filter(|bit| word & (1 << bit) != 0) {
                let mask = 1u64 << bit;
                let feature = FEATURES
                    .iter()
                    .find(|feature| feature.bank == bank && feature.mask == mask);
                let name = feature.map_or("(unmapped)", |feature| feature.name());
                let mut features = available;
                *features.word_mut(bank) &= !mask;
                let probe = match probe_partition(features) {
                    Ok(partition) => feature_cpuid(&partition),
                    Err(err) => {
                        println!("{} bit {bit} {name}: rejected: {err}", bank.name());
                        continue;
                    }
                };
                let removed = removed(&base, &probe);
                let mut verdict = "";
                if let Some(feature) = feature {
                    let index = FEATURE_LEAVES
                        .iter()
                        .position(|&key| key == (feature.cpuid.leaf, feature.cpuid.subleaf))
                        .unwrap();
                    let offered =
                        base[index][feature.cpuid.register] & (1 << feature.cpuid.bit) != 0;
                    if offered && !shared(feature) && !removed.contains(&feature.cpuid) {
                        verdict = " MISMATCH";
                        failures.push(format!(
                            "{} bit {bit} {name}: expected {} removed {}",
                            bank.name(),
                            feature.cpuid,
                            list(&removed)
                        ));
                    }
                }
                println!(
                    "{} bit {bit} {name}: removes {}{verdict}",
                    bank.name(),
                    list(&removed)
                );
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// Derives the features for this host's profile and reports the profile
    /// bits that the features alone do not present (the CPUID results must
    /// supply them). Fails if a mapped feature bit disagrees.
    #[test]
    #[ignore = "requires WHP"]
    fn host_profile_features_present_the_profile() {
        let profile = match cpu_profile::select_auto(&HostCpuSignature::current()) {
            Ok(profile) => profile,
            Err(err) => {
                println!("skipped: no profile for this host: {err}");
                return;
            }
        };
        let available = available();
        let features = profile_features(profile, available).unwrap();
        println!(
            "{}: bank0 {:#x} -> {:#x}, bank1 {:#x} -> {:#x}, xsave {:#x} -> {:#x}",
            profile.id(),
            available.banks[0],
            features.banks[0],
            available.banks[1],
            features.banks[1],
            available.xsave,
            features.xsave
        );
        let partition = probe_partition(features).unwrap();
        let mut failures = Vec::new();
        for entry in profile.cpuid() {
            let (leaf, subleaf) = entry.key();
            let subleaf = subleaf.unwrap_or(0);
            let actual = read(&partition, leaf, subleaf);
            for register in 0..4 {
                let mask = entry.masks()[register];
                let differs = (actual[register] ^ entry.values()[register]) & mask;
                if differs == 0 {
                    continue;
                }
                let bits: Vec<_> = (0..32)
                    .filter(|bit| differs & (1 << bit) != 0)
                    .map(|bit| cpuid(leaf, subleaf, register, bit))
                    .collect();
                let mapped: Vec<_> = bits
                    .iter()
                    .filter(|&&bit| {
                        FEATURES
                            .iter()
                            .any(|feature| feature.cpuid == bit && !feature.is_time_policy())
                    })
                    .copied()
                    .collect();
                println!(
                    "{leaf:#x}.{subleaf} {}: actual {:#010x} profile {:#010x} mask {mask:#010x} ({})",
                    ["eax", "ebx", "ecx", "edx"][register],
                    actual[register],
                    entry.values()[register],
                    list(&bits)
                );
                if !mapped.is_empty() {
                    failures.push(list(&mapped));
                }
            }
        }
        assert!(failures.is_empty(), "mapped bits differ: {failures:?}");
    }

    fn output(registers: [u32; 4]) -> WHV_CPUID_OUTPUT {
        WHV_CPUID_OUTPUT {
            Eax: registers[0],
            Ebx: registers[1],
            Ecx: registers[2],
            Edx: registers[3],
        }
    }

    fn result2(
        leaf: u32,
        subleaf: Option<u32>,
        vp: Option<u32>,
        value: [u32; 4],
        mask: [u32; 4],
    ) -> WHV_X64_CPUID_RESULT2 {
        let mut flags = WHV_X64_CPUID_RESULT2_FLAGS(0);
        if subleaf.is_some() {
            flags |= WHvX64CpuidResult2FlagSubleafSpecific;
        }
        if vp.is_some() {
            flags |= whp::abi::WHvX64CpuidResult2FlagVpSpecific;
        }
        WHV_X64_CPUID_RESULT2 {
            Function: leaf,
            Index: subleaf.unwrap_or(0),
            VpIndex: vp.unwrap_or(0),
            Flags: flags,
            Output: output(value),
            Mask: output(mask),
        }
    }

    /// Programs CPUID results with `CpuidResultList2` (a whole descriptive
    /// leaf, a masked feature bit the banks set, a masked bit, and a subleaf
    /// before setup; per-VP values after the VPs exist) and checks what
    /// `WHvGetVirtualProcessorCpuidOutput` returns.
    #[test]
    #[ignore = "requires WHP"]
    fn cpuid_result_list2_reaches_the_cpuid_output() {
        let available = available();
        let native = probe_partition(available).unwrap();
        let native_7 = read(&native, 7, 0);
        let native_x7 = read(&native, X7, 0);
        let native_xsave1 = read(&native, 0xd, 1);
        drop(native);
        assert_ne!(native_7[EBX] & 1, 0, "the host lacks FSGSBASE");

        let descriptors = [0x7603_6301, 0x00f0_b5ff, 0, 0x00c3_0000];
        let results = [
            result2(2, None, None, descriptors, [!0; 4]),
            result2(7, Some(0), None, [0; 4], [0, 1, 0, 0]),
            result2(X7, None, None, [0, 0, 0, 1 << 8], [0, 0, 0, 1 << 8]),
            result2(0xd, Some(1), None, [0x5, 0, 0, 0], [0xf, 0, 0, 0]),
        ];
        let partition = match probe_partition_with(available, 2, &results) {
            Ok(partition) => partition,
            Err(err) => panic!("WHP rejects CpuidResultList2: {err}"),
        };
        let mut expected_7 = native_7;
        expected_7[EBX] &= !1;
        let mut expected_x7 = native_x7;
        expected_x7[EDX] |= 1 << 8;
        let mut expected_xsave1 = native_xsave1;
        expected_xsave1[EAX] = (native_xsave1[EAX] & !0xf) | 0x5;
        let mut checks = vec![
            ("leaf 2 (whole leaf)", read(&partition, 2, 0), descriptors),
            (
                "leaf 7.0 (FSGSBASE masked clear)",
                read(&partition, 7, 0),
                expected_7,
            ),
            (
                "leaf 0x80000007 (invariant TSC masked set)",
                read(&partition, X7, 0),
                expected_x7,
            ),
            (
                "leaf 0xd.1 (subleaf, masked)",
                read(&partition, 0xd, 1),
                expected_xsave1,
            ),
            ("leaf 2 on VP 1", read_vp(&partition, 1, 2, 0), descriptors),
        ];
        let per_vp = [
            result2(0x16, None, Some(0), [100, 200, 300, 0], [!0; 4]),
            result2(0x16, None, Some(1), [101, 201, 301, 1], [!0; 4]),
        ];
        let mut all = results.to_vec();
        all.extend_from_slice(&per_vp);
        match partition.set_property(whp::PartitionProperty::CpuidResultList2(&all)) {
            Ok(()) => {
                checks.push((
                    "leaf 0x16 on VP 0",
                    read_vp(&partition, 0, 0x16, 0),
                    [100, 200, 300, 0],
                ));
                checks.push((
                    "leaf 0x16 on VP 1",
                    read_vp(&partition, 1, 0x16, 0),
                    [101, 201, 301, 1],
                ));
            }
            Err(err) => println!("per-VP results after setup: rejected: {err}"),
        }
        let mut failures = Vec::new();
        for (name, actual, expected) in checks {
            let verdict = if actual == expected { "ok" } else { "DIFFERS" };
            println!("{name}: {actual:08x?} expected {expected:08x?} {verdict}");
            if actual != expected {
                failures.push(name);
            }
        }
        assert!(failures.is_empty(), "{failures:?}");
    }

    /// Times what a WHP `supported_cpu_surface()` costs with a probe
    /// partition: creating a one-VP partition with every feature WHP offers,
    /// reading its whole CPUID, and tearing it down.
    #[test]
    #[ignore = "requires WHP"]
    fn supported_surface_probe_cost() {
        for round in 0..5 {
            let start = std::time::Instant::now();
            let partition = probe_partition(available()).unwrap();
            let created = start.elapsed();
            let mut reads = 0;
            let entries = cpu_profile::cpuid::enumerate(|leaf, subleaf| {
                reads += 1;
                partition
                    .vp(0)
                    .get_cpuid_output(leaf, subleaf)
                    .map(|output| [output.Eax, output.Ebx, output.Ecx, output.Edx])
            })
            .unwrap();
            let enumerated = start.elapsed();
            drop(partition);
            let total = start.elapsed();
            println!(
                "round {round}: create {created:?}, {reads} reads ({} entries) {:?}, teardown {:?}, total {total:?}",
                entries.len(),
                enumerated - created,
                total - enumerated
            );
        }
    }

    /// Runs `cpuid; hlt` in real mode at the reset vector and checks how
    /// `CpuidResultList2` reaches the guest: a leaf that also exits reports
    /// the programmed result as the exit's default result, which OpenVMM's
    /// exit handler starts from, and a leaf that does not exit returns it
    /// directly. The time ABI exits some profile leaves (1, 4, 6, 7, ...), so
    /// their profile values depend on the former.
    #[test]
    #[ignore = "requires WHP"]
    fn cpuid_result_list2_feeds_exit_defaults() {
        const EXIT_LEAF: u32 = 0x16;
        const PLAIN_LEAF: u32 = 0x8000_0002;
        let exit_value = [0x1111_0001, 0x2222_0002, 0x3333_0003, 0x4444_0004];
        let plain_value = [0x4141_4141, 0x4242_4242, 0x4343_4343, 0x4444_4444];
        let results = [
            result2(EXIT_LEAF, None, None, exit_value, [!0; 4]),
            result2(PLAIN_LEAF, None, None, plain_value, [!0; 4]),
        ];
        let mut config = whp::PartitionConfig::new().unwrap();
        config
            .set_property(whp::PartitionProperty::ProcessorCount(1))
            .unwrap();
        config
            .set_property(whp::PartitionProperty::ExtendedVmExits(
                whp::abi::WHV_EXTENDED_VM_EXITS::X64CpuidExit,
            ))
            .unwrap();
        config
            .set_property(whp::PartitionProperty::CpuidExitList(&[EXIT_LEAF]))
            .unwrap();
        config
            .set_property(whp::PartitionProperty::CpuidResultList2(&results))
            .unwrap();
        let partition = config.create().unwrap();
        partition.create_vp(0).create().unwrap();

        // At 0xffff0: cpuid; hlt; jmp back to the cpuid.
        let layout = std::alloc::Layout::from_size_align(4096, 4096).unwrap();
        // SAFETY: the layout has a nonzero size.
        let page = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!page.is_null());
        let code = [0x0f, 0xa2, 0xf4, 0xeb, 0xfb];
        // SAFETY: `page` is a fresh allocation of 4096 bytes.
        unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), page.add(0xff0), code.len()) };
        let rwx = whp::abi::WHV_MAP_GPA_RANGE_FLAGS(
            whp::abi::WHvMapGpaRangeFlagRead.0
                | whp::abi::WHvMapGpaRangeFlagWrite.0
                | whp::abi::WHvMapGpaRangeFlagExecute.0,
        );
        // SAFETY: `page` stays allocated until after the partition is dropped.
        unsafe { partition.map_range(None, page, 4096, 0xff000, rwx) }.unwrap();

        let vp = partition.vp(0);
        let registers = |vp: &whp::Processor<'_>| {
            [
                whp::Register64::Rax,
                whp::Register64::Rbx,
                whp::Register64::Rcx,
                whp::Register64::Rdx,
            ]
            .map(|register| vp.get_register(register).unwrap() as u32)
        };
        vp.set_register(whp::Register64::Rax, EXIT_LEAF.into())
            .unwrap();
        vp.set_register(whp::Register64::Rcx, 0).unwrap();
        let mut runner = vp.runner();

        // The exiting leaf: its default result is the programmed one.
        let (default, next_rip) = match runner.run().unwrap() {
            whp::Exit {
                vp_context,
                reason: whp::ExitReason::Cpuid(info),
            } => (
                [
                    info.DefaultResultRax as u32,
                    info.DefaultResultRbx as u32,
                    info.DefaultResultRcx as u32,
                    info.DefaultResultRdx as u32,
                ],
                vp_context.Rip + u64::from(vp_context.InstructionLength()),
            ),
            exit => panic!("expected a CPUID exit: {exit:#x?}"),
        };
        println!("exit leaf {EXIT_LEAF:#x}: default result {default:08x?}");
        // Complete the CPUID with the default result and continue to the HLT.
        for (register, value) in [
            whp::Register64::Rax,
            whp::Register64::Rbx,
            whp::Register64::Rcx,
            whp::Register64::Rdx,
        ]
        .into_iter()
        .zip(default)
        {
            vp.set_register(register, value.into()).unwrap();
        }
        vp.set_register(whp::Register64::Rip, next_rip).unwrap();
        match runner.run().unwrap().reason {
            whp::ExitReason::Halt => {}
            reason => panic!("expected a halt: {reason:#x?}"),
        }

        // The plain leaf: the guest gets the programmed result directly.
        vp.set_register(whp::Register64::Rax, PLAIN_LEAF.into())
            .unwrap();
        vp.set_register(whp::Register64::Rcx, 0).unwrap();
        match runner.run().unwrap().reason {
            whp::ExitReason::Halt => {}
            reason => panic!("expected a halt: {reason:#x?}"),
        }
        let plain = registers(&vp);
        println!("plain leaf {PLAIN_LEAF:#x}: guest result {plain:08x?}");

        drop(partition);
        // SAFETY: the partition that mapped the page is gone.
        unsafe { std::alloc::dealloc(page, layout) };
        assert_eq!(default, exit_value, "the exit's default result");
        assert_eq!(plain, plain_value, "the guest's result");
    }
}
