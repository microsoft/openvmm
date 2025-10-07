// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ARM64 type and constant definitions.

#![expect(missing_docs)]
#![forbid(unsafe_code)]
#![no_std]

pub mod gic;
pub mod smccc;

use bitfield_struct::bitfield;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Aarch64 SPSR_EL2 register when in 64-bit mode. Usually called CPSR by
/// hypervisors.
#[bitfield(u64)]
pub struct Cpsr64 {
    /// PSTATE.SP
    pub sp: bool,
    _rsvd0: bool,
    /// Exception Level
    #[bits(2)]
    pub el: u8,
    /// Aarch32 mode. If set, this struct is the wrong one to interpret this
    /// register.
    pub aa32: bool,
    _rsvd1: bool,
    pub f: bool,
    pub i: bool,
    pub a: bool,
    pub d: bool,
    #[bits(2)]
    pub btype: u8,
    pub ssbs: bool,
    #[bits(7)]
    _rsvd2: u8,
    pub il: bool,
    pub ss: bool,
    pub pan: bool,
    pub uao: bool,
    pub dit: bool,
    pub tco: bool,
    #[bits(2)]
    _rsvd3: u8,
    pub v: bool,
    pub c: bool,
    pub z: bool,
    pub n: bool,
    pub res0: u32,
}

/// ESR_EL2, exception syndrome register.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct EsrEl2 {
    #[bits(25)]
    pub iss: u32,
    pub il: bool,
    #[bits(6)]
    pub ec: u8,
    #[bits(5)]
    pub iss2: u8,
    #[bits(27)]
    _rsvd: u32,
}

/// aarch64 SCTRL_EL1
#[bitfield(u64)]
#[derive(PartialEq, Eq)]
pub struct SctlrEl1 {
    pub m: bool,
    pub a: bool,
    pub c: bool,
    pub sa: bool,
    pub sa0: bool,
    pub cp15ben: bool,
    pub n_aa: bool,
    pub itd: bool,
    pub sed: bool,
    pub uma: bool,
    pub en_rctx: bool,
    pub eos: bool,
    pub i: bool,
    pub en_db: bool,
    pub dze: bool,
    pub uct: bool,
    pub n_twi: bool,
    _mbz0: bool,
    pub n_twe: bool,
    pub wxn: bool,
    pub tscxt: bool,
    pub iesb: bool,
    pub eis: bool,
    pub span: bool,
    pub e0e: bool,
    pub ee: bool,
    pub uci: bool,
    pub en_da: bool,
    pub n_tlsmd: bool,
    pub lsmaoe: bool,
    pub en_ib: bool,
    pub en_ia: bool,
    pub cmow: bool,
    pub msc_en: bool,
    _mbz1: bool,
    pub bt0: bool,
    pub bt1: bool,
    pub itfsb: bool,
    #[bits(2)]
    pub tcf0: u64,
    #[bits(2)]
    pub tcf: u64,
    pub ata0: bool,
    pub ata: bool,
    pub dssbs: bool,
    pub twed_en: bool,
    #[bits(4)]
    pub twedel: u64,
    pub tmt0: bool,
    pub tmt: bool,
    pub tme0: bool,
    pub tme: bool,
    pub en_asr: bool,
    pub en_as0: bool,
    pub en_als: bool,
    pub epan: bool,
    pub tcso0: bool,
    pub tcso: bool,
    pub en_tp2: bool,
    pub nmi: bool,
    pub spintmask: bool,
    pub tidcp: bool,
}

open_enum! {
    pub enum ExceptionClass: u8 {
        UNKNOWN = 0b000000,
        WFI = 0b000001,
        MCR_MRC_COPROC_15 = 0b000011,
        MCRR_MRRC_COPROC_15 = 0b000100,
        MCR_MRC_COPROC_14 = 0b000101,
        LDC_STC = 0b000110,
        FP_OR_SIMD = 0b000111,
        VMRS = 0b001000,
        POINTER_AUTH_HCR_OR_SCR = 0b001001,
        LS64 = 0b001010,
        MRRC_COPROC_14 = 0b001100,
        BRANCH_TARGET = 0b001101,
        ILLEGAL_STATE = 0b001110,
        SVC32 = 0b010001,
        HVC32 = 0b010010,
        SMC32 = 0b010011,
        SVC = 0b010101,
        HVC = 0b010110,
        SMC = 0b010111,
        SYSTEM = 0b011000,
        SVE = 0b011001,
        ERET = 0b011010,
        TSTART = 0b011011,
        POINTER_AUTH = 0b011100,
        SME = 0b011101,
        INSTRUCTION_ABORT_LOWER = 0b100000,
        INSTRUCTION_ABORT = 0b100001,
        PC_ALIGNMENT = 0b100010,
        DATA_ABORT_LOWER = 0b100100,
        DATA_ABORT = 0b100101,
        SP_ALIGNMENT_FAULT = 0b100110,
        MEMORY_OP = 0b100111,
        FP_EXCEPTION_32 = 0b101000,
        FP_EXCEPTION_64 = 0b101100,
        SERROR = 0b101111,
        BREAKPOINT_LOWER = 0b110000,
        BREAKPOINT = 0b110001,
        STEP_LOWER = 0b110010,
        STEP = 0b110011,
        WATCHPOINT_LOWER = 0b110100,
        WATCHPOINT = 0b110101,
        BRK32 = 0b111000,
        VECTOR_CATCH_32 = 0b111010,
        BRK = 0b111100,
    }
}

