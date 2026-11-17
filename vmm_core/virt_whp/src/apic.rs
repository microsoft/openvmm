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
use virt_support_apic::ApicClient;
use virt_support_apic::ApicWork;
use virt_support_apic::LocalApic;
use virt_support_apic::LocalApicSet;
use virt_support_x86emu::translate::TranslateFlags;
use virt_support_x86emu::translate::TranslatePrivilegeCheck;
use virt_support_x86emu::translate::translate_gva_to_gpa;
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
                    self.vp(vp_index).unwrap().wake_for_apic(vtl);
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
                    self.vp(vp_index).unwrap().wake_for_apic(vtl);
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
    vtl: Vtl,
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
            vtl,
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
        self.partition.vp(vp_index).unwrap().wake_for_apic(self.vtl);
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
                    nmi_pending_count: None,
                    interrupt_shadow_kind: None,
                }
            }
            LocalApicKind::Offloaded => {
                // Get the NMI pending bit from the APIC.
                let mut activity: vp::Activity = self.vp.get_register_state(vtl)?;
                let apic = vp::ApicRegisters::from_page(
                    &self.vp.whp(vtl).get_apic().for_op("get apic state")?,
                );
                activity.nmi_pending = apic.hv_apic_nmi_pending();
                // The hypervisor can read back a suspend state that a restore
                // wrote as running (see `resuspend_restored_vp`), but the VP
                // is suspended as restored until it runs.
                if let Some(restored) = self.state.vtls[vtl].restored_suspend {
                    if activity.mp_state == vp::MpState::Running {
                        activity.mp_state = restored;
                    }
                }
                activity
            }
        };
        Ok(activity)
    }

    pub(crate) fn restore_activity(&mut self, vtl: Vtl, value: &vp::Activity) -> Result<(), Error> {
        // WHP holds one pending NMI, so it cannot restore a count of them.
        // Refuse it before changing any state.
        if value.nmi_pending_count.is_some() {
            return Err(Error::UnsupportedState("nmi_pending_count"));
        }
        // WHP's interrupt state has one bit for both kinds of interrupt
        // shadow, so it cannot restore a kind. Refuse it before changing any
        // state.
        if value.interrupt_shadow_kind.is_some() {
            return Err(Error::UnsupportedState("interrupt_shadow_kind"));
        }
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
                    nmi_pending_count: None,
                    interrupt_shadow_kind: None,
                };

                self.vp.set_register_state(vtl, &value)?;
            }
            LocalApicKind::Offloaded => {
                self.vp.set_register_state(vtl, value)?;
                // The hypervisor does not keep a suspend state that this
                // writes; see `resuspend_restored_vp`.
                self.state.vtls[vtl].restored_suspend = match value.mp_state {
                    vp::MpState::Halted | vp::MpState::Idle => Some(value.mp_state),
                    vp::MpState::Running | vp::MpState::WaitForSipi => None,
                };
                // Set the NMI pending bit via the APIC.
                let mut apic = vp::ApicRegisters::from_page(
                    &self.vp.whp(vtl).get_apic().for_op("get apic state")?,
                );
                if value.nmi_pending != apic.hv_apic_nmi_pending() {
                    apic.set_hv_apic_nmi_pending(value.nmi_pending);
                    self.vp
                        .whp(vtl)
                        .set_apic(&apic.as_page())
                        .for_op("set apic state")?;
                }
            }
        }
        Ok(())
    }

    /// Makes a VP that was restored suspended suspend itself again, just before
    /// it runs, by running the instruction that suspended it again.
    ///
    /// With the hypervisor's APIC, WHP keeps a suspend state that the VMM wrote
    /// only until the VMM writes more of the VP's state: the VMM's write of the
    /// APIC page can end a suspension that it set with the activity register or,
    /// for a VP that suspended itself, one that follows a write of its RFLAGS,
    /// and a suspension that it sets after the APIC page is dropped. So
    /// `restore_all`, which writes the registers, the activity, and then the
    /// APIC page, can leave a VP that runs on after its `hlt` (or its `rdmsr`
    /// of `HV_X64_MSR_GUEST_IDLE`) when it first runs, whether it restores into
    /// the VP that it was saved from or into a new partition's, although the
    /// activity still reads back as suspended. A guest that halts with its
    /// interrupts enabled gets a wake-up that it tolerates, but one that halts
    /// with them disabled never expects one: Linux halts an offlined processor
    /// so, and the processor would run on.
    ///
    /// The hypervisor keeps a VP suspended when the VP runs the suspending
    /// instruction itself after the VMM's last write of its state. So this puts
    /// RIP back onto that instruction, and lets the VP run it again, with the
    /// registers, the APIC state, and the pending interrupts that it was saved
    /// with. Once it has suspended, the VP is as it was saved.
    ///
    /// The instruction runs in an interrupt shadow, as one after an `sti` does,
    /// so that nothing is taken before the VP has suspended: an interrupt or an
    /// NMI taken first would return to the instruction, and the VP would
    /// suspend again with nothing left to wake it, where it would have resumed
    /// after the instruction. The restore of an LAPIC timer that expired during
    /// the downtime queues such an interrupt. The shadow ends with the
    /// instruction.
    ///
    /// Until the VP runs, the VMM reads the state that it restored:
    /// `save_activity` reports the restored suspend state, and RIP is not
    /// moved, since the VMM can still write the registers or the activity. A
    /// VP whose RIP does not follow the suspending instruction, as one that
    /// was not saved after it ran, is left as the restore wrote it. A stop that
    /// cancels the run before the VP runs the instruction leaves it as set up
    /// here, about to run it in the shadow, which a save reads as that, and a
    /// restore runs to the same result.
    pub(crate) fn resuspend_restored_vp(&mut self) -> Result<(), Error> {
        let vtl = self.state.active_vtl;
        let Some(mp_state) = self.state.vtls[vtl].restored_suspend.take() else {
            return Ok(());
        };
        let instruction: &[u8] = match mp_state {
            vp::MpState::Halted => &[0xf4],     // hlt
            vp::MpState::Idle => &[0x0f, 0x32], // rdmsr
            vp::MpState::Running | vp::MpState::WaitForSipi => return Ok(()),
        };

        let whp = self.vp.whp(vtl);
        let (cs, rip, efer, rcx, activity, interrupt_state, pending_interruption, pending_event) =
            get_registers!(
                whp,
                [
                    whp::RegisterSegment::Cs,
                    whp::Register64::Rip,
                    whp::Register64::Efer,
                    whp::Register64::Rcx,
                    whp::Register64::InternalActivityState,
                    whp::Register64::InterruptState,
                    whp::Register64::PendingInterruption,
                    whp::Register128::PendingEvent,
                ]
            )
            .for_op("get registers")?;

        // An INIT that the VP took since the restore makes it wait for a SIPI.
        if hvdef::HvInternalActivityRegister::from(activity).startup_suspend() {
            return Ok(());
        }

        // An interruption that the VP delivers before the instruction would
        // return to it. The VP runs on after the restore with one pending
        // anyway, so leave it.
        let interrupted = HvX64PendingInterruptionRegister::from(pending_interruption)
            .interruption_pending()
            || HvX64PendingEventReg0::from(pending_event).event_pending();
        // The VP idles in the `rdmsr` of the idle MSR, whose index it still holds.
        let wrong_msr = mp_state == vp::MpState::Idle && rcx as u32 != hvdef::HV_X64_MSR_GUEST_IDLE;
        let start = rip.checked_sub(instruction.len() as u64).filter(|&start| {
            let long_mode = efer & x86defs::X64_EFER_LMA != 0
                && x86defs::SegmentAttributes::from(cs.Attributes).long();
            !interrupted
                && !wrong_msr
                && self.is_instruction_at(vtl, long_mode, cs.Base, start, instruction)
        });
        let Some(start) = start else {
            tracelimit::warn_ratelimited!(
                ?mp_state,
                rip,
                "cannot suspend a restored vp again: its suspending instruction does not \
                 precede rip, or an interruption is pending; the hypervisor may run it on"
            );
            return Ok(());
        };

        tracing::debug!(?mp_state, rip, "suspending a restored vp again");
        let interrupt_state = hvdef::HvX64InterruptStateRegister::from(interrupt_state);
        set_registers!(
            whp,
            [
                (whp::Register64::InternalActivityState, 0),
                (whp::Register64::Rip, start),
                (
                    whp::Register64::InterruptState,
                    u64::from(interrupt_state.with_interrupt_shadow(true))
                ),
            ]
        )
        .for_op("set registers")?;
        Ok(())
    }

    /// Returns whether guest memory holds `instruction` at the offset `start`
    /// of the code segment whose base is `cs_base`, where the VP can execute it.
    fn is_instruction_at(
        &self,
        vtl: Vtl,
        long_mode: bool,
        cs_base: u64,
        start: u64,
        instruction: &[u8],
    ) -> bool {
        instruction.iter().zip(start..).all(|(&expected, offset)| {
            let gva = if long_mode {
                offset
            } else {
                cs_base.wrapping_add(offset) & 0xffff_ffff
            };
            let Some(gpa) = self.translate_for_execute(vtl, gva) else {
                return false;
            };
            let mut byte = [0];
            self.vp.partition.gm.read_at(gpa, &mut byte).is_ok() && byte[0] == expected
        })
    }

    /// Translates the linear address `gva` to the GPA that the VP executes from,
    /// whatever its privilege level, or returns `None` where it cannot.
    ///
    /// The walk of the page tables reads them from guest memory, which does not
    /// need the hypervisor to have the RAM that holds them registered, as a lazy
    /// restore leaves most of it until the VP first touches it. With nested
    /// virtualization, the hypervisor's nested paging state decides instead, as
    /// it does for the emulator.
    fn translate_for_execute(&self, vtl: Vtl, gva: u64) -> Option<u64> {
        if self.vp.partition.caps.nested_virt {
            self.translate_gva_via_hypervisor(
                vtl,
                gva,
                whp::abi::WHvTranslateGvaFlagValidateExecute
                    | whp::abi::WHvTranslateGvaFlagPrivilegeExempt,
            )
            .ok()
        } else {
            translate_gva_to_gpa(
                &self.vp.partition.gm,
                gva,
                &self.translation_registers(vtl),
                TranslateFlags {
                    validate_execute: true,
                    validate_read: false,
                    validate_write: false,
                    override_smap: false,
                    enforce_smap: false,
                    privilege_check: TranslatePrivilegeCheck::None,
                    set_page_table_bits: false,
                },
            )
            .ok()
            .map(|result| result.gpa)
        }
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
                let mut apic = vp::ApicRegisters::from_page(
                    &self.vp.whp(vtl).get_apic().for_op("get apic state")?,
                );
                // Clear the non-architectural NMI pending bit.
                apic.set_hv_apic_nmi_pending(false);
                vp::Apic::new(apic_base.into(), apic, [0; 8])
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
                let nmi_pending = vp::ApicRegisters::from_page(
                    &self.vp.whp(vtl).get_apic().for_op("get apic state")?,
                )
                .hv_apic_nmi_pending();
                let mut apic = *value.registers();
                apic.set_hv_apic_nmi_pending(nmi_pending);
                self.vp
                    .whp(vtl)
                    .set_apic(&apic.as_page())
                    .for_op("set apic state")?;
                self.assert_pending_interrupt(vtl, &apic)?;
                if vtl == Vtl::Vtl0 {
                    self.inner.lapic_written.store(true, Ordering::Release);
                }
            }
        }

        Ok(())
    }

    /// Asserts the highest-priority pending edge-triggered vector of an
    /// offloaded LAPIC state just written with `set_apic` through WHP's
    /// interrupt path.
    ///
    /// A snapshot restore writes saved IRR bits this way, and so does the
    /// restore clock when it queues the vector of a one-shot timer that
    /// expired during the downtime. The hypervisor does not always deliver
    /// such a vector to a halted VP: on a bare-metal host, 2 s after every legacy
    /// 8-VP restore, a VP was still halted with the timer vector pending, and
    /// its timers stalled until some other interrupt woke it. Asserting a
    /// vector that is already pending is idempotent, and wakes the VP when
    /// the vector is deliverable: the hypervisor still applies the PPR and IF
    /// rules.
    fn assert_pending_interrupt(&self, vtl: Vtl, apic: &vp::ApicRegisters) -> Result<(), Error> {
        let Some(vector) = highest_pending_edge_vector(apic) else {
            return Ok(());
        };
        self.vp
            .partition
            .vtlp(vtl)
            .whp
            .interrupt(
                whp::abi::WHvX64InterruptTypeFixed,
                whp::abi::WHvX64InterruptDestinationModePhysical,
                whp::abi::WHvX64InterruptTriggerModeEdge,
                self.inner.vp_info.apic_id,
                vector.into(),
            )
            .for_op("assert a pending LAPIC vector")?;
        Ok(())
    }

    /// Asserts the highest pending edge-triggered vector of the VP's current
    /// offloaded VTL0 LAPIC state again, if the VMM wrote that state since the
    /// VP last ran: partition time may have been suspended and resumed since
    /// the write (the legacy restore clock does both), so the VP runs only
    /// after the assertion.
    pub(crate) fn reassert_written_lapic(&mut self) -> Result<(), Error> {
        if !self.inner.lapic_written.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let apic = vp::ApicRegisters::from_page(
            &self.vp.whp(Vtl::Vtl0).get_apic().for_op("get apic state")?,
        );
        self.assert_pending_interrupt(Vtl::Vtl0, &apic)
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

            // Pair with `wake_for_apic` so the IRR staged before its release
            // publication is visible before scanning.
            let scan_irr = self.vplc(vtl).scan_irr.swap(false, Ordering::Acquire);
            let vtl_state = &mut self.state.vtls[vtl];
            if let Some(lapic) = &mut vtl_state.lapic {
                let work = lapic.apic.scan(&mut self.state.vmtime, scan_irr);
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
            self.state.vtls[vtl].restored_suspend = None;
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
            // Not ready. Register a notification.
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

            self.state.halted = false;
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
        let Self {
            apic,
            startup_suspend,
            nmi_pending,
        } = self;
        apic.reset();
        *startup_suspend = !is_bsp;
        *nmi_pending = false;
    }
}

