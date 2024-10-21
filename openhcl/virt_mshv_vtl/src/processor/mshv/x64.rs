// Copyright (C) Microsoft Corporation. All rights reserved.

//! X64 Processor support for Microsoft hypervisor-backed partitions.

#![cfg(guest_arch = "x86_64")]

type VpRegisterName = HvX64RegisterName;

use super::super::private::BackingParams;
use super::super::signal_mnf;
use super::super::vp_state;
use super::super::vp_state::UhVpStateAccess;
use super::super::BackingPrivate;
use super::super::UhEmulationState;
use super::super::UhRunVpError;
use crate::processor::from_seg;
use crate::processor::mshv::apic;
use crate::processor::BackingSharedParams;
use crate::processor::SidecarExitReason;
use crate::processor::SidecarRemoveExit;
use crate::processor::UhHypercallHandler;
use crate::processor::UhProcessor;
use crate::validate_vtl_gpa_flags;
use crate::Error;
use crate::GuestVsmState;
use crate::GuestVsmVtl1State;
use crate::GuestVsmVtl1StateInner;
use crate::GuestVtl;
use hcl::ioctl;
use hcl::ioctl::ApplyVtlProtectionsError;
use hcl::protocol;
use hvdef::hypercall;
use hvdef::HvDeliverabilityNotificationsRegister;
use hvdef::HvError;
use hvdef::HvInterceptAccessType;
use hvdef::HvMapGpaFlags;
use hvdef::HvMessageType;
use hvdef::HvRegisterValue;
use hvdef::HvRegisterVsmPartitionConfig;
use hvdef::HvRegisterVsmPartitionStatus;
use hvdef::HvX64InterceptMessageHeader;
use hvdef::HvX64PendingEvent;
use hvdef::HvX64RegisterName;
use hvdef::Vtl;
use hvdef::HV_PAGE_SIZE;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use std::sync::atomic::Ordering::Relaxed;
use virt::io::CpuIo;
use virt::state::HvRegisterState;
use virt::state::StateElement;
use virt::vp;
use virt::vp::AccessVpState;
use virt::x86::MsrError;
use virt::x86::MsrErrorExt;
use virt::StopVp;
use virt::VpHaltReason;
use virt::VpIndex;
use virt_support_x86emu::emulate::EmuCheckVtlAccessError;
use virt_support_x86emu::emulate::EmuTranslateError;
use virt_support_x86emu::emulate::EmuTranslateResult;
use virt_support_x86emu::emulate::EmulatorSupport;
use vtl_array::VtlArray;
use vtl_array::VtlSet;
use x86defs::xsave::Fxsave;
use x86defs::xsave::XsaveHeader;
use x86defs::xsave::XFEATURE_SSE;
use x86defs::xsave::XFEATURE_X87;
use zerocopy::AsBytes;
use zerocopy::FromBytes;
use zerocopy::FromZeroes;

/// A backing for hypervisor-backed partitions (non-isolated and
/// software-isolated).
#[derive(InspectMut)]
pub struct HypervisorBackedX86 {
    /// Underhill APIC state
    pub(super) lapics: Option<VtlArray<apic::UhApicState, 2>>,
    // TODO WHP GUEST VSM: To be completely correct here, when emulating the APICs
    // we would need two sets of deliverability notifications too. However currently
    // we don't support VTL 1 on WHP, and on the hypervisor we don't emulate the APIC,
    // so this can wait.
    #[inspect(with = "|x| inspect::AsHex(u64::from(*x))")]
    deliverability_notifications: HvDeliverabilityNotificationsRegister,
    /// Next set of deliverability notifications. See register definition for details.
    #[inspect(with = "|x| inspect::AsHex(u64::from(*x))")]
    pub(super) next_deliverability_notifications: HvDeliverabilityNotificationsRegister,
    stats: ProcessorStatsX86,
}

#[derive(Inspect, Default)]
struct ProcessorStatsX86 {
    io_port: Counter,
    mmio: Counter,
    unaccepted_gpa: Counter,
    hypercall: Counter,
    synic_deliverable: Counter,
    interrupt_deliverable: Counter,
    cpuid: Counter,
    msr: Counter,
    eoi: Counter,
    unrecoverable_exception: Counter,
    halt: Counter,
    exception_intercept: Counter,
}

impl BackingPrivate for HypervisorBackedX86 {
    type HclBacking = ioctl::x64::MshvX64;
    type BackingShared = ();

    fn new_shared_state(_params: BackingSharedParams<'_>) -> Result<Self::BackingShared, Error> {
        Ok(())
    }

    fn new(params: BackingParams<'_, '_, Self>) -> Result<Self, Error> {
        // Initialize shared register state to architectural state. The kernel
        // zero initializes this.
        //
        // When restoring, this will be overwritten, but it's not expensive
        // enough to bother skipping.
        let regs = vp::Registers::at_reset(&params.partition.caps, params.vp_info);
        *params.runner.cpu_context_mut() = protocol::hcl_cpu_context_x64 {
            gps: [
                regs.rax, regs.rcx, regs.rdx, regs.rbx, 0, /* cr2 */
                regs.rbp, regs.rsi, regs.rdi, regs.r8, regs.r9, regs.r10, regs.r11, regs.r12,
                regs.r13, regs.r14, regs.r15,
            ],
            fx_state: vp::Xsave::at_reset(&params.partition.caps, params.vp_info).fxsave(),
            reserved: [0; 384],
        };

        let lapics = params.partition.lapic.as_ref().map(|arr| {
            // Initialize APIC base to match the current VM state.
            let apic_base = params
                .runner
                .get_vp_register(HvX64RegisterName::ApicBase)
                .unwrap()
                .as_u64();
            let mut lapic0 = arr[GuestVtl::Vtl0].add_apic(params.vp_info);
            lapic0.set_apic_base(apic_base).unwrap();
            let mut lapic1 = arr[GuestVtl::Vtl1].add_apic(params.vp_info);
            lapic1.set_apic_base(apic_base).unwrap();

            [
                apic::UhApicState::new(lapic0, GuestVtl::Vtl0, &params.vp_info.base),
                apic::UhApicState::new(lapic1, GuestVtl::Vtl1, &params.vp_info.base),
            ]
            .into()
        });

        Ok(Self {
            lapics,
            deliverability_notifications: Default::default(),
            next_deliverability_notifications: Default::default(),
            stats: Default::default(),
        })
    }

