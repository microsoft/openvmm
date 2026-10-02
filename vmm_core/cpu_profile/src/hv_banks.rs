// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! How the Hyper-V backends, MSHV and WHP, present a CPU profile through a
//! partition's processor features.
//!
//! A Hyper-V partition presents a CPU feature only when the feature's
//! processor feature bank bit, or XSAVE feature bit, is set, and some of
//! those bits also decide which MSRs the guest may use (for example
//! `IA32_SPEC_CTRL`). [`HV_FEATURES`] maps each such bit to the CPUID feature
//! bit it controls. [`profile_features`] starts from the features the
//! hypervisor offers and clears every bit whose CPUID feature the profile
//! clears. The partition then behaves as the profile's CPUID describes.
//!
//! Neither backend lets the VMM intercept `IA32_ARCH_CAPABILITIES`: the
//! hypervisor answers it, deriving the immunity and capability bits from the
//! banks ([`ARCH_CAPABILITIES_BANK_BITS`]). A profile therefore pins those
//! bits through the banks too ([`restrict_banks_to_profile`]).
//!
//! The layout is the hypervisor's `HV_PARTITION_PROCESSOR_FEATURES` and
//! `HV_PARTITION_PROCESSOR_XSAVE_FEATURES`, which MSHV uses directly and WHP's
//! `WHV_PROCESSOR_FEATURES`, `WHV_PROCESSOR_FEATURES1`, and
//! `WHV_PROCESSOR_XSAVE_FEATURES` share.
//!
//! Bits without a CPUID feature, such as the nested-paging details, are the
//! backend's to decide. Features that no bit controls (for example PKU), the
//! descriptive leaves (caches, the brand string), and the time policy bits a
//! host lacks reach the guest through the backend's CPUID results. No XSAVE
//! feature bit controls PKRU either: a root that offered PKRU would let a
//! partition enable it in XCR0 under a profile without PKU, although the
//! guest's CPUID hides it. The roots of the fleet's hosts do not offer it.
//!
//! The hypervisor refuses some feature sets. MSHV fails to create a partition
//! with `mb_clear_support` cleared on its own on a Skylake-SP host, with
//! `rdtscp_support` cleared on its own on Azure, or with an XSAVE prerequisite
//! (`xsave_support`, `avx_support`, `avx512_support`,
//! `xsave_comp_support`) cleared while what depends on it stays set. No pinned
//! profile clears these where the fleet's hosts offer them, and a test keeps
//! it so. A profile that did would fail partition creation with the
//! hypervisor's error rather than `E_PROFILE_UNSUPPORTED`. The time ABI needs
//! RDTSCP, so no profile clears it.

use crate::cpuid::CpuidEntry;
use crate::error::ProfileError;
use crate::error::ProfileErrorCode;
use crate::fingerprint::BackendFingerprint;
use crate::fingerprint::IA32_ARCH_CAPABILITIES;
use crate::profile::CpuProfile;
use crate::surface::SupportedMsr;
use crate::surface::TIME_POLICY_BITS;
use hvdef::HvX64PartitionProcessorFeatures as Bank0;
use hvdef::HvX64PartitionProcessorFeatures1 as Bank1;
use hvdef::HvX64PartitionProcessorXsaveFeatures as XsaveBank;
use std::fmt;

const EAX: usize = 0;
const EBX: usize = 1;
const ECX: usize = 2;
const EDX: usize = 3;

/// The extended feature leaf.
const X1: u32 = 0x8000_0001;
/// The advanced power management leaf.
const X7: u32 = 0x8000_0007;
/// The extended feature identifiers leaf.
const X8: u32 = 0x8000_0008;

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

/// A processor feature word of a Hyper-V partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HvFeatureWord {
    /// Processor feature bank 0.
    Bank0,
    /// Processor feature bank 1.
    Bank1,
    /// The XSAVE features.
    Xsave,
}