#[bitfield(u32)]
pub struct IssDataAbort {
    #[bits(6)]
    pub dfsc: FaultStatusCode,
    // Write operation (write not read)
    pub wnr: bool,
    pub s1ptw: bool,
    pub cm: bool,
    pub ea: bool,
    /// FAR not valid
    pub fnv: bool,
    #[bits(2)]
    pub set: u8,
    pub vncr: bool,
    /// Acquire/release
    pub ar: bool,
    /// (ISV==1) 64-bit, (ISV==0) FAR is approximate
    pub sf: bool,
    #[bits(5)]
    /// Register index.
    pub srt: u8,
    /// Sign extended.
    pub sse: bool,
    #[bits(2)]
    /// access width log2
    pub sas: u8,
    /// Valid ESREL2 iss field.
    pub isv: bool,
    #[bits(7)]
    _unused: u8,
}

impl From<IssDataAbort> for EsrEl2 {
    fn from(abort_code: IssDataAbort) -> Self {
        let val: u32 = abort_code.into();
        EsrEl2::new()
            .with_ec(ExceptionClass::DATA_ABORT.0)
            .with_iss(val & 0x07ffffff)
            .with_iss2((val >> 27) as u8)
    }
}

open_enum! {
    pub enum FaultStatusCode: u8 {
        ADDRESS_SIZE_FAULT_LEVEL0 = 0b000000,
        ADDRESS_SIZE_FAULT_LEVEL1 = 0b000001,
        ADDRESS_SIZE_FAULT_LEVEL2 = 0b000010,
        ADDRESS_SIZE_FAULT_LEVEL3 = 0b000011,
        TRANSLATION_FAULT_LEVEL0 = 0b000100,
        TRANSLATION_FAULT_LEVEL1 = 0b000101,
        TRANSLATION_FAULT_LEVEL2 = 0b000110,
        TRANSLATION_FAULT_LEVEL3 = 0b000111,
        ACCESS_FLAG_FAULT_LEVEL0 = 0b001000,
        ACCESS_FLAG_FAULT_LEVEL1 = 0b001001,
        ACCESS_FLAG_FAULT_LEVEL2 = 0b001010,
        ACCESS_FLAG_FAULT_LEVEL3 = 0b001011,
        PERMISSION_FAULT_LEVEL0 = 0b001100,
        PERMISSION_FAULT_LEVEL1 = 0b001101,
        PERMISSION_FAULT_LEVEL2 = 0b001110,
        PERMISSION_FAULT_LEVEL3 = 0b001111,
        SYNCHRONOUS_EXTERNAL_ABORT = 0b010000,
        SYNC_TAG_CHECK_FAULT = 0b010001,
        SEA_TTW_LEVEL_NEG1 = 0b010011,
        SEA_TTW_LEVEL0 = 0b010100,
        SEA_TTW_LEVEL1 = 0b010101,
        SEA_TTW_LEVEL2 = 0b010110,
        SEA_TTW_LEVEL3 = 0b010111,
        ECC_PARITY = 0b011000,
        ECC_PARITY_TTW_LEVEL_NEG1 = 0b011011,
        ECC_PARITY_TTW_LEVEL0 = 0b011100,
        ECC_PARITY_TTW_LEVEL1 = 0b011101,
        ECC_PARITY_TTW_LEVEL2 = 0b011110,
        ECC_PARITY_TTW_LEVEL3 = 0b011111,
        /// Valid only for data fault.
        ALIGNMENT_FAULT = 0b100001,
        /// Valid only for instruction fault.
        GRANULE_PROTECTION_FAULT_LEVEL_NEG = 0b100011,
        /// Valid only for instruction fault.
        GRANULE_PROTECTION_FAULT_LEVEL0 = 0b100100,
        /// Valid only for instruction fault.
        GRANULE_PROTECTION_FAULT_LEVEL1 = 0b100101,
        /// Valid only for instruction fault.
        GRANULE_PROTECTION_FAULT_LEVEL2 = 0b100110,
        /// Valid only for instruction fault.
        GRANULE_PROTECTION_FAULT_LEVE3 = 0b100111,
        ADDRESS_SIZE_FAULT_LEVEL_NEG1 = 0b101001,
        TRANSLATION_FAULT_LEVEL_NEG1 = 0b101011,
        TLB_CONFLICT_ABORT = 0b110000,
        UNSUPPORTED_HW_UPDATE_FAULT = 0b110001,
    }
}