    fn init(_this: &mut UhProcessor<'_, Self>) {}

    type StateAccess<'p, 'a> = UhVpStateAccess<'a, 'p, Self> where Self: 'a + 'p, 'p: 'a;

    fn access_vp_state<'a, 'p>(
        this: &'a mut UhProcessor<'p, Self>,
        vtl: GuestVtl,
    ) -> Self::StateAccess<'p, 'a> {
        assert_eq!(vtl, GuestVtl::Vtl0);
        UhVpStateAccess::new(this, vtl)
    }

    async fn run_vp(
        this: &mut UhProcessor<'_, Self>,
        dev: &impl CpuIo,
        stop: &mut StopVp<'_>,
    ) -> Result<(), VpHaltReason<UhRunVpError>> {
        if this.backing.deliverability_notifications
            != this.backing.next_deliverability_notifications
        {
            let notifications = this.backing.next_deliverability_notifications;
            tracing::trace!(?notifications, "setting notifications");
            this.runner
                .set_vp_register(
                    VpRegisterName::DeliverabilityNotifications,
                    u64::from(notifications).into(),
                )
                .expect("requesting deliverability is not a fallable operation");
            this.backing.deliverability_notifications =
                this.backing.next_deliverability_notifications;
        }

        let intercepted = if this.runner.is_sidecar() {
            let mut run = this
                .runner
                .run_sidecar()
                .map_err(|e| VpHaltReason::Hypervisor(UhRunVpError::Run(e)))?;
            match stop.until_stop(run.wait()).await {
                Ok(r) => r,
                Err(stop) => {
                    run.cancel();
                    let r = run.wait().await;
                    if matches!(r, Ok(false)) {
                        // No intercept, so stop the VP.
                        return Err(stop.into());
                    }
                    r
                }
            }
            .map_err(|e| VpHaltReason::Hypervisor(UhRunVpError::Sidecar(e)))?
        } else {
            this.unlock_tlb_lock(Vtl::Vtl2);
            this.runner
                .run()
                .map_err(|e| VpHaltReason::Hypervisor(UhRunVpError::Run(e)))?
        };

        if intercepted {
            let stat = match this.runner.exit_message().header.typ {
                HvMessageType::HvMessageTypeX64IoPortIntercept => {
                    this.handle_io_port_exit(dev).await?;
                    &mut this.backing.stats.io_port
                }
                HvMessageType::HvMessageTypeUnmappedGpa
                | HvMessageType::HvMessageTypeGpaIntercept => {
                    this.handle_mmio_exit(dev).await?;
                    &mut this.backing.stats.mmio
                }
                HvMessageType::HvMessageTypeUnacceptedGpa => {
                    this.handle_unaccepted_gpa_intercept(dev).await?;
                    &mut this.backing.stats.unaccepted_gpa
                }
                HvMessageType::HvMessageTypeHypercallIntercept => {
                    this.handle_hypercall_exit(dev)?;
                    &mut this.backing.stats.hypercall
                }
                HvMessageType::HvMessageTypeSynicSintDeliverable => {
                    this.handle_synic_deliverable_exit();
                    &mut this.backing.stats.synic_deliverable
                }
                HvMessageType::HvMessageTypeX64InterruptionDeliverable => {
                    this.handle_interrupt_deliverable_exit(dev)?;
                    &mut this.backing.stats.interrupt_deliverable
                }
                HvMessageType::HvMessageTypeX64CpuidIntercept => {
                    this.handle_cpuid_intercept()?;
                    &mut this.backing.stats.cpuid
                }
                HvMessageType::HvMessageTypeMsrIntercept => {
                    this.handle_msr_intercept(dev)?;
                    &mut this.backing.stats.msr
                }
                HvMessageType::HvMessageTypeX64ApicEoi => {
                    this.handle_eoi(dev)?;
                    &mut this.backing.stats.eoi
                }
                HvMessageType::HvMessageTypeUnrecoverableException => {
                    this.handle_unrecoverable_exception()?;
                    &mut this.backing.stats.unrecoverable_exception
                }
                HvMessageType::HvMessageTypeX64Halt => {
                    this.handle_halt()?;
                    &mut this.backing.stats.halt
                }
                HvMessageType::HvMessageTypeExceptionIntercept => {
                    this.handle_exception()?;
                    &mut this.backing.stats.exception_intercept
                }
                reason => unreachable!("unknown exit reason: {:#x?}", reason),
            };
            stat.increment();

            if this.runner.is_sidecar() && !this.partition.no_sidecar_hotplug.load(Relaxed) {
                // We got and handled an exit and this is a sidecar VP. Cancel
                // the run so that we can move the sidecar VP over to the main
                // kernel and handle future exits there.
                //
                // This is not strictly necessary--we can continue to run the VP
                // in the sidecar kernel. But since we have received at least
                // one exit, we can expect that we will receive more, and
                // handling the exits remotely introduces jitter.
                let message = this.runner.exit_message();
                this.inner
                    .set_sidecar_exit_reason(SidecarExitReason::Exit(parse_sidecar_exit(message)));
                return Err(VpHaltReason::Cancel);
            }
        }
        Ok(())
    }

    fn poll_apic(
        this: &mut UhProcessor<'_, Self>,
        vtl: GuestVtl,
        scan_irr: bool,
    ) -> Result<bool, UhRunVpError> {
        this.poll_apic(vtl, scan_irr)
    }

    fn request_extint_readiness(this: &mut UhProcessor<'_, Self>) {
        this.backing
            .next_deliverability_notifications
            .set_interrupt_notification(true);
    }

    fn request_untrusted_sint_readiness(this: &mut UhProcessor<'_, Self>, sints: u16) {
        this.backing
            .next_deliverability_notifications
            .set_sints(this.backing.next_deliverability_notifications.sints() | sints);
    }

    // If there's no register page, assume only VTL0 is supported.
    fn last_vtl(this: &UhProcessor<'_, Self>) -> GuestVtl {
        this.runner
            .reg_page_vtl()
            .map_or(GuestVtl::Vtl0, |vtl| vtl.try_into().unwrap())
    }

    fn switch_vtl_state(_this: &mut UhProcessor<'_, Self>, _target_vtl: GuestVtl) {
        unreachable!("vtl switching should be managed by the hypervisor");
    }
}

fn parse_sidecar_exit(message: &hvdef::HvMessage) -> SidecarRemoveExit {
    match message.header.typ {
        HvMessageType::HvMessageTypeX64IoPortIntercept => {
            let message =
                hvdef::HvX64IoPortInterceptMessage::ref_from_prefix(message.payload()).unwrap();
            SidecarRemoveExit::Io {
                port: message.port_number,
                write: message.header.intercept_access_type == HvInterceptAccessType::WRITE,
            }
        }
        HvMessageType::HvMessageTypeUnmappedGpa | HvMessageType::HvMessageTypeGpaIntercept => {
            let message =
                hvdef::HvX64MemoryInterceptMessage::ref_from_prefix(message.payload()).unwrap();
            SidecarRemoveExit::Mmio {
                gpa: message.guest_physical_address,
                write: message.header.intercept_access_type == HvInterceptAccessType::WRITE,
            }
        }
        HvMessageType::HvMessageTypeHypercallIntercept => {
            let message =
                hvdef::HvX64HypercallInterceptMessage::ref_from_prefix(message.payload()).unwrap();
            let is_64bit = message.header.execution_state.cr0_pe()
                && message.header.execution_state.efer_lma();
            let control = if is_64bit {
                message.rcx
            } else {
                (message.rdx << 32) | (message.rax as u32 as u64)
            };
            SidecarRemoveExit::Hypercall {
                code: hvdef::HypercallCode(hypercall::Control::from(control).code()),
            }
        }
        HvMessageType::HvMessageTypeX64CpuidIntercept => {
            let message =
                hvdef::HvX64CpuidInterceptMessage::ref_from_prefix(message.payload()).unwrap();
            SidecarRemoveExit::Cpuid {
                leaf: message.rax as u32,
                subleaf: message.rcx as u32,
            }
        }
        HvMessageType::HvMessageTypeMsrIntercept => {
            let message =
                hvdef::HvX64MsrInterceptMessage::ref_from_prefix(message.payload()).unwrap();
            SidecarRemoveExit::Msr {
                msr: message.msr_number,
                value: (message.header.intercept_access_type == HvInterceptAccessType::WRITE)
                    .then_some((message.rdx << 32) | message.rax as u32 as u64),
            }
        }
        typ => SidecarRemoveExit::Hypervisor { message: typ },
    }
}

fn next_rip(value: &HvX64InterceptMessageHeader) -> u64 {
    value.rip.wrapping_add(value.instruction_len() as u64)
}

impl UhProcessor<'_, HypervisorBackedX86> {
    fn set_rip(&mut self, rip: u64) -> Result<(), VpHaltReason<UhRunVpError>> {
        self.runner
            .set_vp_register(HvX64RegisterName::Rip, rip.into())
            .map_err(|e| VpHaltReason::Hypervisor(UhRunVpError::AdvanceRip(e)))?;

        Ok(())
    }

