// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! X64 Processor support for Microsoft hypervisor-backed partitions.

#![cfg(guest_arch = "x86_64")]

type VpRegisterName = HvX64RegisterName;

use super::super::BackingParams;
use super::super::BackingPrivate;
use super::super::UhEmulationState;
use super::super::signal_mnf;
use super::super::vp_state;
use super::super::vp_state::UhVpStateAccess;
use super::MshvRunVpError;
use super::VbsIsolatedVtl1State;
use crate::BackingShared;
use crate::Error;
use crate::GuestVsmState;
use crate::GuestVtl;
use crate::UhPartitionNewParams;
use crate::processor::BackingSharedParams;
use crate::processor::SidecarExitReason;
use crate::processor::SidecarRemoveExit;
use crate::processor::UhHypercallHandler;
use crate::processor::UhProcessor;
use crate::validate_vtl_gpa_flags;
use hcl::ioctl;
use hcl::ioctl::ApplyVtlProtectionsError;
use hcl::ioctl::x64::MshvX64;
use hcl::protocol;
use hv1_emulator::hv::ProcessorVtlHv;
use hv1_hypercall::HvRepResult;
use hv1_structs::VtlSet;
use hvdef::HV_PAGE_SIZE;
use hvdef::HvDeliverabilityNotificationsRegister;
use hvdef::HvError;
use hvdef::HvInterceptAccessType;
use hvdef::HvMapGpaFlags;
use hvdef::HvMessageType;
use hvdef::HvRegisterValue;
use hvdef::HvRegisterVsmPartitionConfig;
use hvdef::HvX64InterceptMessageHeader;
use hvdef::HvX64PendingEvent;
use hvdef::HvX64PendingInterruptionType;
use hvdef::HvX64RegisterName;
use hvdef::Vtl;
use hvdef::hypercall;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use parking_lot::RwLock;
use std::sync::atomic::Ordering::Relaxed;
use virt::EmulatorMonitorSupport;
use virt::StopVp;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::io::CpuIo;
use virt::state::HvRegisterState;
use virt::state::StateElement;
use virt::vp;
use virt::vp::AccessVpState;
use virt::x86::MsrError;
use virt_support_x86emu::emulate::EmuCheckVtlAccessError;
use virt_support_x86emu::emulate::EmuTranslateError;
use virt_support_x86emu::emulate::EmuTranslateResult;
use virt_support_x86emu::emulate::EmulatorSupport;
use x86defs::RFlags;
use x86defs::SegmentRegister;
use x86defs::xsave::Fxsave;
use x86defs::xsave::XFEATURE_SSE;
use x86defs::xsave::XFEATURE_X87;
use x86defs::xsave::XsaveHeader;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// A backing for hypervisor-backed partitions (non-isolated and
/// software-isolated).
#[derive(InspectMut)]
pub struct HypervisorBackedX86 {
    // VTL0 only, used for synic message and extint readiness notifications.
    // We do not currently support synic message ports or extint interrupts for VTL1.
    #[inspect(hex, with = "|&x| u64::from(x)")]
    deliverability_notifications: HvDeliverabilityNotificationsRegister,
    /// Next set of deliverability notifications. See register definition for details.
    #[inspect(hex, with = "|&x| u64::from(x)")]
    pub(super) next_deliverability_notifications: HvDeliverabilityNotificationsRegister,
    stats: ProcessorStatsX86,
    /// Send an INIT to VTL0 before running the VP, to simulate setting startup
    /// suspend. Newer hypervisors allow setting startup suspend explicitly.
    deferred_init: bool,
}

/// Partition-wide shared data for hypervisor backed VMs.
#[derive(Inspect)]
pub struct HypervisorBackedX86Shared {
    pub(crate) guest_vsm: RwLock<GuestVsmState<VbsIsolatedVtl1State>>,
}

impl HypervisorBackedX86Shared {
    /// Creates a new partition-shared data structure for hypervisor backed VMs.
    pub(crate) fn new(
        _partition_params: &UhPartitionNewParams<'_>,
        params: BackingSharedParams<'_>,
    ) -> Result<Self, Error> {
        Ok(Self {
            guest_vsm: RwLock::new(GuestVsmState::from_availability(params.guest_vsm_available)),
        })
    }
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

pub struct MshvEmulationCache {
    rsp: u64,
    es: SegmentRegister,
    ds: SegmentRegister,
    fs: SegmentRegister,
    gs: SegmentRegister,
    ss: SegmentRegister,
    cr0: u64,
    efer: u64,
    rip: u64,
    rflags: RFlags,
}

#[expect(private_interfaces)]
impl BackingPrivate for HypervisorBackedX86 {
    type HclBacking<'mshv> = MshvX64<'mshv>;
    type Shared = HypervisorBackedX86Shared;
    type EmulationCache = MshvEmulationCache;

    fn shared(shared: &BackingShared) -> &Self::Shared {
        let BackingShared::Hypervisor(shared) = shared else {
            unreachable!()
        };
        shared
    }

    fn new(
        params: BackingParams<'_, '_, Self>,
        _shared: &HypervisorBackedX86Shared,
    ) -> Result<Self, Error> {
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

        Ok(Self {
            deliverability_notifications: Default::default(),
            next_deliverability_notifications: Default::default(),
            stats: Default::default(),
            deferred_init: false,
        })
    }

