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

        let mut event_flags = 0;
        if nmi_pending {
            event_flags |= kvm::KVM_VCPUEVENT_VALID_NMI_PENDING;
        }
        if interrupt_shadow {
            event_flags |= kvm::KVM_VCPUEVENT_VALID_SHADOW;
        }

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

        self.kvm().set_lapic(&kvm_lapic_page(value.registers()))?;
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

/// Returns the page that `KVM_SET_LAPIC` takes for `registers`.
///
/// KVM starts a counting timer from the page's current count, so it fires an
/// expired one-shot timer, whose current count is 0 and initial count is not,
/// once more: at once from Linux 6.3, a full period later before. That
/// interrupt is owed when KVM had not injected the expiry before it saved the
/// page, which the page cannot show, but it duplicates an expiry whose vector
/// is pending in IRR, as after a time ABI restore that expires the timer. Such
/// a timer gets an initial count of 0, which KVM does not start, so the guest
/// reads 0 from the initial count register.
fn kvm_lapic_page(registers: &vp::ApicRegisters) -> [u8; 1024] {
    let mut registers = *registers;
    let lvt = x86defs::apic::Lvt::from(registers.lvt_timer);
    let vector = lvt.vector();
    if registers.timer_icr != 0
        && registers.timer_ccr == 0
        && !lvt.masked()
        && lvt.timer_mode() == x86defs::apic::TimerMode::ONE_SHOT.0
        && lvt.delivery_mode() == x86defs::apic::DeliveryMode::FIXED.0
        && vector >= 16
        && registers.irr[usize::from(vector / 32)] & (1 << (vector % 32)) != 0
    {
        registers.timer_icr = 0;
    }
    registers.as_page()
}

#[cfg(test)]
mod tests {
    use super::kvm_lapic_page;
    use virt::x86::vp::ApicRegisters;
    use zerocopy::FromZeros;

    const VECTOR: u32 = 0x40;
    const MASKED: u32 = 1 << 16;

    /// Returns the initial count of the page that KVM gets for a timer with
    /// the LVT entry `lvt_timer` and the counts `(initial, current)`, whose
    /// vector is pending in IRR if `pending`.
    fn kvm_initial_count(lvt_timer: u32, (initial, current): (u32, u32), pending: bool) -> u32 {
        let mut registers = ApicRegisters {
            lvt_timer,
            timer_icr: initial,
            timer_ccr: current,
            ..FromZeros::new_zeroed()
        };
        if pending {
            let vector = lvt_timer & 0xff;
            registers.irr[vector as usize / 32] |= 1 << (vector % 32);
        }
        let page = ApicRegisters::from_page(&kvm_lapic_page(&registers));
        // Nothing else changes.
        assert_eq!(
            ApicRegisters {
                timer_icr: initial,
                ..page
            }
            .as_page(),
            registers.as_page()
        );
        page.timer_icr
    }

    #[test]
    fn an_expired_timer_whose_interrupt_is_pending_is_not_started_again() {
        assert_eq!(kvm_initial_count(VECTOR, (1_000, 0), true), 0);
        // Otherwise the saved counts reach KVM unchanged: a counting timer, an
        // expired timer whose interrupt KVM may not have injected when it
        // saved the page, and timers whose expiry delivers nothing to IRR.
        for (lvt, counts, pending) in [
            (VECTOR, (1_000, 1), true),
            (VECTOR, (1_000, 0), false),
            (VECTOR | MASKED, (1_000, 0), true),
            (VECTOR | 1 << 17, (1_000, 0), true),
            (VECTOR | 4 << 8, (1_000, 0), true),
            (0x0f, (1_000, 0), true),
        ] {
            assert_eq!(
                kvm_initial_count(lvt, counts, pending),
                counts.0,
                "LVT {lvt:#x}, counts {counts:?}, pending {pending}"
            );
        }
        assert_eq!(kvm_initial_count(VECTOR, (0, 0), true), 0);
    }
}

/// Restores of one-shot LAPIC timers through `KVM_SET_LAPIC`, with a real-mode
/// guest that counts its timer interrupts.
#[cfg(test)]
mod kvm_tests {
    use super::kvm_lapic_page;
    use crate::arch::x86_64::test_guest::GuestRam;
    use kvm::Exit;
    use kvm::Kvm;
    use kvm::Partition;
    use kvm::Processor;
    use kvm::VmType;
    use kvm::VpRunner;
    use kvm::kvm_cpuid_entry2;
    use kvm::kvm_regs;
    use std::sync::mpsc;
    use std::time::Duration;
    use test_with_tracing::test;
    use virt::time_abi::tsc::HostTscBracket;
    use virt::x86::vp::Apic;
    use virt::x86::vp::ApicRegisters;