    fn handle_interrupt_deliverable_exit(
        &mut self,
        bus: &impl CpuIo,
    ) -> Result<(), VpHaltReason<UhRunVpError>> {
        let message = hvdef::HvX64InterruptionDeliverableMessage::ref_from_prefix(
            self.runner.exit_message().payload(),
        )
        .unwrap();

        assert_eq!(
            message.deliverable_type,
            hvdef::HvX64PendingInterruptionType::HV_X64_PENDING_INTERRUPT
        );

        self.backing
            .deliverability_notifications
            .set_interrupt_notification(false);

        self.backing
            .next_deliverability_notifications
            .set_interrupt_notification(false);

        if let Some(vector) = bus.acknowledge_pic_interrupt() {
            let event = hvdef::HvX64PendingExtIntEvent::new()
                .with_event_pending(true)
                .with_event_type(hvdef::HV_X64_PENDING_EVENT_EXT_INT)
                .with_vector(vector);

            self.runner
                .set_vp_register(HvX64RegisterName::PendingEvent0, u128::from(event).into())
                .map_err(|e| VpHaltReason::Hypervisor(UhRunVpError::Event(e)))?;
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

        self.backing.deliverability_notifications.set_sints(
            self.backing.deliverability_notifications.sints() & !message.deliverable_sints,
        );

        // This is updated by `deliver_synic_messages below`, so clear it here.
        self.backing.next_deliverability_notifications.set_sints(0);

        // These messages are always VTL0, as VTL1 does not own any VMBUS channels.
        self.deliver_synic_messages(GuestVtl::Vtl0, message.deliverable_sints);
    }

    fn handle_hypercall_exit(
        &mut self,
        bus: &impl CpuIo,
    ) -> Result<(), VpHaltReason<UhRunVpError>> {
        let message = hvdef::HvX64HypercallInterceptMessage::ref_from_prefix(
            self.runner.exit_message().payload(),
        )
        .unwrap();

        tracing::trace!(msg = %format_args!("{:x?}", message), "hypercall");

        let is_64bit =
            message.header.execution_state.cr0_pe() && message.header.execution_state.efer_lma();

        let guest_memory = self.last_vtl_gm();
        let handler = UhHypercallHandler {
            vp: self,
            bus,
            trusted: false,
        };
        UhHypercallHandler::MSHV_DISPATCHER.dispatch(
            guest_memory,
            hv1_hypercall::X64RegisterIo::new(handler, is_64bit),
        );

        Ok(())
    }

    async fn handle_mmio_exit(
        &mut self,
        dev: &impl CpuIo,
    ) -> Result<(), VpHaltReason<UhRunVpError>> {
        let message = hvdef::HvX64MemoryInterceptMessage::ref_from_prefix(
            self.runner.exit_message().payload(),
        )
        .unwrap();

        tracing::trace!(msg = %format_args!("{:x?}", message), "mmio");

        let interruption_pending = message.header.execution_state.interruption_pending();

        // Fast path for monitor page writes.
        if Some(message.guest_physical_address & !(HV_PAGE_SIZE - 1))
            == self.partition.monitor_page.gpa()
            && message.header.intercept_access_type == HvInterceptAccessType::WRITE
        {
            let instruction_bytes = message.instruction_bytes;
            let instruction_bytes = &instruction_bytes[..message.instruction_byte_count as usize];
            let tlb_lock_held = message.memory_access_info.gva_gpa_valid()
                || message.memory_access_info.tlb_locked();
            let mut state = self.emulator_state();
            if let Some(bit) = virt_support_x86emu::emulate::emulate_mnf_write_fast_path(
                instruction_bytes,
                &mut state,
                interruption_pending,
                tlb_lock_held,
            ) {
                self.set_emulator_state(&state);
                if let Some(connection_id) = self.partition.monitor_page.write_bit(bit) {
                    signal_mnf(dev, connection_id);
                }
                return Ok(());
            }
        }

        self.emulate(dev, interruption_pending).await?;
        Ok(())
    }

    async fn handle_io_port_exit(
        &mut self,
        dev: &impl CpuIo,
    ) -> Result<(), VpHaltReason<UhRunVpError>> {
        let message = hvdef::HvX64IoPortInterceptMessage::ref_from_prefix(
            self.runner.exit_message().payload(),
        )
        .unwrap();

        tracing::trace!(msg = %format_args!("{:x?}", message), "io_port");

        assert_eq!(message.rax, self.runner.cpu_context().gps[protocol::RAX]);

        let interruption_pending = message.header.execution_state.interruption_pending();

        if message.access_info.string_op() || message.access_info.rep_prefix() {
            self.emulate(dev, interruption_pending).await
        } else {
            let next_rip = next_rip(&message.header);
            let access_size = message.access_info.access_size();
            virt_support_x86emu::emulate::emulate_io(
                self.vp_index(),
                message.header.intercept_access_type == HvInterceptAccessType::WRITE,
                message.port_number,
                &mut self.runner.cpu_context_mut().gps[protocol::RAX],
                access_size,
                dev,
            )
            .await;
            self.set_rip(next_rip)
        }
    }

    async fn handle_unaccepted_gpa_intercept(
        &mut self,
        dev: &impl CpuIo,
    ) -> Result<(), VpHaltReason<UhRunVpError>> {
        let gpa = hvdef::HvX64MemoryInterceptMessage::ref_from_prefix(
            self.runner.exit_message().payload(),
        )
        .unwrap()
        .guest_physical_address;

        if self.partition.is_gpa_lower_vtl_ram(gpa) {
            // The host may have moved the page to an unaccepted state, so fail
            // here. This does not apply to VTL 2 memory - for unaccepted pages,
            // the intercept goes to host VTL0.
            //
            // Note: SGX memory should be included in this check, so if SGX is
            // no longer included in the lower_vtl_memory_layout, make sure the
            // appropriate changes are reflected here.
            Err(VpHaltReason::InvalidVmState(
                UhRunVpError::UnacceptedMemoryAccess(gpa),
            ))
        } else {
            // TODO SNP: for hardware isolation, if the intercept is due to a guest
            // error, inject a machine check
            self.handle_mmio_exit(dev).await?;
            Ok(())
        }
    }

    fn handle_cpuid_intercept(&mut self) -> Result<(), VpHaltReason<UhRunVpError>> {
        let message = hvdef::HvX64CpuidInterceptMessage::ref_from_prefix(
            self.runner.exit_message().payload(),
        )
        .unwrap();

        let default_result = [
            message.default_result_rax as u32,
            message.default_result_rbx as u32,
            message.default_result_rcx as u32,
            message.default_result_rdx as u32,
        ];

        tracing::trace!(msg = %format_args!("{:x?}", message), "cpuid");

        let [eax, ebx, ecx, edx] = self.partition.cpuid.lock().result(
            message.rax as u32,
            message.rcx as u32,
            &default_result,
        );

        let next_rip = next_rip(&message.header);
        self.runner.cpu_context_mut().gps[protocol::RAX] = eax.into();
        self.runner.cpu_context_mut().gps[protocol::RBX] = ebx.into();
        self.runner.cpu_context_mut().gps[protocol::RCX] = ecx.into();
        self.runner.cpu_context_mut().gps[protocol::RDX] = edx.into();

        self.set_rip(next_rip)
    }

    fn handle_msr_intercept(&mut self, dev: &impl CpuIo) -> Result<(), VpHaltReason<UhRunVpError>> {
        let message =
            hvdef::HvX64MsrInterceptMessage::ref_from_prefix(self.runner.exit_message().payload())
                .unwrap();
        let rip = next_rip(&message.header);
        let last_vtl = self.last_vtl();

        tracing::trace!(msg = %format_args!("{:x?}", message), "msr");

        let msr = message.msr_number;
        match message.header.intercept_access_type {
            HvInterceptAccessType::READ => {
                let r = if let Some(lapics) = &mut self.backing.lapics {
                    lapics[last_vtl].msr_read(
                        self.partition,
                        &mut self.runner,
                        &self.vmtime,
                        dev,
                        msr,
                    )
                } else {
                    Err(MsrError::Unknown)
                };
                let r = r.or_else_if_unknown(|| self.read_msr(msr));

                let value = match r {
                    Ok(v) => v,
                    Err(MsrError::Unknown) => {
                        tracing::trace!(msr, "unknown msr read");
                        0
                    }
                    Err(MsrError::InvalidAccess) => {
                        self.inject_gpf();
                        // Do not advance RIP.
                        return Ok(());
                    }
                };

                self.runner.cpu_context_mut().gps[protocol::RAX] = value & 0xffff_ffff;
                self.runner.cpu_context_mut().gps[protocol::RDX] = value >> 32;
            }
            HvInterceptAccessType::WRITE => {
                let value = (message.rax & 0xffff_ffff) | (message.rdx << 32);
                let r = if let Some(lapic) = &mut self.backing.lapics {
                    lapic[last_vtl].msr_write(
                        self.partition,
                        &mut self.runner,
                        &self.vmtime,
                        dev,
                        msr,
                        value,
                    )
                } else {
                    Err(MsrError::Unknown)
                };
                let r = r.or_else_if_unknown(|| self.write_msr(msr, value));
                match r {
                    Ok(()) => {}
                    Err(MsrError::Unknown) => {
                        tracing::trace!(msr, value, "unknown msr write");
                    }
                    Err(MsrError::InvalidAccess) => {
                        self.inject_gpf();
                        // Do not advance RIP.
                        return Ok(());
                    }
                }
            }
            _ => unreachable!(),
        }

        self.set_rip(rip)
    }

    fn inject_gpf(&mut self) {
        let exception_event = hvdef::HvX64PendingExceptionEvent::new()
            .with_event_pending(true)
            .with_event_type(hvdef::HV_X64_PENDING_EVENT_EXCEPTION)
            .with_vector(x86defs::Exception::GENERAL_PROTECTION_FAULT.0.into())
            .with_deliver_error_code(true)
            .with_error_code(0);

        self.runner
            .set_vp_register(
                HvX64RegisterName::PendingEvent0,
                u128::from(exception_event).into(),
            )
            .expect("set_vp_register should succeed for pending event");
    }

    fn handle_eoi(&self, dev: &impl CpuIo) -> Result<(), VpHaltReason<UhRunVpError>> {
        let message =
            hvdef::HvX64ApicEoiMessage::ref_from_prefix(self.runner.exit_message().payload())
                .unwrap();

        tracing::trace!(msg = %format_args!("{:x?}", message), "eoi");

        dev.handle_eoi(message.interrupt_vector);
        Ok(())
    }

    fn handle_unrecoverable_exception(&self) -> Result<(), VpHaltReason<UhRunVpError>> {
        Err(VpHaltReason::TripleFault {
            vtl: self.last_vtl().into(),
        })
    }

    fn handle_halt(&mut self) -> Result<(), VpHaltReason<UhRunVpError>> {
        let last_vtl = self.last_vtl();
        self.backing.lapics.as_mut().unwrap()[last_vtl].halt();
        Ok(())
    }

    fn handle_exception(&mut self) -> Result<(), VpHaltReason<UhRunVpError>> {
        let message = hvdef::HvX64ExceptionInterceptMessage::ref_from_prefix(
            self.runner.exit_message().payload(),
        )
        .unwrap();

        match x86defs::Exception(message.vector as u8) {
            #[cfg(all(feature = "gdb", guest_arch = "x86_64"))]
            x86defs::Exception::DEBUG => self.handle_debug_exception()?,
            _ => tracing::error!("unexpected exception type {:#x?}", message.vector),
        }
        Ok(())
    }

    fn emulator_state(&mut self) -> x86emu::CpuState {
        const NAMES: &[HvX64RegisterName] = &[
            HvX64RegisterName::Rsp,
            HvX64RegisterName::Es,
            HvX64RegisterName::Ds,
            HvX64RegisterName::Fs,
            HvX64RegisterName::Gs,
            HvX64RegisterName::Ss,
            HvX64RegisterName::Cr0,
            HvX64RegisterName::Efer,
        ];
        let mut values = [FromZeroes::new_zeroed(); NAMES.len()];
        self.runner
            .get_vp_registers(NAMES, &mut values)
            .expect("register query should not fail");

        let [rsp, es, ds, fs, gs, ss, cr0, efer] = values;

        let mut gps = self.runner.cpu_context().gps;
        gps[x86emu::CpuState::RSP] = rsp.as_u64();

        let message = self.runner.exit_message();
        let header = HvX64InterceptMessageHeader::ref_from_prefix(message.payload()).unwrap();

        x86emu::CpuState {
            gps,
            segs: [
                from_seg(es.into()),
                from_seg(header.cs_segment),
                from_seg(ss.into()),
                from_seg(ds.into()),
                from_seg(fs.into()),
                from_seg(gs.into()),
            ],
            rip: header.rip,
            rflags: header.rflags.into(),
            cr0: cr0.as_u64(),
            efer: efer.as_u64(),
        }
    }

    fn set_emulator_state(&mut self, state: &x86emu::CpuState) {
        self.runner
            .set_vp_registers([
                (HvX64RegisterName::Rip, state.rip),
                (HvX64RegisterName::Rflags, state.rflags.into()),
                (HvX64RegisterName::Rsp, state.gps[x86emu::CpuState::RSP]),
            ])
            .unwrap();

        self.runner.cpu_context_mut().gps = state.gps;
    }

    fn set_vsm_partition_config(
        &mut self,
        value: HvRegisterVsmPartitionConfig,
        vtl: GuestVtl,
    ) -> Result<(), HvError> {
        if vtl != GuestVtl::Vtl1 {
            return Err(HvError::InvalidParameter);
        }

        assert!(self.partition.isolation.is_some());

        let status: HvRegisterVsmPartitionStatus = self.partition.vsm_status();

        let vtl1_enabled = VtlSet::from(status.enabled_vtl_set()).is_set(GuestVtl::Vtl1);
        if !vtl1_enabled {
            return Err(HvError::InvalidVtlState);
        }

        let mut guest_vsm_lock = self.partition.guest_vsm.write();

        // Initialize partition.guest_vsm state if necessary.
        match *guest_vsm_lock {
            GuestVsmState::NotPlatformSupported => {
                return Err(HvError::AccessDenied);
            }
            GuestVsmState::NotGuestEnabled => {
                // TODO: check status
                *guest_vsm_lock = GuestVsmState::Enabled {
                    vtl1: GuestVsmVtl1State {
                        enable_vtl_protection: false,
                        inner: GuestVsmVtl1StateInner::SoftwareCvm {
                            state: Default::default(),
                        },
                    },
                };
            }
            GuestVsmState::Enabled { vtl1: _ } => {}
        }

        let guest_vsm = guest_vsm_lock.get_vtl1_mut().unwrap();
        let guest_vsm_inner = guest_vsm.inner.get_software_cvm_mut().unwrap();
        let protections = HvMapGpaFlags::from(value.default_vtl_protection_mask() as u32);

        if value.reserved() != 0 {
            return Err(HvError::InvalidRegisterValue);
        }

        // VTL protection cannot be disabled once enabled.
        //
        // The hypervisor should intercept only the case where the lower VTL is
        // setting the enable_vtl_protection bit when it was previously
        // disabled; other cases are handled directly by the hypervisor.
        if !value.enable_vtl_protection() {
            if guest_vsm.enable_vtl_protection {
                // A malicious guest could change its hypercall parameters in
                // memory while the intercept is being handled; this case
                // explicitly handles that situation.
                return Err(HvError::InvalidRegisterValue);
            } else {
                panic!("unexpected SetVpRegisters intercept");
            }
        }

        // For VBS-isolated VMs, protections apply to VTLs lower than the one specified when
        // setting VsmPartitionConfig.
        let mbec_enabled = VtlSet::from(status.mbec_enabled_vtl_set()).is_set(GuestVtl::Vtl0);
        let shadow_supervisor_stack_enabled =
            VtlSet::from(status.supervisor_shadow_stack_enabled_vtl_set() as u16)
                .is_set(GuestVtl::Vtl0);

        if !validate_vtl_gpa_flags(protections, mbec_enabled, shadow_supervisor_stack_enabled) {
            return Err(HvError::InvalidRegisterValue);
        }

        // Default VTL protection mask must include read and write.
        if !(protections.readable() && protections.writable()) {
            return Err(HvError::InvalidRegisterValue);
        }

        // Don't allow changing existing protections once set.
        if let Some(current_protections) = guest_vsm_inner.default_vtl_protections {
            if protections != current_protections {
                return Err(HvError::InvalidRegisterValue);
            }
        }
        guest_vsm_inner.default_vtl_protections = Some(protections);

        for ram_range in self.partition.lower_vtl_memory_layout.ram().iter() {
            self.partition
                .hcl
                .modify_vtl_protection_mask(ram_range.range, protections, vtl.into())
                .map_err(|e| match e {
                    ApplyVtlProtectionsError::Hypervisor {
                        range: _,
                        output: _,
                        hv_error,
                        vtl: _,
                    } => hv_error,
                    _ => unreachable!(),
                })?;
        }

        let hc_regs = [(HvX64RegisterName::VsmPartitionConfig, u64::from(value))];
        self.runner.set_vp_registers_hvcall(vtl.into(), hc_regs)?;
        guest_vsm.enable_vtl_protection = true;

        Ok(())
    }
}

impl<T: CpuIo> EmulatorSupport for UhEmulationState<'_, '_, T, HypervisorBackedX86> {
    type Error = UhRunVpError;

