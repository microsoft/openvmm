// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Processor support for CCA Planes.

// TODO: CCA: understand what the common functionality with the HV Arm64 implementation is.
// This is one of the most stubbed parts of the CCA implementation, lots more work is needed.

use std::sync::atomic::AtomicU8;

use super::HardwareIsolatedBacking;
use super::vp_state;
use crate::Error;
use crate::TlbFlushLockAccess;
use crate::UhPartitionInner;
use crate::processor::InterceptMessageState;
use crate::{BackingShared, UhCvmPartitionState, UhCvmVpState, UhPartitionNewParams};
use aarch64defs::EsrEl2;
use aarch64defs::SystemReg;
use hcl::protocol::cca_rsi_plane_exit;
use hcl::{GuestVtl, ioctl::cca::Cca, ioctl::register};
use hv1_emulator::hv::ProcessorVtlHv;
use hv1_emulator::synic::ProcessorSynic;
use hv1_structs::VtlArray;
use hvdef::HvRegisterCrInterceptControl;
use inspect::{Inspect, InspectMut};
use virt::VpIndex;
use virt::aarch64::vp;
use virt::io::CpuIo;
use virt::{VpHaltReason, aarch64::vp::AccessVpState};
use virt_support_aarch64emu::translate::TranslationRegisters;
use zerocopy::FromZeros;

use super::{BackingSharedParams, UhProcessor, private::BackingPrivate, vp_state::UhVpStateAccess};

