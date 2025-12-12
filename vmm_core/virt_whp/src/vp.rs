// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Vplc;
use super::VtlPartition;
use super::vtl2::Vtl2InterceptState;
use crate::WhpProcessor;
use guestmem::GuestMemoryError;
use hvdef::HvDeliverabilityNotificationsRegister;
use hvdef::HvError;
use hvdef::HvMessage;
use hvdef::HvMessageType;
use hvdef::HvVtlEntryReason;
use hvdef::Vtl;
use inspect::Inspect;
use inspect_counters::Counter;
use std::convert::Infallible;
use std::future::poll_fn;
use std::mem::offset_of;
use std::sync::atomic::Ordering;
use std::task::Poll;
use thiserror::Error;
use tracing_helpers::ErrorValueExt;
use virt::StopVp;
use virt::VpHaltReason;
use virt::io::CpuIo;
use virt::vp::AccessVpState;
use zerocopy::IntoBytes;

#[derive(Debug, Default, Inspect)]
pub(crate) struct ExitStats {
    msr: Counter,
    hypercall: Counter,
    #[cfg(guest_arch = "x86_64")]
    interrupt_window: Counter,
    sint_deliverable: Counter,
    #[cfg(guest_arch = "x86_64")]
    io: Counter,
    memory: Counter,
    #[cfg(guest_arch = "x86_64")]
    cpuid: Counter,
    #[cfg(guest_arch = "x86_64")]
    apic_eoi: Counter,
    cancel: Counter,
    halt: Counter,
    #[cfg(guest_arch = "x86_64")]
    exception: Counter,
    other: Counter,
}

