// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::KvmProcessor;
use super::regs::register_to_msr;
use crate::KvmError;
use crate::KvmPartitionInner;
use hvdef::HvRegisterValue;
use hvdef::HvX64RegisterName;
use virt::VpIndex;
use virt::state::HvRegisterState;
use virt::x86::SegmentRegister;
use virt::x86::TableRegister;
use virt::x86::vp;
use virt::x86::vp::AccessVpState;
use x86defs::SegmentAttributes;
use zerocopy::FromZeros;

/// Per-VP state access for a bound [`KvmProcessor`].
///
/// This borrows the processor mutably so that synic save/restore can read and
/// re-establish the SIMP/SIEFP overlay pages (and the processor's tracked synic
/// register state) through the very same `OverlayPage` instances the run loop
/// maintains.
pub struct KvmVpStateAccess<'a, 'b> {
    vp: &'a mut KvmProcessor<'b>,
}

impl<'a, 'b> KvmVpStateAccess<'a, 'b> {
    pub(crate) fn new(vp: &'a mut KvmProcessor<'b>) -> Self {
        Self { vp }
    }
}

impl KvmPartitionInner {
    /// Returns a KVM vcpu handle for the given VP, for partition-level register
    /// access that does not require a bound [`KvmProcessor`].
    pub(crate) fn vp_kvm(&self, vp_index: VpIndex) -> kvm::Processor<'_> {
        self.kvm.vp(self.vp(vp_index).unwrap().vp_info.apic_id)
    }
}

/// Reads MSR-backed register state from a KVM vcpu.
pub(crate) fn get_msrs_state<T, const N: usize>(kvm: &kvm::Processor<'_>) -> Result<T, KvmError>
where
    T: HvRegisterState<HvX64RegisterName, N>,
{
    let mut msrs = T::default();
    let names = msrs.names().map(|name| register_to_msr(name).unwrap());
    let mut values = [0; N];
    kvm.get_msrs(&names, &mut values)?;
    msrs.set_values(values.into_iter().map(|v| v.into()));
    Ok(msrs)
}

/// Writes MSR-backed register state to a KVM vcpu.
pub(crate) fn set_msrs_state<T, const N: usize>(
    kvm: &kvm::Processor<'_>,
    value: &T,
) -> Result<(), KvmError>
where
    T: HvRegisterState<HvX64RegisterName, N>,
{
    let mut values = [HvRegisterValue::new_zeroed(); N];
    value.get_values(values.iter_mut());

    let msrs: Vec<_> = value
        .names()
        .map(|name| register_to_msr(name).unwrap())
        .into_iter()
        .zip(values.map(|v| v.as_u64()))
        .collect();

    kvm.set_msrs(&msrs)?;
    Ok(())
}

impl KvmVpStateAccess<'_, '_> {
    pub(crate) fn kvm(&self) -> kvm::Processor<'_> {
        self.vp.partition.kvm.vp(self.vp.inner.vp_info.apic_id)
    }

    fn set_register_state<T, const N: usize>(&self, value: &T) -> Result<(), KvmError>
    where
        T: HvRegisterState<HvX64RegisterName, N>,
    {
        set_msrs_state(&self.kvm(), value)
    }

    fn get_register_state<T, const N: usize>(&self) -> Result<T, KvmError>
    where
        T: HvRegisterState<HvX64RegisterName, N>,
    {
        get_msrs_state(&self.kvm())
    }
}

pub(crate) fn seg_reg(reg: SegmentRegister) -> kvm::kvm_segment {
    let attributes = SegmentAttributes::from(reg.attributes);
    kvm::kvm_segment {
        base: reg.base,
        limit: reg.limit,
        selector: reg.selector,

        type_: attributes.segment_type(),
        present: attributes.present() as u8,
        dpl: attributes.descriptor_privilege_level(),
        db: attributes.default() as u8,
        s: attributes.non_system_segment() as u8,
        l: attributes.long() as u8,
        g: attributes.granularity() as u8,
        avl: attributes.available() as u8,

        unusable: 0,
        padding: 0,
    }
}

