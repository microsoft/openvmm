// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Injection of PIC (ExtINT) interrupts through a VP's injected-interrupt
//! slot.

use crate::KvmRunVpError;

/// Injects an extint interrupt through the VP's injected-interrupt slot.
///
/// Unlike an interrupt queued with [`kvm::VpRunner::queue_extint_interrupt`],
/// KVM reports this interrupt through `KVM_GET_VCPU_EVENTS` until the guest
/// takes it, so it is saved and restored along with the rest of the VP state.
/// One difference in timing: if KVM has halted the VP while the host pages in
/// guest memory, the guest takes the interrupt only once the page is present,
/// as if the memory access were just slow.
///
/// KVM delivers an interrupt in this slot without checking whether the guest
/// can take it, so the caller must ensure that KVM has reported an open
/// interrupt window for the current VP state. KVM also cannot turn this
/// interrupt into an exit from a nested guest, so queue the interrupt instead
/// if the VP may be running one.
pub(super) fn inject(vp: &kvm::Processor<'_>, vector: u8) -> Result<(), KvmRunVpError> {
    let mut events = vp
        .get_vcpu_events()
        .map_err(KvmRunVpError::ExtintInterrupt)?;
    // KVM only reports an open interrupt window when no exception is pending
    // and no event is being injected, so this should never fail. Pending NMIs
    // and SMIs are delivered right after this interrupt, as if they had
    // arrived just after it.
    if events.exception.injected != 0
        || events.exception.pending != 0
        || events.nmi.injected != 0
        || events.interrupt.injected != 0
        || events.interrupt.shadow != 0
    {
        return Err(KvmRunVpError::ExtintNotInjectable(vector));
    }
    events.interrupt.injected = 1;
    events.interrupt.nr = vector;
    events.interrupt.soft = 0;
    // Leave the optional state alone, since KVM may have queued NMIs, SMIs, or
    // INITs since it was read.
    events.flags = 0;
    vp.set_vcpu_events(&events)
        .map_err(KvmRunVpError::ExtintInterrupt)?;

    // KVM does not wake a halted VP for an interrupt in this slot, so leave the
    // halted state just as taking the interrupt would. Do this after setting
    // the interrupt: reading the state makes KVM process a latched INIT, which
    // resets the VP and discards the interrupt, as INIT would.
    if vp.get_mp_state().map_err(KvmRunVpError::ExtintInterrupt)? == kvm::KVM_MP_STATE_HALTED {
        vp.set_mp_state(kvm::KVM_MP_STATE_RUNNABLE)
            .map_err(KvmRunVpError::ExtintInterrupt)?;
    }
    Ok(())
}

/// Tests for extint delivery, which run a real-mode guest in a KVM VM.
#[cfg(test)]
mod tests {
    use super::inject;
    use kvm::Exit;
    use kvm::KVM_MP_STATE_HALTED;
    use kvm::KVM_MP_STATE_RUNNABLE;
    use kvm::Kvm;
    use kvm::Processor;
    use kvm::VmType;
    use kvm::VpRunner;
    use kvm::kvm_regs;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::time::Duration;
    use test_with_tracing::test;

    const VECTOR: u8 = 0x20;
    const CODE_GPA: u64 = 0x1000;
    const HANDLER_GPA: u64 = 0x2000;
    const RESUMED_PORT: u8 = 0x80;
    const HANDLER_PORT: u8 = 0x81;
    const RFLAGS_IF: u64 = 1 << 9;

    struct GuestRam {
        ptr: *mut u8,
        len: usize,
    }

    impl GuestRam {
        fn new(len: usize) -> Self {
            // SAFETY: creating a new anonymous mapping.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(ptr, libc::MAP_FAILED);
            Self {
                ptr: ptr.cast(),
                len,
            }
        }