#[derive(Debug, Error)]
#[error("failed to run")]
pub struct WhpRunVpError(#[source] whp::WHvError);

impl<'a> WhpProcessor<'a> {
    pub(crate) fn current_vtlp(&self) -> &'a VtlPartition {
        self.vp.partition.vtlp(self.state.active_vtl)
    }

    pub(crate) fn vplc(&self, vtl: Vtl) -> &'a Vplc {
        match vtl {
            Vtl::Vtl0 => self.vplc0,
            Vtl::Vtl1 => unimplemented!(),
            Vtl::Vtl2 => self.vplc2.unwrap(),
        }
    }

    pub(crate) fn current_vplc(&self) -> &'a Vplc {
        self.vplc(self.state.active_vtl)
    }

    pub(crate) fn current_whp(&self) -> whp::Processor<'a> {
        self.vp.whp(self.state.active_vtl)
    }

    pub(crate) fn intercept_state(&self) -> Option<&Vtl2InterceptState> {
        self.vp
            .partition
            .vtl2_emulation
            .as_ref()
            .map(|emu| &emu.intercepts)
    }

    pub(crate) fn set_vtl_runnable(&mut self, vtl: Vtl, reason: HvVtlEntryReason) {
        if self.state.runnable_vtls.is_set(vtl) {
            assert_eq!(reason, HvVtlEntryReason::INTERRUPT);
            return;
        }
        self.state.runnable_vtls.set(vtl);
        // The processor is implicitly unhalted when a new VTL becomes runnable.
        self.state.halted = false;
        if vtl == Vtl::Vtl2 {
            self.write_vtl2_entry_reason(reason);
        }
    }

    /// Switch to a new VTL for this VP.
    pub(crate) fn switch_vtl(&mut self, new_vtl: Vtl) {
        // Update active VTL, reflect shared registers from the old vtl to the new vtl.
        let old_vtl = std::mem::replace(&mut self.state.active_vtl, new_vtl);
        if old_vtl != new_vtl {
            tracing::trace!(?old_vtl, ?new_vtl, "switching vtl");

            let mut regs =
                [whp::abi::WHV_REGISTER_VALUE::default(); Self::VTL_SHARED_REGISTERS.len()];
            self.vp
                .whp(old_vtl)
                .get_registers(Self::VTL_SHARED_REGISTERS, &mut regs)
                .expect("should not fail");
            self.vp
                .whp(new_vtl)
                .set_registers(Self::VTL_SHARED_REGISTERS, &regs)
                .expect("should not fail");

            self.state.halted = false;

            if new_vtl == Vtl::Vtl2 {
                // No need to schedule any wakeups until the next VTL0 entry.
                self.state
                    .vtl2_wakeup_vmtime
                    .as_mut()
                    .unwrap()
                    .cancel_timeout();
            }
        }
    }

    fn write_vtl2_entry_reason(&self, reason: HvVtlEntryReason) {
        // Update the assist page with the switch reason.
        if let Some(base_gpa) = self.state.vtls[Vtl::Vtl2].vp_assist_page() {
            match self.vp.partition.gm.write_plain(
                base_gpa
                    + offset_of!(hvdef::HvVpAssistPage, vtl_control) as u64
                    + offset_of!(hvdef::HvVpVtlControl, entry_reason) as u64,
                &reason,
            ) {
                Ok(()) => {}
                Err(err) => {
                    tracing::error!(
                        error = err.as_error(),
                        base_gpa,
                        "failed to update assist page"
                    );
                }
            }
        }
    }

    fn vtl2_intercept_inner(
        &mut self,
        typ: HvMessageType,
        payload: &[u8],
    ) -> Result<(), GuestMemoryError> {
        let vsm_config = self
            .vp
            .partition
            .vtl2_emulation
            .as_ref()
            .unwrap()
            .vsm_config();
        if vsm_config.intercept_page() {
            // The guest has opted into having the intercept written to the VP assist page.
            if let Some(base_gpa) = self.state.vtls[Vtl::Vtl2].vp_assist_page() {
                self.vp.partition.gm.write_at(
                    base_gpa + offset_of!(hvdef::HvVpAssistPage, intercept_message) as u64,
                    HvMessage::new(typ, 0, payload).as_bytes(),
                )?;

                self.set_vtl_runnable(Vtl::Vtl2, HvVtlEntryReason::INTERCEPT);
            } else {
                tracing::error!("invalid configuration: no vp assist page");
            }
        } else {
            self.vp
                .partition
                .post_message(Vtl::Vtl2, self.vp.index, 0, typ, payload.as_bytes());
        }
        Ok(())
    }

    #[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
    pub(crate) fn vtl2_intercept(&mut self, typ: HvMessageType, payload: &[u8]) {
        match self.vtl2_intercept_inner(typ, payload) {
            Ok(()) => {}
            Err(err) => {
                tracing::error!(error = err.as_error(), "failed to update assist page");
            }
        }
    }

    /// Posts a message to the message page immediately, failing if the message
    /// slot is in use.
    pub(crate) fn post_message(
        &mut self,
        vtl: Vtl,
        sint: u8,
        message: &HvMessage,
    ) -> Result<(), HvError> {
        if let Some(hv) = &mut self.state.vtls[vtl].hv {
            hv.synic.post_message(
                sint,
                message,
                &mut self.vp.partition.synic_interrupt(self.vp.index, vtl),
            )
        } else {
            self.vp
                .whp(vtl)
                .post_synic_message(sint, message.as_bytes())
                .map_err(|err| {
                    err.hv_result()
                        .map_or(HvError::InvalidParameter, HvError::from)
                })
        }
    }

    fn flush_messages(&mut self, vtl: Vtl, deliverable_sints: u16) {
        let nonempty_sints = self
            .vplc(vtl)
            .message_queues
            .post_pending_messages(deliverable_sints, |sint, message| {
                self.post_message(vtl, sint, message)
            });

        if nonempty_sints != 0 {
            self.request_sint_notifications(vtl, nonempty_sints);
        }
    }

    pub(crate) async fn run_vp(
        &mut self,
        stop: StopVp<'_>,
        dev: &impl CpuIo,
    ) -> Result<Infallible, VpHaltReason> {
        self.reset_if_requested();

        tracing::trace!(vtl = ?self.state.active_vtl, "current vtl");
        let mut last_waker = None;
        loop {
            self.inner.interrupt.maybe_yield().await;
            poll_fn(|cx| {
                stop.check()?;

                // Ensure the waker is set.
                if !last_waker
                    .as_ref()
                    .is_some_and(|waker| cx.waker().will_wake(waker))
                {
                    last_waker = Some(cx.waker().clone());
                    self.inner.waker.write().clone_from(&last_waker);
                }

                // Cancel any pending timeout, which will be recomputed by the
                // following.
                self.state.vmtime.cancel_timeout();

                if self.state.enabled_vtls.is_clear(Vtl::Vtl2)
                    && self.inner.vtl2_enable.load(Ordering::SeqCst)
                {
                    tracing::debug!("enabled vtl2");
                    self.state.enabled_vtls.set(Vtl::Vtl2);
                    self.set_vtl_runnable(Vtl::Vtl2, HvVtlEntryReason::INTERRUPT);
                    // VTL2 "owns" startup suspend now, so clear the suspension state of VTL0.
                    #[cfg(guest_arch = "x86_64")]
                    if !matches!(self.vp.partition.hvstate, super::Hv1State::Disabled) {
                        if let Some(lapic) = self.state.vtls.lapic(self.state.active_vtl) {
                            lapic.startup_suspend = false;
                        } else {
                            self.current_whp()
                                .set_register(whp::Register64::InternalActivityState, 0)
                                .unwrap();
                        }
                    }
                }

                // Check if we need to make VTL2 runnable for forward progress.
                // These steps are only necessary if using the hypervisor APIC,
                // since otherwise we know exactly when to make VTL2 runnable.
                #[cfg(guest_arch = "x86_64")]
                if self.state.enabled_vtls.is_set(Vtl::Vtl2)
                    && self.state.vtls.lapic(Vtl::Vtl2).is_none()
                {
                    if self.state.runnable_vtls.is_clear(Vtl::Vtl2)
                        && self.inner.vtl2_wake.load(Ordering::SeqCst)
                    {
                        self.inner.vtl2_wake.store(false, Ordering::SeqCst);
                        self.set_vtl_runnable(Vtl::Vtl2, HvVtlEntryReason::INTERRUPT);
                    }

                    // If running VTL0, switch back to VTL2 after a short time to
                    // check for pending interrupts. This is necessary because only
                    // the hypervisor knows that interrupts are pending.
                    let vmtime = self.state.vtl2_wakeup_vmtime.as_mut().unwrap();
                    if self.state.runnable_vtls.is_clear(Vtl::Vtl2) {
                        const VTL2_TIMER_PERIOD: std::time::Duration =
                            std::time::Duration::from_millis(50);

                        vmtime.set_timeout_if_before(vmtime.now().wrapping_add(VTL2_TIMER_PERIOD));
                        if vmtime.poll_timeout(cx).is_ready() {
                            self.set_vtl_runnable(Vtl::Vtl2, HvVtlEntryReason::INTERRUPT);
                        }
                    }
                }

                // Scan each enabled VTL's message queues, synic timers, and for start VP requests.
                for vtl in self.state.enabled_vtls.clone().iter_highest_first() {
                    if self.state.runnable_vtls.is_higher_vtl_set_than(vtl) {
                        break;
                    }

                    if self.state.vtls[vtl].hv.is_some() {
                        let mut interrupt = self.vp.partition.synic_interrupt(self.vp.index, vtl);
                        loop {
                            let hv = self.state.vtls[vtl].hv.as_mut().unwrap();
                            let ref_time_now = hv.ref_time_now();
                            let (ready_sints, next_ref_time) =
                                hv.synic.scan(ref_time_now, &mut interrupt);
                            if let Some(next_ref_time) = next_ref_time {
                                // Convert from reference timer basis to vmtime basis via
                                // difference of programmed timer and current reference time.
                                const NUM_100NS_IN_SEC: u64 = 10 * 1000 * 1000;
                                let ref_diff = next_ref_time.saturating_sub(ref_time_now);
                                let ref_duration = std::time::Duration::new(
                                    ref_diff / NUM_100NS_IN_SEC,
                                    (ref_diff % NUM_100NS_IN_SEC) as u32 * 100,
                                );
                                let timeout = self.state.vmtime.now().wrapping_add(ref_duration);
                                self.state.vmtime.set_timeout_if_before(timeout);
                            }
                            if ready_sints == 0 {
                                break;
                            }
                            self.sints_deliverable(vtl, ready_sints);
                        }
                    }

                    let vplc = self.vplc(vtl);
                    if vplc.start_vp.load(Ordering::Relaxed) {
                        vplc.start_vp.store(false, Ordering::SeqCst);
                        let context = vplc.start_vp_context.lock().take();
                        if let Some(context) = context {
                            self.start_vp(vtl, context);
                        }
                    }
                }

                // Process the user-mode APIC, waiting for interrupts if halted.
                let ready = self.process_apic(dev);

                // Arm the timer.
                if self.state.vmtime.poll_timeout(cx).is_ready() {
                    // The timer has already expired. Yield once to allow other
                    // tasks to run.
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }

                if ready {
                    <Result<_, VpHaltReason>>::Ok(()).into()
                } else {
                    Poll::Pending
                }
            })
            .await?;

            let next_vtl = self
                .state
                .runnable_vtls
                .highest_set()
                .expect("no runnable vtls");

            if next_vtl != self.state.active_vtl {
                self.switch_vtl(next_vtl);
            }

            if self.current_vplc().check_queues.load(Ordering::Relaxed) {
                self.current_vplc()
                    .check_queues
                    .store(false, Ordering::SeqCst);

                self.flush_messages(self.state.active_vtl, !0);
            }

            // Set the lazy EOI bit just before running.
            let lazy_eoi = self.sync_lazy_eoi();

            let mut runner = self.current_whp().runner();
            let exit = runner
                .run()
                .map_err(|err| dev.fatal_error(WhpRunVpError(err).into()))?;

            // Clear lazy EOI before processing the exit.
            if lazy_eoi {
                self.clear_lazy_eoi(dev);
            }

            // Process the actual exit.
            self.handle_exit(dev, exit).await?;
        }
    }

    #[cfg(guest_arch = "x86_64")]
    fn handle_triple_fault(&mut self) -> Result<(), VpHaltReason> {
        let reinject_into_vtl2 = self
            .vp
            .partition
            .vtl2_emulation
            .as_ref()
            .map(|vtl2| vtl2.vsm_config().intercept_unrecoverable_exception())
            .unwrap_or(false)
            && self.state.active_vtl != Vtl::Vtl2;

        // If VTL2 is enabled and has requested triple faults to be forwarded, re-inject.
        if reinject_into_vtl2 {
            self.vtl2_intercept(
                HvMessageType::HvMessageTypeUnrecoverableException,
                hvdef::HvX64UnrecoverableExceptionMessage {
                    header: self.new_intercept_header(0, hvdef::HvInterceptAccessType::EXECUTE),
                }
                .as_bytes(),
            );

            // Return Ok to continue running VTL2. VTL0 is non-runnable at this point.
            return Ok(());
        }

        Err(VpHaltReason::TripleFault {
            vtl: self.state.active_vtl,
        })
    }

    fn sints_deliverable(&mut self, vtl: Vtl, sints: u16) {
        #[cfg(guest_arch = "x86_64")]
        if self.intercept_state().is_some() {
            if vtl == Vtl::Vtl0 && self.state.vtl2_deliverability_notifications.sints() & sints != 0
            {
                tracing::trace!(sints, "handling VTL2 intercept synic sint deliverable");
                let message;
                {
                    let requested_notifications = &mut self.state.vtl2_deliverability_notifications;
                    let deliverable_sints = sints & requested_notifications.sints();
                    requested_notifications
                        .set_sints(requested_notifications.sints() & !deliverable_sints);
                    message = hvdef::HvX64SynicSintDeliverableMessage {
                        header: self.new_intercept_header(0, hvdef::HvInterceptAccessType::EXECUTE),
                        deliverable_sints,
                        rsvd1: 0,
                        rsvd2: 0,
                    };
                }
                self.vtl2_intercept(
                    HvMessageType::HvMessageTypeSynicSintDeliverable,
                    message.as_bytes(),
                );
            }
        }

        let notifications =
            &mut self.state.vtls[self.state.active_vtl].deliverability_notifications;

        notifications.set_sints(notifications.sints() & !sints);
        self.flush_messages(vtl, sints);
    }

    /// Flushes pending register changes.
    pub(crate) fn reset_if_requested(&mut self) {
        if self.inner.reset_next.swap(false, Ordering::SeqCst) {
            self.state.reset(false, self.inner.vp_info.base.is_bsp());
        }

        if self.inner.scrub_next.swap(false, Ordering::SeqCst) {
            self.state.reset(true, self.inner.vp_info.base.is_bsp());
        }

        if self.state.finish_reset_vtl0 {
            self.state.finish_reset_vtl0 = false;
            self.finish_reset(Vtl::Vtl0);
        }

        if self.state.finish_reset_vtl2 {
            self.state.finish_reset_vtl2 = false;
            self.finish_reset(Vtl::Vtl2);
        }
    }

    fn finish_reset(&mut self, vtl: Vtl) {
        self.finish_reset_arch(vtl);
        *self.vplc(vtl).start_vp_context.lock() = None;
        if cfg!(debug_assertions) {
            let vp_info = &self.inner.vp_info;
            self.access_state(vtl).check_reset_all(vp_info);
        }
    }

    fn request_sint_notifications(&mut self, vtl: Vtl, sints: u16) {
        let notifications = self.state.vtls[vtl].deliverability_notifications;
        self.update_deliverability_notifications(
            vtl,
            notifications.with_sints(notifications.sints() | sints),
        );
    }

    pub(crate) fn update_deliverability_notifications(
        &mut self,
        vtl: Vtl,
        mut notifications: HvDeliverabilityNotificationsRegister,
    ) {
        let vtl_state = &mut self.state.vtls[vtl];
        let active_notifications = &mut vtl_state.deliverability_notifications;
        if vtl == Vtl::Vtl0 {
            notifications = HvDeliverabilityNotificationsRegister::from(
                u64::from(notifications) | u64::from(self.state.vtl2_deliverability_notifications),
            );
        }

        // Send the sint notifications to the hv emulator, if there is one.
        let mask = if let Some(hv) = &mut vtl_state.hv {
            hv.synic.request_sint_readiness(notifications.sints());
            !u64::from(HvDeliverabilityNotificationsRegister::new().with_sints(!0))
        } else {
            !0
        };

        if u64::from(notifications) & mask != u64::from(*active_notifications) & mask {
            tracing::trace!(?notifications, ?vtl, "setting notifications");
            self.vp
                .whp(vtl)
                .set_register(
                    whp::Register64::DeliverabilityNotifications,
                    u64::from(notifications) & mask,
                )
                .expect("requesting deliverability is not a fallable operation");
        }

        *active_notifications = notifications;
    }
}

