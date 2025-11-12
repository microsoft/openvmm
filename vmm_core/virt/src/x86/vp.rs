// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-VP state.

use super::SegmentRegister;
use super::TableRegister;
use super::X86PartitionCapabilities;
use crate::state::HvRegisterState;
use crate::state::StateElement;
use crate::state::state_trait;
use hvdef::HV_MESSAGE_SIZE;
use hvdef::HvInternalActivityRegister;
use hvdef::HvRegisterValue;
use hvdef::HvX64InterruptStateRegister;
use hvdef::HvX64PendingEventReg0;
use hvdef::HvX64PendingExceptionEvent;
use hvdef::HvX64PendingExtIntEvent;
use hvdef::HvX64PendingInterruptionRegister;
use hvdef::HvX64PendingInterruptionType;
use hvdef::HvX64RegisterName;
use hvdef::HvX64SegmentRegister;
use hvdef::HvX64TableRegister;
use inspect::Inspect;
use mesh_protobuf::Protobuf;
use std::fmt::Debug;
use vm_topology::processor::x86::X86VpInfo;
use x86defs::RFlags;
use x86defs::X64_CR0_CD;
use x86defs::X64_CR0_ET;
use x86defs::X64_CR0_NW;
use x86defs::X64_EFER_NXE;
use x86defs::X86X_MSR_DEFAULT_PAT;
use x86defs::apic::APIC_BASE_PAGE;
use x86defs::apic::ApicBase;
use x86defs::apic::ApicVersion;
use x86defs::xsave::DEFAULT_MXCSR;
use x86defs::xsave::Fxsave;
use x86defs::xsave::INIT_FCW;
use x86defs::xsave::XCOMP_COMPRESSED;
use x86defs::xsave::XFEATURE_SSE;
use x86defs::xsave::XFEATURE_X87;
use x86defs::xsave::XFEATURE_YMM;
use x86defs::xsave::XSAVE_LEGACY_LEN;
use x86defs::xsave::XSAVE_VARIABLE_OFFSET;
use x86defs::xsave::XsaveHeader;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::Ref;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct Registers {
    #[inspect(hex)]
    #[mesh(1)]
    pub rax: u64,
    #[inspect(hex)]
    #[mesh(2)]
    pub rcx: u64,
    #[inspect(hex)]
    #[mesh(3)]
    pub rdx: u64,
    #[inspect(hex)]
    #[mesh(4)]
    pub rbx: u64,
    #[inspect(hex)]
    #[mesh(5)]
    pub rsp: u64,
    #[inspect(hex)]
    #[mesh(6)]
    pub rbp: u64,
    #[inspect(hex)]
    #[mesh(7)]
    pub rsi: u64,
    #[inspect(hex)]
    #[mesh(8)]
    pub rdi: u64,
    #[inspect(hex)]
    #[mesh(9)]
    pub r8: u64,
    #[inspect(hex)]
    #[mesh(10)]
    pub r9: u64,
    #[inspect(hex)]
    #[mesh(11)]
    pub r10: u64,
    #[inspect(hex)]
    #[mesh(12)]
    pub r11: u64,
    #[inspect(hex)]
    #[mesh(13)]
    pub r12: u64,
    #[inspect(hex)]
    #[mesh(14)]
    pub r13: u64,
    #[inspect(hex)]
    #[mesh(15)]
    pub r14: u64,
    #[inspect(hex)]
    #[mesh(16)]
    pub r15: u64,
    #[inspect(hex)]
    #[mesh(17)]
    pub rip: u64,
    #[inspect(hex)]
    #[mesh(18)]
    pub rflags: u64,
    #[mesh(19)]
    pub cs: SegmentRegister,
    #[mesh(20)]
    pub ds: SegmentRegister,
    #[mesh(21)]
    pub es: SegmentRegister,
    #[mesh(22)]
    pub fs: SegmentRegister,
    #[mesh(23)]
    pub gs: SegmentRegister,
    #[mesh(24)]
    pub ss: SegmentRegister,
    #[mesh(25)]
    pub tr: SegmentRegister,
    #[mesh(26)]
    pub ldtr: SegmentRegister,
    #[mesh(27)]
    pub gdtr: TableRegister,
    #[mesh(28)]
    pub idtr: TableRegister,
    #[inspect(hex)]
    #[mesh(29)]
    pub cr0: u64,
    #[inspect(hex)]
    #[mesh(30)]
    pub cr2: u64,
    #[inspect(hex)]
    #[mesh(31)]
    pub cr3: u64,
    #[inspect(hex)]
    #[mesh(32)]
    pub cr4: u64,
    #[inspect(hex)]
    #[mesh(33)]
    pub cr8: u64,
    #[inspect(hex)]
    #[mesh(34)]
    pub efer: u64,
}

