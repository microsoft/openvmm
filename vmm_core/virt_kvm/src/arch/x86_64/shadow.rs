// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The kind of a VP's interrupt shadow, as KVM reports and sets it.
//!
//! An `STI` that sets RFLAGS.IF, or a `MOV SS` or `POP SS`, leaves a VP in an
//! interrupt shadow, which blocks maskable interrupts until the next
//! instruction ends. The kinds differ at that instruction boundary: a MOV SS
//! shadow also blocks NMIs and suppresses certain debug exceptions, and an STI
//! shadow does not. A restore that turned one kind into the other would give
//! the guest a different instruction boundary than the saved one.
//!
//! `KVM_GET_VCPU_EVENTS` reports the shadow in `interrupt.shadow`, and
//! `KVM_SET_VCPU_EVENTS` sets it. VMX has a shadow of each kind, so KVM reports
//! the bit of the kind that the VMCS holds, and sets the MOV SS shadow if the
//! value has its bit, and otherwise the STI shadow. SVM has one kind of shadow,
//! so KVM reports both bits, and sets the shadow for any value but 0.
//!
//! The saved activity names a kind only for an STI shadow, in an optional field
//! that earlier saved states lack. A restore sets a shadow that has no kind as
//! a MOV SS shadow, as every restore did before the field, so a MOV SS shadow,
//! and SVM's one shadow, have no kind and read back from a restore as saved.

use virt::x86::vp::InterruptShadowKind;

/// The bits of KVM's `interrupt.shadow` for each kind of interrupt shadow.
const MOV_SS: u8 = kvm::KVM_X86_SHADOW_INT_MOV_SS as u8;
const STI: u8 = kvm::KVM_X86_SHADOW_INT_STI as u8;

/// Returns whether KVM's `interrupt.shadow` holds an interrupt shadow, and its
/// kind if it is not the one that restoring no kind sets.
///
/// Only an STI shadow has a kind. A shadow of both kinds is SVM's one shadow,
/// which KVM reports that way, and it restores as a MOV SS shadow would.
pub(super) fn from_kvm(shadow: u8) -> (bool, Option<InterruptShadowKind>) {
    let kind = (shadow == STI).then_some(InterruptShadowKind::Sti);
    (shadow != 0, kind)
}

/// Returns the `interrupt.shadow` that `KVM_SET_VCPU_EVENTS` takes for an
/// activity's interrupt shadow and its kind.
///
/// A shadow that has no kind restores as a MOV SS shadow, as every shadow did
/// before the saved state had a kind. The other way would be unsafe: a MOV SS
/// shadow that restored as an STI shadow would let an NMI or a debug exception
/// split the `MOV SS` from the instruction that follows it, which sets the
/// stack pointer, while an STI shadow that restores as a MOV SS shadow only
/// delays them by an instruction.
///
/// SVM sets its one shadow for any value but 0, and KVM reports it as both
/// kinds, so a restore of the STI kind there reads back with no kind. An SVM
/// capture never names a kind, and a snapshot restore refuses the snapshot of
/// another vendor's CPU generation, so only a crafted activity gets there, and
/// it is accepted.
pub(super) fn to_kvm(shadow: bool, kind: Option<InterruptShadowKind>) -> u8 {
    match (shadow, kind) {
        (false, _) => 0,
        (true, Some(InterruptShadowKind::Sti)) => STI,
        (true, None) => MOV_SS,
    }
}

#[cfg(test)]
mod tests {
    use super::InterruptShadowKind;
    use super::MOV_SS;
    use super::STI;
    use super::from_kvm;
    use super::to_kvm;
    use test_with_tracing::test;

    #[test]
    fn kvm_reports_a_kind_only_for_an_sti_shadow() {
        assert_eq!(from_kvm(0), (false, None));
        assert_eq!(from_kvm(MOV_SS), (true, None));
        assert_eq!(from_kvm(STI), (true, Some(InterruptShadowKind::Sti)));
        // SVM has one kind of shadow, which KVM reports as both bits.
        assert_eq!(from_kvm(MOV_SS | STI), (true, None));
    }