#[cfg(guest_arch = "x86_64")]
mod x86 {
    use crate::Hv1State;
    use crate::WhpProcessor;
    use crate::emu;
    use crate::emu::WhpVpRefEmulation;
    use crate::memory::x86::GpaBackingType;
    use crate::vtl2;
    use hvdef::HvCacheType;
    use hvdef::HvInterceptAccessType;
    use hvdef::HvMessageType;
    use hvdef::HvVtlEntryReason;
    use hvdef::HvX64VpExecutionState;
    use hvdef::Vtl;
    use hvdef::hypercall::InitialVpContextX64;
    use thiserror::Error;
    use virt::LateMapVtl0MemoryPolicy;
    use virt::VpHaltReason;
    use virt::io::CpuIo;
    use virt::state::StateElement;
    use virt::x86::MsrError;
    use virt::x86::MsrErrorExt;
    use whp::get_registers;
    use whp::set_registers;
    use x86defs::X86X_MSR_APIC_BASE;
    use x86defs::apic::X2APIC_MSR_BASE;
    use x86defs::apic::X2APIC_MSR_END;
    use x86defs::cpuid::CpuidFunction;
    use zerocopy::FromZeros;
    use zerocopy::IntoBytes;

    #[derive(Debug, Error)]
    #[error("vp state is invalid")]
    struct InvalidVpState;

    #[derive(Debug, Error)]
    #[error("accessing deferred ram by VTL 2")]
    struct DeferredRamAccess;

    // HACK: on certain machines, Windows booting from the PCAT BIOS spams these
    // MSRs during boot.
    //
    // As a workaround, avoid injecting a GFP on these mystery MSRs until we can get
    // to the bottom of what's going on here.
    const MYSTERY_MSRS: &[u32] = &[0x88, 0x89, 0x8a, 0x116, 0x118, 0x119, 0x11a, 0x11b, 0x11e];

    impl WhpProcessor<'_> {
        pub(super) async fn handle_exit(
            &mut self,
            dev: &impl CpuIo,
            exit: whp::Exit<'_>,
        ) -> Result<(), VpHaltReason> {
            use whp::ExitReason;

            let stat = match exit.reason {
                ExitReason::IoPortAccess(info) => {
                    self.handle_io_port(dev, info, exit).await?;
                    &mut self.state.exits.io
                }
                ExitReason::Cpuid(info) => {
                    self.handle_cpuid(info, exit);
                    &mut self.state.exits.cpuid
                }
                ExitReason::ApicEoi(info) => {
                    self.handle_apic_eoi(info, dev);
                    &mut self.state.exits.apic_eoi
                }
                ExitReason::MsrAccess(info) => {
                    self.handle_msr(dev, info, exit);
                    &mut self.state.exits.msr
                }
                ExitReason::InterruptWindow(info) => {
                    self.handle_interrupt_window(info);
                    &mut self.state.exits.interrupt_window
                }
                ExitReason::Hypercall(info) => {
                    crate::hypercalls::WhpHypercallExit::handle(self, dev, info, exit.vp_context);
                    &mut self.state.exits.hypercall
                }
                ExitReason::MemoryAccess(access) => {
                    self.handle_memory_access(dev, access, exit).await?;
                    &mut self.state.exits.memory
                }
                ExitReason::SynicSintDeliverable(ctx) => {
                    self.handle_sint_deliverable(ctx);
                    &mut self.state.exits.sint_deliverable
                }
                ExitReason::Canceled => &mut self.state.exits.cancel,
                ExitReason::UnrecoverableException => {
                    self.handle_triple_fault()?;
                    &mut self.state.exits.other
                }
                ExitReason::InvalidVpRegisterValue => {
                    return Err(dev.fatal_error(InvalidVpState.into()));
                }
                ExitReason::Halt => {
                    self.handle_halt(exit);
                    &mut self.state.exits.halt
                }
                ExitReason::Exception(info) => {
                    self.handle_exception(dev, info, exit);
                    &mut self.state.exits.exception
                }
                _ => {
                    unreachable!("unsupported exit reason: {:?}", exit);
                }
            };
            stat.increment();
            Ok(())
        }

        /// Create a new hypervisor intercept message header used to inject VTL0 exit events into
        /// VTL2.
        pub(crate) fn new_intercept_header(
            &self,
            instruction_length: u8,
            intercept_access_type: HvInterceptAccessType,
        ) -> hvdef::HvX64InterceptMessageHeader {
            let (cs, ss, rip, rflags, cr0, efer, pending_interruption, interrupt_state) =
                whp::get_registers!(
                    self.vp.whp(Vtl::Vtl0),
                    [
                        whp::RegisterSegment::Cs,
                        whp::RegisterSegment::Ss,
                        whp::Register64::Rip,
                        whp::Register64::Rflags,
                        whp::Register64::Cr0,
                        whp::Register64::Efer,
                        whp::Register64::PendingInterruption,
                        whp::Register64::InterruptState,
                    ]
                )
                .unwrap();

            let pending_interruption =
                hvdef::HvX64PendingInterruptionRegister::from(pending_interruption);

            let interrupt_state = hvdef::HvX64InterruptStateRegister::from(interrupt_state);

            let execution_state = HvX64VpExecutionState::new()
                .with_cpl(
                    x86defs::SegmentAttributes::from(ss.Attributes).descriptor_privilege_level(),
                )
                .with_cr0_pe(cr0 & x86defs::X64_CR0_PE != 0)
                .with_cr0_am(cr0 & x86defs::X64_CR0_AM != 0)
                .with_efer_lma(efer & x86defs::X64_EFER_LMA != 0)
                .with_interruption_pending(pending_interruption.interruption_pending())
                .with_interrupt_shadow(interrupt_state.interrupt_shadow());

            hvdef::HvX64InterceptMessageHeader {
                vp_index: self.vp.index.index(),
                instruction_length_and_cr8: instruction_length,
                intercept_access_type,
                execution_state,
                cs_segment: from_whp_seg(cs),
                rip,
                rflags,
            }
        }