    fn vp_index(&self) -> VpIndex {
        self.vp.vp_index()
    }

    fn vendor(&self) -> x86defs::cpuid::Vendor {
        self.vp.partition.caps.vendor
    }

    fn state(&mut self) -> Result<x86emu::CpuState, Self::Error> {
        Ok(self.vp.emulator_state())
    }

    fn set_state(&mut self, state: x86emu::CpuState) -> Result<(), Self::Error> {
        self.vp.set_emulator_state(&state);
        Ok(())
    }

    fn instruction_bytes(&self) -> &[u8] {
        let message = self.vp.runner.exit_message();
        match message.header.typ {
            HvMessageType::HvMessageTypeGpaIntercept
            | HvMessageType::HvMessageTypeUnmappedGpa
            | HvMessageType::HvMessageTypeUnacceptedGpa => {
                let message =
                    hvdef::HvX64MemoryInterceptMessage::ref_from_prefix(message.payload()).unwrap();
                &message.instruction_bytes[..message.instruction_byte_count as usize]
            }
            HvMessageType::HvMessageTypeX64IoPortIntercept => {
                let message =
                    hvdef::HvX64IoPortInterceptMessage::ref_from_prefix(message.payload()).unwrap();
                &message.instruction_bytes[..message.instruction_byte_count as usize]
            }
            _ => unreachable!(),
        }
    }