        fn write(&self, gpa: u64, data: &[u8]) {
            let offset = gpa as usize;
            assert!(offset + data.len() <= self.len);
            // SAFETY: the range is within the mapping, and the guest is not
            // running yet.
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.add(offset), data.len())
            };
        }
    }

    impl Drop for GuestRam {
        fn drop(&mut self) {
            // SAFETY: unmapping the mapping created in `new`, which KVM no
            // longer references.
            unsafe { libc::munmap(self.ptr.cast(), self.len) };
        }
    }

    /// Runs the VP until it writes to an I/O port, and returns the port.
    fn next_out_port(runner: &mut VpRunner<'_>, timed_out: &AtomicBool) -> u16 {
        loop {
            match runner.run().unwrap() {
                Exit::IoOut { port, .. } => break port,
                Exit::Interrupted | Exit::InterruptWindow => {
                    assert!(
                        !timed_out.load(Ordering::SeqCst),
                        "timed out waiting for the guest"
                    );
                }
                exit => panic!("unexpected exit: {exit:?}"),
            }
        }
    }

    /// Makes KVM report the interrupt window for the current VP state, as
    /// `run_vp` does, by completing any pending exit without running the VP.
    fn refresh_interrupt_window(runner: &mut VpRunner<'_>) {
        assert!(matches!(runner.complete_exit().unwrap(), Exit::Interrupted));
    }

    /// Stops a real-mode VP just after a `hlt` with interrupts enabled
    /// (halting it if `halted` is set), calls `deliver` to deliver an extint,
    /// and then checks that the guest takes the interrupt exactly once before
    /// resuming after the `hlt`.
    fn check_extint(halted: bool, deliver: impl FnOnce(&mut VpRunner<'_>, &Processor<'_>)) {
        kvm::init();

        let ram = GuestRam::new(0x10000);
        // The real-mode IVT entry for the vector.
        ram.write(
            u64::from(VECTOR) * 4,
            &[HANDLER_GPA as u8, (HANDLER_GPA >> 8) as u8, 0, 0],
        );
        // hlt; out RESUMED_PORT, al; jmp $
        ram.write(CODE_GPA, &[0xf4, 0xe6, RESUMED_PORT, 0xeb, 0xfe]);
        // out HANDLER_PORT, al; iret
        ram.write(HANDLER_GPA, &[0xe6, HANDLER_PORT, 0xcf]);

        let kvm = Kvm::new().unwrap();
        let mut partition = kvm.new_vm(VmType::Default).unwrap();
        partition.enable_split_irqchip(24).unwrap();
        partition.add_vp(0).unwrap();
        // SAFETY: `ram` outlives `partition`.
        unsafe {
            partition
                .set_user_memory_region(0, ram.ptr, ram.len, 0, false)
                .unwrap();
        }

        let vp = partition.vp(0);
        let mut sregs = vp.get_sregs().unwrap();
        sregs.cs.selector = 0;
        sregs.cs.base = 0;
        vp.set_sregs(&sregs).unwrap();
        vp.set_regs(&kvm_regs {
            rip: CODE_GPA + 1,
            rsp: 0x8000,
            rflags: RFLAGS_IF | 2,
            ..Default::default()
        })
        .unwrap();

        // Route PIC interrupts through LINT0, as in virtual wire mode.
        let mut lapic = [0; 1024];
        vp.get_lapic(&mut lapic).unwrap();
        lapic[0xf0..0xf4].copy_from_slice(&0x1ffu32.to_le_bytes());
        lapic[0x350..0x354].copy_from_slice(&0x700u32.to_le_bytes());
        vp.set_lapic(&lapic).unwrap();

        if halted {
            vp.set_mp_state(KVM_MP_STATE_HALTED).unwrap();
        }

        let timed_out = &AtomicBool::new(false);
        let partition = &partition;
        std::thread::scope(|s| {
            let (done, wait_done) = mpsc::channel::<()>();
            s.spawn(move || {
                // Kick the VP out of KVM_RUN if the guest gets stuck.
                if let Err(mpsc::RecvTimeoutError::Timeout) =
                    wait_done.recv_timeout(Duration::from_secs(10))
                {
                    timed_out.store(true, Ordering::SeqCst);
                    partition.vp(0).force_exit();
                }
            });

            let mut runner = vp.runner();
            deliver(&mut runner, &vp);
            assert_eq!(next_out_port(&mut runner, timed_out), HANDLER_PORT.into());
            assert_eq!(vp.get_vcpu_events().unwrap().interrupt.injected, 0);
            assert_eq!(next_out_port(&mut runner, timed_out), RESUMED_PORT.into());
            drop(runner);
            drop(done);
        });
    }

    #[test]
    #[ignore = "requires access to /dev/kvm"]
    fn injected_extint_wakes_halted_vp_and_is_saved() {
        check_extint(true, |runner, vp| {
            refresh_interrupt_window(runner);
            assert!(runner.check_or_request_interrupt_window());
            inject(vp, VECTOR).unwrap();

            // The interrupt is part of the saved state until the guest takes
            // it, and the VP is no longer halted.
            let events = vp.get_vcpu_events().unwrap();
            assert_eq!(events.interrupt.injected, 1);
            assert_eq!(events.interrupt.nr, VECTOR);
            assert_eq!(vp.get_mp_state().unwrap(), KVM_MP_STATE_RUNNABLE);
        });
    }

    #[test]
    #[ignore = "requires access to /dev/kvm"]
    fn queued_extint_wakes_halted_vp() {
        check_extint(true, |runner, vp| {
            refresh_interrupt_window(runner);
            assert!(runner.check_or_request_interrupt_window());
            runner.queue_extint_interrupt(VECTOR).unwrap();

            // KVM does not report a queued extint.
            assert_eq!(vp.get_vcpu_events().unwrap().interrupt.injected, 0);
        });
    }

    #[test]
    #[ignore = "requires access to /dev/kvm"]
    fn stale_interrupt_window_is_refreshed() {
        check_extint(false, |runner, vp| {
            refresh_interrupt_window(runner);
            assert!(runner.check_or_request_interrupt_window());

            // Disable interrupts from user mode, as a restore might.
            let mut regs = vp.get_regs().unwrap();
            regs.rflags &= !RFLAGS_IF;
            vp.set_regs(&regs).unwrap();
            refresh_interrupt_window(runner);
            assert!(!runner.check_or_request_interrupt_window());

            regs.rflags |= RFLAGS_IF;
            vp.set_regs(&regs).unwrap();
            refresh_interrupt_window(runner);
            assert!(runner.check_or_request_interrupt_window());
            inject(vp, VECTOR).unwrap();
        });
    }
}