impl HvFeatureWord {
    /// Every word.
    pub const ALL: [Self; 3] = [Self::Bank0, Self::Bank1, Self::Xsave];

    /// Returns the word's name, as messages spell it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Bank0 => "bank0",
            Self::Bank1 => "bank1",
            Self::Xsave => "xsave",
        }
    }
}

/// A partition's processor features: feature banks 0 and 1, and the XSAVE
/// features.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HvFeatures {
    /// Processor feature banks 0 and 1.
    pub banks: [u64; 2],
    /// The XSAVE features.
    pub xsave: u64,
}

impl HvFeatures {
    /// Returns `word`.
    pub fn word(&self, word: HvFeatureWord) -> u64 {
        match word {
            HvFeatureWord::Bank0 => self.banks[0],
            HvFeatureWord::Bank1 => self.banks[1],
            HvFeatureWord::Xsave => self.xsave,
        }
    }

    /// Returns `word`, to change it.
    pub fn word_mut(&mut self, word: HvFeatureWord) -> &mut u64 {
        match word {
            HvFeatureWord::Bank0 => &mut self.banks[0],
            HvFeatureWord::Bank1 => &mut self.banks[1],
            HvFeatureWord::Xsave => &mut self.xsave,
        }
    }
}

/// A CPUID feature bit: leaf, subleaf, register (0 for EAX through 3 for
/// EDX), and bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuidBit {
    /// The leaf.
    pub leaf: u32,
    /// The subleaf, 0 for a leaf without subleaves.
    pub subleaf: u32,
    /// The register: 0 for EAX, 1 for EBX, 2 for ECX, and 3 for EDX.
    pub register: usize,
    /// The bit.
    pub bit: u32,
}

impl CpuidBit {
    /// Returns the CPUID bit `bit` of `register` in `leaf` and `subleaf`.
    pub const fn new(leaf: u32, subleaf: u32, register: usize, bit: u32) -> Self {
        Self {
            leaf,
            subleaf,
            register,
            bit,
        }
    }
}

impl fmt::Display for CpuidBit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let register = ["eax", "ebx", "ecx", "edx"][self.register];
        write!(
            f,
            "{:#x}.{}:{}[{}]",
            self.leaf, self.subleaf, register, self.bit
        )
    }
}

/// A processor feature bit and the CPUID feature bit it controls.
#[derive(Debug)]
pub struct HvFeature {
    setter: &'static str,
    /// The word that holds the bit.
    pub word: HvFeatureWord,
    /// The bit, as a mask of [`Self::word`].
    pub mask: u64,
    /// The CPUID feature bit that a partition presents only with this bit.
    pub cpuid: CpuidBit,
}

impl HvFeature {
    /// Returns the feature's name: hvdef's field name, such as
    /// `sse3_support`.
    pub fn name(&self) -> &'static str {
        self.setter.trim_start_matches("with_")
    }

    /// Returns whether the CPUID bit is one of the time policy bits, which
    /// the time ABI's CPUID supplies whatever the features.
    pub fn is_time_policy(&self) -> bool {
        let bit = self.cpuid;
        TIME_POLICY_BITS.contains(&(bit.leaf, bit.subleaf, bit.register, 1 << bit.bit))
    }
}

macro_rules! feature {
    ($word:ident, $ty:ident, $with:ident, $leaf:expr, $subleaf:expr, $register:expr, $bit:expr) => {
        HvFeature {
            setter: stringify!($with),
            word: HvFeatureWord::$word,
            mask: $ty::new().$with(true).into_bits(),
            cpuid: CpuidBit::new($leaf, $subleaf, $register, $bit),
        }
    };
}

macro_rules! bank0 {
    ($with:ident, $($cpuid:expr),+) => {
        feature!(Bank0, Bank0, $with, $($cpuid),+)
    };
}

macro_rules! bank1 {
    ($with:ident, $($cpuid:expr),+) => {
        feature!(Bank1, Bank1, $with, $($cpuid),+)
    };
}