    fn physical_address(&self) -> Option<u64> {
        let message = self.vp.runner.exit_message();
        match message.header.typ {
            HvMessageType::HvMessageTypeGpaIntercept
            | HvMessageType::HvMessageTypeUnmappedGpa
            | HvMessageType::HvMessageTypeUnacceptedGpa => {
                let message =
                    hvdef::HvX64MemoryInterceptMessage::ref_from_prefix(message.payload()).unwrap();
                Some(message.guest_physical_address)
            }
            _ => None,
        }
    }

    fn initial_gva_translation(&self) -> Option<virt_support_x86emu::emulate::InitialTranslation> {
        if (self.vp.runner.exit_message().header.typ != HvMessageType::HvMessageTypeGpaIntercept)
            && (self.vp.runner.exit_message().header.typ != HvMessageType::HvMessageTypeUnmappedGpa)
            && (self.vp.runner.exit_message().header.typ
                != HvMessageType::HvMessageTypeUnacceptedGpa)
        {
            return None;
        }

        let message = hvdef::HvX64MemoryInterceptMessage::ref_from_prefix(
            self.vp.runner.exit_message().payload(),
        )
        .unwrap();

        if !message.memory_access_info.gva_gpa_valid() {
            tracing::trace!(?message.guest_virtual_address, ?message.guest_physical_address, "gva gpa not valid {:?}", self.vp.runner.exit_message().payload());
            return None;
        }

        let translate_mode = virt_support_x86emu::emulate::TranslateMode::try_from(
            message.header.intercept_access_type,
        )
        .expect("unexpected intercept access type");

        tracing::trace!(?message.guest_virtual_address, ?message.guest_physical_address, ?translate_mode, "initial translation");

        Some(virt_support_x86emu::emulate::InitialTranslation {
            gva: message.guest_virtual_address,
            gpa: message.guest_physical_address,
            translate_mode,
        })
    }

    fn interruption_pending(&self) -> bool {
        self.interruption_pending
    }

    fn check_vtl_access(
        &mut self,
        gpa: u64,
        mode: virt_support_x86emu::emulate::TranslateMode,
    ) -> Result<(), EmuCheckVtlAccessError<Self::Error>> {
        // Underhill currently doesn't set VTL 2 protections against execute exclusively, it removes
        // all permissions from a page. So for VTL 1, no need to check the permissions; if VTL 1
        // doesn't have permissions to a page, Underhill should appropriately fail when it tries
        // to read or write to that page on VTL 1's behalf.
        //
        // For VTL 0, the alias map guards for read and write permissions, so only check VTL execute
        // permissions. Because VTL 2 will not restrict execute exclusively, only VTL 1 execute
        // permissions need to be checked and therefore only check permissions if VTL 1 is allowed.
        //
        // Note: the restriction to VTL 1 support also means that for WHP, which doesn't support VTL 1
        // the HvCheckSparseGpaPageVtlAccess hypercall--which is unimplemented in whp--will never be made.
        if mode == virt_support_x86emu::emulate::TranslateMode::Execute
            && self.vp.last_vtl() == GuestVtl::Vtl0
            && self.vp.vtl1_supported()
        {
            // Should always be called after translate gva with the tlb lock flag
            debug_assert!(self.vp.is_tlb_locked(Vtl::Vtl2, self.vp.last_vtl()));

            let mbec_user_execute = self
                .vp
                .runner
                .get_vp_register(HvX64RegisterName::InstructionEmulationHints)
                .map_err(UhRunVpError::EmulationState)?;

            let flags =
                if hvdef::HvInstructionEmulatorHintsRegister::from(mbec_user_execute.as_u64())
                    .mbec_user_execute_control()
                {
                    HvMapGpaFlags::new().with_user_executable(true)
                } else {
                    HvMapGpaFlags::new().with_kernel_executable(true)
                };

            let access_result = self
                .vp
                .partition
                .hcl
                .check_vtl_access(gpa, GuestVtl::Vtl0, flags)
                .map_err(|e| EmuCheckVtlAccessError::Hypervisor(UhRunVpError::VtlAccess(e)))?;

            if let Some(ioctl::CheckVtlAccessResult { vtl, denied_flags }) = access_result {
                return Err(EmuCheckVtlAccessError::AccessDenied { vtl, denied_flags });
            };
        }

        Ok(())
    }

