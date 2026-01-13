// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

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
use vm_topology::processor::x86::X86VpInfo;
use x86defs::SegmentAttributes;
use zerocopy::FromZeros;

pub struct KvmVpStateAccess<'a> {
    partition: &'a KvmPartitionInner,
    vp_info: X86VpInfo,
}

impl KvmPartitionInner {
    pub fn vp_state_access(&self, vp_index: VpIndex) -> KvmVpStateAccess<'_> {
        KvmVpStateAccess {
            partition: self,
            vp_info: self.vp(vp_index).vp_info,
        }
    }
}

impl KvmVpStateAccess<'_> {
    pub(crate) fn kvm(&self) -> kvm::Processor<'_> {
        self.partition.kvm.vp(self.vp_info.apic_id)
    }

    pub(crate) fn set_register_state<T, const N: usize>(&self, value: &T) -> Result<(), KvmError>
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

        self.kvm().set_msrs(&msrs)?;
        Ok(())
    }

    pub(crate) fn get_register_state<T, const N: usize>(&self) -> Result<T, KvmError>
    where
        T: HvRegisterState<HvX64RegisterName, N>,
    {
        let mut msrs = T::default();
        let names = msrs.names().map(|name| register_to_msr(name).unwrap());
        let mut values = [0; N];
        self.kvm().get_msrs(&names, &mut values)?;
        msrs.set_values(values.into_iter().map(|v| v.into()));
        Ok(msrs)
    }
}

fn seg_reg(reg: SegmentRegister) -> kvm::kvm_segment {
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

fn seg_reg_from_kvm(reg: kvm::kvm_segment) -> SegmentRegister {
    SegmentRegister {
        base: reg.base,
        limit: reg.limit,
        selector: reg.selector,
        attributes: SegmentAttributes::new()
            .with_segment_type(reg.type_)
            .with_non_system_segment(reg.s == 1)
            .with_descriptor_privilege_level(reg.dpl)
            .with_present(reg.present == 1)
            .with_available(reg.avl == 1)
            .with_long(reg.l == 1)
            .with_default(reg.db == 1)
            .with_granularity(reg.g == 1)
            .into(),
    }
}

fn table_reg(reg: TableRegister) -> kvm::kvm_dtable {
    kvm::kvm_dtable {
        base: reg.base,
        limit: reg.limit,
        padding: [0; 3],
    }
}

fn table_reg_from_kvm(reg: kvm::kvm_dtable) -> TableRegister {
    TableRegister {
        base: reg.base,
        limit: reg.limit,
    }
}

impl AccessVpState for KvmVpStateAccess<'_> {
    type Error = KvmError;

    fn caps(&self) -> &virt::PartitionCapabilities {
        &self.partition.caps
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

        // N.B. KVM has no way to get back the pending extint vector.
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
        let state = match value.mp_state {
            vp::MpState::Running => kvm::KVM_MP_STATE_RUNNABLE,
            vp::MpState::WaitForSipi => kvm::KVM_MP_STATE_INIT_RECEIVED,
            vp::MpState::Halted => kvm::KVM_MP_STATE_HALTED,
            vp::MpState::Idle => {
                return Err(KvmError::InvalidState("Hyper-V idle state not supported"));
            }
        };
        self.kvm().set_mp_state(state)?;

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
                shadow: value.interrupt_shadow.into(),
            },
            nmi: kvm::kvm_vcpu_events__bindgen_ty_3 {
                injected: 0,
                pending: value.nmi_pending.into(),
                masked: value.nmi_masked.into(),
                pad: 0,
            },
            sipi_vector: 0,
            flags: 0,
            exception_has_payload: 0,
            exception_payload: 0,
            ..Default::default()
        };

        match value.pending_event {
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
            Some(vp::PendingEvent::ExtInt { vector }) => {
                // N.B. KVM has no way to clear a pending (but non-injected)
                //      extint interrupt.
                self.kvm().interrupt(vector.into())?;
            }
            None => {}
        }

        match value.pending_interruption {
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
        Ok(vp::Xsave::from_standard(&data, &self.partition.caps))
    }

    fn set_xsave(&mut self, value: &vp::Xsave) -> Result<(), Self::Error> {
        let mut data = [0; 4096];
        value.write_standard(&mut data, &self.partition.caps);
        self.kvm().set_xsave(&data)?;
        Ok(())
    }

    fn apic(&mut self) -> Result<vp::Apic, Self::Error> {
        let mut apic_base = [0];
        self.kvm()
            .get_msrs(&[x86defs::X86X_MSR_APIC_BASE], &mut apic_base)?;

        let mut state = FromZeros::new_zeroed();
        self.kvm().get_lapic(&mut state)?;

        Ok(vp::Apic::from_page(apic_base[0], &state))
    }

    fn set_apic(&mut self, value: &vp::Apic) -> Result<(), Self::Error> {
        // Set this first to set the APIC mode before updating the APIC register
        // state.
        self.kvm()
            .set_msrs(&[(x86defs::X86X_MSR_APIC_BASE, value.apic_base)])?;

        self.kvm().set_lapic(&value.as_page())?;
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
        self.set_register_state(value)
    }

    fn synic_message_page(&mut self) -> Result<vp::SynicMessagePage, Self::Error> {
        // TODO
        Ok(vp::SynicMessagePage { data: [0; 4096] })
    }

    fn set_synic_message_page(&mut self, _value: &vp::SynicMessagePage) -> Result<(), Self::Error> {
        // TODO
        Ok(())
    }

    fn synic_event_flags_page(&mut self) -> Result<vp::SynicEventFlagsPage, Self::Error> {
        // TODO
        Ok(vp::SynicEventFlagsPage { data: [0; 4096] })
    }

    fn set_synic_event_flags_page(
        &mut self,
        _value: &vp::SynicEventFlagsPage,
    ) -> Result<(), Self::Error> {
        // TODO
        Ok(())
    }

    fn synic_message_queues(&mut self) -> Result<vp::SynicMessageQueues, Self::Error> {
        // TODO
        Ok(Default::default())
    }

    fn set_synic_message_queues(
        &mut self,
        _value: &vp::SynicMessageQueues,
    ) -> Result<(), Self::Error> {
        // TODO
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
}
