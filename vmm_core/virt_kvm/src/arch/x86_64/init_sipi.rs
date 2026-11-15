// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Detecting the INIT or SIPI that a stopped VP has not accepted yet.
//!
//! When another VP sends an INIT or a SIPI to a VP whose thread is outside
//! `KVM_RUN`, KVM holds the event until the VP next runs. It also accepts the
//! event when `KVM_GET_MP_STATE` reads the VP's MP state, which the VP's
//! activity does. A save reads a VP's registers before its activity, so such an
//! event is accepted in between: the saved registers are from before the event,
//! and the saved activity and the rest of the state are from after it.
//!
//! KVM also holds an INIT that the VP cannot accept yet, because SMM or a clear
//! GIF blocks it, until the guest leaves that state. The saved state cannot
//! represent a latched INIT, so a restore would lose it. The save's own checks
//! of the GIF (see `gif`) and of SMM (see `smm`) refuse a VP in either state
//! whether or not an INIT is latched.

use crate::KvmError;

/// Returns whether the stopped VP `vp` holds an INIT or a SIPI that it has not
/// accepted yet.
///
/// KVM reports a latched INIT without accepting it, whether the VP has not run
/// since the INIT arrived or SMM or a clear GIF blocks the INIT. It reports a
/// pending SIPI only by accepting it, which a read of the MP state does just as
/// `KVM_RUN` would, so this reports one when the read moves the VP's registers:
/// a SIPI starts a VP that waits for one at the SIPI vector, which moves its CS
/// and RIP, and a SIPI to any other VP is dropped. A VP that holds such a SIPI
/// before this call starts at the vector afterwards, as it would have when it
/// ran.
pub(super) fn has_pending_init_sipi(vp: &kvm::Processor<'_>) -> Result<bool, KvmError> {
    if vp.get_vcpu_events()?.smi.latched_init != 0 {
        return Ok(true);
    }
    let before = (vp.get_regs()?, vp.get_sregs()?);
    vp.get_mp_state()?;
    Ok((vp.get_regs()?, vp.get_sregs()?) != before)
}

/// Tests against the host's KVM, with a guest that sends IPIs to a VP whose
/// thread is outside `KVM_RUN`, as when the VMM stops the VPs of a VM.
#[cfg(test)]
mod kvm_tests {
    use super::has_pending_init_sipi;
    use crate::arch::x86_64::test_guest::GuestRam;
    use kvm::Exit;
    use kvm::Kvm;
    use kvm::Partition;
    use kvm::Processor;
    use kvm::VmType;
    use kvm::kvm_cpuid_entry2;
    use kvm::kvm_regs;
    use kvm::kvm_sregs;
    use kvm::kvm_vcpu_events;
    use test_with_tracing::test;

    const CODE_GPA: u64 = 0x1000;
    const DONE_PORT: u8 = 0x80;
    /// The vector of the SIPI: the VP that it starts runs at `SIPI_VECTOR << 12`.
    const SIPI_VECTOR: u8 = 0x08;
    /// Enabled, in x2APIC mode, and the BSP flag that KVM keeps a VP running
    /// after an INIT by.
    const APIC_BASE: u64 = 0xfee0_0c00;
    const APIC_BASE_BSP: u64 = 1 << 8;
    /// The x2APIC's ICR MSR, and the commands for it: an INIT with its level
    /// asserted, and a SIPI, which the vector completes.
    const ICR_MSR: u32 = 0x830;
    const INIT: u32 = 0x4500;
    const SIPI: u32 = 0x4600;
    const RESET_CS_BASE: u64 = 0xffff_0000;

