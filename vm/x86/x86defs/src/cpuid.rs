// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use bitfield_struct::bitfield;
use core::fmt::Display;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::KnownLayout;

open_enum! {
    #[derive(FromBytes, Immutable, KnownLayout)]
    pub enum CpuidFunction : u32 {
        #![expect(non_upper_case_globals, reason = "TODO: rename to SHOUTING_CASE")]
        BasicMinimum = 0x00000000,
        VendorAndMaxFunction = 0x00000000,
        VersionAndFeatures = 0x00000001,
        CacheAndTlbInformation = 0x00000002,
        CacheParameters = 0x00000004,
        MonitorMwait = 0x00000005,
        PowerManagement = 0x00000006,
        ExtendedFeatures = 0x00000007,
        DirectCacheAccessParameters = 0x00000009,
        PerformanceMonitoring = 0x0000000A,
        ExtendedTopologyEnumeration = 0x0000000B,
        ExtendedStateEnumeration = 0x0000000D,
        RdtmEnumeration = 0x0000000F,
        RdtaEnumeration = 0x00000010,
        SgxEnumeration = 0x00000012,
        IptEnumeration = 0x00000014,
        CoreCrystalClockInformation = 0x00000015,
        NativeModelId = 0x0000001A,
        ArchLbr = 0x0000001C,
        TileInformation = 0x0000001D,
        TmulInformation = 0x0000001E,
        V2ExtendedTopologyEnumeration = 0x0000001F,
        HistoryResetFeatures = 0x00000020,

        BasicMaximum = 0x00000020,
        IntelMaximum = 0x00000020,
        AmdMaximum = 0x0000000D,
        CompatBlueBasicMaximum = 0x0000000D,
        GuestBasicMaximum = 0x0000001C,

        UnimplementedMinimum = 0x40000000,
        UnimplementedMaximum = 0x4FFFFFFF,

        ExtendedMaxFunction = 0x80000000,
        ExtendedVersionAndFeatures = 0x80000001,
        ExtendedBrandingString1 = 0x80000002,
        ExtendedBrandingString2 = 0x80000003,
        ExtendedBrandingString3 = 0x80000004,
        ExtendedL1CacheParameters = 0x80000005,
        ExtendedL2CacheParameters = 0x80000006,
        ExtendedPowerManagement = 0x80000007,
        ExtendedAddressSpaceSizes = 0x80000008,
        ExtendedIntelMaximum = 0x80000008,

        Extended80000009 = 0x80000009,
        ExtendedSvmVersionAndFeatures = 0x8000000A,
        ExtendedTlb1GBIdentifiers = 0x80000019,
        ExtendedOptimizationIdentifiers = 0x8000001A,
        InstructionBasedSamplingProfiler = 0x8000001B,
        LightweightProfilingCapabilities = 0x8000001C,
        CacheTopologyDefinition = 0x8000001D,
        ProcessorTopologyDefinition = 0x8000001E,
        ExtendedSevFeatures = 0x8000001F,
        ExtendedFeatures2 = 0x80000021,
        ExtendedPerfmonAndDebug = 0x80000022,
        ExtendedCpuTopology = 0x80000026,

        ExtendedAmdMaximum = 0x80000026,
        ExtendedMaximum = 0x80000026,
    }
}

#[bitfield(u32)]
pub struct VendorAndMaxFunctionEax {
    pub max_function: u32,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub struct Vendor(pub [u8; 12]);

#[cfg(feature = "arbitrary")]
impl<'a> arbitrary::Arbitrary<'a> for Vendor {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        // 25% of the time generate a random vendor
        if u.ratio(1, 4)? {
            Ok(Self(u.arbitrary()?))
        } else {
            Ok(*u.choose(&[Self::INTEL, Self::AMD, Self::HYGON])?)
        }
    }
}

impl Display for Vendor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if let Ok(s) = core::str::from_utf8(&self.0) {
            f.pad(s)
        } else {
            core::fmt::Debug::fmt(&self.0, f)
        }
    }
}

impl Vendor {
    pub const INTEL: Self = Self(*b"GenuineIntel");
    pub const AMD: Self = Self(*b"AuthenticAMD");
    pub const HYGON: Self = Self(*b"HygonGenuine");

    pub fn from_ebx_ecx_edx(ebx: u32, ecx: u32, edx: u32) -> Self {
        let mut vendor = [0; 12];
        vendor[0..4].copy_from_slice(&ebx.to_ne_bytes());
        vendor[4..8].copy_from_slice(&edx.to_ne_bytes());
        vendor[8..12].copy_from_slice(&ecx.to_ne_bytes());
        Self(vendor)
    }

