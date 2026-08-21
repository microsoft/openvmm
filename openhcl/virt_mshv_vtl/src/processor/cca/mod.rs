// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Processor support for CCA Planes.

use super::BackingSharedParams;
use super::HardwareIsolatedBacking;
use super::UhProcessor;
use super::private::BackingPrivate;
use super::vp_state;
use super::vp_state::UhVpStateAccess;
use crate::BackingShared;
use crate::Error;
use crate::TlbFlushLockAccess;
use crate::UhCvmPartitionState;
use crate::UhCvmVpState;
use crate::UhPartitionInner;
use crate::processor::InterceptMessageState;
use aarch64defs::EsrEl2;
use aarch64defs::HpfarEl2;
use aarch64defs::InstructionAbortReason;
use aarch64defs::IssDataAbort;
use aarch64defs::IssInstructionAbort;
use aarch64defs::IssSystem;
use aarch64defs::MpidrEl1;
use aarch64defs::SystemReg;
use aarch64defs::gic::GicrSgi;
use aarch64defs::rsi::cca_rsi_plane_exit;
use hcl::GuestVtl;
use hcl::ioctl::cca::Cca;
use hcl::ioctl::cca::GetIpaStateError;
use hcl::ioctl::register;
use hv1_emulator::hv::ProcessorVtlHv;
use hv1_emulator::synic::ProcessorSynic;
use hv1_structs::VtlArray;
use hvdef::HvRegisterCrInterceptControl;
use inspect::Inspect;
use inspect::InspectMut;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::aarch64::vp;
use virt::aarch64::vp::AccessVpState;
use virt::io::CpuIo;
use virt_support_aarch64emu::translate::TranslationRegisters;
use virt_support_gic::PendingInterrupt;
use zerocopy::FromZeros;

#[derive(Debug, Error)]
#[error("failed to run")]
struct CcaRunVpError(#[source] hcl::ioctl::Error);

#[derive(Debug, Error)]
enum CcaUnsupportedExit {
    #[error("unsupported CCA plane exit reason {0}")]
    ExitReason(u64),
    #[error("unsupported CCA exception class {exception_class:#x} in ESR_EL2 {esr_el2:#x}")]
    ExceptionClass { exception_class: u8, esr_el2: u64 },
    #[error("CCA data abort with invalid instruction syndrome in ESR_EL2 {0:#x}")]
    InvalidDataAbortIss(u64),
    #[error(
        "CCA instruction abort: ESR_EL2 {esr_el2:#x}, ELR_EL2 {elr_el2:#x}, FAR_EL2 {far_el2:#x},
        FIPA {fipa:#x}, FIPA RIPAS state {fipa_state:#x}, IFSC {ifsc:#x}, reason {reason:?}, FNV {far_not_valid}"
    )]
    InstructionAbort {
        esr_el2: u64,
        elr_el2: u64,
        far_el2: u64,
        fipa: u64,
        fipa_state: u8,
        ifsc: u8,
        reason: InstructionAbortReason,
        far_not_valid: bool,
    },
    #[error("CCA private GIC interrupt ID {0} is outside the SGI/PPI range")]
    InvalidPrivateGicInterrupt(u32),
    #[error("no free pending CCA GIC slot for virtual interrupt {0}")]
    NoFreePendingGicInterrupt(u32),
    #[error("unsupported CCA system register trap for {system_reg:?} in ESR_EL2 {esr_el2:#x}")]
    UnsupportedSystemRegister { system_reg: SystemReg, esr_el2: u64 },
    #[error("CCA {system_reg:?} write in ESR_EL2 {esr_el2:#x} has no accessible source register")]
    MissingSystemRegisterValue { system_reg: SystemReg, esr_el2: u64 },
}

const AARCH64_ZERO_REGISTER_INDEX: u8 = 31;
const CNTV_CTL_ENABLE: u64 = 1 << 0;
const CNTV_CTL_IMASK: u64 = 1 << 1;
const CNTV_CTL_ISTATUS: u64 = 1 << 2;

const ICH_HCR_TC: u64 = 1 << 10;

const ICH_LR_VINTID_MASK: u64 = u32::MAX as u64;
const ICH_LR_PRIORITY_SHIFT: u32 = 48;
const ICH_LR_GROUP1: u64 = 1 << 60;
const ICH_LR_PENDING: u64 = 1 << 62;
const ICH_LR_STATE_MASK: u64 = 3 << 62;
const CCA_PENDING_GIC_INTERRUPTS: usize = 16;
const GIC_PRIVATE_INTERRUPT_COUNT: u32 = 32;
const RSI_PLANE_EXIT_INVALID: u64 = u64::MAX;

// For use with Hyper-V synthetic interrupt controller allocated by paravisor.
enum UhDirectOverlay {
    #[expect(unused)]
    Sipp,
    #[expect(unused)]
    Sifp,
    Count,
}

/// Backing for CCA planes.
#[derive(InspectMut)]
pub struct CcaBacked {
    vtls: VtlArray<CcaVtl, 2>,
    cvm: UhCvmVpState,
}

#[derive(Clone, Copy, InspectMut, Inspect)]
struct CcaVtl {
    // TODO: CCA: potentially needed fields, based on TDX implementation:
    // * values of control registers
    // * interrupt information
    // * exception error code
    // * TLB flush state
    // * PMU stats
    sp_el0: u64,
    sp_el1: u64,
    cpsr: u64,
    #[inspect(skip)]
    gic: CcaGic,
}