pub(crate) fn seg_reg_from_kvm(reg: kvm::kvm_segment) -> SegmentRegister {
    // KVM forces the "accessed" bit (bit 0 of the segment type) on every
    // non-LDTR segment when unrestricted guest mode is active. For unusable
    // segments (`present == 0`) this bit is architecturally meaningless, but it
    // makes a save/restore round trip non-idempotent: a null segment saved as
    // `0xc000` is read back as `0xc001`. Mask the accessed bit back off for
    // non-present segments so that the value read here is canonical and stable
    // across a round trip.
    let present = reg.present == 1;
    let segment_type = if present { reg.type_ } else { reg.type_ & !1 };
    SegmentRegister {
        base: reg.base,
        limit: reg.limit,
        selector: reg.selector,
        attributes: SegmentAttributes::new()
            .with_segment_type(segment_type)
            .with_non_system_segment(reg.s == 1)
            .with_descriptor_privilege_level(reg.dpl)
            .with_present(present)
            .with_available(reg.avl == 1)
            .with_long(reg.l == 1)
            .with_default(reg.db == 1)
            .with_granularity(reg.g == 1)
            .into(),
    }
}

pub(crate) fn table_reg(reg: TableRegister) -> kvm::kvm_dtable {
    kvm::kvm_dtable {
        base: reg.base,
        limit: reg.limit,
        padding: [0; 3],
    }
}

pub(crate) fn table_reg_from_kvm(reg: kvm::kvm_dtable) -> TableRegister {
    TableRegister {
        base: reg.base,
        limit: reg.limit,
    }
}

impl AccessVpState for KvmVpStateAccess<'_, '_> {
    type Error = KvmError;

    fn caps(&self) -> &virt::PartitionCapabilities {
        &self.vp.partition.caps
    }