    pub fn to_ebx_ecx_edx(self) -> (u32, u32, u32) {
        let ebx = u32::from_ne_bytes(self.0[0..4].try_into().unwrap());
        let edx = u32::from_ne_bytes(self.0[4..8].try_into().unwrap());
        let ecx = u32::from_ne_bytes(self.0[8..12].try_into().unwrap());

        (ebx, ecx, edx)
    }

    pub fn is_intel_compatible(&self) -> bool {
        self == &Self::INTEL
    }

    pub fn is_amd_compatible(&self) -> bool {
        self == &Self::AMD || self == &Self::HYGON
    }
}

#[bitfield(u32)]
pub struct VersionAndFeaturesEax {
    #[bits(4)]
    pub processor_stepping: u32,
    #[bits(4)]
    pub processor_model: u32,
    #[bits(4)]
    pub processor_family: u32,
    #[bits(2)]
    pub processor_type: u32,
    #[bits(2)]
    _reserved1: u32,
    #[bits(4)]
    pub extended_model: u32,
    pub extended_family: u8,
    #[bits(4)]
    _reserved2: u32,
}

#[bitfield(u32)]
pub struct VersionAndFeaturesEbx {
    pub brand_index: u8,
    pub clflush_line_size: u8,
    pub lps_per_package: u8,
    pub initial_apic_id: u8,
}

#[bitfield(u32)]
pub struct VersionAndFeaturesEcx {
    pub sse3: bool,
    pub pclmulqdq: bool,
    pub dtes64: bool,
    pub monitor: bool,
    pub cpl_ds: bool,
    pub vmx: bool,
    pub smx: bool,
    pub est: bool,
    pub tm2: bool,
    pub ssse3: bool,
    pub cnxt_id: bool,
    pub seg_limit64_bit: bool,
    pub fma: bool,
    pub cx16: bool,
    pub xtpr: bool,
    pub pdcm: bool,
    _reserved1: bool,
    pub pcid: bool,
    pub dca: bool,
    pub sse4_1: bool,
    pub sse4_2: bool,
    pub x2_apic: bool,
    pub movbe: bool,
    pub pop_cnt: bool,
    pub tsc_deadline_tmr: bool,
    pub aes: bool,
    pub xsave: bool,
    pub os_xsave: bool,
    pub avx: bool,
    pub f16c: bool,
    pub rd_rand: bool,
    pub hypervisor_present: bool,
}

#[bitfield(u32)]
pub struct VersionAndFeaturesEdx {
    pub fpu: bool,
    pub vme: bool,
    pub de: bool,
    pub pse: bool,
    pub tsc: bool,
    pub msr: bool,
    pub pae: bool,
    pub mce: bool,
    pub cx8: bool,
    pub apic: bool,
    _reserved1: bool,
    pub sep: bool,
    pub mtrr: bool,
    pub pge: bool,
    pub mca: bool,
    pub cmov: bool,
    pub pat: bool,
    pub pse36: bool,
    pub psn: bool,
    pub cl_fsh: bool,
    _reserved2: bool,
    pub ds: bool,
    pub acpi: bool,
    pub mmx: bool,
    pub fxsr: bool,
    pub sse: bool,
    pub sse2: bool,
    pub ss: bool,
    pub mt_per_socket: bool,
    pub tm: bool,
    _reserved3: bool,
    pub pbe: bool,
}

#[bitfield(u32)]
pub struct ExtendedVersionAndFeaturesEax {
    #[bits(4)]
    pub processor_stepping: u32,
    #[bits(4)]
    pub processor_model: u32,
    #[bits(4)]
    pub processor_family: u32,
    #[bits(2)]
    pub processor_type: u32,
    #[bits(2)]
    _reserved_eax1: u32,
    #[bits(4)]
    pub extended_model: u32,
    pub extended_family: u8,
    #[bits(4)]
    _reserved_eax2: u32,
}

#[bitfield(u32)]
pub struct ExtendedVersionAndFeaturesEbx {
    pub brand_id: u16,
    #[bits(12)]
    _reserved: u32,
    #[bits(4)]
    pub pkg_type: u32,
}

