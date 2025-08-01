// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! APIC handling.
//!
//! This module supports both the APIC running inside the hypervisor, and the
//! APIC running in the VMM.

#![cfg(guest_arch = "x86_64")]

use crate::Error;
use crate::LocalApicKind;
use crate::WhpPartitionInner;
use crate::WhpProcessor;
use crate::WhpResultExt;
use crate::WhpVpRef;
use hvdef::HvVtlEntryReason;
use hvdef::HvX64PendingEventReg0;
use hvdef::HvX64PendingInterruptionRegister;
use hvdef::Vtl;
use inspect::Inspect;
use std::sync::atomic::Ordering;
use virt::VpIndex;
use virt::io::CpuIo;
use virt::irqcon::MsiRequest;
use virt::x86::MsrError;
use virt::x86::vp;
use virt::x86::vp::hv_apic_nmi_pending;
use virt::x86::vp::set_hv_apic_nmi_pending;
use virt_support_apic::ApicClient;
use virt_support_apic::ApicWork;
use virt_support_apic::LocalApic;
use virt_support_apic::LocalApicSet;
use vmcore::vmtime::VmTime;
use vmcore::vmtime::VmTimeAccess;
use whp::get_registers;
use whp::set_registers;
use x86defs::RFlags;

impl WhpPartitionInner {
    pub fn lint(&self, vp_index: VpIndex, vtl: Vtl, index: usize) {
        let Some(vpref) = self.vp(vp_index) else {
            tracelimit::warn_ratelimited!(
                vp = vp_index.index(),
                ?vtl,
                index,
                "invalid vp index for lint"
            );
            return;
        };
        match &self.vtlp(vtl).lapic {
            LocalApicKind::Emulated(apic) => {
                apic.lint(vp_index, index, |vp_index| {
                    self.vp(vp_index)
                        .expect("apic emulator passes valid vp index")
                        .wake()
                });
            }
            LocalApicKind::Offloaded => {
                if index == 0 {
                    vpref.vplc(vtl).extint_pending.store(true, Ordering::SeqCst);
                    vpref.wake();
                } else {
                    self.vtlp(vtl)
                        .whp
                        .interrupt(
                            whp::abi::WHvX64InterruptTypeLocalInt1,
                            whp::abi::WHvX64InterruptDestinationModePhysical,
                            whp::abi::WHvX64InterruptTriggerModeEdge,
                            vp_index.index(),
                            0,
                        )
                        .unwrap();
                }
            }
        }
    }

    pub fn interrupt(&self, vtl: Vtl, request: MsiRequest) -> Result<(), whp::WHvError> {
        match &self.vtlp(vtl).lapic {
            LocalApicKind::Emulated(lapic) => {
                lapic.request_interrupt(request.address, request.data, |vp_index| {
                    self.vp(vp_index)
                        .expect("apic emulator passes valid vp index")
                        .wake()
                });
            }
            LocalApicKind::Offloaded => {
                let (address, data) = request.as_x86();
                let control = request.hv_x86_interrupt_control();

                // WHP interrupt type has the same format as mshv interrupt type.
                let interrupt_type = whp::abi::WHV_INTERRUPT_TYPE(control.interrupt_type().0);

                let dest_mode = if control.x86_logical_destination_mode() {
                    whp::abi::WHvX64InterruptDestinationModeLogical
                } else {
                    whp::abi::WHvX64InterruptDestinationModePhysical
                };
                let trigger_mode = if control.x86_level_triggered() {
                    whp::abi::WHvX64InterruptTriggerModeLevel
                } else {
                    whp::abi::WHvX64InterruptTriggerModeEdge
                };
                self.vtlp(vtl).whp.interrupt(
                    interrupt_type,
                    dest_mode,
                    trigger_mode,
                    address.virt_destination().into(),
                    data.vector().into(),
                )?;
                // Wake up the VTL.
                if !address.destination_mode_logical() {
                    if let Some(vp) = self.vp_by_apic_id(address.virt_destination().into()) {
                        vp.ensure_vtl_runnable(vtl);
                    }
                }
            }
        }

        Ok(())
    }
}