    fn commit(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn registers(&mut self) -> Result<vp::Registers, Self::Error> {
        let regs = self.kvm().get_regs()?;

        let sregs = self.kvm().get_sregs()?;

        Ok(vp::Registers {
            rax: regs.rax,
            rcx: regs.rcx,
            rdx: regs.rdx,
            rbx: regs.rbx,
            rbp: regs.rbp,
            rsp: regs.rsp,
            rsi: regs.rsi,
            rdi: regs.rdi,
            r8: regs.r8,
            r9: regs.r9,
            r10: regs.r10,
            r11: regs.r11,
            r12: regs.r12,
            r13: regs.r13,
            r14: regs.r14,
            r15: regs.r15,
            rip: regs.rip,
            rflags: regs.rflags,
            cs: seg_reg_from_kvm(sregs.cs),
            ds: seg_reg_from_kvm(sregs.ds),
            es: seg_reg_from_kvm(sregs.es),
            fs: seg_reg_from_kvm(sregs.fs),
            gs: seg_reg_from_kvm(sregs.gs),
            ss: seg_reg_from_kvm(sregs.ss),
            tr: seg_reg_from_kvm(sregs.tr),
            ldtr: seg_reg_from_kvm(sregs.ldt),
            gdtr: table_reg_from_kvm(sregs.gdt),
            idtr: table_reg_from_kvm(sregs.idt),
            cr0: sregs.cr0,
            cr2: sregs.cr2,
            cr3: sregs.cr3,
            cr4: sregs.cr4,
            cr8: sregs.cr8,
            efer: sregs.efer,
        })
    }

    fn set_registers(&mut self, value: &vp::Registers) -> Result<(), Self::Error> {
        let regs = kvm::kvm_regs {
            rax: value.rax,
            rbx: value.rbx,
            rcx: value.rcx,
            rdx: value.rdx,
            rsi: value.rsi,
            rdi: value.rdi,
            rsp: value.rsp,
            rbp: value.rbp,
            r8: value.r8,
            r9: value.r9,
            r10: value.r10,
            r11: value.r11,
            r12: value.r12,
            r13: value.r13,
            r14: value.r14,
            r15: value.r15,
            rip: value.rip,
            rflags: value.rflags,
        };

        let sregs = self.kvm().get_sregs()?;

        let sregs = kvm::kvm_sregs {
            cs: seg_reg(value.cs),
            ds: seg_reg(value.ds),
            es: seg_reg(value.es),
            fs: seg_reg(value.fs),
            gs: seg_reg(value.gs),
            ss: seg_reg(value.ss),
            tr: seg_reg(value.tr),
            ldt: seg_reg(value.ldtr),
            gdt: table_reg(value.gdtr),
            idt: table_reg(value.idtr),
            cr0: value.cr0,
            cr2: value.cr2,
            cr3: value.cr3,
            cr4: value.cr4,
            cr8: value.cr8,
            efer: value.efer,
            interrupt_bitmap: [0; 4],
            ..sregs
        };

        self.kvm().set_regs(&regs)?;
        self.kvm().set_sregs(&sregs)?;
        Ok(())
    }

    fn activity(&mut self) -> Result<vp::Activity, Self::Error> {
        let mp_state = match self.kvm().get_mp_state()? {
            kvm::KVM_MP_STATE_RUNNABLE => vp::MpState::Running,
            kvm::KVM_MP_STATE_UNINITIALIZED => vp::MpState::WaitForSipi, // TODO: add a state for this
            kvm::KVM_MP_STATE_INIT_RECEIVED => vp::MpState::WaitForSipi,
            kvm::KVM_MP_STATE_HALTED => vp::MpState::Halted,
            kvm::KVM_MP_STATE_SIPI_RECEIVED => todo!("handle intermediate sipi states"),
            state => {
                panic!("unrecognized mp state {}", state);
            }
        };
        let events = self.kvm().get_vcpu_events()?;

        // N.B. KVM has no way to get back an extint queued with KVM_INTERRUPT.
        //      PIC interrupts are normally placed in the injected-interrupt
        //      slot instead, which is reported below as a pending
        //      interruption.
        let event = if events.exception.pending != 0 {
            Some(vp::PendingEvent::Exception {
                vector: events.exception.nr,
                error_code: (events.exception.has_error_code != 0)
                    .then_some(events.exception.error_code),
                parameter: if events.exception_has_payload != 0 {
                    events.exception_payload
                } else {
                    0
                },
            })
        } else {
            None
        };

        let interruption = if events.exception.injected != 0 {
            Some(vp::PendingInterruption::Exception {
                vector: events.exception.nr,
                error_code: (events.exception.has_error_code != 0)
                    .then_some(events.exception.error_code),
            })
        } else if events.nmi.injected != 0 {
            Some(vp::PendingInterruption::Nmi)
        } else if events.interrupt.injected != 0 {
            Some(vp::PendingInterruption::Interrupt {
                vector: events.interrupt.nr,
            })
        } else {
            None
        };

        Ok(vp::Activity {
            mp_state,
            nmi_pending: events.nmi.pending != 0,
            nmi_masked: events.nmi.masked != 0,
            interrupt_shadow: events.interrupt.shadow != 0,
            pending_event: event,
            pending_interruption: interruption,
        })
    }

    fn set_activity(&mut self, value: &vp::Activity) -> Result<(), Self::Error> {
        let vp::Activity {
            mp_state,
            nmi_pending,
            nmi_masked,
            interrupt_shadow,
            pending_event,
            pending_interruption,
        } = *value;

        // KVM can neither report nor withdraw an extint queued with
        // KVM_INTERRUPT, so a restored one would be missing from the next save,
        // and a reset could not discard it. Reject it before changing any
        // activity state. KVM's own saved state never has one: injected PIC
        // interrupts are saved as pending interruptions, and queued ones are
        // not saved. Like any other restore error, this does not undo what
        // restore_all restored before the activity, such as the registers.
        if let Some(vp::PendingEvent::ExtInt { .. }) = pending_event {
            return Err(KvmError::InvalidState(
                "restoring a pending ExtINT is not supported",
            ));
        }

        let state = match mp_state {
            vp::MpState::Running => kvm::KVM_MP_STATE_RUNNABLE,
            vp::MpState::WaitForSipi => kvm::KVM_MP_STATE_INIT_RECEIVED,
            vp::MpState::Halted => kvm::KVM_MP_STATE_HALTED,
            vp::MpState::Idle => {
                return Err(KvmError::InvalidState("Hyper-V idle state not supported"));
            }
        };
        self.kvm().set_mp_state(state)?;

        // KVM changes the pending NMI and the interrupt shadow only when these
        // flags are set, so set them for false values too: a reset or a
        // restore must clear them. Both happen while the VPs are stopped, so
        // no other VP can send an NMI that this would discard. (KVM cannot
        // withdraw an NMI that it has handed to AMD's virtual NMI, though.)
        let event_flags = kvm::KVM_VCPUEVENT_VALID_NMI_PENDING | kvm::KVM_VCPUEVENT_VALID_SHADOW;

        let mut events = kvm::kvm_vcpu_events {
            exception: kvm::kvm_vcpu_events__bindgen_ty_1 {
                injected: 0,
                nr: 0,
                has_error_code: 0,
                pending: 0,
                error_code: 0,
            },
            interrupt: kvm::kvm_vcpu_events__bindgen_ty_2 {
                injected: 0,
                nr: 0,
                soft: 0,
                shadow: interrupt_shadow.into(),
            },
            nmi: kvm::kvm_vcpu_events__bindgen_ty_3 {
                injected: 0,
                pending: nmi_pending.into(),
                masked: nmi_masked.into(),
                pad: 0,
            },
            sipi_vector: 0,
            flags: event_flags,
            exception_has_payload: 0,
            exception_payload: 0,
            ..Default::default()
        };

        match pending_event {
            Some(vp::PendingEvent::Exception {
                vector,
                error_code,
                parameter,
            }) => {
                events.exception.pending = true.into();
                events.exception.nr = vector;
                events.exception.has_error_code = error_code.is_some().into();
                events.exception.error_code = error_code.unwrap_or(0);
                // TODO
                let _ = parameter;
            }
            // Rejected above.
            Some(vp::PendingEvent::ExtInt { .. }) | None => {}
        }

        match pending_interruption {
            Some(vp::PendingInterruption::Exception { vector, error_code }) => {
                events.exception.injected = true.into();
                events.exception.nr = vector;
                events.exception.has_error_code = error_code.is_some().into();
                events.exception.error_code = error_code.unwrap_or(0);
            }
            Some(vp::PendingInterruption::Interrupt { vector }) => {
                events.interrupt.injected = true.into();
                events.interrupt.nr = vector;
            }
            Some(vp::PendingInterruption::Nmi) => {
                events.nmi.injected = true.into();
            }
            None => {}
        }

        self.kvm().set_vcpu_events(&events)?;
        Ok(())
    }

    fn xsave(&mut self) -> Result<vp::Xsave, Self::Error> {
        let mut data = [0; 4096];
        self.kvm().get_xsave(&mut data)?;
        Ok(vp::Xsave::from_standard(&data, &self.vp.partition.caps))
    }

    fn set_xsave(&mut self, value: &vp::Xsave) -> Result<(), Self::Error> {
        let mut data = [0; 4096];
        value.write_standard(&mut data, &self.vp.partition.caps);
        self.kvm().set_xsave(&data)?;
        Ok(())
    }

    fn apic(&mut self) -> Result<vp::Apic, Self::Error> {
        let mut apic_base = 0;
        self.kvm().get_msrs(
            &[x86defs::X86X_MSR_APIC_BASE],
            std::slice::from_mut(&mut apic_base),
        )?;

        let mut state = FromZeros::new_zeroed();
        self.kvm().get_lapic(&mut state)?;

        Ok(vp::Apic::new(
            apic_base.into(),
            vp::ApicRegisters::from_page(&state),
            [0; 8],
        ))
    }

    fn set_apic(&mut self, value: &vp::Apic) -> Result<(), Self::Error> {
        // Set this first to set the APIC mode before updating the APIC register
        // state.
        self.kvm()
            .set_msrs(&[(x86defs::X86X_MSR_APIC_BASE, value.apic_base)])?;

        self.kvm().set_lapic(&value.registers().as_page())?;
        Ok(())
    }

    fn xcr(&mut self) -> Result<vp::Xcr0, Self::Error> {
        Ok(vp::Xcr0 {
            value: self.kvm().get_xcr0()?,
        })
    }

    fn set_xcr(&mut self, value: &vp::Xcr0) -> Result<(), Self::Error> {
        let vp::Xcr0 { value } = value;
        self.kvm().set_xcr0(*value)?;
        Ok(())
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

    fn set_mtrrs(&mut self, value: &vp::Mtrrs) -> Result<(), Self::Error> {
        self.set_register_state(value)
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

    fn set_virtual_msrs(&mut self, value: &vp::VirtualMsrs) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn debug_regs(&mut self) -> Result<vp::DebugRegisters, Self::Error> {
        let regs = self.kvm().get_debug_regs()?;
        Ok(vp::DebugRegisters {
            dr0: regs.db[0],
            dr1: regs.db[1],
            dr2: regs.db[2],
            dr3: regs.db[3],
            dr6: regs.dr6,
            dr7: regs.dr7,
        })
    }

    fn set_debug_regs(&mut self, value: &vp::DebugRegisters) -> Result<(), Self::Error> {
        self.kvm().set_debug_regs(&kvm::DebugRegisters {
            db: [value.dr0, value.dr1, value.dr2, value.dr3],
            dr6: value.dr6,
            dr7: value.dr7,
        })?;
        Ok(())
    }

    fn tsc(&mut self) -> Result<vp::Tsc, Self::Error> {
        self.get_register_state()
    }

    fn set_tsc(&mut self, tsc: &vp::Tsc) -> Result<(), Self::Error> {
        self.set_register_state(tsc)
    }

    fn tsc_deadline(&mut self) -> Result<vp::TscDeadline, Self::Error> {
        self.get_register_state()
    }

    fn set_tsc_deadline(&mut self, value: &vp::TscDeadline) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn cet(&mut self) -> Result<vp::Cet, Self::Error> {
        self.get_register_state()
    }

    fn set_cet(&mut self, value: &vp::Cet) -> Result<(), Self::Error> {
        self.set_register_state(value)
    }

    fn cet_ss(&mut self) -> Result<vp::CetSs, Self::Error> {
        // TODO: KVM does not appear to support CET_SS and in particular does not have
        // an API to get the SSP register yet.
        Ok(Default::default())
    }

    fn set_cet_ss(&mut self, _value: &vp::CetSs) -> Result<(), Self::Error> {
        // TODO: KVM does not appear to support CET_SS and in particular does not have
        // an API to get the SSP register yet.
        Ok(())
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
        self.set_register_state(value)?;

        // Mirror the restored synic MSRs into the processor's tracked state and
        // overlay pages. Runs before set_synic_{message,event_flags}_page (per
        // the state_trait! ordering) so those writes land in the right backing.
        let scontrol = hvdef::HvSynicScontrol::from(value.scontrol);
        let simp = hvdef::HvSynicSimpSiefp::from(value.simp);
        let siefp = hvdef::HvSynicSimpSiefp::from(value.siefp);
        let partition = self.vp.partition;
        super::sync_synic_overlay(&mut self.vp.simp_overlay, simp, &partition.gm);
        super::sync_synic_overlay(&mut self.vp.siefp_overlay, siefp, &partition.gm);
        self.vp.scontrol = scontrol;
        self.vp.simp = simp;
        self.vp.siefp = siefp;
        *self.vp.inner.siefp.write() = if scontrol.enabled() { siefp } else { 0.into() };
        Ok(())
    }

    fn synic_message_page(&mut self) -> Result<vp::SynicMessagePage, Self::Error> {
        let data = self.vp.simp_overlay.save_page();
        Ok(vp::SynicMessagePage { data })
    }

    fn set_synic_message_page(&mut self, value: &vp::SynicMessagePage) -> Result<(), Self::Error> {
        // set_synic_msrs has already synced the overlay to the restored SIMP
        // MSR, so this lands the contents in the right backing.
        self.vp.simp_overlay.restore_page(&value.data);
        Ok(())
    }

    fn synic_event_flags_page(&mut self) -> Result<vp::SynicEventFlagsPage, Self::Error> {
        let data = self.vp.siefp_overlay.save_page();
        Ok(vp::SynicEventFlagsPage { data })
    }

    fn set_synic_event_flags_page(
        &mut self,
        value: &vp::SynicEventFlagsPage,
    ) -> Result<(), Self::Error> {
        // See set_synic_message_page.
        self.vp.siefp_overlay.restore_page(&value.data);
        Ok(())
    }

    fn synic_message_queues(&mut self) -> Result<vp::SynicMessageQueues, Self::Error> {
        Ok(self.vp.inner.synic_message_queue.save())
    }

    fn set_synic_message_queues(
        &mut self,
        value: &vp::SynicMessageQueues,
    ) -> Result<(), Self::Error> {
        self.vp.inner.synic_message_queue.restore(value);
        Ok(())
    }

    fn synic_timers(&mut self) -> Result<vp::SynicTimers, Self::Error> {
        let mut msrs = [0; 8];
        self.kvm().get_msrs(
            &[
                hvdef::HV_X64_MSR_STIMER0_CONFIG,
                hvdef::HV_X64_MSR_STIMER0_COUNT,
                hvdef::HV_X64_MSR_STIMER1_CONFIG,
                hvdef::HV_X64_MSR_STIMER1_COUNT,
                hvdef::HV_X64_MSR_STIMER2_CONFIG,
                hvdef::HV_X64_MSR_STIMER2_COUNT,
                hvdef::HV_X64_MSR_STIMER3_CONFIG,
                hvdef::HV_X64_MSR_STIMER3_COUNT,
            ],
            &mut msrs,
        )?;

        // KVM does not currently provide a way to get the adjustment or the
        // underlivered message expiration time.
        let timers = [
            vp::SynicTimer {
                config: msrs[0],
                count: msrs[1],
                adjustment: 0,
                undelivered_message_expiration_time: None,
            },
            vp::SynicTimer {
                config: msrs[2],
                count: msrs[3],
                adjustment: 0,
                undelivered_message_expiration_time: None,
            },
            vp::SynicTimer {
                config: msrs[4],
                count: msrs[5],
                adjustment: 0,
                undelivered_message_expiration_time: None,
            },
            vp::SynicTimer {
                config: msrs[6],
                count: msrs[7],
                adjustment: 0,
                undelivered_message_expiration_time: None,
            },
        ];

        Ok(vp::SynicTimers { timers })
    }

    fn set_synic_timers(&mut self, value: &vp::SynicTimers) -> Result<(), Self::Error> {
        // KVM does not yet provide a way to set the expiration time or pending
        // message state.
        self.kvm().set_msrs(&[
            (hvdef::HV_X64_MSR_STIMER0_CONFIG, value.timers[0].config),
            (hvdef::HV_X64_MSR_STIMER0_COUNT, value.timers[0].count),
            (hvdef::HV_X64_MSR_STIMER1_CONFIG, value.timers[1].config),
            (hvdef::HV_X64_MSR_STIMER1_COUNT, value.timers[1].count),
            (hvdef::HV_X64_MSR_STIMER2_CONFIG, value.timers[2].config),
            (hvdef::HV_X64_MSR_STIMER2_COUNT, value.timers[2].count),
            (hvdef::HV_X64_MSR_STIMER3_CONFIG, value.timers[3].config),
            (hvdef::HV_X64_MSR_STIMER3_COUNT, value.timers[3].count),
        ])?;
        Ok(())
    }

    fn nested_state(&mut self) -> Result<vp::NestedState, Self::Error> {
        Err(KvmError::NotSupported)
    }

    fn set_nested_state(&mut self, _value: &vp::NestedState) -> Result<(), Self::Error> {
        Err(KvmError::NotSupported)
    }
}

/// Tests against the host's KVM.
#[cfg(test)]
mod tests {
    use crate::Kvm;
    use crate::KvmPartition;
    use crate::KvmProcessorBinder;
    use guestmem::GuestMemory;
    use hvdef::Vtl;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use test_with_tracing::test;
    use virt::BindProcessor;
    use virt::Hypervisor;
    use virt::Partition;
    use virt::PartitionConfig;
    use virt::Processor;
    use virt::ProtoPartition;
    use virt::ProtoPartitionConfig;
    use virt::irqcon::DeliveryMode;
    use virt::irqcon::MsiRequest;
    use virt::state::StateElement;
    use virt::x86::vp;
    use virt::x86::vp::AccessVpState;
    use vm_topology::memory::MemoryLayout;
    use vm_topology::processor::ProcessorTopology;
    use vm_topology::processor::TopologyBuilder;
    use vmcore::vmtime::VmTime;
    use vmcore::vmtime::VmTimeKeeper;

    /// Returns whether KVM hands pending NMIs to AMD's virtual NMI (the VMCB's
    /// V_NMI_PENDING), from which `KVM_SET_VCPU_EVENTS` cannot withdraw them.
    fn kvm_uses_vnmi() -> bool {
        std::fs::read_to_string("/sys/module/kvm_amd/parameters/vnmi")
            .is_ok_and(|value| value.trim() == "Y")
    }

    /// A partition on the host's KVM whose VPs never run, so that it maps no
    /// RAM.
    struct Fixture {
        topology: ProcessorTopology,
        partition: KvmPartition,
        binders: Vec<KvmProcessorBinder>,
        _keeper: VmTimeKeeper,
    }

    impl Fixture {
        async fn new(driver: &DefaultDriver, vp_count: u32) -> Self {
            let keeper = VmTimeKeeper::new(driver, VmTime::from_100ns(0));
            let vmtime = keeper.builder().build(driver).await.unwrap();
            let topology = TopologyBuilder::new_x86().build(vp_count).unwrap();
            let mem_layout = MemoryLayout::new(4096, &[], &[], &[], None).unwrap();
            let guest_memory = GuestMemory::allocate(4096);
            let (partition, binders) = Kvm::new()
                .unwrap()
                .new_partition(ProtoPartitionConfig {
                    processor_topology: &topology,
                    hv_config: None,
                    vmtime: &vmtime,
                    isolation: virt::ProtoPartitionIsolation::None,
                    nested_virt: false,
                })
                .unwrap()
                .build(PartitionConfig {
                    mem_layout: &mem_layout,
                    guest_memory: &guest_memory,
                    cpuid: &[],
                    vtl0_alias_map: None,
                    fault_resolver: None,
                })
                .unwrap();
            Self {
                topology,
                partition,
                binders,
                _keeper: keeper,
            }
        }
    }

    /// Checks that the reset of a VP, the VP part of a VM reset, discards an
    /// NMI that the VP has not taken and an interrupt shadow (after STI or MOV
    /// SS), on the BSP and on an AP.
    #[async_test]
    #[ignore = "requires access to /dev/kvm"]
    async fn reset_clears_a_pending_nmi_and_the_interrupt_shadow(driver: DefaultDriver) {
        // A pending NMI that KVM has handed to AMD's virtual NMI survives a
        // reset, so only check the interrupt shadow there.
        let nmi = !kvm_uses_vnmi();
        if !nmi {
            tracing::info!("KVM uses AMD's virtual NMI: not checking a pending NMI");
        }
        let mut fixture = Fixture::new(&driver, 2).await;
        let Fixture {
            topology,
            partition,
            binders,
            ..
        } = &mut fixture;
        let mut vps = binders
            .iter_mut()
            .map(|binder| binder.bind().unwrap())
            .collect::<Vec<_>>();

        for (vp, vp_info) in vps.iter_mut().zip(topology.vps_arch()) {
            if nmi {
                partition.request_msi(
                    Vtl::Vtl0,
                    MsiRequest::new_x86(DeliveryMode::NMI, vp_info.apic_id, false, 0, false),
                );
            }
            let mut access = vp.access_state(Vtl::Vtl0);
            let mut activity = access.activity().unwrap();
            assert_eq!(activity.nmi_pending, nmi);
            activity.interrupt_shadow = true;
            access.set_activity(&activity).unwrap();
            let activity = access.activity().unwrap();
            assert_eq!(
                (activity.nmi_pending, activity.interrupt_shadow),
                (nmi, true)
            );
        }

        partition.supports_reset().unwrap().reset().unwrap();
        for (vp, vp_info) in vps.iter_mut().zip(topology.vps_arch()) {
            vp.reset().unwrap();
            assert_eq!(
                vp.access_state(Vtl::Vtl0).activity().unwrap(),
                vp::Activity::at_reset(partition.caps(), &vp_info)
            );
        }
    }

    /// Checks that setting the activity, as a restore does, replaces the
    /// pending NMI, NMI masking, and the interrupt shadow, including clearing
    /// them.
    #[async_test]
    #[ignore = "requires access to /dev/kvm"]
    async fn set_activity_replaces_the_pending_nmi_and_the_interrupt_shadow(
        driver: DefaultDriver,
    ) {
        // KVM cannot withdraw a pending NMI that it has handed to AMD's virtual
        // NMI, so only check the other state there.
        let nmi_values: &[bool] = if kvm_uses_vnmi() {
            tracing::info!("KVM uses AMD's virtual NMI: not checking a pending NMI");
            &[false]
        } else {
            &[false, true]
        };
        let mut values = Vec::new();
        for &nmi_pending in nmi_values {
            for nmi_masked in [false, true] {
                for interrupt_shadow in [false, true] {
                    values.push((nmi_pending, nmi_masked, interrupt_shadow));
                }
            }
        }

        let mut fixture = Fixture::new(&driver, 1).await;
        let mut vp = fixture.binders[0].bind().unwrap();
        let mut access = vp.access_state(Vtl::Vtl0);
        for &from in &values {
            for &to in &values {
                for (nmi_pending, nmi_masked, interrupt_shadow) in [from, to] {
                    let activity = vp::Activity {
                        mp_state: vp::MpState::Running,
                        nmi_pending,
                        nmi_masked,
                        interrupt_shadow,
                        pending_event: None,
                        pending_interruption: None,
                    };
                    access.set_activity(&activity).unwrap();
                    assert_eq!(
                        access.activity().unwrap(),
                        activity,
                        "{from:?} -> {to:?}"
                    );
                }
            }
        }
    }
}
