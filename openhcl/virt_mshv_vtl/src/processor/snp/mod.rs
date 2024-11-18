// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Processor support for SNP partitions.

use super::from_seg;
use super::hardware_cvm;
use super::private::BackingParams;
use super::vp_state;
use super::vp_state::UhVpStateAccess;
use super::BackingPrivate;
use super::BackingSharedParams;
use super::HardwareIsolatedBacking;
use super::LapicState;
use super::UhEmulationState;
use super::UhRunVpError;
use crate::devmsr;
use crate::processor::UhHypercallHandler;
use crate::processor::UhProcessor;
use crate::BackingShared;
use crate::Error;
use crate::GuestVtl;
use crate::UhCvmPartitionState;
use crate::UhCvmVpState;
use crate::UhPartitionInner;
use crate::WakeReason;
use hcl::vmsa::VmsaWrapper;
use hv1_emulator::hv::ProcessorVtlHv;
use hv1_emulator::synic::ProcessorSynic;
use hv1_hypercall::HypercallIo;
use hvdef::hypercall::HvFlushFlags;
use hvdef::hypercall::HvGvaRange;
use hvdef::hypercall::HypercallOutput;
use hvdef::HvDeliverabilityNotificationsRegister;
use hvdef::HvError;
use hvdef::HvMessageType;
use hvdef::HvX64PendingExceptionEvent;
use hvdef::HvX64RegisterName;
use hvdef::Vtl;
use hvdef::HV_PAGE_SIZE;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use std::num::NonZeroU64;
use virt::io::CpuIo;
use virt::state::StateElement;
use virt::vp;
use virt::vp::AccessVpState;
use virt::x86::MsrError;
use virt::x86::MsrErrorExt;
use virt::Processor;
use virt::VpHaltReason;
use virt::VpIndex;
use virt_support_apic::ApicClient;
use virt_support_apic::ApicWork;
use virt_support_x86emu::emulate::emulate_io;
use virt_support_x86emu::emulate::emulate_translate_gva;
use virt_support_x86emu::emulate::EmulatorSupport as X86EmulatorSupport;
use virt_support_x86emu::translate::TranslationRegisters;
use vmcore::vmtime::VmTimeAccess;
use vtl_array::VtlArray;
use x86defs::cpuid::CpuidFunction;
use x86defs::snp::SevEventInjectInfo;
use x86defs::snp::SevExitCode;
use x86defs::snp::SevInvlpgbEcx;
use x86defs::snp::SevInvlpgbEdx;
use x86defs::snp::SevInvlpgbRax;
use x86defs::snp::SevSelector;
use x86defs::snp::SevStatusMsr;
use x86defs::snp::SevVmsa;
use x86defs::snp::Vmpl;
use zerocopy::AsBytes;
use zerocopy::FromBytes;
use zerocopy::FromZeroes;

/// A backing for SNP partitions.
#[derive(InspectMut)]
pub struct SnpBacked {
    lapics: VtlArray<LapicState, 2>,
    // TODO CVM GUEST VSM Do we need two sets of any other fields in here?
    /// PFNs used for overlays.
    #[inspect(iter_by_index)]
    direct_overlays_pfns: [u64; UhDirectOverlay::Count as usize],
    #[inspect(skip)]
    #[allow(dead_code)] // Allocation handle for direct overlays held until drop
    direct_overlay_pfns_handle: shared_pool_alloc::SharedPoolHandle,
    #[inspect(hex)]
    hv_sint_notifications: u16,
    general_stats: VtlArray<GeneralStats, 2>,
    exit_stats: VtlArray<ExitStats, 2>,
    cvm: UhCvmVpState,
}

#[derive(Inspect, Default)]
pub struct GeneralStats {
    pub guest_busy: Counter,
    pub int_ack: Counter,
    pub synth_int: Counter,
}

#[derive(Inspect, Default)]
pub struct ExitStats {
    pub automatic_exit: Counter,
    pub cpuid: Counter,
    pub hlt: Counter,
    pub intr: Counter,
    pub invd: Counter,
    pub invlpgb: Counter,
    pub ioio: Counter,
    pub msr_read: Counter,
    pub msr_write: Counter,
    pub npf: Counter,
    pub npf_no_intercept: Counter,
    pub npf_spurious: Counter,
    pub rdpmc: Counter,
    pub unexpected: Counter,
    pub vmgexit: Counter,
    pub vmmcall: Counter,
    pub xsetbv: Counter,
    pub excp_db: Counter,
}

/// The number of shared pages required per cpu.
pub const fn shared_pages_required_per_cpu() -> u64 {
    UhDirectOverlay::Count as u64
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
}

impl HardwareIsolatedBacking for SnpBacked {
    fn cvm_state_mut(&mut self) -> &mut UhCvmVpState {
        &mut self.cvm
    }

    fn cvm_partition_state(shared: &Self::Shared) -> &UhCvmPartitionState {
        &shared.cvm
    }

    fn switch_vtl_state(
        this: &mut UhProcessor<'_, Self>,
        source_vtl: GuestVtl,
        target_vtl: GuestVtl,
    ) {
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
            ss: from_seg(hv_seg_from_snp(&vmsa.ss())),
            encryption_mode: virt_support_x86emu::translate::EncryptionMode::Vtom(
                this.partition.caps.vtom.unwrap(),
            ),
        }
    }
}

/// Partition-wide shared data for SNP VPs.
#[derive(Inspect)]
pub struct SnpBackedShared {
    cvm: UhCvmPartitionState,
    invlpgb_count_max: u16,
    tsc_aux_virtualized: bool,
}

impl SnpBackedShared {
    pub fn new(params: BackingSharedParams) -> Result<Self, Error> {
        let cvm = params.cvm_state.unwrap();
        let invlpgb_count_max = x86defs::cpuid::ExtendedAddressSpaceSizesEdx::from(
            cvm.cpuid
                .registered_result(CpuidFunction::ExtendedAddressSpaceSizes, 0)
                .edx,
        )
        .invlpgb_count_max();
        let tsc_aux_virtualized = x86defs::cpuid::ExtendedSevFeaturesEax::from(
            cvm.cpuid
                .registered_result(CpuidFunction::ExtendedSevFeatures, 0)
                .eax,
        )
        .tsc_aux_virtualization();

        Ok(Self {
            invlpgb_count_max,
            tsc_aux_virtualized,
            cvm,
        })
    }
}