        fn handle_sint_deliverable(&mut self, ctx: &whp::abi::WHV_SYNIC_SINT_DELIVERABLE_CONTEXT) {
            tracing::trace!(sints = ctx.DeliverableSints, "sints deliverable");
            self.sints_deliverable(self.state.active_vtl, ctx.DeliverableSints);
        }

        fn handle_halt(&mut self, exit: whp::Exit<'_>) {
            if self.state.active_vtl == Vtl::Vtl0 && self.state.vtls[Vtl::Vtl0].lapic.is_none() {
                self.vtl2_intercept(
                    HvMessageType::HvMessageTypeX64Halt,
                    hvdef::HvX64HaltMessage {
                        header: self.new_intercept_header(
                            exit.vp_context.InstructionLength(),
                            HvInterceptAccessType::EXECUTE,
                        ),
                    }
                    .as_bytes(),
                );

                return;
            }

            self.state.halted = true;
        }

        async fn handle_memory_access(
            &mut self,
            dev: &impl CpuIo,
            access: &whp::abi::WHV_MEMORY_ACCESS_CONTEXT,
            exit: whp::Exit<'_>,
        ) -> Result<(), VpHaltReason> {
            let backing_type = self
                .vp
                .partition
                .gpa_backing_type(self.state.active_vtl, access.Gpa);

            // A GPA could be backed by ram, but marked as deny access by a
            // higher VTL. Don't bother trying to populate the range in that
            // case, as to WHP the GPA is unmapped in order to forward the
            // access to a higher VTL.
            let should_populate = match backing_type {
                GpaBackingType::MonitorPage => false,
                GpaBackingType::Ram { writable } => {
                    if access.AccessInfo.AccessType() == whp::abi::WHvMemoryAccessWrite {
                        writable
                    } else {
                        true
                    }
                }
                GpaBackingType::Unmapped
                | GpaBackingType::VtlProtected(_)
                | GpaBackingType::Unaccepted => false,
            };

            if !access.AccessInfo.GpaUnmapped() && should_populate {
                // This is a mapped GPA that wasn't mapped in the SLAT. Tell the
                // kernel to populate the SLAT.
                match self.current_vtlp().whp.populate_ranges(
                    &[whp::abi::WHV_MEMORY_RANGE_ENTRY {
                        GuestAddress: access.Gpa,
                        SizeInBytes: 1,
                    }],
                    access.AccessInfo.AccessType(),
                    Default::default(),
                ) {
                    Ok(()) => {
                        // Fault resolved locally.
                        return Ok(());
                    }
                    Err(err) => {
                        tracing::warn!(
                            gpa = access.Gpa,
                            access = ?access.AccessInfo.AccessType(),
                            error = &err as &dyn std::error::Error,
                            "failed to resolve gpa fault"
                        );
                        // Fall through and handle this via emulation.
                    }
                }
            }

            if self.intercept_state().is_some()
                && self.state.active_vtl == Vtl::Vtl0
                && !dev.is_mmio(access.Gpa)
                && self
                    .state
                    .vtls
                    .lapic(self.state.active_vtl)
                    .and_then(|lapic| lapic.apic.base_address())
                    .is_none_or(|base| access.Gpa & !0xfff != base)
            {
                let access_type = match access.AccessInfo.AccessType() {
                    whp::abi::WHvMemoryAccessRead => HvInterceptAccessType::READ,
                    whp::abi::WHvMemoryAccessWrite => HvInterceptAccessType::WRITE,
                    whp::abi::WHvMemoryAccessExecute => HvInterceptAccessType::EXECUTE,
                    _ => unreachable!(),
                };

                let cr8 = self
                    .vp
                    .whp(Vtl::Vtl0)
                    .get_register(whp::Register64::Cr8)
                    .expect("get register must succeed");
                let message = hvdef::HvX64MemoryInterceptMessage {
                    header: self.new_intercept_header(0, access_type),
                    cache_type: HvCacheType::HvCacheTypeWriteBack, // TODO: SNP sets this, unclear if matters
                    instruction_byte_count: access.InstructionByteCount,
                    memory_access_info: hvdef::HvX64MemoryAccessInfo::new()
                        .with_gva_valid(access.AccessInfo.GvaValid())
                        .with_gva_gpa_valid(false), // TODO fill in based on AccessInfo
                    // TODO: gvagpa valid? other fields?
                    tpr_priority: (cr8 & 0xF) as u8,
                    reserved: 0,
                    guest_virtual_address: access.Gva,
                    guest_physical_address: access.Gpa,
                    instruction_bytes: access.InstructionBytes,
                };

                // The intercept type is only unmapped if the access is unmapped and
                // the page is not VTL protected.
                let typ = match backing_type {
                    GpaBackingType::MonitorPage | GpaBackingType::Ram { .. } => {
                        panic!("unexpected vtl2 intercept {backing_type:?}")
                    }
                    GpaBackingType::Unmapped => {
                        assert!(!self.vp.partition.isolation.is_isolated());
                        HvMessageType::HvMessageTypeUnmappedGpa
                    }
                    GpaBackingType::VtlProtected(_) => HvMessageType::HvMessageTypeGpaIntercept,
                    GpaBackingType::Unaccepted => HvMessageType::HvMessageTypeUnacceptedGpa, // TODO: what are we supposed to fill out for intercept instruction_bytes for isolated?
                };

                // TODO: Earlier versions of Microsoft's HCL only looks at
                //       execution_state, instruction_bytes and instruction_bytes_count.
                //       OpenHCL mostly looks at the same?

                tracing::trace!(?typ, "inject vtl2 memory intercept");
                self.vtl2_intercept(typ, message.as_bytes());
            } else {
                if self.state.active_vtl == Vtl::Vtl2
                    && matches!(backing_type, GpaBackingType::Ram { writable: _ })
                    && self.current_vtlp().in_deferred_range(access.Gpa)
                {
                    let access_type = access.AccessInfo.AccessType();
                    let rip = exit.vp_context.Rip;
                    tracing::error!(
                        ?access,
                        access_type = access_type.to_string(),
                        rip,
                        "invalid access to deferred VTL0 ram by VTL2"
                    );

                    match self
                        .vp
                        .partition
                        .vtl2_emulation
                        .as_ref()
                        .expect("must be set")
                        .vtl0_deferred_policy
                    {
                        LateMapVtl0MemoryPolicy::Halt => {
                            return Err(dev.fatal_error(DeferredRamAccess.into()));
                        }
                        LateMapVtl0MemoryPolicy::Log => {}
                        LateMapVtl0MemoryPolicy::InjectException => {
                            // inject a GPF
                            let event = hvdef::HvX64PendingExceptionEvent::new()
                                .with_event_pending(true)
                                .with_event_type(hvdef::HV_X64_PENDING_EVENT_EXCEPTION)
                                .with_deliver_error_code(true)
                                .with_vector(0xd);

                            self.current_whp()
                                .set_register(whp::Register128::PendingEvent, event.into())
                                .unwrap();

                            return Ok(());
                        }
                    }
                }

                if Some(access.Gpa & !(hvdef::HV_PAGE_SIZE - 1))
                    == self.vp.partition.monitor_page.gpa()
                    && access.AccessInfo.AccessType() == whp::abi::WHvMemoryAccessWrite
                {
                    let guest_memory = &self.vp.partition.gm;
                    let interruption_pending = exit.vp_context.ExecutionState.InterruptionPending();
                    let gva_valid = access.AccessInfo.GvaValid();
                    let access = &WhpVpRefEmulation::MemoryAccessContext(access);
                    let mut state = emu::WhpEmulationState::new(access, self, &exit, dev);
                    if let Some(bit) = virt_support_x86emu::emulate::emulate_mnf_write_fast_path(
                        &mut state,
                        guest_memory,
                        dev,
                        interruption_pending,
                        gva_valid,
                    ) {
                        if let Some(connection_id) = self.vp.partition.monitor_page.write_bit(bit) {
                            self.signal_mnf(dev, connection_id);
                        }
                        return Ok(());
                    }
                }

                self.emulate(&WhpVpRefEmulation::MemoryAccessContext(access), dev, &exit)
                    .await?;
            }
            Ok(())
        }

