// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-VP state.

mod snapshot;

pub use snapshot::LapicRestoreClock;
pub use snapshot::SavedOneShotTimer;

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
use std::ops::Range;
use thiserror::Error;
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

    /// KVM does not keep everything of a segment's attributes:
    ///
    /// - Its segment format has no field for the reserved bits 8-11, so they
    ///   read back clear.
    /// - A CPU with VMX that runs the guest unrestricted, as the CPUs that
    ///   support it do by default, has KVM set the accessed bit, bit 0 of the
    ///   type, of every segment register but the LDTR as it writes one
    ///   (`__vmx_set_segment`, Linux v6.18 `vmx.c`), as the processor requires
    ///   of a usable segment, and an SVM CPU's reads set it of the data
    ///   segment registers that are present (`svm_get_segment`, `svm.c`). The
    ///   KVM backend clears it as it reads a segment that is not present
    ///   (`seg_reg_from_kvm`), which an earlier save did not do. So the bit
    ///   reads back as it was set, or as the segment's present bit.
    fn restore_eq(caps: &X86PartitionCapabilities, restored: &Self, expected: &Self) -> bool {
        const RESERVED: u16 = 0x0f00;
        const ACCESSED: u16 = 0x0001;
        const PRESENT: u16 = 0x0080;

        /// Returns whether the segment `restored` is `expected`, but for the
        /// reserved attribute bits, and, if `accessed_varies`, the accessed
        /// bit, which can read back as the segment's present bit instead.
        fn segment_eq(
            restored: SegmentRegister,
            expected: SegmentRegister,
            accessed_varies: bool,
        ) -> bool {
            let ignored = if accessed_varies {
                RESERVED | ACCESSED
            } else {
                RESERVED
            };
            let accessed = |segment: SegmentRegister| segment.attributes & ACCESSED != 0;
            let present = restored.attributes & PRESENT != 0;
            SegmentRegister {
                attributes: restored.attributes & !ignored,
                ..restored
            } == SegmentRegister {
                attributes: expected.attributes & !ignored,
                ..expected
            } && (!accessed_varies
                || accessed(restored) == accessed(expected)
                || accessed(restored) == present)
        }

        if !caps.normalizes_restored_state {
            return restored == expected;
        }

        let without_segments = |registers: &Registers| {
            let none = SegmentRegister::default();
            Registers {
                cs: none,
                ds: none,
                es: none,
                fs: none,
                gs: none,
                ss: none,
                tr: none,
                ldtr: none,
                ..*registers
            }
        };
        without_segments(restored) == without_segments(expected)
            && segment_eq(restored.cs, expected.cs, true)
            && segment_eq(restored.ds, expected.ds, true)
            && segment_eq(restored.es, expected.es, true)
            && segment_eq(restored.fs, expected.fs, true)
            && segment_eq(restored.gs, expected.gs, true)
            && segment_eq(restored.ss, expected.ss, true)
            && segment_eq(restored.tr, expected.tr, true)
            && segment_eq(restored.ldtr, expected.ldtr, false)
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
    /// The number of pending NMIs when it is 2, and `None` otherwise, when
    /// `nmi_pending` says whether the one NMI that can be pending is.
    ///
    /// KVM holds a second NMI that arrives while no NMI is blocked and none is
    /// being injected, such as when two NMIs reach a VP before it runs: it
    /// injects one at once, and the other once the guest's handler for the
    /// first returns. `nmi_pending` is set then, as for one pending NMI, and
    /// 2 is the only count that a save holds: with an NMI blocked, or being
    /// injected, KVM holds one pending NMI. KVM reports the count from Linux
    /// 6.4 on: an earlier kernel reports only whether an NMI is pending, so a
    /// save on such a host holds no count and loses a second NMI, as before.
    /// A backend that holds at most one pending NMI never reports a count, and
    /// refuses to restore one, rather than drop the second NMI.
    ///
    /// The field needs no version. A saved state from a build that predates
    /// it, or that holds at most one pending NMI, has no such field, so
    /// restoring it sets the one pending NMI that `nmi_pending` gives, as
    /// before. A build that predates the field ignores it in a later state,
    /// so it restores two pending NMIs as one, losing one silently. Never
    /// change or reuse the field's number.
    #[mesh(7)]
    pub nmi_pending_count: Option<u8>,
    /// The kind of the interrupt shadow, when it is not the kind that restoring
    /// `None` sets.
    ///
    /// An interrupt shadow lasts until the next instruction ends. The one that
    /// a MOV SS or POP SS leaves also blocks NMIs and suppresses certain debug
    /// exceptions at that boundary, and the one that an STI leaves does not,
    /// as [`InterruptShadowKind`] says. Where a backend has a shadow of each
    /// kind, restoring `None` sets a MOV SS shadow, so its save reports `None`
    /// for that one and `Some(InterruptShadowKind::Sti)` for the other, and a
    /// state reads back from its restore as it was saved. KVM on an Intel CPU
    /// reports the kind. A backend whose shadow has one kind reports `None` for
    /// every shadow: AMD hardware, and Hyper-V's interrupt state register,
    /// which MSHV and WHP use, which has one bit for both. MSHV, WHP, and
    /// OpenHCL's backends refuse to restore a kind, rather than drop it, and
    /// KVM on an AMD CPU sets its one shadow, which reads back with no kind. A
    /// kind without `interrupt_shadow` describes nothing, and KVM refuses it.
    ///
    /// The field needs no version. A saved state from a build that predates
    /// it, or whose shadow is not an STI shadow, has no such field, which is
    /// byte for byte what earlier builds saved, so restoring it sets the
    /// shadow that `None` gives, as before. A build that predates the field
    /// ignores it in a later state, so it restores an STI shadow as a MOV SS
    /// shadow, as before. Never change or reuse the field's number.
    #[mesh(8)]
    pub interrupt_shadow_kind: Option<InterruptShadowKind>,
}