    fn translate_gva(
        &mut self,
        gva: u64,
        mode: virt_support_x86emu::emulate::TranslateMode,
    ) -> Result<Result<EmuTranslateResult, EmuTranslateError>, Self::Error> {
        let mut control_flags = hypercall::TranslateGvaControlFlagsX64::new();
        match mode {
            virt_support_x86emu::emulate::TranslateMode::Read => {
                control_flags.set_validate_read(true)
            }
            virt_support_x86emu::emulate::TranslateMode::Write => {
                control_flags.set_validate_read(true);
                control_flags.set_validate_write(true);
            }
            virt_support_x86emu::emulate::TranslateMode::Execute => {
                control_flags.set_validate_execute(true)
            }
        };

        let target_vtl = self.vp.last_vtl();

        // The translation will be used, so set the appropriate page table bits
        // (the access/dirty bit).
        //
        // Prevent flushes in order to make sure that translation of this GVA
        // remains usable until the VP is resumed back to direct execution.
        control_flags.set_set_page_table_bits(true);
        control_flags.set_tlb_flush_inhibit(true);
        self.vp.set_tlb_lock(Vtl::Vtl2, target_vtl);

        // In case we're not running ring 0, check privileges against VP state
        // as of when the original intercept came in - since the emulator
        // doesn't support instructions that change ring level, the ring level
        // will remain the same as it was in the VP state as of when the
        // original intercept came in. The privilege exempt flag should
        // not be set.
        assert!(!control_flags.privilege_exempt());

        // Do the translation using the current VTL.
        control_flags.set_input_vtl(target_vtl.into());

        match self
            .vp
            .runner
            .translate_gva_to_gpa(gva, control_flags)
            .map_err(|e| UhRunVpError::TranslateGva(ioctl::Error::TranslateGvaToGpa(e)))?
        {
            Ok(ioctl::TranslateResult {
                gpa_page,
                overlay_page,
            }) => Ok(Ok(EmuTranslateResult {
                gpa: (gpa_page << hvdef::HV_PAGE_SHIFT) + (gva & (HV_PAGE_SIZE - 1)),
                overlay_page: Some(overlay_page),
            })),
            Err(ioctl::x64::TranslateErrorX64 { code, event_info }) => Ok(Err(EmuTranslateError {
                code: hypercall::TranslateGvaResultCode(code),
                event_info: Some(event_info),
            })),
        }
    }

    fn inject_pending_event(&mut self, event_info: HvX64PendingEvent) {
        let regs = [
            (
                HvX64RegisterName::PendingEvent0,
                u128::from(event_info.reg_0),
            ),
            (
                HvX64RegisterName::PendingEvent1,
                u128::from(event_info.reg_1),
            ),
        ];

        let last_vtl = self.vp.last_vtl();

        self.vp
            .runner
            .set_vp_registers_hvcall(last_vtl.into(), regs)
            .expect("set_vp_registers hypercall for setting pending event should not fail");
    }

    fn get_xmm(&mut self, reg: usize) -> Result<u128, Self::Error> {
        Ok(u128::from_le_bytes(
            self.vp.runner.cpu_context().fx_state.xmm[reg],
        ))
    }

    fn set_xmm(&mut self, reg: usize, value: u128) -> Result<(), Self::Error> {
        self.vp.runner.cpu_context_mut().fx_state.xmm[reg] = value.to_le_bytes();
        Ok(())
    }

    fn check_monitor_write(&self, gpa: u64, bytes: &[u8]) -> bool {
        self.vp
            .partition
            .monitor_page
            .check_write(gpa, bytes, |connection_id| {
                signal_mnf(self.devices, connection_id)
            })
    }

    fn is_gpa_mapped(&self, gpa: u64, write: bool) -> bool {
        self.vp.partition.is_gpa_mapped(gpa, write)
    }

    fn lapic_base_address(&self) -> Option<u64> {
        let last_vtl = self.vp.last_vtl();
        self.vp
            .backing
            .lapics
            .as_ref()
            .and_then(|lapic| lapic[last_vtl].base_address())
    }

    fn lapic_read(&mut self, address: u64, data: &mut [u8]) {
        let last_vtl = self.vp.last_vtl();
        self.vp.backing.lapics.as_mut().unwrap()[last_vtl].mmio_read(
            self.vp.partition,
            &mut self.vp.runner,
            &self.vp.vmtime,
            self.devices,
            address,
            data,
        );
    }

    fn lapic_write(&mut self, address: u64, data: &[u8]) {
        let last_vtl = self.vp.last_vtl();
        self.vp.backing.lapics.as_mut().unwrap()[last_vtl].mmio_write(
            self.vp.partition,
            &mut self.vp.runner,
            &self.vp.vmtime,
            self.devices,
            address,
            data,
        );
    }
}

impl<T: CpuIo> UhHypercallHandler<'_, '_, T, HypervisorBackedX86> {
    const MSHV_DISPATCHER: hv1_hypercall::Dispatcher<Self> = hv1_hypercall::dispatcher!(
        Self,
        [
            hv1_hypercall::HvPostMessage,
            hv1_hypercall::HvSignalEvent,
            hv1_hypercall::HvRetargetDeviceInterrupt,
            hv1_hypercall::HvX64StartVirtualProcessor,
            hv1_hypercall::HvGetVpIndexFromApicId,
            hv1_hypercall::HvSetVpRegisters,
        ]
    );
}

impl<T> hv1_hypercall::X64RegisterState for UhHypercallHandler<'_, '_, T, HypervisorBackedX86> {
    fn rip(&mut self) -> u64 {
        HvX64InterceptMessageHeader::ref_from_prefix(self.vp.runner.exit_message().payload())
            .unwrap()
            .rip
    }

    fn set_rip(&mut self, rip: u64) {
        self.vp.set_rip(rip).unwrap()
    }

    fn gp(&mut self, n: hv1_hypercall::X64HypercallRegister) -> u64 {
        match n {
            hv1_hypercall::X64HypercallRegister::Rax => {
                self.vp.runner.cpu_context().gps[protocol::RAX]
            }
            hv1_hypercall::X64HypercallRegister::Rcx => {
                self.vp.runner.cpu_context().gps[protocol::RCX]
            }
            hv1_hypercall::X64HypercallRegister::Rdx => {
                self.vp.runner.cpu_context().gps[protocol::RDX]
            }
            hv1_hypercall::X64HypercallRegister::Rbx => {
                self.vp.runner.cpu_context().gps[protocol::RBX]
            }
            hv1_hypercall::X64HypercallRegister::Rsi => {
                self.vp.runner.cpu_context().gps[protocol::RSI]
            }
            hv1_hypercall::X64HypercallRegister::Rdi => {
                self.vp.runner.cpu_context().gps[protocol::RDI]
            }
            hv1_hypercall::X64HypercallRegister::R8 => {
                self.vp.runner.cpu_context().gps[protocol::R8]
            }
        }
    }

    fn set_gp(&mut self, n: hv1_hypercall::X64HypercallRegister, value: u64) {
        *match n {
            hv1_hypercall::X64HypercallRegister::Rax => {
                &mut self.vp.runner.cpu_context_mut().gps[protocol::RAX]
            }
            hv1_hypercall::X64HypercallRegister::Rcx => {
                &mut self.vp.runner.cpu_context_mut().gps[protocol::RCX]
            }
            hv1_hypercall::X64HypercallRegister::Rdx => {
                &mut self.vp.runner.cpu_context_mut().gps[protocol::RDX]
            }
            hv1_hypercall::X64HypercallRegister::Rbx => {
                &mut self.vp.runner.cpu_context_mut().gps[protocol::RBX]
            }
            hv1_hypercall::X64HypercallRegister::Rsi => {
                &mut self.vp.runner.cpu_context_mut().gps[protocol::RSI]
            }
            hv1_hypercall::X64HypercallRegister::Rdi => {
                &mut self.vp.runner.cpu_context_mut().gps[protocol::RDI]
            }
            hv1_hypercall::X64HypercallRegister::R8 => {
                &mut self.vp.runner.cpu_context_mut().gps[protocol::R8]
            }
        } = value;
    }

    fn xmm(&mut self, n: usize) -> u128 {
        u128::from_ne_bytes(self.vp.runner.cpu_context().fx_state.xmm[n])
    }

    fn set_xmm(&mut self, n: usize, value: u128) {
        self.vp.runner.cpu_context_mut().fx_state.xmm[n] = value.to_ne_bytes();
    }
}

trait ToVpRegisterName: 'static + Copy + std::fmt::Debug {
    fn to_vp_reg_name(self) -> VpRegisterName;
}

impl ToVpRegisterName for VpRegisterName {
    fn to_vp_reg_name(self) -> VpRegisterName {
        self
    }
}

