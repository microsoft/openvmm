// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_arch = "x86_64")]

use crate::RegisterName;
use crate::RegisterValue;
use crate::abi;

/// 64-bit registers
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Register64 {
    Rax = abi::WHvX64RegisterRax.0,
    Rcx = abi::WHvX64RegisterRcx.0,
    Rdx = abi::WHvX64RegisterRdx.0,
    Rbx = abi::WHvX64RegisterRbx.0,
    Rsp = abi::WHvX64RegisterRsp.0,
    Rbp = abi::WHvX64RegisterRbp.0,
    Rsi = abi::WHvX64RegisterRsi.0,
    Rdi = abi::WHvX64RegisterRdi.0,
    R8 = abi::WHvX64RegisterR8.0,
    R9 = abi::WHvX64RegisterR9.0,
    R10 = abi::WHvX64RegisterR10.0,
    R11 = abi::WHvX64RegisterR11.0,
    R12 = abi::WHvX64RegisterR12.0,
    R13 = abi::WHvX64RegisterR13.0,
    R14 = abi::WHvX64RegisterR14.0,
    R15 = abi::WHvX64RegisterR15.0,
    Rip = abi::WHvX64RegisterRip.0,
    Rflags = abi::WHvX64RegisterRflags.0,
    Cr0 = abi::WHvX64RegisterCr0.0,
    Cr2 = abi::WHvX64RegisterCr2.0,
    Cr3 = abi::WHvX64RegisterCr3.0,
    Cr4 = abi::WHvX64RegisterCr4.0,
    Cr8 = abi::WHvX64RegisterCr8.0,
    Dr0 = abi::WHvX64RegisterDr0.0,
    Dr1 = abi::WHvX64RegisterDr1.0,
    Dr2 = abi::WHvX64RegisterDr2.0,
    Dr3 = abi::WHvX64RegisterDr3.0,
    Dr6 = abi::WHvX64RegisterDr6.0,
    Dr7 = abi::WHvX64RegisterDr7.0,
    XCr0 = abi::WHvX64RegisterXCr0.0,

    Tsc = abi::WHvX64RegisterTsc.0,
    Efer = abi::WHvX64RegisterEfer.0,
    KernelGsBase = abi::WHvX64RegisterKernelGsBase.0,
    ApicBase = abi::WHvX64RegisterApicBase.0,
    Pat = abi::WHvX64RegisterPat.0,
    SysenterCs = abi::WHvX64RegisterSysenterCs.0,
    SysenterEip = abi::WHvX64RegisterSysenterEip.0,
    SysenterEsp = abi::WHvX64RegisterSysenterEsp.0,
    Star = abi::WHvX64RegisterStar.0,
    Lstar = abi::WHvX64RegisterLstar.0,
    Cstar = abi::WHvX64RegisterCstar.0,
    Sfmask = abi::WHvX64RegisterSfmask.0,
    MsrMtrrCap = abi::WHvX64RegisterMsrMtrrCap.0,
    MsrMtrrDefType = abi::WHvX64RegisterMsrMtrrDefType.0,
    MsrMtrrPhysBase0 = abi::WHvX64RegisterMsrMtrrPhysBase0.0,
    MsrMtrrPhysBase1 = abi::WHvX64RegisterMsrMtrrPhysBase1.0,
    MsrMtrrPhysBase2 = abi::WHvX64RegisterMsrMtrrPhysBase2.0,
    MsrMtrrPhysBase3 = abi::WHvX64RegisterMsrMtrrPhysBase3.0,
    MsrMtrrPhysBase4 = abi::WHvX64RegisterMsrMtrrPhysBase4.0,
    MsrMtrrPhysBase5 = abi::WHvX64RegisterMsrMtrrPhysBase5.0,
    MsrMtrrPhysBase6 = abi::WHvX64RegisterMsrMtrrPhysBase6.0,
    MsrMtrrPhysBase7 = abi::WHvX64RegisterMsrMtrrPhysBase7.0,
    MsrMtrrPhysBase8 = abi::WHvX64RegisterMsrMtrrPhysBase8.0,
    MsrMtrrPhysBase9 = abi::WHvX64RegisterMsrMtrrPhysBase9.0,
    MsrMtrrPhysBaseA = abi::WHvX64RegisterMsrMtrrPhysBaseA.0,
    MsrMtrrPhysBaseB = abi::WHvX64RegisterMsrMtrrPhysBaseB.0,
    MsrMtrrPhysBaseC = abi::WHvX64RegisterMsrMtrrPhysBaseC.0,
    MsrMtrrPhysBaseD = abi::WHvX64RegisterMsrMtrrPhysBaseD.0,
    MsrMtrrPhysBaseE = abi::WHvX64RegisterMsrMtrrPhysBaseE.0,
    MsrMtrrPhysBaseF = abi::WHvX64RegisterMsrMtrrPhysBaseF.0,
    MsrMtrrPhysMask0 = abi::WHvX64RegisterMsrMtrrPhysMask0.0,
    MsrMtrrPhysMask1 = abi::WHvX64RegisterMsrMtrrPhysMask1.0,
    MsrMtrrPhysMask2 = abi::WHvX64RegisterMsrMtrrPhysMask2.0,
    MsrMtrrPhysMask3 = abi::WHvX64RegisterMsrMtrrPhysMask3.0,
    MsrMtrrPhysMask4 = abi::WHvX64RegisterMsrMtrrPhysMask4.0,
    MsrMtrrPhysMask5 = abi::WHvX64RegisterMsrMtrrPhysMask5.0,
    MsrMtrrPhysMask6 = abi::WHvX64RegisterMsrMtrrPhysMask6.0,
    MsrMtrrPhysMask7 = abi::WHvX64RegisterMsrMtrrPhysMask7.0,
    MsrMtrrPhysMask8 = abi::WHvX64RegisterMsrMtrrPhysMask8.0,
    MsrMtrrPhysMask9 = abi::WHvX64RegisterMsrMtrrPhysMask9.0,
    MsrMtrrPhysMaskA = abi::WHvX64RegisterMsrMtrrPhysMaskA.0,
    MsrMtrrPhysMaskB = abi::WHvX64RegisterMsrMtrrPhysMaskB.0,
    MsrMtrrPhysMaskC = abi::WHvX64RegisterMsrMtrrPhysMaskC.0,
    MsrMtrrPhysMaskD = abi::WHvX64RegisterMsrMtrrPhysMaskD.0,
    MsrMtrrPhysMaskE = abi::WHvX64RegisterMsrMtrrPhysMaskE.0,
    MsrMtrrPhysMaskF = abi::WHvX64RegisterMsrMtrrPhysMaskF.0,
    MsrMtrrFix64k00000 = abi::WHvX64RegisterMsrMtrrFix64k00000.0,
    MsrMtrrFix16k80000 = abi::WHvX64RegisterMsrMtrrFix16k80000.0,
    MsrMtrrFix16kA0000 = abi::WHvX64RegisterMsrMtrrFix16kA0000.0,
    MsrMtrrFix4kC0000 = abi::WHvX64RegisterMsrMtrrFix4kC0000.0,
    MsrMtrrFix4kC8000 = abi::WHvX64RegisterMsrMtrrFix4kC8000.0,
    MsrMtrrFix4kD0000 = abi::WHvX64RegisterMsrMtrrFix4kD0000.0,
    MsrMtrrFix4kD8000 = abi::WHvX64RegisterMsrMtrrFix4kD8000.0,
    MsrMtrrFix4kE0000 = abi::WHvX64RegisterMsrMtrrFix4kE0000.0,
    MsrMtrrFix4kE8000 = abi::WHvX64RegisterMsrMtrrFix4kE8000.0,
    MsrMtrrFix4kF0000 = abi::WHvX64RegisterMsrMtrrFix4kF0000.0,
    MsrMtrrFix4kF8000 = abi::WHvX64RegisterMsrMtrrFix4kF8000.0,
    TscAux = abi::WHvX64RegisterTscAux.0,
    SpecCtrl = abi::WHvX64RegisterSpecCtrl.0,
    PredCmd = abi::WHvX64RegisterPredCmd.0,
    TscVirtualOffset = abi::WHvX64RegisterTscVirtualOffset.0,
    ApicId = abi::WHvX64RegisterApicId.0,
    InitialApicId = abi::WHvX64RegisterInitialApicId.0,
    ApicVersion = abi::WHvX64RegisterApicVersion.0,
    PendingInterruption = abi::WHvRegisterPendingInterruption.0,
    InterruptState = abi::WHvRegisterInterruptState.0,
    DeliverabilityNotifications = abi::WHvRegisterDeliverabilityNotifications.0,
    InternalActivityState = abi::WHvRegisterInternalActivityState.0,
}

