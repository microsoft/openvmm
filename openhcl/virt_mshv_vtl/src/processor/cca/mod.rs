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
use aarch64defs::SystemReg;
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
use std::cmp::min;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::aarch64::vp;
use virt::aarch64::vp::AccessVpState;
use virt::io::CpuIo;
use virt_support_aarch64emu::translate::TranslationRegisters;
use virt_support_gic::ListRegisterInterrupt;
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
    #[allow(dead_code)]
    #[error("CCA private GIC interrupt ID {0} is outside the SGI/PPI range")]
    InvalidPrivateGicInterrupt(u32),
    #[error("unsupported CCA system register trap for {system_reg:?} in ESR_EL2 {esr_el2:#x}")]
    UnsupportedSystemRegister { system_reg: SystemReg, esr_el2: u64 },
    #[allow(dead_code)]
    #[error("CCA {system_reg:?} write in ESR_EL2 {esr_el2:#x} has no accessible source register")]
    MissingSystemRegisterValue { system_reg: SystemReg, esr_el2: u64 },
}

const AARCH64_ZERO_REGISTER_INDEX: u8 = 31;
const CNTV_CTL_ENABLE: u64 = 1 << 0;
const CNTV_CTL_IMASK: u64 = 1 << 1;
const CNTV_CTL_ISTATUS: u64 = 1 << 2;

const ICH_HCR_UIE: u64 = 1 << 1;
const ICH_HCR_LRENPIE: u64 = 1 << 2;
const ICH_HCR_NPIE: u64 = 1 << 3;
const ICH_HCR_TC: u64 = 1 << 10;
const ICH_HCR_TDIR: u64 = 1 << 14;
const ICH_HCR_EOI_COUNT_MASK: u64 = 0x1f << 27;
const ICH_HCR_EOI_COUNT_SHIFT: u32 = 27;

const ICH_VMCR_VEOIM: u64 = 1 << 9;

const ICH_VTR_LIST_REGS_MASK: u64 = 0x1f;

const ICH_LR_VINTID_MASK: u64 = u32::MAX as u64;
const ICH_LR_PRIORITY_SHIFT: u32 = 48;
const ICH_LR_GROUP1: u64 = 1 << 60;
// ICH_LR_EL2.State is encoded in bits [63:62]:
//
// 00 = invalid
// 01 = pending
// 10 = active
// 11 = pending and active
const ICH_LR_PENDING: u64 = 1 << 62;
const ICH_LR_ACTIVE: u64 = 1 << 63;
const ICH_LR_STATE_MASK: u64 = 3 << 62;
const ICH_LR_PRIORITY_MASK: u64 = 0xff << ICH_LR_PRIORITY_SHIFT;

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

#[derive(Clone, InspectMut, Inspect)]
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
    /// Guest-programmed GIC priority mask from ICC_PMR_EL1.
    priority_mask: u8,
    /// Virtual interrupts that did not fit in the implemented LRs.
    ///
    /// These are the tail of the KVM-style active/pending list. They remain
    /// durable here until EOIcount or a trapped ICC_DIR_EL1 deactivates them.
    #[inspect(iter_by_index)]
    gic_lr_overflow: Vec<u64>,
    /// Most recently returned ICH_VMCR_EL2 value for this plane.
    gic_vmcr: u64,
}

impl CcaVtl {
    pub(crate) fn new() -> Self {
        Self {
            sp_el0: 0,
            sp_el1: 0,
            cpsr: 0,
            priority_mask: u8::MAX,
            gic_lr_overflow: Vec::new(),
            gic_vmcr: 0,
        }
    }
}

#[derive(Inspect)]
pub struct CcaBackedShared {
    pub(crate) cvm: UhCvmPartitionState,
    virt_timer_ppi: u32,
    gic_num_lrs: usize,
}