impl HvRegisterState<HvX64RegisterName, 34> for Registers {
    fn names(&self) -> &'static [HvX64RegisterName; 34] {
        &[
            HvX64RegisterName::Rax,
            HvX64RegisterName::Rcx,
            HvX64RegisterName::Rdx,
            HvX64RegisterName::Rbx,
            HvX64RegisterName::Rsp,
            HvX64RegisterName::Rbp,
            HvX64RegisterName::Rsi,
            HvX64RegisterName::Rdi,
            HvX64RegisterName::R8,
            HvX64RegisterName::R9,
            HvX64RegisterName::R10,
            HvX64RegisterName::R11,
            HvX64RegisterName::R12,
            HvX64RegisterName::R13,
            HvX64RegisterName::R14,
            HvX64RegisterName::R15,
            HvX64RegisterName::Rip,
            HvX64RegisterName::Rflags,
            HvX64RegisterName::Cr0,
            HvX64RegisterName::Cr2,
            HvX64RegisterName::Cr3,
            HvX64RegisterName::Cr4,
            HvX64RegisterName::Cr8,
            HvX64RegisterName::Efer,
            HvX64RegisterName::Cs,
            HvX64RegisterName::Ds,
            HvX64RegisterName::Es,
            HvX64RegisterName::Fs,
            HvX64RegisterName::Gs,
            HvX64RegisterName::Ss,
            HvX64RegisterName::Tr,
            HvX64RegisterName::Ldtr,
            HvX64RegisterName::Gdtr,
            HvX64RegisterName::Idtr,
        ]
    }

    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>) {
        for (dest, src) in it.zip([
            self.rax.into(),
            self.rcx.into(),
            self.rdx.into(),
            self.rbx.into(),
            self.rsp.into(),
            self.rbp.into(),
            self.rsi.into(),
            self.rdi.into(),
            self.r8.into(),
            self.r9.into(),
            self.r10.into(),
            self.r11.into(),
            self.r12.into(),
            self.r13.into(),
            self.r14.into(),
            self.r15.into(),
            self.rip.into(),
            self.rflags.into(),
            self.cr0.into(),
            self.cr2.into(),
            self.cr3.into(),
            self.cr4.into(),
            self.cr8.into(),
            self.efer.into(),
            HvX64SegmentRegister::from(self.cs).into(),
            HvX64SegmentRegister::from(self.ds).into(),
            HvX64SegmentRegister::from(self.es).into(),
            HvX64SegmentRegister::from(self.fs).into(),
            HvX64SegmentRegister::from(self.gs).into(),
            HvX64SegmentRegister::from(self.ss).into(),
            HvX64SegmentRegister::from(self.tr).into(),
            HvX64SegmentRegister::from(self.ldtr).into(),
            HvX64TableRegister::from(self.gdtr).into(),
            HvX64TableRegister::from(self.idtr).into(),
        ]) {
            *dest = src;
        }
    }

    fn set_values(&mut self, mut it: impl Iterator<Item = HvRegisterValue>) {
        for (dest, src) in [
            &mut self.rax,
            &mut self.rcx,
            &mut self.rdx,
            &mut self.rbx,
            &mut self.rsp,
            &mut self.rbp,
            &mut self.rsi,
            &mut self.rdi,
            &mut self.r8,
            &mut self.r9,
            &mut self.r10,
            &mut self.r11,
            &mut self.r12,
            &mut self.r13,
            &mut self.r14,
            &mut self.r15,
            &mut self.rip,
            &mut self.rflags,
            &mut self.cr0,
            &mut self.cr2,
            &mut self.cr3,
            &mut self.cr4,
            &mut self.cr8,
            &mut self.efer,
        ]
        .into_iter()
        .zip(&mut it)
        {
            *dest = src.as_u64();
        }

        for (dest, src) in [
            &mut self.cs,
            &mut self.ds,
            &mut self.es,
            &mut self.fs,
            &mut self.gs,
            &mut self.ss,
            &mut self.tr,
            &mut self.ldtr,
        ]
        .into_iter()
        .zip(&mut it)
        {
            *dest = src.as_segment().into();
        }

        for (dest, src) in [&mut self.gdtr, &mut self.idtr].into_iter().zip(it) {
            *dest = src.as_table().into();
        }
    }
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for Registers {
    fn is_present(_caps: &X86PartitionCapabilities) -> bool {
        true
    }

    fn at_reset(caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        let cs = SegmentRegister {
            base: 0xffff0000,
            limit: 0xffff,
            selector: 0xf000,
            attributes: 0x9b,
        };
        let ds = SegmentRegister {
            base: 0,
            limit: 0xffff,
            selector: 0,
            attributes: 0x93,
        };
        let tr = SegmentRegister {
            base: 0,
            limit: 0xffff,
            selector: 0,
            attributes: 0x8b,
        };
        let ldtr = SegmentRegister {
            base: 0,
            limit: 0xffff,
            selector: 0,
            attributes: 0x82,
        };
        let gdtr = TableRegister {
            base: 0,
            limit: 0xffff,
        };
        let efer = if caps.nxe_forced_on { X64_EFER_NXE } else { 0 };
        Self {
            rax: 0,
            rcx: 0,
            rdx: caps.reset_rdx,
            rbx: 0,
            rbp: 0,
            rsp: 0,
            rsi: 0,
            rdi: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rip: 0xfff0,
            rflags: RFlags::at_reset().into(),
            cs,
            ds,
            es: ds,
            fs: ds,
            gs: ds,
            ss: ds,
            tr,
            ldtr,
            gdtr,
            idtr: gdtr,
            cr0: X64_CR0_ET | X64_CR0_CD | X64_CR0_NW,
            cr2: 0,
            cr3: 0,
            cr4: 0,
            cr8: 0,
            efer,
        }
    }
}

#[derive(Default, Debug, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct Activity {
    #[mesh(1)]
    pub mp_state: MpState,
    #[mesh(2)]
    pub nmi_pending: bool,
    #[mesh(3)]
    pub nmi_masked: bool,
    #[mesh(4)]
    pub interrupt_shadow: bool,
    #[mesh(5)]
    pub pending_event: Option<PendingEvent>,
    #[mesh(6)]
    pub pending_interruption: Option<PendingInterruption>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Protobuf, Inspect, Default)]
#[mesh(package = "virt.x86")]
pub enum MpState {
    #[mesh(1)]
    #[default]
    Running,
    #[mesh(2)]
    WaitForSipi,
    #[mesh(3)]
    Halted,
    #[mesh(4)]
    Idle,
}