/// Returns the highest-priority vector pending in `apic`'s IRR that is
/// edge-triggered (TMR clear). Vectors 0 to 15 are skipped: they are not valid
/// fixed interrupts, and the Hyper-V LAPIC state page reuses their IRR bits
/// (for example for the NMI pending bit). Level-triggered vectors are left to
/// their IOAPIC, whose EOI handling an edge assertion would bypass.
fn highest_pending_edge_vector(apic: &vp::ApicRegisters) -> Option<u8> {
    (16..=255u8).rev().find(|&vector| {
        let (word, bit) = (usize::from(vector / 32), vector % 32);
        apic.irr[word] >> bit & 1 == 1 && apic.tmr[word] >> bit & 1 == 0
    })
}

#[cfg(test)]
mod tests {
    use super::highest_pending_edge_vector;
    use virt::x86::vp::ApicRegisters;
    use zerocopy::FromZeros;

    fn pending(irr: &[u8], level: &[u8]) -> ApicRegisters {
        let mut apic = ApicRegisters::new_zeroed();
        for &vector in irr {
            apic.irr[usize::from(vector / 32)] |= 1 << (vector % 32);
        }
        for &vector in level {
            apic.tmr[usize::from(vector / 32)] |= 1 << (vector % 32);
        }
        apic
    }