impl UhVpStateAccess<'_, '_, HypervisorBackedX86> {
    fn set_register_state<T, R: ToVpRegisterName, const N: usize>(
        &mut self,
        regs: &T,
    ) -> Result<(), vp_state::Error>
    where
        T: HvRegisterState<R, N>,
    {
        let names = regs.names().map(|r| r.to_vp_reg_name());
        let mut values = [HvRegisterValue::new_zeroed(); N];
        regs.get_values(values.iter_mut());
        self.vp
            .runner
            .set_vp_registers(names.iter().copied().zip(values))
            .map_err(vp_state::Error::SetRegisters)?;
        Ok(())
    }

    fn get_register_state<T, R: ToVpRegisterName, const N: usize>(
        &mut self,
    ) -> Result<T, vp_state::Error>
    where
        T: HvRegisterState<R, N>,
    {
        let mut regs = T::default();
        let names = regs.names().map(|r| r.to_vp_reg_name());
        let mut values = [HvRegisterValue::new_zeroed(); N];
        self.vp
            .runner
            .get_vp_registers(&names, &mut values)
            .map_err(vp_state::Error::GetRegisters)?;

        regs.set_values(values.into_iter());
        Ok(regs)
    }
}

impl AccessVpState for UhVpStateAccess<'_, '_, HypervisorBackedX86> {
    type Error = vp_state::Error;

    fn caps(&self) -> &virt::x86::X86PartitionCapabilities {
        &self.vp.partition.caps
    }

    fn commit(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn registers(&mut self) -> Result<vp::Registers, Self::Error> {
        self.get_register_state()
    }

    fn set_registers(&mut self, value: &vp::Registers) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn activity(&mut self) -> Result<vp::Activity, Self::Error> {
        let activity: vp::Activity = self.get_register_state()?;

        // TODO: Get the NMI pending bit from the APIC.
        // let apic = self.vp.whp(self.vtl).get_apic()?;
        // activity.nmi_pending = hv_apic_nmi_pending(&apic);
        Ok(activity)
    }

    fn set_activity(&mut self, value: &vp::Activity) -> Result<(), Self::Error> {
        self.set_register_state(value)?;

        // TODO: Set the NMI pending bit via the APIC.
        // let mut apic = self.vp.whp(self.vtl).get_apic()?;
        // set_hv_apic_nmi_pending(&mut apic, value.nmi_pending);
        // self.vp.whp(self.vtl).set_apic(&apic)?;
        Ok(())
    }

    fn xsave(&mut self) -> Result<vp::Xsave, Self::Error> {
        // TODO: get the rest of the xsave state, not just the legacy FP state.
        //
        // This is just used for debugging, so this should not be a problem.
        #[repr(C)]
        #[derive(AsBytes)]
        struct XsaveStandard {
            fxsave: Fxsave,
            xsave_header: XsaveHeader,
        }
        let state = XsaveStandard {
            fxsave: self.vp.runner.cpu_context().fx_state.clone(),
            xsave_header: XsaveHeader {
                xstate_bv: XFEATURE_X87 | XFEATURE_SSE,
                ..FromZeroes::new_zeroed()
            },
        };
        Ok(vp::Xsave::from_standard(state.as_bytes(), self.caps()))
    }

    fn set_xsave(&mut self, _value: &vp::Xsave) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("xsave"))
    }

    fn apic(&mut self) -> Result<vp::Apic, Self::Error> {
        Err(vp_state::Error::Unimplemented("apic"))
    }

    fn set_apic(&mut self, _value: &vp::Apic) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("apic"))
    }

    fn xcr(&mut self) -> Result<vp::Xcr0, Self::Error> {
        self.get_register_state()
    }

    fn set_xcr(&mut self, value: &vp::Xcr0) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn xss(&mut self) -> Result<vp::Xss, Self::Error> {
        self.get_register_state()
    }

    fn set_xss(&mut self, value: &vp::Xss) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn cache_control(&mut self) -> Result<vp::CacheControl, Self::Error> {
        self.get_register_state()
    }

    fn set_cache_control(&mut self, cc: &vp::CacheControl) -> Result<(), Self::Error> {
        self.set_register_state(cc)
    }

    fn virtual_msrs(&mut self) -> Result<vp::VirtualMsrs, Self::Error> {
        self.get_register_state()
    }

    fn set_virtual_msrs(&mut self, msrs: &vp::VirtualMsrs) -> Result<(), Self::Error> {
        self.set_register_state(msrs)
    }

    fn debug_regs(&mut self) -> Result<vp::DebugRegisters, Self::Error> {
        self.get_register_state()
    }

    fn set_debug_regs(&mut self, value: &vp::DebugRegisters) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn tsc(&mut self) -> Result<vp::Tsc, Self::Error> {
        self.get_register_state()
    }

    fn set_tsc(&mut self, value: &vp::Tsc) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn cet(&mut self) -> Result<vp::Cet, Self::Error> {
        self.get_register_state()
    }

    fn set_cet(&mut self, value: &vp::Cet) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn cet_ss(&mut self) -> Result<vp::CetSs, Self::Error> {
        self.get_register_state()
    }

    fn set_cet_ss(&mut self, value: &vp::CetSs) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn tsc_aux(&mut self) -> Result<vp::TscAux, Self::Error> {
        self.get_register_state()
    }

    fn set_tsc_aux(&mut self, value: &vp::TscAux) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn synic_msrs(&mut self) -> Result<vp::SyntheticMsrs, Self::Error> {
        self.get_register_state()
    }

    fn set_synic_msrs(&mut self, value: &vp::SyntheticMsrs) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn synic_timers(&mut self) -> Result<vp::SynicTimers, Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_timers"))
    }

    fn set_synic_timers(&mut self, _value: &vp::SynicTimers) -> Result<(), Self::Error> {
        Err(vp_state::Error::Unimplemented("synic_timers"))
    }

    fn synic_message_queues(&mut self) -> Result<vp::SynicMessageQueues, Self::Error> {
        Ok(self.vp.inner.message_queues[self.vtl].save())
    }

    fn set_synic_message_queues(
        &mut self,
        value: &vp::SynicMessageQueues,
    ) -> Result<(), Self::Error> {
        self.vp.inner.message_queues[self.vtl].restore(value);
        Ok(())
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
}

impl<T: CpuIo> hv1_hypercall::RetargetDeviceInterrupt
    for UhHypercallHandler<'_, '_, T, HypervisorBackedX86>
{
    fn retarget_interrupt(
        &mut self,
        device_id: u64,
        address: u64,
        data: u32,
        params: &hv1_hypercall::HvInterruptParameters<'_>,
    ) -> hvdef::HvResult<()> {
        self.retarget_virtual_interrupt(
            device_id,
            address,
            data,
            params.vector,
            params.multicast,
            params.target_processors,
        )
    }
}

impl<T> hv1_hypercall::SetVpRegisters for UhHypercallHandler<'_, '_, T, HypervisorBackedX86> {
    fn set_vp_registers(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        vtl: Option<Vtl>,
        registers: &[hypercall::HvRegisterAssoc],
    ) -> hvdef::HvRepResult {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err((HvError::AccessDenied, 0));
        }

        if vp_index != hvdef::HV_VP_INDEX_SELF && vp_index != self.vp.vp_index().index() {
            return Err((HvError::InvalidVpIndex, 0));
        }

        let target_vtl = self
            .target_vtl_no_higher(vtl.unwrap_or(self.vp.last_vtl().into()))
            .map_err(|e| (e, 0))?;

        for (i, reg) in registers.iter().enumerate() {
            if reg.name == HvX64RegisterName::VsmPartitionConfig.into() {
                let value = HvRegisterVsmPartitionConfig::from(reg.value.as_u64());
                self.vp
                    .set_vsm_partition_config(value, target_vtl)
                    .map_err(|e| (e, i))?;
            } else {
                return Err((HvError::InvalidParameter, i));
            }
        }

        Ok(())
    }
}