    const TIMER_VECTOR: u8 = 0x40;
    const CODE_GPA: u64 = 0x1000;
    const HANDLER_GPA: u64 = 0x2000;
    const COUNT_GPA: u64 = 0x3000;
    const ARMED_PORT: u8 = 0x80;
    const TIMER_PORT: u8 = 0x81;
    const RFLAGS_IF: u64 = 1 << 9;
    /// KVM's LAPIC timer rate.
    const APIC_HZ: u64 = 1_000_000_000;
    const MS: u64 = 1_000_000;

    fn partition() -> Partition {
        kvm::init();
        let kvm = Kvm::new().unwrap();
        let mut partition = kvm.new_vm(VmType::Default).unwrap();
        partition.enable_split_irqchip(24).unwrap();
        partition.add_vp(0).unwrap();
        partition
    }

    /// Loads a real-mode guest into `ram` that arms its one-shot x2APIC timer
    /// for `count` ticks, writes to `ARMED_PORT`, and then spins with
    /// interrupts enabled. Its timer interrupt handler writes to `TIMER_PORT`
    /// and ends the interrupt.
    fn load_guest(partition: &Partition, ram: &GuestRam<'_>, count: u32) {
        ram.write(
            u64::from(TIMER_VECTOR) * 4,
            &[HANDLER_GPA as u8, (HANDLER_GPA >> 8) as u8, 0, 0],
        );
        ram.write(
            CODE_GPA,
            &[
                0x66, 0xb9, 0x38, 0x08, 0x00, 0x00, // mov ecx, TMICT
                0x66, 0xa1, 0x00, 0x30, // mov eax, [COUNT_GPA]
                0x66, 0x31, 0xd2, // xor edx, edx
                0x0f, 0x30, // wrmsr
                0xe6, ARMED_PORT, // out ARMED_PORT, al
                0xeb, 0xfe, // jmp $
            ],
        );
        ram.write(
            HANDLER_GPA,
            &[
                0xe6, TIMER_PORT, // out TIMER_PORT, al
                0x66, 0xb9, 0x0b, 0x08, 0x00, 0x00, // mov ecx, EOI
                0x66, 0x31, 0xc0, // xor eax, eax
                0x66, 0x31, 0xd2, // xor edx, edx
                0x0f, 0x30, // wrmsr
                0xcf, // iret
            ],
        );
        ram.write(COUNT_GPA, &count.to_le_bytes());

        let vp = partition.vp(0);
        vp.set_cpuid(&[kvm_cpuid_entry2 {
            function: 1,
            ecx: 1 << 21, // x2APIC
            ..Default::default()
        }])
        .unwrap();
        let mut sregs = vp.get_sregs().unwrap();
        sregs.cs.selector = 0;
        sregs.cs.base = 0;
        vp.set_sregs(&sregs).unwrap();
        vp.set_regs(&kvm_regs {
            rip: CODE_GPA,
            rsp: 0x8000,
            rflags: RFLAGS_IF | 2,
            ..Default::default()
        })
        .unwrap();
        // Enabled, in x2APIC mode, of the BSP.
        vp.set_msrs(&[(x86defs::X86X_MSR_APIC_BASE, 0xfee0_0d00)])
            .unwrap();
        let mut page = [0; 1024];
        vp.get_lapic(&mut page).unwrap();
        let mut registers = ApicRegisters::from_page(&page);
        registers.svr = 0x1ff;
        registers.lvt_timer = TIMER_VECTOR.into();
        registers.timer_dcr = 0xb;
        vp.set_lapic(&registers.as_page()).unwrap();
    }

    /// Runs the VP until it writes to `until`, or for `limit`, and returns the
    /// timer interrupts that the guest took and whether it wrote to `until`.
    /// Completes the last exit, so that the VP's state can be read.
    fn run(
        partition: &Partition,
        runner: &mut VpRunner<'_>,
        until: Option<u8>,
        limit: Duration,
    ) -> (u32, bool) {
        std::thread::scope(|s| {
            let (done, wait_done) = mpsc::channel::<()>();
            s.spawn(move || {
                if let Err(mpsc::RecvTimeoutError::Timeout) = wait_done.recv_timeout(limit) {
                    partition.vp(0).force_exit();
                }
            });
            let mut interrupts = 0;
            let reached = loop {
                match runner.run().unwrap() {
                    Exit::IoOut { port, .. } => {
                        if port == u16::from(TIMER_PORT) {
                            interrupts += 1;
                        }
                        if until.is_some_and(|until| port == u16::from(until)) {
                            assert!(matches!(runner.complete_exit().unwrap(), Exit::Interrupted));
                            break true;
                        }
                    }
                    Exit::Interrupted => break false,
                    exit => panic!("unexpected exit: {exit:?}"),
                }
            };
            drop(done);
            (interrupts, reached)
        })
    }

