// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Processor support for SNP partitions.

use super::BackingParams;
use super::BackingPrivate;
use super::BackingSharedParams;
use super::HardwareIsolatedBacking;
use super::InterceptMessageOptionalState;
use super::InterceptMessageState;
use super::UhEmulationState;
use super::hardware_cvm;
use super::hardware_cvm::HardwareIsolatedGuestTimer;
use super::vp_state;
use super::vp_state::UhVpStateAccess;
use crate::BackingShared;
use crate::Error;
use crate::GuestVtl;
use crate::TlbFlushLockAccess;
use crate::UhCvmPartitionState;
use crate::UhCvmVpState;
use crate::UhPartitionInner;
use crate::UhPartitionNewParams;
use crate::WakeReason;
use crate::processor::UhHypercallHandler;
use crate::processor::UhProcessor;
use crate::processor::hardware_cvm::apic::ApicBacking;
use cvm_tracing::CVM_ALLOWED;
use cvm_tracing::CVM_CONFIDENTIAL;
use hcl::vmsa::VmsaWrapper;
use hv1_emulator::hv::ProcessorVtlHv;
use hv1_emulator::synic::ProcessorSynic;
use hv1_hypercall::HvRepResult;
use hv1_hypercall::HypercallIo;
use hv1_structs::ProcessorSet;
use hv1_structs::VtlArray;
use hvdef::HV_PAGE_SIZE;
use hvdef::HvDeliverabilityNotificationsRegister;
use hvdef::HvError;
use hvdef::HvMessageType;
use hvdef::HvX64PendingExceptionEvent;
use hvdef::HvX64RegisterName;
use hvdef::Vtl;
use hvdef::hypercall::Control;
use hvdef::hypercall::HvFlushFlags;
use hvdef::hypercall::HvGvaRange;
use hvdef::hypercall::HypercallOutput;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use virt::EmulatorMonitorSupport;
use virt::Processor;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::io::CpuIo;
use virt::state::StateElement;
use virt::vp;
use virt::vp::AccessVpState;
use virt::vp::MpState;
use virt::x86::MsrError;
use virt::x86::MsrErrorExt;
use virt::x86::SegmentRegister;
use virt::x86::TableRegister;
use virt_support_apic::ApicClient;
use virt_support_x86emu::emulate::EmulatorSupport as X86EmulatorSupport;
use virt_support_x86emu::emulate::emulate_io;
use virt_support_x86emu::emulate::emulate_translate_gva;
use virt_support_x86emu::translate::TranslationRegisters;
use vmcore::vmtime::VmTimeAccess;
use x86defs::RFlags;
use x86defs::cpuid::CpuidFunction;
use x86defs::snp::SevEventInjectInfo;
use x86defs::snp::SevExitCode;
use x86defs::snp::SevInvlpgbEcx;
use x86defs::snp::SevInvlpgbEdx;
use x86defs::snp::SevInvlpgbRax;
use x86defs::snp::SevIoAccessInfo;
use x86defs::snp::SevNpfInfo;
use x86defs::snp::SevSelector;
use x86defs::snp::SevStatusMsr;
use x86defs::snp::SevVmsa;
use x86defs::snp::Vmpl;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

#[derive(Debug, Error)]
#[error("invalid vmcb")]
struct InvalidVmcb;

#[derive(Debug, Error)]
enum SnpGhcbError {
    #[error("failed to access GHCB page")]
    GhcbPageAccess(#[source] guestmem::GuestMemoryError),
    #[error("ghcb page used for vmgexit does not match overlay page")]
    GhcbMisconfiguration,
}

#[derive(Debug, Error)]
#[error("failed to run")]
struct SnpRunVpError(#[source] hcl::ioctl::Error);

/// A backing for SNP partitions.
#[derive(InspectMut)]
pub struct SnpBacked {
    #[inspect(hex)]
    hv_sint_notifications: u16,
    general_stats: VtlArray<GeneralStats, 2>,
    exit_stats: VtlArray<ExitStats, 2>,
    #[inspect(flatten)]
    cvm: UhCvmVpState,
}

#[derive(Inspect, Default)]
struct GeneralStats {
    guest_busy: Counter,
    int_ack: Counter,
    synth_int: Counter,
}

#[derive(Inspect, Default)]
struct ExitStats {
    automatic_exit: Counter,
    cpuid: Counter,
    hlt: Counter,
    intr: Counter,
    invd: Counter,
    invlpgb: Counter,
    ioio: Counter,
    msr_read: Counter,
    msr_write: Counter,
    npf: Counter,
    npf_no_intercept: Counter,
    npf_spurious: Counter,
    rdpmc: Counter,
    vmgexit: Counter,
    vmmcall: Counter,
    xsetbv: Counter,
    excp_db: Counter,
    secure_reg_write: Counter,
}

enum UhDirectOverlay {
    Sipp,
    Sifp,
    Ghcb,
    Count,
}

impl SnpBacked {
    // Fix up the efer value to have the correct long mode flags and SVM flag
    fn calculate_efer(efer: u64, cr0: u64) -> u64 {
        let new_efer = if efer & x86defs::X64_EFER_LME != 0 && cr0 & x86defs::X64_CR0_PG != 0 {
            efer | x86defs::X64_EFER_LMA
        } else {
            efer & !x86defs::X64_EFER_LMA
        };
        new_efer | x86defs::X64_EFER_SVME
    }

    /// Gets the number of pages that will be allocated from the shared page pool
    /// for each CPU.
    pub fn shared_pages_required_per_cpu() -> u64 {
        UhDirectOverlay::Count as u64
    }
}

impl HardwareIsolatedBacking for SnpBacked {
    fn cvm_state(&self) -> &UhCvmVpState {
        &self.cvm
    }

    fn cvm_state_mut(&mut self) -> &mut UhCvmVpState {
        &mut self.cvm
    }

    fn cvm_partition_state(shared: &Self::Shared) -> &UhCvmPartitionState {
        &shared.cvm
    }

    fn switch_vtl(this: &mut UhProcessor<'_, Self>, source_vtl: GuestVtl, target_vtl: GuestVtl) {
        let [vmsa0, vmsa1] = this.runner.vmsas_mut();
        let (current_vmsa, mut target_vmsa) = match (source_vtl, target_vtl) {
            (GuestVtl::Vtl0, GuestVtl::Vtl1) => (vmsa0, vmsa1),
            (GuestVtl::Vtl1, GuestVtl::Vtl0) => (vmsa1, vmsa0),
            _ => unreachable!(),
        };

        target_vmsa.set_rax(current_vmsa.rax());
        target_vmsa.set_rbx(current_vmsa.rbx());
        target_vmsa.set_rcx(current_vmsa.rcx());
        target_vmsa.set_rdx(current_vmsa.rdx());
        target_vmsa.set_rbp(current_vmsa.rbp());
        target_vmsa.set_rsi(current_vmsa.rsi());
        target_vmsa.set_rdi(current_vmsa.rdi());
        target_vmsa.set_r8(current_vmsa.r8());
        target_vmsa.set_r9(current_vmsa.r9());
        target_vmsa.set_r10(current_vmsa.r10());
        target_vmsa.set_r11(current_vmsa.r11());
        target_vmsa.set_r12(current_vmsa.r12());
        target_vmsa.set_r13(current_vmsa.r13());
        target_vmsa.set_r14(current_vmsa.r14());
        target_vmsa.set_r15(current_vmsa.r15());
        target_vmsa.set_xcr0(current_vmsa.xcr0());

        target_vmsa.set_cr2(current_vmsa.cr2());

        // DR6 not shared on AMD
        target_vmsa.set_dr0(current_vmsa.dr0());
        target_vmsa.set_dr1(current_vmsa.dr1());
        target_vmsa.set_dr2(current_vmsa.dr2());
        target_vmsa.set_dr3(current_vmsa.dr3());

        target_vmsa.set_pl0_ssp(current_vmsa.pl0_ssp());
        target_vmsa.set_pl1_ssp(current_vmsa.pl1_ssp());
        target_vmsa.set_pl2_ssp(current_vmsa.pl2_ssp());
        target_vmsa.set_pl3_ssp(current_vmsa.pl3_ssp());
        target_vmsa.set_u_cet(current_vmsa.u_cet());

        target_vmsa.set_x87_registers(&current_vmsa.x87_registers());

        let vec_reg_count = 16;
        for i in 0..vec_reg_count {
            target_vmsa.set_xmm_registers(i, current_vmsa.xmm_registers(i));
            target_vmsa.set_ymm_registers(i, current_vmsa.ymm_registers(i));
        }

        this.backing.cvm_state_mut().exit_vtl = target_vtl;
    }

    fn translation_registers(
        &self,
        this: &UhProcessor<'_, Self>,
        vtl: GuestVtl,
    ) -> TranslationRegisters {
        let vmsa = this.runner.vmsa(vtl);
        TranslationRegisters {
            cr0: vmsa.cr0(),
            cr4: vmsa.cr4(),
            efer: vmsa.efer(),
            cr3: vmsa.cr3(),
            rflags: vmsa.rflags(),
            ss: virt_seg_from_snp(vmsa.ss()).into(),
            encryption_mode: virt_support_x86emu::translate::EncryptionMode::Vtom(
                this.partition.caps.vtom.unwrap(),
            ),
        }
    }

    fn tlb_flush_lock_access<'a>(
        vp_index: Option<VpIndex>,
        partition: &'a UhPartitionInner,
        shared: &'a Self::Shared,
    ) -> impl TlbFlushLockAccess + 'a {
        SnpTlbLockFlushAccess {
            vp_index,
            partition,
            shared,
        }
    }

    fn pending_event_vector(this: &UhProcessor<'_, Self>, vtl: GuestVtl) -> Option<u8> {
        let event_inject = this.runner.vmsa(vtl).event_inject();
        if event_inject.valid() {
            Some(event_inject.vector())
        } else {
            None
        }
    }

    fn set_pending_exception(
        this: &mut UhProcessor<'_, Self>,
        vtl: GuestVtl,
        event: HvX64PendingExceptionEvent,
    ) {
        let inject_info = SevEventInjectInfo::new()
            .with_valid(true)
            .with_deliver_error_code(event.deliver_error_code())
            .with_error_code(event.error_code())
            .with_vector(event.vector().try_into().unwrap())
            .with_interruption_type(x86defs::snp::SEV_INTR_TYPE_EXCEPT);

        this.runner.vmsa_mut(vtl).set_event_inject(inject_info);
    }

    fn cr0(this: &UhProcessor<'_, Self>, vtl: GuestVtl) -> u64 {
        this.runner.vmsa(vtl).cr0()
    }

    fn cr4(this: &UhProcessor<'_, Self>, vtl: GuestVtl) -> u64 {
        this.runner.vmsa(vtl).cr4()
    }

    fn intercept_message_state(
        this: &UhProcessor<'_, Self>,
        vtl: GuestVtl,
        include_optional_state: bool,
    ) -> InterceptMessageState {
        let vmsa = this.runner.vmsa(vtl);

        // next_rip may not be set properly for NPFs, so don't read it.
        let instr_len = if SevExitCode(vmsa.guest_error_code()) == SevExitCode::NPF {
            0
        } else {
            (vmsa.next_rip() - vmsa.rip()) as u8
        };

        InterceptMessageState {
            instruction_length_and_cr8: instr_len,
            cpl: vmsa.cpl(),
            efer_lma: vmsa.efer() & x86defs::X64_EFER_LMA != 0,
            cs: virt_seg_from_snp(vmsa.cs()).into(),
            rip: vmsa.rip(),
            rflags: vmsa.rflags(),
            rax: vmsa.rax(),
            rdx: vmsa.rdx(),
            optional: if include_optional_state {
                Some(InterceptMessageOptionalState {
                    ds: virt_seg_from_snp(vmsa.ds()).into(),
                    es: virt_seg_from_snp(vmsa.es()).into(),
                })
            } else {
                None
            },
            rcx: vmsa.rcx(),
            rsi: vmsa.rsi(),
            rdi: vmsa.rdi(),
        }
    }

    fn cr_intercept_registration(
        this: &mut UhProcessor<'_, Self>,
        intercept_control: hvdef::HvRegisterCrInterceptControl,
    ) {
        // Intercept control is always managed by the hypervisor, so any request
        // here is only opportunistic. Make the request directly with the
        // hypervisor. Since intercept control always applies to VTL 1 control of
        // VTL 0 state, the VTL 1 intercept control register is set here.
        this.runner
            .set_vp_registers_hvcall(
                Vtl::Vtl1,
                [(
                    HvX64RegisterName::CrInterceptControl,
                    u64::from(intercept_control),
                )],
            )
            .expect("setting intercept control succeeds");
    }