pub struct WhpApicClient<'a, T> {
    partition: &'a WhpPartitionInner,
    whp: whp::Processor<'a>,
    dev: &'a T,
    vmtime: &'a VmTimeAccess,
}

impl<'a> WhpVpRef<'a> {
    pub(crate) fn apic_client<T: CpuIo>(
        &self,
        vtl: Vtl,
        dev: &'a T,
        vmtime: &'a VmTimeAccess,
    ) -> WhpApicClient<'a, T> {
        WhpApicClient {
            partition: self.partition,
            whp: self.whp(vtl),
            dev,
            vmtime,
        }
    }
}

impl<T: CpuIo> ApicClient for WhpApicClient<'_, T> {
    fn cr8(&mut self) -> u32 {
        self.whp.get_register(whp::Register64::Cr8).unwrap() as u32
    }

    fn set_cr8(&mut self, value: u32) {
        self.whp
            .set_register(whp::Register64::Cr8, value.into())
            .unwrap();
    }

    fn set_apic_base(&mut self, value: u64) {
        self.whp
            .set_register(whp::Register64::ApicBase, value)
            .unwrap();
    }

    fn wake(&mut self, vp_index: VpIndex) {
        let vp = self
            .partition
            .vp(vp_index)
            .expect("apic emulator passes valid vp index")
            .vp();

        vp.scan_irr.store(true, Ordering::Relaxed);
        if let Some(waker) = &*vp.waker.read() {
            waker.wake_by_ref();
        }
    }

    fn eoi(&mut self, vector: u8) {
        self.dev.handle_eoi(vector.into());
    }

    fn now(&mut self) -> VmTime {
        self.vmtime.now()
    }

    fn pull_offload(&mut self) -> ([u32; 8], [u32; 8]) {
        unreachable!()
    }
}