mod save_restore {
    use super::HypervisorBackedX86;
    use super::UhProcessor;
    use anyhow::Context;
    use hcl::GuestVtl;
    use hvdef::HvInternalActivityRegister;
    use hvdef::HvX64RegisterName;
    use virt::irqcon::MsiRequest;
    use virt::Processor;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;
    use zerocopy::AsBytes;
    use zerocopy::FromZeroes;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "underhill.partition")]
        pub struct ProcessorSavedState {
            #[mesh(1)]
            pub(super) rax: u64,
            #[mesh(2)]
            pub(super) rcx: u64,
            #[mesh(3)]
            pub(super) rdx: u64,
            #[mesh(4)]
            pub(super) rbx: u64,
            #[mesh(5)]
            pub(super) cr2: u64,
            #[mesh(6)]
            pub(super) rbp: u64,
            #[mesh(7)]
            pub(super) rsi: u64,
            #[mesh(8)]
            pub(super) rdi: u64,
            #[mesh(9)]
            pub(super) r8: u64,
            #[mesh(10)]
            pub(super) r9: u64,
            #[mesh(11)]
            pub(super) r10: u64,
            #[mesh(12)]
            pub(super) r11: u64,
            #[mesh(13)]
            pub(super) r12: u64,
            #[mesh(14)]
            pub(super) r13: u64,
            #[mesh(15)]
            pub(super) r14: u64,
            #[mesh(16)]
            pub(super) r15: u64,
            #[mesh(17)]
            pub(super) fx_state: Vec<u8>,
            #[mesh(18)]
            pub(super) dr0: u64,
            #[mesh(19)]
            pub(super) dr1: u64,
            #[mesh(20)]
            pub(super) dr2: u64,
            #[mesh(21)]
            pub(super) dr3: u64,
            #[mesh(22)]
            pub(super) dr6: Option<u64>, // only set when the DR6_SHARED capability is present
            /// If VTL0 should be in the startup suspend state. Older underhill
            /// versions do not save this property, so maintain the old buggy
            /// behavior for those cases its not present in the saved state.
            #[mesh(23)]
            pub(super) startup_suspend: Option<bool>,
        }
    }

    const SHARED_REGISTERS: &[HvX64RegisterName] = &[
        HvX64RegisterName::Dr0,
        HvX64RegisterName::Dr1,
        HvX64RegisterName::Dr2,
        HvX64RegisterName::Dr3,
        HvX64RegisterName::Dr6, // must be last
    ];

    impl SaveRestore for UhProcessor<'_, HypervisorBackedX86> {
        type SavedState = state::ProcessorSavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            // Ensure all async requests are reflected in the saved state.
            self.flush_async_requests()
                .context("failed to flush async requests")
                .map_err(SaveError::Other)?;

            let dr6_shared = self.partition.hcl.dr6_shared();
            let mut values = [FromZeroes::new_zeroed(); SHARED_REGISTERS.len()];
            let len = if dr6_shared {
                SHARED_REGISTERS.len()
            } else {
                SHARED_REGISTERS.len() - 1
            };

            self.runner
                .get_vp_registers(&SHARED_REGISTERS[..len], &mut values[..len])
                .context("failed to get shared registers")
                .map_err(SaveError::Other)?;

            let startup_suspend = match self
                .runner
                .get_vp_register(HvX64RegisterName::InternalActivityState)
            {
                Ok(val) => Some(HvInternalActivityRegister::from(val.as_u64()).startup_suspend()),
                Err(e) => {
                    // The ioctl get_vp_register path does not tell us
                    // hv_status directly, so just log if it failed for any
                    // reason.
                    tracing::warn!(
                            error = &e as &dyn std::error::Error,
                            "unable to query startup suspend, unable to save VTL0 startup suspend state"
                        );

                    None
                }
            };

            let [rax, rcx, rdx, rbx, cr2, rbp, rsi, rdi, r8, r9, r10, r11, r12, r13, r14, r15] =
                self.runner.cpu_context().gps;

            let state = state::ProcessorSavedState {
                rax,
                rcx,
                rdx,
                rbx,
                cr2,
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
                fx_state: self.runner.cpu_context().fx_state.as_bytes().to_vec(),
                dr0: values[0].as_u64(),
                dr1: values[1].as_u64(),
                dr2: values[2].as_u64(),
                dr3: values[3].as_u64(),
                dr6: dr6_shared.then(|| values[4].as_u64()),
                startup_suspend,
            };

            Ok(state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::ProcessorSavedState {
                rax,
                rcx,
                rdx,
                rbx,
                cr2,
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
                fx_state,
                dr0,
                dr1,
                dr2,
                dr3,
                dr6,
                startup_suspend,
            } = state;

            let dr6_shared = self.partition.hcl.dr6_shared();
            self.runner.cpu_context_mut().gps = [
                rax, rcx, rdx, rbx, cr2, rbp, rsi, rdi, r8, r9, r10, r11, r12, r13, r14, r15,
            ];
            if fx_state.len() != self.runner.cpu_context_mut().fx_state.as_bytes().len() {
                return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                    "invalid fpu state"
                )));
            }
            if dr6_shared != state.dr6.is_some() {
                return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                    "dr6 state mismatch"
                )));
            }

            let len = if dr6_shared {
                SHARED_REGISTERS.len()
            } else {
                SHARED_REGISTERS.len() - 1
            };

            let values = [dr0, dr1, dr2, dr3, dr6.unwrap_or(0)];
            self.runner
                .set_vp_registers(SHARED_REGISTERS[..len].iter().copied().zip(values))
                .context("failed to set shared registers")
                .map_err(RestoreError::Other)?;

            self.runner
                .cpu_context_mut()
                .fx_state
                .as_bytes_mut()
                .copy_from_slice(&fx_state);

            let inject_startup_suspend = match startup_suspend {
                Some(true) => {
                    // When Underhill brings up APs during a servicing update
                    // via hypercall, this clears the lower VTL startup suspend
                    // state and makes the VP runnable. Like the cold boot path,
                    // we need to put the AP back into the startup suspend state
                    // in order to not start running the VP incorrectly.
                    true
                }
                None if !self.vp_index().is_bsp() => {
                    // Previous versions of Underhill did not save this value,
                    // which means the VM could be in a bad state if it's being
                    // serviced before VTL0 brings up APs. Log this state to
                    // note that.
                    const NAMES: [HvX64RegisterName; 4] = [
                        HvX64RegisterName::Rip,
                        HvX64RegisterName::Rflags,
                        HvX64RegisterName::Cr0,
                        HvX64RegisterName::Efer,
                    ];
                    let mut values = [FromZeroes::new_zeroed(); NAMES.len()];
                    self.runner
                        .get_vp_registers(&NAMES, &mut values)
                        .context("failed to get VP registers for startup suspend log")
                        .map_err(RestoreError::Other)?;
                    let [rip, rflags, cr0, efer] = values.map(|reg| reg.as_u64());

                    tracing::error!(
                        vp_index = self.vp_index().index(),
                        rip,
                        rflags,
                        cr0,
                        efer,
                        "previous version of underhill did not save startup_suspend state"
                    );

                    false
                }
                Some(false) | None => false,
            };

            if inject_startup_suspend {
                let reg = u64::from(HvInternalActivityRegister::new().with_startup_suspend(true));
                let result = self
                    .runner
                    .set_vp_registers([(HvX64RegisterName::InternalActivityState, reg)]);

                if let Err(e) = result {
                    // The ioctl set_vp_register path does not tell us hv_status
                    // directly, so just log if it failed for any reason.
                    tracing::warn!(
                        error = &e as &dyn std::error::Error,
                        "unable to set internal activity register, falling back to init"
                    );

                    self.partition.request_msi(
                        GuestVtl::Vtl0,
                        MsiRequest::new_x86(
                            virt::irqcon::DeliveryMode::INIT,
                            self.inner.vp_info.apic_id,
                            false,
                            0,
                            true,
                        ),
                    );
                }
            }

            Ok(())
        }
    }
}
