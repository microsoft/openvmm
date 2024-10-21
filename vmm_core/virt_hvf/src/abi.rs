// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Definitions for interfacing with the Hyperfisor framework C ABI.

use aarch64defs::EsrEl2;
use open_enum::open_enum;
use std::ffi::c_void;
use std::fmt::Display;

const ERR_COMMON_HYPERVISOR: i32 = 0xfae94000_u32 as i32;

open_enum! {
    pub enum HvfError: i32 {
        SUCCESS = 0,
        ERROR = ERR_COMMON_HYPERVISOR | 0x1,
        BUSY = ERR_COMMON_HYPERVISOR | 0x2,
        BAD_ARGUMENT = ERR_COMMON_HYPERVISOR | 0x3,
        NO_RESOURCES = ERR_COMMON_HYPERVISOR | 0x5,
        NO_DEVICE = ERR_COMMON_HYPERVISOR | 0x6,
        DENIED = ERR_COMMON_HYPERVISOR | 0x7,
        FAULT = ERR_COMMON_HYPERVISOR | 0x8,
        UNSUPPORTED = ERR_COMMON_HYPERVISOR | 0xf,
    }
}

impl From<i32> for HvfError {
    fn from(value: i32) -> Self {
        Self(value)
    }
}

impl std::error::Error for HvfError {}

impl Display for HvfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match *self {
            Self::SUCCESS => "The operation completed successfully.",
            Self::ERROR => "The operation was unsuccessful.",
            Self::BUSY => "The operation was unsuccessful because the owning resource was busy.",
            Self::BAD_ARGUMENT => "The operation was unsuccessful because the function call had an invalid argument.",
            Self::NO_RESOURCES => "The operation was unsuccessful because the host had no resources available to complete the request.",
            Self::NO_DEVICE => "The operation was unsuccessful because no VM or vCPU was available.",
            Self::DENIED => "The system didn’t allow the requested operation.",
            // Self::FAULT => "",
            Self::UNSUPPORTED => "The operation requested isn’t supported by the hypervisor.",
            _ => return write!(f, "{:#x?}", self),
        };
        f.pad(s)
    }
}

#[must_use]
#[repr(transparent)]
pub struct HvfResult(HvfError);

impl HvfResult {
    pub fn chk(&self) -> Result<(), HvfError> {
        if self.0 == HvfError::SUCCESS {
            Ok(())
        } else {
            Err(self.0)
        }
    }
}

#[link(name = "Hypervisor", kind = "framework")]
extern "C" {
    pub fn hv_vm_create(config: *const ()) -> HvfResult;
    pub fn hv_vm_destroy() -> HvfResult;
    pub fn hv_vm_map(addr: *mut c_void, ipa: u64, size: usize, flags: u64) -> HvfResult;
    pub fn hv_vm_unmap(ipa: u64, size: usize) -> HvfResult;
    pub fn hv_vcpu_create(
        vcpu: *mut u64,
        exit: *mut *mut HvVcpuExit,
        config: *const (),
    ) -> HvfResult;
    pub fn hv_vcpu_destroy(vcpu: u64) -> HvfResult;
    pub fn hv_vcpu_run(vcpu: u64) -> HvfResult;
    pub fn hv_vcpus_exit(vcpus: *const u64, vcpu_count: u32) -> HvfResult;
    pub fn hv_vcpu_get_reg(vcpu: u64, reg: HvReg, value: *mut u64) -> HvfResult;
    pub fn hv_vcpu_set_reg(vcpu: u64, reg: HvReg, value: u64) -> HvfResult;
    pub fn hv_vcpu_get_sys_reg(vcpu: u64, reg: HvSysReg, value: *mut u64) -> HvfResult;
    pub fn hv_vcpu_set_sys_reg(vcpu: u64, reg: HvSysReg, value: u64) -> HvfResult;
    #[allow(dead_code)]
    pub fn hv_vcpu_get_pending_interrupt(
        vcpu: u64,
        ty: HvInterruptType,
        pending: *mut bool,
    ) -> HvfResult;
    pub fn hv_vcpu_set_pending_interrupt(
        vcpu: u64,
        ty: HvInterruptType,
        pending: bool,
    ) -> HvfResult;
    #[allow(dead_code)]
    pub fn hv_vcpu_get_vtimer_mask(vcpu: u64, vtimer_is_masked: *mut bool) -> HvfResult;
    pub fn hv_vcpu_set_vtimer_mask(vcpu: u64, vtimer_is_masked: bool) -> HvfResult;
}