/// Support for embedding within IssDataAbort/IssInstructionAbort
impl FaultStatusCode {
    const fn from_bits(bits: u32) -> Self {
        FaultStatusCode((bits & 0x3f) as u8)
    }

    const fn into_bits(self) -> u32 {
        self.0 as u32
    }
}

#[bitfield(u32)]
pub struct IssInstructionAbort {
    #[bits(6)]
    pub ifsc: FaultStatusCode,
    #[bits(1)]
    _rsvd: u8,
    /// Stage 2 translation fault
    pub s1ptw: bool,
    #[bits(1)]
    _rsvd2: u8,
    /// External abort
    pub ea: bool,
    /// FAR not valid
    pub fnv: bool,
    #[bits(2)]
    pub set: SynchronousErrorType,
    #[bits(11)]
    _rsvd3: u16,
    #[bits(8)]
    _unused: u8,
}

impl From<IssInstructionAbort> for EsrEl2 {
    fn from(instruction_code: IssInstructionAbort) -> Self {
        let val: u32 = instruction_code.into();
        EsrEl2::new()
            .with_ec(ExceptionClass::INSTRUCTION_ABORT.0)
            .with_iss(val & 0x07ffffff)
            .with_iss2((val >> 27) as u8)
    }
}

open_enum! {
    pub enum SynchronousErrorType: u8 {
        RECOVERABLE = 0,
        UNCONTAINABLE = 2,
        RESTARTABLE = 3,
    }
}

/// Support for embedding within IssInstructionAbort
impl SynchronousErrorType {
    const fn from_bits(bits: u32) -> Self {
        SynchronousErrorType((bits & 0x1800) as u8)
    }

    const fn into_bits(self) -> u32 {
        (self.0 as u32) << 11
    }
}

#[bitfield(u32)]
pub struct IssSystem {
    pub direction: bool,
    #[bits(4)]
    pub crm: u8,
    #[bits(5)]
    pub rt: u8,
    #[bits(4)]
    pub crn: u8,
    #[bits(3)]
    pub op1: u8,
    #[bits(3)]
    pub op2: u8,
    #[bits(2)]
    pub op0: u8,
    #[bits(10)]
    _unused: u32,
}

impl IssSystem {
    pub const fn system_reg(&self) -> SystemReg {
        SystemReg(
            SystemRegEncoding::new()
                .with_op0(self.op0())
                .with_op1(self.op1())
                .with_crn(self.crn())
                .with_crm(self.crm())
                .with_op2(self.op2()),
        )
    }
}

#[bitfield(u32)]
#[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SystemRegEncoding {
    #[bits(5)]
    _rsvd: u32,
    #[bits(3)]
    pub op2: u8,
    #[bits(4)]
    pub crm: u8,
    #[bits(4)]
    pub crn: u8,
    #[bits(3)]
    pub op1: u8,
    #[bits(2)]
    pub op0: u8,
    #[bits(11)]
    _rsvd2: u32,
}