    #[test]
    fn only_an_sti_shadow_adds_a_kind_to_what_an_earlier_build_saved() {
        // An earlier build saved whether KVM held a shadow, and no kind.
        for reported in 0..=u8::MAX {
            let (shadow, kind) = from_kvm(reported);
            assert_eq!(shadow, reported != 0, "reported {reported}");
            assert_eq!(kind.is_some(), reported == STI, "reported {reported}");
        }
    }

    #[test]
    fn a_shadow_restores_as_its_kind() {
        assert_eq!(to_kvm(false, None), 0);
        assert_eq!(to_kvm(true, Some(InterruptShadowKind::Sti)), STI);
        // A saved state that has no kind restores a shadow as it always did.
        assert_eq!(to_kvm(true, None), MOV_SS);
    }

    #[test]
    fn a_saved_shadow_reads_back_as_saved() {
        // What `KVM_GET_VCPU_EVENTS` reports once `KVM_SET_VCPU_EVENTS` has set
        // `shadow`: VMX reports the kind that it holds, and SVM reports its one
        // shadow as both kinds.
        fn vmx_reports(shadow: u8) -> u8 {
            shadow
        }
        fn svm_reports(shadow: u8) -> u8 {
            if shadow == 0 { 0 } else { STI | MOV_SS }
        }
        fn read_back(
            reports: fn(u8) -> u8,
            shadow: bool,
            kind: Option<InterruptShadowKind>,
        ) -> (bool, Option<InterruptShadowKind>) {
            from_kvm(reports(to_kvm(shadow, kind)))
        }

        // A state that an earlier build saved, with a shadow or without one,
        // reads back as saved on either, so that a restore needs no allowance
        // for it.
        for (shadow, kind) in [(false, None), (true, None)] {
            assert_eq!(read_back(vmx_reports, shadow, kind), (shadow, kind));
            assert_eq!(read_back(svm_reports, shadow, kind), (shadow, kind));
        }

        // So does an STI shadow on VMX. SVM has no shadow of that kind: it sets
        // its one shadow, which reads back as such.
        let sti = Some(InterruptShadowKind::Sti);
        assert_eq!(read_back(vmx_reports, true, sti), (true, sti));
        assert_eq!(read_back(svm_reports, true, sti), (true, None));
    }
}

/// Tests against the host's KVM, with a real-mode guest that stops with an
/// interrupt shadow: the VP stops at an I/O write, which KVM completes when the
/// VP next runs, so the shadow that the instruction before it left is still in
/// place.
#[cfg(test)]
mod kvm_tests {
    use super::InterruptShadowKind;
    use super::MOV_SS;
    use super::STI;
    use super::from_kvm;
    use super::to_kvm;
    use crate::arch::x86_64::test_guest::GuestRam;
    use kvm::Exit;
    use kvm::KVM_MP_STATE_RUNNABLE;
    use kvm::KVM_VCPUEVENT_VALID_SHADOW;
    use kvm::Kvm;
    use kvm::Processor;
    use kvm::VmType;
    use kvm::kvm_regs;
    use std::sync::mpsc;
    use std::time::Duration;
    use test_with_tracing::test;
    use x86defs::cpuid::Vendor;

    const CODE_GPA: u64 = 0x1000;
    const DONE_PORT: u16 = 0x80;

    /// Returns whether the host's CPU is AMD's.
    fn host_is_amd(kvm: &Kvm) -> bool {
        let leaf = kvm
            .supported_cpuid()
            .unwrap()
            .into_iter()
            .find(|entry| entry.function == 0)
            .unwrap();
        Vendor::from_ebx_ecx_edx(leaf.ebx, leaf.ecx, leaf.edx).is_amd_compatible()
    }