#[bitfield(u32)]
pub struct ExtendedVersionAndFeaturesEcx {
    pub lahf_sahf_available: bool,
    pub cmp_legacy: bool,
    pub svm: bool,
    pub ext_apic_space: bool,
    pub alt_mov_cr8: bool,
    pub abm: bool,
    pub sse4_a: bool,
    pub mis_align_sse: bool,
    pub prefetch: bool,
    pub osvw: bool,
    pub ibs: bool,
    pub xop: bool,
    pub skinit: bool,
    pub wdt: bool,
    _reserved1: bool,
    pub lwp: bool,
    pub fma4: bool,
    #[bits(5)]
    _reserved2: u32,
    pub topology_extensions: bool,
    pub perf_ctr_ext_core: bool,
    pub perf_ctr_ext_df: bool,
    #[bits(3)]
    _reserved4: u32,
    pub perf_ctr_ext_llc: bool,
    pub monitor_x: bool,
    #[bits(2)]
    _reserved5: u32,
}

#[bitfield(u32)]
pub struct ExtendedVersionAndFeaturesEdx {
    pub fpu: bool,
    pub vme: bool,
    pub de: bool,
    pub pse: bool,
    pub tsc: bool,
    pub msr: bool,
    pub pae: bool,
    pub mce: bool,
    pub cx8: bool,
    pub apic: bool,
    _reserved1: bool,
    pub syscall: bool,
    pub mtrr: bool,
    pub pge: bool,
    pub mca: bool,
    pub cmov: bool,
    pub pat: bool,
    pub pse36: bool,
    #[bits(2)]
    _reserved2: u32,
    pub no_execute: bool,
    _reserved3: bool,
    pub amd_mmx: bool,
    pub mmx: bool,
    pub fxsr: bool,
    pub fast_fxsr: bool,
    pub page_1gb: bool,
    pub rdtscp: bool,
    _reserved4: bool,
    pub long_mode: bool,
    pub extended3d_now: bool,
    pub amd3d_now: bool,
}

#[bitfield(u32)]
pub struct SgxCpuidSubleafEax {
    #[bits(4)]
    pub sgx_type: u8, // 0 = Invalid, 1 = EPC section
    #[bits(28)]
    pub reserved_eax: u32,
}

#[bitfield(u32)]
pub struct CacheParametersEax {
    #[bits(5)]
    pub cache_type: u32, // Type is CPUID_CACHE_TYPE
    #[bits(3)]
    pub cache_level: u32,
    #[bits(1)]
    pub self_initializing: u32,
    #[bits(1)]
    pub fully_associative: u32,
    #[bits(4)]
    pub reserved: u32,
    #[bits(12)]
    pub threads_sharing_cache_minus_one: u32,
    #[bits(6)]
    pub cores_per_socket_minus_one: u32,
}

#[bitfield(u32)]
pub struct CacheParametersEbx {
    #[bits(12)]
    pub system_coherency_line_size_minus_one: u32,
    #[bits(10)]
    pub physical_line_partitions_minus_one: u32,
    #[bits(10)]
    pub ways_of_associativity_minus_one: u32,
}

#[bitfield(u32)]
pub struct CacheParametersEcx {
    pub number_of_sets_minus_one: u32,
}
#[bitfield(u32)]
pub struct CacheParametersEdx {
    pub wbinvd_behavior: bool,
    pub cache_inclusiveness: bool,
    #[bits(30)]
    pub reserved: u32,
}

open_enum! {
    pub enum TopologyLevelType: u8 {
        INVALID = 0,
        SMT = 1,
        CORE = 2,
        MODULE = 3,
        TILE = 4,
        DIE = 5,
    }
}

#[bitfield(u32)]
pub struct ExtendedTopologyEax {
    #[bits(5)]
    pub x2_apic_shift: u32,
    #[bits(27)]
    _reserved: u32,
}

#[bitfield(u32)]
pub struct ExtendedTopologyEbx {
    pub num_lps: u16,
    _reserved: u16,
}

#[bitfield(u32)]
pub struct ExtendedTopologyEcx {
    pub level_number: u8,
    pub level_type: u8,
    _reserved: u16,
}

#[bitfield(u32)]
pub struct ExtendedAddressSpaceSizesEax {
    pub physical_address_size: u8,
    pub virtual_address_size: u8,
    pub guest_physical_address_size: u8,
    pub reserved_eax: u8,
}

