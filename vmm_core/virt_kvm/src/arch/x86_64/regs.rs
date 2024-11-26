// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use hvdef::HvX64RegisterName;

#[derive(Debug)]
// Field is stored solely for logging via debug, not actually dead.
pub struct NoRegisterMapping(#[expect(dead_code)] HvX64RegisterName);

/// Converts a register name to an msr.
pub const fn register_to_msr(name: HvX64RegisterName) -> Result<u32, NoRegisterMapping> {
    Ok(match name {
        HvX64RegisterName::ApicBase => x86defs::X86X_MSR_APIC_BASE,
        HvX64RegisterName::KernelGsBase => x86defs::X64_MSR_KERNEL_GS_BASE,
        HvX64RegisterName::SysenterCs => x86defs::X86X_MSR_SYSENTER_CS,
        HvX64RegisterName::SysenterEsp => x86defs::X86X_MSR_SYSENTER_ESP,
        HvX64RegisterName::SysenterEip => x86defs::X86X_MSR_SYSENTER_EIP,
        HvX64RegisterName::Star => x86defs::X86X_MSR_STAR,
        HvX64RegisterName::Lstar => x86defs::X86X_MSR_LSTAR,
        HvX64RegisterName::Cstar => x86defs::X86X_MSR_CSTAR,
        HvX64RegisterName::Sfmask => x86defs::X86X_MSR_SFMASK,

        HvX64RegisterName::Pat => x86defs::X86X_MSR_CR_PAT,
        HvX64RegisterName::MsrMtrrDefType => x86defs::X86X_MSR_MTRR_DEF_TYPE,
        HvX64RegisterName::MsrMtrrFix64k00000 => x86defs::X86X_MSR_MTRR_FIX64K_00000,
        HvX64RegisterName::MsrMtrrFix16k80000 => x86defs::X86X_MSR_MTRR_FIX16K_80000,
        HvX64RegisterName::MsrMtrrFix16kA0000 => x86defs::X86X_MSR_MTRR_FIX16K_A0000,
        HvX64RegisterName::MsrMtrrFix4kC0000 => x86defs::X86X_MSR_MTRR_FIX4K_C0000,
        HvX64RegisterName::MsrMtrrFix4kC8000 => x86defs::X86X_MSR_MTRR_FIX4K_C8000,
        HvX64RegisterName::MsrMtrrFix4kD0000 => x86defs::X86X_MSR_MTRR_FIX4K_D0000,
        HvX64RegisterName::MsrMtrrFix4kD8000 => x86defs::X86X_MSR_MTRR_FIX4K_D8000,
        HvX64RegisterName::MsrMtrrFix4kE0000 => x86defs::X86X_MSR_MTRR_FIX4K_E0000,
        HvX64RegisterName::MsrMtrrFix4kE8000 => x86defs::X86X_MSR_MTRR_FIX4K_E8000,
        HvX64RegisterName::MsrMtrrFix4kF0000 => x86defs::X86X_MSR_MTRR_FIX4K_F0000,
        HvX64RegisterName::MsrMtrrFix4kF8000 => x86defs::X86X_MSR_MTRR_FIX4K_F8000,

        HvX64RegisterName::MsrMtrrPhysBase0 => x86defs::X86X_MSR_MTRR_PHYSBASE0,
        HvX64RegisterName::MsrMtrrPhysMask0 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 1,
        HvX64RegisterName::MsrMtrrPhysBase1 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 2,
        HvX64RegisterName::MsrMtrrPhysMask1 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 3,
        HvX64RegisterName::MsrMtrrPhysBase2 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 4,
        HvX64RegisterName::MsrMtrrPhysMask2 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 5,
        HvX64RegisterName::MsrMtrrPhysBase3 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 6,
        HvX64RegisterName::MsrMtrrPhysMask3 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 7,
        HvX64RegisterName::MsrMtrrPhysBase4 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 8,
        HvX64RegisterName::MsrMtrrPhysMask4 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 9,
        HvX64RegisterName::MsrMtrrPhysBase5 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 10,
        HvX64RegisterName::MsrMtrrPhysMask5 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 11,
        HvX64RegisterName::MsrMtrrPhysBase6 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 12,
        HvX64RegisterName::MsrMtrrPhysMask6 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 13,
        HvX64RegisterName::MsrMtrrPhysBase7 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 14,
        HvX64RegisterName::MsrMtrrPhysMask7 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 15,

        HvX64RegisterName::Tsc => x86defs::X86X_MSR_TSC,
        HvX64RegisterName::TscAux => x86defs::X86X_MSR_TSC_AUX,

        HvX64RegisterName::Xss => x86defs::X86X_MSR_XSS,
        HvX64RegisterName::UCet => x86defs::X86X_MSR_U_CET,
        HvX64RegisterName::SCet => x86defs::X86X_MSR_S_CET,
        HvX64RegisterName::Pl0Ssp => x86defs::X86X_MSR_PL0_SSP,
        HvX64RegisterName::Pl1Ssp => x86defs::X86X_MSR_PL1_SSP,
        HvX64RegisterName::Pl2Ssp => x86defs::X86X_MSR_PL2_SSP,
        HvX64RegisterName::Pl3Ssp => x86defs::X86X_MSR_PL3_SSP,
        HvX64RegisterName::InterruptSspTableAddr => x86defs::X86X_MSR_INTERRUPT_SSP_TABLE_ADDR,

        HvX64RegisterName::GuestOsId => hvdef::HV_X64_MSR_GUEST_OS_ID,
        HvX64RegisterName::Hypercall => hvdef::HV_X64_MSR_HYPERCALL,
        HvX64RegisterName::ReferenceTsc => hvdef::HV_X64_MSR_REFERENCE_TSC,
        HvX64RegisterName::TimeRefCount => hvdef::HV_X64_MSR_TIME_REF_COUNT,

        HvX64RegisterName::VpAssistPage => hvdef::HV_X64_MSR_VP_ASSIST_PAGE,

        HvX64RegisterName::Sversion => hvdef::HV_X64_MSR_SVERSION,
        HvX64RegisterName::Scontrol => hvdef::HV_X64_MSR_SCONTROL,
        HvX64RegisterName::Sifp => hvdef::HV_X64_MSR_SIEFP,
        HvX64RegisterName::Sipp => hvdef::HV_X64_MSR_SIMP,
        HvX64RegisterName::Eom => hvdef::HV_X64_MSR_EOM,
        HvX64RegisterName::Sint0 => hvdef::HV_X64_MSR_SINT0,
        HvX64RegisterName::Sint1 => hvdef::HV_X64_MSR_SINT1,
        HvX64RegisterName::Sint2 => hvdef::HV_X64_MSR_SINT2,
        HvX64RegisterName::Sint3 => hvdef::HV_X64_MSR_SINT3,
        HvX64RegisterName::Sint4 => hvdef::HV_X64_MSR_SINT4,
        HvX64RegisterName::Sint5 => hvdef::HV_X64_MSR_SINT5,
        HvX64RegisterName::Sint6 => hvdef::HV_X64_MSR_SINT6,
        HvX64RegisterName::Sint7 => hvdef::HV_X64_MSR_SINT7,
        HvX64RegisterName::Sint8 => hvdef::HV_X64_MSR_SINT8,
        HvX64RegisterName::Sint9 => hvdef::HV_X64_MSR_SINT9,
        HvX64RegisterName::Sint10 => hvdef::HV_X64_MSR_SINT10,
        HvX64RegisterName::Sint11 => hvdef::HV_X64_MSR_SINT11,
        HvX64RegisterName::Sint12 => hvdef::HV_X64_MSR_SINT12,
        HvX64RegisterName::Sint13 => hvdef::HV_X64_MSR_SINT13,
        HvX64RegisterName::Sint14 => hvdef::HV_X64_MSR_SINT14,
        HvX64RegisterName::Sint15 => hvdef::HV_X64_MSR_SINT15,

        HvX64RegisterName::GuestCrashP0 => hvdef::HV_X64_MSR_GUEST_CRASH_P0,
        HvX64RegisterName::GuestCrashP1 => hvdef::HV_X64_MSR_GUEST_CRASH_P1,
        HvX64RegisterName::GuestCrashP2 => hvdef::HV_X64_MSR_GUEST_CRASH_P2,
        HvX64RegisterName::GuestCrashP3 => hvdef::HV_X64_MSR_GUEST_CRASH_P3,
        HvX64RegisterName::GuestCrashP4 => hvdef::HV_X64_MSR_GUEST_CRASH_P4,
        HvX64RegisterName::GuestCrashCtl => hvdef::HV_X64_MSR_GUEST_CRASH_CTL,

        _ => return Err(NoRegisterMapping(name)),
    })
}