        pub(crate) fn signal_mnf(&self, dev: &impl CpuIo, connection_id: u32) {
            if let Err(err) = dev.signal_synic_event(self.state.active_vtl, connection_id, 0) {
                tracing::warn!(
                    error = &err as &dyn std::error::Error,
                    connection_id,
                    "failed to signal mnf"
                );
            }
        }

        fn handle_interrupt_window(
            &mut self,
            info: &whp::abi::WHV_X64_INTERRUPTION_DELIVERABLE_CONTEXT,
        ) {
            if self.state.enabled_vtls.is_set(Vtl::Vtl2) && self.state.active_vtl == Vtl::Vtl0 {
                let notifications = &mut self.state.vtl2_deliverability_notifications;
                let inject = if notifications.interrupt_notification()
                    && info.DeliverableType == whp::abi::WHvX64PendingInterrupt
                {
                    notifications.set_interrupt_notification(false);
                    notifications.set_interrupt_priority(0);
                    true
                } else if notifications.nmi_notification()
                    && info.DeliverableType == whp::abi::WHvX64PendingNmi
                {
                    notifications.set_nmi_notification(false);
                    true
                } else {
                    false
                };

                if inject {
                    let message = hvdef::HvX64InterruptionDeliverableMessage {
                        header: self.new_intercept_header(0, HvInterceptAccessType::EXECUTE),
                        deliverable_type: hvdef::HvX64PendingInterruptionType(
                            info.DeliverableType.0 as u8,
                        ),
                        ..FromZeros::new_zeroed()
                    };

                    self.vtl2_intercept(
                        HvMessageType::HvMessageTypeX64InterruptionDeliverable,
                        message.as_bytes(),
                    );

                    // If VTL2 wanted this type of notification, then the host did not.
                    return;
                }
            }

            let notifications =
                &mut self.state.vtls[self.state.active_vtl].deliverability_notifications;
            notifications.set_interrupt_notification(false);
            notifications.set_nmi_notification(false);
            notifications.set_interrupt_priority(0);
        }

        fn handle_apic_eoi(&mut self, info: &whp::abi::WHV_X64_APIC_EOI_CONTEXT, dev: &impl CpuIo) {
            if let Some(intercept_state) = self.intercept_state() {
                if intercept_state.contains(vtl2::InterceptType::Eoi)
                    && self.state.active_vtl == Vtl::Vtl0
                {
                    let message = hvdef::HvX64ApicEoiMessage {
                        vp_index: self.vp.index.index(),
                        interrupt_vector: info.InterruptVector,
                    };
                    tracing::trace!("inject vtl2 eoi intercept");
                    self.vtl2_intercept(HvMessageType::HvMessageTypeX64ApicEoi, message.as_bytes());

                    return;
                }
            }
            dev.handle_eoi(info.InterruptVector);
        }

        async fn handle_io_port(
            &mut self,
            dev: &impl CpuIo,
            info: &whp::abi::WHV_X64_IO_PORT_ACCESS_CONTEXT,
            exit: whp::Exit<'_>,
        ) -> Result<(), VpHaltReason> {
            // Before handling, check if we should dispatch the exit to VTL2.
            if let Some(intercept_state) = self.intercept_state() {
                if self.state.active_vtl == Vtl::Vtl0
                    && intercept_state.contains(vtl2::InterceptType::IoPort(info.PortNumber))
                {
                    tracing::trace!(port = info.PortNumber, "inject vtl2 io intercept");

                    let message = hvdef::HvX64IoPortInterceptMessage {
                        header: self.new_intercept_header(
                            exit.vp_context.InstructionLength(),
                            if info.AccessInfo.IsWrite() {
                                HvInterceptAccessType::WRITE
                            } else {
                                HvInterceptAccessType::READ
                            },
                        ),
                        port_number: info.PortNumber,
                        access_info: hvdef::HvX64IoPortAccessInfo::new(
                            info.AccessInfo.AccessSize(),
                            info.AccessInfo.StringOp(),
                            info.AccessInfo.RepPrefix(),
                        ),
                        instruction_byte_count: info.InstructionByteCount,
                        reserved: 0,
                        rax: info.Rax,
                        instruction_bytes: info.InstructionBytes,
                        ds_segment: from_whp_seg(info.Ds),
                        es_segment: from_whp_seg(info.Es),
                        rcx: info.Rcx,
                        rsi: info.Rsi,
                        rdi: info.Rdi,
                    };

                    self.vtl2_intercept(
                        HvMessageType::HvMessageTypeX64IoPortIntercept,
                        message.as_bytes(),
                    );

                    return Ok(());
                }
            }

            if info.AccessInfo.StringOp() || info.AccessInfo.RepPrefix() {
                self.emulate(&WhpVpRefEmulation::IoPortAccessContext(info), dev, &exit)
                    .await?;
            } else {
                let mut rax = info.Rax;
                virt_support_x86emu::emulate::emulate_io(
                    self.vp.index,
                    info.AccessInfo.IsWrite(),
                    info.PortNumber,
                    &mut rax,
                    info.AccessInfo.AccessSize(),
                    dev,
                )
                .await;

                let rip = exit
                    .vp_context
                    .Rip
                    .wrapping_add(exit.vp_context.InstructionLength() as u64);

                set_registers!(
                    self.current_whp(),
                    [(whp::Register64::Rax, rax), (whp::Register64::Rip, rip),]
                )
                .unwrap();
            }
            Ok(())
        }