#[derive(Debug, Error)]
#[error("failed to run")]
struct CcaRunVpError(#[source] hcl::ioctl::Error);

// TODO: CCA: what is this needed for?
#[allow(dead_code)]
enum UhDirectOverlay {
    Sipp,
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
    // CCA: potentially needed fields, based on TDX implementation:
    // * values of control registers
    // * interrupt information
    // * exception error code
    // * TLB flush state
    // * PMU stats
    sp_el0: u64,
    sp_el1: u64,
    cpsr: u64,
}

impl CcaVtl {
    pub(crate) fn new() -> Self {
        Self {
            sp_el0: 0,
            sp_el1: 0,
            cpsr: 0,
        }
    }
}

#[derive(Inspect)]
pub struct CcaBackedShared {
    pub(crate) cvm: UhCvmPartitionState,
    // CCA: potentially needed:
    // The synic state used for untrusted SINTs, that is, the SINTs for which
    // the guest thinks it is interacting directly with the untrusted
    // hypervisor via an architecture-specific interface.
    #[inspect(iter_by_index)]
    active_vtl: Vec<AtomicU8>,
}

impl CcaBackedShared {
    pub(crate) fn new(
        partition_params: &UhPartitionNewParams<'_>,
        params: BackingSharedParams<'_>,
    ) -> Result<Self, Error> {
        Ok(Self {
            cvm: params.cvm_state.unwrap(),
            // VPs start in VTL 2.
            active_vtl: std::iter::repeat_n(2, partition_params.topology.vp_count() as usize)
                .map(AtomicU8::new)
                .collect(),
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
}

impl From<u8> for ExceptionClass {
    fn from(value: u8) -> Self {
        match value {
            0b0010_0100 => ExceptionClass::DataAbort,
            0b0010_0000 => ExceptionClass::InstructionAbort,
            0b0000_0111 => ExceptionClass::SimdAccess,
            0b0001_0111 => ExceptionClass::SmcError,
            _ => panic!("Unknown exception class: {value}"),
        }
    }
}

/// The reason for a CCA plane exit, which can be either a synchronous event
/// (like an MMIO access or an exception) or an IRQ.
#[derive(Debug, Clone, Copy)]
enum PlaneExitReason {
    Sync,
    Irq,
}

impl From<u64> for PlaneExitReason {
    fn from(value: u64) -> Self {
        match value {
            0 => PlaneExitReason::Sync,
            1 => PlaneExitReason::Irq,
            _ => panic!("Unknown CCA plane exit reason: {value}"),
        }
    }
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

    fn init(_this: &mut UhProcessor<'_, Self>) {
        // TODO: CCA: init non-zero registers for plane?
        // TODO: CCA: SIMD regs?
    }

    async fn run_vp(
        this: &mut UhProcessor<'_, Self>,
        dev: &impl CpuIo,
        _stop: &mut virt::StopVp<'_>,
    ) -> Result<(), VpHaltReason> {
        // TODO: CCA: TDX implementation handled "deliverability notifications" here,
        // no clue what they're about, potentially some VBS stuff?

        // TODO: CCA: NEXT: move this to `init`?
        this.set_plane_enter();

        // Run the CCA plane.
        // This will return when the plane exits.
        let intercepted = this
            .runner
            .run()
            .map_err(|e| dev.fatal_error(CcaRunVpError(e).into()))?;

        // let mut has_intercept = self
        //     .runner
        //     .run()
        //     .map_err(|e| dev.fatal_error(SnpRunVpError(e).into()))?;

        // Preserve the plane context, so we can restore it later.
        this.preserve_plane_context();

        if intercepted {
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
                            // Based on the CpuIo impl in tmk_vmm/src/run.rs, dev.is_mmio(address)
                            // always returns false, so we handle MMIO access here.

                            if esr_el2.is_write() {
                                // Handle MMIO write
                                dev.write_mmio(
                                    this.vp_index(),
                                    address,
                                    &this.runner.cca_rsi_plane_exit().gprs[esr_el2.srt() as usize]
                                        .to_ne_bytes(),
                                )
                                .await;
                            } else {
                                // Handle MMIO read
                                println!("MMIO read");
                                todo!();
                            }
                            this.runner.cca_rsi_plane_entry().pc += 4; // Advance PC
                        }
                        ExceptionClass::InstructionAbort => {
                            // Handle instruction abort
                            println!("instruction abort");
                            todo!();
                        }
                        ExceptionClass::SimdAccess => {
                            this.runner.cca_plane_no_trap_simd();
                        }
                        ExceptionClass::SmcError => {
                            println!("SMC error");
                        }
                    }
                }
                PlaneExitReason::Irq => {
                    // Handle IRQ exit
                    println!("IRQ exit");
                    //todo!();
                }
            }
        }
        Ok(())
    }

    fn process_interrupts(
        _this: &mut UhProcessor<'_, Self>,
        _scan_irr: VtlArray<bool, 2>,
        _first_scan_irr: &mut bool,
        _dev: &impl CpuIo,
    ) -> bool {
        false
    }

    fn poll_apic(_this: &mut UhProcessor<'_, Self>, _vtl: GuestVtl, _scan_irr: bool) {
        // TODO: CCA: poll GIC?
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

    fn set_plane_enter(&mut self) {
        self.runner.cca_set_plane_enter();
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

        // Set GICv3 HCR to the value from the exit context.
        plane_run.entry.gicv3_hcr = plane_run.exit.gicv3_hcr;
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
        // TODO: CCA: NEXT: this fails at the end of the TMK
        todo!()
    }

    fn set_system_registers(&mut self, _value: &vp::SystemRegisters) -> Result<(), Self::Error> {
        // TODO: CCA: should figure out where to initialize these registers
        // Maybe in `CcaBacked::init`?
        const SCTLR_EL1_DEFAULT: u64 = 0xC50878;
        const PMCR_EL0_DEFAULT: u64 = 1 << 6;
        const MDSCR_EL1_DEFAULT: u64 = 1 << 11;

        self.vp
            .sysreg_write(GuestVtl::Vtl0, SystemReg::SCTLR, SCTLR_EL1_DEFAULT)
            .map_err(vp_state::Error::SetRegisters)?;
        self.vp
            .sysreg_write(GuestVtl::Vtl0, SystemReg::PMCR_EL0, PMCR_EL0_DEFAULT)
            .map_err(vp_state::Error::SetRegisters)?;
        self.vp
            .sysreg_write(GuestVtl::Vtl0, SystemReg::MDSCR_EL1, MDSCR_EL1_DEFAULT)
            .map_err(vp_state::Error::SetRegisters)
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

#[allow(dead_code)]
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