impl CcaBackedShared {
    pub(crate) fn new(params: BackingSharedParams<'_>, virt_timer_ppi: u32) -> Result<Self, Error> {
        let realm_config = params.hcl.get_realm_config().map_err(Error::Hcl)?;
        Ok(Self {
            cvm: params.cvm_state.unwrap(),
            virt_timer_ppi,
            gic_num_lrs: gic_num_lrs(realm_config.gicv3_vtr()),
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
    system_register_trap: Option<(IssSystem, u64, u64)>,
    virtual_timer_asserted: bool,
    gic_maintenance_status: u64,
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

    /// Returns whether the virtual timer interrupt is enabled, unmasked, and
    /// asserted in the returned CCA plane state.
    fn virtual_timer_asserted(&self) -> bool {
        self.0.cntv_ctl_el0 & (CNTV_CTL_ENABLE | CNTV_CTL_IMASK | CNTV_CTL_ISTATUS)
            == CNTV_CTL_ENABLE | CNTV_CTL_ISTATUS
    }

    fn local_interrupt_exit(&self) -> CcaLocalInterruptExit {
        let esr_el2 = self.esr_el2();
        let exception_class = self.esr_el2_class();
        let system_register_trap =
            matches!(exception_class, ExceptionClass::SystemRegister).then(|| {
                let iss = IssSystem::from(esr_el2.iss());
                let value = self
                    .gpr_or_zero_register(iss.rt())
                    .expect("ISS Rt is a valid AArch64 register index");
                (iss, value, self.0.esr_el2)
            });

        CcaLocalInterruptExit {
            system_register_trap,
            virtual_timer_asserted: self.virtual_timer_asserted(),
            gic_maintenance_status: self.0.gicv3_misr,
        }
    }
}

/// Returns the number of architecturally implemented GIC list registers.
///
/// The RSI run page has room for 16 LRs, but that is only the ABI capacity.
/// ICH_VTR_EL2.ListRegs contains the zero-based implemented count. The GIC
/// architecture limits the usable count to 16 even though the field is wider.
fn gic_num_lrs(gicv3_vtr: u64) -> usize {
    (((gicv3_vtr & ICH_VTR_LIST_REGS_MASK) + 1) as usize)
        .min(aarch64defs::rsi::RSI_PLANE_GIC_NUM_LRS)
}

fn lr_is_valid(lr: u64) -> bool {
    lr & ICH_LR_STATE_MASK != 0
}

fn lr_is_pending(lr: u64) -> bool {
    // NPIE concerns the pure-pending state (01), not active-and-pending (11).
    lr & ICH_LR_STATE_MASK == ICH_LR_PENDING
}

fn lr_is_active(lr: u64) -> bool {
    lr & ICH_LR_ACTIVE != 0
}

/// Recomputes maintenance-interrupt controls after packing the LRs.
///
fn configure_gic_maintenance(
    gicv3_hcr: &mut u64,
    pending_outside_lrs: bool,
    active_outside_lrs: bool,
    any_outside_lrs: bool,
) {
    *gicv3_hcr &=
        !(ICH_HCR_UIE | ICH_HCR_LRENPIE | ICH_HCR_NPIE | ICH_HCR_TDIR | ICH_HCR_EOI_COUNT_MASK);

    if pending_outside_lrs {
        *gicv3_hcr |= ICH_HCR_NPIE;
    }
    if active_outside_lrs {
        // EOIcount provides ordered deactivation for EOImode == 0. Trap DIR as
        // well because EOImode can change without a notification and DIR names
        // the interrupt explicitly when EOImode == 1.
        *gicv3_hcr |= ICH_HCR_LRENPIE | ICH_HCR_TDIR;
    }
    if any_outside_lrs {
        *gicv3_hcr |= ICH_HCR_UIE;
    }
}

fn gic_eoi_count(gicv3_hcr: u64) -> usize {
    ((gicv3_hcr & ICH_HCR_EOI_COUNT_MASK) >> ICH_HCR_EOI_COUNT_SHIFT) as usize
}

/// Applies ordered EOImode-0 deactivations to active entries outside the LRs.
///
/// For example, given:
///
/// ```text
/// overflow before: [P40, A10, A11, A12]
/// EOIcount: 2
/// ```
///
/// The pure-pending P40 is skipped, and the first two active entries, A10 and
/// A11, are deactivated. The resulting overflow list is:
///
/// ```text
/// overflow after: [P40, A12]
/// ```
fn consume_eoi_count(lr_overflow: &mut Vec<u64>, mut eoi_count: usize) {
    lr_overflow.retain(|lr| {
        if eoi_count != 0 && lr_is_active(*lr) {
            eoi_count -= 1;
            false
        } else {
            true
        }
    });

    if eoi_count != 0 {
        tracelimit::warn_ratelimited!(
            eoi_count,
            "CCA GIC EOIcount exceeded the active LR overflow tail"
        );
    }
}

/// Orders the software active/pending list the same way KVM does for overflow:
/// deliverable pure-pending entries first, followed by active entries.
fn sort_gic_candidates(candidates: &mut [u64]) {
    candidates.sort_by_key(|lr| {
        (
            !lr_is_pending(*lr),
            ((*lr & ICH_LR_PRIORITY_MASK) >> ICH_LR_PRIORITY_SHIFT) as u8,
            (*lr & ICH_LR_VINTID_MASK) as u32,
        )
    });
}

fn deactivate_virtual_interrupt(lrs: &mut [u64], active_overflow: &mut Vec<u64>, intid: u32) {
    let deactivate = |lr: &mut u64| {
        if lr_is_active(*lr) && (*lr & ICH_LR_VINTID_MASK) as u32 == intid {
            *lr &= !ICH_LR_ACTIVE;
            if !lr_is_valid(*lr) {
                *lr = 0;
            }
        }
    };

    lrs.iter_mut().for_each(deactivate);
    active_overflow.iter_mut().for_each(deactivate);
    active_overflow.retain(|lr| lr_is_valid(*lr));
}

/// Adds an interrupt to an unbounded software active/pending list.
fn queue_virtual_interrupt(candidates: &mut Vec<u64>, interrupt: PendingInterrupt) {
    if let Some(lr) = candidates
        .iter_mut()
        .find(|lr| lr_is_valid(**lr) && **lr & ICH_LR_VINTID_MASK == u64::from(interrupt.intid))
    {
        *lr |= ICH_LR_PENDING;
        return;
    }

    candidates.push(
        u64::from(interrupt.intid)
            | (u64::from(interrupt.priority) << ICH_LR_PRIORITY_SHIFT)
            | if interrupt.group1 { ICH_LR_GROUP1 } else { 0 }
            | ICH_LR_PENDING,
    );
}

fn running_priority(lrs: &[u64]) -> u8 {
    let mut running = 0xff;

    for &lr in lrs {
        // Pending interrupts are not running and therefore do not constrain
        // which interrupt can be injected next.
        if lr & ICH_LR_ACTIVE == 0 {
            continue;
        }

        let priority = ((lr & ICH_LR_PRIORITY_MASK) >> ICH_LR_PRIORITY_SHIFT) as u8;
        running = min(running, priority);
    }

    running
}

fn interrupt_priority_threshold(priority_mask: u8, lrs: &[u64]) -> u8 {
    min(priority_mask, running_priority(lrs))
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
            vtls: VtlArray::from_fn(|_| CcaVtl::new()),
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
        let vtl = this.backing.cvm.exit_vtl;

        // Run the CCA plane.
        // This will return when the plane exits.
        let has_plane_exit = this
            .runner
            .run_cca_plane()
            .map_err(|e| dev.fatal_error(CcaRunVpError(e).into()))?;

        if has_plane_exit {
            // Preserve the plane context, so we can restore it later.
            this.preserve_plane_context(vtl);

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
                            let value = cca_exit
                                .gpr_or_zero_register(iss.rt())
                                .expect("ISS Rt is a valid AArch64 register index");
                            this.handle_system_register_trap(vtl, iss, value, cca_exit.0.esr_el2)
                                .map_err(|e| dev.fatal_error(e.into()))?;
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
                    this.request_asserted_local_interrupts(vtl, irq_exit)
                        .map_err(|e| dev.fatal_error(e.into()))?;
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
        scan_irr: VtlArray<bool, 2>,
        first_scan_irr: &mut bool,
        dev: &impl CpuIo,
    ) -> bool {
        let _ = dev;
        // RSI exposes one LR bank for the plane that most recently exited.
        // Fold and repack only that plane; polling both VTLs would fold the
        // same returned cache twice and mix their software overflow tails.
        let vtl = this.backing.cvm.exit_vtl;
        Self::poll_interrupt_controller(this, vtl, scan_irr[vtl] || *first_scan_irr);
        *first_scan_irr = false;
        false
    }

    fn poll_interrupt_controller(this: &mut UhProcessor<'_, Self>, vtl: GuestVtl, _scan_irr: bool) {
        this.poll_gic(vtl);
    }

    fn request_extint_readiness(_this: &mut UhProcessor<'_, Self>) {
        unreachable!("extint managed through software apic")
    }

    fn request_untrusted_sint_readiness(_this: &mut UhProcessor<'_, Self>, _sints: u16) {
        // TODO: CCA: handle this for CCA untrusted synic
        unimplemented!();
    }

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

    /// Records interrupt sources reported by a CCA local IRQ exit.
    ///
    /// Trapped ICC SGI writes are emulated through the software GIC. Otherwise,
    /// an asserted virtual timer is recorded as a pending PPI for this VP.
    /// Unsupported system-register traps are returned to the caller as an
    /// error; unrecognized local interrupt sources are ignored after tracing.
    fn request_asserted_local_interrupts(
        &mut self,
        vtl: GuestVtl,
        irq_exit: CcaLocalInterruptExit,
    ) -> Result<(), CcaUnsupportedExit> {
        if irq_exit.gic_maintenance_status != 0 {
            tracing::trace!(
                misr = irq_exit.gic_maintenance_status,
                "CCA virtual GIC maintenance exit"
            );
        }

        if let Some((iss, value, esr_el2)) = irq_exit.system_register_trap {
            self.handle_system_register_trap(vtl, iss, value, esr_el2)?;
        } else if irq_exit.virtual_timer_asserted {
            let intid = self.shared.virt_timer_ppi;

            if !self.shared.cvm.gic.raise_ppi(self.vp_index(), intid) {
                tracing::trace!(
                    intid,
                    "virtual timer PPI was already pending or VP was invalid"
                );
            }
        } else if irq_exit.gic_maintenance_status == 0 {
            tracing::trace!("CCA IRQ exit had an unrecognized local interrupt source");
        }

        Ok(())
    }

    /// Emulates a trapped write to a supported ICC register.
    ///
    /// SGI generation writes are forwarded to the software GIC. ICC_PMR_EL1
    /// writes update the per-plane priority mask used when selecting pending
    /// interrupts. Successful emulation advances the plane-entry PC past the
    /// trapped instruction; it does not modify the guest GPR state.
    fn handle_system_register_trap(
        &mut self,
        vtl: GuestVtl,
        iss: IssSystem,
        value: u64,
        esr_el2: u64,
    ) -> Result<(), CcaUnsupportedExit> {
        let system_reg = iss.system_reg();

        if iss.direction() {
            return Err(CcaUnsupportedExit::UnsupportedSystemRegister {
                system_reg,
                esr_el2,
            });
        }

        let handled = match system_reg {
            SystemReg::ICC_PMR_EL1 => {
                self.backing.vtls[vtl].priority_mask = value as u8;
                true
            }
            SystemReg::ICC_DIR_EL1 => {
                // TDIR is set whenever active interrupts live outside LRs.
                // DIR only performs deactivation in EOImode == 1; in mode 0,
                // EOIR deactivation is represented by HCR.EOIcount instead.
                if self.backing.vtls[vtl].gic_vmcr & ICH_VMCR_VEOIM != 0 {
                    deactivate_virtual_interrupt(
                        &mut self.runner.cca_rsi_plane_entry().gicv3_lrs[..self.shared.gic_num_lrs],
                        &mut self.backing.vtls[vtl].gic_lr_overflow,
                        value as u32,
                    );
                }
                true
            }
            SystemReg::ICC_SGI0R_EL1 | SystemReg::ICC_SGI1R_EL1 => self
                .shared
                .cvm
                .gic
                .write_sysreg(self.vp_index(), system_reg, value, |target_vp| {
                    tracing::trace!(
                        target_vp,
                        ?system_reg,
                        "GIC sysreg write raised an interrupt"
                    );
                }),
            _ => false,
        };

        if !handled {
            return Err(CcaUnsupportedExit::UnsupportedSystemRegister {
                system_reg,
                esr_el2,
            });
        }

        // The trapped AArch64 instruction has been emulated. Resume at the
        // following 4-byte instruction instead of trapping on this one again.
        self.runner.cca_rsi_plane_entry().pc += 4;

        Ok(())
    }

    /// Folds the returned LR cache and repacks it using KVM's overflow policy.
    ///
    /// Pure-pending interrupts are placed before active entries. Active entries
    /// that no longer fit remain in a software tail and are deactivated through
    /// EOIcount (EOImode 0) or trapped DIR writes (EOImode 1).
    fn poll_gic(&mut self, vtl: GuestVtl) {
        let vp = self.vp_index();
        let gic_num_lrs = self.shared.gic_num_lrs;

        // Merge the returned hardware cache with the software-only tail before
        // folding. In EOImode 0, EOIcount identifies ordered deactivations from
        // that tail. EOImode 1 deactivations are handled by trapped DIR writes.
        let gicv3_hcr = self.runner.cca_rsi_plane_entry().gicv3_hcr;
        let mut overflow = std::mem::take(&mut self.backing.vtls[vtl].gic_lr_overflow);
        if self.backing.vtls[vtl].gic_vmcr & ICH_VMCR_VEOIM == 0 {
            consume_eoi_count(&mut overflow, gic_eoi_count(gicv3_hcr));
        }

        let mut logical_lrs: Vec<u64> = self.runner.cca_rsi_plane_entry().gicv3_lrs[..gic_num_lrs]
            .iter()
            .copied()
            .filter(|lr| lr_is_valid(*lr))
            .collect();
        logical_lrs.extend(overflow);

        let returned = logical_lrs
            .iter()
            .map(|lr| ListRegisterInterrupt {
                intid: (lr & ICH_LR_VINTID_MASK) as u32,
                pending: lr & ICH_LR_PENDING != 0,
                active: lr & ICH_LR_ACTIVE != 0,
            })
            .collect::<Vec<_>>();
        self.shared.cvm.gic.fold_list_registers(vp, &returned);

        // Returned pending state is now durable in the software GIC. Seed the
        // next active/pending list with active entries only; pending state will
        // be selected again below according to priority and PMR.
        let mut candidates = logical_lrs
            .into_iter()
            .filter(|lr| lr_is_active(*lr))
            .map(|lr| lr & !ICH_LR_PENDING)
            .collect::<Vec<_>>();

        loop {
            let priority_threshold =
                interrupt_priority_threshold(self.backing.vtls[vtl].priority_mask, &candidates);
            let Some(interrupt) = self
                .shared
                .cvm
                .gic
                .next_pending_private_interrupt(vp, priority_threshold)
            else {
                break;
            };

            queue_virtual_interrupt(&mut candidates, interrupt);
            self.shared
                .cvm
                .gic
                .mark_private_injected(vp, interrupt.intid);
            tracing::debug!(
                intid = interrupt.intid,
                priority = interrupt.priority,
                group1 = interrupt.group1,
                ?vtl,
                "injected CCA GIC interrupt"
            );
        }

        // Device SPIs belong to VTL0.
        if vtl == GuestVtl::Vtl0 {
            loop {
                let priority_threshold =
                    interrupt_priority_threshold(self.backing.vtls[vtl].priority_mask, &candidates);
                let Some(interrupt) = self
                    .shared
                    .cvm
                    .gic
                    .reserve_pending_spi_interrupt(vp, priority_threshold)
                else {
                    break;
                };

                queue_virtual_interrupt(&mut candidates, interrupt);
                tracing::debug!(
                    intid = interrupt.intid,
                    priority = interrupt.priority,
                    group1 = interrupt.group1,
                    ?vtl,
                    "queued pending CCA shared GIC interrupt"
                );
            }
        }

        sort_gic_candidates(&mut candidates);
        let overflow = if candidates.len() > gic_num_lrs {
            candidates.split_off(gic_num_lrs)
        } else {
            Vec::new()
        };
        let pending_outside_lrs = overflow.iter().any(|lr| lr_is_pending(*lr));
        let active_outside_lrs = overflow.iter().any(|lr| lr_is_active(*lr));
        let any_outside_lrs = !overflow.is_empty();

        let entry = self.runner.cca_rsi_plane_entry();
        entry.gicv3_lrs.fill(0);
        entry.gicv3_lrs[..candidates.len()].copy_from_slice(&candidates);
        configure_gic_maintenance(
            &mut entry.gicv3_hcr,
            pending_outside_lrs,
            active_outside_lrs,
            any_outside_lrs,
        );
        self.backing.vtls[vtl].gic_lr_overflow = overflow;
    }

    // Copy the exit context to the entry context.
    fn preserve_plane_context(&mut self, vtl: GuestVtl) {
        let gic_vmcr = {
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

            // Preserve the virtual GIC state across plane exits. HCR.EOIcount
            // is consumed when the returned LR cache is folded.
            plane_run.entry.gicv3_hcr = plane_run.exit.gicv3_hcr;
            plane_run
                .entry
                .gicv3_lrs
                .copy_from_slice(&plane_run.exit.gicv3_lrs);
            plane_run.exit.gicv3_vmcr
        };
        self.backing.vtls[vtl].gic_vmcr = gic_vmcr;
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

    #[test]
    fn priority_mask_defaults_to_allow_all_priorities() {
        assert_eq!(CcaVtl::new().priority_mask, u8::MAX);
    }

    #[test]
    fn priority_threshold_obeys_pmr_and_running_priority() {
        const ACTIVE_PRIORITY: u64 = 0x40 << ICH_LR_PRIORITY_SHIFT;
        let lrs = [ICH_LR_ACTIVE | ACTIVE_PRIORITY];

        assert_eq!(interrupt_priority_threshold(0x80, &lrs), 0x40);
        assert_eq!(interrupt_priority_threshold(0x20, &lrs), 0x20);
        assert_eq!(interrupt_priority_threshold(0x80, &[0]), 0x80);
    }

    #[test]
    fn lr_count_comes_from_vtr_and_is_capped_by_rsi_capacity() {
        assert_eq!(gic_num_lrs(0), 1);
        assert_eq!(gic_num_lrs(3), 4);
        assert_eq!(gic_num_lrs(15), 16);
        assert_eq!(gic_num_lrs(31), 16);
    }

    #[test]
    fn only_pure_pending_lrs_count_for_npie() {
        assert!(lr_is_pending(ICH_LR_PENDING));
        assert!(!lr_is_pending(ICH_LR_ACTIVE));
        assert!(!lr_is_pending(ICH_LR_ACTIVE | ICH_LR_PENDING));
    }

    #[test]
    fn overflow_recomputes_maintenance_controls() {
        let mut hcr = ICH_HCR_TC | ICH_HCR_LRENPIE | ICH_HCR_EOI_COUNT_MASK;

        configure_gic_maintenance(&mut hcr, true, false, true);

        assert_eq!(
            hcr & (ICH_HCR_UIE | ICH_HCR_NPIE),
            ICH_HCR_UIE | ICH_HCR_NPIE
        );
        assert_eq!(
            hcr & (ICH_HCR_LRENPIE | ICH_HCR_TDIR | ICH_HCR_EOI_COUNT_MASK),
            0
        );
        assert_ne!(hcr & ICH_HCR_TC, 0);

        let mut active_only_hcr = 0;
        configure_gic_maintenance(&mut active_only_hcr, false, true, true);
        assert_ne!(active_only_hcr & ICH_HCR_UIE, 0);
        assert_eq!(active_only_hcr & ICH_HCR_NPIE, 0);
        assert_eq!(
            active_only_hcr & (ICH_HCR_LRENPIE | ICH_HCR_TDIR),
            ICH_HCR_LRENPIE | ICH_HCR_TDIR
        );

        configure_gic_maintenance(&mut hcr, false, false, false);
        assert_eq!(
            hcr & (ICH_HCR_UIE | ICH_HCR_NPIE | ICH_HCR_LRENPIE | ICH_HCR_TDIR),
            0
        );
        assert_ne!(hcr & ICH_HCR_TC, 0);
    }

    #[test]
    fn pure_pending_candidates_sort_before_active_candidates() {
        let mut lrs = [
            ICH_LR_ACTIVE | (0x20 << ICH_LR_PRIORITY_SHIFT) | 1,
            ICH_LR_PENDING | (0x80 << ICH_LR_PRIORITY_SHIFT) | 2,
            ICH_LR_PENDING | (0x40 << ICH_LR_PRIORITY_SHIFT) | 3,
        ];

        sort_gic_candidates(&mut lrs);

        assert_eq!(lrs.map(|lr| lr & ICH_LR_VINTID_MASK), [3, 2, 1]);
    }

    #[test]
    fn eoi_count_deactivates_ordered_active_overflow() {
        let mut overflow = vec![ICH_LR_PENDING | 1, ICH_LR_ACTIVE | 2, ICH_LR_ACTIVE | 3];

        consume_eoi_count(&mut overflow, 1);

        assert_eq!(
            overflow
                .iter()
                .map(|lr| lr & ICH_LR_VINTID_MASK)
                .collect::<Vec<_>>(),
            [1, 3]
        );
    }

    #[test]
    fn queueing_an_active_interrupt_marks_it_pending() {
        const INTID: u32 = 7;
        let mut candidates = vec![ICH_LR_ACTIVE | u64::from(INTID)];
        let interrupt = |intid| PendingInterrupt {
            intid,
            priority: 0x80,
            group1: true,
        };

        queue_virtual_interrupt(&mut candidates, interrupt(INTID));

        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0] & ICH_LR_STATE_MASK,
            ICH_LR_ACTIVE | ICH_LR_PENDING
        );
    }

    #[test]
    fn trapped_dir_deactivates_resident_and_overflow_interrupts() {
        let mut lrs = [ICH_LR_ACTIVE | 1, ICH_LR_ACTIVE | ICH_LR_PENDING | 2];
        let mut overflow = vec![ICH_LR_ACTIVE | 3];

        deactivate_virtual_interrupt(&mut lrs, &mut overflow, 1);
        deactivate_virtual_interrupt(&mut lrs, &mut overflow, 3);

        assert_eq!(lrs[0], 0);
        assert_eq!(lrs[1] & ICH_LR_STATE_MASK, ICH_LR_ACTIVE | ICH_LR_PENDING);
        assert!(overflow.is_empty());
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