macro_rules! xsave {
    ($with:ident, $($cpuid:expr),+) => {
        feature!(Xsave, XsaveBank, $with, $($cpuid),+)
    };
}

/// The processor feature bits with a CPUID feature, as leaf, subleaf,
/// register, and bit. The leaves follow the Intel SDM and the AMD APM. The
/// bits that the banks derive into `IA32_ARCH_CAPABILITIES` are
/// [`ARCH_CAPABILITIES_BANK_BITS`].
///
/// WHP's hardware test `features_control_the_mapped_cpuid_bits` checks the
/// table against what WHP presents on a host.
pub const HV_FEATURES: &[HvFeature] = &[
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
    // Also sets the linear address width, 0x80000008 EAX[15:8].
    bank1!(with_la57_support, 7, 0, ECX, 16),
    // MSHV does not expose VMX through this bit alone.
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

/// Returns the bits of `word` that a profile decides: those of
/// [`HV_FEATURES`], and for the banks those of
/// [`ARCH_CAPABILITIES_BANK_BITS`]. The backend decides the other bits.
pub fn mapped_mask(word: HvFeatureWord) -> u64 {
    let features = HV_FEATURES
        .iter()
        .filter(|feature| feature.word == word)
        .fold(0, |mask, feature| mask | feature.mask);
    let bank = match word {
        HvFeatureWord::Bank0 => 0,
        HvFeatureWord::Bank1 => 1,
        HvFeatureWord::Xsave => return features,
    };
    ARCH_CAPABILITIES_BANK_BITS
        .iter()
        .filter(|&&(_, b, _)| b == bank)
        .fold(features, |mask, &(.., bit)| mask | 1 << bit)
}

/// How a profile pins a CPUID bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pinned {
    /// The profile pins the bit set.
    Set,
    /// The profile pins the bit clear.
    Clear,
    /// The bit is VM-owned or runtime-owned: the profile leaves it unpinned.
    Unpinned,
}