/// Segment registers
#[repr(u32)]
#[derive(Clone, Copy, Debug)]
pub enum RegisterSegment {
    Es = abi::WHvX64RegisterEs.0,
    Cs = abi::WHvX64RegisterCs.0,
    Ss = abi::WHvX64RegisterSs.0,
    Ds = abi::WHvX64RegisterDs.0,
    Fs = abi::WHvX64RegisterFs.0,
    Gs = abi::WHvX64RegisterGs.0,
    Ldtr = abi::WHvX64RegisterLdtr.0,
    Tr = abi::WHvX64RegisterTr.0,
}

/// Table registers
#[repr(u32)]
#[derive(Clone, Copy, Debug)]
pub enum RegisterTable {
    Idtr = abi::WHvX64RegisterIdtr.0,
    Gdtr = abi::WHvX64RegisterGdtr.0,
}

/// 128-bit registers
#[repr(u32)]
#[derive(Clone, Copy, Debug)]
pub enum Register128 {
    Xmm0 = abi::WHvX64RegisterXmm0.0,
    Xmm1 = abi::WHvX64RegisterXmm1.0,
    Xmm2 = abi::WHvX64RegisterXmm2.0,
    Xmm3 = abi::WHvX64RegisterXmm3.0,
    Xmm4 = abi::WHvX64RegisterXmm4.0,
    Xmm5 = abi::WHvX64RegisterXmm5.0,
    Xmm6 = abi::WHvX64RegisterXmm6.0,
    Xmm7 = abi::WHvX64RegisterXmm7.0,
    Xmm8 = abi::WHvX64RegisterXmm8.0,
    Xmm9 = abi::WHvX64RegisterXmm9.0,
    Xmm10 = abi::WHvX64RegisterXmm10.0,
    Xmm11 = abi::WHvX64RegisterXmm11.0,
    Xmm12 = abi::WHvX64RegisterXmm12.0,
    Xmm13 = abi::WHvX64RegisterXmm13.0,
    Xmm14 = abi::WHvX64RegisterXmm14.0,
    Xmm15 = abi::WHvX64RegisterXmm15.0,
    FpMmx0 = abi::WHvX64RegisterFpMmx0.0,
    FpMmx1 = abi::WHvX64RegisterFpMmx1.0,
    FpMmx2 = abi::WHvX64RegisterFpMmx2.0,
    FpMmx3 = abi::WHvX64RegisterFpMmx3.0,
    FpMmx4 = abi::WHvX64RegisterFpMmx4.0,
    FpMmx5 = abi::WHvX64RegisterFpMmx5.0,
    FpMmx6 = abi::WHvX64RegisterFpMmx6.0,
    FpMmx7 = abi::WHvX64RegisterFpMmx7.0,
    FpControlStatus = abi::WHvX64RegisterFpControlStatus.0,
    XmmControlStatus = abi::WHvX64RegisterXmmControlStatus.0,