// N.B. This does not include the NMI pending bit, which must be get/set via the
//      APIC page.
impl HvRegisterState<HvX64RegisterName, 4> for Activity {
    fn names(&self) -> &'static [HvX64RegisterName; 4] {
        &[
            HvX64RegisterName::InternalActivityState,
            HvX64RegisterName::PendingInterruption,
            HvX64RegisterName::InterruptState,
            HvX64RegisterName::PendingEvent0,
        ]
    }

    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>) {
        let mut activity = HvInternalActivityRegister::from(0);
        match self.mp_state {
            MpState::Running => {}
            MpState::WaitForSipi => {
                activity.set_startup_suspend(true);
            }
            MpState::Halted => {
                activity.set_halt_suspend(true);
            }
            MpState::Idle => {
                activity.set_idle_suspend(true);
            }
        };

        let pending_event = if let Some(event) = self.pending_event {
            match event {
                PendingEvent::Exception {
                    vector,
                    error_code,
                    parameter,
                } => HvX64PendingExceptionEvent::new()
                    .with_event_pending(true)
                    .with_event_type(hvdef::HV_X64_PENDING_EVENT_EXCEPTION)
                    .with_vector(vector.into())
                    .with_deliver_error_code(error_code.is_some())
                    .with_error_code(error_code.unwrap_or(0))
                    .with_exception_parameter(parameter)
                    .into(),

                PendingEvent::ExtInt { vector } => HvX64PendingExtIntEvent::new()
                    .with_event_pending(true)
                    .with_event_type(hvdef::HV_X64_PENDING_EVENT_EXT_INT)
                    .with_vector(vector)
                    .into(),
            }
        } else {
            0
        };

        let mut pending_interruption = HvX64PendingInterruptionRegister::new();
        if let Some(interruption) = self.pending_interruption {
            pending_interruption.set_interruption_pending(true);
            let ty = match interruption {
                PendingInterruption::Exception { vector, error_code } => {
                    pending_interruption.set_interruption_vector(vector.into());
                    pending_interruption.set_deliver_error_code(error_code.is_some());
                    pending_interruption.set_error_code(error_code.unwrap_or(0));
                    HvX64PendingInterruptionType::HV_X64_PENDING_EXCEPTION
                }
                PendingInterruption::Interrupt { vector } => {
                    pending_interruption.set_interruption_vector(vector.into());
                    HvX64PendingInterruptionType::HV_X64_PENDING_INTERRUPT
                }
                PendingInterruption::Nmi => HvX64PendingInterruptionType::HV_X64_PENDING_NMI,
            };
            pending_interruption.set_interruption_type(ty.0);
        }

        let interrupt_state = HvX64InterruptStateRegister::new()
            .with_nmi_masked(self.nmi_masked)
            .with_interrupt_shadow(self.interrupt_shadow);

        for (dest, src) in it.zip([
            HvRegisterValue::from(u64::from(activity)),
            u64::from(pending_interruption).into(),
            u64::from(interrupt_state).into(),
            pending_event.into(),
        ]) {
            *dest = src;
        }
    }

    fn set_values(&mut self, mut it: impl Iterator<Item = HvRegisterValue>) {
        let activity = HvInternalActivityRegister::from(it.next().unwrap().as_u64());
        let interruption = HvX64PendingInterruptionRegister::from(it.next().unwrap().as_u64());
        let interrupt_state = HvX64InterruptStateRegister::from(it.next().unwrap().as_u64());
        let event = HvX64PendingEventReg0::from(it.next().unwrap().as_u128());

        let mp_state = if activity.startup_suspend() {
            MpState::WaitForSipi
        } else if activity.halt_suspend() {
            MpState::Halted
        } else if activity.idle_suspend() {
            MpState::Idle
        } else {
            MpState::Running
        };

        let pending_event = event.event_pending().then(|| match event.event_type() {
            hvdef::HV_X64_PENDING_EVENT_EXCEPTION => {
                let event = HvX64PendingExceptionEvent::from(u128::from(event));
                PendingEvent::Exception {
                    vector: event.vector().try_into().expect("exception code is 8 bits"),
                    error_code: event.deliver_error_code().then(|| event.error_code()),
                    parameter: event.exception_parameter(),
                }
            }
            hvdef::HV_X64_PENDING_EVENT_EXT_INT => {
                let event = HvX64PendingExtIntEvent::from(u128::from(event));
                PendingEvent::ExtInt {
                    vector: event.vector(),
                }
            }
            ty => panic!("unhandled event type: {}", ty),
        });

        let pending_interruption = interruption.interruption_pending().then(|| {
            match HvX64PendingInterruptionType(interruption.interruption_type()) {
                HvX64PendingInterruptionType::HV_X64_PENDING_INTERRUPT => {
                    PendingInterruption::Interrupt {
                        vector: interruption
                            .interruption_vector()
                            .try_into()
                            .expect("x86 vector is 8 bits"),
                    }
                }
                HvX64PendingInterruptionType::HV_X64_PENDING_NMI => PendingInterruption::Nmi,
                HvX64PendingInterruptionType::HV_X64_PENDING_EXCEPTION => {
                    PendingInterruption::Exception {
                        vector: interruption
                            .interruption_vector()
                            .try_into()
                            .expect("exception code is 8 bits"),
                        error_code: interruption
                            .deliver_error_code()
                            .then(|| interruption.error_code()),
                    }
                }
                ty => panic!("unhandled interruption type: {ty:?}"),
            }
        });

        *self = Self {
            mp_state,
            nmi_pending: false,
            nmi_masked: interrupt_state.nmi_masked(),
            interrupt_shadow: interrupt_state.interrupt_shadow(),
            pending_event,
            pending_interruption,
        };
    }
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for Activity {
    fn is_present(_caps: &X86PartitionCapabilities) -> bool {
        true
    }

    fn at_reset(_caps: &X86PartitionCapabilities, vp_info: &X86VpInfo) -> Self {
        let mp_state = if vp_info.base.is_bsp() {
            MpState::Running
        } else {
            // FUTURE: we should really emulate INIT and SIPI to have
            // finer-grained control over the states.
            MpState::WaitForSipi
        };
        Self {
            mp_state,
            nmi_pending: false,
            nmi_masked: false,
            interrupt_shadow: false,
            pending_event: None,
            pending_interruption: None,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Copy, Clone, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
#[inspect(external_tag)]
pub enum PendingEvent {
    #[mesh(1)]
    Exception {
        #[mesh(1)]
        vector: u8,
        #[mesh(2)]
        error_code: Option<u32>,
        #[mesh(3)]
        parameter: u64,
    },
    #[mesh(2)]
    ExtInt {
        #[mesh(1)]
        vector: u8,
    },
}

#[derive(Debug, PartialEq, Eq, Copy, Clone, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
#[inspect(external_tag)]
pub enum PendingInterruption {
    #[mesh(1)]
    Exception {
        #[mesh(1)]
        vector: u8,
        #[mesh(2)]
        error_code: Option<u32>,
    },
    #[mesh(2)]
    Interrupt {
        #[mesh(1)]
        vector: u8,
    },
    #[mesh(3)]
    Nmi,
}

#[derive(Debug, Default, PartialEq, Eq, Copy, Clone, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct DebugRegisters {
    #[mesh(1)]
    #[inspect(hex)]
    pub dr0: u64,
    #[mesh(2)]
    #[inspect(hex)]
    pub dr1: u64,
    #[mesh(3)]
    #[inspect(hex)]
    pub dr2: u64,
    #[mesh(4)]
    #[inspect(hex)]
    pub dr3: u64,
    #[mesh(5)]
    #[inspect(hex)]
    pub dr6: u64,
    #[mesh(6)]
    #[inspect(hex)]
    pub dr7: u64,
}

impl HvRegisterState<HvX64RegisterName, 6> for DebugRegisters {
    fn names(&self) -> &'static [HvX64RegisterName; 6] {
        &[
            HvX64RegisterName::Dr0,
            HvX64RegisterName::Dr1,
            HvX64RegisterName::Dr2,
            HvX64RegisterName::Dr3,
            HvX64RegisterName::Dr6,
            HvX64RegisterName::Dr7,
        ]
    }

    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>) {
        for (dest, src) in it.zip([
            self.dr0.into(),
            self.dr1.into(),
            self.dr2.into(),
            self.dr3.into(),
            self.dr6.into(),
            self.dr7.into(),
        ]) {
            *dest = src;
        }
    }

    fn set_values(&mut self, it: impl Iterator<Item = HvRegisterValue>) {
        for (src, dest) in it.zip([
            &mut self.dr0,
            &mut self.dr1,
            &mut self.dr2,
            &mut self.dr3,
            &mut self.dr6,
            &mut self.dr7,
        ]) {
            *dest = src.as_u64();
        }
    }
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for DebugRegisters {
    fn is_present(_caps: &X86PartitionCapabilities) -> bool {
        true
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self {
            dr0: 0,
            dr1: 0,
            dr2: 0,
            dr3: 0,
            dr6: 0xffff0ff0,
            dr7: 0x400,
        }
    }

    fn can_compare(caps: &X86PartitionCapabilities) -> bool {
        // Some machines support clearing bit 16 for some TSX debugging feature,
        // but the hypervisor does not support restoring DR6 into this state.
        // Ignore comparison failures in this case.
        !caps.dr6_tsx_broken
    }
}

#[derive(PartialEq, Eq, Protobuf)]
#[mesh(package = "virt.x86")]
pub struct Xsave {
    #[mesh(1)]
    pub data: Vec<u64>,
}

impl Xsave {
    fn normalize(&mut self) {
        let (mut fxsave, data) = Ref::<_, Fxsave>::from_prefix(self.data.as_mut_bytes()).unwrap();
        let header = XsaveHeader::mut_from_prefix(data).unwrap().0; // TODO: zerocopy: ref-from-prefix: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)

        // Clear the mxcsr mask since it's ignored in the restore process and
        // will only cause xsave comparisons to fail.
        fxsave.mxcsr_mask = 0;

        // Clear SSE state if it's not actually set to anything interesting.
        // This normalizes behavior between mshv (which always sets SSE in
        // xstate_bv) and KVM (which does not).
        if header.xstate_bv & XFEATURE_SSE != 0 {
            if fxsave.xmm.iter().eq(std::iter::repeat_n(&[0; 16], 16))
                && fxsave.mxcsr == DEFAULT_MXCSR
            {
                header.xstate_bv &= !XFEATURE_SSE;
            }
        } else {
            fxsave.xmm.fill(Default::default());
        }

        if header.xstate_bv & (XFEATURE_SSE | XFEATURE_YMM) == 0 {
            fxsave.mxcsr = 0;
        }

        // Clear init FPU state as well.
        if header.xstate_bv & XFEATURE_X87 != 0 {
            if fxsave.fcw == INIT_FCW
                && fxsave.fsw == 0
                && fxsave.ftw == 0
                && fxsave.fop == 0
                && fxsave.fip == 0
                && fxsave.fdp == 0
                && fxsave.st == [[0; 16]; 8]
            {
                fxsave.fcw = 0;
                header.xstate_bv &= !XFEATURE_X87;
            }
        } else {
            fxsave.fcw = 0;
            fxsave.fsw = 0;
            fxsave.ftw = 0;
            fxsave.fop = 0;
            fxsave.fip = 0;
            fxsave.fdp = 0;
            fxsave.st.fill(Default::default());
        }

        // Clear the portion of the xsave legacy region that's specified to not
        // to be used by the processor. Never versions of KVM put garbage values
        // in here for some (possibly incorrect) reason.
        fxsave.unused.fill(0);
    }

    /// Construct from the xsave compact format.
    pub fn from_compact(data: &[u8], caps: &X86PartitionCapabilities) -> Self {
        assert_eq!(data.len() % 8, 0);
        let mut aligned = vec![0; data.len() / 8];
        aligned.as_mut_bytes().copy_from_slice(data);
        let mut this = Self { data: aligned };

        this.normalize();

        // Some versions of the MS hypervisor fail to set xstate_bv for
        // supervisor states. In this case, force-enable them--this is always
        // safe (since their init state == zero) and does not have a performance
        // penalty.
        if caps.xsaves_state_bv_broken {
            let header =
                XsaveHeader::mut_from_prefix(&mut this.data.as_mut_bytes()[XSAVE_LEGACY_LEN..])
                    .unwrap()
                    .0; // TODO: zerocopy: ref-from-prefix: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)

            // Just enable supervisor states that were possible when the
            // hypervisor had the bug. Future ones will only be supported by
            // fixed hypervisors.
            header.xstate_bv |= header.xcomp_bv & 0x1c00;
        }

        this
    }

    /// Construct from standard (non-compact) xsave format.
    pub fn from_standard(src: &[u8], caps: &X86PartitionCapabilities) -> Self {
        let mut this = Self {
            data: vec![0; caps.xsave.compact_len as usize / 8],
        };
        this.data.as_mut_bytes()[..XSAVE_VARIABLE_OFFSET]
            .copy_from_slice(&src[..XSAVE_VARIABLE_OFFSET]);

        let (mut header, data) =
            Ref::<_, XsaveHeader>::from_prefix(&mut this.data.as_mut_bytes()[XSAVE_LEGACY_LEN..])
                .unwrap();

        header.xcomp_bv = caps.xsave.features | caps.xsave.supervisor_features | XCOMP_COMPRESSED;
        let mut cur = 0;
        for i in 2..63 {
            if header.xcomp_bv & (1 << i) != 0 {
                let feature = &caps.xsave.feature_info[i];
                let offset = feature.offset as usize;
                let len = feature.len as usize;
                if feature.align {
                    cur = (cur + 63) & !63;
                }
                if header.xstate_bv & (1 << i) != 0 {
                    data[cur..cur + len].copy_from_slice(&src[offset..offset + len]);
                }
                cur += len;
            }
        }
        this.normalize();
        this
    }

    /// Write out to standard (non-compact) xsave format.
    pub fn write_standard(&self, data: &mut [u8], caps: &X86PartitionCapabilities) {
        // Copy the legacy region including default values for disabled features.
        data[..XSAVE_LEGACY_LEN].copy_from_slice(self.fxsave().as_bytes());

        // Copy the xsave header but clear xcomp_bv.
        let header = self.xsave_header();
        data[XSAVE_LEGACY_LEN..XSAVE_VARIABLE_OFFSET].copy_from_slice(
            XsaveHeader {
                xcomp_bv: 0,
                ..*header
            }
            .as_bytes(),
        );

        // Copy the features.
        let mut cur = XSAVE_VARIABLE_OFFSET;
        for i in 2..63 {
            if header.xcomp_bv & (1 << i) != 0 {
                let feature = &caps.xsave.feature_info[i];
                let offset = feature.offset as usize;
                let len = feature.len as usize;
                if feature.align {
                    cur = (cur + 63) & !63;
                }
                if header.xstate_bv & (1 << i) != 0 {
                    data[offset..offset + len]
                        .copy_from_slice(&self.data.as_bytes()[cur..cur + len]);
                }
                cur += len;
            }
        }
    }

    /// Returns the compact form.
    pub fn compact(&self) -> &[u8] {
        self.data.as_bytes()
    }

    /// Returns the legacy fxsave state only.
    ///
    /// Since this does not include `xstate_bv`, fields for disabled features
    /// will be set to their default values.
    pub fn fxsave(&self) -> Fxsave {
        let mut fxsave = Fxsave::read_from_prefix(self.data.as_bytes()).unwrap().0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let header = self.xsave_header();
        if header.xstate_bv & XFEATURE_X87 == 0 {
            fxsave.fcw = INIT_FCW;
        }
        if header.xstate_bv & (XFEATURE_SSE | XFEATURE_YMM) == 0 {
            fxsave.mxcsr = DEFAULT_MXCSR;
        }
        fxsave
    }

    fn xsave_header(&self) -> &XsaveHeader {
        XsaveHeader::ref_from_prefix(&self.data.as_bytes()[XSAVE_LEGACY_LEN..])
            .unwrap()
            .0 // TODO: zerocopy: ref-from-prefix: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }
}

impl Debug for Xsave {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Xsave")
            .field("legacy", &format_args!("{:x?}", self.fxsave()))
            .field("header", &format_args!("{:x?}", self.xsave_header()))
            .field("data", &&self.data[XSAVE_VARIABLE_OFFSET / 8..])
            .finish()
    }
}

impl Inspect for Xsave {
    fn inspect(&self, req: inspect::Request<'_>) {
        let Fxsave {
            fcw,
            fsw,
            ftw,
            reserved: _,
            fop,
            fip,
            fdp,
            mxcsr,
            mxcsr_mask,
            st,
            xmm,
            reserved2: _,
            unused: _,
        } = self.fxsave();

        let &XsaveHeader {
            xstate_bv,
            xcomp_bv,
            reserved: _,
        } = self.xsave_header();

        let mut resp = req.respond();
        resp.hex("fcw", fcw)
            .hex("fsw", fsw)
            .hex("ftw", ftw)
            .hex("fop", fop)
            .hex("fip", fip)
            .hex("fdp", fdp)
            .hex("mxcsr", mxcsr)
            .hex("mxcsr_mask", mxcsr_mask)
            .hex("xstate_bv", xstate_bv)
            .hex("xcomp_bv", xcomp_bv);

        for (st, name) in st
            .iter()
            .zip(["st0", "st1", "st2", "st3", "st4", "st5", "st6", "st7"])
        {
            resp.field(name, st);
        }

        for (xmm, name) in xmm.iter().zip([
            "xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5", "xmm6", "xmm7", "xmm8", "xmm9",
            "xmm10", "xmm11", "xmm12", "xmm13", "xmm14", "xmm15",
        ]) {
            resp.field(name, xmm);
        }
    }
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for Xsave {
    fn is_present(_caps: &X86PartitionCapabilities) -> bool {
        true
    }

    fn at_reset(caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        let mut data = vec![0; caps.xsave.compact_len as usize];
        *XsaveHeader::mut_from_prefix(&mut data[XSAVE_LEGACY_LEN..])
            .unwrap()
            .0 = XsaveHeader {
            // TODO: zerocopy: ref-from-prefix: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
            xstate_bv: 0,
            xcomp_bv: XCOMP_COMPRESSED | caps.xsave.features | caps.xsave.supervisor_features,
            reserved: [0; 6],
        };
        Self::from_compact(&data, caps)
    }
}

#[derive(PartialEq, Eq, Clone, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
#[inspect(hex)]
pub struct Apic {
    #[mesh(1)]
    pub apic_base: u64,
    #[inspect(with = "ApicRegisters::from")]
    #[mesh(2)]
    pub registers: [u32; 64],
    #[inspect(iter_by_index)]
    #[mesh(3)]
    pub auto_eoi: [u32; 8],
}

impl Debug for Apic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            apic_base,
            registers,
            auto_eoi,
        } = self;
        f.debug_struct("Apic")
            .field("apic_base", &format_args!("{:#x}", apic_base))
            .field("registers", &format_args!("{:#x?}", registers))
            .field("registers", &format_args!("{:#x?}", auto_eoi))
            .finish()
    }
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
#[inspect(hex)]
pub struct ApicRegisters {
    #[inspect(skip)]
    pub reserved_0: [u32; 2],
    pub id: u32,
    pub version: u32,
    #[inspect(skip)]
    pub reserved_4: [u32; 4],
    pub tpr: u32, // Task Priority Register
    pub apr: u32, // Arbitration Priority Register
    pub ppr: u32, // Processor Priority Register
    pub eoi: u32, //
    pub rrd: u32, // Remote Read Register
    pub ldr: u32, // Logical Destination Register
    pub dfr: u32, // Destination Format Register
    pub svr: u32, // Spurious Interrupt Vector
    #[inspect(iter_by_index)]
    pub isr: [u32; 8], // In-Service Register
    #[inspect(iter_by_index)]
    pub tmr: [u32; 8], // Trigger Mode Register
    #[inspect(iter_by_index)]
    pub irr: [u32; 8], // Interrupt Request Register
    pub esr: u32, // Error Status Register
    #[inspect(skip)]
    pub reserved_29: [u32; 6],
    pub lvt_cmci: u32,
    #[inspect(iter_by_index)]
    pub icr: [u32; 2], // Interrupt Command Register
    pub lvt_timer: u32,
    pub lvt_thermal: u32,
    pub lvt_pmc: u32,
    pub lvt_lint0: u32,
    pub lvt_lint1: u32,
    pub lvt_error: u32,
    pub timer_icr: u32, // Initial Count Register
    pub timer_ccr: u32, // Current Count Register
    #[inspect(skip)]
    pub reserved_3a: [u32; 4],
    pub timer_dcr: u32, // Divide Configuration Register
    #[inspect(skip)]
    pub reserved_3f: u32,
}