    fn is_interrupt_pending(
        this: &mut UhProcessor<'_, Self>,
        vtl: GuestVtl,
        check_rflags: bool,
        dev: &impl CpuIo,
    ) -> bool {
        let vmsa = this.runner.vmsa_mut(vtl);
        if vmsa.event_inject().valid()
            && vmsa.event_inject().interruption_type() == x86defs::snp::SEV_INTR_TYPE_NMI
        {
            return true;
        }

        let vmsa_priority = vmsa.v_intr_cntrl().priority() as u32;
        let lapic = &mut this.backing.cvm.lapics[vtl].lapic;
        let ppr = lapic
            .access(&mut SnpApicClient {
                partition: this.partition,
                vmsa,
                dev,
                vmtime: &this.vmtime,
                vtl,
            })
            .get_ppr();
        let ppr_priority = ppr >> 4;
        if vmsa_priority <= ppr_priority {
            return false;
        }

        let vmsa = this.runner.vmsa_mut(vtl);
        if (check_rflags && !RFlags::from_bits(vmsa.rflags()).interrupt_enable())
            || vmsa.v_intr_cntrl().intr_shadow()
            || !vmsa.v_intr_cntrl().irq()
        {
            return false;
        }

        true
    }

    fn untrusted_synic_mut(&mut self) -> Option<&mut ProcessorSynic> {
        None
    }

    fn update_deadline(this: &mut UhProcessor<'_, Self>, ref_time_now: u64, next_ref_time: u64) {
        this.shared
            .guest_timer
            .update_deadline(this, ref_time_now, next_ref_time);
    }

    fn clear_deadline(this: &mut UhProcessor<'_, Self>) {
        this.shared.guest_timer.clear_deadline(this);
    }
}

/// Partition-wide shared data for SNP VPs.
#[derive(Inspect)]
pub struct SnpBackedShared {
    #[inspect(flatten)]
    pub(crate) cvm: UhCvmPartitionState,
    invlpgb_count_max: u16,
    tsc_aux_virtualized: bool,
    #[inspect(debug)]
    sev_status: SevStatusMsr,
    /// Accessor for managing lower VTL timer deadlines.
    #[inspect(skip)]
    guest_timer: hardware_cvm::VmTimeGuestTimer,
}

impl SnpBackedShared {
    pub(crate) fn new(
        _partition_params: &UhPartitionNewParams<'_>,
        params: BackingSharedParams<'_>,
    ) -> Result<Self, Error> {
        let cvm = params.cvm_state.unwrap();
        let invlpgb_count_max = x86defs::cpuid::ExtendedAddressSpaceSizesEdx::from(
            params
                .cpuid
                .result(CpuidFunction::ExtendedAddressSpaceSizes.0, 0, &[0; 4])[3],
        )
        .invlpgb_count_max();
        let tsc_aux_virtualized = x86defs::cpuid::ExtendedSevFeaturesEax::from(
            params
                .cpuid
                .result(CpuidFunction::ExtendedSevFeatures.0, 0, &[0; 4])[0],
        )
        .tsc_aux_virtualization();

        // Query the SEV_FEATURES MSR to determine the features enabled on VTL2's VMSA
        // and use that to set btb_isolation, prevent_host_ibs, and VMSA register protection.
        let msr = crate::MsrDevice::new(0).expect("open msr");
        let sev_status =
            SevStatusMsr::from(msr.read_msr(x86defs::X86X_AMD_MSR_SEV).expect("read msr"));
        tracing::info!(CVM_ALLOWED, ?sev_status, "SEV status");

        // Configure timer interface for lower VTLs.
        let guest_timer = hardware_cvm::VmTimeGuestTimer;

        Ok(Self {
            sev_status,
            invlpgb_count_max,
            tsc_aux_virtualized,
            cvm,
            guest_timer,
        })
    }
}

#[expect(private_interfaces)]
impl BackingPrivate for SnpBacked {
    type HclBacking<'snp> = hcl::ioctl::snp::Snp<'snp>;
    type Shared = SnpBackedShared;
    type EmulationCache = ();

    fn shared(shared: &BackingShared) -> &Self::Shared {
        let BackingShared::Snp(shared) = shared else {
            unreachable!()
        };
        shared
    }

    fn new(params: BackingParams<'_, '_, Self>, shared: &SnpBackedShared) -> Result<Self, Error> {
        Ok(Self {
            hv_sint_notifications: 0,
            general_stats: VtlArray::from_fn(|_| Default::default()),
            exit_stats: VtlArray::from_fn(|_| Default::default()),
            cvm: UhCvmVpState::new(
                &shared.cvm,
                params.partition,
                params.vp_info,
                UhDirectOverlay::Count as usize,
            )?,
        })
    }

    fn init(this: &mut UhProcessor<'_, Self>) {
        let sev_status = this.vp().shared.sev_status;
        for vtl in [GuestVtl::Vtl0, GuestVtl::Vtl1] {
            init_vmsa(
                &mut this.runner.vmsa_mut(vtl),
                vtl,
                this.partition.caps.vtom,
                sev_status,
            );

            // Reset VMSA-backed state.
            let registers = vp::Registers::at_reset(&this.partition.caps, &this.inner.vp_info);
            this.access_state(vtl.into())
                .set_registers(&registers)
                .expect("Resetting to architectural state should succeed");

            let debug_registers =
                vp::DebugRegisters::at_reset(&this.partition.caps, &this.inner.vp_info);

            this.access_state(vtl.into())
                .set_debug_regs(&debug_registers)
                .expect("Resetting to architectural state should succeed");

            let xcr0 = vp::Xcr0::at_reset(&this.partition.caps, &this.inner.vp_info);
            this.access_state(vtl.into())
                .set_xcr(&xcr0)
                .expect("Resetting to architectural state should succeed");

            let cache_control = vp::Mtrrs::at_reset(&this.partition.caps, &this.inner.vp_info);
            this.access_state(vtl.into())
                .set_mtrrs(&cache_control)
                .expect("Resetting to architectural state should succeed");
        }

        // Configure the synic direct overlays.
        // So far, only VTL 0 is using these (for VMBus).
        let pfns = &this.backing.cvm.direct_overlay_handle.pfns();
        let values: &[(HvX64RegisterName, u64); 3] = &[
            (
                HvX64RegisterName::Sipp,
                hvdef::HvSynicSimpSiefp::new()
                    .with_enabled(true)
                    .with_base_gpn(pfns[UhDirectOverlay::Sipp as usize])
                    .into(),
            ),
            (
                HvX64RegisterName::Sifp,
                hvdef::HvSynicSimpSiefp::new()
                    .with_enabled(true)
                    .with_base_gpn(pfns[UhDirectOverlay::Sifp as usize])
                    .into(),
            ),
            (
                HvX64RegisterName::Ghcb,
                x86defs::snp::GhcbMsr::new()
                    .with_info(x86defs::snp::GhcbInfo::REGISTER_REQUEST.0)
                    .with_pfn(pfns[UhDirectOverlay::Ghcb as usize])
                    .into(),
            ),
        ];

        this.runner
            .set_vp_registers_hvcall(Vtl::Vtl0, values)
            .expect("set_vp_registers hypercall for direct overlays should succeed");
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

    async fn run_vp(
        this: &mut UhProcessor<'_, Self>,
        dev: &impl CpuIo,
        _stop: &mut virt::StopVp<'_>,
    ) -> Result<(), VpHaltReason> {
        this.run_vp_snp(dev).await
    }

    fn poll_apic(this: &mut UhProcessor<'_, Self>, vtl: GuestVtl, scan_irr: bool) {
        // Clear any pending interrupt.
        this.runner.vmsa_mut(vtl).v_intr_cntrl_mut().set_irq(false);

        hardware_cvm::apic::poll_apic_core(this, vtl, scan_irr)
    }

    fn request_extint_readiness(_this: &mut UhProcessor<'_, Self>) {
        unreachable!("extint managed through software apic")
    }

    fn request_untrusted_sint_readiness(this: &mut UhProcessor<'_, Self>, sints: u16) {
        let sints = this.backing.hv_sint_notifications | sints;
        if this.backing.hv_sint_notifications == sints {
            return;
        }
        let notifications = HvDeliverabilityNotificationsRegister::new().with_sints(sints);
        tracing::trace!(?notifications, "setting notifications");
        this.runner
            .set_vp_register(
                GuestVtl::Vtl0,
                HvX64RegisterName::DeliverabilityNotifications,
                u64::from(notifications).into(),
            )
            .expect("requesting deliverability is not a fallable operation");

        this.backing.hv_sint_notifications = sints;
    }

    fn inspect_extra(this: &mut UhProcessor<'_, Self>, resp: &mut inspect::Response<'_>) {
        let vtl0_vmsa = this.runner.vmsa(GuestVtl::Vtl0);
        let vtl1_vmsa = if this.backing.cvm_state().vtl1.is_some() {
            Some(this.runner.vmsa(GuestVtl::Vtl1))
        } else {
            None
        };

        let add_vmsa_inspect = |req: inspect::Request<'_>, vmsa: VmsaWrapper<'_, &SevVmsa>| {
            req.respond()
                .hex("guest_error_code", vmsa.guest_error_code())
                .hex("exit_info1", vmsa.exit_info1())
                .hex("exit_info2", vmsa.exit_info2())
                .hex("v_intr_cntrl", u64::from(vmsa.v_intr_cntrl()));
        };

        resp.child("vmsa_additional", |req| {
            req.respond()
                .child("vtl0", |inner_req| add_vmsa_inspect(inner_req, vtl0_vmsa))
                .child("vtl1", |inner_req| {
                    if let Some(vtl1_vmsa) = vtl1_vmsa {
                        add_vmsa_inspect(inner_req, vtl1_vmsa);
                    }
                });
        });
    }

    fn hv(&self, vtl: GuestVtl) -> Option<&ProcessorVtlHv> {
        Some(&self.cvm.hv[vtl])
    }

    fn hv_mut(&mut self, vtl: GuestVtl) -> Option<&mut ProcessorVtlHv> {
        Some(&mut self.cvm.hv[vtl])
    }

    fn handle_vp_start_enable_vtl_wake(this: &mut UhProcessor<'_, Self>, vtl: GuestVtl) {
        this.hcvm_handle_vp_start_enable_vtl(vtl)
    }

    fn vtl1_inspectable(this: &UhProcessor<'_, Self>) -> bool {
        this.hcvm_vtl1_inspectable()
    }

    fn process_interrupts(
        this: &mut UhProcessor<'_, Self>,
        scan_irr: VtlArray<bool, 2>,
        first_scan_irr: &mut bool,
        dev: &impl CpuIo,
    ) -> bool {
        this.cvm_process_interrupts(scan_irr, first_scan_irr, dev)
    }
}

fn virt_seg_to_snp(val: SegmentRegister) -> SevSelector {
    SevSelector {
        selector: val.selector,
        attrib: (val.attributes & 0xFF) | ((val.attributes >> 4) & 0xF00),
        limit: val.limit,
        base: val.base,
    }
}

fn virt_table_to_snp(val: TableRegister) -> SevSelector {
    SevSelector {
        limit: val.limit as u32,
        base: val.base,
        ..FromZeros::new_zeroed()
    }
}

fn virt_seg_from_snp(selector: SevSelector) -> SegmentRegister {
    SegmentRegister {
        base: selector.base,
        limit: selector.limit,
        selector: selector.selector,
        attributes: (selector.attrib & 0xFF) | ((selector.attrib & 0xF00) << 4),
    }
}

fn virt_table_from_snp(selector: SevSelector) -> TableRegister {
    TableRegister {
        limit: selector.limit as u16,
        base: selector.base,
    }
}

fn init_vmsa(
    vmsa: &mut VmsaWrapper<'_, &mut SevVmsa>,
    vtl: GuestVtl,
    vtom: Option<u64>,
    sev_status: SevStatusMsr,
) {
    // BUGBUG: this isn't fully accurate--the hypervisor can try running
    // from this at any time, so we need to be careful to set the field
    // that makes this valid last.
    vmsa.reset(sev_status.vmsa_reg_prot());
    vmsa.sev_features_mut()
        .set_snp_btb_isolation(sev_status.snp_btb_isolation());
    vmsa.sev_features_mut()
        .set_ibpb_on_entry(sev_status.ibpb_on_entry());
    vmsa.sev_features_mut()
        .set_prevent_host_ibs(sev_status.prevent_host_ibs());
    vmsa.sev_features_mut()
        .set_vmsa_reg_prot(sev_status.vmsa_reg_prot());
    vmsa.sev_features_mut().set_snp(true);
    vmsa.sev_features_mut().set_vtom(vtom.is_some());
    vmsa.set_virtual_tom(vtom.unwrap_or(0));

    // Enable alternate injection and VC reflection to enable the paravisor to
    // handle injection and intercepts using trustworthy information.
    vmsa.sev_features_mut().set_alternate_injection(true);
    vmsa.sev_features_mut().set_reflect_vc(true);
    vmsa.v_intr_cntrl_mut().set_guest_busy(true);
    vmsa.sev_features_mut().set_debug_swap(true);

    let vmpl = match vtl {
        GuestVtl::Vtl0 => Vmpl::Vmpl2,
        GuestVtl::Vtl1 => Vmpl::Vmpl1,
    };
    vmsa.set_vmpl(vmpl.into());

    // Mark the VMSA with a benign exit code so that any attempt to process intercepts prior
    // to VM execution will not result in erroneous intercept delivery.
    vmsa.set_guest_error_code(SevExitCode::INTR.0);

    // Efer has a value that is different than the architectural default (for SNP, efer
    // must always have the SVME bit set).
    vmsa.set_efer(x86defs::X64_EFER_SVME);
}

struct SnpApicClient<'a, T> {
    partition: &'a UhPartitionInner,
    vmsa: VmsaWrapper<'a, &'a mut SevVmsa>,
    dev: &'a T,
    vmtime: &'a VmTimeAccess,
    vtl: GuestVtl,
}

