// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub trait ToWhpRegister: 'static + Copy + std::fmt::Debug {
    fn to_whp_register(self) -> Option<whp::abi::WHV_REGISTER_NAME>;
}

#[derive(Debug)]
pub struct NoRegisterMapping<T>(T);

pub fn hv_register_to_whp<T: ToWhpRegister>(
    name: T,
) -> Result<whp::abi::WHV_REGISTER_NAME, NoRegisterMapping<T>> {
    T::to_whp_register(name).ok_or(NoRegisterMapping(name))
}

#[cfg(guest_arch = "x86_64")]
mod x86_64 {
    use super::ToWhpRegister;
    use hvdef::HvX64RegisterName;
    use whp::abi::WHV_REGISTER_NAME;

    impl ToWhpRegister for HvX64RegisterName {
        /// Maps a hypervisor register name to a WHP register name.
        fn to_whp_register(self) -> Option<WHV_REGISTER_NAME> {
            let r = match self {
                HvX64RegisterName::InternalActivityState => {
                    whp::abi::WHvRegisterInternalActivityState
                }

                HvX64RegisterName::PendingInterruption => whp::abi::WHvRegisterPendingInterruption,
                HvX64RegisterName::InterruptState => whp::abi::WHvRegisterInterruptState,
                HvX64RegisterName::PendingEvent0 => whp::abi::WHvRegisterPendingEvent,

                HvX64RegisterName::Rax => whp::abi::WHvX64RegisterRax,
                HvX64RegisterName::Rcx => whp::abi::WHvX64RegisterRcx,
                HvX64RegisterName::Rdx => whp::abi::WHvX64RegisterRdx,
                HvX64RegisterName::Rbx => whp::abi::WHvX64RegisterRbx,
                HvX64RegisterName::Rsp => whp::abi::WHvX64RegisterRsp,
                HvX64RegisterName::Rbp => whp::abi::WHvX64RegisterRbp,
                HvX64RegisterName::Rsi => whp::abi::WHvX64RegisterRsi,
                HvX64RegisterName::Rdi => whp::abi::WHvX64RegisterRdi,
                HvX64RegisterName::R8 => whp::abi::WHvX64RegisterR8,
                HvX64RegisterName::R9 => whp::abi::WHvX64RegisterR9,
                HvX64RegisterName::R10 => whp::abi::WHvX64RegisterR10,
                HvX64RegisterName::R11 => whp::abi::WHvX64RegisterR11,
                HvX64RegisterName::R12 => whp::abi::WHvX64RegisterR12,
                HvX64RegisterName::R13 => whp::abi::WHvX64RegisterR13,
                HvX64RegisterName::R14 => whp::abi::WHvX64RegisterR14,
                HvX64RegisterName::R15 => whp::abi::WHvX64RegisterR15,
                HvX64RegisterName::Rip => whp::abi::WHvX64RegisterRip,
                HvX64RegisterName::Rflags => whp::abi::WHvX64RegisterRflags,

                HvX64RegisterName::Dr0 => whp::abi::WHvX64RegisterDr0,
                HvX64RegisterName::Dr1 => whp::abi::WHvX64RegisterDr1,
                HvX64RegisterName::Dr2 => whp::abi::WHvX64RegisterDr2,
                HvX64RegisterName::Dr3 => whp::abi::WHvX64RegisterDr3,
                HvX64RegisterName::Dr6 => whp::abi::WHvX64RegisterDr6,
                HvX64RegisterName::Dr7 => whp::abi::WHvX64RegisterDr7,

                HvX64RegisterName::Cs => whp::abi::WHvX64RegisterCs,
                HvX64RegisterName::Ds => whp::abi::WHvX64RegisterDs,
                HvX64RegisterName::Es => whp::abi::WHvX64RegisterEs,
                HvX64RegisterName::Fs => whp::abi::WHvX64RegisterFs,
                HvX64RegisterName::Gs => whp::abi::WHvX64RegisterGs,
                HvX64RegisterName::Ss => whp::abi::WHvX64RegisterSs,
                HvX64RegisterName::Tr => whp::abi::WHvX64RegisterTr,
                HvX64RegisterName::Ldtr => whp::abi::WHvX64RegisterLdtr,
                HvX64RegisterName::Gdtr => whp::abi::WHvX64RegisterGdtr,
                HvX64RegisterName::Idtr => whp::abi::WHvX64RegisterIdtr,
                HvX64RegisterName::Cr0 => whp::abi::WHvX64RegisterCr0,
                HvX64RegisterName::Cr2 => whp::abi::WHvX64RegisterCr2,
                HvX64RegisterName::Cr3 => whp::abi::WHvX64RegisterCr3,
                HvX64RegisterName::Cr4 => whp::abi::WHvX64RegisterCr4,
                HvX64RegisterName::Cr8 => whp::abi::WHvX64RegisterCr8,
                HvX64RegisterName::Xfem => whp::abi::WHvX64RegisterXCr0,
                HvX64RegisterName::Tsc => whp::abi::WHvX64RegisterTsc,
                HvX64RegisterName::Efer => whp::abi::WHvX64RegisterEfer,

                HvX64RegisterName::ApicBase => whp::abi::WHvX64RegisterApicBase,
                HvX64RegisterName::KernelGsBase => whp::abi::WHvX64RegisterKernelGsBase,
                HvX64RegisterName::SysenterCs => whp::abi::WHvX64RegisterSysenterCs,
                HvX64RegisterName::SysenterEsp => whp::abi::WHvX64RegisterSysenterEsp,
                HvX64RegisterName::SysenterEip => whp::abi::WHvX64RegisterSysenterEip,
                HvX64RegisterName::Star => whp::abi::WHvX64RegisterStar,
                HvX64RegisterName::Lstar => whp::abi::WHvX64RegisterLstar,
                HvX64RegisterName::Cstar => whp::abi::WHvX64RegisterCstar,
                HvX64RegisterName::Sfmask => whp::abi::WHvX64RegisterSfmask,
                HvX64RegisterName::TscAux => whp::abi::WHvX64RegisterTscAux,

                HvX64RegisterName::Pat => whp::abi::WHvX64RegisterPat,
                HvX64RegisterName::MsrMtrrDefType => whp::abi::WHvX64RegisterMsrMtrrDefType,
                HvX64RegisterName::MsrMtrrFix64k00000 => whp::abi::WHvX64RegisterMsrMtrrFix64k00000,
                HvX64RegisterName::MsrMtrrFix16k80000 => whp::abi::WHvX64RegisterMsrMtrrFix16k80000,
                HvX64RegisterName::MsrMtrrFix16kA0000 => whp::abi::WHvX64RegisterMsrMtrrFix16kA0000,
                HvX64RegisterName::MsrMtrrFix4kC0000 => whp::abi::WHvX64RegisterMsrMtrrFix4kC0000,
                HvX64RegisterName::MsrMtrrFix4kC8000 => whp::abi::WHvX64RegisterMsrMtrrFix4kC8000,
                HvX64RegisterName::MsrMtrrFix4kD0000 => whp::abi::WHvX64RegisterMsrMtrrFix4kD0000,
                HvX64RegisterName::MsrMtrrFix4kD8000 => whp::abi::WHvX64RegisterMsrMtrrFix4kD8000,
                HvX64RegisterName::MsrMtrrFix4kE0000 => whp::abi::WHvX64RegisterMsrMtrrFix4kE0000,
                HvX64RegisterName::MsrMtrrFix4kE8000 => whp::abi::WHvX64RegisterMsrMtrrFix4kE8000,
                HvX64RegisterName::MsrMtrrFix4kF0000 => whp::abi::WHvX64RegisterMsrMtrrFix4kF0000,
                HvX64RegisterName::MsrMtrrFix4kF8000 => whp::abi::WHvX64RegisterMsrMtrrFix4kF8000,

                HvX64RegisterName::MsrMtrrPhysBase0 => whp::abi::WHvX64RegisterMsrMtrrPhysBase0,
                HvX64RegisterName::MsrMtrrPhysMask0 => whp::abi::WHvX64RegisterMsrMtrrPhysMask0,
                HvX64RegisterName::MsrMtrrPhysBase1 => whp::abi::WHvX64RegisterMsrMtrrPhysBase1,
                HvX64RegisterName::MsrMtrrPhysMask1 => whp::abi::WHvX64RegisterMsrMtrrPhysMask1,
                HvX64RegisterName::MsrMtrrPhysBase2 => whp::abi::WHvX64RegisterMsrMtrrPhysBase2,
                HvX64RegisterName::MsrMtrrPhysMask2 => whp::abi::WHvX64RegisterMsrMtrrPhysMask2,
                HvX64RegisterName::MsrMtrrPhysBase3 => whp::abi::WHvX64RegisterMsrMtrrPhysBase3,
                HvX64RegisterName::MsrMtrrPhysMask3 => whp::abi::WHvX64RegisterMsrMtrrPhysMask3,
                HvX64RegisterName::MsrMtrrPhysBase4 => whp::abi::WHvX64RegisterMsrMtrrPhysBase4,
                HvX64RegisterName::MsrMtrrPhysMask4 => whp::abi::WHvX64RegisterMsrMtrrPhysMask4,
                HvX64RegisterName::MsrMtrrPhysBase5 => whp::abi::WHvX64RegisterMsrMtrrPhysBase5,
                HvX64RegisterName::MsrMtrrPhysMask5 => whp::abi::WHvX64RegisterMsrMtrrPhysMask5,
                HvX64RegisterName::MsrMtrrPhysBase6 => whp::abi::WHvX64RegisterMsrMtrrPhysBase6,
                HvX64RegisterName::MsrMtrrPhysMask6 => whp::abi::WHvX64RegisterMsrMtrrPhysMask6,
                HvX64RegisterName::MsrMtrrPhysBase7 => whp::abi::WHvX64RegisterMsrMtrrPhysBase7,
                HvX64RegisterName::MsrMtrrPhysMask7 => whp::abi::WHvX64RegisterMsrMtrrPhysMask7,

                HvX64RegisterName::Xss => whp::abi::WHvX64RegisterXss,
                HvX64RegisterName::UCet => whp::abi::WHvX64RegisterUCet,
                HvX64RegisterName::SCet => whp::abi::WHvX64RegisterSCet,
                HvX64RegisterName::Ssp => whp::abi::WHvX64RegisterSsp,
                HvX64RegisterName::Pl0Ssp => whp::abi::WHvX64RegisterPl0Ssp,
                HvX64RegisterName::Pl1Ssp => whp::abi::WHvX64RegisterPl1Ssp,
                HvX64RegisterName::Pl2Ssp => whp::abi::WHvX64RegisterPl2Ssp,
                HvX64RegisterName::Pl3Ssp => whp::abi::WHvX64RegisterPl3Ssp,
                HvX64RegisterName::InterruptSspTableAddr => {
                    whp::abi::WHvX64RegisterInterruptSspTableAddr
                }

                HvX64RegisterName::GuestOsId => whp::abi::WHvRegisterGuestOsId,
                HvX64RegisterName::Hypercall => whp::abi::WHvX64RegisterHypercall,
                HvX64RegisterName::ReferenceTsc => whp::abi::WHvRegisterReferenceTsc,

                HvX64RegisterName::VpAssistPage => whp::abi::WHvRegisterVpAssistPage,

                HvX64RegisterName::Sversion => whp::abi::WHvRegisterSversion,
                HvX64RegisterName::Scontrol => whp::abi::WHvRegisterScontrol,
                HvX64RegisterName::Sifp => whp::abi::WHvRegisterSiefp,
                HvX64RegisterName::Sipp => whp::abi::WHvRegisterSimp,
                HvX64RegisterName::Eom => whp::abi::WHvRegisterEom,
                HvX64RegisterName::Sint0 => whp::abi::WHvRegisterSint0,
                HvX64RegisterName::Sint1 => whp::abi::WHvRegisterSint1,
                HvX64RegisterName::Sint2 => whp::abi::WHvRegisterSint2,
                HvX64RegisterName::Sint3 => whp::abi::WHvRegisterSint3,
                HvX64RegisterName::Sint4 => whp::abi::WHvRegisterSint4,
                HvX64RegisterName::Sint5 => whp::abi::WHvRegisterSint5,
                HvX64RegisterName::Sint6 => whp::abi::WHvRegisterSint6,
                HvX64RegisterName::Sint7 => whp::abi::WHvRegisterSint7,
                HvX64RegisterName::Sint8 => whp::abi::WHvRegisterSint8,
                HvX64RegisterName::Sint9 => whp::abi::WHvRegisterSint9,
                HvX64RegisterName::Sint10 => whp::abi::WHvRegisterSint10,
                HvX64RegisterName::Sint11 => whp::abi::WHvRegisterSint11,
                HvX64RegisterName::Sint12 => whp::abi::WHvRegisterSint12,
                HvX64RegisterName::Sint13 => whp::abi::WHvRegisterSint13,
                HvX64RegisterName::Sint14 => whp::abi::WHvRegisterSint14,
                HvX64RegisterName::Sint15 => whp::abi::WHvRegisterSint15,

                _ => return None,
            };
            Some(r)
        }
    }
}