impl BackingPrivate for SnpBacked {
    type HclBacking = hcl::ioctl::snp::Snp;
    type Shared = SnpBackedShared;

    fn shared(shared: &BackingShared) -> &Self::Shared {
        let BackingShared::Snp(shared) = shared else {
            unreachable!()
        };
        shared
    }

    fn new(params: BackingParams<'_, '_, Self>, _shared: &SnpBackedShared) -> Result<Self, Error> {
        let pfns_handle = params
            .partition
            .shared_vis_pages_pool
            .as_ref()
            .expect("must have shared vis pool when using SNP")
            .alloc(
                NonZeroU64::new(shared_pages_required_per_cpu()).expect("is nonzero"),
                format!("direct overlay vp {}", params.vp_info.base.vp_index.index()),
            )
            .map_err(Error::AllocateSharedVisOverlay)?;
        let pfns = pfns_handle.base_pfn()..pfns_handle.base_pfn() + pfns_handle.size_pages();

        let overlays: Vec<_> = pfns.collect();

        Ok(Self {
            lapics: params.lapics.unwrap(),
            direct_overlays_pfns: overlays.try_into().unwrap(),
            direct_overlay_pfns_handle: pfns_handle,
            hv_sint_notifications: 0,
            general_stats: VtlArray::from_fn(|_| Default::default()),
            exit_stats: VtlArray::from_fn(|_| Default::default()),
            cvm: UhCvmVpState::new(params.hv.unwrap()),
        })
    }

    fn init(this: &mut UhProcessor<'_, Self>) {
        init_vmsa(
            &mut this.runner.vmsa_mut(GuestVtl::Vtl0),
            GuestVtl::Vtl0,
            this.partition.caps.vtom,
        );

        init_vmsa(
            &mut this.runner.vmsa_mut(GuestVtl::Vtl1),
            GuestVtl::Vtl1,
            this.partition.caps.vtom,
        );

        // Reset VMSA-backed state.
        let mut reset_state = |vtl: GuestVtl| {
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

            let cache_control =
                vp::CacheControl::at_reset(&this.partition.caps, &this.inner.vp_info);
            this.access_state(vtl.into())
                .set_cache_control(&cache_control)
                .expect("Resetting to architectural state should succeed");
        };

        reset_state(GuestVtl::Vtl0);
        reset_state(GuestVtl::Vtl1);

        // So far, only VTL 0 is using these (for VMBus). Initialize the direct
        // overlays for VTL 0.
        let pfns = &this.backing.direct_overlays_pfns;
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

    type StateAccess<'p, 'a> = UhVpStateAccess<'a, 'p, Self>
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
    ) -> Result<(), VpHaltReason<UhRunVpError>> {
        this.run_vp_snp(dev).await
    }

    fn poll_apic(
        this: &mut UhProcessor<'_, Self>,
        vtl: GuestVtl,
        scan_irr: bool,
    ) -> Result<(), UhRunVpError> {
        // Check for interrupt requests from the host.
        // TODO SNP GUEST VSM supporting VTL 1 proxy irrs requires kernel changes
        if vtl == GuestVtl::Vtl0 {
            if let Some(irr) = this.runner.proxy_irr() {
                // TODO SNP: filter proxy IRRs.
                this.backing.lapics[vtl].lapic.request_fixed_interrupts(irr);
            }
        }

        // Clear any pending interrupt.
        this.runner.vmsa_mut(vtl).v_intr_cntrl_mut().set_irq(false);

        let ApicWork {
            init,
            extint,
            sipi,
            nmi,
            interrupt,
        } = this.backing.lapics[vtl]
            .lapic
            .scan(&mut this.vmtime, scan_irr);

        if nmi {
            this.handle_nmi(vtl);
        }

        if let Some(vector) = interrupt {
            this.handle_interrupt(vtl, vector);
        }

        if extint {
            tracelimit::warn_ratelimited!("extint not supported");
        }

        // An INIT/SIPI targeted at a VP with more than one guest VTL enabled is ignored.
        // Check VTL enablement inside each block to avoid taking a lock on the hot path,
        // INIT and SIPI are quite cold.
        if init {
            if !*this.inner.hcvm_vtl1_enabled.lock() {
                this.handle_init(vtl)?;
            }
        }

        if let Some(vector) = sipi {
            if !*this.inner.hcvm_vtl1_enabled.lock() {
                this.handle_sipi(vtl, vector)?;
            }
        }

        Ok(())
    }

    fn request_extint_readiness(_this: &mut UhProcessor<'_, Self>) {
        unreachable!("extint managed through software apic")
    }

    fn request_untrusted_sint_readiness(
        this: &mut UhProcessor<'_, Self>,
        vtl: GuestVtl,
        sints: u16,
    ) {
        if vtl == GuestVtl::Vtl1 {
            todo!("TODO: handle untrusted sints for VTL1");
        }
        if this.backing.hv_sint_notifications & !sints == 0 {
            return;
        }
        this.backing.hv_sint_notifications |= sints;

        let notifications = HvDeliverabilityNotificationsRegister::new().with_sints(sints);
        tracing::trace!(?notifications, "setting notifications");
        this.runner
            .set_vp_register(
                vtl,
                HvX64RegisterName::DeliverabilityNotifications,
                u64::from(notifications).into(),
            )
            .expect("requesting deliverability is not a fallable operation");
    }