open_enum! {
    pub enum SystemReg: SystemRegEncoding {
        SPSR_EL1 = SystemRegEncoding::make(3, 0, 4, 0, 0),
        SPSR_EL2 = SystemRegEncoding::make(3, 4, 4, 0, 0),
        SPSR_EL3 = SystemRegEncoding::make(3, 6, 4, 0, 0),
        ELR_EL1 = SystemRegEncoding::make(3, 0, 4, 0, 1),
        ELR_EL2 = SystemRegEncoding::make(3, 4, 4, 0, 1),
        ELR_EL3 = SystemRegEncoding::make(3, 6, 4, 0, 1),
        SP_EL0 = SystemRegEncoding::make(3, 0, 4, 1, 0),
        SP_EL1 = SystemRegEncoding::make(3, 4, 4, 1, 0),
        SP_EL2 = SystemRegEncoding::make(3, 6, 4, 1, 0),
        FPSR = SystemRegEncoding::make(3, 3, 4, 4, 1),
        FPCR = SystemRegEncoding::make(3, 3, 4, 4, 0),
        SPSR_ABT = SystemRegEncoding::make(3, 4, 4, 3, 1),
        IFSR32_EL2 = SystemRegEncoding::make(3, 4, 5, 0, 1),

        VPIDR_EL2 = SystemRegEncoding::make(3, 4, 0, 0, 0),
        ARM64_REVIDR_EL1 = SystemRegEncoding::make(3, 0, 0, 0, 6),
        CTR_EL0 = SystemRegEncoding::make(3, 3, 0, 0, 1),
        ARM64_VMPIDR_EL2 = SystemRegEncoding::make(3, 4, 0, 0, 5),
        ID_AA64PFR1_EL1 = SystemRegEncoding::make(3, 0, 0, 4, 1),
        ID_AA64DFR0_EL1 = SystemRegEncoding::make(3, 0, 0, 5, 0),
        ID_AA64DFR1_EL1 = SystemRegEncoding::make(3, 0, 0, 5, 1),
        ID_AA64AFR0_EL1 = SystemRegEncoding::make(3, 0, 0, 5, 4),
        ID_AA64AFR1_EL1 = SystemRegEncoding::make(3, 0, 0, 5, 5),
        ID_AA64ISAR0_EL1 = SystemRegEncoding::make(3, 0, 0, 6, 0),
        ID_AA64ISAR1_EL1 = SystemRegEncoding::make(3, 0, 0, 6, 1),
        ID_AA64MMFR0_EL1 = SystemRegEncoding::make(3, 0, 0, 7, 0),
        ID_AA64MMFR1_EL1 = SystemRegEncoding::make(3, 0, 0, 7, 1),
        ID_AA64MMFR2_EL1 = SystemRegEncoding::make(3, 0, 0, 7, 2),

        ID_MMFR0 = SystemRegEncoding::make(3, 0, 0, 1, 4),
        ID_MMFR1 = SystemRegEncoding::make(3, 0, 0, 1, 5),
        ID_MMFR2 = SystemRegEncoding::make(3, 0, 0, 1, 6),
        ID_MMFR3 = SystemRegEncoding::make(3, 0, 0, 1, 7),
        ID_MMFR4 = SystemRegEncoding::make(3, 0, 0, 2, 6),
        ID_ISAR0 = SystemRegEncoding::make(3, 0, 0, 2, 0),
        ID_ISAR1 = SystemRegEncoding::make(3, 0, 0, 2, 1),
        ID_ISAR2 = SystemRegEncoding::make(3, 0, 0, 2, 2),
        ID_ISAR3 = SystemRegEncoding::make(3, 0, 0, 2, 3),
        ID_ISAR4 = SystemRegEncoding::make(3, 0, 0, 2, 4),
        ID_ISAR5 = SystemRegEncoding::make(3, 0, 0, 2, 5),
        ID_ISAR6 = SystemRegEncoding::make(3, 0, 0, 2, 7),
        MVFR0_EL1 = SystemRegEncoding::make(3, 0, 0, 3, 0),
        MVFR1_EL1 = SystemRegEncoding::make(3, 3, 0, 0, 1),
        MVFR2_EL1 = SystemRegEncoding::make(3, 3, 0, 0, 2),
        ID_AA64ZFR0_EL1 = SystemRegEncoding::make(3, 0, 0, 4, 4),
        DACR32_EL2 = SystemRegEncoding::make(3, 4, 3, 0, 0),
        FPEXC32_EL2 = SystemRegEncoding::make(3, 4, 5, 3, 0),
        VMPIDR_EL2 = SystemRegEncoding::make(3, 4, 0, 0, 5),

        SCTLR = SystemRegEncoding::make(3, 0, 1, 0, 0),
        SCTLR_EL2 = SystemRegEncoding::make(3, 4, 1, 0, 0),
        HCR_EL2 = SystemRegEncoding::make(3, 4, 1, 1, 0),
        HSTR_EL2 = SystemRegEncoding::make(3, 4, 1, 1, 3),
        HACR_EL2 = SystemRegEncoding::make(3, 4, 1, 1, 7),
        ACTLR_EL1 = SystemRegEncoding::make(3, 0, 1, 0, 1),
        ACTLR_EL2 = SystemRegEncoding::make(3, 4, 1, 0, 1),
        CPACR = SystemRegEncoding::make(3, 0, 1, 0, 2),
        CPTR_EL2 = SystemRegEncoding::make(3, 4, 1, 1, 2),
        CPUECTLR_EL1 = SystemRegEncoding::make(3, 0, 15, 1, 4),
        CNTPS_CTL_EL1 = SystemRegEncoding::make(3, 7, 14, 2, 1),
        CPUMERRSR_EL1 = SystemRegEncoding::make(3, 1, 15, 2, 2),
        CNTPS_CVAL_EL1 = SystemRegEncoding::make(3, 7, 14, 2, 2),
        L2MERRSR_EL1 = SystemRegEncoding::make(3, 1, 15, 2, 3),

        TTBR0_EL1 = SystemRegEncoding::make(3, 0, 2, 0, 0),
        TTBR0_EL2 = SystemRegEncoding::make(3, 4, 2, 0, 0),
        TTBR1_EL1 = SystemRegEncoding::make(3, 0, 2, 0, 1),
        VTTBR_EL2 = SystemRegEncoding::make(3, 4, 2, 1, 0),
        TCR_EL1 = SystemRegEncoding::make(3, 0, 2, 0, 2),
        TCR_EL2 = SystemRegEncoding::make(3, 4, 2, 0, 2),
        VTCR_EL2 = SystemRegEncoding::make(3, 4, 2, 1, 2),
        ESR_EL1 = SystemRegEncoding::make(3, 0, 5, 2, 0),
        ESR_EL2 = SystemRegEncoding::make(3, 4, 5, 2, 0),
        ESR_EL3 = SystemRegEncoding::make(3, 6, 5, 2, 0),
        FAR_EL1 = SystemRegEncoding::make(3, 0, 6, 0, 0),
        FAR_EL2 = SystemRegEncoding::make(3, 4, 6, 0, 0),
        HPFAR_EL2 = SystemRegEncoding::make(3, 4, 6, 0, 4),
        AFSR0_EL1 = SystemRegEncoding::make(3, 0, 5, 1, 0),
        AFSR0_EL2 = SystemRegEncoding::make(3, 4, 5, 1, 0),
        AFSR1_EL1 = SystemRegEncoding::make(3, 0, 5, 1, 1),
        AFSR1_EL2 = SystemRegEncoding::make(3, 4, 5, 1, 1),

        PAR_EL1 = SystemRegEncoding::make(3, 0, 7, 4, 0),
        CNTFRQ_EL0 = SystemRegEncoding::make(3, 3, 14, 0, 0),
        CNTP_CTL_EL0 = SystemRegEncoding::make(3, 3, 14, 2, 1),
        CNTP_CVAL_EL0 = SystemRegEncoding::make(3, 3, 14, 2, 2),
        CNTV_CTL_EL0 = SystemRegEncoding::make(3, 3, 14, 3, 1),
        CNTV_CVAL_EL0 = SystemRegEncoding::make(3, 3, 14, 3, 2),
        CNTHCTL_EL2 = SystemRegEncoding::make(3, 4, 14, 1, 0),
        CNTHP_CTL_EL2 = SystemRegEncoding::make(3, 4, 14, 2, 1),
        CNTHP_CVAL_EL2 = SystemRegEncoding::make(3, 4, 14, 2, 2),
        PMCCFILTR_EL0 = SystemRegEncoding::make(3, 3, 14, 15, 7),
        MDCR_EL2 = SystemRegEncoding::make(3, 4, 1, 1, 1),
        PMCR_EL0 = SystemRegEncoding::make(3, 3, 9, 12, 0),
        PMCNTENSET_EL0 = SystemRegEncoding::make(3, 3, 9, 12, 1),
        PMCNTENCLR_EL0 = SystemRegEncoding::make(3, 3, 9, 12, 2),
        PMOVSSET_EL0 = SystemRegEncoding::make(3, 3, 9, 14, 3),
        PMOVSCLR_EL0 = SystemRegEncoding::make(3, 3, 9, 12, 3),
        PMSELR_EL0 = SystemRegEncoding::make(3, 3, 9, 12, 5),
        PMCEID0_EL0 = SystemRegEncoding::make(3, 3, 9, 12, 6),
        PMCEID1_EL0 = SystemRegEncoding::make(3, 3, 9, 12, 7),
        PMCCNTR_EL0 = SystemRegEncoding::make(3, 3, 9, 13, 0),
        PMUSERENR_EL0 = SystemRegEncoding::make(3, 3, 9, 14, 0),
        PMINTENSET_EL1 = SystemRegEncoding::make(3, 0, 9, 14, 1),
        PMINTENCLR_EL1 = SystemRegEncoding::make(3, 0, 9, 14, 2),

        MAIR_EL1 = SystemRegEncoding::make(3, 0, 10, 2, 0),
        AMAIR0 = SystemRegEncoding::make(3, 0, 10, 3, 0),
        MAIR_EL2 = SystemRegEncoding::make(3, 4, 10, 2, 0),
        AMAIR_EL2 = SystemRegEncoding::make(3, 4, 10, 3, 0),
        MAIR_EL3 = SystemRegEncoding::make(3, 6, 10, 2, 0),

        VBAR = SystemRegEncoding::make(3, 0, 12, 0, 0),
        VBAR_EL2 = SystemRegEncoding::make(3, 4, 12, 0, 0),
        RVBAR_EL2 = SystemRegEncoding::make(3, 4, 12, 0, 1),

        TPIDR_EL0 = SystemRegEncoding::make(3, 3, 13, 0, 2),
        TPIDRRO_EL0 = SystemRegEncoding::make(3, 3, 13, 0, 3),
        TPIDR_EL1 = SystemRegEncoding::make(3, 0, 13, 0, 4),
        TPIDR_EL2 = SystemRegEncoding::make(3, 4, 13, 0, 2),
        CONTEXTIDR_EL1 = SystemRegEncoding::make(3, 0, 13, 0, 1),

        CLIDR = SystemRegEncoding::make(3, 1, 0, 0, 1),
        AIDR = SystemRegEncoding::make(3, 1, 0, 0, 7),
        CSSELR = SystemRegEncoding::make(3, 2, 0, 0, 0),

        CNTKCTL = SystemRegEncoding::make(3, 0, 14, 1, 0),
        CNTVOFF_EL2 = SystemRegEncoding::make(3, 4, 14, 0, 3),

        MDCCSR_EL0 = SystemRegEncoding::make(2, 3, 0, 1, 0),
        MDSCR_EL1 = SystemRegEncoding::make(2, 0, 0, 2, 2),
        MDRAR_EL1 = SystemRegEncoding::make(2, 0, 1, 0, 0),
        OSLSR_EL1 = SystemRegEncoding::make(2, 0, 1, 1, 4),
        DBGBVR0 = SystemRegEncoding::make(2, 0, 0, 0, 4),
        DBGBVR1 = SystemRegEncoding::make(2, 0, 0, 1, 4),
        DBGBVR2 = SystemRegEncoding::make(2, 0, 0, 2, 4),
        DBGBVR3 = SystemRegEncoding::make(2, 0, 0, 3, 4),
        DBGBVR4 = SystemRegEncoding::make(2, 0, 0, 4, 4),
        DBGBVR5 = SystemRegEncoding::make(2, 0, 0, 5, 4),
        DBGBCR0 = SystemRegEncoding::make(2, 0, 0, 0, 5),
        DBGBCR1 = SystemRegEncoding::make(2, 0, 0, 1, 5),
        DBGBCR2 = SystemRegEncoding::make(2, 0, 0, 2, 5),
        DBGBCR3 = SystemRegEncoding::make(2, 0, 0, 3, 5),
        DBGBCR4 = SystemRegEncoding::make(2, 0, 0, 4, 5),
        DBGBCR5 = SystemRegEncoding::make(2, 0, 0, 5, 5),
        DBGWVR0 = SystemRegEncoding::make(2, 0, 0, 0, 6),
        DBGWVR1 = SystemRegEncoding::make(2, 0, 0, 1, 6),
        DBGWVR2 = SystemRegEncoding::make(2, 0, 0, 2, 6),
        DBGWVR3 = SystemRegEncoding::make(2, 0, 0, 3, 6),
        DBGWCR0 = SystemRegEncoding::make(2, 0, 0, 0, 7),
        DBGWCR1 = SystemRegEncoding::make(2, 0, 0, 1, 7),
        DBGWCR2 = SystemRegEncoding::make(2, 0, 0, 2, 7),
        DBGWCR3 = SystemRegEncoding::make(2, 0, 0, 3, 7),

        ICC_AP0R0_EL1 = SystemRegEncoding::make(3, 0, 12, 8, 4),
        ICC_AP0R1_EL1 = SystemRegEncoding::make(3, 0, 12, 8, 5),
        ICC_AP0R2_EL1 = SystemRegEncoding::make(3, 0, 12, 8, 6),
        ICC_AP0R3_EL1 = SystemRegEncoding::make(3, 0, 12, 8, 7),
        ICC_AP1R0_EL1 = SystemRegEncoding::make(3, 0, 12, 9, 0),
        ICC_AP1R1_EL1 = SystemRegEncoding::make(3, 0, 12, 9, 1),
        ICC_AP1R2_EL1 = SystemRegEncoding::make(3, 0, 12, 9, 2),
        ICC_AP1R3_EL1 = SystemRegEncoding::make(3, 0, 12, 9, 3),
        ICC_ASGI1R_EL1 = SystemRegEncoding::make(3, 0, 12, 11, 6),
        ICC_BPR0_EL1 = SystemRegEncoding::make(3, 0, 12, 8, 3),
        ICC_BPR1_EL1 = SystemRegEncoding::make(3, 0, 12, 12, 3),
        ICC_CTLR_EL1 = SystemRegEncoding::make(3, 0, 12, 12, 4),
        ICC_CTLR_EL3 = SystemRegEncoding::make(3, 6, 12, 12, 4),
        ICC_DIR_EL1 = SystemRegEncoding::make(3, 0, 12, 11, 1),
        ICC_EOIR0_EL1 = SystemRegEncoding::make(3, 0, 12, 8, 1),
        ICC_EOIR1_EL1 = SystemRegEncoding::make(3, 0, 12, 12, 1),
        ICC_HPPIR0_EL1 = SystemRegEncoding::make(3, 0, 12, 8, 2),
        ICC_HPPIR1_EL1 = SystemRegEncoding::make(3, 0, 12, 12, 2),
        ICC_IAR0_EL1 = SystemRegEncoding::make(3, 0, 12, 8, 0),
        ICC_IAR1_EL1 = SystemRegEncoding::make(3, 0, 12, 12, 0),
        ICC_IGRPEN0_EL1 = SystemRegEncoding::make(3, 0, 12, 12, 6),
        ICC_IGRPEN1_EL1 = SystemRegEncoding::make(3, 0, 12, 12, 7),
        ICC_IGRPEN1_EL3 = SystemRegEncoding::make(3, 6, 12, 12, 7),
        ICC_PMR_EL1 = SystemRegEncoding::make(3, 0, 4, 6, 0),
        ICC_RPR_EL1 = SystemRegEncoding::make(3, 0, 12, 11, 3),
        ICC_SGI0R_EL1 = SystemRegEncoding::make(3, 0, 12, 11, 7),
        ICC_SGI1R_EL1 = SystemRegEncoding::make(3, 0, 12, 11, 5),
        ICC_SRE_EL1 = SystemRegEncoding::make(3, 0, 12, 12, 5),
        ICC_SRE_EL2 = SystemRegEncoding::make(3, 4, 12, 9, 5),
        ICC_SRE_EL3 = SystemRegEncoding::make(3, 6, 12, 12, 5),
    }
}