#[cfg(guest_arch = "aarch64")]
mod aarch64 {
    use super::ToWhpRegister;
    use hvdef::HvArm64RegisterName;

    impl ToWhpRegister for HvArm64RegisterName {
        fn to_whp_register(self) -> Option<whp::abi::WHV_REGISTER_NAME> {
            let r = match self {
                HvArm64RegisterName::X0 => whp::abi::WHvArm64RegisterX0,
                HvArm64RegisterName::X1 => whp::abi::WHvArm64RegisterX1,
                HvArm64RegisterName::X2 => whp::abi::WHvArm64RegisterX2,
                HvArm64RegisterName::X3 => whp::abi::WHvArm64RegisterX3,
                HvArm64RegisterName::X4 => whp::abi::WHvArm64RegisterX4,
                HvArm64RegisterName::X5 => whp::abi::WHvArm64RegisterX5,
                HvArm64RegisterName::X6 => whp::abi::WHvArm64RegisterX6,
                HvArm64RegisterName::X7 => whp::abi::WHvArm64RegisterX7,
                HvArm64RegisterName::X8 => whp::abi::WHvArm64RegisterX8,
                HvArm64RegisterName::X9 => whp::abi::WHvArm64RegisterX9,
                HvArm64RegisterName::X10 => whp::abi::WHvArm64RegisterX10,
                HvArm64RegisterName::X11 => whp::abi::WHvArm64RegisterX11,
                HvArm64RegisterName::X12 => whp::abi::WHvArm64RegisterX12,
                HvArm64RegisterName::X13 => whp::abi::WHvArm64RegisterX13,
                HvArm64RegisterName::X14 => whp::abi::WHvArm64RegisterX14,
                HvArm64RegisterName::X15 => whp::abi::WHvArm64RegisterX15,
                HvArm64RegisterName::X16 => whp::abi::WHvArm64RegisterX16,
                HvArm64RegisterName::X17 => whp::abi::WHvArm64RegisterX17,
                HvArm64RegisterName::X18 => whp::abi::WHvArm64RegisterX18,
                HvArm64RegisterName::X19 => whp::abi::WHvArm64RegisterX19,
                HvArm64RegisterName::X20 => whp::abi::WHvArm64RegisterX20,
                HvArm64RegisterName::X21 => whp::abi::WHvArm64RegisterX21,
                HvArm64RegisterName::X22 => whp::abi::WHvArm64RegisterX22,
                HvArm64RegisterName::X23 => whp::abi::WHvArm64RegisterX23,
                HvArm64RegisterName::X24 => whp::abi::WHvArm64RegisterX24,
                HvArm64RegisterName::X25 => whp::abi::WHvArm64RegisterX25,
                HvArm64RegisterName::X26 => whp::abi::WHvArm64RegisterX26,
                HvArm64RegisterName::X27 => whp::abi::WHvArm64RegisterX27,
                HvArm64RegisterName::X28 => whp::abi::WHvArm64RegisterX28,
                HvArm64RegisterName::XFp => whp::abi::WHvArm64RegisterFp,
                HvArm64RegisterName::XLr => whp::abi::WHvArm64RegisterLr,
                HvArm64RegisterName::XSpEl0 => whp::abi::WHvArm64RegisterSpEl0,
                HvArm64RegisterName::XSpElx => whp::abi::WHvArm64RegisterSpEl1,
                HvArm64RegisterName::XPc => whp::abi::WHvArm64RegisterPc,
                HvArm64RegisterName::Cpsr => whp::abi::WHvArm64RegisterPstate,
                HvArm64RegisterName::SctlrEl1 => whp::abi::WHvArm64RegisterSctlrEl1,
                HvArm64RegisterName::Ttbr0El1 => whp::abi::WHvArm64RegisterTtbr0El1,
                HvArm64RegisterName::Ttbr1El1 => whp::abi::WHvArm64RegisterTtbr1El1,
                HvArm64RegisterName::TcrEl1 => whp::abi::WHvArm64RegisterTcrEl1,
                HvArm64RegisterName::EsrEl1 => whp::abi::WHvArm64RegisterEsrEl1,
                HvArm64RegisterName::FarEl1 => whp::abi::WHvArm64RegisterFarEl1,
                HvArm64RegisterName::MairEl1 => whp::abi::WHvArm64RegisterMairEl1,
                HvArm64RegisterName::VbarEl1 => whp::abi::WHvArm64RegisterVbarEl1,
                HvArm64RegisterName::ElrEl1 => whp::abi::WHvArm64RegisterElrEl1,

                HvArm64RegisterName::GuestOsId => whp::abi::WHvRegisterGuestOsId,

                HvArm64RegisterName::Sversion => whp::abi::WHvRegisterSversion,
                HvArm64RegisterName::Scontrol => whp::abi::WHvRegisterScontrol,
                HvArm64RegisterName::Sifp => whp::abi::WHvRegisterSiefp,
                HvArm64RegisterName::Sipp => whp::abi::WHvRegisterSimp,
                HvArm64RegisterName::Eom => whp::abi::WHvRegisterEom,
                HvArm64RegisterName::Sint0 => whp::abi::WHvRegisterSint0,
                HvArm64RegisterName::Sint1 => whp::abi::WHvRegisterSint1,
                HvArm64RegisterName::Sint2 => whp::abi::WHvRegisterSint2,
                HvArm64RegisterName::Sint3 => whp::abi::WHvRegisterSint3,
                HvArm64RegisterName::Sint4 => whp::abi::WHvRegisterSint4,
                HvArm64RegisterName::Sint5 => whp::abi::WHvRegisterSint5,
                HvArm64RegisterName::Sint6 => whp::abi::WHvRegisterSint6,
                HvArm64RegisterName::Sint7 => whp::abi::WHvRegisterSint7,
                HvArm64RegisterName::Sint8 => whp::abi::WHvRegisterSint8,
                HvArm64RegisterName::Sint9 => whp::abi::WHvRegisterSint9,
                HvArm64RegisterName::Sint10 => whp::abi::WHvRegisterSint10,
                HvArm64RegisterName::Sint11 => whp::abi::WHvRegisterSint11,
                HvArm64RegisterName::Sint12 => whp::abi::WHvRegisterSint12,
                HvArm64RegisterName::Sint13 => whp::abi::WHvRegisterSint13,
                HvArm64RegisterName::Sint14 => whp::abi::WHvRegisterSint14,
                HvArm64RegisterName::Sint15 => whp::abi::WHvRegisterSint15,

                _ => return None,
            };
            Some(r)
        }
    }
}