impl CcaVtl {
    pub(crate) fn new() -> Self {
        Self {
            sp_el0: 0,
            sp_el1: 0,
            cpsr: 0,
            gic: CcaGic::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CcaGicRequestError {
    InvalidIntid,
    NoFreeSlot,
}

#[derive(Clone, Copy)]
struct CcaGic {
    pending: [Option<u32>; CCA_PENDING_GIC_INTERRUPTS],
    priority_mask: u8,
}

impl CcaGic {
    const fn new() -> Self {
        Self {
            pending: [None; CCA_PENDING_GIC_INTERRUPTS],
            priority_mask: u8::MAX,
        }
    }

    fn request_interrupt(&mut self, intid: u32) -> Result<(), CcaGicRequestError> {
        if intid >= GIC_PRIVATE_INTERRUPT_COUNT {
            return Err(CcaGicRequestError::InvalidIntid);
        }

        if self.pending.contains(&Some(intid)) {
            return Ok(());
        }

        let Some(slot) = self.pending.iter_mut().find(|slot| slot.is_none()) else {
            return Err(CcaGicRequestError::NoFreeSlot);
        };
        *slot = Some(intid);
        Ok(())
    }

    fn pending_mask(&self) -> u32 {
        self.pending
            .iter()
            .flatten()
            .fold(0, |pending, intid| pending | 1 << intid)
    }

    fn complete_interrupt(&mut self, intid: u32) {
        if let Some(slot) = self.pending.iter_mut().find(|slot| **slot == Some(intid)) {
            *slot = None;
        }
    }
}

#[derive(Inspect)]
pub struct CcaBackedShared {
    pub(crate) cvm: UhCvmPartitionState,
    virt_timer_ppi: u32,
}

impl CcaBackedShared {
    pub(crate) fn new(params: BackingSharedParams<'_>, virt_timer_ppi: u32) -> Result<Self, Error> {
        Ok(Self {
            cvm: params.cvm_state.unwrap(),
            virt_timer_ppi,
        })
    }
}

/// Types of exceptions that can occur in the CCA plane,
/// and get reported back to use from the RMM.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
enum ExceptionClass {
    DataAbort,
    InstructionAbort,
    SimdAccess,
    SmcError,
    SystemRegister,
    Unknown(u8),
}

impl From<u8> for ExceptionClass {
    fn from(value: u8) -> Self {
        match value {
            0b0010_0100 => ExceptionClass::DataAbort,
            0b0010_0000 => ExceptionClass::InstructionAbort,
            0b0000_0111 => ExceptionClass::SimdAccess,
            0b0001_0111 => ExceptionClass::SmcError,
            0b0001_1000 => ExceptionClass::SystemRegister,
            _ => ExceptionClass::Unknown(value),
        }
    }
}

/// The reason for a CCA plane exit, which can be either a synchronous event
/// (like an MMIO access or an exception) or an IRQ.
#[derive(Debug, Clone, Copy)]
enum PlaneExitReason {
    Sync,
    Irq,
    Unknown(u64),
}

impl From<u64> for PlaneExitReason {
    fn from(value: u64) -> Self {
        match value {
            0 => PlaneExitReason::Sync,
            1 => PlaneExitReason::Irq,
            _ => PlaneExitReason::Unknown(value),
        }
    }
}

struct CcaLocalInterruptExit {
    exception_class: ExceptionClass,
    esr_el2: EsrEl2,
    source_value: Option<u64>,
    exit_esr_el2: u64,
    virtual_timer_asserted: bool,
}

/// A wrapper around the CCA RSI plane exit structure, providing methods to
/// access information regarding the exit of the plane.
struct CcaExit<'a>(&'a cca_rsi_plane_exit);

impl<'a> CcaExit<'a> {
    fn exit_reason(&self) -> PlaneExitReason {
        self.0.exit_reason.into()
    }

    fn esr_el2(&self) -> EsrEl2 {
        self.0.esr_el2.into()
    }

    fn esr_el2_class(&self) -> ExceptionClass {
        ExceptionClass::from(EsrEl2::from_bits(self.0.esr_el2).ec())
    }

    fn far_el2(&self) -> u64 {
        self.0.far_el2
    }

    fn hpfar_el2(&self) -> HpfarEl2 {
        self.0.hpfar_el2.into()
    }

    fn elr_el2(&self) -> u64 {
        self.0.elr_el2
    }

    fn gpr_or_zero_register(&self, index: u8) -> Option<u64> {
        match index {
            AARCH64_ZERO_REGISTER_INDEX => Some(0),
            index => self.0.gprs.get(usize::from(index)).copied(),
        }
    }

    fn virtual_timer_asserted(&self) -> bool {
        self.0.cntv_ctl_el0 & (CNTV_CTL_ENABLE | CNTV_CTL_IMASK | CNTV_CTL_ISTATUS)
            == CNTV_CTL_ENABLE | CNTV_CTL_ISTATUS
    }

    fn local_interrupt_exit(&self) -> CcaLocalInterruptExit {
        let esr_el2 = self.esr_el2();
        let exception_class = self.esr_el2_class();
        let source_value = if matches!(exception_class, ExceptionClass::SystemRegister) {
            let iss = IssSystem::from(esr_el2.iss());
            self.gpr_or_zero_register(iss.rt())
        } else {
            None
        };

        CcaLocalInterruptExit {
            exception_class,
            esr_el2,
            source_value,
            exit_esr_el2: self.0.esr_el2,
            virtual_timer_asserted: self.virtual_timer_asserted(),
        }
    }
}

fn inject_virtual_interrupt(lrs: &mut [u64], interrupt: PendingInterrupt) -> bool {
    if virtual_interrupt_is_listed(lrs, interrupt.intid) {
        return true;
    }

    let Some(lr) = lrs.iter_mut().find(|lr| **lr & ICH_LR_STATE_MASK == 0) else {
        return false;
    };

    *lr = u64::from(interrupt.intid)
        | (u64::from(interrupt.priority) << ICH_LR_PRIORITY_SHIFT)
        | if interrupt.group1 { ICH_LR_GROUP1 } else { 0 }
        | ICH_LR_PENDING;
    true
}

fn virtual_interrupt_is_listed(lrs: &[u64], intid: u32) -> bool {
    lrs.iter()
        .any(|lr| *lr & ICH_LR_STATE_MASK != 0 && *lr & ICH_LR_VINTID_MASK == u64::from(intid))
}

fn sgi_targets_current_vp(sgi: GicrSgi, mpidr: MpidrEl1) -> bool {
    if sgi.irm()
        || sgi.aff3() != mpidr.aff3()
        || sgi.aff2() != mpidr.aff2()
        || sgi.aff1() != mpidr.aff1()
        || sgi.rs() != mpidr.aff0() / 16
    {
        return false;
    }

    sgi.target_list() & (1 << (mpidr.aff0() % 16)) != 0
}

fn extend_mmio_read(data: [u8; size_of::<u64>()], len: usize, sign_extend: bool, sf: bool) -> u64 {
    let value = u64::from_ne_bytes(data);
    if sign_extend {
        let shift = 64 - len * 8;
        let value = ((value as i64) << shift >> shift) as u64;
        if sf {
            value
        } else {
            value & u64::from(u32::MAX)
        }
    } else {
        value & ((1u128 << (len * 8)) - 1) as u64
    }
}

/// Stub, just so we have a type to implement the `BackingPrivate` trait.
#[derive(Default)]
pub struct CcaEmulationCache;

#[expect(private_interfaces)]
impl BackingPrivate for CcaBacked {
    type HclBacking<'cca> = Cca;
    type Shared = CcaBackedShared;
    type EmulationCache = CcaEmulationCache;

    fn shared(shared: &BackingShared) -> &Self::Shared {
        let BackingShared::Cca(shared) = shared else {
            unreachable!()
        };
        shared
    }

    fn new(
        params: super::BackingParams<'_, '_, Self>,
        shared: &CcaBackedShared,
    ) -> Result<Self, Error> {
        // TODO: CCA: do we need a "flush_page" here (?)
        // TODO: CCA: initialize untrusted synic (?)
        Ok(Self {
            vtls: VtlArray::new(CcaVtl::new()),
            cvm: UhCvmVpState::new(
                &shared.cvm,
                params.partition,
                params.vp_info,
                UhDirectOverlay::Count as usize,
            )?,
        })
    }

    type StateAccess<'p, 'a>
        = UhVpStateAccess<'a, 'p, Self>
    where
        Self: 'a + 'p,
        'p: 'a;

    fn access_vp_state<'a, 'p>(
        this: &'a mut UhProcessor<'p, Self>,
        vtl: GuestVtl,
    ) -> Self::StateAccess<'p, 'a> {
        UhVpStateAccess::new(this, vtl)
    }

    fn init(vp: &mut UhProcessor<'_, Self>) {
        // initialise non-zero registers for plane
        // TODO: CCA: SIMD regs?
        const PMCR_EL0_DEFAULT: u64 = 1 << 6;
        const MDSCR_EL1_DEFAULT: u64 = 1 << 11;

        vp.sysreg_write(GuestVtl::Vtl0, SystemReg::PMCR_EL0, PMCR_EL0_DEFAULT)
            .map_err(vp_state::Error::SetRegisters)
            .unwrap();

        vp.sysreg_write(GuestVtl::Vtl0, SystemReg::MDSCR_EL1, MDSCR_EL1_DEFAULT)
            .map_err(vp_state::Error::SetRegisters)
            .unwrap()
    }

    async fn run_vp(
        this: &mut UhProcessor<'_, Self>,
        dev: &impl CpuIo,
        _stop: &mut virt::StopVp<'_>,
    ) -> Result<(), VpHaltReason> {
        // TODO: CCA: TDX implementation handled "deliverability events/interrupts" here,
        // no clue what they're about, potentially some VBS stuff?

        // TODO: CCA: NEXT: move this to `init`?
        this.set_plane_enter();
        this.runner.cca_rsi_plane_run_mut().exit.exit_reason = RSI_PLANE_EXIT_INVALID;

        // Run the CCA plane.
        // This will return when the plane exits.
        let intercepted = this
            .runner
            .run()
            .map_err(|e| dev.fatal_error(CcaRunVpError(e).into()))?;

        if intercepted && this.runner.cca_rsi_plane_exit().exit_reason != RSI_PLANE_EXIT_INVALID {
            // Preserve the plane context, so we can restore it later.
            this.preserve_plane_context();

            // CCA: note, this is a very simplified version of the exit handling,
            // just enough to get the TMK running.
            // TODO: CCA: NEXT: document how we integrate with the wider emulation
            // system.
            let cca_exit = CcaExit(this.runner.cca_rsi_plane_exit());
            let exit_reason = cca_exit.exit_reason();
            let esr_el2 = cca_exit.esr_el2();
            match exit_reason {
                PlaneExitReason::Sync => {
                    match cca_exit.esr_el2_class() {
                        ExceptionClass::DataAbort => {
                            // get the address that caused the data abort
                            let address = cca_exit.far_el2();
                            let iss = IssDataAbort::from(esr_el2.iss());
                            if !iss.isv() {
                                tracing::warn!(
                                    esr_el2 = cca_exit.0.esr_el2,
                                    "CCA data abort has no valid instruction syndrome"
                                );
                                return Err(dev.fatal_error(
                                    CcaUnsupportedExit::InvalidDataAbortIss(cca_exit.0.esr_el2)
                                        .into(),
                                ));
                            }

                            let len = 1usize << iss.sas();
                            let srt = iss.srt();

                            if iss.wnr() {
                                // Handle MMIO write
                                if let Some(value) = cca_exit.gpr_or_zero_register(srt) {
                                    dev.write_mmio(
                                        this.vp_index(),
                                        address,
                                        &value.to_ne_bytes()[..len],
                                    )
                                    .await;
                                } else {
                                    tracing::warn!(
                                        srt,
                                        "MMIO write not handled, srt is outside the RSI GPR array"
                                    );
                                }
                            } else {
                                // Handle MMIO read
                                let mut value = [0u8; size_of::<u64>()];
                                dev.read_mmio(this.vp_index(), address, &mut value[..len])
                                    .await;

                                if srt != AARCH64_ZERO_REGISTER_INDEX {
                                    if let Some(gpr) = this
                                        .runner
                                        .cca_rsi_plane_entry()
                                        .gprs
                                        .get_mut(usize::from(srt))
                                    {
                                        *gpr = extend_mmio_read(value, len, iss.sse(), iss.sf());
                                    } else {
                                        tracing::warn!(
                                            srt,
                                            "MMIO read not handled, srt is outside the RSI GPR array"
                                        );
                                    }
                                }
                            }
                            this.runner.cca_rsi_plane_entry().pc += 4; // Advance PC
                        }
                        ExceptionClass::InstructionAbort => {
                            // Handle instruction abort
                            let iss = IssInstructionAbort::from_bits(esr_el2.iss());

                            let reason = InstructionAbortReason::from(iss.ifsc());

                            if iss.fnv() {
                                tracing::warn!("CCA InstructionAbort: FAR_EL2 is not valid");

                                return Err(dev.fatal_error(
                                    CcaUnsupportedExit::InstructionAbort {
                                        esr_el2: cca_exit.0.esr_el2,
                                        elr_el2: cca_exit.elr_el2(),
                                        far_el2: cca_exit.far_el2(),
                                        fipa: 0,
                                        fipa_state: u8::MAX,
                                        ifsc: iss.ifsc().0,
                                        reason,
                                        far_not_valid: iss.fnv(),
                                    }
                                    .into(),
                                ));
                            }

                            let far = cca_exit.far_el2();
                            let hpfar = cca_exit.hpfar_el2();
                            let fipa = (hpfar.fipa() << 12) | (far & 0xfff);

                            let plane_state = match this.ipa_state_read(fipa) {
                                Ok(state) => state,
                                Err(e) => {
                                    tracing::warn!(
                                        error = ?e,
                                        fipa,
                                        "failed to read IPA state; state will be u8::MAX which is unavailable"
                                    );
                                    None
                                }
                            };

                            return Err(dev.fatal_error(
                                CcaUnsupportedExit::InstructionAbort {
                                    esr_el2: cca_exit.0.esr_el2,
                                    elr_el2: cca_exit.elr_el2(),
                                    far_el2: cca_exit.far_el2(),
                                    fipa,
                                    fipa_state: plane_state.map_or(u8::MAX, |state| state as u8),
                                    ifsc: iss.ifsc().0,
                                    reason,
                                    far_not_valid: iss.fnv(),
                                }
                                .into(),
                            ));
                        }
                        ExceptionClass::SimdAccess => {
                            this.runner.cca_plane_no_trap_simd();
                        }
                        ExceptionClass::SmcError => {
                            tracing::warn!("SmcError exception triggered, but not handled");
                        }
                        ExceptionClass::SystemRegister => {
                            let iss = IssSystem::from(esr_el2.iss());
                            let source_value = cca_exit.gpr_or_zero_register(iss.rt());
                            let exit_esr_el2 = cca_exit.0.esr_el2;
                            this.handle_system_register_trap(iss, source_value, exit_esr_el2, dev)?;
                        }
                        ExceptionClass::Unknown(exception_class) => {
                            tracing::warn!(
                                exception_class,
                                esr_el2 = cca_exit.0.esr_el2,
                                "unsupported CCA exception class"
                            );
                            return Err(dev.fatal_error(
                                CcaUnsupportedExit::ExceptionClass {
                                    exception_class,
                                    esr_el2: cca_exit.0.esr_el2,
                                }
                                .into(),
                            ));
                        }
                    }
                }
                PlaneExitReason::Irq => {
                    let irq_exit = cca_exit.local_interrupt_exit();
                    this.request_asserted_local_interrupts(irq_exit, dev)?;
                }
                PlaneExitReason::Unknown(exit_reason) => {
                    tracing::warn!(exit_reason, "unsupported CCA plane exit reason");
                    return Err(dev.fatal_error(CcaUnsupportedExit::ExitReason(exit_reason).into()));
                }
            }
        }
        Ok(())
    }

    fn process_interrupts(
        this: &mut UhProcessor<'_, Self>,
        _scan_irr: VtlArray<bool, 2>,
        first_scan_irr: &mut bool,
        dev: &impl CpuIo,
    ) -> bool {
        let _ = dev;
        for vtl in [GuestVtl::Vtl1, GuestVtl::Vtl0] {
            this.poll_gic(vtl);
        }
        *first_scan_irr = false;
        false
    }

    fn poll_apic(_this: &mut UhProcessor<'_, Self>, _vtl: GuestVtl, _scan_irr: bool) {
        // CCA uses poll_gic from its process_interrupts implementation.
    }

    fn request_extint_readiness(_this: &mut UhProcessor<'_, Self>) {
        unreachable!("extint managed through software apic")
    }

    fn request_untrusted_sint_readiness(_this: &mut UhProcessor<'_, Self>, _sints: u16) {
        // TODO: CCA: handle this for CCA untrusted synic
        unimplemented!();
    }

    // fn handle_cross_vtl_interrupts(
    //     _this: &mut UhProcessor<'_, Self>,
    //     _dev: &impl CpuIo,
    // ) -> Result<bool, UhRunVpError> {
    //     // TODO: CCA: handle cross VTL interrupts when GIC support is added
    //     Ok(false)
    // }

    fn hv(&self, _vtl: GuestVtl) -> Option<&ProcessorVtlHv> {
        None
    }

    fn hv_mut(&mut self, _vtl: GuestVtl) -> Option<&mut ProcessorVtlHv> {
        None
    }

    fn handle_vp_start_enable_vtl_wake(_this: &mut UhProcessor<'_, Self>, _vtl: GuestVtl) {
        todo!()
    }

    fn vtl1_inspectable(_this: &UhProcessor<'_, Self>) -> bool {
        todo!()
    }
}

impl UhProcessor<'_, CcaBacked> {
    fn sysreg_write(
        &mut self,
        vtl: GuestVtl,
        reg: SystemReg,
        val: u64,
    ) -> Result<(), register::SetRegError> {
        self.runner.cca_sysreg_write(vtl, reg, val)
    }

    fn sysreg_read(
        &mut self,
        vtl: GuestVtl,
        reg: SystemReg,
        val: &mut u64,
    ) -> Result<(), register::GetRegError> {
        self.runner.cca_sysreg_read(vtl, reg, val)
    }

    fn ipa_state_read(&self, fipa: u64) -> Result<Option<u64>, GetIpaStateError> {
        self.runner.cca_ipa_state_read(fipa)
    }

    fn set_plane_enter(&mut self) {
        self.runner.cca_set_plane_enter();
        self.runner.cca_rsi_plane_entry().gicv3_hcr |= ICH_HCR_TC;
    }

    fn request_virtual_timer_interrupt(
        &mut self,
        vtl: GuestVtl,
        dev: &impl CpuIo,
    ) -> Result<(), VpHaltReason> {
        self.request_gic_interrupt(vtl, self.shared.virt_timer_ppi, dev)
    }

    fn request_asserted_local_interrupts(
        &mut self,
        irq_exit: CcaLocalInterruptExit,
        dev: &impl CpuIo,
    ) -> Result<(), VpHaltReason> {
        if matches!(irq_exit.exception_class, ExceptionClass::SystemRegister) {
            let iss = IssSystem::from(irq_exit.esr_el2.iss());
            self.handle_system_register_trap(
                iss,
                irq_exit.source_value,
                irq_exit.exit_esr_el2,
                dev,
            )?;
        } else if irq_exit.virtual_timer_asserted {
            self.request_virtual_timer_interrupt(self.backing.cvm.exit_vtl, dev)?;
        } else {
            tracing::trace!("CCA IRQ exit had an unrecognized local interrupt source");
        }

        Ok(())
    }

    fn handle_system_register_trap(
        &mut self,
        iss: IssSystem,
        source_value: Option<u64>,
        exit_esr_el2: u64,
        dev: &impl CpuIo,
    ) -> Result<(), VpHaltReason> {
        let system_reg = iss.system_reg();

        match system_reg {
            SystemReg::ICC_PMR_EL1 if !iss.direction() => {
                let Some(value) = source_value else {
                    return Err(dev.fatal_error(
                        CcaUnsupportedExit::MissingSystemRegisterValue {
                            system_reg,
                            esr_el2: exit_esr_el2,
                        }
                        .into(),
                    ));
                };
                let vtl = self.backing.cvm.exit_vtl;
                self.backing.vtls[vtl].gic.priority_mask = value as u8;
                tracing::trace!(priority_mask = value as u8, ?vtl, "updated CCA ICC_PMR_EL1");
                self.runner.cca_rsi_plane_entry().pc += 4;
            }
            SystemReg::ICC_SGI1R_EL1 if !iss.direction() => {
                let Some(value) = source_value else {
                    tracing::warn!(
                        rt = iss.rt(),
                        "CCA ICC_SGI1R_EL1 write has source register outside RSI GPR array"
                    );
                    return Err(dev.fatal_error(
                        CcaUnsupportedExit::MissingSystemRegisterValue {
                            system_reg,
                            esr_el2: exit_esr_el2,
                        }
                        .into(),
                    ));
                };

                self.handle_icc_sgi1r_el1_write(value, dev)?;
                self.runner.cca_rsi_plane_entry().pc += 4;
            }
            _ => {
                tracing::warn!(
                    ?system_reg,
                    esr_el2 = exit_esr_el2,
                    "unsupported CCA system register trap"
                );
                return Err(dev.fatal_error(
                    CcaUnsupportedExit::UnsupportedSystemRegister {
                        system_reg,
                        esr_el2: exit_esr_el2,
                    }
                    .into(),
                ));
            }
        }

        Ok(())
    }

    fn handle_icc_sgi1r_el1_write(
        &mut self,
        value: u64,
        dev: &impl CpuIo,
    ) -> Result<(), VpHaltReason> {
        let sgi = GicrSgi::from(value);
        let intid = sgi.intid();

        if sgi_targets_current_vp(sgi, self.inner.vp_info.mpidr) {
            let vtl = self.backing.cvm.exit_vtl;
            self.request_gic_interrupt(vtl, intid, dev)?;
            tracing::debug!(
                intid,
                ?vtl,
                value,
                "queued CCA self SGI from ICC_SGI1R_EL1 write"
            );
        } else {
            tracing::trace!(
                intid,
                value,
                "ignored CCA ICC_SGI1R_EL1 write that does not target current VP"
            );
        }

        Ok(())
    }

    fn request_gic_interrupt(
        &mut self,
        vtl: GuestVtl,
        intid: u32,
        dev: &impl CpuIo,
    ) -> Result<(), VpHaltReason> {
        if let Err(err) = self.backing.vtls[vtl].gic.request_interrupt(intid) {
            let err = match err {
                CcaGicRequestError::InvalidIntid => {
                    CcaUnsupportedExit::InvalidPrivateGicInterrupt(intid)
                }
                CcaGicRequestError::NoFreeSlot => {
                    CcaUnsupportedExit::NoFreePendingGicInterrupt(intid)
                }
            };
            return Err(dev.fatal_error(err.into()));
        }

        tracing::info!(intid, ?vtl, "requested CCA private GIC interrupt");
        Ok(())
    }

    fn poll_gic(&mut self, vtl: GuestVtl) {
        loop {
            let gic = &self.backing.vtls[vtl].gic;
            let Some(interrupt) = self.shared.cvm.gic.next_pending_private_interrupt(
                self.vp_index(),
                gic.pending_mask(),
                gic.priority_mask,
            ) else {
                break;
            };

            if !inject_virtual_interrupt(
                &mut self.runner.cca_rsi_plane_entry().gicv3_lrs,
                interrupt,
            ) {
                tracelimit::warn_ratelimited!(
                    intid = interrupt.intid,
                    priority = interrupt.priority,
                    group1 = interrupt.group1,
                    ?vtl,
                    "no free CCA GIC list register; leaving interrupt pending"
                );
                return;
            }

            self.backing.vtls[vtl]
                .gic
                .complete_interrupt(interrupt.intid);
            tracing::debug!(
                intid = interrupt.intid,
                priority = interrupt.priority,
                group1 = interrupt.group1,
                ?vtl,
                "injected CCA GIC interrupt"
            );
        }

        // Device SPIs belong to VTL0. Reconcile the model with the RMM list
        // registers before selecting another interrupt. If a level-triggered
        // line remains asserted after its LR is retired, it becomes eligible
        // for injection again.
        if vtl != GuestVtl::Vtl0 {
            return;
        }

        let vp = self.vp_index();
        let lrs = &self.runner.cca_rsi_plane_entry().gicv3_lrs;
        self.shared
            .cvm
            .gic
            .retain_in_flight_spis(vp, |intid| virtual_interrupt_is_listed(lrs, intid));

        let running_priority = self.backing.vtls[vtl].gic.priority_mask;
        while let Some(interrupt) = self
            .shared
            .cvm
            .gic
            .next_pending_spi_interrupt(self.vp_index(), running_priority)
        {
            if !inject_virtual_interrupt(
                &mut self.runner.cca_rsi_plane_entry().gicv3_lrs,
                interrupt,
            ) {
                tracelimit::warn_ratelimited!(
                    intid = interrupt.intid,
                    priority = interrupt.priority,
                    group1 = interrupt.group1,
                    ?vtl,
                    "no free CCA GIC list register; leaving shared interrupt pending"
                );
                return;
            }

            self.shared
                .cvm
                .gic
                .mark_spi_injected(self.vp_index(), interrupt.intid);
            tracing::debug!(
                intid = interrupt.intid,
                priority = interrupt.priority,
                group1 = interrupt.group1,
                ?vtl,
                "injected pending CCA shared GIC interrupt"
            );
        }
    }

    // Copy the exit context to the entry context.
    fn preserve_plane_context(&mut self) {
        let plane_run = self.runner.cca_rsi_plane_run_mut();

        // Copy GPRs across.
        plane_run
            .entry
            .gprs
            .copy_from_slice(&plane_run.exit.gprs[..]);

        // Set the PC to the ELR_EL2 value from the exit context.
        plane_run.entry.pc = plane_run.exit.elr_el2;

        // Restore the interrupted PSTATE, including the IRQ mask.
        plane_run.entry.pstate = plane_run.exit.pstate;

        // Preserve the virtual GIC state across plane exits.
        plane_run.entry.gicv3_hcr = plane_run.exit.gicv3_hcr;
        plane_run
            .entry
            .gicv3_lrs
            .copy_from_slice(&plane_run.exit.gicv3_lrs);
    }

    // TODO: CCA: lots of stuff might be needed based on the TDX implementation, something akin to:
    // async fn run_vp_cca(&mut self, dev: &impl CpuIo) -> Result<(), VpHaltReason<UhRunVpError>>
}

impl AccessVpState for UhVpStateAccess<'_, '_, CcaBacked> {
    type Error = vp_state::Error;