impl SystemRegEncoding {
    pub const fn make(op0: u8, op1: u8, crn: u8, crm: u8, op2: u8) -> Self {
        Self::new()
            .with_op0(op0)
            .with_op1(op1)
            .with_crn(crn)
            .with_crm(crm)
            .with_op2(op2)
    }
}

/// MPIDR_EL1
#[bitfield(u64)]
pub struct MpidrEl1 {
    pub aff0: u8,
    pub aff1: u8,
    pub aff2: u8,
    pub mt: bool,
    #[bits(5)]
    pub res0_25_29: u8,
    pub u: bool,
    pub res1_31: bool,
    pub aff3: u8,
    #[bits(24)]
    pub res0_40_63: u32,
}

impl MpidrEl1 {
    pub const AFFINITY_MASK: Self = Self::new()
        .with_aff0(0xff)
        .with_aff1(0xff)
        .with_aff2(0xff)
        .with_aff3(0xff);
}

open_enum! {
    /// aarch64 translation granule size for TTBR0_EL1
    pub enum TranslationGranule0: u64 {
        TG_4KB = 0b00,
        TG_64KB = 0b01,
        TG_16KB = 0b10,
    }
}

impl TranslationGranule0 {
    const fn into_bits(self) -> u64 {
        self.0
    }