impl WhpProcessor<'_> {
    pub(crate) fn save_activity(&mut self, vtl: Vtl) -> Result<vp::Activity, Error> {
        let activity = match self.vp.partition.vtlp(vtl).lapic {
            LocalApicKind::Emulated(_) => {
                let activity: vp::Activity = self.vp.get_register_state(vtl)?;
                let lapic = self.state.vtls[vtl].lapic.as_ref().unwrap();
                vp::Activity {
                    mp_state: if lapic.startup_suspend {
                        vp::MpState::WaitForSipi
                    } else if self.state.halted {
                        vp::MpState::Halted
                    } else {
                        vp::MpState::Running
                    },
                    nmi_pending: lapic.nmi_pending,
                    nmi_masked: activity.nmi_masked,
                    interrupt_shadow: activity.interrupt_shadow,
                    pending_event: activity.pending_event,
                    pending_interruption: activity.pending_interruption,
                }
            }
            LocalApicKind::Offloaded => {
                // Get the NMI pending bit from the APIC.
                let mut activity: vp::Activity = self.vp.get_register_state(vtl)?;
                let apic = self.vp.whp(vtl).get_apic().for_op("get apic state")?;
                activity.nmi_pending = hv_apic_nmi_pending(&apic);
                activity
            }
        };
        Ok(activity)
    }

    pub(crate) fn restore_activity(&mut self, vtl: Vtl, value: &vp::Activity) -> Result<(), Error> {
        match self.vp.partition.vtlp(vtl).lapic {
            LocalApicKind::Emulated(_) => {
                let lapic = self.state.vtls[vtl].lapic.as_mut().unwrap();
                let startup_suspend;
                let halted;
                match value.mp_state {
                    vp::MpState::Running => {
                        startup_suspend = false;
                        halted = false;
                    }
                    vp::MpState::WaitForSipi => {
                        startup_suspend = true;
                        halted = false;
                    }
                    vp::MpState::Halted => {
                        startup_suspend = false;
                        halted = true;
                    }
                    vp::MpState::Idle => unimplemented!(),
                }

                lapic.startup_suspend = startup_suspend;
                lapic.nmi_pending = value.nmi_pending;
                self.state.halted = halted;

                let value = vp::Activity {
                    mp_state: vp::MpState::Running,
                    nmi_pending: false,
                    nmi_masked: value.nmi_masked,
                    interrupt_shadow: value.interrupt_shadow,
                    pending_event: value.pending_event,
                    pending_interruption: value.pending_interruption,
                };

                self.vp.set_register_state(vtl, &value)?;
            }
            LocalApicKind::Offloaded => {
                self.vp.set_register_state(vtl, value)?;
                // Set the NMI pending bit via the APIC.
                let mut apic = self.vp.whp(vtl).get_apic().for_op("get apic state")?;
                set_hv_apic_nmi_pending(&mut apic, value.nmi_pending);
                self.vp.whp(vtl).set_apic(&apic).for_op("set apic state")?;
            }
        }
        Ok(())
    }

    pub(crate) fn save_apic(&mut self, vtl: Vtl) -> Result<vp::Apic, Error> {
        let apic_base = self
            .vp
            .whp(vtl)
            .get_register(whp::Register64::ApicBase)
            .for_op("get apic base")?;

        let apic = match self.vp.partition.vtlp(vtl).lapic {
            LocalApicKind::Emulated(_) => {
                let lapic = self.state.vtls[vtl].lapic.as_mut().unwrap();
                lapic.apic.save()
            }
            LocalApicKind::Offloaded => {
                let mut apic = self.vp.whp(vtl).get_apic().for_op("get apic state")?;
                // Clear the non-architectural NMI pending bit.
                set_hv_apic_nmi_pending(&mut apic, false);
                vp::Apic::from_page(apic_base, &apic[..1024].try_into().unwrap())
            }
        };

        Ok(apic)
    }

    pub(crate) fn restore_apic(&mut self, vtl: Vtl, value: &vp::Apic) -> Result<(), Error> {
        // Set this first to set the APIC mode before updating the APIC register
        // state.
        self.vp
            .whp(vtl)
            .set_register(whp::Register64::ApicBase, value.apic_base)
            .for_op("set apic base")?;

        match self.vp.partition.vtlp(vtl).lapic {
            LocalApicKind::Emulated(_) => {
                let lapic = self.state.vtls[vtl].lapic.as_mut().unwrap();
                lapic.apic.restore(value).map_err(Error::InvalidApicBase)?;
            }
            LocalApicKind::Offloaded => {
                // Preserve NMI pending.
                let mut apic = self.vp.whp(vtl).get_apic().for_op("get apic state")?;
                let nmi_pending = hv_apic_nmi_pending(&apic);
                apic[..1024].copy_from_slice(&value.as_page());
                set_hv_apic_nmi_pending(&mut apic, nmi_pending);
                self.vp.whp(vtl).set_apic(&apic).for_op("set apic state")?;
            }
        }

        Ok(())
    }
    pub(crate) fn apic_write(&mut self, address: u64, data: &[u8], dev: &impl CpuIo) {
        if let Some(lapic) = self.state.vtls.lapic(self.state.active_vtl) {
            lapic
                .apic
                .access(
                    &mut self
                        .vp
                        .apic_client(self.state.active_vtl, dev, &self.state.vmtime),
                )
                .mmio_write(address, data);
        }
    }

    pub(crate) fn apic_read(&mut self, address: u64, data: &mut [u8], dev: &impl CpuIo) {
        if let Some(lapic) = self.state.vtls.lapic(self.state.active_vtl) {
            lapic
                .apic
                .access(
                    &mut self
                        .vp
                        .apic_client(self.state.active_vtl, dev, &self.state.vmtime),
                )
                .mmio_read(address, data);
        } else {
            data.fill(!0);
        }
    }

    pub(crate) fn apic_msr_read(&mut self, dev: &impl CpuIo, msr: u32) -> Result<u64, MsrError> {
        self.state
            .vtls
            .lapic(self.state.active_vtl)
            .ok_or(MsrError::Unknown)?
            .apic
            .access(
                &mut self
                    .vp
                    .apic_client(self.state.active_vtl, dev, &self.state.vmtime),
            )
            .msr_read(msr)
    }

    pub(crate) fn apic_msr_write(
        &mut self,
        dev: &impl CpuIo,
        msr: u32,
        value: u64,
    ) -> Result<(), MsrError> {
        self.state
            .vtls
            .lapic(self.state.active_vtl)
            .ok_or(MsrError::Unknown)?
            .apic
            .access(
                &mut self
                    .vp
                    .apic_client(self.state.active_vtl, dev, &self.state.vmtime),
            )
            .msr_write(msr, value)
    }

    /// Flush pending APIC inputs to processor state.
    pub(crate) fn flush_apic(&mut self, vtl: Vtl) {
        if let Some(lapic) = self.state.vtls.lapic(vtl) {
            let work = lapic.apic.flush();
            lapic.nmi_pending |= work.nmi;

            assert!(work.interrupt.is_none());

            let previous_vtl = self.state.active_vtl;
            self.handle_non_interrupt_work(vtl, &work);
            self.switch_vtl(previous_vtl);
        }
    }

    fn handle_non_interrupt_work(&mut self, vtl: Vtl, work: &ApicWork) {
        if work.init {
            self.inject_init(vtl);
        }

        if let Some(vector) = work.sipi {
            self.inject_sipi(vtl, vector);
        }

        if work.extint {
            self.vplc(vtl).extint_pending.store(true, Ordering::Relaxed);
        }
    }

    /// Returns whether the VP should be run.
    pub(crate) fn process_apic(&mut self, dev: &impl CpuIo) -> bool {
        // Scan each enabled VTL, stopping at the highest runnable VTL.
        for vtl in self.state.enabled_vtls.clone().iter_highest_first() {
            if self.state.runnable_vtls.is_higher_vtl_set_than(vtl) {
                break;
            }

            let vtl_state = &mut self.state.vtls[vtl];
            if let Some(lapic) = &mut vtl_state.lapic {
                let work = lapic.apic.scan(
                    &mut self.state.vmtime,
                    self.inner.scan_irr.swap(false, Ordering::Relaxed),
                );
                lapic.nmi_pending |= work.nmi;
                if lapic.nmi_pending {
                    self.inject_nmi(vtl);
                }

                self.handle_non_interrupt_work(vtl, &work);
                if let Some(vector) = work.interrupt {
                    self.inject_interrupt(vtl, vector);
                }
            }

            if self.vplc(vtl).extint_pending.load(Ordering::Relaxed) {
                self.inject_extint(vtl, dev);
            }
        }

        // Ensure the runnable VTL is not effectively halted.
        let halted = self
            .state
            .vtls
            .lapic(self.state.runnable_vtls.highest_set().unwrap())
            .as_ref()
            .is_some_and(|lapic| self.state.halted || lapic.startup_suspend);

        !halted
    }

    #[must_use]
    pub(crate) fn sync_lazy_eoi(&mut self) -> bool {
        let vtl_state = &mut self.state.vtls[self.state.active_vtl];
        if let Some(hv) = &mut vtl_state.hv {
            if vtl_state.lapic.as_ref().unwrap().apic.is_lazy_eoi_pending() {
                return hv.set_lazy_eoi();
            }
        }
        false
    }

    pub(crate) fn clear_lazy_eoi(&mut self, dev: &impl CpuIo) {
        let vtl_state = &mut self.state.vtls[self.state.active_vtl];
        if vtl_state.hv.as_mut().unwrap().clear_lazy_eoi() {
            vtl_state
                .lapic
                .as_mut()
                .unwrap()
                .apic
                .access(
                    &mut self
                        .vp
                        .apic_client(self.state.active_vtl, dev, &self.state.vmtime),
                )
                .lazy_eoi();
        }
    }

    fn inject_init(&mut self, vtl: Vtl) {
        // Synchronize the register state.
        self.switch_vtl(vtl);

        let vp_info = self.inner.vp_info;
        let mut access = self.access_state(vtl);

        vp::x86_init(&mut access, &vp_info).unwrap();
        self.set_vtl_runnable(vtl, HvVtlEntryReason::INTERRUPT);
    }

    fn inject_sipi(&mut self, vtl: Vtl, vector: u8) {
        // Synchronize the register state.
        self.switch_vtl(vtl);

        let whp = self.vp.whp(vtl);
        let lapic = self.state.vtls.lapic(vtl).unwrap();
        if lapic.startup_suspend {
            let address = (vector as u64) << 12;
            let cs = whp::abi::WHV_X64_SEGMENT_REGISTER {
                Base: address,
                Limit: 0xffff,
                Selector: (address >> 4) as u16,
                Attributes: 0x9b,
            };
            set_registers!(
                whp,
                [(whp::RegisterSegment::Cs, cs), (whp::Register64::Rip, 0)]
            )
            .unwrap();
            lapic.startup_suspend = false;
            self.state.halted = false;
            self.set_vtl_runnable(vtl, HvVtlEntryReason::INTERRUPT)
        }
    }

    fn inject_extint(&mut self, vtl: Vtl, dev: &impl CpuIo) -> bool {
        self.set_vtl_runnable(vtl, HvVtlEntryReason::INTERRUPT);

        let whp = self.vp.whp(vtl);
        let (rflags, interrupt_state, pending_interruption, pending_event) = get_registers!(
            whp,
            [
                whp::Register64::Rflags,
                whp::Register64::InterruptState,
                whp::Register64::PendingInterruption,
                whp::Register128::PendingEvent,
            ]
        )
        .unwrap();

        let pending_interruption = HvX64PendingInterruptionRegister::from(pending_interruption);
        let interrupt_state = hvdef::HvX64InterruptStateRegister::from(interrupt_state);
        let pending_event = HvX64PendingEventReg0::from(pending_event);
        let rflags = RFlags::from(rflags);

        // Check if the processor is ready for an interrupt.
        if pending_interruption.interruption_pending()
            || interrupt_state.interrupt_shadow()
            || !rflags.interrupt_enable()
            || pending_event.event_pending()
        {
            // Not ready. Register a notification.
            let notifications = self.state.vtls[vtl].deliverability_notifications;
            if !notifications.interrupt_notification() || notifications.interrupt_priority() != 0 {
                self.update_deliverability_notifications(
                    vtl,
                    notifications
                        .with_interrupt_notification(true)
                        .with_interrupt_priority(0),
                );
            }

            self.state.halted = false;
            return false;
        }

        // Clear the pending bit before getting the vector from the PIC.
        self.vp
            .vplc(vtl)
            .extint_pending
            .store(false, Ordering::SeqCst);

        // Get the vector.
        let Some(vector) = dev.acknowledge_pic_interrupt() else {
            return true;
        };

        if self.state.vtls.lapic(vtl).is_some() {
            // Inject the interrupt as expected by the minapic.
            let event = HvX64PendingInterruptionRegister::new()
                .with_interruption_type(
                    hvdef::HvX64PendingInterruptionType::HV_X64_PENDING_INTERRUPT.0,
                )
                .with_interruption_vector(vector.into())
                .with_interruption_pending(true);

            whp.set_register(whp::Register64::PendingInterruption, event.into())
                .unwrap();

            self.state.halted = false;
            tracing::trace!(vector, "extint interrupted");
        } else {
            // Inject the interrupt as expected by the synic.
            //
            // TEMPORARY: force the processor out of the halted
            // state--otherwise, the hypervisor delays injecting the
            // interrupt.
            let event = hvdef::HvX64PendingExtIntEvent::new()
                .with_event_pending(true)
                .with_event_type(hvdef::HV_X64_PENDING_EVENT_EXT_INT)
                .with_vector(vector);

            set_registers!(
                whp,
                [
                    (whp::Register128::PendingEvent, event.into()),
                    (whp::Register64::InternalActivityState, 0)
                ]
            )
            .unwrap();
        }

        true
    }

    fn inject_nmi(&mut self, vtl: Vtl) {
        self.set_vtl_runnable(vtl, HvVtlEntryReason::INTERRUPT);

        let whp = self.vp.whp(vtl);
        let (interrupt_state, pending_interruption, pending_event) = get_registers!(
            whp,
            [
                whp::Register64::InterruptState,
                whp::Register64::PendingInterruption,
                whp::Register128::PendingEvent,
            ]
        )
        .unwrap();

        let interrupt_state = hvdef::HvX64InterruptStateRegister::from(interrupt_state);
        let pending_event = HvX64PendingEventReg0::from(pending_event);

        // Check if the processor is ready for an interrupt.
        if HvX64PendingInterruptionRegister::from(pending_interruption).interruption_pending()
            || interrupt_state.nmi_masked()
            || interrupt_state.interrupt_shadow()
            || pending_event.event_pending()
        {
            // Not ready. Register a notification.
            let notifications = self.state.vtls[vtl].deliverability_notifications;
            if !notifications.nmi_notification() {
                self.update_deliverability_notifications(
                    vtl,
                    notifications.with_nmi_notification(true),
                );
            }

            self.state.halted = false;
            return;
        }

        // Inject the interrupt as expected by the minapic.
        let event = HvX64PendingInterruptionRegister::new()
            .with_interruption_type(hvdef::HvX64PendingInterruptionType::HV_X64_PENDING_NMI.0)
            .with_interruption_vector(2)
            .with_interruption_pending(true);

        whp.set_register(whp::Register64::PendingInterruption, event.into())
            .unwrap();

        self.state.halted = false;
        self.state.vtls.lapic(vtl).unwrap().nmi_pending = false;

        tracing::trace!("nmi interrupted");
    }

    fn inject_interrupt(&mut self, vtl: Vtl, vector: u8) {
        self.set_vtl_runnable(vtl, HvVtlEntryReason::INTERRUPT);

        let whp = self.vp.whp(vtl);
        let (rflags, cr8, interrupt_state, pending_interruption, pending_event) = get_registers!(
            whp,
            [
                whp::Register64::Rflags,
                whp::Register64::Cr8,
                whp::Register64::InterruptState,
                whp::Register64::PendingInterruption,
                whp::Register128::PendingEvent,
            ]
        )
        .unwrap();

        let priority = vector >> 4;

        let pending_interruption = HvX64PendingInterruptionRegister::from(pending_interruption);
        let interrupt_state = hvdef::HvX64InterruptStateRegister::from(interrupt_state);
        let rflags = RFlags::from(rflags);
        let pending_event = HvX64PendingEventReg0::from(pending_event);
        if pending_interruption.interruption_pending()
            || interrupt_state.interrupt_shadow()
            || !rflags.interrupt_enable()
            || cr8 >= priority as u64
            || pending_event.event_pending()
        {
            let notifications = self.state.vtls[vtl].deliverability_notifications;
            if !notifications.interrupt_notification()
                || (notifications.interrupt_priority() != 0
                    && notifications.interrupt_priority() < priority)
            {
                self.update_deliverability_notifications(
                    vtl,
                    notifications
                        .with_interrupt_notification(true)
                        .with_interrupt_priority(priority),
                );
            }

            return;
        }

        let interruption = HvX64PendingInterruptionRegister::new()
            .with_interruption_type(hvdef::HvX64PendingInterruptionType::HV_X64_PENDING_INTERRUPT.0)
            .with_interruption_vector(vector.into())
            .with_interruption_pending(true);

        whp.set_register(whp::Register64::PendingInterruption, interruption.into())
            .unwrap();

        self.state.halted = false;

        tracing::trace!(vector, "interrupted");
        self.state
            .vtls
            .lapic(vtl)
            .unwrap()
            .apic
            .acknowledge_interrupt(vector);
    }
}

#[derive(Inspect)]
pub(crate) struct ApicState {
    #[inspect(flatten)]
    pub(crate) apic: LocalApic,
    pub(crate) startup_suspend: bool,
    nmi_pending: bool,
}

impl ApicState {
    pub fn new(table: &LocalApicSet, vp_info: &vm_topology::processor::x86::X86VpInfo) -> Self {
        Self {
            apic: table.add_apic(vp_info, false),
            startup_suspend: !vp_info.base.is_bsp(),
            nmi_pending: false,
        }
    }
}

impl ApicState {
    pub fn reset(&mut self, is_bsp: bool) {
        self.apic.reset();
        self.startup_suspend = !is_bsp;
    }
}