impl<T: CpuIo> ApicClient for SnpApicClient<'_, T> {
    fn cr8(&mut self) -> u32 {
        self.vmsa.v_intr_cntrl().tpr().into()
    }

    fn set_cr8(&mut self, value: u32) {
        self.vmsa.v_intr_cntrl_mut().set_tpr(value as u8);
    }

    fn set_apic_base(&mut self, _value: u64) {
        // No-op--the APIC base is stored in the APIC itself.
    }

    fn wake(&mut self, vp_index: VpIndex) {
        self.partition.vps[vp_index.index() as usize].wake(self.vtl, WakeReason::INTCON);
    }

    fn eoi(&mut self, vector: u8) {
        debug_assert_eq!(self.vtl, GuestVtl::Vtl0);
        self.dev.handle_eoi(vector.into())
    }

    fn now(&mut self) -> vmcore::vmtime::VmTime {
        self.vmtime.now()
    }

    fn pull_offload(&mut self) -> ([u32; 8], [u32; 8]) {
        unreachable!()
    }
}

impl<T: CpuIo> UhHypercallHandler<'_, '_, T, SnpBacked> {
    // Trusted hypercalls from the guest.
    const TRUSTED_DISPATCHER: hv1_hypercall::Dispatcher<Self> = hv1_hypercall::dispatcher!(
        Self,
        [
            hv1_hypercall::HvModifySparseGpaPageHostVisibility,
            hv1_hypercall::HvQuerySparseGpaPageHostVisibility,
            hv1_hypercall::HvX64StartVirtualProcessor,
            hv1_hypercall::HvGetVpIndexFromApicId,
            hv1_hypercall::HvGetVpRegisters,
            hv1_hypercall::HvEnablePartitionVtl,
            hv1_hypercall::HvRetargetDeviceInterrupt,
            hv1_hypercall::HvPostMessage,
            hv1_hypercall::HvSignalEvent,
            hv1_hypercall::HvX64EnableVpVtl,
            hv1_hypercall::HvExtQueryCapabilities,
            hv1_hypercall::HvVtlCall,
            hv1_hypercall::HvVtlReturn,
            hv1_hypercall::HvFlushVirtualAddressList,
            hv1_hypercall::HvFlushVirtualAddressListEx,
            hv1_hypercall::HvFlushVirtualAddressSpace,
            hv1_hypercall::HvFlushVirtualAddressSpaceEx,
            hv1_hypercall::HvSetVpRegisters,
            hv1_hypercall::HvModifyVtlProtectionMask,
            hv1_hypercall::HvX64TranslateVirtualAddress,
            hv1_hypercall::HvSendSyntheticClusterIpi,
            hv1_hypercall::HvSendSyntheticClusterIpiEx,
            hv1_hypercall::HvInstallIntercept,
            hv1_hypercall::HvAssertVirtualInterrupt,
        ],
    );

    // These are untrusted hypercalls from the hypervisor (hopefully originally
    // from the guest). Only allow HvPostMessage and HvSignalEvent.
    const UNTRUSTED_DISPATCHER: hv1_hypercall::Dispatcher<Self> = hv1_hypercall::dispatcher!(
        Self,
        [hv1_hypercall::HvPostMessage, hv1_hypercall::HvSignalEvent],
    );
}

struct GhcbEnlightenedHypercall<'a, 'b, T> {
    handler: UhHypercallHandler<'a, 'b, T, SnpBacked>,
    control: u64,
    output_gpa: u64,
    input_gpa: u64,
    result: u64,
}

impl<'a, 'b, T> hv1_hypercall::AsHandler<UhHypercallHandler<'a, 'b, T, SnpBacked>>
    for &mut GhcbEnlightenedHypercall<'a, 'b, T>
{
    fn as_handler(&mut self) -> &mut UhHypercallHandler<'a, 'b, T, SnpBacked> {
        &mut self.handler
    }
}

impl<T> HypercallIo for GhcbEnlightenedHypercall<'_, '_, T> {
    fn advance_ip(&mut self) {
        // No-op for GHCB hypercall ABI
    }

    fn retry(&mut self, control: u64) {
        // The GHCB ABI does not support automatically retrying hypercalls by
        // updating the control and reissuing the instruction, since doing so
        // would require the hypervisor (the normal implementor of the GHCB
        // hypercall ABI) to be able to control the instruction pointer.
        //
        // Instead, explicitly return `HV_STATUS_TIMEOUT` to indicate that the
        // guest should retry the hypercall after setting `rep_start` to the
        // number of elements processed.
        let control = Control::from(control);
        self.set_result(
            HypercallOutput::from(HvError::Timeout)
                .with_elements_processed(control.rep_start())
                .into(),
        );
    }

    fn control(&mut self) -> u64 {
        self.control
    }

    fn input_gpa(&mut self) -> u64 {
        self.input_gpa
    }

    fn output_gpa(&mut self) -> u64 {
        self.output_gpa
    }

    fn fast_register_pair_count(&mut self) -> usize {
        0
    }

    fn extended_fast_hypercalls_ok(&mut self) -> bool {
        false
    }

    fn fast_input(&mut self, _buf: &mut [[u64; 2]], _output_register_pairs: usize) -> usize {
        unimplemented!("not supported for secure enlightened abi")
    }

    fn fast_output(&mut self, _starting_pair_index: usize, _buf: &[[u64; 2]]) {
        unimplemented!("not supported for secure enlightened abi")
    }

    fn vtl_input(&mut self) -> u64 {
        unimplemented!("not supported for secure enlightened abi")
    }

    fn set_result(&mut self, n: u64) {
        self.result = n;
    }

    fn fast_regs(&mut self, _starting_pair_index: usize, _buf: &mut [[u64; 2]]) {
        unimplemented!("not supported for secure enlightened abi")
    }
}

impl<'b> ApicBacking<'b, SnpBacked> for UhProcessor<'b, SnpBacked> {
    fn vp(&mut self) -> &mut UhProcessor<'b, SnpBacked> {
        self
    }

    fn handle_interrupt(&mut self, vtl: GuestVtl, vector: u8) {
        let mut vmsa = self.runner.vmsa_mut(vtl);
        vmsa.v_intr_cntrl_mut().set_vector(vector);
        vmsa.v_intr_cntrl_mut().set_priority((vector >> 4).into());
        vmsa.v_intr_cntrl_mut().set_ignore_tpr(false);
        vmsa.v_intr_cntrl_mut().set_irq(true);
        self.backing.cvm.lapics[vtl].activity = MpState::Running;
    }

    fn handle_nmi(&mut self, vtl: GuestVtl) {
        // TODO SNP: support virtual NMI injection
        // For now, just inject an NMI and hope for the best.
        // Don't forget to update handle_cross_vtl_interrupts if this code changes.
        let mut vmsa = self.runner.vmsa_mut(vtl);

        // TODO GUEST VSM: Don't inject the NMI if there's already an event
        // pending.

        vmsa.set_event_inject(
            SevEventInjectInfo::new()
                .with_interruption_type(x86defs::snp::SEV_INTR_TYPE_NMI)
                .with_vector(2)
                .with_valid(true),
        );
        self.backing.cvm.lapics[vtl].nmi_pending = false;
        self.backing.cvm.lapics[vtl].activity = MpState::Running;
    }

    fn handle_sipi(&mut self, vtl: GuestVtl, cs: SegmentRegister) {
        let mut vmsa = self.runner.vmsa_mut(vtl);
        vmsa.set_cs(virt_seg_to_snp(cs));
        vmsa.set_rip(0);
        self.backing.cvm.lapics[vtl].activity = MpState::Running;
    }
}