    fn caps(&self) -> &virt::PartitionCapabilities {
        &self.vp.partition.caps
    }

    fn commit(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn registers(&mut self) -> Result<vp::Registers, Self::Error> {
        let mut reg: vp::Registers = vp::Registers::default();

        let plane_enter = self.vp.runner.cca_rsi_plane_entry();

        reg.x0 = plane_enter.gprs[0];
        reg.x1 = plane_enter.gprs[1];
        reg.x2 = plane_enter.gprs[2];
        reg.x3 = plane_enter.gprs[3];
        reg.x4 = plane_enter.gprs[4];
        reg.x5 = plane_enter.gprs[5];
        reg.x6 = plane_enter.gprs[6];
        reg.x7 = plane_enter.gprs[7];
        reg.x8 = plane_enter.gprs[8];
        reg.x9 = plane_enter.gprs[9];
        reg.x10 = plane_enter.gprs[10];
        reg.x11 = plane_enter.gprs[11];
        reg.x12 = plane_enter.gprs[12];
        reg.x13 = plane_enter.gprs[13];
        reg.x14 = plane_enter.gprs[14];
        reg.x15 = plane_enter.gprs[15];
        reg.x16 = plane_enter.gprs[16];
        reg.x17 = plane_enter.gprs[17];
        reg.x18 = plane_enter.gprs[18];
        reg.x19 = plane_enter.gprs[19];
        reg.x20 = plane_enter.gprs[20];
        reg.x21 = plane_enter.gprs[21];
        reg.x22 = plane_enter.gprs[22];
        reg.x23 = plane_enter.gprs[23];
        reg.x24 = plane_enter.gprs[24];
        reg.x25 = plane_enter.gprs[25];
        reg.x26 = plane_enter.gprs[26];
        reg.x27 = plane_enter.gprs[27];
        reg.x28 = plane_enter.gprs[28];
        reg.fp = plane_enter.gprs[29];
        reg.lr = plane_enter.gprs[30];
        reg.pc = plane_enter.pc;

        Ok(reg)
    }

    fn set_registers(&mut self, value: &vp::Registers) -> Result<(), Self::Error> {
        self.vp.runner.cca_plane_trap_simd();
        self.vp.runner.cca_set_default_pstate();

        let vp::Registers {
            x0,
            x1,
            x2,
            x3,
            x4,
            x5,
            x6,
            x7,
            x8,
            x9,
            x10,
            x11,
            x12,
            x13,
            x14,
            x15,
            x16,
            x17,
            x18,
            x19,
            x20,
            x21,
            x22,
            x23,
            x24,
            x25,
            x26,
            x27,
            x28,
            fp,
            lr,
            pc,
            ..
        } = value;

        let plane_enter = self.vp.runner.cca_rsi_plane_entry();
        plane_enter.gprs[0] = *x0;
        plane_enter.gprs[1] = *x1;
        plane_enter.gprs[2] = *x2;
        plane_enter.gprs[3] = *x3;
        plane_enter.gprs[4] = *x4;
        plane_enter.gprs[5] = *x5;
        plane_enter.gprs[6] = *x6;
        plane_enter.gprs[7] = *x7;
        plane_enter.gprs[8] = *x8;
        plane_enter.gprs[9] = *x9;
        plane_enter.gprs[10] = *x10;
        plane_enter.gprs[11] = *x11;
        plane_enter.gprs[12] = *x12;
        plane_enter.gprs[13] = *x13;
        plane_enter.gprs[14] = *x14;
        plane_enter.gprs[15] = *x15;
        plane_enter.gprs[16] = *x16;
        plane_enter.gprs[17] = *x17;
        plane_enter.gprs[18] = *x18;
        plane_enter.gprs[19] = *x19;
        plane_enter.gprs[20] = *x20;
        plane_enter.gprs[21] = *x21;
        plane_enter.gprs[22] = *x22;
        plane_enter.gprs[23] = *x23;
        plane_enter.gprs[24] = *x24;
        plane_enter.gprs[25] = *x25;
        plane_enter.gprs[26] = *x26;
        plane_enter.gprs[27] = *x27;
        plane_enter.gprs[28] = *x28;
        plane_enter.gprs[29] = *fp;
        plane_enter.gprs[30] = *lr;
        plane_enter.pc = *pc;

        Ok(())
    }

    fn system_registers(&mut self) -> Result<vp::SystemRegisters, Self::Error> {
        let mut vp_regs = vp::SystemRegisters::default();

        let mut get = |reg: SystemReg, value: &mut u64| {
            self.vp
                .sysreg_read(self.vtl, reg, value)
                .map_err(vp_state::Error::GetRegisters)
        };

        get(SystemReg::SCTLR, &mut vp_regs.sctlr_el1)?;
        get(SystemReg::TTBR0_EL1, &mut vp_regs.ttbr0_el1)?;
        get(SystemReg::TTBR1_EL1, &mut vp_regs.ttbr1_el1)?;
        get(SystemReg::TCR_EL1, &mut vp_regs.tcr_el1)?;
        get(SystemReg::ESR_EL1, &mut vp_regs.esr_el1)?;
        get(SystemReg::FAR_EL1, &mut vp_regs.far_el1)?;
        get(SystemReg::MAIR_EL1, &mut vp_regs.mair_el1)?;
        get(SystemReg::ELR_EL1, &mut vp_regs.elr_el1)?;
        get(SystemReg::VBAR, &mut vp_regs.vbar_el1)?;

        Ok(vp_regs)
    }

    fn set_system_registers(&mut self, value: &vp::SystemRegisters) -> Result<(), Self::Error> {
        let vp::SystemRegisters {
            sctlr_el1,
            ttbr0_el1,
            ttbr1_el1,
            tcr_el1,
            esr_el1,
            far_el1,
            mair_el1,
            elr_el1,
            vbar_el1,
        } = *value;

        for (reg, value) in [
            (SystemReg::SCTLR, sctlr_el1),
            (SystemReg::TTBR0_EL1, ttbr0_el1),
            (SystemReg::TTBR1_EL1, ttbr1_el1),
            (SystemReg::TCR_EL1, tcr_el1),
            (SystemReg::ESR_EL1, esr_el1),
            (SystemReg::FAR_EL1, far_el1),
            (SystemReg::MAIR_EL1, mair_el1),
            (SystemReg::ELR_EL1, elr_el1),
            (SystemReg::VBAR, vbar_el1),
        ] {
            self.vp
                .sysreg_write(self.vtl, reg, value)
                .map_err(vp_state::Error::SetRegisters)?;
        }

        Ok(())
    }
}

impl HardwareIsolatedBacking for CcaBacked {
    fn cvm_state(&self) -> &UhCvmVpState {
        &self.cvm
    }