    fn handle_cross_vtl_interrupts(
        this: &mut UhProcessor<'_, Self>,
        dev: &impl CpuIo,
    ) -> Result<bool, UhRunVpError> {
        this.hcvm_handle_cross_vtl_interrupts(|this, vtl, check_rflags| {
            let vmsa = this.runner.vmsa_mut(vtl);
            if vmsa.event_inject().valid()
                && vmsa.event_inject().interruption_type() == x86defs::snp::SEV_INTR_TYPE_NMI
            {
                return true;
            }

            if (check_rflags && !x86defs::RFlags::from_bits(vmsa.rflags()).interrupt_enable())
                || vmsa.v_intr_cntrl().intr_shadow()
                || !vmsa.v_intr_cntrl().irq()
            {
                return false;
            }

            let vmsa_priority = vmsa.v_intr_cntrl().priority() as u32;
            let lapic = &mut this.backing.lapics[vtl].lapic;
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
            vmsa_priority > ppr_priority
        })
    }

    fn inspect_extra(this: &mut UhProcessor<'_, Self>, resp: &mut inspect::Response<'_>) {
        let vtl0_vmsa = this.runner.vmsa(GuestVtl::Vtl0);
        let vtl1_vmsa = if *this.inner.hcvm_vtl1_enabled.lock() {
            Some(this.runner.vmsa(GuestVtl::Vtl1))
        } else {
            None
        };

        let add_vmsa_inspect = |req: inspect::Request<'_>, vmsa: VmsaWrapper<'_, &SevVmsa>| {
            req.respond()
                .field("guest_error_code", inspect::AsHex(vmsa.guest_error_code()))
                .field("exit_info1", inspect::AsHex(vmsa.exit_info1()))
                .field("exit_info2", inspect::AsHex(vmsa.exit_info2()))
                .field(
                    "v_intr_cntrl",
                    inspect::AsHex(u64::from(vmsa.v_intr_cntrl())),
                );
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

    fn untrusted_synic(&self) -> Option<&ProcessorSynic> {
        None
    }

    fn untrusted_synic_mut(&mut self) -> Option<&mut ProcessorSynic> {
        None
    }
}

fn hv_seg_to_snp(val: &hvdef::HvX64SegmentRegister) -> SevSelector {
    SevSelector {
        selector: val.selector,
        attrib: (val.attributes & 0xFF) | ((val.attributes >> 4) & 0xF00),
        limit: val.limit,
        base: val.base,
    }
}

fn hv_table_to_snp(val: &hvdef::HvX64TableRegister) -> SevSelector {
    SevSelector {
        limit: val.limit as u32,
        base: val.base,
        ..FromZeroes::new_zeroed()
    }
}

fn hv_seg_from_snp(selector: &SevSelector) -> hvdef::HvX64SegmentRegister {
    hvdef::HvX64SegmentRegister {
        base: selector.base,
        limit: selector.limit,
        selector: selector.selector,
        attributes: (selector.attrib & 0xFF) | ((selector.attrib & 0xF00) << 4),
    }
}

fn hv_table_from_snp(selector: &SevSelector) -> hvdef::HvX64TableRegister {
    hvdef::HvX64TableRegister {
        limit: selector.limit as u16,
        base: selector.base,
        ..FromZeroes::new_zeroed()
    }
}

fn init_vmsa(vmsa: &mut VmsaWrapper<'_, &mut SevVmsa>, vtl: GuestVtl, vtom: Option<u64>) {
    // Query the SEV_FEATURES MSR to determine the features enabled on VTL2's VMSA
    // and use that to set btb_isolation, prevent_host_ibs, and VMSA register protection.
    let msr = devmsr::MsrDevice::new(0).expect("open msr");
    let sev_status = SevStatusMsr::from(msr.read_msr(x86defs::X86X_AMD_MSR_SEV).expect("read msr"));
    tracing::info!("VMSA creation {:?} {:?}", vtl, sev_status);

    // BUGBUG: this isn't fully accurate--the hypervisor can try running
    // from this at any time, so we need to be careful to set the field
    // that makes this valid last.
    vmsa.reset(sev_status.vmsa_reg_prot());
    vmsa.sev_features_mut()
        .set_snp_btb_isolation(sev_status.snp_btb_isolation());
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
        ],
    );

    // These are untrusted hypercalls from the hypervisor (hopefully originally
    // from the guest). Only allow HvPostMessage and HvSignalEvent.
    const UNTRUSTED_DISPATCHER: hv1_hypercall::Dispatcher<Self> = hv1_hypercall::dispatcher!(
        Self,
        [hv1_hypercall::HvPostMessage, hv1_hypercall::HvSignalEvent],
    );
}

struct GhcbEnlightenedHypercall<'a, 'b, 'c, T> {
    handler: UhHypercallHandler<'a, 'b, T, SnpBacked>,
    control: &'c mut u64,
    output_gpa: u64,
    input_gpa: u64,
    result: u64,
}

impl<'a, 'b, T> hv1_hypercall::AsHandler<UhHypercallHandler<'a, 'b, T, SnpBacked>>
    for &mut GhcbEnlightenedHypercall<'a, 'b, '_, T>
{
    fn as_handler(&mut self) -> &mut UhHypercallHandler<'a, 'b, T, SnpBacked> {
        &mut self.handler
    }
}