impl UhProcessor<'_, SnpBacked> {
    fn handle_synic_deliverable_exit(&mut self) {
        let message = self
            .runner
            .exit_message()
            .as_message::<hvdef::HvX64SynicSintDeliverableMessage>();

        tracing::trace!(
            deliverable_sints = message.deliverable_sints,
            "sint deliverable"
        );

        self.backing.hv_sint_notifications &= !message.deliverable_sints;

        // These messages are always VTL0, as VTL1 does not own any VMBUS channels.
        self.deliver_synic_messages(GuestVtl::Vtl0, message.deliverable_sints);
    }

    fn handle_vmgexit(
        &mut self,
        dev: &impl CpuIo,
        intercepted_vtl: GuestVtl,
    ) -> Result<(), SnpGhcbError> {
        let message = self
            .runner
            .exit_message()
            .as_message::<hvdef::HvX64VmgexitInterceptMessage>();

        let ghcb_msr = x86defs::snp::GhcbMsr::from(message.ghcb_msr);
        let flags = message.flags;
        let sw_exit_code = message.ghcb_page.standard.sw_exit_code;
        let sw_exit_info1 = message.ghcb_page.standard.sw_exit_info1;
        let sw_exit_info2 = message.ghcb_page.standard.sw_exit_info2;
        tracing::trace!(?ghcb_msr, "vmgexit intercept");

        match x86defs::snp::GhcbInfo(ghcb_msr.info()) {
            x86defs::snp::GhcbInfo::NORMAL => {
                assert!(message.flags.ghcb_page_valid());
                let ghcb_pfn = ghcb_msr.pfn();

                let ghcb_overlay =
                    self.backing.cvm.direct_overlay_handle.pfns()[UhDirectOverlay::Ghcb as usize];

                // TODO SNP: Should allow arbitrary page to be used for GHCB
                if ghcb_pfn != ghcb_overlay {
                    tracelimit::warn_ratelimited!(
                        CVM_ALLOWED,
                        vmgexit_pfn = ghcb_pfn,
                        overlay_pfn = ghcb_overlay,
                        "ghcb page used for vmgexit does not match overlay page"
                    );

                    return Err(SnpGhcbError::GhcbMisconfiguration);
                }

                match x86defs::snp::GhcbUsage(message.ghcb_page.ghcb_usage) {
                    x86defs::snp::GhcbUsage::HYPERCALL => {
                        let guest_memory = &self.shared.cvm.shared_memory;
                        // Read GHCB parameters from guest memory before
                        // dispatching.
                        let overlay_base = ghcb_overlay * HV_PAGE_SIZE;
                        let x86defs::snp::GhcbHypercallParameters {
                            output_gpa,
                            input_control,
                        } = guest_memory
                            .read_plain(
                                overlay_base
                                    + x86defs::snp::GHCB_PAGE_HYPERCALL_PARAMETERS_OFFSET as u64,
                            )
                            .map_err(SnpGhcbError::GhcbPageAccess)?;

                        let mut handler = GhcbEnlightenedHypercall {
                            handler: UhHypercallHandler {
                                vp: self,
                                bus: dev,
                                trusted: false,
                                intercepted_vtl,
                            },
                            control: input_control,
                            output_gpa,
                            input_gpa: overlay_base,
                            result: 0,
                        };

                        UhHypercallHandler::UNTRUSTED_DISPATCHER
                            .dispatch(guest_memory, &mut handler);

                        // Commit the hypercall result outside the dispatcher
                        // incase memory access fails so we can return an
                        // appropriate error.
                        //
                        // Note that we should only be returning this error if
                        // something is catastrophically wrong, as we already
                        // accessed this page earlier to read input parameters.
                        guest_memory
                            .write_at(
                                overlay_base
                                    + x86defs::snp::GHCB_PAGE_HYPERCALL_OUTPUT_OFFSET as u64,
                                handler.result.as_bytes(),
                            )
                            .map_err(SnpGhcbError::GhcbPageAccess)?;
                    }
                    usage => unimplemented!(
                        r#"
                         Invalid ghcb message.
                         usage {usage:?}
                         flags {flags:?}
                         ghcb_msr {ghcb_msr:?}
                         sw_exit_code {sw_exit_code:?}
                         sw_exit_info1 {sw_exit_info1:?}
                         sw_exit_info2 {sw_exit_info2:?}
                    "#
                    ),
                }
            }
            info => unimplemented!("ghcb info {info:?}"),
        }

        Ok(())
    }

    fn handle_msr_access(
        &mut self,
        dev: &impl CpuIo,
        entered_from_vtl: GuestVtl,
        msr: u32,
        is_write: bool,
    ) {
        if is_write && self.cvm_try_protect_msr_write(entered_from_vtl, msr) {
            return;
        }

        let vmsa = self.runner.vmsa_mut(entered_from_vtl);
        let gp = if is_write {
            let value = (vmsa.rax() as u32 as u64) | ((vmsa.rdx() as u32 as u64) << 32);

            let r = self.backing.cvm.lapics[entered_from_vtl]
                .lapic
                .access(&mut SnpApicClient {
                    partition: self.partition,
                    vmsa,
                    dev,
                    vmtime: &self.vmtime,
                    vtl: entered_from_vtl,
                })
                .msr_write(msr, value)
                .or_else_if_unknown(|| self.write_msr_cvm(msr, value, entered_from_vtl))
                .or_else_if_unknown(|| self.write_msr_snp(dev, msr, value, entered_from_vtl));

            match r {
                Ok(()) => false,
                Err(MsrError::Unknown) => {
                    tracing::debug!(msr, value, "unknown cvm msr write");
                    false
                }
                Err(MsrError::InvalidAccess) => true,
            }
        } else {
            let r = self.backing.cvm.lapics[entered_from_vtl]
                .lapic
                .access(&mut SnpApicClient {
                    partition: self.partition,
                    vmsa,
                    dev,
                    vmtime: &self.vmtime,
                    vtl: entered_from_vtl,
                })
                .msr_read(msr)
                .or_else_if_unknown(|| self.read_msr_cvm(msr, entered_from_vtl))
                .or_else_if_unknown(|| self.read_msr_snp(dev, msr, entered_from_vtl));

            let value = match r {
                Ok(v) => Some(v),
                Err(MsrError::Unknown) => {
                    tracing::debug!(msr, "unknown cvm msr read");
                    Some(0)
                }
                Err(MsrError::InvalidAccess) => None,
            };

            if let Some(value) = value {
                let mut vmsa = self.runner.vmsa_mut(entered_from_vtl);
                vmsa.set_rax((value as u32).into());
                vmsa.set_rdx(((value >> 32) as u32).into());
                false
            } else {
                true
            }
        };

        let mut vmsa = self.runner.vmsa_mut(entered_from_vtl);
        if gp {
            vmsa.set_event_inject(
                SevEventInjectInfo::new()
                    .with_interruption_type(x86defs::snp::SEV_INTR_TYPE_EXCEPT)
                    .with_vector(x86defs::Exception::GENERAL_PROTECTION_FAULT.0)
                    .with_deliver_error_code(true)
                    .with_valid(true),
            );
        } else {
            advance_to_next_instruction(&mut vmsa);
        }
    }

    fn handle_xsetbv(&mut self, entered_from_vtl: GuestVtl) {
        let vmsa = self.runner.vmsa(entered_from_vtl);
        if let Some(value) = hardware_cvm::validate_xsetbv_exit(hardware_cvm::XsetbvExitInput {
            rax: vmsa.rax(),
            rcx: vmsa.rcx(),
            rdx: vmsa.rdx(),
            cr4: vmsa.cr4(),
            cpl: vmsa.cpl(),
        }) {
            if !self.cvm_try_protect_secure_register_write(
                entered_from_vtl,
                HvX64RegisterName::Xfem,
                value,
            ) {
                let mut vmsa = self.runner.vmsa_mut(entered_from_vtl);
                vmsa.set_xcr0(value);
                advance_to_next_instruction(&mut vmsa);
            }
        } else {
            let mut vmsa = self.runner.vmsa_mut(entered_from_vtl);
            vmsa.set_event_inject(
                SevEventInjectInfo::new()
                    .with_interruption_type(x86defs::snp::SEV_INTR_TYPE_EXCEPT)
                    .with_vector(x86defs::Exception::GENERAL_PROTECTION_FAULT.0)
                    .with_deliver_error_code(true)
                    .with_valid(true),
            );
        }
    }

    fn handle_crx_intercept(&mut self, entered_from_vtl: GuestVtl, reg: HvX64RegisterName) {
        let vmsa = self.runner.vmsa(entered_from_vtl);
        let mov_crx_drx = x86defs::snp::MovCrxDrxInfo::from(vmsa.exit_info1());
        let reg_value = {
            let gpr_name =
                HvX64RegisterName(HvX64RegisterName::Rax.0 + mov_crx_drx.gpr_number() as u32);

            match gpr_name {
                HvX64RegisterName::Rax => vmsa.rax(),
                HvX64RegisterName::Rbx => vmsa.rbx(),
                HvX64RegisterName::Rcx => vmsa.rcx(),
                HvX64RegisterName::Rdx => vmsa.rdx(),
                HvX64RegisterName::Rsp => vmsa.rsp(),
                HvX64RegisterName::Rbp => vmsa.rbp(),
                HvX64RegisterName::Rsi => vmsa.rsi(),
                HvX64RegisterName::Rdi => vmsa.rdi(),
                HvX64RegisterName::R8 => vmsa.r8(),
                HvX64RegisterName::R9 => vmsa.r9(),
                HvX64RegisterName::R10 => vmsa.r10(),
                HvX64RegisterName::R11 => vmsa.r11(),
                HvX64RegisterName::R12 => vmsa.r12(),
                HvX64RegisterName::R13 => vmsa.r13(),
                HvX64RegisterName::R14 => vmsa.r14(),
                HvX64RegisterName::R15 => vmsa.r15(),
                _ => unreachable!("unexpected register"),
            }
        };

        // Special case: LMSW/CLTS/SMSW intercepts do not provide decode assist
        // information. No support to emulate these instructions yet, but the
        // access by the guest might be allowed by the higher VTL and therefore
        // crashing is not necessarily the correct behavior.
        //
        // TODO SNP: consider emulating the instruction.
        if !mov_crx_drx.mov_crx() {
            tracelimit::warn_ratelimited!(
                CVM_ALLOWED,
                "Intercepted crx access, instruction is not mov crx"
            );
            return;
        }

        if !self.cvm_try_protect_secure_register_write(entered_from_vtl, reg, reg_value) {
            let mut vmsa = self.runner.vmsa_mut(entered_from_vtl);
            match reg {
                HvX64RegisterName::Cr0 => vmsa.set_cr0(reg_value),
                HvX64RegisterName::Cr4 => vmsa.set_cr4(reg_value),
                _ => unreachable!(),
            }
            advance_to_next_instruction(&mut vmsa);
        }
    }

    #[must_use]
    fn sync_lazy_eoi(&mut self, vtl: GuestVtl) -> bool {
        if self.backing.cvm.lapics[vtl].lapic.is_lazy_eoi_pending() {
            return self.backing.cvm.hv[vtl].set_lazy_eoi();
        }

        false
    }

    async fn run_vp_snp(&mut self, dev: &impl CpuIo) -> Result<(), VpHaltReason> {
        let next_vtl = self.backing.cvm.exit_vtl;

        let mut vmsa = self.runner.vmsa_mut(next_vtl);
        let last_interrupt_ctrl = vmsa.v_intr_cntrl();

        if vmsa.sev_features().alternate_injection() {
            vmsa.v_intr_cntrl_mut().set_guest_busy(false);
        }

        self.unlock_tlb_lock(Vtl::Vtl2);
        let tlb_halt = self.should_halt_for_tlb_unlock(next_vtl);

        let halt = self.backing.cvm.lapics[next_vtl].activity != MpState::Running || tlb_halt;

        if halt && next_vtl == GuestVtl::Vtl1 && !tlb_halt {
            tracelimit::warn_ratelimited!(CVM_ALLOWED, "halting VTL 1, which might halt the guest");
        }

        self.runner.set_halted(halt);

        self.runner.set_exit_vtl(next_vtl);

        // Set the lazy EOI bit just before running.
        let lazy_eoi = self.sync_lazy_eoi(next_vtl);

        let mut has_intercept = self
            .runner
            .run()
            .map_err(|e| dev.fatal_error(SnpRunVpError(e).into()))?;

        let entered_from_vtl = next_vtl;
        let mut vmsa = self.runner.vmsa_mut(entered_from_vtl);

        // TODO SNP: The guest busy bit needs to be tested and set atomically.
        let inject = if vmsa.sev_features().alternate_injection() {
            if vmsa.v_intr_cntrl().guest_busy() {
                self.backing.general_stats[entered_from_vtl]
                    .guest_busy
                    .increment();
                // Software interrupts/exceptions cannot be automatically re-injected, but RIP still
                // points to the instruction and the event should be re-generated when the
                // instruction is re-executed. Note that hardware does not provide instruction
                // length in this case so it's impossible to directly re-inject a software event if
                // delivery generates an intercept.
                //
                // TODO SNP: Handle ICEBP.
                let exit_int_info = SevEventInjectInfo::from(vmsa.exit_int_info());
                assert!(
                    exit_int_info.valid(),
                    "event inject info should be valid {exit_int_info:x?}"
                );

                match exit_int_info.interruption_type() {
                    x86defs::snp::SEV_INTR_TYPE_EXCEPT => {
                        if exit_int_info.vector() != 3 && exit_int_info.vector() != 4 {
                            // If the event is an exception, we can inject it.
                            Some(exit_int_info)
                        } else {
                            None
                        }
                    }
                    x86defs::snp::SEV_INTR_TYPE_SW => None,
                    _ => Some(exit_int_info),
                }
            } else {
                None
            }
        } else {
            unimplemented!("Only alternate injection is supported for SNP")
        };

        if let Some(inject) = inject {
            vmsa.set_event_inject(inject);
        }
        if vmsa.sev_features().alternate_injection() {
            vmsa.v_intr_cntrl_mut().set_guest_busy(true);
        }

        if last_interrupt_ctrl.irq() && !vmsa.v_intr_cntrl().irq() {
            self.backing.general_stats[entered_from_vtl]
                .int_ack
                .increment();
            // The guest has acknowledged the interrupt.
            self.backing.cvm.lapics[entered_from_vtl]
                .lapic
                .acknowledge_interrupt(last_interrupt_ctrl.vector());
        }

        vmsa.v_intr_cntrl_mut().set_irq(false);

        // Clear lazy EOI before processing the exit.
        if lazy_eoi && self.backing.cvm.hv[entered_from_vtl].clear_lazy_eoi() {
            self.backing.cvm.lapics[entered_from_vtl]
                .lapic
                .access(&mut SnpApicClient {
                    partition: self.partition,
                    vmsa,
                    dev,
                    vmtime: &self.vmtime,
                    vtl: entered_from_vtl,
                })
                .lazy_eoi();
        }

        let mut vmsa = self.runner.vmsa_mut(entered_from_vtl);
        let sev_error_code = SevExitCode(vmsa.guest_error_code());

        let stat = match sev_error_code {
            SevExitCode::CPUID => {
                self.handle_cpuid(entered_from_vtl);
                &mut self.backing.exit_stats[entered_from_vtl].cpuid
            }

            SevExitCode::MSR => {
                let is_write = vmsa.exit_info1() & 1 != 0;
                let msr = vmsa.rcx() as u32;

                self.handle_msr_access(dev, entered_from_vtl, msr, is_write);

                if is_write {
                    &mut self.backing.exit_stats[entered_from_vtl].msr_write
                } else {
                    &mut self.backing.exit_stats[entered_from_vtl].msr_read
                }
            }

            SevExitCode::IOIO => {
                let io_info =
                    SevIoAccessInfo::from(self.runner.vmsa(entered_from_vtl).exit_info1() as u32);

                let access_size = if io_info.access_size32() {
                    4
                } else if io_info.access_size16() {
                    2
                } else {
                    1
                };

                let port_access_protected = self.cvm_try_protect_io_port_access(
                    entered_from_vtl,
                    io_info.port(),
                    io_info.read_access(),
                    access_size,
                    io_info.string_access(),
                    io_info.rep_access(),
                );

                let vmsa = self.runner.vmsa(entered_from_vtl);
                if !port_access_protected {
                    if io_info.string_access() || io_info.rep_access() {
                        let interruption_pending = vmsa.event_inject().valid()
                            || SevEventInjectInfo::from(vmsa.exit_int_info()).valid();

                        // TODO GUEST VSM: consider changing the emulation path
                        // to also check for io port installation, mainly for
                        // handling rep instructions.

                        self.emulate(dev, interruption_pending, entered_from_vtl, ())
                            .await?;
                    } else {
                        let mut rax = vmsa.rax();
                        emulate_io(
                            self.inner.vp_info.base.vp_index,
                            !io_info.read_access(),
                            io_info.port(),
                            &mut rax,
                            access_size,
                            dev,
                        )
                        .await;

                        let mut vmsa = self.runner.vmsa_mut(entered_from_vtl);
                        vmsa.set_rax(rax);
                        advance_to_next_instruction(&mut vmsa);
                    }
                }
                &mut self.backing.exit_stats[entered_from_vtl].ioio
            }

            SevExitCode::VMMCALL => {
                let is_64bit = self.long_mode(entered_from_vtl);
                let guest_memory = &self.partition.gm[entered_from_vtl];
                let handler = UhHypercallHandler {
                    trusted: !self.cvm_partition().hide_isolation,
                    vp: &mut *self,
                    bus: dev,
                    intercepted_vtl: entered_from_vtl,
                };

                // Note: Successful VtlCall/Return handling will change the
                // current/last vtl
                UhHypercallHandler::TRUSTED_DISPATCHER.dispatch(
                    guest_memory,
                    hv1_hypercall::X64RegisterIo::new(handler, is_64bit),
                );
                &mut self.backing.exit_stats[entered_from_vtl].vmmcall
            }

            SevExitCode::SHUTDOWN => {
                return Err(VpHaltReason::TripleFault {
                    vtl: entered_from_vtl.into(),
                });
            }

            SevExitCode::WBINVD | SevExitCode::INVD => {
                // TODO SNP: reissue these locally to forward them to the
                // hypervisor. This isn't pressing because the hypervisor
                // currently doesn't do anything with these for guest VMs.
                advance_to_next_instruction(&mut vmsa);
                &mut self.backing.exit_stats[entered_from_vtl].invd
            }

            SevExitCode::NPF if has_intercept => {
                // TODO SNP: This code needs to be fixed to not rely on the
                // hypervisor message to check the validity of the NPF, rather
                // we should look at the SNP hardware exit info only like we do
                // with TDX.
                //
                // TODO SNP: This code should be fixed so we do not attempt to
                // emulate a NPF with an address that has the wrong shared bit,
                // as this will cause the emulator to raise an internal error,
                // and instead inject a machine check like TDX.
                //
                // Determine whether an NPF needs to be handled. If not, assume
                // this fault is spurious and that the instruction can be
                // retried. The intercept itself may be presented by the
                // hypervisor as either a GPA intercept or an exception
                // intercept. The hypervisor configures the NPT to generate a
                // #VC inside the guest for accesses to unmapped memory. This
                // means that accesses to unmapped memory for lower VTLs will be
                // forwarded to underhill as a #VC exception.
                let gpa = vmsa.exit_info2();
                let interruption_pending = vmsa.event_inject().valid()
                    || SevEventInjectInfo::from(vmsa.exit_int_info()).valid();
                let exit_info = SevNpfInfo::from(vmsa.exit_info1());
                let exit_message = self.runner.exit_message();
                let real = match exit_message.header.typ {
                    HvMessageType::HvMessageTypeExceptionIntercept => {
                        let exception_message =
                            exit_message.as_message::<hvdef::HvX64ExceptionInterceptMessage>();

                        exception_message.vector
                            == x86defs::Exception::SEV_VMM_COMMUNICATION.0 as u16
                    }
                    HvMessageType::HvMessageTypeUnmappedGpa
                    | HvMessageType::HvMessageTypeGpaIntercept
                    | HvMessageType::HvMessageTypeUnacceptedGpa => {
                        let gpa_message =
                            exit_message.as_message::<hvdef::HvX64MemoryInterceptMessage>();

                        // Only the page numbers need to match.
                        (gpa_message.guest_physical_address >> hvdef::HV_PAGE_SHIFT)
                            == (gpa >> hvdef::HV_PAGE_SHIFT)
                    }
                    _ => false,
                };

                if real {
                    has_intercept = false;
                    if self.check_mem_fault(entered_from_vtl, gpa, exit_info.is_write(), exit_info)
                    {
                        self.emulate(dev, interruption_pending, entered_from_vtl, ())
                            .await?;
                    }
                    &mut self.backing.exit_stats[entered_from_vtl].npf
                } else {
                    &mut self.backing.exit_stats[entered_from_vtl].npf_spurious
                }
            }

            SevExitCode::NPF => &mut self.backing.exit_stats[entered_from_vtl].npf_no_intercept,

            SevExitCode::HLT => {
                self.backing.cvm.lapics[entered_from_vtl].activity = MpState::Halted;
                // RIP has already advanced. Clear interrupt shadow.
                vmsa.v_intr_cntrl_mut().set_intr_shadow(false);
                &mut self.backing.exit_stats[entered_from_vtl].hlt
            }

            SevExitCode::INVALID_VMCB => {
                return Err(dev.fatal_error(InvalidVmcb.into()));
            }

            SevExitCode::INVLPGB | SevExitCode::ILLEGAL_INVLPGB => {
                vmsa.set_event_inject(
                    SevEventInjectInfo::new()
                        .with_interruption_type(x86defs::snp::SEV_INTR_TYPE_EXCEPT)
                        .with_vector(x86defs::Exception::INVALID_OPCODE.0)
                        .with_valid(true),
                );
                &mut self.backing.exit_stats[entered_from_vtl].invlpgb
            }

            SevExitCode::RDPMC => {
                // AMD64 always supports at least 4 core performance counters (PerfCtr0-3). Return 0
                // when the guest reads one of the core perf counters, otherwise inject an exception.
                let cr4 = vmsa.cr4();
                if ((vmsa.cpl() > 0) && (cr4 & x86defs::X64_CR4_PCE == 0))
                    || (vmsa.rcx() as u32 >= 4)
                {
                    vmsa.set_event_inject(
                        SevEventInjectInfo::new()
                            .with_interruption_type(x86defs::snp::SEV_INTR_TYPE_EXCEPT)
                            .with_vector(x86defs::Exception::GENERAL_PROTECTION_FAULT.0)
                            .with_deliver_error_code(true)
                            .with_valid(true),
                    );
                } else {
                    vmsa.set_rax(0);
                    vmsa.set_rdx(0);
                    advance_to_next_instruction(&mut vmsa);
                }
                &mut self.backing.exit_stats[entered_from_vtl].rdpmc
            }

            SevExitCode::VMGEXIT if has_intercept => {
                has_intercept = false;
                match self.runner.exit_message().header.typ {
                    HvMessageType::HvMessageTypeX64SevVmgexitIntercept => {
                        self.handle_vmgexit(dev, entered_from_vtl)
                            .map_err(|e| dev.fatal_error(e.into()))?;
                    }
                    _ => has_intercept = true,
                }
                &mut self.backing.exit_stats[entered_from_vtl].vmgexit
            }

            SevExitCode::NMI
            | SevExitCode::PAUSE
            | SevExitCode::SMI
            | SevExitCode::VMGEXIT
            | SevExitCode::BUSLOCK
            | SevExitCode::IDLE_HLT => {
                // Ignore intercept processing if the guest exited due to an automatic exit.
                &mut self.backing.exit_stats[entered_from_vtl].automatic_exit
            }

            SevExitCode::VINTR => {
                // Receipt of a virtual interrupt intercept indicates that a virtual interrupt is ready
                // for injection but injection cannot complete due to the intercept. Rewind the pending
                // virtual interrupt so it is reinjected as a fixed interrupt.

                // TODO SNP: Rewind the interrupt.
                unimplemented!("SevExitCode::VINTR");
            }

            SevExitCode::INTR => {
                // No action is necessary after a physical interrupt intercept. A physical interrupt
                // code is also used as a sentinel value to overwrite the previous error code.
                &mut self.backing.exit_stats[entered_from_vtl].intr
            }

            SevExitCode::XSETBV => {
                self.handle_xsetbv(entered_from_vtl);
                &mut self.backing.exit_stats[entered_from_vtl].xsetbv
            }

            SevExitCode::EXCP_DB => &mut self.backing.exit_stats[entered_from_vtl].excp_db,

            SevExitCode::CR0_WRITE => {
                self.handle_crx_intercept(entered_from_vtl, HvX64RegisterName::Cr0);
                &mut self.backing.exit_stats[entered_from_vtl].secure_reg_write
            }
            SevExitCode::CR4_WRITE => {
                self.handle_crx_intercept(entered_from_vtl, HvX64RegisterName::Cr4);
                &mut self.backing.exit_stats[entered_from_vtl].secure_reg_write
            }

            tr_exit_code @ (SevExitCode::GDTR_WRITE
            | SevExitCode::IDTR_WRITE
            | SevExitCode::LDTR_WRITE
            | SevExitCode::TR_WRITE) => {
                let reg = match tr_exit_code {
                    SevExitCode::GDTR_WRITE => HvX64RegisterName::Gdtr,
                    SevExitCode::IDTR_WRITE => HvX64RegisterName::Idtr,
                    SevExitCode::LDTR_WRITE => HvX64RegisterName::Ldtr,
                    SevExitCode::TR_WRITE => HvX64RegisterName::Tr,
                    _ => unreachable!(),
                };

                if !self.cvm_try_protect_secure_register_write(entered_from_vtl, reg, 0) {
                    // This is an unexpected intercept: should only have received an
                    // intercept for these registers if a VTL (i.e. VTL 1) requested
                    // it. If an unexpected intercept has been received, then the
                    // host must have enabled an intercept that was not desired.
                    // Since the intercept cannot correctly be emulated, this must
                    // be treated as a fatal error.
                    panic!("unexpected secure register");
                }

                &mut self.backing.exit_stats[entered_from_vtl].secure_reg_write
            }

            _ => {
                tracing::error!(
                    CVM_CONFIDENTIAL,
                    "SEV exit code {sev_error_code:x?} sev features {:x?} v_intr_control {:x?} event inject {:x?} \
                    vmpl {:x?} cpl {:x?} exit_info1 {:x?} exit_info2 {:x?} exit_int_info {:x?} virtual_tom {:x?} \
                    efer {:x?} cr4 {:x?} cr3 {:x?} cr0 {:x?} rflag {:x?} rip {:x?} next rip {:x?}",
                    vmsa.sev_features(),
                    vmsa.v_intr_cntrl(),
                    vmsa.event_inject(),
                    vmsa.vmpl(),
                    vmsa.cpl(),
                    vmsa.exit_info1(),
                    vmsa.exit_info2(),
                    vmsa.exit_int_info(),
                    vmsa.virtual_tom(),
                    vmsa.efer(),
                    vmsa.cr4(),
                    vmsa.cr3(),
                    vmsa.cr0(),
                    vmsa.rflags(),
                    vmsa.rip(),
                    vmsa.next_rip(),
                );
                panic!("Received unexpected SEV exit code {sev_error_code:x?}");
            }
        };
        stat.increment();

        // Process debug exceptions before handling other intercepts.
        if cfg!(feature = "gdb") && sev_error_code == SevExitCode::EXCP_DB {
            return self.handle_debug_exception(dev, entered_from_vtl);
        }

        // If there is an unhandled intercept message from the hypervisor, then
        // it may be a synthetic message that should be handled regardless of
        // the SNP exit code.
        if has_intercept {
            self.backing.general_stats[entered_from_vtl]
                .synth_int
                .increment();
            match self.runner.exit_message().header.typ {
                HvMessageType::HvMessageTypeSynicSintDeliverable => {
                    self.handle_synic_deliverable_exit();
                }
                HvMessageType::HvMessageTypeX64Halt
                | HvMessageType::HvMessageTypeExceptionIntercept => {
                    // Ignore.
                    //
                    // TODO SNP: Figure out why we are getting these.
                }
                message_type => {
                    tracelimit::error_ratelimited!(
                        CVM_ALLOWED,
                        ?message_type,
                        "unknown synthetic exit"
                    );
                }
            }
        }

        // Update the guest error code in the vmsa to be a no-op. This prevents the hypervisor from
        // presenting the same #VC twice. A repeated #VC could result in incorrect operation since the
        // first instance would modify state that could be read by the second instance. This must be
        // done regardless of whether the vmsa is runnable or not, since a non-runnable vmsa will still
        // be processed when a proxy interrupt arrives and makes it runnable. It must be done
        // immediately after processing the intercept since another SINT can be taken to process proxy
        // interrupts, regardless of whether the lower VTL has executed.

        self.runner
            .vmsa_mut(entered_from_vtl)
            .set_guest_error_code(SevExitCode::INTR.0);
        Ok(())
    }

    fn long_mode(&self, vtl: GuestVtl) -> bool {
        let vmsa = self.runner.vmsa(vtl);
        vmsa.cr0() & x86defs::X64_CR0_PE != 0 && vmsa.efer() & x86defs::X64_EFER_LMA != 0
    }

    fn handle_cpuid(&mut self, vtl: GuestVtl) {
        let vmsa = self.runner.vmsa(vtl);
        let leaf = vmsa.rax() as u32;
        let subleaf = vmsa.rcx() as u32;
        let [mut eax, mut ebx, mut ecx, mut edx] = self.cvm_cpuid_result(vtl, leaf, subleaf);

        // Apply SNP specific fixups. These must be runtime changes only, for
        // parts of cpuid that are dynamic (either because it's a function of
        // the current VP's identity or the current VP or partition state).
        //
        // We rely on the cpuid set being accurate during partition startup,
        // without running through this code, so violations of this principle
        // may cause the partition to be constructed improperly.
        match CpuidFunction(leaf) {
            CpuidFunction::ProcessorTopologyDefinition => {
                let apic_id = self.inner.vp_info.apic_id;
                let vps_per_socket = self.cvm_partition().vps_per_socket;
                eax = x86defs::cpuid::ProcessorTopologyDefinitionEax::from(eax)
                    .with_extended_apic_id(apic_id)
                    .into();

                let topology_ebx = x86defs::cpuid::ProcessorTopologyDefinitionEbx::from(ebx);
                let mut new_unit_id = apic_id & (vps_per_socket - 1);

                if topology_ebx.threads_per_compute_unit() > 0 {
                    new_unit_id /= 2;
                }

                ebx = topology_ebx.with_compute_unit_id(new_unit_id as u8).into();

                // TODO SNP: Ideally we would use the actual value of this property from the host, but
                // we currently have no way of obtaining it. 1 is the default value for all current VMs.
                let amd_nodes_per_socket = 1u32;

                let node_id = apic_id
                    >> (vps_per_socket
                        .trailing_zeros()
                        .saturating_sub(amd_nodes_per_socket.trailing_zeros()));
                // TODO: just set this part statically.
                let nodes_per_processor = amd_nodes_per_socket - 1;

                ecx = x86defs::cpuid::ProcessorTopologyDefinitionEcx::from(ecx)
                    .with_node_id(node_id as u8)
                    .with_nodes_per_processor(nodes_per_processor as u8)
                    .into();
            }
            CpuidFunction::ExtendedSevFeatures => {
                // SEV features are not exposed to lower VTLs at this time, but
                // we can still query them, so we can't mask them out in the
                // cached CPUID leaf.
                eax = 0;
                ebx = 0;
                ecx = 0;
                edx = 0;
            }
            _ => {}
        }

        let mut vmsa = self.runner.vmsa_mut(vtl);
        vmsa.set_rax(eax.into());
        vmsa.set_rbx(ebx.into());
        vmsa.set_rcx(ecx.into());
        vmsa.set_rdx(edx.into());
        advance_to_next_instruction(&mut vmsa);
    }
}