    fn init(this: &mut UhProcessor<'_, Self>) {
        // The hypervisor initializes startup suspend to false. Set it to the
        // architectural default.
        if !this.vp_index().is_bsp() {
            this.backing.deferred_init = true;
        }
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

    fn pre_run_vp(this: &mut UhProcessor<'_, Self>) {
        if std::mem::take(&mut this.backing.deferred_init) {
            tracelimit::info_ratelimited!(
                vp = this.vp_index().index(),
                "sending deferred INIT to set startup suspend"
            );
            this.partition.request_msi(
                GuestVtl::Vtl0,
                virt::irqcon::MsiRequest::new_x86(
                    virt::irqcon::DeliveryMode::INIT,
                    this.inner.vp_info.apic_id,
                    false,
                    0,
                    true,
                ),
            );
        }
    }

    async fn run_vp(
        this: &mut UhProcessor<'_, Self>,
        dev: &impl CpuIo,
        stop: &mut StopVp<'_>,
    ) -> Result<(), VpHaltReason> {
        if this.backing.deliverability_notifications
            != this.backing.next_deliverability_notifications
        {
            let notifications = this.backing.next_deliverability_notifications;
            tracing::trace!(?notifications, "setting notifications");
            this.runner
                .set_vp_register(
                    // TODO GUEST VSM
                    GuestVtl::Vtl0,
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
                .map_err(|e| dev.fatal_error(e.into()))?;
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
            .map_err(|e| dev.fatal_error(ioctl::Error::Sidecar(e).into()))?
        } else {
            this.unlock_tlb_lock(Vtl::Vtl2);
            this.runner
                .run()
                .map_err(|e| dev.fatal_error(MshvRunVpError(e).into()))?
        };

        if intercepted {
            let message_type = this.runner.exit_message().header.typ;

            let mut intercept_handler =
                InterceptHandler::new(this).map_err(|e| dev.fatal_error(e.into()))?;

            let stat = match message_type {
                HvMessageType::HvMessageTypeX64IoPortIntercept => {
                    intercept_handler.handle_io_port_exit(dev).await?;
                    &mut this.backing.stats.io_port
                }
                HvMessageType::HvMessageTypeUnmappedGpa
                | HvMessageType::HvMessageTypeGpaIntercept => {
                    intercept_handler.handle_mmio_exit(dev).await?;
                    &mut this.backing.stats.mmio
                }
                HvMessageType::HvMessageTypeUnacceptedGpa => {
                    intercept_handler
                        .handle_unaccepted_gpa_intercept(dev)
                        .await?;
                    &mut this.backing.stats.unaccepted_gpa
                }
                HvMessageType::HvMessageTypeHypercallIntercept => {
                    intercept_handler.handle_hypercall_exit(dev);
                    &mut this.backing.stats.hypercall
                }
                HvMessageType::HvMessageTypeSynicSintDeliverable => {
                    intercept_handler.handle_synic_deliverable_exit();
                    &mut this.backing.stats.synic_deliverable
                }
                HvMessageType::HvMessageTypeX64InterruptionDeliverable => {
                    intercept_handler.handle_interrupt_deliverable_exit(dev)?;
                    &mut this.backing.stats.interrupt_deliverable
                }
                HvMessageType::HvMessageTypeX64CpuidIntercept => {
                    intercept_handler.handle_cpuid_intercept();
                    &mut this.backing.stats.cpuid
                }
                HvMessageType::HvMessageTypeMsrIntercept => {
                    intercept_handler.handle_msr_intercept();
                    &mut this.backing.stats.msr
                }
                HvMessageType::HvMessageTypeX64ApicEoi => {
                    intercept_handler.handle_eoi(dev);
                    &mut this.backing.stats.eoi
                }
                HvMessageType::HvMessageTypeUnrecoverableException => {
                    intercept_handler.handle_unrecoverable_exception()?;
                    &mut this.backing.stats.unrecoverable_exception
                }
                HvMessageType::HvMessageTypeExceptionIntercept => {
                    intercept_handler.handle_exception(dev)?;
                    &mut this.backing.stats.exception_intercept
                }
                reason => unreachable!("unknown exit reason: {:#x?}", reason),
            };
            stat.increment();

            if this.runner.is_sidecar()
                && !this.signaled_sidecar_exit
                && !this.partition.no_sidecar_hotplug.load(Relaxed)
            {
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
                this.signaled_sidecar_exit = true;
                return Err(VpHaltReason::Cancel);
            }
        }
        Ok(())
    }

    fn poll_apic(_this: &mut UhProcessor<'_, Self>, _vtl: GuestVtl, _scan_irr: bool) {}

    fn process_interrupts(
        _this: &mut UhProcessor<'_, Self>,
        _scan_irr: hv1_structs::VtlArray<bool, 2>,
        _first_scan_irr: &mut bool,
        _dev: &impl CpuIo,
    ) -> bool {
        false
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

    fn hv(&self, _vtl: GuestVtl) -> Option<&ProcessorVtlHv> {
        None
    }

    fn hv_mut(&mut self, _vtl: GuestVtl) -> Option<&mut ProcessorVtlHv> {
        None
    }

    fn handle_vp_start_enable_vtl_wake(_this: &mut UhProcessor<'_, Self>, _vtl: GuestVtl) {
        unimplemented!()
    }

    fn vtl1_inspectable(_this: &UhProcessor<'_, Self>) -> bool {
        // TODO: Use the VsmVpStatus register to query the hypervisor for
        // whether VTL 1 is enabled on the vp (this can be cached).
        false
    }
}

fn parse_sidecar_exit(message: &hvdef::HvMessage) -> SidecarRemoveExit {
    match message.header.typ {
        HvMessageType::HvMessageTypeX64IoPortIntercept => {
            let message = message.as_message::<hvdef::HvX64IoPortInterceptMessage>();
            SidecarRemoveExit::Io {
                port: message.port_number,
                write: message.header.intercept_access_type == HvInterceptAccessType::WRITE,
            }
        }
        HvMessageType::HvMessageTypeUnmappedGpa | HvMessageType::HvMessageTypeGpaIntercept => {
            let message = message.as_message::<hvdef::HvX64MemoryInterceptMessage>();
            SidecarRemoveExit::Mmio {
                gpa: message.guest_physical_address,
                write: message.header.intercept_access_type == HvInterceptAccessType::WRITE,
            }
        }
        HvMessageType::HvMessageTypeHypercallIntercept => {
            let message = message.as_message::<hvdef::HvX64HypercallInterceptMessage>();
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
            let message = message.as_message::<hvdef::HvX64CpuidInterceptMessage>();
            SidecarRemoveExit::Cpuid {
                leaf: message.rax as u32,
                subleaf: message.rcx as u32,
            }
        }
        HvMessageType::HvMessageTypeMsrIntercept => {
            let message = message.as_message::<hvdef::HvX64MsrInterceptMessage>();
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

struct InterceptHandler<'a, 'b> {
    vp: &'a mut UhProcessor<'b, HypervisorBackedX86>,
    intercepted_vtl: GuestVtl,
}

#[derive(Debug, Error)]
#[error("invalid intercepted vtl {0:?}")]
struct InvalidInterceptedVtl(u8);

#[derive(Debug, Error)]
#[error("guest accessed unaccepted gpa {0}")]
struct UnacceptedMemoryAccess(u64);

impl<'a, 'b> InterceptHandler<'a, 'b> {
    fn new(
        vp: &'a mut UhProcessor<'b, HypervisorBackedX86>,
    ) -> Result<Self, InvalidInterceptedVtl> {
        let message_type = vp.runner.exit_message().header.typ;

        let intercepted_vtl = match vp.runner.reg_page_vtl() {
            Ok(vtl) => vtl,
            Err(ioctl::x64::RegisterPageVtlError::InvalidVtl(vtl)) => {
                return Err(InvalidInterceptedVtl(vtl));
            }
            Err(ioctl::x64::RegisterPageVtlError::NoRegisterPage) => {
                if matches!(&message_type, &HvMessageType::HvMessageTypeX64ApicEoi) {
                    // At the moment this is only used for the ioapic, so assume
                    // that this is targeting VTL 0 for now. TODO: fix
                    GuestVtl::Vtl0
                } else {
                    let message_header = match &message_type {
                        &HvMessageType::HvMessageTypeX64IoPortIntercept => {
                            &vp.runner
                                .exit_message()
                                .as_message::<hvdef::HvX64IoPortInterceptMessage>()
                                .header
                        }
                        &HvMessageType::HvMessageTypeUnmappedGpa
                        | &HvMessageType::HvMessageTypeGpaIntercept => {
                            &vp.runner
                                .exit_message()
                                .as_message::<hvdef::HvX64MemoryInterceptMessage>()
                                .header
                        }
                        &HvMessageType::HvMessageTypeUnacceptedGpa => {
                            &vp.runner
                                .exit_message()
                                .as_message::<hvdef::HvX64MemoryInterceptMessage>()
                                .header
                        }
                        &HvMessageType::HvMessageTypeHypercallIntercept => {
                            &vp.runner
                                .exit_message()
                                .as_message::<hvdef::HvX64HypercallInterceptMessage>()
                                .header
                        }
                        &HvMessageType::HvMessageTypeSynicSintDeliverable => {
                            &vp.runner
                                .exit_message()
                                .as_message::<hvdef::HvX64SynicSintDeliverableMessage>()
                                .header
                        }
                        &HvMessageType::HvMessageTypeX64InterruptionDeliverable => {
                            &vp.runner
                                .exit_message()
                                .as_message::<hvdef::HvX64InterruptionDeliverableMessage>()
                                .header
                        }
                        &HvMessageType::HvMessageTypeX64CpuidIntercept => {
                            &vp.runner
                                .exit_message()
                                .as_message::<hvdef::HvX64CpuidInterceptMessage>()
                                .header
                        }
                        &HvMessageType::HvMessageTypeMsrIntercept => {
                            &vp.runner
                                .exit_message()
                                .as_message::<hvdef::HvX64MsrInterceptMessage>()
                                .header
                        }
                        &HvMessageType::HvMessageTypeUnrecoverableException => {
                            &vp.runner
                                .exit_message()
                                .as_message::<hvdef::HvX64UnrecoverableExceptionMessage>()
                                .header
                        }
                        &HvMessageType::HvMessageTypeX64Halt => {
                            &vp.runner
                                .exit_message()
                                .as_message::<hvdef::HvX64HaltMessage>()
                                .header
                        }
                        &HvMessageType::HvMessageTypeExceptionIntercept => {
                            &vp.runner
                                .exit_message()
                                .as_message::<hvdef::HvX64ExceptionInterceptMessage>()
                                .header
                        }
                        reason => unreachable!("unknown exit reason: {:#x?}", reason),
                    };

                    message_header
                        .execution_state
                        .vtl()
                        .try_into()
                        .map_err(|hcl::UnsupportedGuestVtl(vtl)| InvalidInterceptedVtl(vtl))?
                }
            }
        };

        Ok(Self {
            vp,
            intercepted_vtl,
        })
    }

    fn handle_interrupt_deliverable_exit(&mut self, bus: &impl CpuIo) -> Result<(), VpHaltReason> {
        let message = self
            .vp
            .runner
            .exit_message()
            .as_message::<hvdef::HvX64InterruptionDeliverableMessage>();

        assert_eq!(
            message.deliverable_type,
            HvX64PendingInterruptionType::HV_X64_PENDING_INTERRUPT
        );

        self.vp
            .backing
            .deliverability_notifications
            .set_interrupt_notification(false);

        self.vp
            .backing
            .next_deliverability_notifications
            .set_interrupt_notification(false);

        if let Some(vector) = bus.acknowledge_pic_interrupt() {
            let event = hvdef::HvX64PendingExtIntEvent::new()
                .with_event_pending(true)
                .with_event_type(hvdef::HV_X64_PENDING_EVENT_EXT_INT)
                .with_vector(vector);

            self.vp
                .runner
                .set_vp_register(
                    self.intercepted_vtl,
                    HvX64RegisterName::PendingEvent0,
                    u128::from(event).into(),
                )
                .unwrap();
        }

        Ok(())
    }

    fn handle_synic_deliverable_exit(&mut self) {
        let message = self
            .vp
            .runner
            .exit_message()
            .as_message::<hvdef::HvX64SynicSintDeliverableMessage>();

        tracing::trace!(
            deliverable_sints = message.deliverable_sints,
            "sint deliverable"
        );

        self.vp.backing.deliverability_notifications.set_sints(
            self.vp.backing.deliverability_notifications.sints() & !message.deliverable_sints,
        );

        // This is updated by `deliver_synic_messages below`, so clear it here.
        self.vp
            .backing
            .next_deliverability_notifications
            .set_sints(0);

        // These messages are always delivered to VTL0, as VTL1 does not own any VMBUS channels.
        self.vp
            .deliver_synic_messages(GuestVtl::Vtl0, message.deliverable_sints);
    }

    fn handle_hypercall_exit(&mut self, bus: &impl CpuIo) {
        let message = self
            .vp
            .runner
            .exit_message()
            .as_message::<hvdef::HvX64HypercallInterceptMessage>();

        tracing::trace!(msg = %format_args!("{:x?}", message), "hypercall");

        let is_64bit =
            message.header.execution_state.cr0_pe() && message.header.execution_state.efer_lma();

        let guest_memory = &self.vp.partition.gm[self.intercepted_vtl];
        let handler = UhHypercallHandler {
            vp: self.vp,
            bus,
            trusted: false,
            intercepted_vtl: self.intercepted_vtl,
        };
        UhHypercallHandler::MSHV_DISPATCHER.dispatch(
            guest_memory,
            hv1_hypercall::X64RegisterIo::new(handler, is_64bit),
        );
    }

    async fn handle_mmio_exit(&mut self, dev: &impl CpuIo) -> Result<(), VpHaltReason> {
        let message = self
            .vp
            .runner
            .exit_message()
            .as_message::<hvdef::HvX64MemoryInterceptMessage>();

        tracing::trace!(msg = %format_args!("{:x?}", message), "mmio");

        let interruption_pending = message.header.execution_state.interruption_pending();

        // Fast path for monitor page writes.
        if Some(message.guest_physical_address & !(HV_PAGE_SIZE - 1))
            == self.vp.partition.monitor_page.gpa()
            && message.header.intercept_access_type == HvInterceptAccessType::WRITE
        {
            let tlb_lock_held = message.memory_access_info.gva_gpa_valid()
                || message.memory_access_info.tlb_locked();
            let guest_memory = &self.vp.partition.gm[self.intercepted_vtl];
            let cache = self.vp.emulation_cache(self.intercepted_vtl);
            let mut emulation_state = UhEmulationState {
                vp: &mut *self.vp,
                interruption_pending,
                devices: dev,
                vtl: self.intercepted_vtl,
                cache,
            };
            if let Some(bit) = virt_support_x86emu::emulate::emulate_mnf_write_fast_path(
                &mut emulation_state,
                guest_memory,
                dev,
                interruption_pending,
                tlb_lock_held,
            ) {
                if let Some(connection_id) = self.vp.partition.monitor_page.write_bit(bit) {
                    signal_mnf(dev, connection_id);
                }
                return Ok(());
            }
        }

        let cache = self.vp.emulation_cache(self.intercepted_vtl);
        self.vp
            .emulate(dev, interruption_pending, self.intercepted_vtl, cache)
            .await
    }

    async fn handle_io_port_exit(&mut self, dev: &impl CpuIo) -> Result<(), VpHaltReason> {
        let message = self
            .vp
            .runner
            .exit_message()
            .as_message::<hvdef::HvX64IoPortInterceptMessage>();

        tracing::trace!(msg = %format_args!("{:x?}", message), "io_port");

        assert_eq!(message.rax, self.vp.runner.cpu_context().gps[protocol::RAX]);

        let interruption_pending = message.header.execution_state.interruption_pending();

        if message.access_info.string_op() || message.access_info.rep_prefix() {
            let cache = self.vp.emulation_cache(self.intercepted_vtl);
            self.vp
                .emulate(dev, interruption_pending, self.intercepted_vtl, cache)
                .await
        } else {
            let next_rip = next_rip(&message.header);
            let access_size = message.access_info.access_size();
            virt_support_x86emu::emulate::emulate_io(
                self.vp.vp_index(),
                message.header.intercept_access_type == HvInterceptAccessType::WRITE,
                message.port_number,
                &mut self.vp.runner.cpu_context_mut().gps[protocol::RAX],
                access_size,
                dev,
            )
            .await;
            self.vp.set_rip(self.intercepted_vtl, next_rip);
            Ok(())
        }
    }

    async fn handle_unaccepted_gpa_intercept(
        &mut self,
        dev: &impl CpuIo,
    ) -> Result<(), VpHaltReason> {
        let gpa = self
            .vp
            .runner
            .exit_message()
            .as_message::<hvdef::HvX64MemoryInterceptMessage>()
            .guest_physical_address;

        if self.vp.partition.is_gpa_lower_vtl_ram(gpa) {
            // The host may have moved the page to an unaccepted state, so fail
            // here. This does not apply to VTL 2 memory - for unaccepted pages,
            // the intercept goes to host VTL0.
            //
            // Note: SGX memory should be included in this check, so if SGX is
            // no longer included in the lower_vtl_memory_layout, make sure the
            // appropriate changes are reflected here.
            Err(dev.fatal_error(UnacceptedMemoryAccess(gpa).into()))
        } else {
            self.handle_mmio_exit(dev).await
        }
    }

    fn handle_cpuid_intercept(&mut self) {
        let message = self
            .vp
            .runner
            .exit_message()
            .as_message::<hvdef::HvX64CpuidInterceptMessage>();

        let default_result = [
            message.default_result_rax as u32,
            message.default_result_rbx as u32,
            message.default_result_rcx as u32,
            message.default_result_rdx as u32,
        ];

        tracing::trace!(msg = %format_args!("{:x?}", message), "cpuid");

        let [eax, ebx, ecx, edx] =
            self.vp
                .partition
                .cpuid
                .result(message.rax as u32, message.rcx as u32, &default_result);

        let next_rip = next_rip(&message.header);
        self.vp.runner.cpu_context_mut().gps[protocol::RAX] = eax.into();
        self.vp.runner.cpu_context_mut().gps[protocol::RBX] = ebx.into();
        self.vp.runner.cpu_context_mut().gps[protocol::RCX] = ecx.into();
        self.vp.runner.cpu_context_mut().gps[protocol::RDX] = edx.into();

        self.vp.set_rip(self.intercepted_vtl, next_rip);
    }

    fn handle_msr_intercept(&mut self) {
        let message = self
            .vp
            .runner
            .exit_message()
            .as_message::<hvdef::HvX64MsrInterceptMessage>();
        let rip = next_rip(&message.header);

        tracing::trace!(msg = %format_args!("{:x?}", message), "msr");

        let msr = message.msr_number;
        match message.header.intercept_access_type {
            HvInterceptAccessType::READ => {
                // Only supported MSRs are the crash MSRs.
                let value = match self.vp.read_crash_msr(msr, self.intercepted_vtl) {
                    Ok(v) => v,
                    Err(MsrError::Unknown) => {
                        tracing::trace!(msr, "unknown msr read");
                        0
                    }
                    Err(MsrError::InvalidAccess) => {
                        self.vp.inject_gpf(self.intercepted_vtl);
                        // Do not advance RIP.
                        return;
                    }
                };

                self.vp.runner.cpu_context_mut().gps[protocol::RAX] = value & 0xffff_ffff;
                self.vp.runner.cpu_context_mut().gps[protocol::RDX] = value >> 32;
            }
            HvInterceptAccessType::WRITE => {
                let value = (message.rax & 0xffff_ffff) | (message.rdx << 32);
                // Only supported MSRs are the crash MSRs.
                match self.vp.write_crash_msr(msr, value, self.intercepted_vtl) {
                    Ok(()) => {}
                    Err(MsrError::Unknown) => {
                        tracing::trace!(msr, value, "unknown msr write");
                    }
                    Err(MsrError::InvalidAccess) => {
                        self.vp.inject_gpf(self.intercepted_vtl);
                        // Do not advance RIP.
                        return;
                    }
                }
            }
            _ => unreachable!(),
        }

        self.vp.set_rip(self.intercepted_vtl, rip);
    }

    fn handle_eoi(&self, dev: &impl CpuIo) {
        let message = self
            .vp
            .runner
            .exit_message()
            .as_message::<hvdef::HvX64ApicEoiMessage>();

        tracing::trace!(msg = %format_args!("{:x?}", message), "eoi");

        dev.handle_eoi(message.interrupt_vector);
    }

    fn handle_unrecoverable_exception(&self) -> Result<(), VpHaltReason> {
        Err(VpHaltReason::TripleFault {
            vtl: self.intercepted_vtl.into(),
        })
    }

    fn handle_exception(&mut self, dev: &impl CpuIo) -> Result<(), VpHaltReason> {
        let message = self
            .vp
            .runner
            .exit_message()
            .as_message::<hvdef::HvX64ExceptionInterceptMessage>();

        match x86defs::Exception(message.vector as u8) {
            x86defs::Exception::DEBUG if cfg!(feature = "gdb") => {
                self.vp.handle_debug_exception(dev, self.intercepted_vtl)?
            }
            _ => tracing::error!("unexpected exception type {:#x?}", message.vector),
        }
        Ok(())
    }
}

impl UhProcessor<'_, HypervisorBackedX86> {
    fn set_rip(&mut self, vtl: GuestVtl, rip: u64) {
        self.runner
            .set_vp_register(vtl, HvX64RegisterName::Rip, rip.into())
            .unwrap();
    }

    fn inject_gpf(&mut self, vtl: GuestVtl) {
        let exception_event = hvdef::HvX64PendingExceptionEvent::new()
            .with_event_pending(true)
            .with_event_type(hvdef::HV_X64_PENDING_EVENT_EXCEPTION)
            .with_vector(x86defs::Exception::GENERAL_PROTECTION_FAULT.0.into())
            .with_deliver_error_code(true)
            .with_error_code(0);

        self.runner
            .set_vp_register(
                vtl,
                HvX64RegisterName::PendingEvent0,
                u128::from(exception_event).into(),
            )
            .expect("set_vp_register should succeed for pending event");
    }

    fn set_vsm_partition_config(
        &mut self,
        vtl: GuestVtl,
        value: HvRegisterVsmPartitionConfig,
    ) -> Result<(), HvError> {
        if vtl != GuestVtl::Vtl1 {
            return Err(HvError::InvalidParameter);
        }

        assert!(self.partition.isolation.is_isolated());

        let status = self
            .partition
            .vsm_status()
            .expect("cannot fail to query vsm status");

        let vtl1_enabled = VtlSet::from(status.enabled_vtl_set()).is_set(GuestVtl::Vtl1);
        if !vtl1_enabled {
            return Err(HvError::InvalidVtlState);
        }

        let mut guest_vsm_lock = self.shared.guest_vsm.write();

        // Initialize partition.guest_vsm state if necessary.
        match *guest_vsm_lock {
            GuestVsmState::NotPlatformSupported => {
                return Err(HvError::AccessDenied);
            }
            GuestVsmState::NotGuestEnabled => {
                // TODO: check status
                *guest_vsm_lock = GuestVsmState::Enabled {
                    vtl1: Default::default(),
                };
            }
            GuestVsmState::Enabled { .. } => {}
        }

        let GuestVsmState::Enabled { vtl1 } = &mut *guest_vsm_lock else {
            unreachable!()
        };
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
            if vtl1.enable_vtl_protection {
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
        if let Some(current_protections) = vtl1.default_vtl_protections {
            if protections != current_protections {
                return Err(HvError::InvalidRegisterValue);
            }
        }
        vtl1.default_vtl_protections = Some(protections);

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
        vtl1.enable_vtl_protection = true;

        Ok(())
    }

    ///Eagerly load registers for emulation
    ///Typically we load expensive registers lazily, however some registers will always be used,
    ///and the underlying ioctl supports batching multiple register retrievals into a single call
    fn emulation_cache(&mut self, vtl: GuestVtl) -> MshvEmulationCache {
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
        let mut values = [FromZeros::new_zeroed(); NAMES.len()];
        self.runner
            .get_vp_registers(vtl, NAMES, &mut values)
            .expect("register query should not fail");

        let [rsp, es, ds, fs, gs, ss, cr0, efer] = values;

        let header = self
            .runner
            .exit_message()
            .as_message::<HvX64InterceptMessageHeader>();

        MshvEmulationCache {
            rsp: rsp.as_u64(),
            es: from_seg(es.into()),
            ds: from_seg(ds.into()),
            fs: from_seg(fs.into()),
            gs: from_seg(gs.into()),
            ss: from_seg(ss.into()),
            cr0: cr0.as_u64(),
            efer: efer.as_u64(),
            rip: header.rip,
            rflags: header.rflags.into(),
        }
    }
}

fn from_seg(reg: hvdef::HvX64SegmentRegister) -> SegmentRegister {
    SegmentRegister {
        base: reg.base,
        limit: reg.limit,
        selector: reg.selector,
        attributes: reg.attributes.into(),
    }
}

impl<T: CpuIo> EmulatorSupport for UhEmulationState<'_, '_, T, HypervisorBackedX86> {
    fn flush(&mut self) {
        self.vp
            .runner
            .set_vp_registers(
                self.vtl,
                [
                    (HvX64RegisterName::Rip, self.cache.rip),
                    (HvX64RegisterName::Rflags, self.cache.rflags.into()),
                    (HvX64RegisterName::Rsp, self.cache.rsp),
                ],
            )
            .unwrap();
    }

    fn vp_index(&self) -> VpIndex {
        self.vp.vp_index()
    }

    fn vendor(&self) -> x86defs::cpuid::Vendor {
        self.vp.partition.caps.vendor
    }

    fn gp(&mut self, reg: x86emu::Gp) -> u64 {
        match reg {
            x86emu::Gp::RSP => self.cache.rsp,
            _ => self.vp.runner.cpu_context().gps[reg as usize],
        }
    }

    fn set_gp(&mut self, reg: x86emu::Gp, v: u64) {
        if reg == x86emu::Gp::RSP {
            self.cache.rsp = v;
        }
        self.vp.runner.cpu_context_mut().gps[reg as usize] = v;
    }

    fn xmm(&mut self, index: usize) -> u128 {
        u128::from_le_bytes(self.vp.runner.cpu_context().fx_state.xmm[index])
    }

    fn set_xmm(&mut self, index: usize, v: u128) {
        self.vp.runner.cpu_context_mut().fx_state.xmm[index] = v.to_le_bytes();
    }

    fn rip(&mut self) -> u64 {
        self.cache.rip
    }

    fn set_rip(&mut self, v: u64) {
        self.cache.rip = v;
    }

    fn segment(&mut self, index: x86emu::Segment) -> SegmentRegister {
        match index {
            x86emu::Segment::CS => {
                let header = self
                    .vp
                    .runner
                    .exit_message()
                    .as_message::<HvX64InterceptMessageHeader>();
                from_seg(header.cs_segment)
            }
            x86emu::Segment::ES => self.cache.es,
            x86emu::Segment::SS => self.cache.ss,
            x86emu::Segment::DS => self.cache.ds,
            x86emu::Segment::FS => self.cache.fs,
            x86emu::Segment::GS => self.cache.gs,
        }
    }

    fn efer(&mut self) -> u64 {
        self.cache.efer
    }

    fn cr0(&mut self) -> u64 {
        self.cache.cr0
    }

    fn rflags(&mut self) -> RFlags {
        self.cache.rflags
    }

    fn set_rflags(&mut self, v: RFlags) {
        self.cache.rflags = v;
    }

    fn instruction_bytes(&self) -> &[u8] {
        let message = self.vp.runner.exit_message();
        match message.header.typ {
            HvMessageType::HvMessageTypeGpaIntercept
            | HvMessageType::HvMessageTypeUnmappedGpa
            | HvMessageType::HvMessageTypeUnacceptedGpa => {
                let message = message.as_message::<hvdef::HvX64MemoryInterceptMessage>();
                &message.instruction_bytes[..message.instruction_byte_count as usize]
            }
            HvMessageType::HvMessageTypeX64IoPortIntercept => {
                let message = message.as_message::<hvdef::HvX64IoPortInterceptMessage>();
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
                let message = message.as_message::<hvdef::HvX64MemoryInterceptMessage>();
                Some(message.guest_physical_address)
            }
            _ => None,
        }
    }

    fn initial_gva_translation(
        &mut self,
    ) -> Option<virt_support_x86emu::emulate::InitialTranslation> {
        if (self.vp.runner.exit_message().header.typ != HvMessageType::HvMessageTypeGpaIntercept)
            && (self.vp.runner.exit_message().header.typ != HvMessageType::HvMessageTypeUnmappedGpa)
            && (self.vp.runner.exit_message().header.typ
                != HvMessageType::HvMessageTypeUnacceptedGpa)
        {
            return None;
        }

        let message = self
            .vp
            .runner
            .exit_message()
            .as_message::<hvdef::HvX64MemoryInterceptMessage>();

        if !message.memory_access_info.gva_gpa_valid() {
            tracing::trace!(?message.guest_virtual_address, ?message.guest_physical_address, "gva gpa not valid {:?}", self.vp.runner.exit_message().payload());
            return None;
        }

        let translate_mode = virt_support_x86emu::emulate::TranslateMode::try_from(
            message.header.intercept_access_type,
        )
        .expect("unexpected intercept access type");

        let translation = virt_support_x86emu::emulate::InitialTranslation {
            gva: message.guest_virtual_address,
            gpa: message.guest_physical_address,
            translate_mode,
        };

        tracing::trace!(?translation, "initial translation");

        // If we have a valid translation, the hypervisor must have set the TLB lock
        // so the translation remains valid for the duration of this exit.
        // Update our local cache appropriately.
        self.vp.mark_tlb_locked(Vtl::Vtl2, self.vtl);

        Some(translation)
    }

    fn interruption_pending(&self) -> bool {
        self.interruption_pending
    }

    fn check_vtl_access(
        &mut self,
        gpa: u64,
        mode: virt_support_x86emu::emulate::TranslateMode,
    ) -> Result<(), EmuCheckVtlAccessError> {
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
            && self.vtl == GuestVtl::Vtl0
            && !matches!(
                *self.vp.shared.guest_vsm.read(),
                GuestVsmState::NotPlatformSupported,
            )
        {
            // Should always be called after translate gva with the tlb lock flag
            // or with an initial translation.
            debug_assert!(self.vp.is_tlb_locked(Vtl::Vtl2, self.vtl));

            let mbec_user_execute = self
                .vp
                .runner
                .get_vp_register(self.vtl, HvX64RegisterName::InstructionEmulationHints)
                .unwrap();

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
                .check_vtl_access(gpa, self.vtl, flags)
                .unwrap();

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
    ) -> Result<EmuTranslateResult, EmuTranslateError> {
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

        // The translation will be used, so set the appropriate page table bits
        // (the access/dirty bit).
        //
        // Prevent flushes in order to make sure that translation of this GVA
        // remains usable until the VP is resumed back to direct execution.
        control_flags.set_set_page_table_bits(true);
        control_flags.set_tlb_flush_inhibit(true);

        // In case we're not running ring 0, check privileges against VP state
        // as of when the original intercept came in - since the emulator
        // doesn't support instructions that change ring level, the ring level
        // will remain the same as it was in the VP state as of when the
        // original intercept came in. The privilege exempt flag should
        // not be set.
        assert!(!control_flags.privilege_exempt());

        // Do the translation using the current VTL.
        control_flags.set_input_vtl(self.vtl.into());

        match self
            .vp
            .runner
            .translate_gva_to_gpa(gva, control_flags)
            .unwrap()
        {
            Ok(ioctl::TranslateResult {
                gpa_page,
                overlay_page,
            }) => {
                self.vp.mark_tlb_locked(Vtl::Vtl2, self.vtl);
                Ok(EmuTranslateResult {
                    gpa: (gpa_page << hvdef::HV_PAGE_SHIFT) + (gva & (HV_PAGE_SIZE - 1)),
                    overlay_page: Some(overlay_page),
                })
            }
            Err(ioctl::x64::TranslateErrorX64 { code, event_info }) => Err(EmuTranslateError {
                code: hypercall::TranslateGvaResultCode(code),
                event_info: Some(event_info),
            }),
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

        self.vp
            .runner
            .set_vp_registers_hvcall(self.vtl.into(), regs)
            .expect("set_vp_registers hypercall for setting pending event should not fail");
    }

    fn monitor_support(&self) -> Option<&dyn EmulatorMonitorSupport> {
        Some(self)
    }

    fn is_gpa_mapped(&self, gpa: u64, write: bool) -> bool {
        self.vp.partition.is_gpa_mapped(gpa, write)
    }

    fn lapic_base_address(&self) -> Option<u64> {
        None
    }

    fn lapic_read(&mut self, _address: u64, _data: &mut [u8]) {
        unimplemented!()
    }

    fn lapic_write(&mut self, _address: u64, _data: &[u8]) {
        unimplemented!()
    }
}

impl<T: CpuIo> UhHypercallHandler<'_, '_, T, HypervisorBackedX86> {
    const MSHV_DISPATCHER: hv1_hypercall::Dispatcher<Self> = hv1_hypercall::dispatcher!(
        Self,
        [
            hv1_hypercall::HvPostMessage,
            hv1_hypercall::HvSignalEvent,
            hv1_hypercall::HvRetargetDeviceInterrupt,
            hv1_hypercall::HvGetVpIndexFromApicId,
            hv1_hypercall::HvSetVpRegisters,
            hv1_hypercall::HvModifyVtlProtectionMask
        ]
    );
}

impl<T> hv1_hypercall::X64RegisterState for UhHypercallHandler<'_, '_, T, HypervisorBackedX86> {
    fn rip(&mut self) -> u64 {
        self.vp
            .runner
            .exit_message()
            .as_message::<HvX64InterceptMessageHeader>()
            .rip
    }

    fn set_rip(&mut self, rip: u64) {
        self.vp.set_rip(self.intercepted_vtl, rip)
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
            .set_vp_registers(self.vtl, names.iter().copied().zip(values))
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
            .get_vp_registers(self.vtl, &names, &mut values)
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
        #[derive(IntoBytes, Immutable, KnownLayout)]
        struct XsaveStandard {
            fxsave: Fxsave,
            xsave_header: XsaveHeader,
        }
        let state = XsaveStandard {
            fxsave: self.vp.runner.cpu_context().fx_state.clone(),
            xsave_header: XsaveHeader {
                xstate_bv: XFEATURE_X87 | XFEATURE_SSE,
                ..FromZeros::new_zeroed()
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

    fn mtrrs(&mut self) -> Result<vp::Mtrrs, Self::Error> {
        self.get_register_state()
    }

    fn set_mtrrs(&mut self, cc: &vp::Mtrrs) -> Result<(), Self::Error> {
        self.set_register_state(cc)
    }

    fn pat(&mut self) -> Result<vp::Pat, Self::Error> {
        self.get_register_state()
    }

    fn set_pat(&mut self, value: &vp::Pat) -> Result<(), Self::Error> {
        self.set_register_state(value)
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
        params: hv1_hypercall::HvInterruptParameters<'_>,
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
    ) -> HvRepResult {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err((HvError::AccessDenied, 0));
        }

        if vp_index != hvdef::HV_VP_INDEX_SELF && vp_index != self.vp.vp_index().index() {
            return Err((HvError::InvalidVpIndex, 0));
        }

        let target_vtl = self
            .target_vtl_no_higher(vtl.unwrap_or(self.intercepted_vtl.into()))
            .map_err(|e| (e, 0))?;

        for (i, reg) in registers.iter().enumerate() {
            if reg.name == HvX64RegisterName::VsmPartitionConfig.into() {
                let value = HvRegisterVsmPartitionConfig::from(reg.value.as_u64());
                self.vp
                    .set_vsm_partition_config(target_vtl, value)
                    .map_err(|e| (e, i))?;
            } else {
                return Err((HvError::InvalidParameter, i));
            }
        }

        Ok(())
    }
}

impl<T> hv1_hypercall::ModifyVtlProtectionMask
    for UhHypercallHandler<'_, '_, T, HypervisorBackedX86>
{
    fn modify_vtl_protection_mask(
        &mut self,
        partition_id: u64,
        _map_flags: HvMapGpaFlags,
        target_vtl: Option<Vtl>,
        gpa_pages: &[u64],
    ) -> HvRepResult {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err((HvError::AccessDenied, 0));
        }

        let target_vtl = self
            .target_vtl_no_higher(target_vtl.unwrap_or(self.intercepted_vtl.into()))
            .map_err(|e| (e, 0))?;
        if target_vtl == GuestVtl::Vtl0 {
            return Err((HvError::InvalidParameter, 0));
        }

        // A VTL cannot change its own VTL permissions until it has enabled VTL protection and
        // configured default permissions. Higher VTLs are not under this restriction (as they may
        // need to apply default permissions before VTL protection is enabled).
        if target_vtl == self.intercepted_vtl
            && !matches!(
                *self.vp.shared.guest_vsm.read(),
                GuestVsmState::Enabled {
                    vtl1: VbsIsolatedVtl1State {
                        enable_vtl_protection: true,
                        default_vtl_protections: Some(_),
                    },
                }
            )
        {
            return Err((HvError::AccessDenied, 0));
        }

        // TODO VBS GUEST VSM: verify this logic is correct
        // TODO VBS GUEST VSM: validation on map_flags, similar to default
        // protections mask changes
        // Can receive an intercept on adjust permissions, and for isolated
        // VMs if the page is unaccepted
        if self.vp.partition.isolation.is_isolated() {
            return Err((HvError::OperationDenied, 0));
        } else {
            if !gpa_pages.is_empty() {
                if !self.vp.partition.is_gpa_lower_vtl_ram(gpa_pages[0]) {
                    return Err((HvError::OperationDenied, 0));
                } else {
                    panic!("Should not be handling this hypercall for guest ram");
                }
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
    use hvdef::HV_X64_MSR_GUEST_CRASH_CTL;
    use hvdef::HvInternalActivityRegister;
    use hvdef::HvX64RegisterName;
    use hvdef::Vtl;
    use virt::Processor;
    use virt::vp::AccessVpState;
    use virt::vp::Mtrrs;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;
    use zerocopy::FromZeros;
    use zerocopy::IntoBytes;

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

            /// Only set when the DR6_SHARED capability is present
            #[mesh(22)]
            pub(super) dr6: Option<u64>,

            /// If VTL0 should be in the startup suspend state. Older underhill
            /// versions do not save this property, so maintain the old buggy
            /// behavior for those cases its not present in the saved state.
            #[mesh(23)]
            pub(super) startup_suspend: Option<bool>,

            #[mesh(24)]
            pub(super) crash_reg: Option<[u64; 5]>,

            /// This value is ignored going forward, but may still be read by downlevel
            /// versions.
            #[mesh(25)]
            pub(super) crash_control: u64,

            #[mesh(26)]
            pub(super) msr_mtrr_def_type: u64,
            #[mesh(27)]
            pub(super) fixed_mtrrs: Option<[u64; 11]>,
            #[mesh(28)]
            pub(super) variable_mtrrs: Option<[u64; 16]>,
            #[mesh(29)]
            pub(super) per_vtl: Vec<ProcessorVtlSavedState>,
        }

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "underhill.partition")]
        pub struct ProcessorVtlSavedState {
            #[mesh(1)]
            pub(super) message_queue: virt::vp::SynicMessageQueues,
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
            self.flush_async_requests();

            let dr6_shared = self.partition.hcl.dr6_shared();
            let mut values = [FromZeros::new_zeroed(); SHARED_REGISTERS.len()];
            let len = if dr6_shared {
                SHARED_REGISTERS.len()
            } else {
                SHARED_REGISTERS.len() - 1
            };

            self.runner
                // All these registers are shared, so the VTL we ask for doesn't matter
                .get_vp_registers(GuestVtl::Vtl0, &SHARED_REGISTERS[..len], &mut values[..len])
                .context("failed to get shared registers")
                .map_err(SaveError::Other)?;

            let [
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
            ] = self.runner.cpu_context().gps;

            // We are responsible for saving shared MSRs too, but other than
            // the MTRRs all shared MSRs are read-only. So this is all we need.
            let Mtrrs {
                msr_mtrr_def_type,
                fixed: fixed_mtrrs,
                variable: variable_mtrrs,
            } = self
                // MTRRs are shared, so it doesn't matter which VTL we ask for.
                .access_state(Vtl::Vtl0)
                .mtrrs()
                .context("failed to get MTRRs")
                .map_err(SaveError::Other)?;

            // This value is ignored during restore, but may still be read by downlevel
            // versions. Set it to the correct hardcoded read value as a best effort for them.
            let crash_control = self
                .read_crash_msr(HV_X64_MSR_GUEST_CRASH_CTL, GuestVtl::Vtl0)
                .unwrap();

            let UhProcessor {
                _not_send,
                inner:
                    crate::UhVpInner {
                        // Saved
                        message_queues,
                        // Sidecar state is reset during servicing
                        sidecar_exit_reason: _,
                        // Will be cleared by flush_async_requests above
                        wake_reasons: _,
                        // Runtime glue
                        waker: _,
                        // Topology information
                        vp_info: _,
                        cpu_index: _,
                    },
                // Saved
                crash_reg,
                // Runtime glue
                partition: _,
                idle_control: _,
                vmtime: _,
                timer: _,
                // This field is only used in dev/test scenarios
                force_exit_sidecar: _,
                signaled_sidecar_exit: _,
                // Just caching the hypervisor value, let it handle saving
                vtls_tlb_locked: _,
                // Statistic that should reset to 0 on restore
                kernel_returns: _,
                // Shared state should be handled by the backing
                shared: _,
                // The runner doesn't hold anything needing saving
                runner: _,
                backing:
                    HypervisorBackedX86 {
                        deliverability_notifications: _,
                        next_deliverability_notifications: _,
                        stats: _,
                        deferred_init,
                    },
                // Currently only meaningful for CVMs
                exit_activities: _,
            } = *self;

            // Non-VTL0 VPs should never be in startup suspend, so we only need to check VTL0.
            // The hypervisor handles halt and idle for us.
            let startup_suspend = if deferred_init {
                Some(true)
            } else {
                let internal_activity = self
                    .runner
                    .get_vp_register(GuestVtl::Vtl0, HvX64RegisterName::InternalActivityState)
                    .inspect_err(|e| {
                        // The ioctl get_vp_register path does not tell us
                        // hv_status directly, so just log if it failed for any
                        // reason.
                        tracing::warn!(
                            error = e as &dyn std::error::Error,
                            "unable to query startup suspend, unable to save VTL0 startup suspend state"
                        );
                    })
                    .ok();

                internal_activity
                    .map(|a| HvInternalActivityRegister::from(a.as_u64()).startup_suspend())
            };

            let per_vtl = [GuestVtl::Vtl0, GuestVtl::Vtl1]
                .map(|vtl| state::ProcessorVtlSavedState {
                    message_queue: message_queues[vtl].save(),
                })
                .into();

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
                crash_reg: Some(crash_reg),
                crash_control,
                msr_mtrr_def_type,
                fixed_mtrrs: Some(fixed_mtrrs),
                variable_mtrrs: Some(variable_mtrrs),
                per_vtl,
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
                crash_reg,
                crash_control: _crash_control,
                msr_mtrr_def_type,
                fixed_mtrrs,
                variable_mtrrs,
                per_vtl,
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
                .set_vp_registers(
                    GuestVtl::Vtl0,
                    SHARED_REGISTERS[..len].iter().copied().zip(values),
                )
                .context("failed to set shared registers")
                .map_err(RestoreError::Other)?;

            self.runner
                .cpu_context_mut()
                .fx_state
                .as_mut_bytes()
                .copy_from_slice(&fx_state);

            self.crash_reg = crash_reg.unwrap_or_default();

            // Previous versions of Underhill did not save the MTRRs.
            // If we get a restore state with them missing then assume they weren't
            // saved and don't zero out whatever the system already has.
            if let (Some(fixed), Some(variable)) = (fixed_mtrrs, variable_mtrrs) {
                let mut access = self.access_state(Vtl::Vtl0);
                access
                    .set_mtrrs(&Mtrrs {
                        msr_mtrr_def_type,
                        fixed,
                        variable,
                    })
                    .context("failed to set MTRRs")
                    .map_err(RestoreError::Other)?;
            }

            for (per, vtl) in per_vtl.into_iter().zip(0u8..) {
                let vtl = GuestVtl::try_from(vtl)
                    .context("too many vtls")
                    .map_err(RestoreError::Other)?;
                self.inner.message_queues[vtl].restore(&per.message_queue);
            }

            let startup_suspend = match startup_suspend {
                Some(true) => {
                    // When Underhill brings up APs during a servicing update
                    // via hypercall, this clears the VTL0 startup suspend
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
                    let mut values = [FromZeros::new_zeroed(); NAMES.len()];
                    self.runner
                        // Non-VTL0 VPs should never be in startup suspend, so we only need to handle VTL0.
                        .get_vp_registers(GuestVtl::Vtl0, &NAMES, &mut values)
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

            self.backing.deferred_init = match self.set_vtl0_startup_suspend(startup_suspend) {
                Ok(()) => false,
                Err(e) => {
                    if startup_suspend {
                        tracing::warn!(
                            error = &e as &dyn std::error::Error,
                            "unable to set internal activity register, falling back to deferred init"
                        );
                    }
                    startup_suspend
                }
            };
            Ok(())
        }
    }
}