const _: () = assert!(size_of::<ApicRegisters>() == 0x100);

impl From<&'_ [u32; 64]> for ApicRegisters {
    fn from(value: &'_ [u32; 64]) -> Self {
        Self::read_from_bytes(value.as_bytes()).unwrap()
    }
}

impl From<ApicRegisters> for [u32; 64] {
    fn from(value: ApicRegisters) -> Self {
        Self::read_from_bytes(value.as_bytes()).unwrap()
    }
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct ApicRegister {
    value: u32,
    zero: [u32; 3],
}

// The IRR bit number corresponding to NMI pending in the Hyper-V exo APIC saved
// state.
const HV_IRR_NMI_PENDING_SHIFT: u32 = 2;

impl Apic {
    pub fn as_page(&self) -> [u8; 1024] {
        let mut bytes = [0; 1024];
        self.registers
            .map(|value| ApicRegister {
                value,
                zero: [0; 3],
            })
            .write_to(bytes.as_mut_slice())
            .unwrap();
        bytes
    }

    /// Convert from an APIC page.
    ///
    /// N.B. The MS hypervisor's APIC page format includes a non-architectural
    /// NMI pending bit that should be stripped first.
    pub fn from_page(apic_base: u64, page: &[u8; 1024]) -> Self {
        let registers = <[ApicRegister; 64]>::read_from_bytes(page.as_slice()).unwrap();
        Self {
            apic_base,
            registers: registers.map(|reg| reg.value),
            auto_eoi: [0; 8],
        }
    }
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for Apic {
    fn is_present(_caps: &X86PartitionCapabilities) -> bool {
        true
    }

    fn at_reset(caps: &X86PartitionCapabilities, vp_info: &X86VpInfo) -> Self {
        let x2apic = caps.x2apic_enabled;

        let mut regs = ApicRegisters::new_zeroed();
        regs.id = if x2apic {
            vp_info.apic_id
        } else {
            vp_info.apic_id << 24
        };
        regs.version = ApicVersion::new()
            .with_version(0x14)
            .with_max_lvt_entry(5)
            .into();
        if x2apic {
            regs.ldr = ((vp_info.apic_id << 12) & 0xffff0000) | (1 << (vp_info.apic_id & 0xf));
        } else {
            regs.dfr = !0;
        }
        regs.svr = 0xff;
        regs.lvt_timer = 0x10000;
        regs.lvt_thermal = 0x10000;
        regs.lvt_pmc = 0x10000;
        regs.lvt_lint0 = 0x10000;
        regs.lvt_lint1 = 0x10000;
        regs.lvt_error = 0x10000;

        let apic_base = ApicBase::new()
            .with_base_page(APIC_BASE_PAGE)
            .with_bsp(vp_info.base.is_bsp())
            .with_x2apic(x2apic)
            .with_enable(true);

        Apic {
            apic_base: apic_base.into(),
            registers: regs.into(),
            auto_eoi: [0; 8],
        }
    }

    fn can_compare(caps: &X86PartitionCapabilities) -> bool {
        // If a partition (ie KVM) cannot freeze time, one of the APIC timer values will continue counting up after restore.
        // For now, disallow comparing the whole Apic structure if so.
        caps.can_freeze_time
    }
}

/// Sets the non-architectural Hyper-V NMI pending bit in the APIC page.
pub fn set_hv_apic_nmi_pending(page: &mut [u8], pending: bool) {
    page[0x200] &= !(1 << HV_IRR_NMI_PENDING_SHIFT);
    page[0x200] |= (pending as u8) << HV_IRR_NMI_PENDING_SHIFT;
}

/// Gets the non-architectural Hyper-V NMI pending bit from the APIC page.
pub fn hv_apic_nmi_pending(page: &[u8]) -> bool {
    page[0x200] & (1 << HV_IRR_NMI_PENDING_SHIFT) != 0
}

#[derive(Debug, Default, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct Xcr0 {
    #[mesh(1)]
    #[inspect(hex)]
    pub value: u64,
}

impl HvRegisterState<HvX64RegisterName, 1> for Xcr0 {
    fn names(&self) -> &'static [HvX64RegisterName; 1] {
        &[HvX64RegisterName::Xfem]
    }

    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>) {
        for (dest, src) in it.zip([self.value]) {
            *dest = src.into();
        }
    }

    fn set_values(&mut self, it: impl Iterator<Item = HvRegisterValue>) {
        for (src, dest) in it.zip([&mut self.value]) {
            *dest = src.as_u64();
        }
    }
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for Xcr0 {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.xsave.features != 0
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self { value: 1 }
    }
}