    const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }
}

open_enum! {
    /// aarch64 translation granule size for TTBR1_EL1
    pub enum TranslationGranule1: u64 {
        TG_INVALID = 0b00,
        TG_16KB = 0b01,
        TG_4KB = 0b10,
        TG_64KB = 0b11,
    }
}

impl TranslationGranule1 {
    const fn into_bits(self) -> u64 {
        self.0
    }

    const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }
}

open_enum! {
    /// aarch64 intermediate physical address size
    pub enum IntermPhysAddrSize: u64{
        IPA_32_BITS_4_GB = 0b000,
        IPA_36_BITS_64_GB = 0b001,
        IPA_40_BITS_1_TB = 0b010,
        IPA_42_BITS_4_TB = 0b011,
        IPA_44_BITS_16_TB = 0b100,
        IPA_48_BITS_256_TB = 0b101,
        IPA_52_BITS_4_PB = 0b110,
        IPA_56_BITS_64_PB = 0b111,
    }
}

impl IntermPhysAddrSize {
    const fn into_bits(self) -> u64 {
        self.0
    }

    const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }
}

/// aarch64 TCR_EL1 register
#[bitfield(u64)]
#[derive(PartialEq, Eq)]
pub struct TranslationControlEl1 {
    #[bits(6)]
    pub t0sz: u64,
    #[bits(1)]
    _mbz0: u64,
    #[bits(1)]
    pub epd0: u64,
    #[bits(2)]
    pub irgn0: u64,
    #[bits(2)]
    pub orgn0: u64,
    #[bits(2)]
    pub sh0: u64,
    #[bits(2)]
    pub tg0: TranslationGranule0,
    #[bits(6)]
    pub t1sz: u64,
    #[bits(1)]
    pub a1: u64,
    #[bits(1)]
    pub epd1: u64,
    #[bits(2)]
    pub irgn1: u64,
    #[bits(2)]
    pub orgn1: u64,
    #[bits(2)]
    pub sh1: u64,
    #[bits(2)]
    pub tg1: TranslationGranule1,
    #[bits(3)]
    pub ips: IntermPhysAddrSize,
    #[bits(1)]
    _mbz1: u64,
    #[bits(1)]
    pub a_s: u64,
    #[bits(1)]
    pub tbi0: u64,
    #[bits(1)]
    pub tbi1: u64,
    #[bits(1)]
    pub ha: u64,
    #[bits(1)]
    pub hd: u64,
    #[bits(1)]
    pub hpd0: u64,
    #[bits(1)]
    pub hpd1: u64,
    #[bits(1)]
    pub hwu059: u64,
    #[bits(1)]
    pub hwu060: u64,
    #[bits(1)]
    pub hwu061: u64,
    #[bits(1)]
    pub hwu062: u64,
    #[bits(1)]
    pub hwu159: u64,
    #[bits(1)]
    pub hwu160: u64,
    #[bits(1)]
    pub hwu161: u64,
    #[bits(1)]
    pub hwu162: u64,
    #[bits(1)]
    pub tbid0: u64,
    #[bits(1)]
    pub tbid1: u64,
    #[bits(1)]
    pub nfd0: u64,
    #[bits(1)]
    pub nfd1: u64,
    #[bits(1)]
    pub e0pd0: u64,
    #[bits(1)]
    pub e0pd1: u64,
    #[bits(1)]
    pub tcma0: u64,
    #[bits(1)]
    pub tcma1: u64,
    #[bits(1)]
    pub ds: u64,
    #[bits(1)]
    pub mtx0: u64,
    #[bits(1)]
    pub mtx1: u64,
    #[bits(2)]
    _mbz2: u64,
}