    PendingEvent = abi::WHvRegisterPendingEvent.0,
    PendingEvent1 = abi::WHvRegisterPendingEvent1.0,
}

impl RegisterName for RegisterTable {
    type Value = abi::WHV_X64_TABLE_REGISTER;

    fn as_abi(&self) -> abi::WHV_REGISTER_NAME {
        abi::WHV_REGISTER_NAME(*self as u32)
    }
}

impl RegisterName for RegisterSegment {
    type Value = abi::WHV_X64_SEGMENT_REGISTER;

    fn as_abi(&self) -> abi::WHV_REGISTER_NAME {
        abi::WHV_REGISTER_NAME(*self as u32)
    }
}

impl RegisterValue for abi::WHV_X64_SEGMENT_REGISTER {
    fn as_abi(&self) -> abi::WHV_REGISTER_VALUE {
        // SAFETY: any bit pattern is a valid `Self`.
        unsafe { std::mem::transmute(*self) }
    }

    fn from_abi(value: &abi::WHV_REGISTER_VALUE) -> Self {
        // SAFETY: `Self` is safe to cast to a set of bytes, and any bit pattern
        // is a valid `WHV_UINT128`.
        unsafe { std::mem::transmute(value.0) }
    }
}

impl RegisterValue for abi::WHV_X64_TABLE_REGISTER {
    fn as_abi(&self) -> abi::WHV_REGISTER_VALUE {
        // SAFETY: any bit pattern is a valid `Self`.
        unsafe { std::mem::transmute(*self) }
    }

    fn from_abi(value: &abi::WHV_REGISTER_VALUE) -> Self {
        // SAFETY: `Self` is safe to cast to a set of bytes, and any bit pattern
        // is a valid `WHV_UINT128`.
        unsafe { std::mem::transmute(value.0) }
    }
}