#[derive(Debug, Default, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct Xss {
    #[mesh(1)]
    #[inspect(hex)]
    pub value: u64,
}

impl HvRegisterState<HvX64RegisterName, 1> for Xss {
    fn names(&self) -> &'static [HvX64RegisterName; 1] {
        &[HvX64RegisterName::Xss]
    }

    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>) {
        for (dest, src) in it.zip([self.value]) {
            *dest = src.into();
        }
    }

    fn set_values(&mut self, it: impl Iterator<Item = HvRegisterValue>) {
        for (src, dest) in it.zip([&mut self.value]) {
            *dest = src.as_u64();
        }
    }
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for Xss {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.xsave.supervisor_features != 0
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self { value: 0 }
    }
}

#[repr(C)]
#[derive(Default, Debug, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct Pat {
    #[mesh(1)]
    #[inspect(hex)]
    pub value: u64,
}

impl HvRegisterState<HvX64RegisterName, 1> for Pat {
    fn names(&self) -> &'static [HvX64RegisterName; 1] {
        &[HvX64RegisterName::Pat]
    }

    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>) {
        for (dest, src) in it.zip([self.value]) {
            *dest = src.into();
        }
    }

    fn set_values(&mut self, it: impl Iterator<Item = HvRegisterValue>) {
        for (src, dest) in it.zip([&mut self.value]) {
            *dest = src.as_u64();
        }
    }
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for Pat {
    fn is_present(_caps: &X86PartitionCapabilities) -> bool {
        true
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self {
            value: X86X_MSR_DEFAULT_PAT,
        }
    }
}