    #[test]
    fn nothing_pending_asserts_nothing() {
        assert_eq!(highest_pending_edge_vector(&pending(&[], &[])), None);
    }

    #[test]
    fn the_expired_timer_vector_is_asserted() {
        // Linux's LOCAL_TIMER_VECTOR, as the restore clock queues it.
        assert_eq!(
            highest_pending_edge_vector(&pending(&[0xec], &[])),
            Some(0xec)
        );
    }

    #[test]
    fn the_highest_edge_vector_wins() {
        assert_eq!(
            highest_pending_edge_vector(&pending(&[0x30, 0xec, 0x41], &[])),
            Some(0xec)
        );
        assert_eq!(
            highest_pending_edge_vector(&pending(&[0xff, 0x10], &[])),
            Some(0xff)
        );
    }

    #[test]
    fn level_triggered_vectors_are_left_to_their_ioapic() {
        assert_eq!(
            highest_pending_edge_vector(&pending(&[0x41], &[0x41])),
            None
        );
        assert_eq!(
            highest_pending_edge_vector(&pending(&[0x41, 0x30], &[0x41])),
            Some(0x30)
        );
    }

    #[test]
    fn reserved_vectors_and_the_nmi_pending_bit_are_ignored() {
        let mut apic = pending(&[], &[]);
        apic.set_hv_apic_nmi_pending(true);
        apic.irr[0] |= 0xffff;
        assert_eq!(highest_pending_edge_vector(&apic), None);
    }
}