#[bitfield(u32)]
pub struct ExtendedAddressSpaceSizesEbx {
    pub cl_zero: bool,
    pub inst_ret_cnt_msr: bool,
    pub x_save_er_ptr: bool,
    pub invlpgb: bool,
    pub rdpru: bool,
    _rsvd1: bool,
    pub mbe: bool,
    #[bits(2)]
    _rsvd2: u8,
    pub wbnoinvd: bool,
    #[bits(2)]
    _rsvd3: u8,
    pub ibpb: bool,
    pub int_wbinvd: bool,
    pub ibrs: bool,
    pub stibp: bool,
    pub rsvd4: bool,
    pub stibp_always_on: bool,
    #[bits(2)]
    _rsvd5: u8,
    pub efer_lmsle_unsupported: bool,
    pub nested_invlpgb: bool,
    #[bits(2)]
    _rsvd6: u8,
    pub ssbd: bool,
    pub ssbd_virt_spec_ctrl: bool,
    pub ssbd_not_required: bool,
    pub cppc: bool,
    pub psfd: bool,
    pub btc_no: bool,
    pub ibpb_ret: bool,
    _rsvd7: bool,
}

#[bitfield(u32)]
pub struct ExtendedAddressSpaceSizesEcx {
    pub nc: u8,
    #[bits(4)]
    pub rsvd1: u8,
    #[bits(4)]
    pub apic_core_id_size: u8,
    pub rsvd2: u16,
}

#[bitfield(u32)]
pub struct ExtendedAddressSpaceSizesEdx {
    pub invlpgb_count_max: u16,
    pub rdpru_max_ecx: u16,
}

#[bitfield(u32)]
pub struct ProcessorTopologyDefinitionEax {
    pub extended_apic_id: u32,
}

#[bitfield(u32)]
pub struct ProcessorTopologyDefinitionEbx {
    pub compute_unit_id: u8,
    pub threads_per_compute_unit: u8,
    _reserved: u16,
}

#[bitfield(u32)]
pub struct ProcessorTopologyDefinitionEcx {
    pub node_id: u8,
    #[bits(3)]
    pub nodes_per_processor: u8,
    #[bits(21)]
    _reserved: u32,
}

#[bitfield(u32)]
pub struct ProcessorTopologyDefinitionEdx {
    _reserved: u32,
}

#[bitfield(u32)]
pub struct ExtendedStateEnumerationSubleaf0Eax {
    pub x87: bool,
    pub sse: bool,
    pub avx: bool,
    pub bndreg: bool,
    pub bndcsr: bool,
    pub opmask: bool,
    pub zmmhi: bool,
    pub zmm16_31: bool,
    #[bits(9)]
    reserved1: u32,
    pub xtile_cfg: bool,
    pub xtile_dta: bool,
    #[bits(13)]
    reserved2: u32,
}

#[bitfield(u32)]
pub struct ExtendedStateEnumerationSubleaf1Eax {
    pub xsave_opt: bool,
    pub xsave_c: bool,
    pub xcr1: bool,
    pub xsave_s: bool,
    pub xfd: bool,
    #[bits(27)]
    _reserved: u32,
}

#[bitfield(u32)]
pub struct ExtendedStateEnumerationSubleafNEcx {
    pub supervisor: bool,
    pub aligned: bool,
    pub xfd: bool,
    #[bits(29)]
    _reserved: u32,
}

#[bitfield(u32)]
pub struct ExtendedFeatureSubleaf0Ebx {
    pub rd_wr_fs_gs: bool,
    pub tsc_adjust: bool,
    pub sgx: bool,
    pub bmi1: bool,
    pub hle: bool,
    pub avx2: bool,
    _reserved0: bool,
    pub smep: bool,
    pub bmi2: bool,
    pub enhanced_fast_string: bool,
    pub inv_pcid: bool,
    pub rtm: bool,
    pub rdt_m: bool,
    pub dep_x87_fpu_save: bool,
    pub mpx: bool,
    pub rdt_a: bool,
    pub avx512f: bool,
    pub avx512dq: bool,
    pub rd_seed: bool,
    pub adx: bool,
    pub smap: bool,
    pub avx512_ifma: bool,
    _reserved3: bool,
    pub clflushopt: bool,
    pub clwb: bool,
    pub ipt: bool,
    #[bits(2)]
    _reserved4: u32,
    pub avx512cd: bool,
    pub sha: bool,
    pub avx512bw: bool,
    pub avx512vl: bool,
}