    fn cvm_state_mut(&mut self) -> &mut UhCvmVpState {
        &mut self.cvm
    }

    fn cvm_partition_state(shared: &Self::Shared) -> &UhCvmPartitionState {
        &shared.cvm
    }

    fn switch_vtl(this: &mut UhProcessor<'_, Self>, _source_vtl: GuestVtl, target_vtl: GuestVtl) {
        // TODO: CCA: This might need more work when multiple VTLs are supported.

        this.backing.cvm_state_mut().exit_vtl = target_vtl;
    }

    fn translation_registers(
        &self,
        _this: &UhProcessor<'_, Self>,
        _vtl: GuestVtl,
    ) -> TranslationRegisters {
        unimplemented!()
    }

    fn tlb_flush_lock_access<'a>(
        vp_index: Option<VpIndex>,
        partition: &'a UhPartitionInner,
        shared: &'a Self::Shared,
    ) -> impl TlbFlushLockAccess + 'a {
        let vp_index_t = vp_index.unwrap_or_else(|| VpIndex::new(0));

        CcaTlbLockFlushAccess {
            vp_index: vp_index_t,
            partition,
            shared,
        }
    }

    fn pending_event_vector(_this: &UhProcessor<'_, Self>, _vtl: GuestVtl) -> Option<u8> {
        None
    }