/// Returns how `profile` pins `bit`. A leaf or subleaf the profile does not
/// list counts as zero, pinned.
pub fn pinned(profile: &CpuProfile, bit: CpuidBit) -> Pinned {
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

/// Returns the processor features of a partition that presents `profile`,
/// given the features the hypervisor offers.
///
/// Every feature bit whose CPUID feature the profile clears is cleared, and
/// the `IA32_ARCH_CAPABILITIES` bits that the banks derive are restricted to
/// the profile's pinned value ([`restrict_banks_to_profile`]). Bits that
/// [`mapped_mask`] excludes are left as offered.
///
/// Fails with `E_PROFILE_UNSUPPORTED`, naming every shortfall, when the
/// profile sets a CPUID feature whose bit the hypervisor does not offer
/// (except the time policy bits, which the time ABI's CPUID supplies), or
/// when the banks cannot present the profile's `IA32_ARCH_CAPABILITIES`.
pub fn profile_features(
    profile: &CpuProfile,
    available: HvFeatures,
) -> Result<HvFeatures, ProfileError> {
    let mut features = available;
    let mut missing = Vec::new();
    for feature in HV_FEATURES {
        let word = features.word_mut(feature.word);
        match pinned(profile, feature.cpuid) {
            Pinned::Clear => *word &= !feature.mask,
            Pinned::Set if *word & feature.mask == 0 && !feature.is_time_policy() => {
                missing.push(feature.name().to_owned());
            }
            Pinned::Set | Pinned::Unpinned => {}
        }
    }
    restrict_banks_to_profile(profile, &mut features.banks);
    if let Some((value, mask)) = profile.msr(IA32_ARCH_CAPABILITIES) {
        let presented = arch_capabilities_from_banks(features.banks);
        if presented & mask != value & mask {
            missing.push(format!(
                "IA32_ARCH_CAPABILITIES {:#x} (the banks present {:#x} under mask {mask:#x})",
                value & mask,
                presented & mask
            ));
        }
    }
    if !missing.is_empty() {
        return Err(ProfileError::new(
            ProfileErrorCode::ProfileUnsupported,
            format!(
                "the hypervisor's processor features cannot present CPU profile {}: they lack {}",
                profile.id(),
                missing.join(", ")
            ),
        ));
    }
    Ok(features)
}

/// Clears from `cpuid` every CPUID bit that [`HV_FEATURES`] maps to a feature
/// `available` lacks, so a host's CPUID and its features describe what a
/// partition can present, without a probe partition.
///
/// A CPUID bit that several features control (IBRS and IBPB both control
/// `SPEC_CTRL`) is cleared when any of them is missing. Bits the table does
/// not map are left alone.
///
/// The function only clears, so the host's own view bounds the result.
/// MSHV's root CPUID hides `TSC_ADJUST` (`7.0:EBX[1]`), which a partition with
/// `tsc_adjust_support` presents, so that bit is under-reported; every profile
/// pins it clear. The root views of MSHV and WHP also show what neither gives
/// a guest. Those over-claims stay set: MONITOR, VMX, EST, TM and TM2, PDCM,
/// DS, ACPI, HTT, PT, TME, the hybrid bit, the topology fields, and leaves 5,
/// 6, `0xA`, `0x14`, and `0x15`. They are harmless to
/// [`verify_support`](crate::verify_support): no profile sets them, except
/// ARAT in leaf 6, a time policy bit that support checks exempt. The entries
/// outside a profile's tables over-claim too: on Skylake-SP, the root of both
/// MSHV and WHP reads Intel PT's subleaf `0x14.1` as non-zero, though neither
/// gives guests PT. So a surface built here is a
/// [`CpuidPresentation::PassThroughHostView`](crate::CpuidPresentation::PassThroughHostView).
/// The surface's guest physical address width must come from the hypervisor,
/// not from this CPUID (see [`HostCpuSurface`](crate::HostCpuSurface)).
pub fn restrict_cpuid_to_features(cpuid: &mut [CpuidEntry], available: HvFeatures) {
    for feature in HV_FEATURES {
        if available.word(feature.word) & feature.mask != 0 {
            continue;
        }
        let bit = feature.cpuid;
        for entry in cpuid.iter_mut().filter(|entry| {
            entry.leaf.0 == bit.leaf && entry.subleaf.map_or(0, |subleaf| subleaf.0) == bit.subleaf
        }) {
            let mut registers = entry.registers();
            registers[bit.register] &= !(1 << bit.bit);
            *entry = CpuidEntry::new(bit.leaf, entry.subleaf.map(|subleaf| subleaf.0), registers);
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
    use crate::test_support::profile;
    use crate::test_support::profile_entries;
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
        let icelake = profile("intel.icelake-sp.v1");
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

        let skylake = profile("intel.skylake-sp.v1");
        let mut banks = [!0, !0];
        restrict_banks_to_profile(skylake, &mut banks);
        assert_eq!(arch_capabilities_from_banks(banks), 0);
    }

    /// The processor features the hypervisors offered on the fleet's hosts
    /// (CPU fingerprints): WHP's capability and MSHV's host partition
    /// properties on the bare-metal Skylake-SP hosts and on the Ice Lake-SP
    /// (8370C) and Emerald Rapids (8573C) Azure runners.
    const HOSTS: [(&str, &str, HvFeatures); 6] = [
        (
            "intel.skylake-sp.v1",
            "whp 4114",
            HvFeatures {
                banks: [0x1001_f9ff_e7f7_859f, 0x0000_000f_1086_0063],
                xsave: 0x3fff,
            },
        ),
        (
            "intel.icelake-sp.v1",
            "whp 8370C",
            HvFeatures {
                banks: [0x2e0a_8bff_e7f7_859f, 0x0001_000e_0000_00f1],
                xsave: 0x7f_ffdf,
            },
        ),
        (
            "intel.emeraldrapids.v1",
            "whp 8573C",
            HvFeatures {
                banks: [0x0e0a_8bff_e7f7_859f, 0x0000_000c_0000_0051],
                xsave: 0x7f_ffdf,
            },
        ),
        (
            "intel.skylake-sp.v1",
            "mshv 4114",
            HvFeatures {
                banks: [0x1005_f9ff_fff7_859f, 0x0000_008f_1086_0063],
                xsave: 0x3fff,
            },
        ),
        (
            "intel.icelake-sp.v1",
            "mshv 8370C",
            HvFeatures {
                banks: [0x2e2a_8bff_fff7_859f, 0x0001_00ae_0000_00f1],
                xsave: 0x7f_ffdf,
            },
        ),
        (
            "intel.emeraldrapids.v1",
            "mshv 8573C",
            HvFeatures {
                banks: [0x0e2a_8bff_fff7_859f, 0x0000_000c_0000_0051],
                xsave: 0x7f_ffdf,
            },
        ),
    ];

    /// The features WHP offers on the bare-metal Skylake-SP host.
    const SKYLAKE_SP_WHP_HOST: HvFeatures = HOSTS[0].2;

    fn mask(word: HvFeatureWord, name: &str) -> u64 {
        HV_FEATURES
            .iter()
            .find(|feature| feature.word == word && feature.name() == name)
            .unwrap_or_else(|| panic!("no feature {name} in {}", word.name()))
            .mask
    }

    fn has(features: &HvFeatures, word: HvFeatureWord, name: &str) -> bool {
        features.word(word) & mask(word, name) != 0
    }

    #[test]
    fn every_feature_is_one_distinct_bit() {
        let mut seen = Vec::new();
        for feature in HV_FEATURES {
            assert_eq!(feature.mask.count_ones(), 1, "{}", feature.name());
            assert!(
                !seen.contains(&(feature.word, feature.mask)),
                "{} repeats a bit",
                feature.name()
            );
            seen.push((feature.word, feature.mask));
        }
        // No feature bit is also an IA32_ARCH_CAPABILITIES bit.
        for &(_, bank, bit) in &ARCH_CAPABILITIES_BANK_BITS {
            assert!(
                !seen.contains(&(HvFeatureWord::ALL[bank], 1 << bit)),
                "bank{bank} bit {bit}"
            );
        }
    }

    #[test]
    fn the_mapped_mask_covers_the_features_and_arch_capabilities() {
        for word in HvFeatureWord::ALL {
            let features = HV_FEATURES
                .iter()
                .filter(|feature| feature.word == word)
                .fold(0, |mask, feature| mask | feature.mask);
            let arch_capabilities = ARCH_CAPABILITIES_BANK_BITS
                .iter()
                .filter(|&&(_, bank, _)| HvFeatureWord::ALL[bank] == word)
                .fold(0u64, |mask, &(.., bit)| mask | 1 << bit);
            assert_eq!(
                mapped_mask(word),
                features | arch_capabilities,
                "{}",
                word.name()
            );
        }
    }

    #[test]
    fn pinned_profiles_derive_on_their_hosts() {
        for (id, host, available) in HOSTS {
            let profile = profile(id);
            let features =
                profile_features(profile, available).unwrap_or_else(|err| panic!("{host}: {err}"));
            for word in HvFeatureWord::ALL {
                assert_eq!(
                    features.word(word) & !available.word(word),
                    0,
                    "{host}: {} gained bits",
                    word.name()
                );
                assert_eq!(
                    features.word(word) & !mapped_mask(word),
                    available.word(word) & !mapped_mask(word),
                    "{host}: {} changed unmapped bits",
                    word.name()
                );
            }
            // Every mapped bit follows the profile's CPUID.
            for feature in HV_FEATURES {
                let offered = available.word(feature.word) & feature.mask != 0;
                let kept = features.word(feature.word) & feature.mask != 0;
                match pinned(profile, feature.cpuid) {
                    Pinned::Clear => assert!(!kept, "{host}: {}", feature.name()),
                    Pinned::Set | Pinned::Unpinned => {
                        assert_eq!(kept, offered, "{host}: {}", feature.name())
                    }
                }
            }
            // The time policy: no TSC-deadline, TSC_ADJUST, or APERF/MPERF.
            for name in [
                "tsc_deadline_tmr_support",
                "tsc_adjust_support",
                "a_count_m_count_support",
            ] {
                assert!(
                    !has(&features, HvFeatureWord::Bank1, name),
                    "{host}: {name}"
                );
            }
            let (value, msr_mask) = profile.msr(IA32_ARCH_CAPABILITIES).unwrap();
            assert_eq!(
                arch_capabilities_from_banks(features.banks) & msr_mask,
                value & msr_mask,
                "{host}"
            );
        }
    }

    #[test]
    fn skylake_keeps_the_speculation_controls() {
        // The Skylake-SP profile pins SPEC_CTRL, STIBP, SSBD, and MD_CLEAR
        // (CPUID 7.0 EDX 0xac000400), which WHP's default features omit.
        for (id, host, available) in HOSTS {
            if id != "intel.skylake-sp.v1" {
                continue;
            }
            let features = profile_features(profile(id), available).unwrap();
            for name in [
                "ibrs_support",
                "ibpb_support",
                "stibp_support",
                "mdd_support",
                "mb_clear_support",
            ] {
                assert!(has(&features, HvFeatureWord::Bank0, name), "{host}: {name}");
            }
        }
    }

    #[test]
    fn invariant_tsc_follows_the_host() {
        let skylake =
            profile_features(profile("intel.skylake-sp.v1"), SKYLAKE_SP_WHP_HOST).unwrap();
        assert!(has(&skylake, HvFeatureWord::Bank1, "tsc_invariant_support"));
        // Azure's WHP offers no invariant TSC. The profile still pins it, and
        // the time ABI's CPUID supplies it.
        let icelake = profile_features(profile("intel.icelake-sp.v1"), HOSTS[1].2).unwrap();
        assert!(!has(
            &icelake,
            HvFeatureWord::Bank1,
            "tsc_invariant_support"
        ));
    }

    #[test]
    fn pinned_profiles_keep_what_the_hypervisor_requires() {
        // MSHV refuses a partition with mb_clear or rdtscp cleared on its own,
        // or with an XSAVE prerequisite cleared while a feature that depends
        // on it stays set (see the module documentation).
        for (id, host, available) in HOSTS {
            let features = profile_features(profile(id), available).unwrap();
            for name in ["mb_clear_support", "rdtscp_support"] {
                if has(&available, HvFeatureWord::Bank0, name) {
                    assert!(
                        has(&features, HvFeatureWord::Bank0, name),
                        "{host}: {id} clears {name}"
                    );
                }
            }
            let kept = |name: &str| has(&features, HvFeatureWord::Xsave, name);
            for feature in HV_FEATURES {
                let name = feature.name();
                if feature.word != HvFeatureWord::Xsave || features.xsave & feature.mask == 0 {
                    continue;
                }
                assert!(kept("xsave_support"), "{host}: {name} without xsave");
                if name.starts_with("avx") && name != "avx_support" {
                    assert!(kept("avx_support"), "{host}: {name} without avx");
                }
                if name.starts_with("avx512_") {
                    assert!(kept("avx512_support"), "{host}: {name} without avx512");
                }
            }
        }
    }

    #[test]
    fn a_missing_feature_is_unsupported() {
        let mut available = SKYLAKE_SP_WHP_HOST;
        available.xsave &= !mask(HvFeatureWord::Xsave, "avx512_support");
        available.banks[0] &= !mask(HvFeatureWord::Bank0, "ibrs_support");
        let error = profile_features(profile("intel.skylake-sp.v1"), available).unwrap_err();
        assert_eq!(error.code, ProfileErrorCode::ProfileUnsupported);
        assert!(error.message.contains("avx512_support"), "{error}");
        assert!(error.message.contains("ibrs_support"), "{error}");

        // A newer generation's profile on an older host.
        let error =
            profile_features(profile("intel.emeraldrapids.v1"), SKYLAKE_SP_WHP_HOST).unwrap_err();
        assert_eq!(error.code, ProfileErrorCode::ProfileUnsupported);
    }

    #[test]
    fn unlisted_leaves_read_zero_and_unmasked_bits_are_unpinned() {
        let skylake = profile("intel.skylake-sp.v1");
        // Skylake-SP lists no leaf 7.1, so its features are pinned clear.
        assert_eq!(pinned(skylake, CpuidBit::new(7, 1, EAX, 5)), Pinned::Clear);
        // OSXSAVE is runtime state: the profile leaves it unpinned.
        assert_eq!(
            pinned(skylake, CpuidBit::new(1, 0, ECX, 27)),
            Pinned::Unpinned
        );
        assert_eq!(pinned(skylake, CpuidBit::new(1, 0, ECX, 26)), Pinned::Set);
        assert_eq!(CpuidBit::new(7, 0, EDX, 26).to_string(), "0x7.0:edx[26]");
    }

    #[test]
    fn restricts_cpuid_to_the_features() {
        // Each host's features present its profile, except time policy bits
        // that the time ABI's CPUID supplies.
        for (id, host, available) in HOSTS {
            let original = profile_entries(profile(id));
            let mut cpuid = original.clone();
            restrict_cpuid_to_features(&mut cpuid, available);
            for (entry, before) in cpuid.iter().zip(&original) {
                let (leaf, subleaf) = (entry.leaf.0, entry.subleaf.map_or(0, |s| s.0));
                for register in 0..4 {
                    let lost = before.registers()[register] & !entry.registers()[register];
                    for bit in (0..32).filter(|bit| lost & (1 << bit) != 0) {
                        assert!(
                            TIME_POLICY_BITS.contains(&(leaf, subleaf, register, 1 << bit)),
                            "{host}: lost {}",
                            CpuidBit::new(leaf, subleaf, register, bit)
                        );
                    }
                }
            }
        }

        // A missing feature clears its CPUID bit, and nothing else.
        let skylake = profile_entries(profile("intel.skylake-sp.v1"));
        let mut available = SKYLAKE_SP_WHP_HOST;
        available.xsave &= !mask(HvFeatureWord::Xsave, "avx512_support");
        available.banks[0] &= !mask(HvFeatureWord::Bank0, "ibpb_support");
        let mut cpuid = skylake.clone();
        restrict_cpuid_to_features(&mut cpuid, available);
        let leaf7 = |cpuid: &[CpuidEntry]| {
            cpuid
                .iter()
                .find(|entry| entry.leaf.0 == 7 && entry.subleaf.map_or(0, |s| s.0) == 0)
                .unwrap()
                .registers()
        };
        let (before, after) = (leaf7(&skylake), leaf7(&cpuid));
        // AVX512F goes, AVX512DQ stays.
        assert_eq!(after[EBX], before[EBX] & !(1 << 16));
        // IBPB missing clears SPEC_CTRL, which IBRS and IBPB share.
        assert_eq!(after[EDX], before[EDX] & !(1 << 26));
        assert_eq!(
            (after[EAX], after[ECX]),
            (before[EAX], before[ECX]),
            "other registers"
        );
        for (entry, before) in cpuid.iter().zip(&skylake) {
            if entry.leaf.0 != 7 {
                assert_eq!(entry, before);
            }
        }
    }
}