#[bitfield(u32)]
pub struct ExtendedFeatureSubleaf0Ecx {
    _reserved0: bool,
    pub avx512_vbmi: bool,
    pub umip: bool,
    #[bits(2)]
    _reserved1: u32,
    pub umwait_tpause: bool,
    pub avx512_vbmi2: bool,
    pub cet_ss: bool,
    pub gfni: bool,
    pub vaes: bool,
    pub vpclmulqdq: bool,
    pub avx512_vnni: bool,
    pub avx512_bitalg: bool,
    pub tme: bool,
    pub avx512_vpopcntdq: bool,
    _reserved2: bool,
    pub la57: bool,
    #[bits(5)]
    _reserved3: u32,
    pub rd_pid: bool,
    #[bits(2)]
    _reserved4: u32,
    pub cldemote: bool,
    _reserved5: bool,
    pub movdiri: bool,
    pub movdir64b: bool,
    pub enqcmd: bool,
    pub sgx_lc: bool,
    _reserved6: bool,
}

#[bitfield(u32)]
pub struct ExtendedFeatureSubleaf0Edx {
    #[bits(4)]
    _reserved0: u32,
    pub fast_short_rep_move: bool,
    #[bits(3)]
    _reserved1: u32,
    pub avx512_vp2_intersect: bool,
    _reserved2: bool,
    pub mb_clear: bool,
    #[bits(3)]
    _reserved3: u32,
    pub serialize: bool,
    pub hetero: bool,
    pub tsx_ld_trk: bool,
    _reserved4: bool,
    pub pconfig: bool,
    pub arch_lbr: bool,
    pub cet_ibt: bool,
    _reserved5: bool,
    pub amx_bf16: bool,
    pub avx512_fp16: bool,
    pub amx_tile: bool,
    pub amx_int8: bool,
    pub ibrs: bool,
    pub stibp: bool,
    pub l1d_cache_flush: bool,
    pub arch_capabilities: bool,
    _reserved6: bool,
    pub ssbd: bool,
}

#[bitfield(u32)]
pub struct ExtendedFeatureSubleaf1Eax {
    #[bits(4)]
    _reserved0: u32,
    pub avx_vnni: bool,
    pub avx512_bfloat16: bool,
    #[bits(4)]
    _reserved1: u32,
    pub fzlrep_movsb: bool,
    pub fsrep_stosb: bool,
    pub fsrep_cmpsb: bool,
    #[bits(9)]
    _reserved2: u32,
    pub hreset: bool,
    pub avx_ifma: bool,
    _reserved3: u8,
}

#[bitfield(u32)]
pub struct ExtendedFeatureSubleaf1Edx {
    #[bits(4)]
    pub _reserved1: u32,
    pub avx_vnni_int8: bool,
    pub avx_ne_convert: bool,
    #[bits(4)]
    pub _reserved2: u32,
    pub avx_vnni_int16: bool,
    #[bits(21)]
    pub _reserved3: u32,
}

#[bitfield(u32)]
pub struct ExtendedSvmVersionAndFeaturesEax {
    pub svm_rev: u8,
    #[bits(24)]
    _reserved: u32,
}

#[bitfield(u32)]
pub struct ExtendedOptimizationIdentifiersEax {
    pub fp128: bool,
    pub mov_u: bool,
    pub fp256: bool,
    #[bits(29)]
    _reserved: u32,
}

#[bitfield(u32)]
pub struct ExtendedSevFeaturesEax {
    pub sme: bool,
    pub sev: bool,
    pub vmpage_flush_msr_available: bool,
    pub sev_es: bool,
    pub sev_snp: bool,
    pub vmpl: bool,
    pub rmp_query: bool,
    pub vmpl_isss: bool,
    pub secure_tsc: bool,
    pub tsc_aux_virtualization: bool,
    pub coherency_enforced: bool,
    pub req64_bit_hypervisor: bool,
    pub restrict_injection: bool,
    pub alternate_injection: bool,
    pub debug_state_swap: bool,
    pub prevent_host_ibs: bool,
    pub vte: bool,
    pub vmgexit_parameter: bool,
    pub virtual_tom_msr: bool,
    pub ibs_virtualization: bool,
    #[bits(4)]
    _reserved1: u32,
    pub vmsa_register_protection: bool,
    #[bits(4)]
    _reserved2: u32,
    pub nested_virt_msr_snp: bool,
    #[bits(2)]
    _reserved3: u32,
}

#[bitfield(u32)]
pub struct ExtendedSevFeaturesEbx {
    #[bits(6)]
    pub cbit_position: u8,
    #[bits(6)]
    pub encryption_physical_bits_used: u8,
    #[bits(4)]
    pub number_of_vmpls: u8,
    _reserved: u16,
}