    fn is_interrupt_pending(
        _this: &mut UhProcessor<'_, Self>,
        _vtl: GuestVtl,
        _check_rflags: bool,
        _dev: &impl CpuIo,
    ) -> bool {
        false
    }

    fn set_pending_exception(
        _this: &mut UhProcessor<'_, Self>,
        _vtl: GuestVtl,
        _event: hvdef::HvX64PendingExceptionEvent,
    ) {
    }

    ///TODO Place holder. Not implemented for arm64.
    fn intercept_message_state(
        _this: &UhProcessor<'_, Self>,
        _vtl: GuestVtl,
        _include_optional_state: bool,
    ) -> InterceptMessageState {
        InterceptMessageState {
            instruction_length_and_cr8: 0,
            cpl: 0,
            efer_lma: false,
            cs: hvdef::HvX64SegmentRegister::new_zeroed(),
            rip: 0,
            rflags: 0,
            rax: 0,
            rdx: 0,
            rcx: 0,
            rsi: 0,
            rdi: 0,
            optional: None,
        }
    }

    fn cr0(_this: &UhProcessor<'_, Self>, _vtl: GuestVtl) -> u64 {
        0
    }

    fn cr4(_this: &UhProcessor<'_, Self>, _vtl: GuestVtl) -> u64 {
        0
    }