impl<T> HypercallIo for GhcbEnlightenedHypercall<'_, '_, '_, T> {
    fn advance_ip(&mut self) {
        // No-op for GHCB hypercall ABI
    }

    fn retry(&mut self, control: u64) {
        // TODO SNP: If we need to support resumption of rep hypercalls,
        // this will need the new start index.
        *self.control = control;
        self.set_result(HypercallOutput::from(HvError::Timeout).into())
    }

    fn control(&mut self) -> u64 {
        *self.control
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

impl UhProcessor<'_, SnpBacked> {
    fn handle_interrupt(&mut self, vtl: GuestVtl, vector: u8) {
        let mut vmsa = self.runner.vmsa_mut(vtl);
        vmsa.v_intr_cntrl_mut().set_vector(vector);
        vmsa.v_intr_cntrl_mut().set_priority((vector >> 4).into());
        vmsa.v_intr_cntrl_mut().set_ignore_tpr(false);
        vmsa.v_intr_cntrl_mut().set_irq(true);
        self.backing.lapics[vtl].halted = false;
    }

    fn handle_nmi(&mut self, vtl: GuestVtl) {
        // TODO SNP: support virtual NMI injection
        // For now, just inject an NMI and hope for the best.
        // Don't forget to update handle_cross_vtl_interrupts if this code changes.
        let mut vmsa = self.runner.vmsa_mut(vtl);
        vmsa.set_event_inject(
            SevEventInjectInfo::new()
                .with_interruption_type(x86defs::snp::SEV_INTR_TYPE_NMI)
                .with_vector(2)
                .with_valid(true),
        );

        self.backing.lapics[vtl].halted = false;
    }

    fn handle_init(&mut self, vtl: GuestVtl) -> Result<(), UhRunVpError> {
        assert_eq!(vtl, GuestVtl::Vtl0);
        let vp_info = self.inner.vp_info;
        let mut access = self.access_state(vtl.into());
        vp::x86_init(&mut access, &vp_info).map_err(UhRunVpError::State)?;
        Ok(())
    }

    fn handle_sipi(&mut self, vtl: GuestVtl, vector: u8) -> Result<(), UhRunVpError> {
        assert_eq!(vtl, GuestVtl::Vtl0);
        if self.backing.lapics[vtl].startup_suspend {
            let mut vmsa = self.runner.vmsa_mut(vtl);
            let address = (vector as u64) << 12;
            vmsa.set_cs(hv_seg_to_snp(&hvdef::HvX64SegmentRegister {
                base: address,
                limit: 0xffff,
                selector: (address >> 4) as u16,
                attributes: 0x9b,
            }));
            vmsa.set_rip(0);
            self.backing.lapics[vtl].startup_suspend = false;
            self.backing.lapics[vtl].halted = false;
        }
        Ok(())
    }

    fn handle_synic_deliverable_exit(&mut self) {
        let message = hvdef::HvX64SynicSintDeliverableMessage::ref_from_prefix(
            self.runner.exit_message().payload(),
        )
        .unwrap();

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
    ) -> Result<(), UhRunVpError> {
        let message = hvdef::HvX64VmgexitInterceptMessage::ref_from_prefix(
            self.runner.exit_message().payload(),
        )
        .unwrap();

        let ghcb_msr = x86defs::snp::GhcbMsr::from(message.ghcb_msr);
        tracing::trace!(?ghcb_msr, "vmgexit intercept");

        match x86defs::snp::GhcbInfo(ghcb_msr.info()) {
            x86defs::snp::GhcbInfo::NORMAL => {
                assert!(message.flags.ghcb_page_valid());
                let ghcb_pfn = ghcb_msr.pfn();

                let ghcb_overlay =
                    self.backing.direct_overlays_pfns[UhDirectOverlay::Ghcb as usize];

                // TODO SNP: Should allow arbitrary page to be used for GHCB
                if ghcb_pfn != ghcb_overlay {
                    tracelimit::warn_ratelimited!(
                        vmgexit_pfn = ghcb_pfn,
                        overlay_pfn = ghcb_overlay,
                        "ghcb page used for vmgexit does not match overlay page"
                    );

                    return Err(UhRunVpError::EmulationState(
                        hcl::ioctl::Error::InvalidRegisterValue,
                    ));
                }

                match x86defs::snp::GhcbUsage(message.ghcb_page.ghcb_usage) {
                    x86defs::snp::GhcbUsage::HYPERCALL => {
                        let guest_memory = &self.shared.cvm.shared_memory;
                        // Read GHCB parameters from guest memory before
                        // dispatching.
                        let overlay_base = ghcb_overlay * HV_PAGE_SIZE;
                        let x86defs::snp::GhcbHypercallParameters {
                            output_gpa,
                            mut input_control,
                        } = guest_memory
                            .read_plain(
                                overlay_base
                                    + x86defs::snp::GHCB_PAGE_HYPERCALL_PARAMETERS_OFFSET as u64,
                            )
                            .map_err(UhRunVpError::HypercallParameters)?;

                        let mut handler = GhcbEnlightenedHypercall {
                            handler: UhHypercallHandler {
                                vp: self,
                                bus: dev,
                                trusted: false,
                                intercepted_vtl,
                            },
                            control: &mut input_control,
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
                            .map_err(UhRunVpError::HypercallResult)?;

                        // Write the (potentially updated) control back to the GHCB as well.
                        guest_memory
                            .write_at(
                                overlay_base
                                    + x86defs::snp::GHCB_PAGE_HYPERCALL_PARAMETERS_OFFSET as u64
                                    + std::mem::offset_of!(
                                        x86defs::snp::GhcbHypercallParameters,
                                        input_control
                                    ) as u64,
                                input_control.as_bytes(),
                            )
                            .map_err(UhRunVpError::HypercallRetry)?;
                    }
                    usage => unimplemented!("ghcb usage {usage:?}"),
                }
            }
            info => unimplemented!("ghcb info {info:?}"),
        }

        Ok(())
    }

    #[must_use]
    fn sync_lazy_eoi(&mut self, vtl: GuestVtl) -> bool {
        if self.backing.lapics[vtl].lapic.is_lazy_eoi_pending() {
            return self.backing.cvm.hv[vtl].set_lazy_eoi();
        }

        false
    }

    async fn run_vp_snp(&mut self, dev: &impl CpuIo) -> Result<(), VpHaltReason<UhRunVpError>> {
        let next_vtl = self.backing.cvm.exit_vtl;

        let mut vmsa = self.runner.vmsa_mut(next_vtl);
        let last_interrupt_ctrl = vmsa.v_intr_cntrl();

        vmsa.v_intr_cntrl_mut().set_guest_busy(false);

        self.unlock_tlb_lock(Vtl::Vtl2);
        let tlb_halt = self.should_halt_for_tlb_unlock(next_vtl);

        let halt = self.backing.lapics[next_vtl].halted
            || self.backing.lapics[next_vtl].startup_suspend
            || tlb_halt;

        if halt && next_vtl == GuestVtl::Vtl1 && !tlb_halt {
            tracelimit::warn_ratelimited!("halting VTL 1, which might halt the guest");
        }

        self.runner.set_halted(halt);

        self.runner.set_exit_vtl(next_vtl);

        // Set the lazy EOI bit just before running.
        let lazy_eoi = self.sync_lazy_eoi(next_vtl);

        let mut has_intercept = self
            .runner
            .run()
            .map_err(|err| VpHaltReason::Hypervisor(UhRunVpError::Run(err)))?;

        let entered_from_vtl = next_vtl;
        let mut vmsa = self.runner.vmsa_mut(entered_from_vtl);

        // TODO SNP: The guest busy bit needs to be tested and set atomically.
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
            debug_assert!(exit_int_info.valid());

            let inject = match exit_int_info.interruption_type() {
                x86defs::snp::SEV_INTR_TYPE_EXCEPT => {
                    exit_int_info.vector() != 3 && exit_int_info.vector() != 4
                }
                x86defs::snp::SEV_INTR_TYPE_SW => false,
                _ => true,
            };

            if inject {
                vmsa.set_event_inject(exit_int_info);
            }
        }
        vmsa.v_intr_cntrl_mut().set_guest_busy(true);

        if last_interrupt_ctrl.irq() && !vmsa.v_intr_cntrl().irq() {
            self.backing.general_stats[entered_from_vtl]
                .int_ack
                .increment();
            // The guest has acknowledged the interrupt.
            self.backing.lapics[entered_from_vtl]
                .lapic
                .acknowledge_interrupt(last_interrupt_ctrl.vector());
        }

        vmsa.v_intr_cntrl_mut().set_irq(false);

        // Clear lazy EOI before processing the exit.
        if lazy_eoi && self.backing.cvm.hv[entered_from_vtl].clear_lazy_eoi() {
            self.backing.lapics[entered_from_vtl]
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
                let guest_state = crate::cvm_cpuid::CpuidGuestState {
                    xfem: vmsa.xcr0(),
                    xss: vmsa.xss(),
                    cr4: vmsa.cr4(),
                    apic_id: self.inner.vp_info.apic_id,
                };

                let result = self.shared.cvm.cpuid.guest_result(
                    CpuidFunction(vmsa.rax() as u32),
                    vmsa.rcx() as u32,
                    &guest_state,
                );

                let [eax, ebx, ecx, edx] = self.partition.cpuid.lock().result(
                    vmsa.rax() as u32,
                    vmsa.rcx() as u32,
                    &[result.eax, result.ebx, result.ecx, result.edx],
                );

                vmsa.set_rax(eax.into());
                vmsa.set_rbx(ebx.into());
                vmsa.set_rcx(ecx.into());
                vmsa.set_rdx(edx.into());
                advance_to_next_instruction(&mut vmsa);
                &mut self.backing.exit_stats[entered_from_vtl].cpuid
            }

            SevExitCode::MSR => {
                let is_write = vmsa.exit_info1() & 1 != 0;
                let msr = vmsa.rcx() as u32;

                let gp = if is_write {
                    let value = (vmsa.rax() as u32 as u64) | ((vmsa.rdx() as u32 as u64) << 32);
                    let r = self.backing.lapics[entered_from_vtl]
                        .lapic
                        .access(&mut SnpApicClient {
                            partition: self.partition,
                            vmsa,
                            dev,
                            vmtime: &self.vmtime,
                            vtl: entered_from_vtl,
                        })
                        .msr_write(msr, value)
                        .or_else_if_unknown(|| self.write_msr(msr, value, entered_from_vtl))
                        .or_else_if_unknown(|| {
                            self.write_msr_cvm(dev, msr, value, entered_from_vtl)
                        });

                    match r {
                        Ok(()) => false,
                        Err(MsrError::Unknown) => {
                            tracing::debug!(msr, value, "unknown cvm msr write");
                            false
                        }
                        Err(MsrError::InvalidAccess) => true,
                    }
                } else {
                    let r = self.backing.lapics[entered_from_vtl]
                        .lapic
                        .access(&mut SnpApicClient {
                            partition: self.partition,
                            vmsa,
                            dev,
                            vmtime: &self.vmtime,
                            vtl: entered_from_vtl,
                        })
                        .msr_read(msr)
                        .or_else_if_unknown(|| self.read_msr(msr, entered_from_vtl))
                        .or_else_if_unknown(|| self.read_msr_cvm(dev, msr, entered_from_vtl));

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

                if is_write {
                    &mut self.backing.exit_stats[entered_from_vtl].msr_write
                } else {
                    &mut self.backing.exit_stats[entered_from_vtl].msr_read
                }
            }

            SevExitCode::IOIO => {
                let io_info = x86defs::snp::SevIoAccessInfo::from(vmsa.exit_info1() as u32);
                if io_info.string_access() || io_info.rep_access() {
                    self.emulate(dev, false, entered_from_vtl).await?;
                } else {
                    let len = if io_info.access_size32() {
                        4
                    } else if io_info.access_size16() {
                        2
                    } else {
                        1
                    };

                    let mut rax = vmsa.rax();
                    emulate_io(
                        self.inner.vp_info.base.vp_index,
                        !io_info.read_access(),
                        io_info.port(),
                        &mut rax,
                        len,
                        dev,
                    )
                    .await;

                    let mut vmsa = self.runner.vmsa_mut(entered_from_vtl);
                    vmsa.set_rax(rax);
                    advance_to_next_instruction(&mut vmsa);
                }
                &mut self.backing.exit_stats[entered_from_vtl].ioio
            }

            SevExitCode::VMMCALL => {
                let is_64bit = self.long_mode(entered_from_vtl);
                let guest_memory = &self.partition.gm[entered_from_vtl];
                let handler = UhHypercallHandler {
                    trusted: !self.partition.hide_isolation,
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
                // Determine whether an NPF needs to be handled. If not, assume this fault is spurious
                // and that the instruction can be retried. The intercept itself may be presented by the
                // hypervisor as either a GPA intercept or an exception intercept.
                // The hypervisor configures the NPT to generate a #VC inside the guest for accesses to
                // unmapped memory. This means that accesses to unmapped memory for lower VTLs will be
                // forwarded to underhill as a #VC exception.
                let exit_info2 = vmsa.exit_info2();
                let exit_message = self.runner.exit_message();
                let payload = exit_message.payload();
                let emulate = match exit_message.header.typ {
                    HvMessageType::HvMessageTypeExceptionIntercept => {
                        let exception_message =
                            hvdef::HvX64ExceptionInterceptMessage::ref_from_prefix(payload)
                                .unwrap();

                        exception_message.vector
                            == x86defs::Exception::SEV_VMM_COMMUNICATION.0 as u16
                    }
                    HvMessageType::HvMessageTypeUnmappedGpa
                    | HvMessageType::HvMessageTypeGpaIntercept
                    | HvMessageType::HvMessageTypeUnacceptedGpa => {
                        // TODO GUEST VSM:
                        // - determine whether the intercept message should be delivered to VTL 1
                        // - determine whether emulation is appropriate for this gpa
                        let gpa_message: &hvdef::HvX64MemoryInterceptMessage =
                            hvdef::HvX64MemoryInterceptMessage::ref_from_prefix(payload).unwrap();

                        // Only the page numbers need to match.
                        (gpa_message.guest_physical_address >> hvdef::HV_PAGE_SHIFT)
                            == (exit_info2 >> hvdef::HV_PAGE_SHIFT)
                    }
                    _ => false,
                };

                if emulate {
                    has_intercept = false;
                    self.emulate(dev, false, entered_from_vtl).await?;
                    &mut self.backing.exit_stats[entered_from_vtl].npf
                } else {
                    &mut self.backing.exit_stats[entered_from_vtl].npf_spurious
                }
            }

            SevExitCode::NPF => &mut self.backing.exit_stats[entered_from_vtl].npf_no_intercept,

            SevExitCode::HLT => {
                self.backing.lapics[entered_from_vtl].halted = true;
                // RIP has already advanced. Clear interrupt shadow.
                vmsa.v_intr_cntrl_mut().set_intr_shadow(false);
                &mut self.backing.exit_stats[entered_from_vtl].hlt
            }

            SevExitCode::INVALID_VMCB => {
                return Err(VpHaltReason::InvalidVmState(UhRunVpError::InvalidVmcb));
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
                            .map_err(VpHaltReason::InvalidVmState)?;
                    }
                    _ => has_intercept = true,
                }
                &mut self.backing.exit_stats[entered_from_vtl].vmgexit
            }

            SevExitCode::NMI | SevExitCode::PAUSE | SevExitCode::SMI | SevExitCode::VMGEXIT => {
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
                if let Some(value) =
                    hardware_cvm::validate_xsetbv_exit(hardware_cvm::XsetbvExitInput {
                        rax: vmsa.rax(),
                        rcx: vmsa.rcx(),
                        rdx: vmsa.rdx(),
                        cr4: vmsa.cr4(),
                        cpl: vmsa.cpl(),
                    })
                {
                    vmsa.set_xcr0(value);
                    advance_to_next_instruction(&mut vmsa);
                } else {
                    vmsa.set_event_inject(
                        SevEventInjectInfo::new()
                            .with_interruption_type(x86defs::snp::SEV_INTR_TYPE_EXCEPT)
                            .with_vector(x86defs::Exception::GENERAL_PROTECTION_FAULT.0)
                            .with_deliver_error_code(true)
                            .with_valid(true),
                    );
                }
                &mut self.backing.exit_stats[entered_from_vtl].xsetbv
            }

            SevExitCode::EXCP_DB => &mut self.backing.exit_stats[entered_from_vtl].excp_db,

            _ => {
                debug_assert!(
                    false,
                    "Received unexpected exit code {}",
                    vmsa.guest_error_code()
                );
                &mut self.backing.exit_stats[entered_from_vtl].unexpected
            }
        };
        stat.increment();

        // Process debug exceptions before handling other intercepts.
        if cfg!(feature = "gdb") && sev_error_code == SevExitCode::EXCP_DB {
            return self.handle_debug_exception(entered_from_vtl);
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
                    tracelimit::error_ratelimited!(?message_type, "unknown synthetic exit");
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
}

impl<T: CpuIo> X86EmulatorSupport for UhEmulationState<'_, '_, T, SnpBacked> {
    type Error = UhRunVpError;

    fn vp_index(&self) -> VpIndex {
        self.vp.vp_index()
    }

    fn vendor(&self) -> x86defs::cpuid::Vendor {
        self.vp.partition.caps.vendor
    }

    fn state(&mut self) -> Result<x86emu::CpuState, Self::Error> {
        let vmsa = self.vp.runner.vmsa(self.vtl);
        Ok(x86emu::CpuState {
            gps: [
                vmsa.rax(),
                vmsa.rcx(),
                vmsa.rdx(),
                vmsa.rbx(),
                vmsa.rsp(),
                vmsa.rbp(),
                vmsa.rsi(),
                vmsa.rdi(),
                vmsa.r8(),
                vmsa.r9(),
                vmsa.r10(),
                vmsa.r11(),
                vmsa.r12(),
                vmsa.r13(),
                vmsa.r14(),
                vmsa.r15(),
            ],
            segs: [
                from_seg(hv_seg_from_snp(&vmsa.es())),
                from_seg(hv_seg_from_snp(&vmsa.cs())),
                from_seg(hv_seg_from_snp(&vmsa.ss())),
                from_seg(hv_seg_from_snp(&vmsa.ds())),
                from_seg(hv_seg_from_snp(&vmsa.fs())),
                from_seg(hv_seg_from_snp(&vmsa.gs())),
            ],
            rip: vmsa.rip(),
            rflags: vmsa.rflags().into(),
            cr0: vmsa.cr0(),
            efer: vmsa.efer(),
        })
    }

    fn set_state(&mut self, state: x86emu::CpuState) -> Result<(), Self::Error> {
        let mut vmsa = self.vp.runner.vmsa_mut(self.vtl);
        let x86emu::CpuState {
            gps: [rax, rcx, rdx, rbx, rsp, rbp, rsi, rdi, r8, r9, r10, r11, r12, r13, r14, r15],
            segs: _, // immutable
            rip,
            rflags,
            cr0: _,  // immutable
            efer: _, // immutable
        } = state;
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
        vmsa.set_rflags(rflags.into());
        Ok(())
    }

    fn get_xmm(&mut self, reg: usize) -> Result<u128, Self::Error> {
        Ok(self.vp.runner.vmsa(self.vtl).xmm_registers(reg))
    }

    fn set_xmm(&mut self, reg: usize, value: u128) -> Result<(), Self::Error> {
        self.vp
            .runner
            .vmsa_mut(self.vtl)
            .set_xmm_registers(reg, value);
        Ok(())
    }

    fn instruction_bytes(&self) -> &[u8] {
        &[]
    }

    fn physical_address(&self) -> Option<u64> {
        Some(self.vp.runner.vmsa(self.vtl).exit_info2())
    }

    fn initial_gva_translation(&self) -> Option<virt_support_x86emu::emulate::InitialTranslation> {
        None
    }

    fn interruption_pending(&self) -> bool {
        self.interruption_pending
    }

    fn check_vtl_access(
        &mut self,
        _gpa: u64,
        _mode: virt_support_x86emu::emulate::TranslateMode,
    ) -> Result<(), virt_support_x86emu::emulate::EmuCheckVtlAccessError<Self::Error>> {
        // TODO GUEST VSM
        // TODO lock tlb?
        Ok(())
    }

    fn translate_gva(
        &mut self,
        gva: u64,
        mode: virt_support_x86emu::emulate::TranslateMode,
    ) -> Result<
        Result<
            virt_support_x86emu::emulate::EmuTranslateResult,
            virt_support_x86emu::emulate::EmuTranslateError,
        >,
        Self::Error,
    > {
        emulate_translate_gva(self, gva, mode)
    }

    fn inject_pending_event(&mut self, event_info: hvdef::HvX64PendingEvent) {
        assert!(event_info.reg_0.event_pending());
        assert_eq!(
            event_info.reg_0.event_type(),
            hvdef::HV_X64_PENDING_EVENT_EXCEPTION
        );

        let exception = HvX64PendingExceptionEvent::from(u128::from(event_info.reg_0));

        self.vp.runner.vmsa_mut(self.vtl).set_event_inject(
            SevEventInjectInfo::new()
                .with_valid(true)
                .with_interruption_type(x86defs::snp::SEV_INTR_TYPE_EXCEPT)
                .with_vector(exception.vector() as u8)
                .with_deliver_error_code(exception.deliver_error_code())
                .with_error_code(exception.error_code()),
        );
    }

    fn is_gpa_mapped(&self, gpa: u64, write: bool) -> bool {
        // Ignore the VTOM address bit when checking, since memory is mirrored
        // across the VTOM.
        let vtom = self.vp.partition.caps.vtom.unwrap();
        debug_assert!(vtom == 0 || vtom.is_power_of_two());
        self.vp.partition.is_gpa_mapped(gpa & !vtom, write)
    }

    fn lapic_base_address(&self) -> Option<u64> {
        self.vp.backing.lapics[self.vtl].lapic.base_address()
    }

    fn lapic_read(&mut self, address: u64, data: &mut [u8]) {
        let vtl = self.vtl;
        self.vp.backing.lapics[vtl]
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
        self.vp.backing.lapics[vtl]
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

#[allow(unused)]
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
            cs: hv_seg_from_snp(&vmsa.cs()).into(),
            ds: hv_seg_from_snp(&vmsa.ds()).into(),
            es: hv_seg_from_snp(&vmsa.es()).into(),
            fs: hv_seg_from_snp(&vmsa.fs()).into(),
            gs: hv_seg_from_snp(&vmsa.gs()).into(),
            ss: hv_seg_from_snp(&vmsa.ss()).into(),
            tr: hv_seg_from_snp(&vmsa.tr()).into(),
            ldtr: hv_seg_from_snp(&vmsa.ldtr()).into(),
            gdtr: hv_table_from_snp(&vmsa.gdtr()).into(),
            idtr: hv_table_from_snp(&vmsa.idtr()).into(),
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
        vmsa.set_cs(hv_seg_to_snp(&cs.into()));
        vmsa.set_ds(hv_seg_to_snp(&ds.into()));
        vmsa.set_es(hv_seg_to_snp(&es.into()));
        vmsa.set_fs(hv_seg_to_snp(&fs.into()));
        vmsa.set_gs(hv_seg_to_snp(&gs.into()));
        vmsa.set_ss(hv_seg_to_snp(&ss.into()));
        vmsa.set_tr(hv_seg_to_snp(&tr.into()));
        vmsa.set_ldtr(hv_seg_to_snp(&ldtr.into()));
        vmsa.set_gdtr(hv_table_to_snp(&gdtr.into()));
        vmsa.set_idtr(hv_table_to_snp(&idtr.into()));
        vmsa.set_cr0(cr0);
        vmsa.set_cr2(cr2);
        vmsa.set_cr3(cr3);
        vmsa.set_cr4(cr4);
        vmsa.v_intr_cntrl_mut().set_tpr(cr8 as u8);
        vmsa.set_efer(SnpBacked::calculate_efer(efer, cr0));
        Ok(())
    }

    fn activity(&mut self) -> Result<vp::Activity, Self::Error> {
        let lapic = &self.vp.backing.lapics[self.vtl];
        let mp_state = if lapic.startup_suspend {
            vp::MpState::WaitForSipi
        } else if lapic.halted {
            vp::MpState::Halted
        } else {
            vp::MpState::Running
        };
        Ok(vp::Activity {
            mp_state,
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
        let (halted, startup_suspend) = match mp_state {
            vp::MpState::Running => (false, false),
            vp::MpState::WaitForSipi => (false, true),
            vp::MpState::Halted => (true, false),
            vp::MpState::Idle => (false, false), // TODO SNP: idle support
        };
        let lapic = &mut self.vp.backing.lapics[self.vtl];
        lapic.halted = halted;
        lapic.startup_suspend = startup_suspend;
        lapic.nmi_pending = nmi_pending;
        Ok(())
    }

    fn xsave(&mut self) -> Result<vp::Xsave, Self::Error> {
        Err(vp_state::Error::Unimplemented("xsave"))
    }

    fn set_xsave(&mut self, value: &vp::Xsave) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("xsave"))
    }

    fn apic(&mut self) -> Result<vp::Apic, Self::Error> {
        Ok(self.vp.backing.lapics[self.vtl].lapic.save())
    }

    fn set_apic(&mut self, value: &vp::Apic) -> Result<(), Self::Error> {
        self.vp.backing.lapics[self.vtl]
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

    fn cache_control(&mut self) -> Result<vp::CacheControl, Self::Error> {
        let vmsa = self.vp.runner.vmsa(self.vtl);
        Ok(vp::CacheControl {
            msr_cr_pat: vmsa.pat(),
            msr_mtrr_def_type: 0,
            fixed: [0; 11],
            variable: [0; 16],
        })
    }

    fn set_cache_control(&mut self, value: &vp::CacheControl) -> Result<(), Self::Error> {
        let vp::CacheControl {
            msr_cr_pat,
            msr_mtrr_def_type: _,
            fixed: _,
            variable: _,
        } = *value;
        self.vp.runner.vmsa_mut(self.vtl).set_pat(msr_cr_pat);
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

    fn set_tsc(&mut self, value: &vp::Tsc) -> Result<(), Self::Error> {
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

    fn set_synic_msrs(&mut self, value: &vp::SyntheticMsrs) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_msrs"))
    }

    fn synic_message_page(&mut self) -> Result<vp::SynicMessagePage, Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_message_page"))
    }

    fn set_synic_message_page(&mut self, value: &vp::SynicMessagePage) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_message_page"))
    }

    fn synic_event_flags_page(&mut self) -> Result<vp::SynicEventFlagsPage, Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_event_flags_page"))
    }

    fn set_synic_event_flags_page(
        &mut self,
        value: &vp::SynicEventFlagsPage,
    ) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_event_flags_page"))
    }

    fn synic_message_queues(&mut self) -> Result<vp::SynicMessageQueues, Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_message_queues"))
    }

    fn set_synic_message_queues(
        &mut self,
        value: &vp::SynicMessageQueues,
    ) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_message_queues"))
    }

    fn synic_timers(&mut self) -> Result<vp::SynicTimers, Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_timers"))
    }

    fn set_synic_timers(&mut self, value: &vp::SynicTimers) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_timers"))
    }
}