impl<T: CpuIo> X86EmulatorSupport for UhEmulationState<'_, '_, T, SnpBacked> {
    fn flush(&mut self) {
        //AMD SNP does not require an emulation cache
    }

    fn vp_index(&self) -> VpIndex {
        self.vp.vp_index()
    }

    fn vendor(&self) -> x86defs::cpuid::Vendor {
        self.vp.partition.caps.vendor
    }

    fn gp(&mut self, reg: x86emu::Gp) -> u64 {
        let vmsa = self.vp.runner.vmsa(self.vtl);
        match reg {
            x86emu::Gp::RAX => vmsa.rax(),
            x86emu::Gp::RCX => vmsa.rcx(),
            x86emu::Gp::RDX => vmsa.rdx(),
            x86emu::Gp::RBX => vmsa.rbx(),
            x86emu::Gp::RSP => vmsa.rsp(),
            x86emu::Gp::RBP => vmsa.rbp(),
            x86emu::Gp::RSI => vmsa.rsi(),
            x86emu::Gp::RDI => vmsa.rdi(),
            x86emu::Gp::R8 => vmsa.r8(),
            x86emu::Gp::R9 => vmsa.r9(),
            x86emu::Gp::R10 => vmsa.r10(),
            x86emu::Gp::R11 => vmsa.r11(),
            x86emu::Gp::R12 => vmsa.r12(),
            x86emu::Gp::R13 => vmsa.r13(),
            x86emu::Gp::R14 => vmsa.r14(),
            x86emu::Gp::R15 => vmsa.r15(),
        }
    }