/// A kind of interrupt shadow, which lasts until the next instruction ends.
///
/// Every kind blocks maskable interrupts at that instruction boundary. The
/// shadow that a MOV SS or POP SS leaves also blocks NMIs and suppresses
/// certain debug exceptions there, and the one that an STI leaves does not. A
/// restored guest differs from the saved one at that boundary if the restore
/// sets one kind as the other. The MOV SS shadow is the safe one to set: an STI
/// shadow that restores as a MOV SS shadow only delays an NMI or a debug
/// exception by an instruction, while the reverse lets one split a MOV SS from
/// the instruction that follows it, which sets the stack pointer. So
/// [`Activity::interrupt_shadow_kind`] names only the kinds that restoring
/// `None` does not set, and no variant stands for the MOV SS shadow.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub enum InterruptShadowKind {
    /// `STI`, executed with RFLAGS.IF clear.
    #[mesh(1)]
    Sti,
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
            nmi_pending_count: None,
            // The interrupt state register has one bit for both kinds.
            interrupt_shadow_kind: None,
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
            nmi_pending_count: None,
            interrupt_shadow_kind: None,
        }
    }

    /// KVM, without `KVM_CAP_EXCEPTION_PAYLOAD`, reports an exception that is
    /// queued but not yet injected as pending and injected both, and restores
    /// only an injected one. An earlier save recorded such an exception as
    /// both, which reads back as the injected exception alone.
    ///
    /// Every other field is compared exactly, the count of pending NMIs and the
    /// kind of the interrupt shadow included: KVM restores the count that it is
    /// given, and reports it from Linux 6.4 on, and restores the kind that it
    /// is given where it has one. KVM on an AMD CPU, whose one shadow has no
    /// kind, sets a kind as that shadow and reads it back as none, which this
    /// check reports. Only a crafted state gets there: an AMD save holds no
    /// kind, and a snapshot of another vendor's CPU does not restore on AMD.
    fn restore_eq(caps: &X86PartitionCapabilities, restored: &Self, expected: &Self) -> bool {
        /// Returns `activity`, but for a pending exception that is the
        /// injected one, which is no event of its own.
        fn without_queued_exception(activity: &Activity) -> Activity {
            let &Activity {
                mp_state,
                nmi_pending,
                nmi_masked,
                interrupt_shadow,
                pending_event,
                pending_interruption,
                nmi_pending_count,
                interrupt_shadow_kind,
            } = activity;
            let pending_event = match (pending_event, pending_interruption) {
                (
                    Some(PendingEvent::Exception {
                        vector,
                        error_code,
                        parameter: 0,
                    }),
                    Some(PendingInterruption::Exception {
                        vector: injected_vector,
                        error_code: injected_error_code,
                    }),
                ) if (vector, error_code) == (injected_vector, injected_error_code) => None,
                (event, _) => event,
            };
            Activity {
                mp_state,
                nmi_pending,
                nmi_masked,
                interrupt_shadow,
                pending_event,
                pending_interruption,
                nmi_pending_count,
                interrupt_shadow_kind,
            }
        }

        if !caps.normalizes_restored_state {
            return restored == expected;
        }

        without_queued_exception(restored) == without_queued_exception(expected)
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

/// Why [`Xsave::write_standard`] cannot write an area in the standard format.
///
/// A save never returns an area that has too few words or selects a component
/// past its end, so only a crafted or damaged saved state holds one.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum XsaveError {
    /// The area has too few words to hold its legacy area and header.
    #[error("the XSAVE area has {words} words, too few for its legacy area and header")]
    AreaTooShort { words: usize },
    /// The area's XCOMP_BV and XSTATE_BV both select a component that ends past
    /// the end of the area.
    #[error("XSAVE component {component} ends past the end of the area, which has {words} words")]
    ComponentPastArea { component: u32, words: usize },
    /// The buffer is too short for the standard format of the area.
    #[error("the {len}-byte buffer is too short for the standard format of the XSAVE area")]
    BufferTooShort { len: usize },
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
    ///
    /// Fails, writing nothing, if the area has too few words for its legacy
    /// area and header, if a component that both its XCOMP_BV and its
    /// XSTATE_BV select ends past its end, or if `data` is too short for the
    /// standard format. A save never returns such an area, but a saved state
    /// can hold one, so this checks the area instead of assuming it.
    pub fn write_standard(
        &self,
        data: &mut [u8],
        caps: &X86PartitionCapabilities,
    ) -> Result<(), XsaveError> {
        let words = self.data.len();
        if words < XSAVE_VARIABLE_OFFSET / size_of::<u64>() {
            return Err(XsaveError::AreaTooShort { words });
        }
        if data.len() < XSAVE_VARIABLE_OFFSET {
            return Err(XsaveError::BufferTooShort { len: data.len() });
        }
        for (component, source, destination) in self.selected_components(caps) {
            if source.end > self.data.as_bytes().len() {
                return Err(XsaveError::ComponentPastArea { component, words });
            }
            if destination.end > data.len() {
                return Err(XsaveError::BufferTooShort { len: data.len() });
            }
        }

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
        for (_, source, destination) in self.selected_components(caps) {
            data[destination].copy_from_slice(&self.data.as_bytes()[source]);
        }
        Ok(())
    }

    /// Returns the components that both XCOMP_BV and XSTATE_BV of the area's
    /// header select, in order, each with its bytes in the area and its bytes
    /// in the standard format. The area must hold its header.
    fn selected_components(
        &self,
        caps: &X86PartitionCapabilities,
    ) -> impl Iterator<Item = (u32, Range<usize>, Range<usize>)> {
        let header = self.xsave_header();
        // In the compact format, each component that XCOMP_BV has follows the
        // one before it, at the next 64 bytes if it asks for alignment.
        let mut cur = XSAVE_VARIABLE_OFFSET;
        caps.xsave
            .variable_features(header.xcomp_bv)
            .filter_map(move |(i, feature)| {
                let len = feature.len as usize;
                if feature.align {
                    cur = (cur + 63) & !63;
                }
                let compact = cur..cur + len;
                cur += len;
                let offset = feature.offset as usize;
                (header.xstate_bv & (1 << i) != 0).then(|| (i, compact, offset..offset + len))
            })
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

/// The virtual MSRs of a VP.
///
/// The saved state holds IA32_SYSENTER_ESP (MSR 0x175) in field 3 and
/// IA32_SYSENTER_EIP (MSR 0x176) in field 4, in the order of their MSR
/// numbers. KVM, MSHV, and WHP have always saved them there, so existing saved
/// states restore only while the fields keep their numbers.
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
    pub sysenter_esp: u64,
    #[mesh(4)]
    pub sysenter_eip: u64,
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
            self.sysenter_esp,
            self.sysenter_eip,
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
            &mut self.sysenter_esp,
            &mut self.sysenter_eip,
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
            sysenter_esp: 0,
            sysenter_eip: 0,
            star: 0,
            lstar: 0,
            cstar: 0,
            sfmask: 0,
        }
    }

    /// KVM keeps some SYSENTER MSRs in a narrower form than the 64 bits that
    /// they hold, so a value that it did not save itself, as one from a host
    /// with a different width of virtual addresses, can read back changed
    /// (Linux v6.18):
    ///
    /// - It makes SYSENTER_EIP and SYSENTER_ESP canonical, by sign extension
    ///   from bit 47, or from bit 56 on a host with 5-level paging
    ///   (`__kvm_set_msr` in `x86.c`). An SVM host keeps only their low 32 bits
    ///   for a guest CPU that is not Intel-compatible (`svm_set_msr` in
    ///   `svm.c`).
    /// - A VMX host keeps SYSENTER_CS in a 32-bit field of the VMCS
    ///   (`vmx_set_msr` in `vmx.c`).
    fn restore_eq(caps: &X86PartitionCapabilities, restored: &Self, expected: &Self) -> bool {
        /// Returns whether `restored` is `expected`, or the low 32 bits of it.
        fn low_eq(restored: u64, expected: u64) -> bool {
            restored == expected || restored == expected & 0xffff_ffff
        }

        /// Returns whether `restored` is `expected`, or its canonical form at
        /// either width of a virtual address, or its low 32 bits.
        fn address_eq(restored: u64, expected: u64) -> bool {
            let canonical = |bits: u32| (((expected << (64 - bits)) as i64) >> (64 - bits)) as u64;
            restored == canonical(48) || restored == canonical(57) || low_eq(restored, expected)
        }

        if !caps.normalizes_restored_state {
            return restored == expected;
        }

        // The other MSRs are kept whole.
        let whole = |msrs: &VirtualMsrs| VirtualMsrs {
            sysenter_cs: 0,
            sysenter_esp: 0,
            sysenter_eip: 0,
            ..*msrs
        };
        whole(restored) == whole(expected)
            && low_eq(restored.sysenter_cs, expected.sysenter_cs)
            && address_eq(restored.sysenter_esp, expected.sysenter_esp)
            && address_eq(restored.sysenter_eip, expected.sysenter_eip)
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

    /// KVM keeps the low 32 bits of TSC_AUX for a guest CPU that is not
    /// Intel-compatible, since Intel reserves bits 63:32 and AMD's CPUs ignore
    /// them, and it refuses a restore that sets them for one that is
    /// (`__kvm_set_msr` in Linux v6.18 `x86.c`).
    fn restore_eq(caps: &X86PartitionCapabilities, restored: &Self, expected: &Self) -> bool {
        if !caps.normalizes_restored_state {
            return restored == expected;
        }

        restored.value == expected.value || restored.value == expected.value & 0xffff_ffff
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

/// The architectural IA32_SPEC_CTRL MSR, which holds the guest's speculation
/// controls: IBRS, STIBP, SSBD, and those that later processors added.
///
/// The guest can write it only if its CPUID enumerates a control of the MSR,
/// which [`X86PartitionCapabilities::spec_ctrl`] records, so the saved state
/// holds it only then. A VP that never saw a write holds 0, the reset value.
#[repr(C)]
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct SpecCtrl {
    #[mesh(1)]
    #[inspect(hex)]
    pub value: u64,
}

impl HvRegisterState<HvX64RegisterName, 1> for SpecCtrl {
    fn names(&self) -> &'static [HvX64RegisterName; 1] {
        &[HvX64RegisterName::SpecCtrl]
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

impl StateElement<X86PartitionCapabilities, X86VpInfo> for SpecCtrl {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.spec_ctrl
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self::default()
    }
}

/// The architectural IA32_UMWAIT_CONTROL MSR, which holds the longest that
/// `UMWAIT` and `TPAUSE` may wait, in bits 31:2, and in bit 0 whether they may
/// not enter the C0.2 state.
///
/// The guest can write it only if its CPUID enumerates WAITPKG, which
/// [`X86PartitionCapabilities::umwait_control`] records, so the saved state
/// holds it only then. Linux writes it on every CPU at boot. A VP that never
/// saw a write holds 0, the reset value.
#[repr(C)]
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct UmwaitControl {
    #[mesh(1)]
    #[inspect(hex)]
    pub value: u64,
}

impl HvRegisterState<HvX64RegisterName, 1> for UmwaitControl {
    fn names(&self) -> &'static [HvX64RegisterName; 1] {
        &[HvX64RegisterName::UmwaitControl]
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

impl StateElement<X86PartitionCapabilities, X86VpInfo> for UmwaitControl {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.umwait_control
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self::default()
    }
}

/// The architectural IA32_DEBUGCTL MSR, which holds the guest's debugging
/// controls: last-branch recording, branch single-stepping, bus-lock detection,
/// and the controls that later processors added.
///
/// A backend drops the controls that it does not virtualize, so the VP holds
/// only those that it kept, and the saved state holds what the VP does. A VP
/// that never saw a write holds 0, the reset value.
#[repr(C)]
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct DebugCtl {
    #[mesh(1)]
    #[inspect(hex)]
    pub value: u64,
}

impl HvRegisterState<HvX64RegisterName, 1> for DebugCtl {
    fn names(&self) -> &'static [HvX64RegisterName; 1] {
        &[HvX64RegisterName::DebugCtl]
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

impl StateElement<X86PartitionCapabilities, X86VpInfo> for DebugCtl {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.debug_ctl
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self::default()
    }
}

/// The feature and power controls that a guest can write on any CPU, which a
/// backend that lets the guest write them keeps in the VP:
/// `IA32_MISC_ENABLE`, `MSR_MISC_FEATURES_ENABLES` (CPUID faulting), and
/// `IA32_POWER_CTL`.
///
/// [`X86PartitionCapabilities::misc_msrs`] records that the backend holds them.
/// A VP that never saw a write holds the reset value of each: `IA32_MISC_ENABLE`
/// as [`x86defs::MISC_ENABLE_DEFAULT`] gives it, and 0 for the others.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
#[inspect(hex)]
pub struct MiscMsrs {
    #[mesh(1)]
    pub misc_enable: u64,
    #[mesh(2)]
    pub misc_features_enables: u64,
    #[mesh(3)]
    pub power_ctl: u64,
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for MiscMsrs {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.misc_msrs
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self {
            misc_enable: x86defs::MISC_ENABLE_DEFAULT.into(),
            misc_features_enables: 0,
            power_ctl: 0,
        }
    }
}

/// The registers of one machine-check error-reporting bank.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
#[inspect(hex)]
pub struct MachineCheckBank {
    /// `IA32_MCi_CTL`.
    #[mesh(1)]
    pub ctl: u64,
    /// `IA32_MCi_STATUS`.
    #[mesh(2)]
    pub status: u64,
    /// `IA32_MCi_ADDR`.
    #[mesh(3)]
    pub addr: u64,
    /// `IA32_MCi_MISC`.
    #[mesh(4)]
    pub misc: u64,
    /// `IA32_MCi_CTL2`, the corrected-error (CMCI) control, which is 0 on a VP
    /// that has no CMCI.
    #[mesh(5)]
    pub ctl2: u64,
}

/// The machine-check state of a VP: `IA32_MCG_STATUS`, `IA32_MCG_CTL`, and the
/// registers of each error-reporting bank.
///
/// A backend that lets the guest write these registers holds them in the VP,
/// and [`X86PartitionCapabilities::machine_check`] records that the saved state
/// holds them. The state lists the banks in the order of their numbers, as many
/// as the VP has, which its `IA32_MCG_CAP` gives. A restore sets the banks that
/// the state lists, puts the VP's other banks back at the state that the
/// backend gave them when it bound the VP, and refuses a state that lists more
/// banks than the VP has. The state at reset lists none, so it resets every
/// bank.
#[derive(Default, Debug, Clone, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct MachineCheck {
    /// `IA32_MCG_STATUS`.
    #[mesh(1)]
    #[inspect(hex)]
    pub status: u64,
    /// `IA32_MCG_CTL`, which is 0 on a VP whose `IA32_MCG_CAP` lacks
    /// `MCG_CTL_P`.
    #[mesh(2)]
    #[inspect(hex)]
    pub ctl: u64,
    /// The error-reporting banks.
    #[mesh(3)]
    #[inspect(iter_by_index)]
    pub banks: Vec<MachineCheckBank>,
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for MachineCheck {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.machine_check
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self::default()
    }

    // The banks that a backend gives a new VP are not the same on every host:
    // KVM enables every error of every bank only on the hosts that ask it to
    // advertise CMCI, which are Intel's. The state at reset lists no bank, so
    // it cannot say which.
    fn can_compare(_caps: &X86PartitionCapabilities) -> bool {
        false
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

// A VP state element is one field of the saved state, an `Option<T>` under a
// field number of its own. A saved state from a build that predates an element
// has no such field, so restoring it sets nothing and the VP keeps its reset
// value, as it did before the element existed; and a build that predates the
// element ignores the field. Neither needs a version, so never change or
// reuse a field number.
//
// To add an element for state that only some guests can have, such as an MSR
// that a CPUID feature enumerates:
// - take the next free number below 100;
// - make `StateElement::is_present` follow a capability of
//   `X86PartitionCapabilities` that `from_cpuid` derives from the guest's
//   CPUID, so that a save holds the element only for a guest that can have
//   changed it, or, for state that every guest can have, that `from_cpuid`
//   leaves clear for a backend that can access the state to set;
// - implement the element's two accessors in every backend, which clears the
//   capability if it cannot access the state, and document in the Guide's
//   snapshot format how an older state and an older reader treat the element;
// - a backend that can access the state on some hosts only sets or clears the
//   capability for the host: the KVM backend clears it for an MSR that the
//   host's `KVM_GET_MSR_INDEX_LIST` lacks, unless every host's KVM has the MSR
//   and only some list it, as for `IA32_DEBUGCTL`.
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
    // Saved after the APIC state, whose LAPIC timer count a time ABI restore
    // dates with the saved TSC.
    (11, "tsc", tsc, set_tsc, Tsc),
    (12, "cet", cet, set_cet, Cet),
    (13, "cet_ss", cet_ss, set_cet_ss, CetSs),
    (14, "tsc_aux", tsc_aux, set_tsc_aux, TscAux),
    // The deadline must be restored after APIC mode and guest TSC.
    (15, "tsc_deadline", tsc_deadline, set_tsc_deadline, TscDeadline),
    (16, "spec_ctrl", spec_ctrl, set_spec_ctrl, SpecCtrl),
    (17, "umwait_control", umwait_control, set_umwait_control, UmwaitControl),
    (18, "debug_ctl", debug_ctl, set_debug_ctl, DebugCtl),
    (19, "misc_msrs", misc_msrs, set_misc_msrs, MiscMsrs),
    (20, "machine_check", machine_check, set_machine_check, MachineCheck),

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

impl VpSavedState {
    /// Removes the saved TSC, so restoring this state leaves the VP's TSC to
    /// the NVX time ABI's synchronized TSC set.
    pub fn clear_tsc(&mut self) {
        self.tsc = None;
    }

    /// Takes the LAPIC timer out of the saved APIC state, with the saved TSC
    /// that dates its count, so restoring this state runs no timer until
    /// [`Apic::restore_one_shot_timer`] sets it. Returns `None` if the state
    /// holds no APIC state.
    pub fn take_one_shot_timer(&mut self) -> Option<SavedOneShotTimer> {
        let saved_tsc = self.tsc.as_ref().map(|tsc| tsc.value);
        self.apic
            .as_mut()
            .map(|apic| apic.take_one_shot_timer(saved_tsc))
    }
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

    #[test]
    fn clear_tsc_keeps_the_other_elements() {
        let mut state = VpSavedState {
            tsc: Some(Tsc { value: 7 }),
            tsc_aux: Some(TscAux { value: 3 }),
            ..Default::default()
        };
        state.clear_tsc();
        assert_eq!(
            state,
            VpSavedState {
                tsc_aux: Some(TscAux { value: 3 }),
                ..Default::default()
            }
        );
    }

    #[test]
    fn take_one_shot_timer_dates_the_count_with_the_saved_tsc() {
        let registers = ApicRegisters {
            lvt_timer: 0x40,
            timer_icr: 1_000,
            timer_ccr: 400,
            ..FromZeros::new_zeroed()
        };
        let mut state = VpSavedState {
            apic: Some(Apic::new(ApicBase::new(), registers, [0; 8])),
            tsc: Some(Tsc { value: 7 }),
            ..Default::default()
        };
        assert_eq!(
            state.take_one_shot_timer(),
            Some(SavedOneShotTimer {
                initial_count: 1_000,
                current_count: 400,
                saved_tsc: Some(7),
            })
        );
        let apic = state.apic.as_ref().unwrap().registers();
        assert_eq!(
            (apic.timer_icr, apic.timer_ccr, apic.lvt_timer),
            (0, 0, 0x40)
        );
        // The TSC stays for the caller to clear.
        assert_eq!(state.tsc, Some(Tsc { value: 7 }));

        let mut without_tsc = VpSavedState {
            apic: Some(Apic::new(ApicBase::new(), registers, [0; 8])),
            ..Default::default()
        };
        assert_eq!(without_tsc.take_one_shot_timer().unwrap().saved_tsc, None);
        assert_eq!(VpSavedState::default().take_one_shot_timer(), None);
    }

    // Distinct values, so that a mix-up of two registers shows.
    const REGISTERS: [(HvX64RegisterName, u64); 8] = [
        (HvX64RegisterName::KernelGsBase, 0x10),
        (HvX64RegisterName::SysenterCs, 0x20),
        (HvX64RegisterName::SysenterEip, 0x1111),
        (HvX64RegisterName::SysenterEsp, 0x2222),
        (HvX64RegisterName::Star, 0x30),
        (HvX64RegisterName::Lstar, 0x40),
        (HvX64RegisterName::Cstar, 0x50),
        (HvX64RegisterName::Sfmask, 0x60),
    ];

    /// The virtual MSRs that hold the values of `REGISTERS`.
    fn virtual_msrs() -> VirtualMsrs {
        VirtualMsrs {
            kernel_gs_base: 0x10,
            sysenter_cs: 0x20,
            sysenter_eip: 0x1111,
            sysenter_esp: 0x2222,
            star: 0x30,
            lstar: 0x40,
            cstar: 0x50,
            sfmask: 0x60,
        }
    }

    #[test]
    fn virtual_msrs_write_each_register_from_its_own_field() {
        let msrs = virtual_msrs();
        let mut values = [HvRegisterValue::from(0u64); 8];
        msrs.get_values(values.iter_mut());
        let written = msrs
            .names()
            .iter()
            .copied()
            .zip(values.map(|value| value.as_u64()))
            .collect::<Vec<_>>();
        assert_eq!(written.len(), REGISTERS.len());
        for register in REGISTERS {
            assert!(
                written.contains(&register),
                "{register:?} is not written: {written:?}"
            );
        }
    }

    #[test]
    fn virtual_msrs_read_each_register_into_its_own_field() {
        let mut msrs = VirtualMsrs::default();
        let values = msrs
            .names()
            .iter()
            .map(|name| {
                let (_, value) = REGISTERS
                    .iter()
                    .find(|(register, _)| register == name)
                    .expect("the test holds every register of the virtual MSRs");
                HvRegisterValue::from(*value)
            })
            .collect::<Vec<_>>();
        msrs.set_values(values.into_iter());
        assert_eq!(msrs, virtual_msrs());
    }

    /// The saved fields of a `VirtualMsrs` named for the MSRs that they hold,
    /// as the saved states of KVM, MSHV, and WHP have always had them:
    /// IA32_SYSENTER_ESP (0x175) in field 3 and IA32_SYSENTER_EIP (0x176) in
    /// field 4.
    #[derive(Debug, PartialEq, Protobuf)]
    struct SavedMsrs {
        #[mesh(1)]
        kernel_gs_base: u64,
        #[mesh(2)]
        ia32_sysenter_cs: u64,
        #[mesh(3)]
        ia32_sysenter_esp: u64,
        #[mesh(4)]
        ia32_sysenter_eip: u64,
        #[mesh(5)]
        star: u64,
        #[mesh(6)]
        lstar: u64,
        #[mesh(7)]
        cstar: u64,
        #[mesh(8)]
        sfmask: u64,
    }

    fn saved_msrs() -> SavedMsrs {
        SavedMsrs {
            kernel_gs_base: 0x10,
            ia32_sysenter_cs: 0x20,
            ia32_sysenter_esp: 0x2222,
            ia32_sysenter_eip: 0x1111,
            star: 0x30,
            lstar: 0x40,
            cstar: 0x50,
            sfmask: 0x60,
        }
    }

    #[test]
    fn virtual_msrs_keep_the_layout_of_earlier_saved_states() {
        // A saved state of an earlier release restores to the right registers,
        // and a release that restores this one's saved state does as well.
        assert_eq!(
            mesh_protobuf::decode::<VirtualMsrs>(&mesh_protobuf::encode(saved_msrs())).unwrap(),
            virtual_msrs()
        );
        assert_eq!(
            mesh_protobuf::decode::<SavedMsrs>(&mesh_protobuf::encode(virtual_msrs())).unwrap(),
            saved_msrs()
        );
    }

    /// The saved state of a build that predates the SPEC_CTRL element, which
    /// has none of its field, shown here with two of the other elements.
    #[derive(Debug, PartialEq, Protobuf)]
    struct EarlierVpSavedState {
        #[mesh(11)]
        tsc: Option<Tsc>,
        #[mesh(14)]
        tsc_aux: Option<TscAux>,
    }

    #[test]
    fn spec_ctrl_needs_no_version_to_coexist_with_earlier_builds() {
        let earlier = || EarlierVpSavedState {
            tsc: Some(Tsc { value: 7 }),
            tsc_aux: Some(TscAux { value: 3 }),
        };
        let current = |spec_ctrl| VpSavedState {
            tsc: Some(Tsc { value: 7 }),
            tsc_aux: Some(TscAux { value: 3 }),
            spec_ctrl,
            ..Default::default()
        };

        // A saved state of an earlier build holds no SPEC_CTRL, so restoring
        // it sets none.
        assert_eq!(
            mesh_protobuf::decode::<VpSavedState>(&mesh_protobuf::encode(earlier())).unwrap(),
            current(None)
        );
        // An earlier build reads the saved state of this one and ignores the
        // element.
        assert_eq!(
            mesh_protobuf::decode::<EarlierVpSavedState>(&mesh_protobuf::encode(current(Some(
                SpecCtrl { value: 5 }
            ))))
            .unwrap(),
            earlier()
        );

        // The element is field 16 of the saved state, a message with the value
        // in its field 1: bytes that every build that holds it reads, and
        // that no later change may alter.
        assert_eq!(
            mesh_protobuf::encode(VpSavedState {
                spec_ctrl: Some(SpecCtrl { value: 5 }),
                ..Default::default()
            }),
            [0x82, 0x01, 0x02, 0x08, 0x05]
        );
        assert_eq!(
            mesh_protobuf::decode::<VpSavedState>(&[0x82, 0x01, 0x02, 0x08, 0x05]).unwrap(),
            VpSavedState {
                spec_ctrl: Some(SpecCtrl { value: 5 }),
                ..Default::default()
            }
        );
    }

    fn machine_check_state() -> MachineCheck {
        MachineCheck {
            status: 1,
            ctl: 2,
            banks: vec![MachineCheckBank {
                ctl: 3,
                status: 4,
                addr: 5,
                misc: 6,
                ctl2: 7,
            }],
        }
    }

    #[test]
    fn the_msr_elements_need_no_version_to_coexist_with_earlier_builds() {
        let earlier = || EarlierVpSavedState {
            tsc: Some(Tsc { value: 7 }),
            tsc_aux: Some(TscAux { value: 3 }),
        };
        let current = || VpSavedState {
            tsc: Some(Tsc { value: 7 }),
            tsc_aux: Some(TscAux { value: 3 }),
            umwait_control: Some(UmwaitControl { value: 5 }),
            debug_ctl: Some(DebugCtl { value: 5 }),
            misc_msrs: Some(MiscMsrs {
                misc_enable: 1,
                misc_features_enables: 2,
                power_ctl: 3,
            }),
            machine_check: Some(machine_check_state()),
            ..Default::default()
        };

        // A saved state of an earlier build holds none of the elements, so
        // restoring it sets none of them.
        assert_eq!(
            mesh_protobuf::decode::<VpSavedState>(&mesh_protobuf::encode(earlier())).unwrap(),
            VpSavedState {
                tsc: Some(Tsc { value: 7 }),
                tsc_aux: Some(TscAux { value: 3 }),
                ..Default::default()
            }
        );
        // An earlier build reads the saved state of this one and ignores them.
        assert_eq!(
            mesh_protobuf::decode::<EarlierVpSavedState>(&mesh_protobuf::encode(current()))
                .unwrap(),
            earlier()
        );
        // This build reads its own.
        assert_eq!(
            mesh_protobuf::decode::<VpSavedState>(&mesh_protobuf::encode(current())).unwrap(),
            current()
        );

        // Each element is a message in a field of the saved state, which these
        // bytes pin: every build that holds the element reads them, and no
        // later change may alter them. The machine-check state's banks are
        // the repeated field 3.
        for (state, bytes) in [
            (
                VpSavedState {
                    umwait_control: Some(UmwaitControl { value: 5 }),
                    ..Default::default()
                },
                &[0x8a, 0x01, 0x02, 0x08, 0x05][..],
            ),
            (
                VpSavedState {
                    debug_ctl: Some(DebugCtl { value: 5 }),
                    ..Default::default()
                },
                &[0x92, 0x01, 0x02, 0x08, 0x05],
            ),
            (
                VpSavedState {
                    misc_msrs: Some(MiscMsrs {
                        misc_enable: 1,
                        misc_features_enables: 2,
                        power_ctl: 3,
                    }),
                    ..Default::default()
                },
                &[0x9a, 0x01, 0x06, 0x08, 0x01, 0x10, 0x02, 0x18, 0x03],
            ),
            (
                VpSavedState {
                    machine_check: Some(machine_check_state()),
                    ..Default::default()
                },
                &[
                    0xa2, 0x01, 0x10, 0x08, 0x01, 0x10, 0x02, 0x1a, 0x0a, 0x08, 0x03, 0x10, 0x04,
                    0x18, 0x05, 0x20, 0x06, 0x28, 0x07,
                ],
            ),
        ] {
            assert_eq!(mesh_protobuf::encode(state), bytes);
            assert_eq!(
                mesh_protobuf::encode(mesh_protobuf::decode::<VpSavedState>(bytes).unwrap()),
                bytes
            );
        }
    }

    /// The components beyond x87 and SSE of the XSAVE layout of the tests of
    /// `write_standard`, each `(index, size, standard offset, aligned)`. In the
    /// compact format, with all of them present, they sit at the bytes 576
    /// (AVX), 832, 896, 1408, and 1472, which is 1416 rounded up to the 64
    /// bytes that component 17 asks for, and the area ends at 1536, which is
    /// 192 words.
    const LAYOUT: [(u32, u32, u32, bool); 5] = [
        (2, 256, 576, false),
        (5, 64, 1088, false),
        (6, 512, 1152, false),
        (9, 8, 2688, false),
        (17, 64, 2752, true),
    ];

    /// Returns the capabilities of a CPU with XSAVE whose components beyond
    /// x87 and SSE are `components`, each `(index, size, standard offset,
    /// aligned)`.
    fn caps_with_components(components: &[(u32, u32, u32, bool)]) -> X86PartitionCapabilities {
        let topology = vm_topology::processor::TopologyBuilder::new_x86()
            .x2apic(vm_topology::processor::x86::X2ApicState::Supported)
            .build(1)
            .unwrap();
        let features = components
            .iter()
            .fold(3, |features, &(index, ..)| features | 1u64 << index);
        X86PartitionCapabilities::from_cpuid(
            &topology,
            &mut |leaf, subleaf| match (leaf, subleaf) {
                (0, _) => [0xd, 0x756e_6547, 0x6c65_746e, 0x4965_6e69],
                (1, _) => [0, 0, (1 << 26) | (1 << 21), 0],
                (0xd, 0) => [features as u32, 0, 4096, (features >> 32) as u32],
                (0xd, index @ 2..) => components
                    .iter()
                    .find(|component| component.0 == index)
                    .map_or([0; 4], |&(_, size, offset, aligned)| {
                        [size, offset, u32::from(aligned) << 1, 0]
                    }),
                _ => [0; 4],
            },
        )
        .unwrap()
    }

    /// Returns an XSAVE area of `words` words, whose header, when it holds one,
    /// has XSTATE_BV `xstate_bv` and XCOMP_BV `xcomp_bv`, and whose other words
    /// are all different.
    fn xsave_area(words: usize, xstate_bv: u64, xcomp_bv: u64) -> Xsave {
        let mut data: Vec<u64> = (0..words as u64).map(|word| 0xa5a5 << 48 | word).collect();
        if words > 65 {
            data[64] = xstate_bv;
            data[65] = xcomp_bv;
        }
        Xsave { data }
    }

    /// Writes `area` into a buffer of `len` bytes that holds 0xee, which a
    /// refusal must leave as it was, and returns the outcome.
    fn try_write(
        area: &Xsave,
        len: usize,
        caps: &X86PartitionCapabilities,
    ) -> Result<(), XsaveError> {
        let mut buffer = vec![0xee; len];
        let result = area.write_standard(&mut buffer, caps);
        if result.is_err() {
            assert!(
                buffer.iter().all(|&byte| byte == 0xee),
                "a refusal wrote to the buffer, {result:?}"
            );
        }
        result
    }

    #[test]
    fn write_standard_refuses_an_area_without_its_legacy_area_and_header() {
        let caps = caps_with_components(&LAYOUT);
        let xcomp_bv = XCOMP_COMPRESSED | caps.xsave.features;
        // The legacy area takes 64 words, and the header 8 more.
        for words in 0..72 {
            assert_eq!(
                try_write(&xsave_area(words, 3, xcomp_bv), 4096, &caps),
                Err(XsaveError::AreaTooShort { words })
            );
        }
        try_write(&xsave_area(72, 3, xcomp_bv), 4096, &caps).unwrap();
    }

    #[test]
    fn write_standard_refuses_a_selected_component_that_ends_past_the_area() {
        let caps = caps_with_components(&LAYOUT);
        let all = XCOMP_COMPRESSED | caps.xsave.features;
        let past = |component, words| Err(XsaveError::ComponentPastArea { component, words });
        // The area that a header selects components in, its words, and the
        // outcome of writing it.
        for (xstate_bv, xcomp_bv, words, expected) in [
            // AVX ends at the byte 832, the word 104: an area of the legacy
            // area and the header alone, and one a word short, do not hold it.
            (1 << 2, all, 72, past(2, 72)),
            (1 << 2, all, 103, past(2, 103)),
            (1 << 2, all, 104, Ok(())),
            // PKRU ends at the byte 1416, the word 177.
            (1 << 9, all, 176, past(9, 176)),
            (1 << 9, all, 177, Ok(())),
            // Component 17 asks for 64-byte alignment, so it starts at the byte
            // 1472, not 1416 where PKRU ends, and ends at the byte 1536, the
            // word 192, not 1480, the word 185.
            (1 << 17, all, 185, past(17, 185)),
            (1 << 17, all, 191, past(17, 191)),
            (1 << 17, all, 192, Ok(())),
            // The first selected component that does not fit is the one named.
            (1 << 2 | 1 << 17, all, 191, past(17, 191)),
            (1 << 5 | 1 << 17, all, 103, past(5, 103)),
            // Where the area puts a component depends on the components that
            // XCOMP_BV has before it, whether or not XSTATE_BV selects them:
            // component 17 alone starts at the byte 576, and after component
            // 5 at the byte 640.
            (1 << 17, XCOMP_COMPRESSED | 1 << 17, 79, past(17, 79)),
            (1 << 17, XCOMP_COMPRESSED | 1 << 17, 80, Ok(())),
            (
                1 << 17,
                XCOMP_COMPRESSED | 1 << 5 | 1 << 17,
                87,
                past(17, 87),
            ),
            (1 << 17, XCOMP_COMPRESSED | 1 << 5 | 1 << 17, 88, Ok(())),
            // A component that only one of the two masks selects is not read.
            (3, all, 72, Ok(())),
            (1 << 17 | 3, XCOMP_COMPRESSED | 3, 72, Ok(())),
        ] {
            let area = xsave_area(words, xstate_bv, xcomp_bv);
            assert_eq!(
                try_write(&area, 4096, &caps),
                expected,
                "XSTATE_BV {xstate_bv:#x}, XCOMP_BV {xcomp_bv:#x}, {words} words"
            );
        }
    }

    #[test]
    fn write_standard_refuses_a_buffer_that_is_too_short_for_the_standard_format() {
        let caps = caps_with_components(&LAYOUT);
        let all = XCOMP_COMPRESSED | caps.xsave.features;
        let short = |len| Err(XsaveError::BufferTooShort { len });
        let area = |xstate_bv| xsave_area(192, xstate_bv, all);
        // The legacy area and the header take 576 bytes.
        assert_eq!(try_write(&area(3), 0, &caps), short(0));
        assert_eq!(try_write(&area(3), 575, &caps), short(575));
        try_write(&area(3), 576, &caps).unwrap();
        // Component 17 ends at the standard offset 2752 + 64, and the earlier
        // components, which XSTATE_BV leaves out, do not need their room.
        assert_eq!(try_write(&area(1 << 17), 2815, &caps), short(2815));
        try_write(&area(1 << 17), 2816, &caps).unwrap();
        try_write(&area(1 << 2), 832, &caps).unwrap();
    }

    #[test]
    fn write_standard_writes_each_selected_component_at_its_standard_offset() {
        let caps = caps_with_components(&LAYOUT);
        assert_eq!(caps.xsave.compact_len, 1536);
        // XSTATE_BV selects all but component 5, which XCOMP_BV has.
        let xstate_bv = caps.xsave.features & !(1 << 5);
        let area = xsave_area(192, xstate_bv, XCOMP_COMPRESSED | caps.xsave.features);
        let compact = area.compact();
        let mut standard = [0xee; 4096];
        area.write_standard(&mut standard, &caps).unwrap();

        // The legacy area, whose x87 and SSE state XSTATE_BV selects, and the
        // header with XCOMP_BV cleared, then each selected component at its
        // standard offset from its compact place, and nothing else.
        let mut expected = [0xee; 4096];
        expected[..576].copy_from_slice(&compact[..576]);
        expected[520..528].fill(0);
        for (size, compact_place, standard_offset) in [
            (256, 576, 576),
            (512, 896, 1152),
            (8, 1408, 2688),
            (64, 1472, 2752),
        ] {
            expected[standard_offset..standard_offset + size]
                .copy_from_slice(&compact[compact_place..compact_place + size]);
        }
        let first_difference = standard.iter().zip(&expected).position(|(a, b)| a != b);
        assert_eq!(first_difference, None, "the first byte that differs");
    }

    #[test]
    fn write_standard_refuses_exactly_the_areas_that_do_not_fit() {
        // The words that an area needs, and the bytes that a buffer needs, for
        // the legacy area, the header, and each component that both masks
        // select, which the layout puts at its place in the area and at its
        // standard offset in the buffer.
        fn needs(caps: &X86PartitionCapabilities, xstate_bv: u64, xcomp_bv: u64) -> (usize, usize) {
            let (mut words, mut bytes) = (72, 576);
            let mut place = 576;
            for index in 2..63 {
                if xcomp_bv >> index & 1 == 0 {
                    continue;
                }
                let feature = &caps.xsave.feature_info[index];
                if feature.align {
                    place = usize::next_multiple_of(place, 64);
                }
                let len = feature.len as usize;
                if xstate_bv >> index & 1 != 0 {
                    words = words.max((place + len).div_ceil(8));
                    bytes = bytes.max(feature.offset as usize + len);
                }
                place += len;
            }
            (words, bytes)
        }

        // A generator of numbers below a bound, which gives the same ones every
        // run, so that a failure repeats.
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut random = move |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };

        let mut outcomes = [0; 2];
        for _ in 0..5000 {
            // A layout of up to 6 components, which may overlap, lie past 4096
            // bytes, have no size, or repeat an index.
            let mut caps = caps_with_components(&[]);
            let mut defined = 0u64;
            for _ in 0..random(7) {
                let index = 2 + random(61) as usize;
                caps.xsave.feature_info[index] = crate::x86::XsaveFeature {
                    offset: 576 + random(3700) as u32,
                    len: random(300) as u32,
                    align: random(2) == 0,
                };
                defined |= 1 << index;
            }
            let xcomp_bv = XCOMP_COMPRESSED | defined & random(u64::MAX);
            let xstate_bv = match random(3) {
                0 => defined,
                1 => xcomp_bv & random(u64::MAX),
                _ => random(u64::MAX),
            };

            // Sizes of the area and the buffer that are one short of what they
            // need, exact, and one more, or anything.
            let (needed_words, needed_bytes) = needs(&caps, xstate_bv, xcomp_bv);
            let words = match random(4) {
                0 => random(needed_words as u64 + 8) as usize,
                _ => needed_words - 1 + random(3) as usize,
            };
            let buffer_len = match random(4) {
                0 => random(5000) as usize,
                _ => needed_bytes - 1 + random(3) as usize,
            };
            let fits = words >= needed_words && buffer_len >= needed_bytes;

            let area = xsave_area(words, xstate_bv, xcomp_bv);
            let result = try_write(&area, buffer_len, &caps);
            assert_eq!(
                result.is_ok(),
                fits,
                "{words} words, XSTATE_BV {xstate_bv:#x}, XCOMP_BV {xcomp_bv:#x}, a buffer of {buffer_len} bytes, {result:?}"
            );
            outcomes[usize::from(fits)] += 1;
        }
        assert!(
            outcomes.iter().all(|&count| count >= 1000),
            "both outcomes are drawn: {outcomes:?}"
        );
    }

    /// The activity of a build that predates `Activity::nmi_pending_count`.
    #[derive(Debug, PartialEq, Protobuf)]
    struct EarlierActivity {
        #[mesh(1)]
        mp_state: MpState,
        #[mesh(2)]
        nmi_pending: bool,
        #[mesh(3)]
        nmi_masked: bool,
        #[mesh(4)]
        interrupt_shadow: bool,
        #[mesh(5)]
        pending_event: Option<PendingEvent>,
        #[mesh(6)]
        pending_interruption: Option<PendingInterruption>,
    }

    /// The number of the field of `Activity::nmi_pending_count`, which its
    /// `mesh` attribute must carry.
    const NMI_PENDING_COUNT_FIELD: u8 = 7;

    #[test]
    fn nmi_pending_count_needs_no_version_to_coexist_with_earlier_builds() {
        let earlier = || EarlierActivity {
            mp_state: MpState::Halted,
            nmi_pending: true,
            nmi_masked: false,
            interrupt_shadow: true,
            pending_event: None,
            pending_interruption: Some(PendingInterruption::Interrupt { vector: 0x30 }),
        };
        let current = |nmi_pending_count| Activity {
            mp_state: MpState::Halted,
            nmi_pending: true,
            interrupt_shadow: true,
            pending_interruption: Some(PendingInterruption::Interrupt { vector: 0x30 }),
            nmi_pending_count,
            ..Default::default()
        };

        // A saved state of an earlier build holds no count, so restoring it
        // sets the one pending NMI that `nmi_pending` gives, as that build did.
        assert_eq!(
            mesh_protobuf::decode::<Activity>(&mesh_protobuf::encode(earlier())).unwrap(),
            current(None)
        );
        // A state with at most one pending NMI is the one that the earlier
        // build saved, byte for byte.
        assert_eq!(
            mesh_protobuf::encode(current(None)),
            mesh_protobuf::encode(earlier())
        );
        // An earlier build reads the state of two pending NMIs and ignores the
        // count, so it restores one NMI.
        assert_eq!(
            mesh_protobuf::decode::<EarlierActivity>(&mesh_protobuf::encode(current(Some(2))))
                .unwrap(),
            earlier()
        );

        // The count is a varint under the field's number, after the earlier
        // fields: bytes that every build that holds it reads, and that no later
        // change may alter.
        let two = [NMI_PENDING_COUNT_FIELD << 3, 2];
        assert_eq!(
            mesh_protobuf::encode(current(Some(2))),
            [mesh_protobuf::encode(earlier()), two.to_vec()].concat()
        );
        assert_eq!(
            mesh_protobuf::encode(Activity {
                nmi_pending: true,
                nmi_pending_count: Some(2),
                ..Default::default()
            }),
            [0x0a, 0x02, 0x0a, 0x00, 0x10, 0x01, two[0], two[1]]
        );
        assert_eq!(
            mesh_protobuf::decode::<Activity>(&[
                0x0a, 0x02, 0x0a, 0x00, 0x10, 0x01, two[0], two[1]
            ])
            .unwrap(),
            Activity {
                nmi_pending: true,
                nmi_pending_count: Some(2),
                ..Default::default()
            }
        );
    }

    /// Returns the capabilities of a hypervisor that does or does not
    /// normalize the state that a restore sets.
    fn caps(normalizes_restored_state: bool) -> X86PartitionCapabilities {
        let topology = vm_topology::processor::TopologyBuilder::new_x86()
            .build(1)
            .unwrap();
        let x2apic = topology.apic_mode() != vm_topology::processor::x86::ApicMode::XApic;
        let mut caps =
            X86PartitionCapabilities::from_cpuid(&topology, &mut |function, _| match function {
                0 => [1, 0, 0, 0],
                1 => [0, 0, u32::from(x2apic) << 21, 0],
                _ => [0; 4],
            })
            .unwrap();
        caps.normalizes_restored_state = normalizes_restored_state;
        caps
    }

    /// Returns whether a restore accepts `restored`, which it read back, as the
    /// state `expected` that it set, on a hypervisor that does or does not
    /// normalize the state that a restore sets.
    fn accepted<T: StateElement<X86PartitionCapabilities, X86VpInfo>>(
        normalizes_restored_state: bool,
        restored: &T,
        expected: &T,
    ) -> bool {
        T::restore_eq(&caps(normalizes_restored_state), restored, expected)
    }

    #[test]
    fn a_restore_accepts_the_segment_attributes_that_kvm_normalizes() {
        /// Returns the registers of a reset VP but for the segment register
        /// `index`, which has `attributes`.
        fn with_segment(index: usize, attributes: u16) -> Registers {
            let segment = SegmentRegister {
                attributes,
                ..SegmentRegister::default()
            };
            let mut registers = Registers::default();
            match index {
                0 => registers.cs = segment,
                1 => registers.ds = segment,
                2 => registers.es = segment,
                3 => registers.fs = segment,
                4 => registers.gs = segment,
                5 => registers.ss = segment,
                6 => registers.tr = segment,
                _ => registers.ldtr = segment,
            }
            registers
        }

        const LDTR: usize = 7;
        for index in 0..=LDTR {
            let check = |restored, expected| {
                accepted(
                    true,
                    &with_segment(index, restored),
                    &with_segment(index, expected),
                )
            };

            // The reserved bits read back clear.
            assert!(check(0x0092, 0x0f92), "segment {index}");
            // Every segment register but the LDTR can read back accessed. It
            // reads back as it was set, or as the segment's present bit: KVM
            // sets the bit of a segment, and the backend clears it of a segment
            // that is not present, so a saved state can hold either. A segment
            // that is present is not given a clear accessed bit, nor one that
            // is not present a set one.
            assert_eq!(check(0x0093, 0x0092), index != LDTR, "segment {index}");
            assert_eq!(check(0x0012, 0x0013), index != LDTR, "segment {index}");
            assert!(!check(0x0013, 0x0012), "segment {index}");
            assert!(!check(0x0092, 0x0093), "segment {index}");
            assert!(check(0x0093, 0x0093), "segment {index}");
            assert!(check(0x0012, 0x0012), "segment {index}");
            // Nothing else changes: the other bits of the type and attributes,
            // and the rest of the registers.
            for other in [
                0x0090, 0x0096, 0x0082, 0x0012, 0x1092, 0x2092, 0x4092, 0x8092,
            ] {
                assert!(!check(other, 0x0f92), "segment {index}: {other:#x}");
            }
            let moved = Registers {
                rax: 1,
                ..with_segment(index, 0x0092)
            };
            assert!(!accepted(true, &moved, &with_segment(index, 0x0f92)));
            assert!(!accepted(true, &with_segment(index, 0x0092), &moved));

            // A hypervisor that does not normalize keeps every bit.
            assert!(accepted(
                false,
                &with_segment(index, 0x0f92),
                &with_segment(index, 0x0f92)
            ));
            assert!(!accepted(
                false,
                &with_segment(index, 0x0092),
                &with_segment(index, 0x0f92)
            ));
            assert!(!accepted(
                false,
                &with_segment(index, 0x0093),
                &with_segment(index, 0x0092)
            ));
        }
    }

    fn exception(vector: u8, error_code: Option<u32>) -> PendingInterruption {
        PendingInterruption::Exception { vector, error_code }
    }

    fn pending_exception(vector: u8, error_code: Option<u32>, parameter: u64) -> PendingEvent {
        PendingEvent::Exception {
            vector,
            error_code,
            parameter,
        }
    }

    #[test]
    fn a_restore_accepts_a_queued_exception_as_the_injected_one() {
        let activity = |pending_event, pending_interruption| Activity {
            pending_event,
            pending_interruption,
            ..Default::default()
        };

        // An earlier save recorded an exception that KVM had queued, but not
        // injected yet, as pending and injected both, and KVM restores it as
        // the injected exception alone.
        let queued_event = Some(pending_exception(13, Some(0), 0));
        let injected_exception = Some(exception(13, Some(0)));
        let queued = activity(queued_event, injected_exception);
        let injected = activity(None, injected_exception);
        assert!(accepted(true, &injected, &queued));
        assert!(accepted(true, &queued, &queued));
        assert!(accepted(true, &injected, &injected));
        // A hypervisor that does not normalize keeps both.
        assert!(!accepted(false, &injected, &queued));
        assert!(accepted(false, &queued, &queued));

        // The rest of the activity is kept.
        for (mp_state, nmi_pending, nmi_masked, interrupt_shadow) in [
            (MpState::Halted, false, false, false),
            (MpState::Running, true, false, false),
            (MpState::Running, false, true, false),
            (MpState::Running, false, false, true),
        ] {
            let surround = |pending_event, pending_interruption| Activity {
                mp_state,
                nmi_pending,
                nmi_masked,
                interrupt_shadow,
                pending_event,
                pending_interruption,
                ..Default::default()
            };
            assert!(accepted(
                true,
                &surround(None, injected_exception),
                &surround(queued_event, injected_exception)
            ));
            assert!(!accepted(
                true,
                &surround(None, injected_exception),
                &queued
            ));
            assert!(!accepted(
                true,
                &injected,
                &surround(queued_event, injected_exception)
            ));
        }

        // So is the count of pending NMIs, which KVM restores as it is given
        // and reports from Linux 6.4 on: a difference in it is a loss, beside
        // a queued exception or not.
        let counted = |pending_event, nmi_pending_count| Activity {
            nmi_pending: true,
            nmi_pending_count,
            ..activity(pending_event, injected_exception)
        };
        for restored in [None, Some(2)] {
            for expected in [None, Some(2)] {
                assert_eq!(
                    accepted(
                        true,
                        &counted(None, restored),
                        &counted(queued_event, expected)
                    ),
                    restored == expected
                );
                assert_eq!(
                    accepted(false, &counted(None, restored), &counted(None, expected)),
                    restored == expected
                );
            }
        }

        // And so is the kind of the interrupt shadow, which KVM restores as it
        // is given where it has one: a difference in it is a loss as well.
        let shadowed = |pending_event, interrupt_shadow_kind| Activity {
            interrupt_shadow: true,
            interrupt_shadow_kind,
            ..activity(pending_event, injected_exception)
        };
        for restored in [None, Some(InterruptShadowKind::Sti)] {
            for expected in [None, Some(InterruptShadowKind::Sti)] {
                assert_eq!(
                    accepted(
                        true,
                        &shadowed(None, restored),
                        &shadowed(queued_event, expected)
                    ),
                    restored == expected
                );
                assert_eq!(
                    accepted(false, &shadowed(None, restored), &shadowed(None, expected)),
                    restored == expected
                );
            }
        }

        // A pending event is the injected exception only if its vector and
        // error code say so, and it has no payload. Another is an event of its
        // own, which the restore must not lose.
        for pending_event in [
            pending_exception(14, Some(0), 0),
            pending_exception(13, Some(1), 0),
            pending_exception(13, None, 0),
            pending_exception(13, Some(0), 1),
            PendingEvent::ExtInt { vector: 13 },
        ] {
            let pair = activity(Some(pending_event), injected_exception);
            assert!(!accepted(true, &injected, &pair), "{pending_event:?}");
            assert!(accepted(true, &pair, &pair), "{pending_event:?}");
        }
        for pending_interruption in [
            None,
            Some(exception(14, Some(0))),
            Some(exception(13, None)),
            Some(PendingInterruption::Interrupt { vector: 13 }),
            Some(PendingInterruption::Nmi),
        ] {
            let pair = activity(queued_event, pending_interruption);
            assert!(
                !accepted(true, &activity(None, pending_interruption), &pair),
                "{pending_interruption:?}"
            );
        }
    }

    fn msrs(sysenter_cs: u64, sysenter_eip: u64, sysenter_esp: u64) -> VirtualMsrs {
        VirtualMsrs {
            sysenter_cs,
            sysenter_eip,
            sysenter_esp,
            ..virtual_msrs()
        }
    }

    #[test]
    fn a_restore_accepts_the_msrs_that_kvm_keeps_narrow() {
        // The SYSENTER addresses that KVM makes canonical: what is given, and
        // what is kept on a host with 4 levels of paging, and on one with 5.
        let addresses = [
            // Canonical at neither width.
            (0x8000_0000_0000_1000, 0x1000, 0x1000),
            // Canonical at 57 bits, but not at 48.
            (0x0080_0000_0000_1000, 0x1000, 0x0080_0000_0000_1000),
            (
                0x0000_8000_0000_1000,
                0xffff_8000_0000_1000,
                0x0000_8000_0000_1000,
            ),
            (0xff80_0000_0000_1000, 0x1000, 0xff80_0000_0000_1000),
            // Canonical at both.
            (
                0xffff_8000_0000_1000,
                0xffff_8000_0000_1000,
                0xffff_8000_0000_1000,
            ),
            (0x1000, 0x1000, 0x1000),
        ];
        for (given, at_4_levels, at_5_levels) in addresses {
            // An SVM host also keeps the low 32 bits for a guest that is not
            // Intel-compatible.
            for kept in [given, at_4_levels, at_5_levels, given & 0xffff_ffff] {
                assert!(
                    accepted(true, &msrs(0x10, kept, 0x2000), &msrs(0x10, given, 0x2000)),
                    "SYSENTER_EIP {given:#x} as {kept:#x}"
                );
                assert!(
                    accepted(true, &msrs(0x10, 0x1000, kept), &msrs(0x10, 0x1000, given)),
                    "SYSENTER_ESP {given:#x} as {kept:#x}"
                );
            }
            // No other address is.
            for kept in [given ^ 1, given ^ 0x1000, 0] {
                if [given, at_4_levels, at_5_levels, given & 0xffff_ffff].contains(&kept) {
                    continue;
                }
                assert!(
                    !accepted(true, &msrs(0x10, kept, 0x2000), &msrs(0x10, given, 0x2000)),
                    "SYSENTER_EIP {given:#x} as {kept:#x}"
                );
                assert!(
                    !accepted(true, &msrs(0x10, 0x1000, kept), &msrs(0x10, 0x1000, given)),
                    "SYSENTER_ESP {given:#x} as {kept:#x}"
                );
            }
            // A hypervisor that does not normalize keeps an address whole.
            assert!(accepted(
                false,
                &msrs(0x10, given, given),
                &msrs(0x10, given, given)
            ));
            if at_4_levels != given {
                assert!(!accepted(
                    false,
                    &msrs(0x10, at_4_levels, 0x2000),
                    &msrs(0x10, given, 0x2000)
                ));
            }
        }

        // A VMX host keeps the low 32 bits of SYSENTER_CS, and nothing else
        // of it changes.
        assert!(accepted(
            true,
            &msrs(0x10, 0x1000, 0x2000),
            &msrs(0x1_0000_0010, 0x1000, 0x2000)
        ));
        assert!(accepted(
            true,
            &msrs(0x1_0000_0010, 0x1000, 0x2000),
            &msrs(0x1_0000_0010, 0x1000, 0x2000)
        ));
        assert!(!accepted(
            true,
            &msrs(0x11, 0x1000, 0x2000),
            &msrs(0x1_0000_0010, 0x1000, 0x2000)
        ));
        assert!(!accepted(
            true,
            &msrs(0x1_0000_0010, 0x1000, 0x2000),
            &msrs(0x10, 0x1000, 0x2000)
        ));
        assert!(!accepted(
            false,
            &msrs(0x10, 0x1000, 0x2000),
            &msrs(0x1_0000_0010, 0x1000, 0x2000)
        ));

        // The other MSRs are kept whole.
        for other in [
            VirtualMsrs {
                kernel_gs_base: 0,
                ..virtual_msrs()
            },
            VirtualMsrs {
                star: 0,
                ..virtual_msrs()
            },
            VirtualMsrs {
                lstar: 0,
                ..virtual_msrs()
            },
            VirtualMsrs {
                cstar: 0,
                ..virtual_msrs()
            },
            VirtualMsrs {
                sfmask: 0,
                ..virtual_msrs()
            },
        ] {
            assert!(!accepted(true, &other, &virtual_msrs()), "{other:?}");
        }
    }

    #[test]
    fn a_restore_accepts_the_tsc_aux_that_kvm_keeps_narrow() {
        let aux = |value| TscAux { value };
        // KVM keeps the low 32 bits for a guest that is not Intel-compatible.
        assert!(accepted(true, &aux(0x1234), &aux(0x1_0000_1234)));
        assert!(accepted(true, &aux(0x1_0000_1234), &aux(0x1_0000_1234)));
        assert!(!accepted(true, &aux(0x1235), &aux(0x1_0000_1234)));
        assert!(!accepted(true, &aux(0x1_0000_1234), &aux(0x1234)));
        // A hypervisor that does not normalize keeps all of it.
        assert!(accepted(false, &aux(0x1234), &aux(0x1234)));
        assert!(!accepted(false, &aux(0x1234), &aux(0x1_0000_1234)));
    }

    /// The activity of a build that predates `Activity::interrupt_shadow_kind`.
    #[derive(Debug, PartialEq, Protobuf)]
    struct ActivityBeforeShadowKind {
        #[mesh(1)]
        mp_state: MpState,
        #[mesh(2)]
        nmi_pending: bool,
        #[mesh(3)]
        nmi_masked: bool,
        #[mesh(4)]
        interrupt_shadow: bool,
        #[mesh(5)]
        pending_event: Option<PendingEvent>,
        #[mesh(6)]
        pending_interruption: Option<PendingInterruption>,
        #[mesh(7)]
        nmi_pending_count: Option<u8>,
    }

    /// The number of the field of `Activity::interrupt_shadow_kind`, which its
    /// `mesh` attribute must carry.
    const INTERRUPT_SHADOW_KIND_FIELD: u8 = 8;

    #[test]
    fn interrupt_shadow_kind_needs_no_version_to_coexist_with_earlier_builds() {
        let earlier = |interrupt_shadow| ActivityBeforeShadowKind {
            mp_state: MpState::Halted,
            nmi_pending: true,
            nmi_masked: false,
            interrupt_shadow,
            pending_event: None,
            pending_interruption: Some(PendingInterruption::Interrupt { vector: 0x30 }),
            nmi_pending_count: Some(2),
        };
        let current = |interrupt_shadow, interrupt_shadow_kind| Activity {
            mp_state: MpState::Halted,
            nmi_pending: true,
            interrupt_shadow,
            pending_interruption: Some(PendingInterruption::Interrupt { vector: 0x30 }),
            nmi_pending_count: Some(2),
            interrupt_shadow_kind,
            ..Default::default()
        };
        let sti = Some(InterruptShadowKind::Sti);

        for interrupt_shadow in [false, true] {
            // A saved state of an earlier build holds no kind, so restoring it
            // sets the shadow that `None` gives, as that build did.
            assert_eq!(
                mesh_protobuf::decode::<Activity>(&mesh_protobuf::encode(earlier(
                    interrupt_shadow
                )))
                .unwrap(),
                current(interrupt_shadow, None)
            );
            // A state whose shadow is not an STI shadow, or that has none, is
            // the one that the earlier build saved, byte for byte.
            assert_eq!(
                mesh_protobuf::encode(current(interrupt_shadow, None)),
                mesh_protobuf::encode(earlier(interrupt_shadow))
            );
        }
        // An earlier build reads the state of an STI shadow and ignores the
        // kind, so it restores a MOV SS shadow.
        assert_eq!(
            mesh_protobuf::decode::<ActivityBeforeShadowKind>(&mesh_protobuf::encode(current(
                true, sti
            )))
            .unwrap(),
            earlier(true)
        );

        // The kind is a message under the field's number, after the earlier
        // fields: bytes that every build that holds it reads, and that no
        // later change may alter.
        let kind = [(INTERRUPT_SHADOW_KIND_FIELD << 3) | 2, 2, 0x0a, 0x00];
        assert_eq!(
            mesh_protobuf::encode(current(true, sti)),
            [mesh_protobuf::encode(earlier(true)), kind.to_vec()].concat()
        );
        let pinned = [
            0x0a, 0x02, 0x0a, 0x00, 0x20, 0x01, kind[0], kind[1], kind[2], kind[3],
        ];
        let activity = Activity {
            interrupt_shadow: true,
            interrupt_shadow_kind: sti,
            ..Default::default()
        };
        assert_eq!(mesh_protobuf::encode(activity), pinned);
        assert_eq!(
            mesh_protobuf::decode::<Activity>(&pinned).unwrap(),
            Activity {
                interrupt_shadow: true,
                interrupt_shadow_kind: sti,
                ..Default::default()
            }
        );
    }

    #[test]
    fn activity_registers_hold_no_interrupt_shadow_kind() {
        // The interrupt state register has one bit for both kinds, so the
        // backends that hold the activity in registers (MSHV and WHP) neither
        // write nor read a kind.
        fn values(kind: Option<InterruptShadowKind>) -> [HvRegisterValue; 4] {
            let mut values = [HvRegisterValue::from(0u64); 4];
            Activity {
                interrupt_shadow: true,
                interrupt_shadow_kind: kind,
                ..Default::default()
            }
            .get_values(values.iter_mut());
            values
        }
        assert_eq!(values(Some(InterruptShadowKind::Sti)), values(None));

        let mut read = Activity::default();
        read.set_values(values(Some(InterruptShadowKind::Sti)).into_iter());
        assert!(read.interrupt_shadow);
        assert_eq!(read.interrupt_shadow_kind, None);
    }
}