/// Advances rip to be the same as next_rip.
fn advance_to_next_instruction(vmsa: &mut VmsaWrapper<'_, &mut SevVmsa>) {
    vmsa.set_rip(vmsa.next_rip());
    vmsa.v_intr_cntrl_mut().set_intr_shadow(false);
}

impl UhProcessor<'_, SnpBacked> {
    fn read_msr_cvm(
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
            _ => return Err(MsrError::Unknown),
        };
        Ok(value)
    }

    fn write_msr_cvm(
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

impl<T: CpuIo> hv1_hypercall::EnablePartitionVtl for UhHypercallHandler<'_, '_, T, SnpBacked> {
    fn enable_partition_vtl(
        &mut self,
        partition_id: u64,
        target_vtl: Vtl,
        flags: hvdef::hypercall::EnablePartitionVtlFlags,
    ) -> hvdef::HvResult<()> {
        self.hcvm_enable_partition_vtl(partition_id, target_vtl, flags)
    }
}

impl<T: CpuIo> hv1_hypercall::EnableVpVtl<hvdef::hypercall::InitialVpContextX64>
    for UhHypercallHandler<'_, '_, T, SnpBacked>
{
    fn enable_vp_vtl(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        vtl: Vtl,
        vp_context: &hvdef::hypercall::InitialVpContextX64,
    ) -> hvdef::HvResult<()> {
        self.hcvm_enable_vp_vtl(partition_id, vp_index, vtl, vp_context)
    }
}

impl<T: CpuIo> hv1_hypercall::RetargetDeviceInterrupt for UhHypercallHandler<'_, '_, T, SnpBacked> {
    fn retarget_interrupt(
        &mut self,
        device_id: u64,
        address: u64,
        data: u32,
        params: &hv1_hypercall::HvInterruptParameters<'_>,
    ) -> hvdef::HvResult<()> {
        self.hcvm_retarget_interrupt(
            device_id,
            address,
            data,
            params.vector,
            params.multicast,
            params.target_processors,
        )
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
        processor_set: Vec<u32>,
        flags: HvFlushFlags,
        gva_ranges: &[HvGvaRange],
    ) -> hvdef::HvRepResult {
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
        processor_set: Vec<u32>,
        flags: HvFlushFlags,
        gva_ranges: &[HvGvaRange],
    ) -> hvdef::HvRepResult {
        self.hcvm_validate_flush_inputs(&processor_set, flags, true)
            .map_err(|e| (e, 0))?;

        // As a performance optimization if we are asked to do too large an amount of work
        // just do a flush entire instead.
        if gva_ranges.len() > 16 || gva_ranges.iter().any(|range| if flags.use_extended_range_format() { range.as_extended().additional_pages() } else { range.as_simple().additional_pages() } > 16) {
            self.do_flush_virtual_address_space(&processor_set, flags);
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
        processor_set: Vec<u32>,
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
        processor_set: Vec<u32>,
        flags: HvFlushFlags,
    ) -> hvdef::HvResult<()> {
        self.hcvm_validate_flush_inputs(&processor_set, flags, false)?;

        self.do_flush_virtual_address_space(&processor_set, flags);

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

    fn do_flush_virtual_address_space(&mut self, processor_set: &[u32], flags: HvFlushFlags) {
        let only_self = processor_set.len() == 1 && processor_set[0] == self.vp.vp_index().index();
        if only_self && flags.non_global_mappings_only() {
            self.vp.runner.vmsa_mut(self.intercepted_vtl).set_pcpu_id(0);
        } else {
            let rax = SevInvlpgbRax::new()
                .with_asid_valid(true)
                .with_global(!flags.non_global_mappings_only());
            let ecx = SevInvlpgbEcx::new();
            let edx = SevInvlpgbEdx::new();
            self.vp
                .partition
                .hcl
                .invlpgb(rax.into(), edx.into(), ecx.into());
            self.vp.partition.hcl.tlbsync();
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