        fn handle_cpuid(
            &mut self,
            info: &whp::abi::WHV_X64_CPUID_ACCESS_CONTEXT,
            exit: whp::Exit<'_>,
        ) {
            let function = info.Rax as u32;
            let index = info.Rcx as u32;
            let default = [
                info.DefaultResultRax as u32,
                info.DefaultResultRbx as u32,
                info.DefaultResultRcx as u32,
                info.DefaultResultRdx as u32,
            ];

            let mut default = self.vp.partition.cpuid.result(function, index, &default);

            match CpuidFunction(function) {
                // The hypervisor does not consistently set this.
                CpuidFunction::ExtendedTopologyEnumeration
                | CpuidFunction::V2ExtendedTopologyEnumeration => {
                    default[3] = self.inner.vp_info.apic_id;
                }
                CpuidFunction(n) if matches!(n, 0x40000000..=0x400000ff) => {
                    match n {
                        hvdef::HV_CPUID_FUNCTION_MS_HV_FEATURES => {
                            match self.vp.partition.isolation {
                                virt::IsolationType::None => {}
                                virt::IsolationType::Vbs => {
                                    // Advertise this partition is VBS isolated.
                                    default[1] |= (u64::from(
                                        hvdef::HvPartitionPrivilege::new().with_isolation(true),
                                    ) >> 32)
                                        as u32;
                                }
                                ty => {
                                    unimplemented!("isolation type unsupported: {ty:?}")
                                }
                            }
                        }
                        hvdef::HV_CPUID_FUNCTION_MS_HV_ISOLATION_CONFIGURATION => {
                            match self.vp.partition.isolation {
                                virt::IsolationType::None => {}
                                virt::IsolationType::Vbs => {
                                    // Eax report paravisor present if VTL2 enabled.
                                    // TODO: Should Underhill be handling this?
                                    if self.vp.partition.vtl2.is_some()
                                        && self.state.active_vtl != Vtl::Vtl2
                                    {
                                        default[0] |= 1;
                                    }

                                    // Ebx report vbs isolated type.
                                    default[1] |= 1;
                                }
                                ty => {
                                    unimplemented!("isolation type unsupported: {ty:?}")
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }

            let [eax, ebx, ecx, edx] = default;

            if self.vp.partition.vtl2_emulation.is_some() && self.state.active_vtl == Vtl::Vtl0 {
                // Forward "unknown" cpuid entries. The hypervisor doesn't tell us
                // what is known or not, so just put what we need here.
                let forward = match function {
                    // Hyper-V virt stack.
                    0x4000_0080..=0x4fff_ffff => true,
                    // Hyper-V with emulation in VTL2.
                    0x4000_0000..=0x4fff_ffff
                        if matches!(self.vp.partition.hvstate, Hv1State::Disabled) =>
                    {
                        true
                    }
                    _ => false,
                };
                if forward {
                    let message = hvdef::HvX64CpuidInterceptMessage {
                        header: self.new_intercept_header(
                            exit.vp_context.InstructionLength(),
                            HvInterceptAccessType::WRITE,
                        ),
                        rax: info.Rax,
                        rcx: info.Rcx,
                        rdx: info.Rdx,
                        rbx: info.Rbx,
                        default_result_rax: eax.into(),
                        default_result_rcx: ecx.into(),
                        default_result_rdx: edx.into(),
                        default_result_rbx: ebx.into(),
                    };

                    self.vtl2_intercept(
                        HvMessageType::HvMessageTypeX64CpuidIntercept,
                        message.as_bytes(),
                    );
                    return;
                }
            }

            let rip = exit.vp_context.Rip.wrapping_add(2);
            set_registers!(
                self.current_whp(),
                [
                    (whp::Register64::Rax, eax.into()),
                    (whp::Register64::Rbx, ebx.into()),
                    (whp::Register64::Rcx, ecx.into()),
                    (whp::Register64::Rdx, edx.into()),
                    (whp::Register64::Rip, rip),
                ]
            )
            .unwrap();
        }

        fn send_unknown_msrs_to_vtl2(&self) -> bool {
            if let Some(intercept_state) = self.intercept_state() {
                intercept_state.contains(vtl2::InterceptType::Msr)
                    && self.state.active_vtl == Vtl::Vtl0
            } else {
                false
            }
        }

        fn send_msr_to_vtl2(
            &mut self,
            header: hvdef::HvX64InterceptMessageHeader,
            msr_number: u32,
            rdx: u64,
            rax: u64,
        ) {
            let message = hvdef::HvX64MsrInterceptMessage {
                header,
                msr_number,
                reserved: 0,
                rdx,
                rax,
            };

            tracing::trace!("inject vtl2 msr intercept");
            self.vtl2_intercept(HvMessageType::HvMessageTypeMsrIntercept, message.as_bytes());
        }

        fn handle_msr(
            &mut self,
            dev: &impl CpuIo,
            info: &whp::abi::WHV_X64_MSR_ACCESS_CONTEXT,
            exit: whp::Exit<'_>,
        ) {
            let handled = if info.AccessInfo.IsWrite() {
                self.msr_write(dev, exit, info.MsrNumber, info.Rax, info.Rdx)
            } else {
                self.msr_read(dev, exit, info.MsrNumber)
            };
            if !handled {
                // inject a GPF
                let event = hvdef::HvX64PendingExceptionEvent::new()
                    .with_event_pending(true)
                    .with_event_type(hvdef::HV_X64_PENDING_EVENT_EXCEPTION)
                    .with_deliver_error_code(true)
                    .with_vector(0xd);

                self.current_whp()
                    .set_register(whp::Register128::PendingEvent, event.into())
                    .unwrap();
            }
        }

        fn msr_write(
            &mut self,
            dev: &impl CpuIo,
            exit: whp::Exit<'_>,
            msr: u32,
            rax: u64,
            rdx: u64,
        ) -> bool {
            let v = rax & 0xffffffff | rdx << 32;
            let r = self
                .apic_msr_write(dev, msr, v)
                .or_else_if_unknown(|| match msr {
                    hvdef::HV_X64_MSR_GUEST_CRASH_P0..=hvdef::HV_X64_MSR_GUEST_CRASH_CTL
                        if !self.send_unknown_msrs_to_vtl2() =>
                    {
                        tracing::warn!(msr, v, "Guest signaled crash register");
                        match msr {
                            hvdef::HV_X64_MSR_GUEST_CRASH_P3 => {
                                self.state.crash_msg_address = Some(v)
                            }
                            hvdef::HV_X64_MSR_GUEST_CRASH_P4 => {
                                self.state.crash_msg_len =
                                    Some(std::cmp::min(v as usize, hvdef::HV_PAGE_SIZE_USIZE))
                            }
                            hvdef::HV_X64_MSR_GUEST_CRASH_CTL => {
                                if let (Some(addr), Some(len)) = (
                                    self.state.crash_msg_address.take(),
                                    self.state.crash_msg_len.take(),
                                ) {
                                    let mut bytes = vec![0u8; len];
                                    match self.vp.partition.gm.read_at(addr, bytes.as_mut_slice()) {
                                        Ok(()) => {
                                            let txt = String::from_utf8_lossy(&bytes);
                                            tracelimit::warn_ratelimited!(
                                                vtl = ?self.state.active_vtl,
                                                txt = txt.as_ref(),
                                                "guest reported crash"
                                            );
                                        }
                                        Err(err) => {
                                            tracelimit::error_ratelimited!(
                                                vtl = ?self.state.active_vtl,
                                                addr,
                                                error = &err as &dyn std::error::Error,
                                                "failed to read crash message"
                                            );
                                        }
                                    }
                                } else {
                                    tracing::warn!(
                                        vtl = ?self.state.active_vtl,
                                        "guest reported crash but did not provide message"
                                    );
                                }
                            }
                            _ => {}
                        }
                        Ok(())
                    }
                    0x40000000..=0x4fffffff => {
                        if let Some(hv) = &mut self.state.vtls[self.state.active_vtl].hv {
                            hv.msr_write(
                                msr,
                                v,
                                &mut crate::WhpNoVtlProtections(&self.vp.partition.gm),
                            )
                        } else {
                            match msr {
                                hvdef::HV_X64_MSR_VP_ASSIST_PAGE
                                    if self.state.active_vtl == Vtl::Vtl2 =>
                                {
                                    self.state.vtls[self.state.active_vtl].vp_assist_page = v;
                                    Ok(())
                                }
                                _ => Err(MsrError::Unknown),
                            }
                        }
                    }
                    msr @ (X86X_MSR_APIC_BASE | X2APIC_MSR_BASE..=X2APIC_MSR_END) => {
                        self.apic_msr_write(dev, msr, v)
                    }
                    x86defs::X86X_AMD_MSR_NB_CFG
                        if self.vp.partition.caps.vendor.is_amd_compatible() =>
                    {
                        Ok(())
                    }
                    // see comment on MYSTERY_MSRS for details
                    msr if MYSTERY_MSRS.contains(&msr) => {
                        tracelimit::warn_ratelimited!(?msr, "stubbed out mystery MSR write");
                        Ok(())
                    }
                    _ => Err(MsrError::Unknown),
                });

            if let Err(MsrError::Unknown) = r {
                if self.send_unknown_msrs_to_vtl2() {
                    self.send_msr_to_vtl2(
                        self.new_intercept_header(
                            exit.vp_context.InstructionLength(),
                            HvInterceptAccessType::WRITE,
                        ),
                        msr,
                        rdx,
                        rax,
                    );
                    return true;
                }
            }

            let gpf = match r {
                Ok(()) => false,
                Err(err) => {
                    tracelimit::warn_ratelimited!(
                        rip = exit.vp_context.Rip,
                        msr,
                        rax,
                        rdx,
                        ?err,
                        "invalid msr write"
                    );
                    true
                }
            };

            if !gpf {
                let rip = exit.vp_context.Rip.wrapping_add(2);
                self.current_whp()
                    .set_register(whp::Register64::Rip, rip)
                    .unwrap();
            }

            !gpf
        }

        fn msr_read(&mut self, dev: &impl CpuIo, exit: whp::Exit<'_>, msr: u32) -> bool {
            let r = self
                .apic_msr_read(dev, msr)
                .or_else_if_unknown(|| match msr {
                    x86defs::X86X_IA32_MSR_PLATFORM_ID => {
                        // Windows requires accessing this to boot. WHP
                        // used to pass this through to the hardware,
                        // but this regressed. Zero seems to work fine
                        // for Windows.
                        //
                        // TODO: Pass through the host value if it can
                        //       be retrieved.
                        Ok(0)
                    }
                    x86defs::X86X_MSR_EBL_CR_POWERON => Ok(0),
                    0x40000000..=0x4fffffff => {
                        if let Some(hv) = &mut self.state.vtls[self.state.active_vtl].hv {
                            hv.msr_read(msr)
                        } else {
                            match msr {
                                hvdef::HV_X64_MSR_VP_ASSIST_PAGE
                                    if self.state.active_vtl == Vtl::Vtl2 =>
                                {
                                    Ok(self.state.vtls[self.state.active_vtl].vp_assist_page)
                                }
                                _ => Err(MsrError::Unknown),
                            }
                        }
                    }
                    msr @ (X86X_MSR_APIC_BASE | X2APIC_MSR_BASE..=X2APIC_MSR_END) => {
                        self.apic_msr_read(dev, msr)
                    }
                    x86defs::X86X_AMD_MSR_PERF_EVT_SEL0..=x86defs::X86X_AMD_MSR_PERF_CTR3
                    | x86defs::X86X_AMD_MSR_SYSCFG
                    | x86defs::X86X_AMD_MSR_HW_CFG
                    | x86defs::X86X_AMD_MSR_NB_CFG
                    | x86defs::X86X_AMD_MSR_OSVW_ID_LENGTH..=x86defs::X86X_AMD_MSR_OSVW_ID_STATUS
                        if self.vp.partition.caps.vendor.is_amd_compatible() =>
                    {
                        Ok(0)
                    }
                    // see comment on MYSTERY_MSRS for details
                    msr if MYSTERY_MSRS.contains(&msr) => {
                        tracelimit::warn_ratelimited!(?msr, "stubbed out mystery MSR read");
                        Ok(0)
                    }
                    _ => Err(MsrError::Unknown),
                });

            if let Err(MsrError::Unknown) = r {
                if self.send_unknown_msrs_to_vtl2() {
                    self.send_msr_to_vtl2(
                        self.new_intercept_header(
                            exit.vp_context.InstructionLength(),
                            HvInterceptAccessType::READ,
                        ),
                        msr,
                        0,
                        0,
                    );
                    return true;
                }
            }

            let v = match r {
                Ok(v) => Some(v),
                Err(err) => {
                    tracing::warn!(rip = exit.vp_context.Rip, msr, ?err, "invalid msr read");
                    None
                }
            };

            if let Some(v) = v {
                let rax = v & 0xffffffff;
                let rdx = v >> 32;
                let rip = exit.vp_context.Rip.wrapping_add(2);

                set_registers!(
                    &self.current_whp(),
                    [
                        (whp::Register64::Rax, rax),
                        (whp::Register64::Rdx, rdx),
                        (whp::Register64::Rip, rip),
                    ]
                )
                .unwrap();
            }

            v.is_some()
        }

        /// Handles exception exits, which are only used to handle emulating
        /// instructions for which the hypervisor doesn't provide appropriate exits.
        fn handle_exception(
            &mut self,
            dev: &impl CpuIo,
            info: &whp::abi::WHV_VP_EXCEPTION_CONTEXT,
            exit: whp::Exit<'_>,
        ) {
            if !info.ExceptionInfo.SoftwareException()
                && info.ExceptionType.0 == x86defs::Exception::GENERAL_PROTECTION_FAULT.0
            {
                match info.InstructionBytes[..info.InstructionByteCount as usize] {
                    [0x0f, 0x30, ..] => {
                        // wrmsr
                        let (rcx, rax, rdx) = get_registers!(
                            self.current_whp(),
                            [
                                whp::Register64::Rcx,
                                whp::Register64::Rax,
                                whp::Register64::Rdx
                            ]
                        )
                        .unwrap();

                        if self.msr_write(dev, exit, rcx as u32, rax, rdx) {
                            return;
                        }
                    }
                    [0x0f, 0x32, ..] => {
                        // rdmsr
                        let rcx = self
                            .current_whp()
                            .get_register(whp::Register64::Rcx)
                            .unwrap();

                        if self.msr_read(dev, exit, rcx as u32) {
                            return;
                        }
                    }
                    _ => {}
                }
            }

            let event = if info.ExceptionInfo.SoftwareException() {
                todo!()
            } else {
                hvdef::HvX64PendingExceptionEvent::new()
                    .with_event_pending(true)
                    .with_event_type(hvdef::HV_X64_PENDING_EVENT_EXCEPTION)
                    .with_deliver_error_code(info.ExceptionInfo.ErrorCodeValid())
                    .with_exception_parameter(info.ExceptionParameter)
                    .with_error_code(info.ErrorCode)
                    .with_vector(info.ExceptionType.0.into())
                    .with_vector(0xd)
                    .into()
            };

            self.current_whp()
                .set_register(whp::Register128::PendingEvent, event)
                .unwrap();
        }

        /// Emulates an instruction due to a memory access exit.
        async fn emulate(
            &mut self,
            access: &WhpVpRefEmulation<'_>,
            dev: &impl CpuIo,
            exit: &whp::Exit<'_>,
        ) -> Result<(), VpHaltReason> {
            let vp = self.vp;
            let mut state = emu::WhpEmulationState::new(access, self, exit, dev);
            let emu_mem = virt_support_x86emu::emulate::EmulatorMemoryAccess {
                gm: &vp.partition.gm,
                kx_gm: &vp.partition.gm,
                ux_gm: &vp.partition.gm,
            };
            virt_support_x86emu::emulate::emulate(&mut state, &emu_mem, dev).await
        }

        pub(super) fn finish_reset_arch(&mut self, vtl: Vtl) {
            // The hypervisor fails to set CS to the architectural value, and
            // fails to clear TSC to 0.
            set_registers!(
                self.vp.whp(vtl),
                [
                    (
                        whp::RegisterSegment::Cs,
                        whp::abi::WHV_X64_SEGMENT_REGISTER {
                            Base: 0xffff0000,
                            Limit: 0xffff,
                            Selector: 0xf000,
                            Attributes: 0x9b,
                        }
                    ),
                    (whp::Register64::Tsc, 0),
                ],
            )
            .unwrap();

            let vp_info = self.inner.vp_info;
            if self.vp.partition.caps.x2apic_enabled {
                // Enable x2apic.
                let apic_base =
                    virt::x86::vp::Apic::at_reset(&self.vp.partition.caps, &vp_info).apic_base;

                self.vp
                    .whp(vtl)
                    .set_register(whp::Register64::ApicBase, apic_base)
                    .unwrap();

                if let Some(lapic) = &mut self.state.vtls.lapic(vtl) {
                    lapic.apic.set_apic_base(apic_base).unwrap();
                }
            }

            if self.state.vtls.lapic(vtl).is_none() {
                // Set the APIC ID. The hypervisor resets this back to the VP index
                // sometimes, so do this every reset.
                if vp_info.apic_id != vp_info.base.vp_index.index() {
                    self.vp
                        .whp(vtl)
                        .set_register(whp::Register64::ApicId, vp_info.apic_id.into())
                        .unwrap();
                }
            }
        }

        pub(super) fn start_vp(&mut self, vtl: Vtl, vp_context: Box<InitialVpContextX64>) {
            // Synchronize the register state.
            self.switch_vtl(vtl);

            tracing::debug!(vp_index = self.vp.index.index(), ?vtl, "starting vp");

            match hv1_emulator::hypercall::set_x86_vp_context(
                &mut self.access_state(vtl),
                &vp_context,
            ) {
                Ok(()) => {
                    self.set_vtl_runnable(vtl, HvVtlEntryReason::INTERRUPT);
                }
                Err(err) => {
                    tracelimit::warn_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        vp_index = self.vp.index.index(),
                        ?vtl,
                        "failed to start VP"
                    );
                }
            }
        }

        /// The list of shared registers between VTLs, as specified here:
        ///
        /// https://docs.microsoft.com/en-us/virtualization/hyper-v-on-windows/tlfs/vsm#virtual-processor-state-isolation
        ///
        /// TODO: Not not the full list, but cribbed from what previous Microsoft HCL versions used.
        #[cfg(guest_arch = "x86_64")]
        pub(super) const VTL_SHARED_REGISTERS: &'static [whp::abi::WHV_REGISTER_NAME] = &[
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrDefType as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrPhysBase0 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrPhysBase1 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrPhysBase2 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrPhysBase3 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrPhysBase4 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrPhysBase5 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrPhysBase6 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrPhysBase7 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrPhysMask0 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrPhysMask1 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrPhysMask2 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrPhysMask3 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrPhysMask4 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrPhysMask5 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrPhysMask6 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrPhysMask7 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrFix64k00000 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrFix16k80000 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrFix16kA0000 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrFix4kC0000 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrFix4kC8000 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrFix4kD0000 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrFix4kD8000 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrFix4kE0000 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrFix4kE8000 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrFix4kF0000 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::MsrMtrrFix4kF8000 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::Rax as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::Rbx as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::Rcx as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::Rdx as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::Rsi as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::Rdi as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::Rbp as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::R8 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::R9 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::R10 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::R11 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::R12 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::R13 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::R14 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register64::R15 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register128::Xmm0 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register128::Xmm1 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register128::Xmm2 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register128::Xmm3 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register128::Xmm4 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register128::Xmm5 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register128::Xmm6 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register128::Xmm7 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register128::Xmm8 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register128::Xmm9 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register128::Xmm10 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register128::Xmm11 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register128::Xmm12 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register128::Xmm13 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register128::Xmm14 as u32),
            whp::abi::WHV_REGISTER_NAME(whp::Register128::Xmm15 as u32),
        ];
    }

    fn from_whp_seg(reg: whp::abi::WHV_X64_SEGMENT_REGISTER) -> hvdef::HvX64SegmentRegister {
        hvdef::HvX64SegmentRegister {
            base: reg.Base,
            limit: reg.Limit,
            selector: reg.Selector,
            attributes: reg.Attributes,
        }
    }
}

#[cfg(guest_arch = "aarch64")]
mod aarch64 {
    use crate::InitialVpContext;
    use crate::WhpProcessor;
    use aarch64defs::EsrEl2;
    use aarch64defs::ExceptionClass;
    use aarch64defs::IssDataAbort;
    use hvdef::HvMessageType;
    use hvdef::Vtl;
    use virt::VpHaltReason;
    use virt::io::CpuIo;

