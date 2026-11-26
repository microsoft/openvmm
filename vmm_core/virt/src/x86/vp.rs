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
use hvdef::HvMessage;
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
        for (i, feature) in caps.xsave.variable_features(header.xcomp_bv) {
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
        for (i, feature) in caps.xsave.variable_features(header.xcomp_bv) {
            let offset = feature.offset as usize;
            let len = feature.len as usize;
            if feature.align {
                cur = (cur + 63) & !63;
            }
            if header.xstate_bv & (1 << i) != 0 {
                data[offset..offset + len].copy_from_slice(&self.data.as_bytes()[cur..cur + len]);
            }
            cur += len;
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
    #[inspect(with = "ApicRegisters::from_array_ref")]
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

impl Apic {
    pub fn new(apic_base: ApicBase, registers: ApicRegisters, auto_eoi: [u32; 8]) -> Self {
        Self {
            apic_base: apic_base.into(),
            registers: *registers.as_array(),
            auto_eoi,
        }
    }

    pub fn registers(&self) -> &ApicRegisters {
        ApicRegisters::from_array_ref(&self.registers)
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Inspect)]
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

impl From<ApicRegisters> for hvdef::HvX64InterruptControllerState {
    fn from(value: ApicRegisters) -> Self {
        Self {
            apic_id: value.id,
            apic_version: value.version,
            apic_ldr: value.ldr,
            apic_dfr: value.dfr,
            apic_spurious: value.svr,
            apic_isr: value.isr,
            apic_tmr: value.tmr,
            apic_irr: value.irr,
            apic_esr: value.esr,
            apic_icr_high: value.icr[1],
            apic_icr_low: value.icr[0],
            apic_lvt_timer: value.lvt_timer,
            apic_lvt_thermal: value.lvt_thermal,
            apic_lvt_perfmon: value.lvt_pmc,
            apic_lvt_lint0: value.lvt_lint0,
            apic_lvt_lint1: value.lvt_lint1,
            apic_lvt_error: value.lvt_error,
            apic_lvt_cmci: value.lvt_cmci,
            apic_error_status: value.esr,
            apic_initial_count: value.timer_icr,
            apic_counter_value: value.timer_ccr,
            apic_divide_configuration: value.timer_dcr,
            apic_remote_read: value.rrd,
        }
    }
}

impl From<hvdef::HvX64InterruptControllerState> for ApicRegisters {
    fn from(value: hvdef::HvX64InterruptControllerState) -> Self {
        let hvdef::HvX64InterruptControllerState {
            apic_id,
            apic_version,
            apic_ldr,
            apic_dfr,
            apic_spurious,
            apic_isr,
            apic_tmr,
            apic_irr,
            apic_esr,
            apic_icr_high,
            apic_icr_low,
            apic_lvt_timer,
            apic_lvt_thermal,
            apic_lvt_perfmon,
            apic_lvt_lint0,
            apic_lvt_lint1,
            apic_lvt_error,
            apic_lvt_cmci,
            // The unlatched error status is not preserved across save/restore.
            apic_error_status: _,
            apic_initial_count,
            apic_counter_value,
            apic_divide_configuration,
            apic_remote_read,
        } = value;
        Self {
            reserved_0: [0; 2],
            id: apic_id,
            version: apic_version,
            reserved_4: [0; 4],
            tpr: 0,
            apr: 0,
            ppr: 0,
            eoi: 0,
            rrd: apic_remote_read,
            ldr: apic_ldr,
            dfr: apic_dfr,
            svr: apic_spurious,
            isr: apic_isr,
            tmr: apic_tmr,
            irr: apic_irr,
            esr: apic_esr,
            reserved_29: [0; 6],
            lvt_cmci: apic_lvt_cmci,
            icr: [apic_icr_low, apic_icr_high],
            lvt_timer: apic_lvt_timer,
            lvt_thermal: apic_lvt_thermal,
            lvt_pmc: apic_lvt_perfmon,
            lvt_lint0: apic_lvt_lint0,
            lvt_lint1: apic_lvt_lint1,
            lvt_error: apic_lvt_error,
            timer_icr: apic_initial_count,
            timer_ccr: apic_counter_value,
            reserved_3a: [0; 4],
            timer_dcr: apic_divide_configuration,
            reserved_3f: 0,
        }
    }
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct ApicRegister {
    value: u32,
    zero: [u32; 3],
}

impl ApicRegisters {
    pub fn as_array(&self) -> &[u32; 64] {
        zerocopy::transmute_ref!(self)
    }

    pub fn from_array(array: [u32; 64]) -> Self {
        zerocopy::transmute!(array)
    }

    pub fn from_array_ref(array: &[u32; 64]) -> &Self {
        zerocopy::transmute_ref!(array)
    }

    pub fn as_page(&self) -> [u8; 1024] {
        let mut bytes = [0; 1024];
        self.as_array()
            .map(|value| ApicRegister {
                value,
                zero: [0; 3],
            })
            .write_to(bytes.as_mut_slice())
            .unwrap();
        bytes
    }

    /// Convert from an APIC page.
    pub fn from_page(page: &[u8; 1024]) -> Self {
        let registers = <[ApicRegister; 64]>::read_from_bytes(page.as_slice()).unwrap();
        Self::from_array(registers.map(|reg| reg.value))
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

        Apic::new(apic_base, regs, [0; 8])
    }

    fn can_compare(caps: &X86PartitionCapabilities) -> bool {
        // If a partition (ie KVM) cannot freeze time, one of the APIC timer values will continue counting up after restore.
        // For now, disallow comparing the whole Apic structure if so.
        caps.can_freeze_time
    }
}

// The IRR bit number corresponding to NMI pending in the Hyper-V exo APIC saved
// state.
const NMI_VECTOR: u32 = 2;

impl ApicRegisters {
    /// Sets the non-architectural Hyper-V NMI pending bit in the APIC page.
    pub fn set_hv_apic_nmi_pending(&mut self, pending: bool) {
        self.irr[0] &= !(1 << NMI_VECTOR);
        self.irr[0] |= (pending as u32) << NMI_VECTOR;
    }

    /// Gets the non-architectural Hyper-V NMI pending bit from the APIC page.
    pub fn hv_apic_nmi_pending(&self) -> bool {
        self.irr[0] & (1 << NMI_VECTOR) != 0
    }
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

/// The architectural IA32_TSC_DEADLINE MSR, in guest TSC ticks.
///
/// Zero disarms the timer. Restore after both APIC configuration and TSC;
/// changing APIC mode or rebasing TSC can otherwise change the expiration.
#[repr(C)]
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct TscDeadline {
    #[mesh(1)]
    #[inspect(hex)]
    pub value: u64,
}

impl HvRegisterState<HvX64RegisterName, 1> for TscDeadline {
    fn names(&self) -> &'static [HvX64RegisterName; 1] {
        &[HvX64RegisterName::TscDeadline]
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

impl StateElement<X86PartitionCapabilities, X86VpInfo> for TscDeadline {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.tsc_deadline
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self::default()
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

#[derive(Debug, Copy, Clone, PartialEq, Eq, Protobuf)]
#[mesh(package = "virt.x86")]
pub struct SynicMessagePage {
    #[mesh(1)]
    pub data: [u8; 4096],
}

impl Inspect for SynicMessagePage {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        let mut occupied = 0u16;
        for (sint, slot) in self
            .data
            .as_chunks::<HV_MESSAGE_SIZE>()
            .0
            .iter()
            .enumerate()
        {
            let msg: HvMessage = zerocopy::transmute!(*slot);
            if msg.header.typ != hvdef::HvMessageType::HvMessageTypeNone {
                occupied |= 1 << sint;
            }
        }
        resp.binary("occupied_bitmap", occupied);
    }
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

/// Opaque nested-virtualization state blob for save/restore.
///
/// An empty blob denotes architectural reset, not a serialized hardware state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
#[inspect(skip)]
pub struct NestedState {
    #[mesh(1)]
    pub data: Vec<u8>,
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for NestedState {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.nested_virt
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self::default()
    }

    fn can_compare(_caps: &X86PartitionCapabilities) -> bool {
        false
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
    // The deadline must be restored after APIC mode and guest TSC.
    (15, "tsc_deadline", tsc_deadline, set_tsc_deadline, TscDeadline),

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

    (200, "nested_state", nested_state, set_nested_state, NestedState),
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
    if TscDeadline::is_present(access.caps()) {
        access.set_tsc_deadline(&TscDeadline::at_reset(access.caps(), vp_info))?;
    }

    // Enable the wait-for-SIPI state.
    if !vp_info.base.is_bsp() {
        let mut activity = access.activity()?;
        activity.mp_state = MpState::WaitForSipi;
        access.set_activity(&activity)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    /// Converts `state` to register values in `names()` order.
    fn values_of<T: HvRegisterState<HvX64RegisterName, N>, const N: usize>(
        state: &T,
    ) -> [HvRegisterValue; N] {
        let mut values = [HvRegisterValue::from(0u64); N];
        state.get_values(values.iter_mut());
        values
    }

    /// Returns the value `state` reports for the register `name`.
    fn value_of<T: HvRegisterState<HvX64RegisterName, N>, const N: usize>(
        state: &T,
        name: HvX64RegisterName,
    ) -> HvRegisterValue {
        let index = state
            .names()
            .iter()
            .position(|&n| n == name)
            .unwrap_or_else(|| panic!("{name:?} not in names()"));
        values_of(state)[index]
    }

    /// Converts `state` to register values and back.
    fn round_trip<T: HvRegisterState<HvX64RegisterName, N>, const N: usize>(state: &T) -> T {
        let mut out = T::default();
        out.set_values(values_of(state).into_iter());
        out
    }

    /// Sets distinct values in every register of a `u64`-only state type and
    /// reads them back.
    fn assert_u64_state_round_trips<T: HvRegisterState<HvX64RegisterName, N>, const N: usize>() {
        let values: [HvRegisterValue; N] =
            std::array::from_fn(|i| (0x0101_0101_0101_0101u64 * (i as u64 + 1)).into());
        let mut state = T::default();
        state.set_values(values.into_iter());

        assert_eq!(values_of(&state), values, "{}", std::any::type_name::<T>());

        let names = state.names();
        for (i, name) in names.iter().enumerate() {
            assert!(
                !names[..i].contains(name),
                "{} lists {name:?} twice",
                std::any::type_name::<T>()
            );
        }
    }

    fn segment(seed: u64) -> SegmentRegister {
        SegmentRegister {
            base: 0x1000_0000_0000 + seed,
            limit: 0x10_0000 + seed as u32,
            selector: 0x100 + seed as u16,
            attributes: 0x0a00 + seed as u16,
        }
    }

    // Test: Convert a register set with a distinct value in every field to
    //       register values and back, and look up individual registers by
    //       name.
    // Expected: Every field round-trips, and each named register reports the
    //       matching field, so the names, get, and set lists stay aligned.
    #[test]
    fn registers_round_trip_in_named_order() {
        let regs = Registers {
            rax: 0x1,
            rcx: 0x2,
            rdx: 0x3,
            rbx: 0x4,
            rsp: 0x5,
            rbp: 0x6,
            rsi: 0x7,
            rdi: 0x8,
            r8: 0x9,
            r9: 0xa,
            r10: 0xb,
            r11: 0xc,
            r12: 0xd,
            r13: 0xe,
            r14: 0xf,
            r15: 0x10,
            rip: 0x11,
            rflags: 0x12,
            cr0: 0x13,
            cr2: 0x14,
            cr3: 0x15,
            cr4: 0x16,
            cr8: 0x17,
            efer: 0x18,
            cs: segment(1),
            ds: segment(2),
            es: segment(3),
            fs: segment(4),
            gs: segment(5),
            ss: segment(6),
            tr: segment(7),
            ldtr: segment(8),
            gdtr: TableRegister {
                base: 0x2000,
                limit: 0x27,
            },
            idtr: TableRegister {
                base: 0x3000,
                limit: 0xfff,
            },
        };

        assert_eq!(round_trip(&regs), regs);

        assert_eq!(value_of(&regs, HvX64RegisterName::Rax).as_u64(), regs.rax);
        assert_eq!(value_of(&regs, HvX64RegisterName::Rbx).as_u64(), regs.rbx);
        assert_eq!(value_of(&regs, HvX64RegisterName::Rsp).as_u64(), regs.rsp);
        assert_eq!(value_of(&regs, HvX64RegisterName::Rip).as_u64(), regs.rip);
        assert_eq!(value_of(&regs, HvX64RegisterName::Cr3).as_u64(), regs.cr3);
        assert_eq!(value_of(&regs, HvX64RegisterName::Efer).as_u64(), regs.efer);
        assert_eq!(
            SegmentRegister::from(value_of(&regs, HvX64RegisterName::Cs).as_segment()),
            regs.cs
        );
        assert_eq!(
            SegmentRegister::from(value_of(&regs, HvX64RegisterName::Ss).as_segment()),
            regs.ss
        );
        assert_eq!(
            SegmentRegister::from(value_of(&regs, HvX64RegisterName::Tr).as_segment()),
            regs.tr
        );
        assert_eq!(
            TableRegister::from(value_of(&regs, HvX64RegisterName::Gdtr).as_table()),
            regs.gdtr
        );
        assert_eq!(
            TableRegister::from(value_of(&regs, HvX64RegisterName::Idtr).as_table()),
            regs.idtr
        );
    }

    // Test: Convert activity states covering every MP state, pending event
    //       kind, and pending interruption kind to register values and back.
    // Expected: Each state round-trips exactly. NMI pending is not part of
    //       these registers (it lives in the APIC page) and reads back false.
    #[test]
    fn activity_round_trips_every_state() {
        for activity in [
            Activity::default(),
            Activity {
                mp_state: MpState::Halted,
                nmi_pending: false,
                nmi_masked: true,
                interrupt_shadow: false,
                pending_event: Some(PendingEvent::Exception {
                    vector: 14,
                    error_code: Some(0x6),
                    parameter: 0xdead_b000,
                }),
                pending_interruption: Some(PendingInterruption::Interrupt { vector: 0x30 }),
            },
            Activity {
                mp_state: MpState::WaitForSipi,
                nmi_pending: false,
                nmi_masked: false,
                interrupt_shadow: true,
                pending_event: Some(PendingEvent::ExtInt { vector: 0x21 }),
                pending_interruption: Some(PendingInterruption::Nmi),
            },
            Activity {
                mp_state: MpState::Idle,
                nmi_pending: false,
                nmi_masked: true,
                interrupt_shadow: true,
                pending_event: Some(PendingEvent::Exception {
                    vector: 3,
                    error_code: None,
                    parameter: 0,
                }),
                pending_interruption: Some(PendingInterruption::Exception {
                    vector: 13,
                    error_code: Some(0),
                }),
            },
            Activity {
                mp_state: MpState::Running,
                nmi_pending: false,
                nmi_masked: false,
                interrupt_shadow: false,
                pending_event: None,
                pending_interruption: Some(PendingInterruption::Exception {
                    vector: 6,
                    error_code: None,
                }),
            },
        ] {
            assert_eq!(round_trip(&activity), activity);
        }

        let with_nmi = Activity {
            nmi_pending: true,
            ..Default::default()
        };
        assert!(!round_trip(&with_nmi).nmi_pending);
    }

    // Test: Write a distinct value to every register of each u64-only VP state
    //       type, read them back, and check the register names.
    // Expected: Every value reads back in the same position and no register
    //       name is listed twice, so get and set use the same order.
    #[test]
    fn u64_register_states_round_trip() {
        assert_u64_state_round_trips::<DebugRegisters, 6>();
        assert_u64_state_round_trips::<Xcr0, 1>();
        assert_u64_state_round_trips::<Xss, 1>();
        assert_u64_state_round_trips::<Pat, 1>();
        assert_u64_state_round_trips::<Mtrrs, 28>();
        assert_u64_state_round_trips::<VirtualMsrs, 8>();
        assert_u64_state_round_trips::<TscAux, 1>();
        assert_u64_state_round_trips::<Tsc, 1>();
        assert_u64_state_round_trips::<TscDeadline, 1>();
        assert_u64_state_round_trips::<Cet, 1>();
        assert_u64_state_round_trips::<CetSs, 2>();
        assert_u64_state_round_trips::<SyntheticMsrs, 20>();
    }

    // IGNORED: this test fails because of a known bug. VirtualMsrs::names() lists
    // SysenterEsp before SysenterEip, but get_values()/set_values() use sysenter_eip
    // before sysenter_esp, so backends that move this state by register name (KVM,
    // mshv, WHP, OpenHCL non-isolated) store the hypervisor's SYSENTER_ESP in the
    // sysenter_eip field and vice versa. Backends that build VirtualMsrs by field
    // (OpenHCL TDX/SNP, KVM SNP) use the correct meaning.
    //
    // Fixing this is a trap: VirtualMsrs is saved by field, so state saved today by
    // a name-based backend already has the two values swapped. Simply reordering
    // names() or get/set would make state saved before the fix restore with EIP and
    // ESP swapped, faulting 32-bit guests on their next SYSENTER. A fix needs
    // saved-state compatibility handling (for example, swapping old saved values on
    // restore). Re-enable this test (remove #[ignore]) together with that fix.
    //
    // Test: Look up each virtual MSR by name, where a consistent swap of two
    //       fields between the names list and get/set would still round-trip.
    // Expected: Each named register reports its own field (for example,
    //       SysenterEip reports sysenter_eip).
    #[test]
    #[ignore = "known bug: SysenterEip/SysenterEsp names are swapped (see comment)"]
    fn virtual_msrs_report_fields_under_their_names() {
        let msrs = VirtualMsrs {
            kernel_gs_base: 1,
            sysenter_cs: 2,
            sysenter_eip: 3,
            sysenter_esp: 4,
            star: 5,
            lstar: 6,
            cstar: 7,
            sfmask: 8,
        };
        for (name, value) in [
            (HvX64RegisterName::KernelGsBase, msrs.kernel_gs_base),
            (HvX64RegisterName::SysenterCs, msrs.sysenter_cs),
            (HvX64RegisterName::SysenterEip, msrs.sysenter_eip),
            (HvX64RegisterName::SysenterEsp, msrs.sysenter_esp),
            (HvX64RegisterName::Star, msrs.star),
            (HvX64RegisterName::Lstar, msrs.lstar),
            (HvX64RegisterName::Cstar, msrs.cstar),
            (HvX64RegisterName::Sfmask, msrs.sfmask),
        ] {
            assert_eq!(value_of(&msrs, name).as_u64(), value, "{name:?}");
        }
    }

    // Test: Look up debug registers and synthetic MSRs by name, where a
    //       consistent swap of two fields would still round-trip.
    // Expected: Each named register reports its own field.
    #[test]
    fn debug_and_synthetic_msrs_report_fields_under_their_names() {
        let debug = DebugRegisters {
            dr0: 1,
            dr1: 2,
            dr2: 3,
            dr3: 4,
            dr6: 5,
            dr7: 6,
        };
        assert_eq!(value_of(&debug, HvX64RegisterName::Dr3).as_u64(), debug.dr3);
        assert_eq!(value_of(&debug, HvX64RegisterName::Dr6).as_u64(), debug.dr6);
        assert_eq!(value_of(&debug, HvX64RegisterName::Dr7).as_u64(), debug.dr7);

        let mut synic = SyntheticMsrs::default();
        synic.set_values((1..=20u64).map(HvRegisterValue::from));
        assert_eq!(
            value_of(&synic, HvX64RegisterName::Sint0).as_u64(),
            synic.sint[0]
        );
        assert_eq!(
            value_of(&synic, HvX64RegisterName::Sint15).as_u64(),
            synic.sint[15]
        );
        assert_eq!(
            value_of(&synic, HvX64RegisterName::Sipp).as_u64(),
            synic.simp
        );
        assert_eq!(
            value_of(&synic, HvX64RegisterName::Sifp).as_u64(),
            synic.siefp
        );
    }

    const FEATURE_A: u64 = 1 << 3;
    const FEATURE_B: u64 = 1 << 5;
    const YMM_OFFSET: usize = XSAVE_VARIABLE_OFFSET;
    const YMM_LEN: usize = 256;
    const A_OFFSET: usize = 960;
    const A_LEN: usize = 40;
    const B_OFFSET: usize = 1088;
    const B_LEN: usize = 64;
    // In compact format, A follows YMM directly, and B (64-byte aligned)
    // follows A after padding.
    const COMPACT_A: usize = XSAVE_VARIABLE_OFFSET + YMM_LEN;
    const COMPACT_B: usize = (COMPACT_A + A_LEN + 63) & !63;

    /// Capabilities for a synthetic XSAVE layout with x87, SSE, YMM, and two
    /// extended features, where B requires 64-byte alignment in compact form.
    fn xsave_caps(
        xsaves_state_bv_broken: bool,
        supervisor_features: u64,
    ) -> X86PartitionCapabilities {
        let mut feature_info = [crate::x86::XsaveFeature::default(); 63];
        feature_info[2] = crate::x86::XsaveFeature {
            offset: YMM_OFFSET as u32,
            len: YMM_LEN as u32,
            align: false,
        };
        feature_info[3] = crate::x86::XsaveFeature {
            offset: A_OFFSET as u32,
            len: A_LEN as u32,
            align: false,
        };
        feature_info[5] = crate::x86::XsaveFeature {
            offset: B_OFFSET as u32,
            len: B_LEN as u32,
            align: true,
        };
        let features = XFEATURE_X87 | XFEATURE_SSE | XFEATURE_YMM | FEATURE_A | FEATURE_B;
        let mut xsave = crate::x86::XsaveCapabilities {
            features,
            supervisor_features,
            standard_len: 0,
            compact_len: 0,
            feature_info,
        };
        xsave.standard_len = xsave.standard_len_for(features);
        xsave.compact_len =
            xsave.compact_len_for(features | supervisor_features | XCOMP_COMPRESSED);
        X86PartitionCapabilities {
            vendor: x86defs::cpuid::Vendor::INTEL,
            hv1: false,
            hv1_reference_tsc_page: false,
            xsave,
            x2apic: false,
            x2apic_enabled: false,
            reset_rdx: 0,
            cet: false,
            cet_ss: false,
            sgx: false,
            tsc_aux: false,
            tsc_deadline: false,
            vtom: None,
            physical_address_width: 48,
            snp_c_bit: None,
            can_freeze_time: false,
            xsaves_state_bv_broken,
            dr6_tsx_broken: false,
            nxe_forced_on: false,
            nested_virt: false,
        }
    }

    /// Legacy x87/SSE state with every field set to a non-init value.
    fn active_fxsave() -> Fxsave {
        let mut fxsave = Fxsave::new_zeroed();
        fxsave.fcw = 0x027f;
        fxsave.fsw = 0x0001;
        fxsave.ftw = 0x80;
        fxsave.fop = 0x0123;
        fxsave.fip = 0x1111_2222;
        fxsave.fdp = 0x3333_4444;
        fxsave.mxcsr = 0x1fa0;
        fxsave.st = std::array::from_fn(|i| [0x10 + i as u8; 16]);
        fxsave.xmm = std::array::from_fn(|i| [0x40 + i as u8; 16]);
        fxsave
    }

    /// Builds a buffer of `len` bytes with `fxsave` and an XSAVE header.
    fn xsave_buffer(len: usize, fxsave: &Fxsave, xstate_bv: u64, xcomp_bv: u64) -> Vec<u8> {
        let mut data = vec![0; len];
        data[..XSAVE_LEGACY_LEN].copy_from_slice(fxsave.as_bytes());
        data[XSAVE_LEGACY_LEN..XSAVE_VARIABLE_OFFSET].copy_from_slice(
            XsaveHeader {
                xstate_bv,
                xcomp_bv,
                reserved: [0; 6],
            }
            .as_bytes(),
        );
        data
    }

    fn header_of(data: &[u8]) -> XsaveHeader {
        XsaveHeader::read_from_prefix(&data[XSAVE_LEGACY_LEN..])
            .unwrap()
            .0
    }

    // Test: Convert a standard-format XSAVE area with every feature enabled to
    //       the compact format and back.
    // Expected: The standard output equals the input, and the compact form
    //       packs each feature at its compact offset (including alignment
    //       padding) with xcomp_bv marking the compacted features.
    #[test]
    fn xsave_round_trips_between_standard_and_compact() {
        let caps = xsave_caps(false, 0);
        let xstate_bv = XFEATURE_X87 | XFEATURE_SSE | XFEATURE_YMM | FEATURE_A | FEATURE_B;
        let mut standard = xsave_buffer(
            caps.xsave.standard_len as usize,
            &active_fxsave(),
            xstate_bv,
            0,
        );
        standard[YMM_OFFSET..][..YMM_LEN].fill(0x22);
        standard[A_OFFSET..][..A_LEN].fill(0x33);
        standard[B_OFFSET..][..B_LEN].fill(0x55);

        let xsave = Xsave::from_standard(&standard, &caps);
        let mut out = vec![0; standard.len()];
        xsave.write_standard(&mut out, &caps);

        assert!(out == standard, "standard format did not round-trip");
        let compact = xsave.compact();
        assert_eq!(compact.len(), caps.xsave.compact_len as usize);
        assert_eq!(header_of(compact).xstate_bv, xstate_bv);
        assert_eq!(
            header_of(compact).xcomp_bv,
            caps.xsave.features | XCOMP_COMPRESSED
        );
        assert!(
            compact[XSAVE_VARIABLE_OFFSET..][..YMM_LEN]
                .iter()
                .all(|&b| b == 0x22)
        );
        assert!(compact[COMPACT_A..][..A_LEN].iter().all(|&b| b == 0x33));
        assert!(
            compact[COMPACT_A + A_LEN..COMPACT_B]
                .iter()
                .all(|&b| b == 0)
        );
        assert!(compact[COMPACT_B..][..B_LEN].iter().all(|&b| b == 0x55));
    }

    // Test: Convert a standard-format area whose YMM and A regions contain
    //       data, but whose xstate_bv enables only x87, SSE, and B.
    // Expected: Only enabled features are carried into the compact form and
    //       written back; disabled feature regions are left untouched.
    #[test]
    fn xsave_ignores_data_for_disabled_features() {
        let caps = xsave_caps(false, 0);
        let mut standard = xsave_buffer(
            caps.xsave.standard_len as usize,
            &active_fxsave(),
            XFEATURE_X87 | XFEATURE_SSE | FEATURE_B,
            0,
        );
        standard[YMM_OFFSET..][..YMM_LEN].fill(0xee);
        standard[A_OFFSET..][..A_LEN].fill(0xee);
        standard[B_OFFSET..][..B_LEN].fill(0x55);

        let xsave = Xsave::from_standard(&standard, &caps);
        let mut out = vec![0; standard.len()];
        xsave.write_standard(&mut out, &caps);

        let compact = xsave.compact();
        assert!(
            compact[XSAVE_VARIABLE_OFFSET..COMPACT_B]
                .iter()
                .all(|&b| b == 0)
        );
        assert!(compact[COMPACT_B..][..B_LEN].iter().all(|&b| b == 0x55));
        assert!(out[YMM_OFFSET..][..YMM_LEN].iter().all(|&b| b == 0));
        assert!(out[A_OFFSET..][..A_LEN].iter().all(|&b| b == 0));
        assert!(out[B_OFFSET..][..B_LEN].iter().all(|&b| b == 0x55));
    }

    // Test: Read the legacy region of a compact area whose xstate_bv enables
    //       neither x87 nor SSE/YMM state, while its legacy bytes hold stale
    //       values.
    // Expected: The stale x87 and SSE fields are discarded, and the legacy
    //       view reports the architectural init values for FCW and MXCSR.
    #[test]
    fn xsave_reports_init_legacy_state_for_disabled_features() {
        let caps = xsave_caps(false, 0);
        let compact = xsave_buffer(
            caps.xsave.compact_len as usize,
            &active_fxsave(),
            0,
            caps.xsave.features | XCOMP_COMPRESSED,
        );

        let fxsave = Xsave::from_compact(&compact, &caps).fxsave();

        assert_eq!(fxsave.fcw, INIT_FCW);
        assert_eq!(fxsave.mxcsr, DEFAULT_MXCSR);
        assert_eq!(
            (fxsave.fsw, fxsave.ftw, fxsave.fop, fxsave.fip, fxsave.fdp),
            (0, 0, 0, 0, 0)
        );
        assert_eq!(fxsave.st, [[0; 16]; 8]);
        assert_eq!(fxsave.xmm, [[0; 16]; 16]);
    }

    // Test: Load two compact areas with identical architectural state, one
    //       reporting x87/SSE as enabled with init values (as mshv does) and
    //       one reporting them as disabled (as KVM does), with different
    //       ignored fields (MXCSR mask and the unused legacy bytes).
    // Expected: They compare equal after loading, while a state with real
    //       SSE data keeps it.
    #[test]
    fn xsave_normalizes_equivalent_init_states() {
        let caps = xsave_caps(false, 0);
        let xcomp_bv = caps.xsave.features | XCOMP_COMPRESSED;
        let len = caps.xsave.compact_len as usize;

        let mut init = Fxsave::new_zeroed();
        init.fcw = INIT_FCW;
        init.mxcsr = DEFAULT_MXCSR;
        init.mxcsr_mask = 0xffff;
        init.unused = [0xaa; 48];
        let mshv_style = xsave_buffer(len, &init, XFEATURE_X87 | XFEATURE_SSE, xcomp_bv);
        let kvm_style = xsave_buffer(len, &Fxsave::new_zeroed(), 0, xcomp_bv);

        assert!(
            Xsave::from_compact(&mshv_style, &caps) == Xsave::from_compact(&kvm_style, &caps),
            "equivalent init states compared unequal"
        );

        let mut active = init;
        active.xmm[3] = [0x77; 16];
        let active = Xsave::from_compact(
            &xsave_buffer(len, &active, XFEATURE_X87 | XFEATURE_SSE, xcomp_bv),
            &caps,
        );
        assert_ne!(header_of(active.compact()).xstate_bv & XFEATURE_SSE, 0);
        assert_eq!(active.fxsave().xmm[3], [0x77; 16]);
    }

    // Test: Load a compact area whose xcomp_bv includes supervisor features
    //       but whose xstate_bv omits them, with and without the
    //       xsaves_state_bv_broken workaround.
    // Expected: With the workaround, the supervisor states possible on
    //       affected hypervisors (bits 10-12) are marked present; without it,
    //       xstate_bv is left as reported.
    #[test]
    fn xsave_marks_supervisor_states_present_only_when_hypervisor_is_broken() {
        let supervisor = 0x1c00 | (1 << 14);
        for broken in [false, true] {
            let caps = xsave_caps(broken, supervisor);
            let compact = xsave_buffer(
                caps.xsave.compact_len as usize,
                &Fxsave::new_zeroed(),
                XFEATURE_YMM,
                caps.xsave.features | supervisor | XCOMP_COMPRESSED,
            );

            let xstate_bv = header_of(Xsave::from_compact(&compact, &caps).compact()).xstate_bv;

            let expected = if broken {
                XFEATURE_YMM | 0x1c00
            } else {
                XFEATURE_YMM
            };
            assert_eq!(xstate_bv, expected, "broken: {broken}");
        }
    }

    fn vp(index: u32, apic_id: u32) -> X86VpInfo {
        X86VpInfo {
            base: vm_topology::processor::VpInfo {
                vp_index: vm_topology::processor::VpIndex::new(index),
                vnode: 0,
            },
            apic_id,
        }
    }

    // Test: Compute the reset register state with a processor signature of
    //       0x906ea, with and without EFER.NXE forced on.
    // Expected: The state matches the architectural power-on state (CS base
    //       0xffff0000 selector 0xf000, RIP 0xfff0, RFLAGS 2, CR0 with ET, CD,
    //       and NW, 64 KiB segment limits, RDX holding the signature); EFER is
    //       zero unless NXE is forced on. The initial boot state uses these
    //       registers with the default PAT and MTRRs cleared.
    #[test]
    fn registers_reset_to_power_on_state() {
        let mut caps = crate::x86::tests::basic_caps();
        caps.reset_rdx = 0x906ea;
        let regs = Registers::at_reset(&caps, &vp(0, 0));

        assert_eq!(regs.rdx, 0x906ea);
        let other_gprs = [
            regs.rax, regs.rcx, regs.rbx, regs.rsp, regs.rbp, regs.rsi, regs.rdi, regs.r8, regs.r9,
            regs.r10, regs.r11, regs.r12, regs.r13, regs.r14, regs.r15,
        ];
        assert_eq!(other_gprs, [0; 15]);
        assert_eq!((regs.rip, regs.rflags), (0xfff0, 2));
        assert_eq!(
            regs.cs,
            SegmentRegister {
                base: 0xffff_0000,
                limit: 0xffff,
                selector: 0xf000,
                attributes: 0x9b,
            }
        );
        for seg in [regs.ds, regs.es, regs.fs, regs.gs, regs.ss] {
            assert_eq!((seg.base, seg.limit, seg.selector), (0, 0xffff, 0));
            assert_eq!(seg.attributes, 0x93);
        }
        assert_eq!((regs.tr.attributes, regs.ldtr.attributes), (0x8b, 0x82));
        assert_eq!(
            regs.gdtr,
            TableRegister {
                base: 0,
                limit: 0xffff
            }
        );
        assert_eq!(regs.idtr, regs.gdtr);
        assert_eq!(regs.cr0, 0x6000_0010);
        assert_eq!((regs.cr2, regs.cr3, regs.cr4, regs.cr8), (0, 0, 0, 0));
        assert_eq!(regs.efer, 0);

        caps.nxe_forced_on = true;
        assert_eq!(Registers::at_reset(&caps, &vp(0, 0)).efer, X64_EFER_NXE);

        let initial = crate::x86::X86InitialRegs::at_reset(&caps, &vp(0, 0));
        assert_eq!(initial.registers, Registers::at_reset(&caps, &vp(0, 0)));
        assert_eq!(initial.pat.value, X86X_MSR_DEFAULT_PAT);
        assert_eq!(initial.mtrrs, Mtrrs::default());
    }

    // Test: Compute the reset activity state and debug registers for the BSP
    //       and an AP, and ask whether debug registers can be compared when the
    //       hypervisor can't restore DR6 exactly.
    // Expected: The BSP runs and the AP waits for a startup IPI, with nothing
    //       pending; DR6 and DR7 hold their reset values (0xffff0ff0, 0x400);
    //       the comparison is skipped only for the broken hypervisor.
    #[test]
    fn activity_and_debug_registers_reset() {
        let mut caps = crate::x86::tests::basic_caps();

        let bsp = Activity::at_reset(&caps, &vp(0, 0));
        assert_eq!(bsp.mp_state, MpState::Running);
        assert!(!bsp.nmi_pending && !bsp.nmi_masked && !bsp.interrupt_shadow);
        assert!(bsp.pending_event.is_none() && bsp.pending_interruption.is_none());
        assert_eq!(
            Activity::at_reset(&caps, &vp(1, 1)).mp_state,
            MpState::WaitForSipi
        );

        let drs = DebugRegisters::at_reset(&caps, &vp(0, 0));
        assert_eq!(
            drs,
            DebugRegisters {
                dr6: 0xffff_0ff0,
                dr7: 0x400,
                ..Default::default()
            }
        );
        assert!(DebugRegisters::can_compare(&caps));
        caps.dr6_tsx_broken = true;
        assert!(!DebugRegisters::can_compare(&caps));
    }

    // Test: Compute the reset APIC for the BSP in xAPIC mode and for an AP
    //       with APIC ID 0x23 in x2APIC mode.
    // Expected: Both are enabled at 0xfee00000 with version 0x14, five LVT
    //       entries, every LVT masked, and spurious vector 0xff; only the BSP
    //       has the BSP flag. xAPIC puts the ID in bits 24-31 with a flat
    //       DFR; x2APIC uses the full ID and the cluster-based logical ID.
    #[test]
    fn apic_resets_for_xapic_and_x2apic() {
        let mut caps = crate::x86::tests::basic_caps();

        let check_common = |apic: &Apic| {
            let regs = apic.registers();
            assert_eq!(regs.version, 0x5_0014);
            assert_eq!(regs.svr, 0xff);
            for lvt in [
                regs.lvt_timer,
                regs.lvt_thermal,
                regs.lvt_pmc,
                regs.lvt_lint0,
                regs.lvt_lint1,
                regs.lvt_error,
            ] {
                assert_eq!(lvt, 0x10000);
            }
            assert_eq!(apic.auto_eoi, [0; 8]);
        };

        let bsp = Apic::at_reset(&caps, &vp(0, 0));
        check_common(&bsp);
        let base = ApicBase::from(bsp.apic_base);
        assert_eq!(base.base_page(), 0xfee00);
        assert!(base.enable() && base.bsp() && !base.x2apic());
        assert_eq!((bsp.registers().id, bsp.registers().dfr), (0, !0));
        assert_eq!(bsp.registers().ldr, 0);

        caps.x2apic_enabled = true;
        let ap = Apic::at_reset(&caps, &vp(1, 0x23));
        check_common(&ap);
        let base = ApicBase::from(ap.apic_base);
        assert!(base.enable() && !base.bsp() && base.x2apic());
        assert_eq!(ap.registers().id, 0x23);
        assert_eq!(ap.registers().ldr, (2 << 16) | (1 << 3));

        caps.x2apic_enabled = false;
        assert_eq!(
            Apic::at_reset(&caps, &vp(1, 0x23)).registers().id,
            0x23 << 24
        );

        assert!(!Apic::can_compare(&caps));
        caps.can_freeze_time = true;
        assert!(Apic::can_compare(&caps));
    }

    /// APIC registers with a distinct value in every field the hypervisor
    /// state carries.
    fn distinct_apic_registers() -> ApicRegisters {
        let mut regs = ApicRegisters::from_array(std::array::from_fn(|i| 0x100 + i as u32));
        regs.reserved_0 = [0; 2];
        regs.reserved_4 = [0; 4];
        regs.reserved_29 = [0; 6];
        regs.reserved_3a = [0; 4];
        regs.reserved_3f = 0;
        regs.tpr = 0;
        regs.apr = 0;
        regs.ppr = 0;
        regs.eoi = 0;
        regs
    }

    // Test: Convert APIC registers with distinct values to the hypervisor's
    //       interrupt controller state and back.
    // Expected: Every field the hypervisor state carries round-trips, with ICR
    //       low and high in the right order and the error status mirrored from
    //       ESR.
    #[test]
    fn apic_registers_round_trip_through_hypervisor_state() {
        let regs = distinct_apic_registers();

        let hv = hvdef::HvX64InterruptControllerState::from(regs);
        assert_eq!(
            (hv.apic_icr_low, hv.apic_icr_high),
            (regs.icr[0], regs.icr[1])
        );
        assert_eq!(hv.apic_error_status, regs.esr);

        let back = ApicRegisters::from(hv);
        assert_eq!(back.as_array(), regs.as_array());
    }

    // Test: Lay APIC registers out as a 1 KiB APIC page and read the page back,
    //       and set and clear the Hyper-V NMI-pending bit.
    // Expected: Register i is at byte offset 16 * i with the rest of each
    //       16-byte slot zero, and the page reads back to the same registers;
    //       the NMI bit is IRR bit 2 and doesn't disturb other IRR bits.
    #[test]
    fn apic_registers_page_layout_and_nmi_bit() {
        let mut regs = distinct_apic_registers();
        let page = regs.as_page();

        for (i, slot) in page.as_chunks::<16>().0.iter().enumerate() {
            assert_eq!(slot[..4], regs.as_array()[i].to_le_bytes(), "register {i}");
            assert_eq!(slot[4..], [0; 12], "register {i}");
        }
        assert_eq!(ApicRegisters::from_page(&page).as_array(), regs.as_array());

        regs.irr[0] = 0x8000_0001;
        regs.set_hv_apic_nmi_pending(true);
        assert!(regs.hv_apic_nmi_pending());
        assert_eq!(regs.irr[0], 0x8000_0005);
        regs.set_hv_apic_nmi_pending(false);
        assert!(!regs.hv_apic_nmi_pending());
        assert_eq!(regs.irr[0], 0x8000_0001);
    }

    // Test: Compute the reset value and presence of each remaining VP state
    //       element for a processor with and without the features they
    //       depend on.
    // Expected: Core register, activity, XSAVE, APIC, PAT, MTRR, virtual MSR,
    //       debug register, and TSC state is always present. Each other
    //       element is present only when its feature is: XCR0 with
    //       XSAVE, XSS with supervisor XSAVE features, TSC_AUX, TSC deadline,
    //       CET, CET shadow stacks, nested state, and every synthetic
    //       element with the Hyper-V interface. Reset values are XCR0 1, the
    //       default PAT, synthetic interrupt sources masked with SCONTROL
    //       enabled, and zero for everything else.
    #[test]
    fn remaining_state_presence_and_reset_values() {
        let off = crate::x86::tests::basic_caps();
        let mut on = crate::x86::tests::basic_caps();
        on.xsave.features = 0x7;
        on.xsave.supervisor_features = 1 << 11;
        on.tsc_aux = true;
        on.tsc_deadline = true;
        on.cet = true;
        on.cet_ss = true;
        on.hv1 = true;
        on.nested_virt = true;

        let presence = |caps: &X86PartitionCapabilities| {
            [
                Xcr0::is_present(caps),
                Xss::is_present(caps),
                TscAux::is_present(caps),
                TscDeadline::is_present(caps),
                Cet::is_present(caps),
                CetSs::is_present(caps),
                SyntheticMsrs::is_present(caps),
                SynicTimers::is_present(caps),
                SynicMessageQueues::is_present(caps),
                SynicMessagePage::is_present(caps),
                SynicEventFlagsPage::is_present(caps),
                NestedState::is_present(caps),
            ]
        };
        assert_eq!(presence(&off), [false; 12]);
        assert_eq!(presence(&on), [true; 12]);
        let always = [
            Registers::is_present(&off),
            Activity::is_present(&off),
            Xsave::is_present(&off),
            Apic::is_present(&off),
            Pat::is_present(&off),
            Mtrrs::is_present(&off),
            VirtualMsrs::is_present(&off),
            DebugRegisters::is_present(&off),
            Tsc::is_present(&off),
        ];
        assert_eq!(always, [true; 9]);

        let bsp = vp(0, 0);
        assert_eq!(Xcr0::at_reset(&on, &bsp), Xcr0 { value: 1 });
        assert_eq!(Xss::at_reset(&on, &bsp), Xss::default());
        assert_eq!(Pat::at_reset(&on, &bsp).value, 0x0007_0406_0007_0406);
        assert_eq!(Mtrrs::at_reset(&on, &bsp), Mtrrs::default());
        assert_eq!(VirtualMsrs::at_reset(&on, &bsp), VirtualMsrs::default());
        assert_eq!(TscAux::at_reset(&on, &bsp), TscAux::default());
        assert_eq!(Tsc::at_reset(&on, &bsp), Tsc::default());
        assert_eq!(TscDeadline::at_reset(&on, &bsp), TscDeadline::default());
        assert_eq!(Cet::at_reset(&on, &bsp), Cet::default());
        assert_eq!(CetSs::at_reset(&on, &bsp), CetSs::default());
        assert_eq!(
            SyntheticMsrs::at_reset(&on, &bsp),
            SyntheticMsrs {
                scontrol: 1,
                sint: [0x10000; 16],
                ..Default::default()
            }
        );
        assert_eq!(
            SynicTimers::at_reset(&on, &bsp).timers,
            [SynicTimer::default(); 4]
        );
        assert_eq!(
            SynicMessageQueues::at_reset(&on, &bsp),
            SynicMessageQueues::default()
        );
        assert_eq!(SynicMessagePage::at_reset(&on, &bsp).data, [0; 4096]);
        assert_eq!(SynicEventFlagsPage::at_reset(&on, &bsp).data, [0; 4096]);
        assert_eq!(NestedState::at_reset(&on, &bsp), NestedState::default());
    }

    // Test: Ask whether each time-dependent or hypervisor-owned element can be
    //       compared after being set, with and without frozen time.
    // Expected: TSC and TSC deadline can be compared only when time is frozen;
    //       synthetic timers and nested state never can.
    #[test]
    fn time_dependent_state_is_compared_only_when_frozen() {
        let mut caps = crate::x86::tests::basic_caps();
        let comparable = |caps: &X86PartitionCapabilities| {
            [
                Tsc::can_compare(caps),
                TscDeadline::can_compare(caps),
                SynicTimers::can_compare(caps),
                NestedState::can_compare(caps),
            ]
        };

        assert_eq!(comparable(&caps), [false; 4]);
        caps.can_freeze_time = true;
        assert_eq!(comparable(&caps), [true, true, false, false]);
    }

    // Test: Convert synthetic timers to the hypervisor's form and back, with
    //       one timer holding an undelivered message and the others not.
    // Expected: Config, count, and adjustment round-trip; the undelivered
    //       message becomes the pending flag plus expiration time and comes
    //       back as the same expiration; timers without one stay empty.
    #[test]
    fn synic_timers_round_trip_through_hypervisor_state() {
        let timers = SynicTimers {
            timers: std::array::from_fn(|i| SynicTimer {
                config: 0x10 + i as u64,
                count: 0x20 + i as u64,
                adjustment: 0x30 + i as u64,
                undelivered_message_expiration_time: (i == 2).then_some(0x4000),
            }),
        };

        let hv = timers.as_hv();
        assert_eq!(hv.timers[2].undelivered_message_pending, 1);
        assert_eq!(hv.timers[2].undelivered_expiration_time, 0x4000);
        assert_eq!(hv.timers[0].undelivered_message_pending, 0);
        assert_eq!(SynicTimers::from_hv(hv), timers);
    }

    #[derive(Debug, thiserror::Error)]
    #[error("fake access failed")]
    struct FakeError;

    /// A VP that keeps the state `x86_init` touches in memory.
    struct FakeVp {
        caps: X86PartitionCapabilities,
        registers: Registers,
        debug_regs: DebugRegisters,
        apic: Apic,
        tsc_deadline: TscDeadline,
        mp_state: MpState,
        nmi_pending: bool,
    }

    macro_rules! untouched_state {
        ($($get:ident $set:ident $ty:ty),* $(,)?) => {
            $(
                fn $get(&mut self) -> Result<$ty, FakeError> {
                    unreachable!(concat!("x86_init read ", stringify!($get)))
                }
                fn $set(&mut self, _: &$ty) -> Result<(), FakeError> {
                    unreachable!(concat!("x86_init wrote ", stringify!($get)))
                }
            )*
        };
    }

    impl AccessVpState for FakeVp {
        type Error = FakeError;

        fn caps(&self) -> &X86PartitionCapabilities {
            &self.caps
        }

        fn commit(&mut self) -> Result<(), FakeError> {
            Ok(())
        }

        fn registers(&mut self) -> Result<Registers, FakeError> {
            Ok(self.registers)
        }

        fn set_registers(&mut self, value: &Registers) -> Result<(), FakeError> {
            self.registers = *value;
            Ok(())
        }

        fn activity(&mut self) -> Result<Activity, FakeError> {
            Ok(Activity {
                mp_state: self.mp_state,
                nmi_pending: self.nmi_pending,
                ..Default::default()
            })
        }

        fn set_activity(&mut self, value: &Activity) -> Result<(), FakeError> {
            self.mp_state = value.mp_state;
            self.nmi_pending = value.nmi_pending;
            Ok(())
        }

        fn apic(&mut self) -> Result<Apic, FakeError> {
            Ok(self.apic.clone())
        }

        fn set_apic(&mut self, value: &Apic) -> Result<(), FakeError> {
            self.apic = value.clone();
            Ok(())
        }

        fn debug_regs(&mut self) -> Result<DebugRegisters, FakeError> {
            Ok(self.debug_regs)
        }

        fn set_debug_regs(&mut self, value: &DebugRegisters) -> Result<(), FakeError> {
            self.debug_regs = *value;
            Ok(())
        }

        fn tsc_deadline(&mut self) -> Result<TscDeadline, FakeError> {
            Ok(self.tsc_deadline)
        }

        fn set_tsc_deadline(&mut self, value: &TscDeadline) -> Result<(), FakeError> {
            self.tsc_deadline = *value;
            Ok(())
        }

        untouched_state!(
            xsave set_xsave Xsave,
            xcr set_xcr Xcr0,
            xss set_xss Xss,
            mtrrs set_mtrrs Mtrrs,
            pat set_pat Pat,
            virtual_msrs set_virtual_msrs VirtualMsrs,
            tsc set_tsc Tsc,
            cet set_cet Cet,
            cet_ss set_cet_ss CetSs,
            tsc_aux set_tsc_aux TscAux,
            synic_msrs set_synic_msrs SyntheticMsrs,
            synic_message_page set_synic_message_page SynicMessagePage,
            synic_event_flags_page set_synic_event_flags_page SynicEventFlagsPage,
            synic_message_queues set_synic_message_queues SynicMessageQueues,
            synic_timers set_synic_timers SynicTimers,
            nested_state set_nested_state NestedState,
        );
    }

    /// A VP that has been running in protected mode with paging and caching
    /// enabled, breakpoints armed, a relocated APIC with a guest-assigned ID,
    /// and a TSC deadline armed.
    fn running_vp(vp_info: &X86VpInfo) -> FakeVp {
        let mut caps = crate::x86::tests::basic_caps();
        caps.tsc_deadline = true;
        let mut registers = Registers::at_reset(&caps, vp_info);
        registers.cr0 = x86defs::X64_CR0_PE | x86defs::X64_CR0_PG | X64_CR0_ET;
        registers.rip = 0xffff_8000_0000_1000;
        let mut apic = Apic::at_reset(&caps, vp_info);
        apic.apic_base = 0xfed0_0900;
        apic.registers[x86defs::apic::ApicRegister::ID.0 as usize] = 7 << 24;
        apic.registers[x86defs::apic::ApicRegister::SVR.0 as usize] = 0x1ff;
        FakeVp {
            caps,
            registers,
            debug_regs: DebugRegisters {
                dr0: 0x1000,
                dr7: 0x401,
                ..DebugRegisters::at_reset(&crate::x86::tests::basic_caps(), vp_info)
            },
            apic,
            tsc_deadline: TscDeadline { value: 0x1234 },
            mp_state: MpState::Halted,
            nmi_pending: true,
        }
    }

    // Test: Send INIT to a running AP and to a halted BSP.
    // Expected: Registers, debug registers, and the APIC are reset, except
    //       that CR0.CD/NW, the APIC base, and the APIC ID keep their current
    //       values; the TSC deadline is disarmed. The AP waits for a startup
    //       IPI with its pending NMI kept; the BSP's activity is untouched.
    #[test]
    fn x86_init_resets_vp_but_keeps_cache_and_apic_identity() {
        let ap_info = vp(1, 1);
        let mut ap = running_vp(&ap_info);

        x86_init(&mut ap, &ap_info).unwrap();

        let mut expected = Registers::at_reset(&ap.caps, &ap_info);
        expected.cr0 = X64_CR0_ET;
        assert_eq!(ap.registers, expected);
        assert_eq!(ap.debug_regs, DebugRegisters::at_reset(&ap.caps, &ap_info));
        assert_eq!(ap.apic.apic_base, 0xfed0_0900);
        assert_eq!(ap.apic.registers().id, 7 << 24);
        assert_eq!(ap.apic.registers().svr, 0xff);
        assert_eq!(ap.tsc_deadline, TscDeadline::default());
        assert_eq!(ap.mp_state, MpState::WaitForSipi);
        assert!(ap.nmi_pending);

        let bsp_info = vp(0, 0);
        let mut bsp = running_vp(&bsp_info);
        x86_init(&mut bsp, &bsp_info).unwrap();
        assert_eq!(bsp.mp_state, MpState::Halted);
    }
}