open_enum! {
    pub enum HvMemoryFlags: u64 {
        READ = 1 << 0,
        WRITE = 1 << 1,
        EXEC = 1 << 2,
        UEXEC = 1 << 3,
        MAXPROT = 1 << 4,
        MAXPROT_READ = 1 << 5,
        MAXPROT_WRITE = 1 << 6,
        MAXPROT_EXEC = 1 << 7,
        MAXPROT_UEXEC = 1 << 8,
    }
}

open_enum! {
    pub enum HvExitReason: u32 {
        CANCELED = 0,
        EXCEPTION = 1,
        VTIMER_ACTIVATED = 2,
        UNKNOWN = 3,
    }
}

#[repr(C)]
pub struct HvVcpuExit {
    pub reason: HvExitReason,
    pub exception: HvVcpuExitException,
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
pub struct HvVcpuExitException {
    pub syndrome: EsrEl2,
    pub virtual_address: u64,
    pub physical_address: u64,
}

open_enum! {
    pub enum HvReg: u32 {
        X0 = 0,
        X1 = 1,
        X2 = 2,
        X3 = 3,
        X4 = 4,
        X5 = 5,
        X6 = 6,
        X7 = 7,
        X8 = 8,
        X9 = 9,
        X10 = 10,
        X11 = 11,
        X12 = 12,
        X13 = 13,
        X14 = 14,
        X15 = 15,
        X16 = 16,
        X17 = 17,
        X18 = 18,
        X19 = 19,
        X20 = 20,
        X21 = 21,
        X22 = 22,
        X23 = 23,
        X24 = 24,
        X25 = 25,
        X26 = 26,
        X27 = 27,
        X28 = 28,
        FP = 29,
        LR = 30,
        PC = 31,
        FPCR = 32,
        FPSR = 33,
        CPSR = 34,
    }
}

open_enum! {
    pub enum HvSysReg: u16 {
        DBGBVR0_EL1 = 0x8004,
        DBGBCR0_EL1 = 0x8005,
        DBGWVR0_EL1 = 0x8006,
        DBGWCR0_EL1 = 0x8007,
        DBGBVR1_EL1 = 0x800c,
        DBGBCR1_EL1 = 0x800d,
        DBGWVR1_EL1 = 0x800e,
        DBGWCR1_EL1 = 0x800f,
        MDCCINT_EL1 = 0x8010,
        MDSCR_EL1 = 0x8012,
        DBGBVR2_EL1 = 0x8014,
        DBGBCR2_EL1 = 0x8015,
        DBGWVR2_EL1 = 0x8016,
        DBGWCR2_EL1 = 0x8017,
        DBGBVR3_EL1 = 0x801c,
        DBGBCR3_EL1 = 0x801d,
        DBGWVR3_EL1 = 0x801e,
        DBGWCR3_EL1 = 0x801f,
        DBGBVR4_EL1 = 0x8024,
        DBGBCR4_EL1 = 0x8025,
        DBGWVR4_EL1 = 0x8026,
        DBGWCR4_EL1 = 0x8027,
        DBGBVR5_EL1 = 0x802c,
        DBGBCR5_EL1 = 0x802d,
        DBGWVR5_EL1 = 0x802e,
        DBGWCR5_EL1 = 0x802f,
        DBGBVR6_EL1 = 0x8034,
        DBGBCR6_EL1 = 0x8035,
        DBGWVR6_EL1 = 0x8036,
        DBGWCR6_EL1 = 0x8037,
        DBGBVR7_EL1 = 0x803c,
        DBGBCR7_EL1 = 0x803d,
        DBGWVR7_EL1 = 0x803e,
        DBGWCR7_EL1 = 0x803f,
        DBGBVR8_EL1 = 0x8044,
        DBGBCR8_EL1 = 0x8045,
        DBGWVR8_EL1 = 0x8046,
        DBGWCR8_EL1 = 0x8047,
        DBGBVR9_EL1 = 0x804c,
        DBGBCR9_EL1 = 0x804d,
        DBGWVR9_EL1 = 0x804e,
        DBGWCR9_EL1 = 0x804f,
        DBGBVR10_EL1 = 0x8054,
        DBGBCR10_EL1 = 0x8055,
        DBGWVR10_EL1 = 0x8056,
        DBGWCR10_EL1 = 0x8057,
        DBGBVR11_EL1 = 0x805c,
        DBGBCR11_EL1 = 0x805d,
        DBGWVR11_EL1 = 0x805e,
        DBGWCR11_EL1 = 0x805f,
        DBGBVR12_EL1 = 0x8064,
        DBGBCR12_EL1 = 0x8065,
        DBGWVR12_EL1 = 0x8066,
        DBGWCR12_EL1 = 0x8067,
        DBGBVR13_EL1 = 0x806c,
        DBGBCR13_EL1 = 0x806d,
        DBGWVR13_EL1 = 0x806e,
        DBGWCR13_EL1 = 0x806f,
        DBGBVR14_EL1 = 0x8074,
        DBGBCR14_EL1 = 0x8075,
        DBGWVR14_EL1 = 0x8076,
        DBGWCR14_EL1 = 0x8077,
        DBGBVR15_EL1 = 0x807c,
        DBGBCR15_EL1 = 0x807d,
        DBGWVR15_EL1 = 0x807e,
        DBGWCR15_EL1 = 0x807f,
        MIDR_EL1 = 0xc000,
        MPIDR_EL1 = 0xc005,
        ID_AA64PFR0_EL1 = 0xc020,
        ID_AA64PFR1_EL1 = 0xc021,
        ID_AA64DFR0_EL1 = 0xc028,
        ID_AA64DFR1_EL1 = 0xc029,
        ID_AA64ISAR0_EL1 = 0xc030,
        ID_AA64ISAR1_EL1 = 0xc031,
        ID_AA64MMFR0_EL1 = 0xc038,
        ID_AA64MMFR1_EL1 = 0xc039,
        ID_AA64MMFR2_EL1 = 0xc03a,
        SCTLR_EL1 = 0xc080,
        CPACR_EL1 = 0xc082,
        TTBR0_EL1 = 0xc100,
        TTBR1_EL1 = 0xc101,
        TCR_EL1 = 0xc102,
        APIAKEYLO_EL1 = 0xc108,
        APIAKEYHI_EL1 = 0xc109,
        APIBKEYLO_EL1 = 0xc10a,
        APIBKEYHI_EL1 = 0xc10b,
        APDAKEYLO_EL1 = 0xc110,
        APDAKEYHI_EL1 = 0xc111,
        APDBKEYLO_EL1 = 0xc112,
        APDBKEYHI_EL1 = 0xc113,
        APGAKEYLO_EL1 = 0xc118,
        APGAKEYHI_EL1 = 0xc119,
        SPSR_EL1 = 0xc200,
        ELR_EL1 = 0xc201,
        SP_EL0 = 0xc208,
        AFSR0_EL1 = 0xc288,
        AFSR1_EL1 = 0xc289,
        ESR_EL1 = 0xc290,
        FAR_EL1 = 0xc300,
        PAR_EL1 = 0xc3a0,
        MAIR_EL1 = 0xc510,
        AMAIR_EL1 = 0xc518,
        VBAR_EL1 = 0xc600,
        CONTEXTIDR_EL1 = 0xc681,
        TPIDR_EL1 = 0xc684,
        CNTKCTL_EL1 = 0xc708,
        CSSELR_EL1 = 0xd000,
        TPIDR_EL0 = 0xde82,
        TPIDRRO_EL0 = 0xde83,
        CNTV_CTL_EL0 = 0xdf19,
        CNTV_CVAL_EL0 = 0xdf1a,
        SP_EL1 = 0xe208,
    }
}

open_enum! {
    pub enum HvInterruptType: u32 {
        IRQ = 0,
        FIQ = 1,
    }
}