#[repr(C)]
#[derive(Default, Debug, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
#[inspect(hex)]
pub struct Mtrrs {
    #[mesh(1)]
    #[inspect(hex)]
    pub msr_mtrr_def_type: u64,
    #[mesh(2)]
    #[inspect(iter_by_index)]
    pub fixed: [u64; 11],
    #[mesh(3)]
    #[inspect(iter_by_index)]
    pub variable: [u64; 16],
}

impl HvRegisterState<HvX64RegisterName, 28> for Mtrrs {
    fn names(&self) -> &'static [HvX64RegisterName; 28] {
        &[
            HvX64RegisterName::MsrMtrrDefType,
            HvX64RegisterName::MsrMtrrFix64k00000,
            HvX64RegisterName::MsrMtrrFix16k80000,
            HvX64RegisterName::MsrMtrrFix16kA0000,
            HvX64RegisterName::MsrMtrrFix4kC0000,
            HvX64RegisterName::MsrMtrrFix4kC8000,
            HvX64RegisterName::MsrMtrrFix4kD0000,
            HvX64RegisterName::MsrMtrrFix4kD8000,
            HvX64RegisterName::MsrMtrrFix4kE0000,
            HvX64RegisterName::MsrMtrrFix4kE8000,
            HvX64RegisterName::MsrMtrrFix4kF0000,
            HvX64RegisterName::MsrMtrrFix4kF8000,
            HvX64RegisterName::MsrMtrrPhysBase0,
            HvX64RegisterName::MsrMtrrPhysMask0,
            HvX64RegisterName::MsrMtrrPhysBase1,
            HvX64RegisterName::MsrMtrrPhysMask1,
            HvX64RegisterName::MsrMtrrPhysBase2,
            HvX64RegisterName::MsrMtrrPhysMask2,
            HvX64RegisterName::MsrMtrrPhysBase3,
            HvX64RegisterName::MsrMtrrPhysMask3,
            HvX64RegisterName::MsrMtrrPhysBase4,
            HvX64RegisterName::MsrMtrrPhysMask4,
            HvX64RegisterName::MsrMtrrPhysBase5,
            HvX64RegisterName::MsrMtrrPhysMask5,
            HvX64RegisterName::MsrMtrrPhysBase6,
            HvX64RegisterName::MsrMtrrPhysMask6,
            HvX64RegisterName::MsrMtrrPhysBase7,
            HvX64RegisterName::MsrMtrrPhysMask7,
        ]
    }

    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>) {
        for (dest, src) in it.zip(
            [self.msr_mtrr_def_type]
                .into_iter()
                .chain(self.fixed)
                .chain(self.variable),
        ) {
            *dest = src.into();
        }
    }

    fn set_values(&mut self, it: impl Iterator<Item = HvRegisterValue>) {
        for (src, dest) in it.zip(
            [&mut self.msr_mtrr_def_type]
                .into_iter()
                .chain(&mut self.fixed)
                .chain(&mut self.variable),
        ) {
            *dest = src.as_u64();
        }
    }
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for Mtrrs {
    fn is_present(_caps: &X86PartitionCapabilities) -> bool {
        true
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self {
            msr_mtrr_def_type: 0,
            fixed: [0; 11],
            variable: [0; 16],
        }
    }
}