    fn cr_intercept_registration(
        _this: &mut UhProcessor<'_, Self>,
        _intercept_control: HvRegisterCrInterceptControl,
    ) {
    }

    fn untrusted_synic_mut(&mut self) -> Option<&mut ProcessorSynic> {
        None
    }

    fn update_deadline(_this: &mut UhProcessor<'_, Self>, _ref_time_now: u64, _next_ref_time: u64) {
        unimplemented!()
    }

    fn clear_deadline(_this: &mut UhProcessor<'_, Self>) {
        unimplemented!()
    }
}

#[expect(unused)]
struct CcaTlbLockFlushAccess<'a> {
    vp_index: VpIndex,
    partition: &'a UhPartitionInner,
    shared: &'a CcaBackedShared,
}

impl TlbFlushLockAccess for CcaTlbLockFlushAccess<'_> {
    fn flush(&mut self, _vtl: GuestVtl) {
        unimplemented!()
    }

    fn flush_entire(&mut self) {
        unimplemented!()
    }

    fn set_wait_for_tlb_locks(&mut self, _vtl: GuestVtl) {
        unimplemented!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sgi_for(mpidr: MpidrEl1) -> GicrSgi {
        GicrSgi::new()
            .with_aff3(mpidr.aff3())
            .with_aff2(mpidr.aff2())
            .with_aff1(mpidr.aff1())
            .with_rs(mpidr.aff0() / 16)
            .with_target_list(1 << (mpidr.aff0() % 16))
    }

    #[test]
    fn private_interrupt_queue_rejects_non_private_intid() {
        let mut gic = CcaGic::new();

        assert_eq!(
            gic.request_interrupt(GIC_PRIVATE_INTERRUPT_COUNT),
            Err(CcaGicRequestError::InvalidIntid)
        );
        assert_eq!(gic.pending_mask(), 0);
    }

    #[test]
    fn sgi_target_matches_current_vp() {
        let mpidr = MpidrEl1::new()
            .with_aff3(4)
            .with_aff2(3)
            .with_aff1(2)
            .with_aff0(17);

        assert!(sgi_targets_current_vp(sgi_for(mpidr), mpidr));
    }

    #[test]
    fn sgi_target_rejects_other_vps() {
        let mpidr = MpidrEl1::new()
            .with_aff3(4)
            .with_aff2(3)
            .with_aff1(2)
            .with_aff0(1);
        let matching = sgi_for(mpidr);

        assert!(!sgi_targets_current_vp(matching.with_aff1(1), mpidr));
        assert!(!sgi_targets_current_vp(
            matching.with_target_list(1 << 2),
            mpidr
        ));
        assert!(!sgi_targets_current_vp(matching.with_rs(1), mpidr));
        assert!(!sgi_targets_current_vp(matching.with_irm(true), mpidr));
    }
}

mod save_restore {
    use super::CcaBacked;
    use super::UhProcessor;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;
    use vmcore::save_restore::SavedStateNotSupported;

    impl SaveRestore for UhProcessor<'_, CcaBacked> {
        type SavedState = SavedStateNotSupported;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Err(SaveError::NotSupported)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            match state {}
        }
    }
}