    fn message_ref<T: hvdef::MessagePayload>(v: &whp::abi::WHV_RUN_VP_EXIT_CONTEXT_u) -> &T {
        T::ref_from_prefix(&v.message).unwrap().0
    }

    impl WhpProcessor<'_> {
        pub(super) fn process_apic(&mut self, _dev: &impl CpuIo) -> bool {
            true
        }

        pub(super) fn sync_lazy_eoi(&mut self) -> bool {
            false
        }

        pub(super) fn clear_lazy_eoi(&mut self, _dev: &impl CpuIo) {
            unreachable!()
        }

        pub(crate) fn flush_apic(&mut self, _vtl: Vtl) {}

        fn get_x(&self, n: u8) -> u64 {
            let mut value = [Default::default()];
            self.current_whp()
                .get_registers(
                    &[whp::abi::WHV_REGISTER_NAME(
                        whp::abi::WHvArm64RegisterX0.0 + n as u32,
                    )],
                    &mut value,
                )
                .unwrap();
            u128::from(value[0].0) as u64
        }

        fn set_x(&self, n: u8, v: u64) {
            let value = [whp::abi::WHV_REGISTER_VALUE(v.into())];
            self.current_whp()
                .set_registers(
                    &[whp::abi::WHV_REGISTER_NAME(
                        whp::abi::WHvArm64RegisterX0.0 + n as u32,
                    )],
                    &value,
                )
                .unwrap();
        }