impl TranslationControlEl1 {
    pub fn ttbr0_valid_address_bits(&self) -> u64 {
        64 - self.t0sz()
    }

    pub fn ttbr1_valid_address_bits(&self) -> u64 {
        64 - self.t1sz()
    }
}

/// aarch64 TTBRx_EL1 content
#[bitfield(u64)]
#[derive(PartialEq, Eq)]
pub struct TranslationBaseEl1 {
    // Hardcoding CnP to be `0` for simplicity.
    // #[bits(1)]
    // pub cnp: u64,
    #[bits(48)]
    pub baddr: u64,
    #[bits(16)]
    pub asid: u64,
}

#[bitfield(u64)]
#[derive(PartialEq, Eq, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Pte {
    pub valid: bool,
    pub not_large_page: bool,
    #[bits(3)]
    pub attribute_index: u64,
    pub non_secure: bool,
    pub ap_unprivileged: bool,
    pub ap_read_only: bool,
    #[bits(2)]
    pub shareability: u64,
    pub access_flag: bool,
    pub not_global: bool,
    #[bits(36)]
    pub pfn: u64,
    #[bits(3)]
    pub reserved_must_be_zero: u64,
    pub dbm: bool,
    pub contiguous_hint: bool,
    pub privilege_no_execute: bool,
    pub user_no_execute: bool,
    #[bits(4)]
    pub _reserved2: u64,
    pub pxn_table: bool,
    pub uxn_table: bool,
    pub ap_table_privileged_only: bool,
    pub ap_table_read_only: bool,
    pub ns_table: bool,
}

/// The contents of ID_MMFR0_EL1
#[bitfield(u64)]
pub struct MmFeatures0El1 {
    #[bits(4)]
    pub pa_range: IntermPhysAddrSize,
    #[bits(60)]
    _rest: u64,
}

pub const GIC_DISTRIBUTOR_SIZE: u64 = 0x1_0000;
pub const GIC_REDISTRIBUTOR_FRAME_SIZE: u64 = 0x1_0000;
pub const GIC_SGI_FRAME_SIZE: u64 = 0x1_0000;
pub const GIC_REDISTRIBUTOR_SIZE: u64 = GIC_REDISTRIBUTOR_FRAME_SIZE + GIC_SGI_FRAME_SIZE;

open_enum! {
    pub enum SystemReset2Code: u32 {
        WARM_RESET = 0,
    }
}

open_enum! {
    pub enum SystemOff2Code: u32 {
        DEFAULT = 0,
        HIBERNATE_OFF = 1,
    }
}