    fn guest_tsc(vp: &Processor<'_>) -> u64 {
        let mut tsc = [0];
        vp.get_msrs(&[x86defs::X86X_MSR_TSC], &mut tsc).unwrap();
        tsc[0]
    }

    fn get_apic(vp: &Processor<'_>) -> Apic {
        let mut apic_base = [0];
        vp.get_msrs(&[x86defs::X86X_MSR_APIC_BASE], &mut apic_base)
            .unwrap();
        let mut page = [0; 1024];
        vp.get_lapic(&mut page).unwrap();
        Apic::new(apic_base[0].into(), ApicRegisters::from_page(&page), [0; 8])
    }

    fn timer_pending(apic: &Apic) -> bool {
        let vector = usize::from(TIMER_VECTOR);
        apic.registers().irr[vector / 32] & (1 << (vector % 32)) != 0
    }

    /// A capture: the guest TSC at its anchor, and the VP's saved APIC state.
    struct Capture {
        capture_tsc: u64,
        apic: Apic,
    }

    impl Capture {
        fn take(vp: &Processor<'_>) -> Self {
            Self {
                capture_tsc: guest_tsc(vp),
                apic: get_apic(vp),
            }
        }
    }

    /// Restores `capture` into the VP as a time ABI restore does, after a
    /// downtime of `downtime_ns`.
    fn restore(vp: &Processor<'_>, capture: &Capture, downtime_ns: u64) {
        let tsc_hz = vp.tsc_frequency_hz().unwrap();
        // The processor restore.
        vp.set_lapic(&kvm_lapic_page(capture.apic.registers()))
            .unwrap();
        // The synchronized TSC set, at the restore anchor.
        let restore_tsc =
            virt::time_abi::downtime::tsc_target(capture.capture_tsc, downtime_ns, tsc_hz).unwrap();
        let (bracket, ()) = HostTscBracket::around(|| ());
        vp.set_tsc_offset(restore_tsc.wrapping_sub(bracket.end))
            .unwrap();
        // The LAPIC advance.
        let mut apic = get_apic(vp);
        apic.advance_one_shot_timer(downtime_ns, APIC_HZ).unwrap();
        vp.set_lapic(&kvm_lapic_page(apic.registers())).unwrap();
    }

    /// A guest, with its VP stopped just after it armed its timer.
    struct ArmedGuest<'a> {
        partition: &'a Partition,
        runner: VpRunner<'a>,
    }

    impl<'a> ArmedGuest<'a> {
        fn new(partition: &'a Partition, ram: &GuestRam<'_>, count: u32) -> Self {
            load_guest(partition, ram, count);
            let mut runner = partition.vp(0).runner();
            assert_eq!(
                run(
                    partition,
                    &mut runner,
                    Some(ARMED_PORT),
                    Duration::from_secs(5)
                ),
                (0, true)
            );
            Self { partition, runner }
        }

        fn run(&mut self, until: Option<u8>, limit: Duration) -> (u32, bool) {
            run(self.partition, &mut self.runner, until, limit)
        }
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn a_timer_that_expires_in_the_downtime_fires_once() {
        let partition = partition();
        let ram = GuestRam::new(&partition, 0x10000);
        let mut guest = ArmedGuest::new(&partition, &ram, 1_000 * MS as u32);
        let vp = partition.vp(0);
        let capture = Capture::take(&vp);
        assert_ne!(capture.apic.registers().timer_ccr, 0);
        // The downtime expires the timer, which queues its interrupt, and
        // KVM must not fire the timer again for a VP that runs with
        // interrupts enabled.
        restore(&vp, &capture, 2_000 * MS);
        assert!(timer_pending(&get_apic(&vp)));
        assert_eq!(guest.run(None, Duration::from_millis(100)), (1, false));
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn a_timer_whose_expiry_kvm_had_not_delivered_fires_once() {
        let partition = partition();
        let ram = GuestRam::new(&partition, 0x10000);
        let mut guest = ArmedGuest::new(&partition, &ram, 2 * MS as u32);
        let vp = partition.vp(0);
        // The timer expires while the VP is stopped, so KVM saves it as one
        // whose interrupt the guest took: expired, its vector neither pending
        // nor in service. Its interrupt must not be lost.
        std::thread::sleep(Duration::from_millis(20));
        let capture = Capture::take(&vp);
        let saved = capture.apic.registers();
        let vector = usize::from(TIMER_VECTOR);
        assert_eq!((saved.timer_icr, saved.timer_ccr), (2 * MS as u32, 0));
        assert!(!timer_pending(&capture.apic));
        assert_eq!(saved.isr[vector / 32] & (1 << (vector % 32)), 0);
        restore(&vp, &capture, 10 * MS);
        assert_eq!(guest.run(None, Duration::from_millis(100)), (1, false));
    }
}