    fn set_gp(&mut self, reg: x86emu::Gp, v: u64) {
        let mut vmsa = self.vp.runner.vmsa_mut(self.vtl);
        match reg {
            x86emu::Gp::RAX => vmsa.set_rax(v),
            x86emu::Gp::RCX => vmsa.set_rcx(v),
            x86emu::Gp::RDX => vmsa.set_rdx(v),
            x86emu::Gp::RBX => vmsa.set_rbx(v),
            x86emu::Gp::RSP => vmsa.set_rsp(v),
            x86emu::Gp::RBP => vmsa.set_rbp(v),
            x86emu::Gp::RSI => vmsa.set_rsi(v),
            x86emu::Gp::RDI => vmsa.set_rdi(v),
            x86emu::Gp::R8 => vmsa.set_r8(v),
            x86emu::Gp::R9 => vmsa.set_r9(v),
            x86emu::Gp::R10 => vmsa.set_r10(v),
            x86emu::Gp::R11 => vmsa.set_r11(v),
            x86emu::Gp::R12 => vmsa.set_r12(v),
            x86emu::Gp::R13 => vmsa.set_r13(v),
            x86emu::Gp::R14 => vmsa.set_r14(v),
            x86emu::Gp::R15 => vmsa.set_r15(v),
        };
    }

    fn xmm(&mut self, index: usize) -> u128 {
        self.vp.runner.vmsa_mut(self.vtl).xmm_registers(index)
    }

    fn set_xmm(&mut self, index: usize, v: u128) {
        self.vp
            .runner
            .vmsa_mut(self.vtl)
            .set_xmm_registers(index, v);
    }

    fn rip(&mut self) -> u64 {
        let vmsa = self.vp.runner.vmsa(self.vtl);
        vmsa.rip()
    }

    fn set_rip(&mut self, v: u64) {
        let mut vmsa = self.vp.runner.vmsa_mut(self.vtl);
        vmsa.set_rip(v);
    }

    fn segment(&mut self, index: x86emu::Segment) -> x86defs::SegmentRegister {
        let vmsa = self.vp.runner.vmsa(self.vtl);
        match index {
            x86emu::Segment::ES => virt_seg_from_snp(vmsa.es()),
            x86emu::Segment::CS => virt_seg_from_snp(vmsa.cs()),
            x86emu::Segment::SS => virt_seg_from_snp(vmsa.ss()),
            x86emu::Segment::DS => virt_seg_from_snp(vmsa.ds()),
            x86emu::Segment::FS => virt_seg_from_snp(vmsa.fs()),
            x86emu::Segment::GS => virt_seg_from_snp(vmsa.gs()),
        }
        .into()
    }

    fn efer(&mut self) -> u64 {
        let vmsa = self.vp.runner.vmsa(self.vtl);
        vmsa.efer()
    }

    fn cr0(&mut self) -> u64 {
        let vmsa = self.vp.runner.vmsa(self.vtl);
        vmsa.cr0()
    }

    fn rflags(&mut self) -> RFlags {
        let vmsa = self.vp.runner.vmsa(self.vtl);
        vmsa.rflags().into()
    }

    fn set_rflags(&mut self, v: RFlags) {
        let mut vmsa = self.vp.runner.vmsa_mut(self.vtl);
        vmsa.set_rflags(v.into());
    }

    fn instruction_bytes(&self) -> &[u8] {
        &[]
    }

    fn physical_address(&self) -> Option<u64> {
        Some(self.vp.runner.vmsa(self.vtl).exit_info2())
    }

    fn initial_gva_translation(
        &mut self,
    ) -> Option<virt_support_x86emu::emulate::InitialTranslation> {
        None
    }

    fn interruption_pending(&self) -> bool {
        self.interruption_pending
    }

    fn check_vtl_access(
        &mut self,
        _gpa: u64,
        _mode: virt_support_x86emu::emulate::TranslateMode,
    ) -> Result<(), virt_support_x86emu::emulate::EmuCheckVtlAccessError> {
        // Nothing to do here, the guest memory object will handle the check.
        Ok(())
    }

    fn translate_gva(
        &mut self,
        gva: u64,
        mode: virt_support_x86emu::emulate::TranslateMode,
    ) -> Result<
        virt_support_x86emu::emulate::EmuTranslateResult,
        virt_support_x86emu::emulate::EmuTranslateError,
    > {
        emulate_translate_gva(self, gva, mode)
    }

    fn inject_pending_event(&mut self, event_info: hvdef::HvX64PendingEvent) {
        assert!(event_info.reg_0.event_pending());
        assert_eq!(
            event_info.reg_0.event_type(),
            hvdef::HV_X64_PENDING_EVENT_EXCEPTION
        );

        let exception = HvX64PendingExceptionEvent::from(event_info.reg_0.into_bits());
        assert!(!self.interruption_pending);

        // There's no interruption pending, so just inject the exception
        // directly without checking for double fault.
        SnpBacked::set_pending_exception(self.vp, self.vtl, exception);
    }

    fn is_gpa_mapped(&self, gpa: u64, write: bool) -> bool {
        // Ignore the VTOM address bit when checking, since memory is mirrored
        // across the VTOM.
        let vtom = self.vp.partition.caps.vtom.unwrap();
        debug_assert!(vtom == 0 || vtom.is_power_of_two());
        self.vp.partition.is_gpa_mapped(gpa & !vtom, write)
    }

    fn lapic_base_address(&self) -> Option<u64> {
        self.vp.backing.cvm.lapics[self.vtl].lapic.base_address()
    }

    fn lapic_read(&mut self, address: u64, data: &mut [u8]) {
        let vtl = self.vtl;
        self.vp.backing.cvm.lapics[vtl]
            .lapic
            .access(&mut SnpApicClient {
                partition: self.vp.partition,
                vmsa: self.vp.runner.vmsa_mut(vtl),
                dev: self.devices,
                vmtime: &self.vp.vmtime,
                vtl,
            })
            .mmio_read(address, data);
    }

    fn lapic_write(&mut self, address: u64, data: &[u8]) {
        let vtl = self.vtl;
        self.vp.backing.cvm.lapics[vtl]
            .lapic
            .access(&mut SnpApicClient {
                partition: self.vp.partition,
                vmsa: self.vp.runner.vmsa_mut(vtl),
                dev: self.devices,
                vmtime: &self.vp.vmtime,
                vtl,
            })
            .mmio_write(address, data);
    }

    fn monitor_support(&self) -> Option<&dyn EmulatorMonitorSupport> {
        Some(self)
    }
}

impl<T> hv1_hypercall::X64RegisterState for UhHypercallHandler<'_, '_, T, SnpBacked> {
    fn rip(&mut self) -> u64 {
        self.vp.runner.vmsa(self.intercepted_vtl).rip()
    }

    fn set_rip(&mut self, rip: u64) {
        self.vp.runner.vmsa_mut(self.intercepted_vtl).set_rip(rip);
    }

    fn gp(&mut self, n: hv1_hypercall::X64HypercallRegister) -> u64 {
        let vmsa = self.vp.runner.vmsa(self.intercepted_vtl);
        match n {
            hv1_hypercall::X64HypercallRegister::Rax => vmsa.rax(),
            hv1_hypercall::X64HypercallRegister::Rcx => vmsa.rcx(),
            hv1_hypercall::X64HypercallRegister::Rdx => vmsa.rdx(),
            hv1_hypercall::X64HypercallRegister::Rbx => vmsa.rbx(),
            hv1_hypercall::X64HypercallRegister::Rsi => vmsa.rsi(),
            hv1_hypercall::X64HypercallRegister::Rdi => vmsa.rdi(),
            hv1_hypercall::X64HypercallRegister::R8 => vmsa.r8(),
        }
    }

    fn set_gp(&mut self, n: hv1_hypercall::X64HypercallRegister, value: u64) {
        let mut vmsa = self.vp.runner.vmsa_mut(self.intercepted_vtl);
        match n {
            hv1_hypercall::X64HypercallRegister::Rax => vmsa.set_rax(value),
            hv1_hypercall::X64HypercallRegister::Rcx => vmsa.set_rcx(value),
            hv1_hypercall::X64HypercallRegister::Rdx => vmsa.set_rdx(value),
            hv1_hypercall::X64HypercallRegister::Rbx => vmsa.set_rbx(value),
            hv1_hypercall::X64HypercallRegister::Rsi => vmsa.set_rsi(value),
            hv1_hypercall::X64HypercallRegister::Rdi => vmsa.set_rdi(value),
            hv1_hypercall::X64HypercallRegister::R8 => vmsa.set_r8(value),
        }
    }

    fn xmm(&mut self, n: usize) -> u128 {
        self.vp.runner.vmsa(self.intercepted_vtl).xmm_registers(n)
    }

    fn set_xmm(&mut self, n: usize, value: u128) {
        self.vp
            .runner
            .vmsa_mut(self.intercepted_vtl)
            .set_xmm_registers(n, value);
    }
}

impl AccessVpState for UhVpStateAccess<'_, '_, SnpBacked> {
    type Error = vp_state::Error;

    fn caps(&self) -> &virt::x86::X86PartitionCapabilities {
        &self.vp.partition.caps
    }