        pub(super) async fn handle_exit(
            &mut self,
            dev: &impl CpuIo,
            exit: whp::Exit<'_>,
        ) -> Result<(), VpHaltReason> {
            use whp::ExitReason;

            let stat = match exit.reason {
                ExitReason::Canceled => &mut self.state.exits.cancel,
                ExitReason::None => unreachable!(),
                ExitReason::Hypervisor(reason, message) => match HvMessageType(reason) {
                    HvMessageType::HvMessageTypeUnmappedGpa
                    | HvMessageType::HvMessageTypeGpaIntercept => {
                        self.handle_memory_access(dev, message_ref(message), exit)
                            .await?;
                        &mut self.state.exits.memory
                    }
                    HvMessageType::HvMessageTypeSynicSintDeliverable => {
                        self.handle_sint_deliverable(message_ref(message));
                        &mut self.state.exits.sint_deliverable
                    }
                    HvMessageType::HvMessageTypeHypercallIntercept => {
                        crate::hypercalls::WhpHypercallExit::handle(
                            self,
                            dev,
                            message_ref(message),
                        );
                        &mut self.state.exits.hypercall
                    }
                    HvMessageType::HvMessageTypeArm64ResetIntercept => {
                        return Err(self.handle_reset(message_ref(message)));
                    }
                    _ => {
                        unreachable!("unsupported exit reason: {:?}", exit);
                    }
                },
            };
            stat.increment();
            Ok(())
        }

        #[cfg(guest_arch = "aarch64")]
        fn handle_sint_deliverable(&mut self, message: &hvdef::HvArm64SynicSintDeliverableMessage) {
            tracing::trace!(sints = message.deliverable_sints, "sints deliverable");
            self.sints_deliverable(self.state.active_vtl, message.deliverable_sints);
        }

        async fn handle_memory_access(
            &mut self,
            dev: &impl CpuIo,
            message: &hvdef::HvArm64MemoryInterceptMessage,
            exit: whp::Exit<'_>,
        ) -> Result<(), VpHaltReason> {
            let _ = (dev, message, exit);
            let syndrome = EsrEl2::from(message.syndrome);
            tracing::trace!(
                gpa = message.guest_physical_address,
                ?syndrome,
                "memory access"
            );
            match ExceptionClass(syndrome.ec()) {
                ExceptionClass::DATA_ABORT_LOWER => {
                    let iss = IssDataAbort::from(syndrome.iss());
                    if !iss.isv() {
                        return Err(dev.fatal_error(
                            anyhow::anyhow!("can't handle data abort without isv: {iss:?}").into(),
                        ));
                    }
                    let len = 1 << iss.sas();
                    let sign_extend = iss.sse();
                    let reg = iss.srt();
                    if iss.wnr() {
                        let data = self.get_x(reg).to_ne_bytes();
                        dev.write_mmio(self.vp.index, message.guest_physical_address, &data[..len])
                            .await;
                    } else {
                        let mut data = [0; 8];
                        dev.read_mmio(
                            self.vp.index,
                            message.guest_physical_address,
                            &mut data[..len],
                        )
                        .await;
                        let mut data = u64::from_ne_bytes(data);
                        if sign_extend {
                            let shift = 64 - len * 8;
                            data = ((data as i64) << shift >> shift) as u64;
                            if !iss.sf() {
                                data &= 0xffffffff;
                            }
                        }
                        self.set_x(reg, data);
                    }
                    let pc = message
                        .header
                        .pc
                        .wrapping_add(if syndrome.il() { 4 } else { 2 });
                    self.current_whp()
                        .set_register(whp::Register64::Pc, pc)
                        .unwrap();
                }
                ec => {
                    return Err(dev.fatal_error(
                        anyhow::anyhow!("unknown memory access exception: {ec:?}").into(),
                    ));
                }
            }
            Ok(())
        }

        /// Handle a reset from the hypervisor-handled PSCI call.
        fn handle_reset(&mut self, info: &hvdef::HvArm64ResetInterceptMessage) -> VpHaltReason {
            match info.reset_type {
                hvdef::HvArm64ResetType::POWER_OFF => VpHaltReason::PowerOff,
                hvdef::HvArm64ResetType::REBOOT => VpHaltReason::Reset,
                hvdef::HvArm64ResetType::HIBERNATE => VpHaltReason::Hibernate,
                hvdef::HvArm64ResetType::SYSTEM_RESET => {
                    // TODO: What values can it have?
                    tracing::debug!(reset_code = info.reset_code, "system reset");
                    VpHaltReason::Reset
                }
                ty => unreachable!("unknown reset type: {:#x?}, {:#x}", ty, info.reset_code),
            }
        }

        pub(super) fn finish_reset_arch(&mut self, vtl: Vtl) {
            let _ = vtl;
        }

        pub(super) fn start_vp(&mut self, vtl: Vtl, vp_context: Box<InitialVpContext>) {
            let _ = (vtl, vp_context);
            todo!("TODO-aarch64")
        }

        // TODO-aarch64
        pub(super) const VTL_SHARED_REGISTERS: &'static [whp::abi::WHV_REGISTER_NAME] = &[];
    }
}