#[repr(C)]
#[derive(Default, Debug, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
#[inspect(hex)]
pub struct VirtualMsrs {
    #[mesh(1)]
    pub kernel_gs_base: u64,
    #[mesh(2)]
    pub sysenter_cs: u64,
    #[mesh(3)]
    pub sysenter_eip: u64,
    #[mesh(4)]
    pub sysenter_esp: u64,
    #[mesh(5)]
    pub star: u64,
    #[mesh(6)]
    pub lstar: u64,
    #[mesh(7)]
    pub cstar: u64,
    #[mesh(8)]
    pub sfmask: u64,
}

impl HvRegisterState<HvX64RegisterName, 8> for VirtualMsrs {
    fn names(&self) -> &'static [HvX64RegisterName; 8] {
        &[
            HvX64RegisterName::KernelGsBase,
            HvX64RegisterName::SysenterCs,
            HvX64RegisterName::SysenterEsp,
            HvX64RegisterName::SysenterEip,
            HvX64RegisterName::Star,
            HvX64RegisterName::Lstar,
            HvX64RegisterName::Cstar,
            HvX64RegisterName::Sfmask,
        ]
    }

    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>) {
        for (dest, src) in it.zip([
            self.kernel_gs_base,
            self.sysenter_cs,
            self.sysenter_eip,
            self.sysenter_esp,
            self.star,
            self.lstar,
            self.cstar,
            self.sfmask,
        ]) {
            *dest = src.into();
        }
    }

    fn set_values(&mut self, it: impl Iterator<Item = HvRegisterValue>) {
        for (src, dest) in it.zip([
            &mut self.kernel_gs_base,
            &mut self.sysenter_cs,
            &mut self.sysenter_eip,
            &mut self.sysenter_esp,
            &mut self.star,
            &mut self.lstar,
            &mut self.cstar,
            &mut self.sfmask,
        ]) {
            *dest = src.as_u64();
        }
    }
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for VirtualMsrs {
    fn is_present(_caps: &X86PartitionCapabilities) -> bool {
        true
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self {
            kernel_gs_base: 0,
            sysenter_cs: 0,
            sysenter_eip: 0,
            sysenter_esp: 0,
            star: 0,
            lstar: 0,
            cstar: 0,
            sfmask: 0,
        }
    }
}

#[repr(C)]
#[derive(Default, Debug, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct TscAux {
    #[mesh(1)]
    #[inspect(hex)]
    pub value: u64,
}

impl HvRegisterState<HvX64RegisterName, 1> for TscAux {
    fn names(&self) -> &'static [HvX64RegisterName; 1] {
        &[HvX64RegisterName::TscAux]
    }

    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>) {
        for (dest, src) in it.zip([self.value]) {
            *dest = src.into();
        }
    }

    fn set_values(&mut self, it: impl Iterator<Item = HvRegisterValue>) {
        for (src, dest) in it.zip([&mut self.value]) {
            *dest = src.as_u64();
        }
    }
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for TscAux {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.tsc_aux
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Default::default()
    }
}

#[repr(C)]
#[derive(Default, Debug, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct Tsc {
    #[mesh(1)]
    #[inspect(hex)]
    pub value: u64,
}

impl HvRegisterState<HvX64RegisterName, 1> for Tsc {
    fn names(&self) -> &'static [HvX64RegisterName; 1] {
        &[HvX64RegisterName::Tsc]
    }

    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>) {
        for (dest, src) in it.zip([self.value]) {
            *dest = src.into();
        }
    }

    fn set_values(&mut self, it: impl Iterator<Item = HvRegisterValue>) {
        for (src, dest) in it.zip([&mut self.value]) {
            *dest = src.as_u64();
        }
    }
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for Tsc {
    fn is_present(_caps: &X86PartitionCapabilities) -> bool {
        true
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self { value: 0 }
    }

    fn can_compare(caps: &X86PartitionCapabilities) -> bool {
        caps.can_freeze_time
    }
}

#[repr(C)]
#[derive(Default, Debug, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct Cet {
    #[mesh(1)]
    #[inspect(hex)]
    pub scet: u64,
    // Ucet is part of xsave state.
}

impl HvRegisterState<HvX64RegisterName, 1> for Cet {
    fn names(&self) -> &'static [HvX64RegisterName; 1] {
        &[HvX64RegisterName::SCet]
    }

    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>) {
        for (dest, src) in it.zip([self.scet]) {
            *dest = src.into();
        }
    }

    fn set_values(&mut self, it: impl Iterator<Item = HvRegisterValue>) {
        for (src, dest) in it.zip([&mut self.scet]) {
            *dest = src.as_u64();
        }
    }
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for Cet {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.cet
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self { scet: 0 }
    }
}

#[repr(C)]
#[derive(Default, Debug, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
#[inspect(hex)]
pub struct CetSs {
    #[mesh(1)]
    pub ssp: u64,
    #[mesh(2)]
    pub interrupt_ssp_table_addr: u64,
    // Plx_ssp are part of xsave state.
}

impl HvRegisterState<HvX64RegisterName, 2> for CetSs {
    fn names(&self) -> &'static [HvX64RegisterName; 2] {
        &[
            HvX64RegisterName::Ssp,
            HvX64RegisterName::InterruptSspTableAddr,
        ]
    }

    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>) {
        for (dest, src) in it.zip([self.ssp, self.interrupt_ssp_table_addr]) {
            *dest = src.into();
        }
    }

    fn set_values(&mut self, it: impl Iterator<Item = HvRegisterValue>) {
        for (src, dest) in it.zip([&mut self.ssp, &mut self.interrupt_ssp_table_addr]) {
            *dest = src.as_u64();
        }
    }
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for CetSs {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.cet_ss
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Default::default()
    }
}

#[repr(C)]
#[derive(Debug, Default, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
#[inspect(hex)]
pub struct SyntheticMsrs {
    #[mesh(1)]
    pub vp_assist_page: u64,
    #[mesh(2)]
    pub scontrol: u64,
    #[mesh(3)]
    pub siefp: u64,
    #[mesh(4)]
    pub simp: u64,
    #[mesh(5)]
    #[inspect(iter_by_index)]
    pub sint: [u64; 16],
}