    fn commit(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn registers(&mut self) -> Result<vp::Registers, Self::Error> {
        let vmsa = self.vp.runner.vmsa(self.vtl);

        Ok(vp::Registers {
            rax: vmsa.rax(),
            rcx: vmsa.rcx(),
            rdx: vmsa.rdx(),
            rbx: vmsa.rbx(),
            rsp: vmsa.rsp(),
            rbp: vmsa.rbp(),
            rsi: vmsa.rsi(),
            rdi: vmsa.rdi(),
            r8: vmsa.r8(),
            r9: vmsa.r9(),
            r10: vmsa.r10(),
            r11: vmsa.r11(),
            r12: vmsa.r12(),
            r13: vmsa.r13(),
            r14: vmsa.r14(),
            r15: vmsa.r15(),
            rip: vmsa.rip(),
            rflags: vmsa.rflags(),
            cs: virt_seg_from_snp(vmsa.cs()),
            ds: virt_seg_from_snp(vmsa.ds()),
            es: virt_seg_from_snp(vmsa.es()),
            fs: virt_seg_from_snp(vmsa.fs()),
            gs: virt_seg_from_snp(vmsa.gs()),
            ss: virt_seg_from_snp(vmsa.ss()),
            tr: virt_seg_from_snp(vmsa.tr()),
            ldtr: virt_seg_from_snp(vmsa.ldtr()),
            gdtr: virt_table_from_snp(vmsa.gdtr()),
            idtr: virt_table_from_snp(vmsa.idtr()),
            cr0: vmsa.cr0(),
            cr2: vmsa.cr2(),
            cr3: vmsa.cr3(),
            cr4: vmsa.cr4(),
            cr8: vmsa.v_intr_cntrl().tpr().into(),
            efer: vmsa.efer(),
        })
    }

    fn set_registers(&mut self, value: &vp::Registers) -> Result<(), Self::Error> {
        let mut vmsa = self.vp.runner.vmsa_mut(self.vtl);

        let vp::Registers {
            rax,
            rcx,
            rdx,
            rbx,
            rsp,
            rbp,
            rsi,
            rdi,
            r8,
            r9,
            r10,
            r11,
            r12,
            r13,
            r14,
            r15,
            rip,
            rflags,
            cs,
            ds,
            es,
            fs,
            gs,
            ss,
            tr,
            ldtr,
            gdtr,
            idtr,
            cr0,
            cr2,
            cr3,
            cr4,
            cr8,
            efer,
        } = *value;
        vmsa.set_rax(rax);
        vmsa.set_rcx(rcx);
        vmsa.set_rdx(rdx);
        vmsa.set_rbx(rbx);
        vmsa.set_rsp(rsp);
        vmsa.set_rbp(rbp);
        vmsa.set_rsi(rsi);
        vmsa.set_rdi(rdi);
        vmsa.set_r8(r8);
        vmsa.set_r9(r9);
        vmsa.set_r10(r10);
        vmsa.set_r11(r11);
        vmsa.set_r12(r12);
        vmsa.set_r13(r13);
        vmsa.set_r14(r14);
        vmsa.set_r15(r15);
        vmsa.set_rip(rip);
        vmsa.set_rflags(rflags);
        vmsa.set_cs(virt_seg_to_snp(cs));
        vmsa.set_ds(virt_seg_to_snp(ds));
        vmsa.set_es(virt_seg_to_snp(es));
        vmsa.set_fs(virt_seg_to_snp(fs));
        vmsa.set_gs(virt_seg_to_snp(gs));
        vmsa.set_ss(virt_seg_to_snp(ss));
        vmsa.set_tr(virt_seg_to_snp(tr));
        vmsa.set_ldtr(virt_seg_to_snp(ldtr));
        vmsa.set_gdtr(virt_table_to_snp(gdtr));
        vmsa.set_idtr(virt_table_to_snp(idtr));
        vmsa.set_cr0(cr0);
        vmsa.set_cr2(cr2);
        vmsa.set_cr3(cr3);
        vmsa.set_cr4(cr4);
        vmsa.v_intr_cntrl_mut().set_tpr(cr8 as u8);
        vmsa.set_efer(SnpBacked::calculate_efer(efer, cr0));
        Ok(())
    }

    fn activity(&mut self) -> Result<vp::Activity, Self::Error> {
        let lapic = &self.vp.backing.cvm.lapics[self.vtl];

        Ok(vp::Activity {
            mp_state: lapic.activity,
            nmi_pending: lapic.nmi_pending,
            nmi_masked: false,          // TODO SNP
            interrupt_shadow: false,    // TODO SNP
            pending_event: None,        // TODO SNP
            pending_interruption: None, // TODO SNP
        })
    }

    fn set_activity(&mut self, value: &vp::Activity) -> Result<(), Self::Error> {
        let &vp::Activity {
            mp_state,
            nmi_pending,
            nmi_masked: _,           // TODO SNP
            interrupt_shadow: _,     // TODO SNP
            pending_event: _,        // TODO SNP
            pending_interruption: _, // TODO SNP
        } = value;
        let lapic = &mut self.vp.backing.cvm.lapics[self.vtl];
        lapic.activity = mp_state;
        lapic.nmi_pending = nmi_pending;

        Ok(())
    }

    fn xsave(&mut self) -> Result<vp::Xsave, Self::Error> {
        Err(vp_state::Error::Unimplemented("xsave"))
    }

    fn set_xsave(&mut self, _value: &vp::Xsave) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("xsave"))
    }

    fn apic(&mut self) -> Result<vp::Apic, Self::Error> {
        Ok(self.vp.backing.cvm.lapics[self.vtl].lapic.save())
    }

    fn set_apic(&mut self, value: &vp::Apic) -> Result<(), Self::Error> {
        self.vp.backing.cvm.lapics[self.vtl]
            .lapic
            .restore(value)
            .map_err(vp_state::Error::InvalidApicBase)?;
        Ok(())
    }

    fn xcr(&mut self) -> Result<vp::Xcr0, Self::Error> {
        let vmsa = self.vp.runner.vmsa(self.vtl);
        Ok(vp::Xcr0 { value: vmsa.xcr0() })
    }

    fn set_xcr(&mut self, value: &vp::Xcr0) -> Result<(), Self::Error> {
        let vp::Xcr0 { value } = *value;
        self.vp.runner.vmsa_mut(self.vtl).set_xcr0(value);
        Ok(())
    }

    fn xss(&mut self) -> Result<vp::Xss, Self::Error> {
        let vmsa = self.vp.runner.vmsa(self.vtl);
        Ok(vp::Xss { value: vmsa.xss() })
    }

    fn set_xss(&mut self, value: &vp::Xss) -> Result<(), Self::Error> {
        let vp::Xss { value } = *value;
        self.vp.runner.vmsa_mut(self.vtl).set_xss(value);
        Ok(())
    }

    fn mtrrs(&mut self) -> Result<vp::Mtrrs, Self::Error> {
        Ok(vp::Mtrrs {
            msr_mtrr_def_type: 0,
            fixed: [0; 11],
            variable: [0; 16],
        })
    }

    fn set_mtrrs(&mut self, _value: &vp::Mtrrs) -> Result<(), Self::Error> {
        Ok(())
    }

    fn pat(&mut self) -> Result<vp::Pat, Self::Error> {
        let vmsa = self.vp.runner.vmsa(self.vtl);
        Ok(vp::Pat { value: vmsa.pat() })
    }

    fn set_pat(&mut self, value: &vp::Pat) -> Result<(), Self::Error> {
        let vp::Pat { value } = *value;
        self.vp.runner.vmsa_mut(self.vtl).set_pat(value);
        Ok(())
    }

    fn virtual_msrs(&mut self) -> Result<vp::VirtualMsrs, Self::Error> {
        let vmsa = self.vp.runner.vmsa(self.vtl);

        Ok(vp::VirtualMsrs {
            kernel_gs_base: vmsa.kernel_gs_base(),
            sysenter_cs: vmsa.sysenter_cs(),
            sysenter_eip: vmsa.sysenter_eip(),
            sysenter_esp: vmsa.sysenter_esp(),
            star: vmsa.star(),
            lstar: vmsa.lstar(),
            cstar: vmsa.cstar(),
            sfmask: vmsa.sfmask(),
        })
    }

    fn set_virtual_msrs(&mut self, value: &vp::VirtualMsrs) -> Result<(), Self::Error> {
        let mut vmsa = self.vp.runner.vmsa_mut(self.vtl);
        let vp::VirtualMsrs {
            kernel_gs_base,
            sysenter_cs,
            sysenter_eip,
            sysenter_esp,
            star,
            lstar,
            cstar,
            sfmask,
        } = *value;
        vmsa.set_kernel_gs_base(kernel_gs_base);
        vmsa.set_sysenter_cs(sysenter_cs);
        vmsa.set_sysenter_eip(sysenter_eip);
        vmsa.set_sysenter_esp(sysenter_esp);
        vmsa.set_star(star);
        vmsa.set_lstar(lstar);
        vmsa.set_cstar(cstar);
        vmsa.set_sfmask(sfmask);

        Ok(())
    }

    fn debug_regs(&mut self) -> Result<vp::DebugRegisters, Self::Error> {
        let vmsa = self.vp.runner.vmsa(self.vtl);
        Ok(vp::DebugRegisters {
            dr0: vmsa.dr0(),
            dr1: vmsa.dr1(),
            dr2: vmsa.dr2(),
            dr3: vmsa.dr3(),
            dr6: vmsa.dr6(),
            dr7: vmsa.dr7(),
        })
    }

    fn set_debug_regs(&mut self, value: &vp::DebugRegisters) -> Result<(), Self::Error> {
        let mut vmsa = self.vp.runner.vmsa_mut(self.vtl);
        let vp::DebugRegisters {
            dr0,
            dr1,
            dr2,
            dr3,
            dr6,
            dr7,
        } = *value;
        vmsa.set_dr0(dr0);
        vmsa.set_dr1(dr1);
        vmsa.set_dr2(dr2);
        vmsa.set_dr3(dr3);
        vmsa.set_dr6(dr6);
        vmsa.set_dr7(dr7);
        Ok(())
    }

    fn tsc(&mut self) -> Result<vp::Tsc, Self::Error> {
        Err(vp_state::Error::Unimplemented("tsc"))
    }

    fn set_tsc(&mut self, _value: &vp::Tsc) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("tsc"))
    }

    fn tsc_aux(&mut self) -> Result<vp::TscAux, Self::Error> {
        let vmsa = self.vp.runner.vmsa(self.vtl);
        Ok(vp::TscAux {
            value: vmsa.tsc_aux() as u64,
        })
    }

    fn set_tsc_aux(&mut self, value: &vp::TscAux) -> Result<(), Self::Error> {
        let vp::TscAux { value } = *value;
        self.vp.runner.vmsa_mut(self.vtl).set_tsc_aux(value as u32);
        Ok(())
    }

    fn cet(&mut self) -> Result<vp::Cet, Self::Error> {
        let vmsa = self.vp.runner.vmsa(self.vtl);
        Ok(vp::Cet { scet: vmsa.s_cet() })
    }

    fn set_cet(&mut self, value: &vp::Cet) -> Result<(), Self::Error> {
        let vp::Cet { scet } = *value;
        self.vp.runner.vmsa_mut(self.vtl).set_s_cet(scet);
        Ok(())
    }

    fn cet_ss(&mut self) -> Result<vp::CetSs, Self::Error> {
        let vmsa = self.vp.runner.vmsa(self.vtl);
        Ok(vp::CetSs {
            ssp: vmsa.ssp(),
            interrupt_ssp_table_addr: vmsa.interrupt_ssp_table_addr(),
        })
    }

    fn set_cet_ss(&mut self, value: &vp::CetSs) -> Result<(), Self::Error> {
        let mut vmsa = self.vp.runner.vmsa_mut(self.vtl);
        let vp::CetSs {
            ssp,
            interrupt_ssp_table_addr,
        } = *value;
        vmsa.set_ssp(ssp);
        vmsa.set_interrupt_ssp_table_addr(interrupt_ssp_table_addr);
        Ok(())
    }

    fn synic_msrs(&mut self) -> Result<vp::SyntheticMsrs, Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_msrs"))
    }

    fn set_synic_msrs(&mut self, _value: &vp::SyntheticMsrs) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_msrs"))
    }

    fn synic_message_page(&mut self) -> Result<vp::SynicMessagePage, Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_message_page"))
    }

    fn set_synic_message_page(&mut self, _value: &vp::SynicMessagePage) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_message_page"))
    }

    fn synic_event_flags_page(&mut self) -> Result<vp::SynicEventFlagsPage, Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_event_flags_page"))
    }

    fn set_synic_event_flags_page(
        &mut self,
        _value: &vp::SynicEventFlagsPage,
    ) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_event_flags_page"))
    }

    fn synic_message_queues(&mut self) -> Result<vp::SynicMessageQueues, Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_message_queues"))
    }

    fn set_synic_message_queues(
        &mut self,
        _value: &vp::SynicMessageQueues,
    ) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_message_queues"))
    }

    fn synic_timers(&mut self) -> Result<vp::SynicTimers, Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_timers"))
    }

    fn set_synic_timers(&mut self, _value: &vp::SynicTimers) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_timers"))
    }
}

/// Advances rip to be the same as next_rip.
fn advance_to_next_instruction(vmsa: &mut VmsaWrapper<'_, &mut SevVmsa>) {
    vmsa.set_rip(vmsa.next_rip());
    vmsa.v_intr_cntrl_mut().set_intr_shadow(false);
}