    /// A VM of two VPs, VP 0 the BSP. The guest of either VP sends IPIs to the
    /// other, and the thread of neither is in `KVM_RUN` otherwise, so an IPI
    /// reaches a VP as it would one that the VMM has stopped.
    struct Vm<'a> {
        partition: &'a Partition,
        ram: &'a GuestRam<'a>,
    }

    fn with_vm(f: impl FnOnce(&Vm<'_>)) {
        kvm::init();
        let mut partition = Kvm::new().unwrap().new_vm(VmType::Default).unwrap();
        partition.enable_split_irqchip(24).unwrap();
        partition.enable_x2apic_api().unwrap();
        partition.add_vp(0).unwrap();
        partition.add_vp(1).unwrap();
        let ram = GuestRam::new(&partition, 0x10000);
        for index in 0..2 {
            let vp = partition.vp(index);
            // Enumerate the x2APIC and enable it.
            vp.set_cpuid(&[kvm_cpuid_entry2 {
                function: 1,
                ecx: 1 << 21,
                ..Default::default()
            }])
            .unwrap();
            let bsp = if index == 0 { APIC_BASE_BSP } else { 0 };
            vp.set_msrs(&[(x86defs::X86X_MSR_APIC_BASE, APIC_BASE | bsp)])
                .unwrap();
        }
        f(&Vm {
            partition: &partition,
            ram: &ram,
        });
    }

    impl Vm<'_> {
        fn vp(&self, index: u32) -> Processor<'_> {
            self.partition.vp(index)
        }

        /// Makes the VP `index` one that has been running real-mode guest
        /// code, which an INIT resets.
        fn run_guest_code(&self, index: u32) {
            let vp = self.vp(index);
            let mut sregs = vp.get_sregs().unwrap();
            sregs.cs.selector = 0;
            sregs.cs.base = 0;
            vp.set_sregs(&sregs).unwrap();
            vp.set_regs(&kvm_regs {
                rip: CODE_GPA,
                rsp: 0x8000,
                rflags: 2,
                ..Default::default()
            })
            .unwrap();
            vp.set_mp_state(kvm::KVM_MP_STATE_RUNNABLE).unwrap();
        }

        /// Has the guest on the VP `from` send the IPI `icr` to the VP `to`:
        /// the guest writes the x2APIC's ICR, which KVM delivers at once, and
        /// then writes `DONE_PORT`. The VP `from` stops there, outside
        /// `KVM_RUN`.
        fn send_ipi(&self, from: u32, to: u32, icr: u32) {
            self.run_guest_code(from);
            let mut code = vec![0x66, 0xb9]; // mov ecx, ICR_MSR
            code.extend(ICR_MSR.to_le_bytes());
            code.extend([0x66, 0xb8]); // mov eax, icr
            code.extend(icr.to_le_bytes());
            code.extend([0x66, 0xba]); // mov edx, to
            code.extend(to.to_le_bytes());
            code.extend([0x0f, 0x30]); // wrmsr
            code.extend([0xe6, DONE_PORT]); // out DONE_PORT, al
            self.ram.write(CODE_GPA, &code);

            let mut runner = self.vp(from).runner();
            match runner.run().unwrap() {
                Exit::IoOut { port, .. } => assert_eq!(port, u16::from(DONE_PORT)),
                exit => panic!("unexpected exit: {exit:?}"),
            }
            assert!(matches!(runner.complete_exit().unwrap(), Exit::Interrupted));
        }
    }

    /// What KVM holds for `vp`, which reading does not change: its registers,
    /// special registers, and events, which include a latched INIT.
    fn held(vp: &Processor<'_>) -> (kvm_regs, kvm_sregs, kvm_vcpu_events) {
        (
            vp.get_regs().unwrap(),
            vp.get_sregs().unwrap(),
            vp.get_vcpu_events().unwrap(),
        )
    }

    fn init_latched(vp: &Processor<'_>) -> bool {
        vp.get_vcpu_events().unwrap().smi.latched_init != 0
    }

    /// Returns where the VP runs: its CS selector and base, and its RIP.
    fn code_address(vp: &Processor<'_>) -> (u16, u64, u64) {
        let sregs = vp.get_sregs().unwrap();
        (sregs.cs.selector, sregs.cs.base, vp.get_regs().unwrap().rip)
    }

    /// The reset state of a VP that took an INIT, and the state that a SIPI
    /// moves it to.
    const AFTER_INIT: (u16, u64, u64) = (0xf000, RESET_CS_BASE, 0xfff0);
    const AFTER_SIPI: (u16, u64, u64) = ((SIPI_VECTOR as u16) << 8, (SIPI_VECTOR as u64) << 12, 0);

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn a_pending_init_is_reported_and_left_pending() {
        with_vm(|vm| {
            vm.run_guest_code(1);
            vm.send_ipi(0, 1, INIT);
            let target = vm.vp(1);
            assert!(init_latched(&target));
            let before = held(&target);

            assert!(has_pending_init_sipi(&target).unwrap());
            assert_eq!(held(&target), before, "nothing was accepted");
            assert!(init_latched(&target));

            // The VP still takes the INIT when it next runs, or here when KVM
            // reads its MP state.
            assert_eq!(
                target.get_mp_state().unwrap(),
                kvm::KVM_MP_STATE_INIT_RECEIVED
            );
            assert_eq!(code_address(&target), AFTER_INIT);
            assert!(!init_latched(&target));
        });
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn a_pending_sipi_is_reported_when_the_vp_waits_for_it() {
        with_vm(|vm| {
            vm.send_ipi(0, 1, INIT);
            // The VP takes the INIT, and then waits for a SIPI.
            let target = vm.vp(1);
            assert_eq!(
                target.get_mp_state().unwrap(),
                kvm::KVM_MP_STATE_INIT_RECEIVED
            );
            assert!(!has_pending_init_sipi(&target).unwrap());

            vm.send_ipi(0, 1, SIPI | u32::from(SIPI_VECTOR));
            assert!(!init_latched(&target));
            assert!(has_pending_init_sipi(&target).unwrap());
            // KVM reports a SIPI by accepting it, as it does before the VP
            // runs: the VP starts at the SIPI vector.
            assert_eq!(target.get_mp_state().unwrap(), kvm::KVM_MP_STATE_RUNNABLE);
            assert_eq!(code_address(&target), AFTER_SIPI);
            assert!(!has_pending_init_sipi(&target).unwrap());
        });
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn a_pending_init_and_sipi_are_reported_without_accepting_either() {
        with_vm(|vm| {
            vm.run_guest_code(1);
            vm.send_ipi(0, 1, INIT);
            vm.send_ipi(0, 1, SIPI | u32::from(SIPI_VECTOR));
            let target = vm.vp(1);
            let before = held(&target);

            assert!(has_pending_init_sipi(&target).unwrap());
            assert_eq!(held(&target), before, "nothing was accepted");

            // The VP takes both when it next runs: the INIT resets it, and the
            // SIPI starts it.
            assert_eq!(target.get_mp_state().unwrap(), kvm::KVM_MP_STATE_RUNNABLE);
            assert_eq!(code_address(&target), AFTER_SIPI);
        });
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn a_vp_without_a_pending_event_is_not_reported() {
        with_vm(|vm| {
            let target = vm.vp(1);
            // A VP that no INIT has reached waits for one.
            let uninitialized = held(&target);
            assert!(!has_pending_init_sipi(&target).unwrap());
            assert_eq!(held(&target), uninitialized);
            assert_eq!(
                target.get_mp_state().unwrap(),
                kvm::KVM_MP_STATE_UNINITIALIZED
            );

            vm.run_guest_code(1);
            let running = held(&target);
            assert!(!has_pending_init_sipi(&target).unwrap());
            assert_eq!(held(&target), running);

            target.set_mp_state(kvm::KVM_MP_STATE_HALTED).unwrap();
            let halted = held(&target);
            assert!(!has_pending_init_sipi(&target).unwrap());
            assert_eq!(held(&target), halted);
            assert_eq!(target.get_mp_state().unwrap(), kvm::KVM_MP_STATE_HALTED);

            // A VP that took an INIT waits for its SIPI.
            vm.send_ipi(0, 1, INIT);
            assert_eq!(
                target.get_mp_state().unwrap(),
                kvm::KVM_MP_STATE_INIT_RECEIVED
            );
            let waiting = held(&target);
            assert!(!has_pending_init_sipi(&target).unwrap());
            assert_eq!(held(&target), waiting);
        });
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn a_sipi_to_a_vp_that_does_not_wait_for_it_is_not_reported() {
        with_vm(|vm| {
            // KVM drops a SIPI for a VP that no INIT has reset, as the CPU
            // does, so a save loses nothing.
            vm.run_guest_code(1);
            let target = vm.vp(1);
            vm.send_ipi(0, 1, SIPI | u32::from(SIPI_VECTOR));
            let before = held(&target);
            assert!(!has_pending_init_sipi(&target).unwrap());
            assert_eq!(held(&target), before);

            let uninitialized = vm.vp(1);
            uninitialized
                .set_mp_state(kvm::KVM_MP_STATE_UNINITIALIZED)
                .unwrap();
            vm.send_ipi(0, 1, SIPI | u32::from(SIPI_VECTOR));
            let before = held(&uninitialized);
            assert!(!has_pending_init_sipi(&uninitialized).unwrap());
            assert_eq!(held(&uninitialized), before);
        });
    }
}