impl HvRegisterState<HvX64RegisterName, 20> for SyntheticMsrs {
    fn names(&self) -> &'static [HvX64RegisterName; 20] {
        &[
            HvX64RegisterName::VpAssistPage,
            HvX64RegisterName::Scontrol,
            HvX64RegisterName::Sifp,
            HvX64RegisterName::Sipp,
            HvX64RegisterName::Sint0,
            HvX64RegisterName::Sint1,
            HvX64RegisterName::Sint2,
            HvX64RegisterName::Sint3,
            HvX64RegisterName::Sint4,
            HvX64RegisterName::Sint5,
            HvX64RegisterName::Sint6,
            HvX64RegisterName::Sint7,
            HvX64RegisterName::Sint8,
            HvX64RegisterName::Sint9,
            HvX64RegisterName::Sint10,
            HvX64RegisterName::Sint11,
            HvX64RegisterName::Sint12,
            HvX64RegisterName::Sint13,
            HvX64RegisterName::Sint14,
            HvX64RegisterName::Sint15,
        ]
    }
    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>) {
        for (dest, src) in it.zip(
            [self.vp_assist_page, self.scontrol, self.siefp, self.simp]
                .into_iter()
                .chain(self.sint),
        ) {
            *dest = src.into();
        }
    }

    fn set_values(&mut self, it: impl Iterator<Item = HvRegisterValue>) {
        for (src, dest) in it.zip(
            [
                &mut self.vp_assist_page,
                &mut self.scontrol,
                &mut self.siefp,
                &mut self.simp,
            ]
            .into_iter()
            .chain(&mut self.sint),
        ) {
            *dest = src.as_u64();
        }
    }
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for SyntheticMsrs {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.hv1
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self {
            vp_assist_page: 0,
            scontrol: 1,
            siefp: 0,
            simp: 0,
            sint: [0x10000; 16],
        }
    }
}

#[derive(Default, Debug, Copy, Clone, Eq, PartialEq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
#[inspect(hex)]
pub struct SynicTimer {
    #[mesh(1)]
    pub config: u64,
    #[mesh(2)]
    pub count: u64,
    #[mesh(3)]
    pub adjustment: u64,
    #[mesh(4)]
    pub undelivered_message_expiration_time: Option<u64>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct SynicTimers {
    #[mesh(1)]
    #[inspect(iter_by_index)]
    pub timers: [SynicTimer; 4],
}

impl SynicTimers {
    pub fn as_hv(&self) -> hvdef::HvSyntheticTimersState {
        let timers = self.timers.map(|timer| hvdef::HvStimerState {
            undelivered_message_pending: timer.undelivered_message_expiration_time.is_some().into(),
            reserved: 0,
            config: timer.config,
            count: timer.count,
            adjustment: timer.adjustment,
            undelivered_expiration_time: timer.undelivered_message_expiration_time.unwrap_or(0),
        });

        hvdef::HvSyntheticTimersState {
            timers,
            reserved: [0; 5],
        }
    }

    pub fn from_hv(state: hvdef::HvSyntheticTimersState) -> Self {
        let timers = state.timers.map(|timer| SynicTimer {
            config: timer.config,
            count: timer.count,
            adjustment: timer.adjustment,
            undelivered_message_expiration_time: (timer.undelivered_message_pending & 1 != 0)
                .then_some(timer.undelivered_expiration_time),
        });
        Self { timers }
    }
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for SynicTimers {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.hv1
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self {
            timers: [SynicTimer::default(); 4],
        }
    }

    fn can_compare(_caps: &X86PartitionCapabilities) -> bool {
        // These can't be compared, since the hypervisor may choose to
        // immediately deliver the undelivered message.
        false
    }
}

#[derive(Debug, Default, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct SynicMessageQueues {
    #[mesh(1)]
    #[inspect(with = "|x| inspect::iter_by_index(x.iter().map(Vec::len))")]
    pub queues: [Vec<[u8; HV_MESSAGE_SIZE]>; 16],
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for SynicMessageQueues {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.hv1
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Default::default()
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
#[inspect(skip)]
pub struct SynicMessagePage {
    #[mesh(1)]
    pub data: [u8; 4096],
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for SynicMessagePage {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.hv1
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self { data: [0; 4096] }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
#[inspect(skip)]
pub struct SynicEventFlagsPage {
    #[mesh(1)]
    pub data: [u8; 4096],
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for SynicEventFlagsPage {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.hv1
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self { data: [0; 4096] }
    }
}

state_trait! {
    "Per-VP state",
    AccessVpState,
    X86PartitionCapabilities,
    X86VpInfo,
    VpSavedState,
    "virt.x86",
    (1, "registers", registers, set_registers, Registers),
    (2, "activity", activity, set_activity, Activity),
    (3, "xsave", xsave, set_xsave, Xsave),
    (4, "apic", apic, set_apic, Apic),
    (5, "xcr", xcr, set_xcr, Xcr0),
    (6, "xss", xss, set_xss, Xss),
    (7, "mtrrs", mtrrs, set_mtrrs, Mtrrs),
    (8, "pat", pat, set_pat, Pat),
    (9, "msrs", virtual_msrs, set_virtual_msrs, VirtualMsrs),
    (10, "drs", debug_regs, set_debug_regs, DebugRegisters),
    (11, "tsc", tsc, set_tsc, Tsc),
    (12, "cet", cet, set_cet, Cet),
    (13, "cet_ss", cet_ss, set_cet_ss, CetSs),
    (14, "tsc_aux", tsc_aux, set_tsc_aux, TscAux),

    // Synic state
    (100, "synic", synic_msrs, set_synic_msrs, SyntheticMsrs),
    // The simp page contents must come after synic MSRs so that the SIMP page
    // register is set, but before the message queues and timers in case the
    // hypervisor decides to flush a pending message to the message page during
    // restore.
    (
        101,
        "simp",
        synic_message_page,
        set_synic_message_page,
        SynicMessagePage
    ),
    (
        102,
        "siefp",
        synic_event_flags_page,
        set_synic_event_flags_page,
        SynicEventFlagsPage
    ),
    (
        103,
        "synic_message_queues",
        synic_message_queues,
        set_synic_message_queues,
        SynicMessageQueues
    ),
    (104, "synic_timers", synic_timers, set_synic_timers, SynicTimers),
}

/// Resets register state for an x86 INIT via the APIC.
pub fn x86_init<T: AccessVpState>(access: &mut T, vp_info: &X86VpInfo) -> Result<(), T::Error> {
    // Reset core register and debug register state, but preserve a few bits of cr0.
    let cr0 = access.registers()?.cr0;
    let mut regs = Registers::at_reset(access.caps(), vp_info);
    let cr0_mask = X64_CR0_NW | X64_CR0_CD;
    regs.cr0 = (cr0 & cr0_mask) | (regs.cr0 & !cr0_mask);
    access.set_registers(&regs)?;
    access.set_debug_regs(&StateElement::at_reset(access.caps(), vp_info))?;

    // Reset the APIC state, leaving the APIC base address and APIC ID intact.
    //
    // Note that there may be still be pending interrupt requests in the APIC
    // (e.g. an incoming SIPI), which this should not affect.
    let current_apic = access.apic()?;
    let mut apic = Apic::at_reset(access.caps(), vp_info);
    apic.registers[x86defs::apic::ApicRegister::ID.0 as usize] =
        current_apic.registers[x86defs::apic::ApicRegister::ID.0 as usize];
    apic.apic_base = current_apic.apic_base;
    access.set_apic(&apic)?;

    // Enable the wait-for-SIPI state.
    if !vp_info.base.is_bsp() {
        let mut activity = access.activity()?;
        activity.mp_state = MpState::WaitForSipi;
        access.set_activity(&activity)?;
    }

    Ok(())
}