impl UhProcessor<'_, SnpBacked> {
    fn read_msr_snp(
        &mut self,
        _dev: &impl CpuIo,
        msr: u32,
        vtl: GuestVtl,
    ) -> Result<u64, MsrError> {
        let vmsa = self.runner.vmsa(vtl);
        let value = match msr {
            x86defs::X64_MSR_FS_BASE => vmsa.fs().base,
            x86defs::X64_MSR_GS_BASE => vmsa.gs().base,
            x86defs::X64_MSR_KERNEL_GS_BASE => vmsa.kernel_gs_base(),
            x86defs::X86X_MSR_TSC_AUX => {
                if self.shared.tsc_aux_virtualized {
                    vmsa.tsc_aux() as u64
                } else {
                    return Err(MsrError::InvalidAccess);
                }
            }
            x86defs::X86X_MSR_SPEC_CTRL => vmsa.spec_ctrl(),
            x86defs::X86X_MSR_U_CET => vmsa.u_cet(),
            x86defs::X86X_MSR_S_CET => vmsa.s_cet(),
            x86defs::X86X_MSR_PL0_SSP => vmsa.pl0_ssp(),
            x86defs::X86X_MSR_PL1_SSP => vmsa.pl1_ssp(),
            x86defs::X86X_MSR_PL2_SSP => vmsa.pl2_ssp(),
            x86defs::X86X_MSR_PL3_SSP => vmsa.pl3_ssp(),
            x86defs::X86X_MSR_INTERRUPT_SSP_TABLE_ADDR => vmsa.interrupt_ssp_table_addr(),
            x86defs::X86X_MSR_CR_PAT => vmsa.pat(),
            x86defs::X86X_MSR_EFER => vmsa.efer(),
            x86defs::X86X_MSR_STAR => vmsa.star(),
            x86defs::X86X_MSR_LSTAR => vmsa.lstar(),
            x86defs::X86X_MSR_CSTAR => vmsa.cstar(),
            x86defs::X86X_MSR_SFMASK => vmsa.sfmask(),
            x86defs::X86X_MSR_SYSENTER_CS => vmsa.sysenter_cs(),
            x86defs::X86X_MSR_SYSENTER_ESP => vmsa.sysenter_esp(),
            x86defs::X86X_MSR_SYSENTER_EIP => vmsa.sysenter_eip(),
            x86defs::X86X_MSR_XSS => vmsa.xss(),
            x86defs::X86X_AMD_MSR_VM_CR => 0,
            x86defs::X86X_MSR_TSC => safe_intrinsics::rdtsc(),
            x86defs::X86X_MSR_MC_UPDATE_PATCH_LEVEL => 0xffff_ffff,
            x86defs::X86X_MSR_MTRR_CAP => {
                // Advertise the absence of MTRR capabilities, but include the availability of write
                // combining.
                0x400
            }
            x86defs::X86X_MSR_MTRR_DEF_TYPE => {
                // Because the MTRR registers are advertised via CPUID, even though no actual ranges
                // are supported a guest may choose to write to this MSR. Implement it as read as
                // zero/write ignore.
                0
            }
            x86defs::X86X_AMD_MSR_SYSCFG
            | x86defs::X86X_MSR_MCG_CAP
            | x86defs::X86X_MSR_MCG_STATUS => 0,

            hvdef::HV_X64_MSR_GUEST_IDLE => {
                self.backing.cvm.lapics[vtl].activity = MpState::Idle;
                let mut vmsa = self.runner.vmsa_mut(vtl);
                vmsa.v_intr_cntrl_mut().set_intr_shadow(false);
                0
            }
            _ => return Err(MsrError::Unknown),
        };
        Ok(value)
    }

    fn write_msr_snp(
        &mut self,
        _dev: &impl CpuIo,
        msr: u32,
        value: u64,
        vtl: GuestVtl,
    ) -> Result<(), MsrError> {
        // TODO SNP: validation on the values being set, e.g. checking addresses
        // are canonical, etc.
        let mut vmsa = self.runner.vmsa_mut(vtl);
        match msr {
            x86defs::X64_MSR_FS_BASE => {
                let fs = vmsa.fs();
                vmsa.set_fs(SevSelector {
                    attrib: fs.attrib,
                    selector: fs.selector,
                    limit: fs.limit,
                    base: value,
                });
            }
            x86defs::X64_MSR_GS_BASE => {
                let gs = vmsa.gs();
                vmsa.set_gs(SevSelector {
                    attrib: gs.attrib,
                    selector: gs.selector,
                    limit: gs.limit,
                    base: value,
                });
            }
            x86defs::X64_MSR_KERNEL_GS_BASE => vmsa.set_kernel_gs_base(value),
            x86defs::X86X_MSR_TSC_AUX => {
                if self.shared.tsc_aux_virtualized {
                    vmsa.set_tsc_aux(value as u32);
                } else {
                    return Err(MsrError::InvalidAccess);
                }
            }
            x86defs::X86X_MSR_SPEC_CTRL => vmsa.set_spec_ctrl(value),
            x86defs::X86X_MSR_U_CET => vmsa.set_u_cet(value),
            x86defs::X86X_MSR_S_CET => vmsa.set_s_cet(value),
            x86defs::X86X_MSR_PL0_SSP => vmsa.set_pl0_ssp(value),
            x86defs::X86X_MSR_PL1_SSP => vmsa.set_pl1_ssp(value),
            x86defs::X86X_MSR_PL2_SSP => vmsa.set_pl2_ssp(value),
            x86defs::X86X_MSR_PL3_SSP => vmsa.set_pl3_ssp(value),
            x86defs::X86X_MSR_INTERRUPT_SSP_TABLE_ADDR => vmsa.set_interrupt_ssp_table_addr(value),

            x86defs::X86X_MSR_CR_PAT => vmsa.set_pat(value),
            x86defs::X86X_MSR_EFER => vmsa.set_efer(SnpBacked::calculate_efer(value, vmsa.cr0())),

            x86defs::X86X_MSR_STAR => vmsa.set_star(value),
            x86defs::X86X_MSR_LSTAR => vmsa.set_lstar(value),
            x86defs::X86X_MSR_CSTAR => vmsa.set_cstar(value),
            x86defs::X86X_MSR_SFMASK => vmsa.set_sfmask(value),
            x86defs::X86X_MSR_SYSENTER_CS => vmsa.set_sysenter_cs(value),
            x86defs::X86X_MSR_SYSENTER_ESP => vmsa.set_sysenter_esp(value),
            x86defs::X86X_MSR_SYSENTER_EIP => vmsa.set_sysenter_eip(value),
            x86defs::X86X_MSR_XSS => vmsa.set_xss(value),

            x86defs::X86X_MSR_TSC => {} // ignore writes to the TSC for now
            x86defs::X86X_MSR_MC_UPDATE_PATCH_LEVEL => {}
            x86defs::X86X_MSR_MTRR_DEF_TYPE => {}

            x86defs::X86X_AMD_MSR_VM_CR
            | x86defs::X86X_MSR_MTRR_CAP
            | x86defs::X86X_AMD_MSR_SYSCFG
            | x86defs::X86X_MSR_MCG_CAP => return Err(MsrError::InvalidAccess),

            x86defs::X86X_MSR_MCG_STATUS => {
                // Writes are swallowed, except for reserved bits violations
                if x86defs::X86xMcgStatusRegister::from(value).reserved0() != 0 {
                    return Err(MsrError::InvalidAccess);
                }
            }
            _ => {
                tracing::debug!(msr, value, "unknown cvm msr write");
            }
        }
        Ok(())
    }
}

impl<T: CpuIo> hv1_hypercall::VtlSwitchOps for UhHypercallHandler<'_, '_, T, SnpBacked> {
    fn advance_ip(&mut self) {
        let is_64bit = self.vp.long_mode(self.intercepted_vtl);
        let mut io = hv1_hypercall::X64RegisterIo::new(self, is_64bit);
        io.advance_ip();
    }

    fn inject_invalid_opcode_fault(&mut self) {
        self.vp
            .runner
            .vmsa_mut(self.intercepted_vtl)
            .set_event_inject(
                SevEventInjectInfo::new()
                    .with_valid(true)
                    .with_interruption_type(x86defs::snp::SEV_INTR_TYPE_EXCEPT)
                    .with_vector(x86defs::Exception::INVALID_OPCODE.0),
            );
    }
}

impl<T: CpuIo> hv1_hypercall::FlushVirtualAddressList for UhHypercallHandler<'_, '_, T, SnpBacked> {
    fn flush_virtual_address_list(
        &mut self,
        processor_set: ProcessorSet<'_>,
        flags: HvFlushFlags,
        gva_ranges: &[HvGvaRange],
    ) -> HvRepResult {
        hv1_hypercall::FlushVirtualAddressListEx::flush_virtual_address_list_ex(
            self,
            processor_set,
            flags,
            gva_ranges,
        )
    }
}

impl<T: CpuIo> hv1_hypercall::FlushVirtualAddressListEx
    for UhHypercallHandler<'_, '_, T, SnpBacked>
{
    fn flush_virtual_address_list_ex(
        &mut self,
        processor_set: ProcessorSet<'_>,
        flags: HvFlushFlags,
        gva_ranges: &[HvGvaRange],
    ) -> HvRepResult {
        self.hcvm_validate_flush_inputs(processor_set, flags, true)
            .map_err(|e| (e, 0))?;

        // As a performance optimization if we are asked to do too large an amount of work
        // just do a flush entire instead.
        if gva_ranges.len() > 16 || gva_ranges.iter().any(|range| if flags.use_extended_range_format() { range.as_extended().additional_pages() } else { range.as_simple().additional_pages() } > 16) {
            self.do_flush_virtual_address_space(processor_set, flags);
        } else {
            self.do_flush_virtual_address_list(flags, gva_ranges);
        }

        // Mark that this VP needs to wait for all TLB locks to be released before returning.
        self.vp.set_wait_for_tlb_locks(self.intercepted_vtl);
        Ok(())
    }
}

impl<T: CpuIo> hv1_hypercall::FlushVirtualAddressSpace
    for UhHypercallHandler<'_, '_, T, SnpBacked>
{
    fn flush_virtual_address_space(
        &mut self,
        processor_set: ProcessorSet<'_>,
        flags: HvFlushFlags,
    ) -> hvdef::HvResult<()> {
        hv1_hypercall::FlushVirtualAddressSpaceEx::flush_virtual_address_space_ex(
            self,
            processor_set,
            flags,
        )
    }
}

impl<T: CpuIo> hv1_hypercall::FlushVirtualAddressSpaceEx
    for UhHypercallHandler<'_, '_, T, SnpBacked>
{
    fn flush_virtual_address_space_ex(
        &mut self,
        processor_set: ProcessorSet<'_>,
        flags: HvFlushFlags,
    ) -> hvdef::HvResult<()> {
        self.hcvm_validate_flush_inputs(processor_set, flags, false)?;

        self.do_flush_virtual_address_space(processor_set, flags);

        // Mark that this VP needs to wait for all TLB locks to be released before returning.
        self.vp.set_wait_for_tlb_locks(self.intercepted_vtl);
        Ok(())
    }
}

impl<T: CpuIo> UhHypercallHandler<'_, '_, T, SnpBacked> {
    fn do_flush_virtual_address_list(&mut self, flags: HvFlushFlags, gva_ranges: &[HvGvaRange]) {
        for range in gva_ranges {
            let mut rax = SevInvlpgbRax::new()
                .with_asid_valid(true)
                .with_va_valid(true)
                .with_global(!flags.non_global_mappings_only());
            let mut ecx = SevInvlpgbEcx::new();
            let mut count;
            let mut gpn;

            if flags.use_extended_range_format() && range.as_extended().large_page() {
                ecx.set_large_page(true);
                if range.as_extended_large_page().page_size() {
                    let range = range.as_extended_large_page();
                    count = range.additional_pages();
                    gpn = range.gva_large_page_number();
                } else {
                    let range = range.as_extended();
                    count = range.additional_pages();
                    gpn = range.gva_page_number();
                }
            } else {
                let range = range.as_simple();
                count = range.additional_pages();
                gpn = range.gva_page_number();
            }
            count += 1; // account for self

            while count > 0 {
                rax.set_virtual_page_number(gpn);
                ecx.set_additional_count(std::cmp::min(
                    count - 1,
                    self.vp.shared.invlpgb_count_max.into(),
                ));

                let edx = SevInvlpgbEdx::new();
                self.vp
                    .partition
                    .hcl
                    .invlpgb(rax.into(), edx.into(), ecx.into());

                count -= ecx.additional_count() + 1;
                gpn += ecx.additional_count() + 1;
            }
        }

        self.vp.partition.hcl.tlbsync();
    }

    fn do_flush_virtual_address_space(
        &mut self,
        processor_set: ProcessorSet<'_>,
        flags: HvFlushFlags,
    ) {
        let only_self = [self.vp.vp_index().index()].into_iter().eq(processor_set);
        if only_self && flags.non_global_mappings_only() {
            self.vp.runner.vmsa_mut(self.intercepted_vtl).set_pcpu_id(0);
        } else {
            self.vp.partition.hcl.invlpgb(
                SevInvlpgbRax::new()
                    .with_asid_valid(true)
                    .with_global(!flags.non_global_mappings_only())
                    .into(),
                SevInvlpgbEdx::new().into(),
                SevInvlpgbEcx::new().into(),
            );
            self.vp.partition.hcl.tlbsync();
        }
    }
}

struct SnpTlbLockFlushAccess<'a> {
    vp_index: Option<VpIndex>,
    partition: &'a UhPartitionInner,
    shared: &'a SnpBackedShared,
}

impl TlbFlushLockAccess for SnpTlbLockFlushAccess<'_> {
    fn flush(&mut self, vtl: GuestVtl) {
        // SNP provides no mechanism to flush a single VTL across multiple VPs
        // Do a flush entire, but only wait on the VTL that was asked for
        self.partition.hcl.invlpgb(
            SevInvlpgbRax::new()
                .with_asid_valid(true)
                .with_global(true)
                .into(),
            SevInvlpgbEdx::new().into(),
            SevInvlpgbEcx::new().into(),
        );
        self.partition.hcl.tlbsync();
        self.set_wait_for_tlb_locks(vtl);
    }

    fn flush_entire(&mut self) {
        self.partition.hcl.invlpgb(
            SevInvlpgbRax::new()
                .with_asid_valid(true)
                .with_global(true)
                .into(),
            SevInvlpgbEdx::new().into(),
            SevInvlpgbEcx::new().into(),
        );
        self.partition.hcl.tlbsync();
        for vtl in [GuestVtl::Vtl0, GuestVtl::Vtl1] {
            self.set_wait_for_tlb_locks(vtl);
        }
    }

    fn set_wait_for_tlb_locks(&mut self, vtl: GuestVtl) {
        if let Some(vp_index) = self.vp_index {
            hardware_cvm::tlb_lock::TlbLockAccess {
                vp_index,
                cvm_partition: &self.shared.cvm,
            }
            .set_wait_for_tlb_locks(vtl);
        }
    }
}

mod save_restore {
    use super::SnpBacked;
    use super::UhProcessor;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;
    use vmcore::save_restore::SavedStateNotSupported;

    impl SaveRestore for UhProcessor<'_, SnpBacked> {
        type SavedState = SavedStateNotSupported;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Err(SaveError::NotSupported)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            match state {}
        }
    }
}