    /// Runs `code` in a real-mode guest, which ends by writing `DONE_PORT`, and
    /// calls `f` with the VP, stopped at the write, and whether the host's CPU
    /// is AMD's.
    fn with_stopped_vp(code: &[u8], f: impl FnOnce(&Processor<'_>, bool)) {
        kvm::init();
        let kvm = Kvm::new().unwrap();
        let amd = host_is_amd(&kvm);
        let mut partition = kvm.new_vm(VmType::Default).unwrap();
        partition.enable_split_irqchip(24).unwrap();
        partition.add_vp(0).unwrap();

        let ram = GuestRam::new(&partition, 0x10000);
        ram.write(CODE_GPA, code);
        let vp = partition.vp(0);
        let mut sregs = vp.get_sregs().unwrap();
        sregs.cs.selector = 0;
        sregs.cs.base = 0;
        vp.set_sregs(&sregs).unwrap();
        vp.set_regs(&kvm_regs {
            rip: CODE_GPA,
            rsp: 0x8000,
            // IF is clear, so that an STI leaves a shadow.
            rflags: 2,
            ..Default::default()
        })
        .unwrap();
        vp.set_mp_state(KVM_MP_STATE_RUNNABLE).unwrap();

        let partition = &partition;
        std::thread::scope(|s| {
            let (done, wait_done) = mpsc::channel::<()>();
            s.spawn(move || {
                // Kick the VP out of KVM_RUN if the guest gets stuck.
                if let Err(mpsc::RecvTimeoutError::Timeout) =
                    wait_done.recv_timeout(Duration::from_secs(10))
                {
                    partition.vp(0).force_exit();
                }
            });
            let mut runner = vp.runner();
            match runner.run().unwrap() {
                Exit::IoOut { port, .. } => assert_eq!(port, DONE_PORT),
                exit => panic!("unexpected exit: {exit:?}"),
            }
            f(&vp, amd);
            drop(runner);
            drop(done);
        });
    }

    fn shadow(vp: &Processor<'_>) -> u8 {
        vp.get_vcpu_events().unwrap().interrupt.shadow
    }

    fn set_shadow(vp: &Processor<'_>, shadow: u8) {
        let mut events = vp.get_vcpu_events().unwrap();
        events.interrupt.shadow = shadow;
        events.flags = KVM_VCPUEVENT_VALID_SHADOW;
        vp.set_vcpu_events(&events).unwrap();
    }

    /// Checks that a VP that `code` stops in a shadow that VMX reports as
    /// `vmx_shadow`, and SVM as both kinds, restores the shadow that it
    /// reports, and that the kind that the save names, which only an STI shadow
    /// has and only on VMX, is the one that `vmx_kind` gives.
    fn check_restore(code: &[u8], vmx_shadow: u8, vmx_kind: Option<InterruptShadowKind>) {
        with_stopped_vp(code, |vp, amd| {
            let reported = shadow(vp);
            assert_eq!(reported, if amd { STI | MOV_SS } else { vmx_shadow });
            let (held, found) = from_kvm(reported);
            assert!(held);
            assert_eq!(found, if amd { None } else { vmx_kind });

            // A destination that holds no shadow.
            set_shadow(vp, 0);
            assert_eq!(shadow(vp), 0);

            set_shadow(vp, to_kvm(held, found));
            assert_eq!(shadow(vp), reported);
            assert_eq!(from_kvm(shadow(vp)), (held, found));
        });
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn a_vp_stopped_after_an_sti_restores_an_sti_shadow() {
        // sti; out DONE_PORT, al
        check_restore(
            &[0xfb, 0xe6, DONE_PORT as u8],
            STI,
            Some(InterruptShadowKind::Sti),
        );
    }

    // This is the shadow that a save without a kind restores, as an earlier
    // build's saves do, and it reads back without one.
    #[test]
    #[ignore = "requires /dev/kvm"]
    fn a_vp_stopped_after_a_mov_ss_restores_a_mov_ss_shadow() {
        // mov ax, 0; mov ss, ax; out DONE_PORT, al
        check_restore(
            &[0xb8, 0x00, 0x00, 0x8e, 0xd0, 0xe6, DONE_PORT as u8],
            MOV_SS,
            None,
        );
    }
}
