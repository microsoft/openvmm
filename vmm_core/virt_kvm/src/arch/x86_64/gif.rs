// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A save's check of a vCPU's global interrupt flag (GIF).
//!
//! While AMD's GIF is clear, a vCPU takes no interrupt, NMI, or SMI, and KVM
//! holds an INIT latched. Only a guest with SVM can clear it: CLGI raises #UD
//! while EFER.SVME is clear. But before Linux 7.1, a KVM that uses virtual GIF
//! (vGIF) does not intercept CLGI, and runs the guest with SVME set in the
//! VMCB's EFER whatever the guest's, so a guest whose CPUID has no SVM can
//! clear GIF.
//!
//! The saved VP state has no GIF, so a save of such a vCPU would restore it
//! with GIF set: the interrupts and NMIs that the guest held off would be
//! delivered at once, and an INIT that KVM holds latched behind the clear GIF
//! would be lost. KVM reports the flag only in the flags of the vCPU's nested
//! state, so a save reads it there, and refuses a vCPU whose GIF is clear,
//! which only the guest can set again. A capture then rolls back, and the
//! guest may request it again.

use crate::KvmError;
use x86defs::cpuid::Vendor;

/// Refuses a save of the VP `vp_index` of a CPU of `vendor` while its GIF is
/// clear. `nested_state_header` reads the header of the VP's nested state.
///
/// A save refuses a VP whose GIF it cannot read either, since it cannot tell
/// whether the save would lose it.
pub(super) fn check(
    vendor: &Vendor,
    vp_index: u32,
    nested_state_header: impl FnOnce() -> Result<kvm::NestedStateHeader, kvm::Error>,
) -> Result<(), KvmError> {
    // Only AMD has a GIF, so there is no need to read an Intel VP's.
    if !vendor.is_amd_compatible() {
        return Ok(());
    }
    if nested_state_header()?.global_interrupt_flag() == Some(false) {
        return Err(KvmError::GifClear { vp_index });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::check;
    use crate::KvmError;
    use crate::arch::x86_64::test_guest::GuestRam;
    use kvm::Exit;
    use kvm::KVM_MP_STATE_RUNNABLE;
    use kvm::KVM_STATE_NESTED_FORMAT_SVM;
    use kvm::KVM_STATE_NESTED_GIF_SET;
    use kvm::KVM_STATE_NESTED_GUEST_MODE;
    use kvm::Kvm;
    use kvm::NestedStateHeader;
    use kvm::VmType;
    use kvm::kvm_dtable;
    use kvm::kvm_regs;
    use kvm::kvm_segment;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::time::Duration;
    use test_with_tracing::test;
    use x86defs::cpuid::Vendor;

    const CODE_GPA: u64 = 0x1000;
    const DONE_PORT: u16 = 0x80;

    fn svm(flags: u32) -> NestedStateHeader {
        NestedStateHeader {
            flags: flags.try_into().unwrap(),
            format: KVM_STATE_NESTED_FORMAT_SVM.try_into().unwrap(),
        }
    }

    #[test]
    fn an_amd_vp_whose_gif_is_clear_is_refused() {
        // The flags that describe a nested guest do not set GIF.
        for vendor in [Vendor::AMD, Vendor::HYGON] {
            for flags in [0, KVM_STATE_NESTED_GUEST_MODE] {
                let err = check(&vendor, 3, || Ok(svm(flags))).unwrap_err();
                assert!(matches!(err, KvmError::GifClear { vp_index: 3 }), "{err:?}");
            }
        }
    }

    #[test]
    fn an_amd_vp_whose_gif_is_set_is_saved() {
        for flags in [
            KVM_STATE_NESTED_GIF_SET,
            KVM_STATE_NESTED_GIF_SET | KVM_STATE_NESTED_GUEST_MODE,
        ] {
            check(&Vendor::AMD, 0, || Ok(svm(flags))).unwrap();
        }
    }

    #[test]
    fn an_amd_vp_whose_gif_cannot_be_read_is_refused() {
        let err = check(&Vendor::AMD, 0, || {
            Err(kvm::Error::MissingCapability("KVM_CAP_NESTED_STATE"))
        })
        .unwrap_err();
        assert!(
            matches!(err, KvmError::Kvm(kvm::Error::MissingCapability(_))),
            "{err:?}"
        );
    }

    #[test]
    fn an_intel_vp_is_saved_without_reading_its_nested_state() {
        check(&Vendor::INTEL, 0, || {
            panic!("read the nested state of an Intel VP")
        })
        .unwrap();
    }

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

    #[test]
    #[ignore = "requires access to /dev/kvm"]
    fn a_new_vp_reports_the_nested_state_of_its_vendor() {
        let kvm = Kvm::new().unwrap();
        let mut partition = kvm.new_vm(VmType::Default).unwrap();
        partition.enable_split_irqchip(24).unwrap();
        partition.add_vp(0).unwrap();
        let header = partition.vp(0).nested_state_header().unwrap();
        // GIF is set at reset, and only AMD has one.
        let amd = host_is_amd(&kvm);
        assert_eq!(header.global_interrupt_flag(), amd.then_some(true));
        let vendor = if amd { Vendor::AMD } else { Vendor::INTEL };
        check(&vendor, 0, || partition.vp(0).nested_state_header()).unwrap();
    }

    /// Runs CLGI in a protected-mode guest whose CPUID has no SVM, and checks
    /// that a save agrees with what the guest saw: where CLGI faulted, as it
    /// does on every Intel host, and on an AMD host whose KVM does not use
    /// vGIF or has Linux 7.1's fix, GIF is set, but where it ran, as it does
    /// on an AMD host whose KVM uses vGIF and predates the fix, a save
    /// refuses the VP.
    #[test]
    #[ignore = "requires access to /dev/kvm"]
    fn a_save_refuses_a_vp_whose_guest_cleared_gif_without_svm() {
        kvm::init();

        let kvm = Kvm::new().unwrap();
        let amd = host_is_amd(&kvm);
        let mut partition = kvm.new_vm(VmType::Default).unwrap();
        partition.enable_split_irqchip(24).unwrap();
        partition.add_vp(0).unwrap();

        let ram = GuestRam::new(&partition, 0x10000);
        // clgi; mov al, 1; out DONE_PORT, al; hlt
        ram.write(CODE_GPA, &[0x0f, 0x01, 0xdd, 0xb0, 0x01, 0xe6, 0x80, 0xf4]);

        // A flat 32-bit protected-mode guest at CPL 0, with no IDT, so that a
        // fault in it ends in a triple fault.
        let vp = partition.vp(0);
        let code = kvm_segment {
            base: 0,
            limit: 0xffff_ffff,
            selector: 0x8,
            type_: 0xb,
            present: 1,
            dpl: 0,
            db: 1,
            s: 1,
            l: 0,
            g: 1,
            avl: 0,
            unusable: 0,
            padding: 0,
        };
        let data = kvm_segment {
            selector: 0x10,
            type_: 0x3,
            ..code
        };
        let mut sregs = vp.get_sregs().unwrap();
        sregs.cs = code;
        sregs.ds = data;
        sregs.es = data;
        sregs.fs = data;
        sregs.gs = data;
        sregs.ss = data;
        sregs.idt = kvm_dtable {
            base: 0,
            limit: 0,
            padding: [0; 3],
        };
        sregs.cr0 |= 1;
        vp.set_sregs(&sregs).unwrap();
        vp.set_regs(&kvm_regs {
            rip: CODE_GPA,
            rflags: 2,
            ..Default::default()
        })
        .unwrap();
        vp.set_mp_state(KVM_MP_STATE_RUNNABLE).unwrap();

        let timed_out = &AtomicBool::new(false);
        let partition = &partition;
        let executed = std::thread::scope(|s| {
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
            let executed = loop {
                match runner.run().unwrap() {
                    Exit::IoOut { port, .. } => {
                        assert_eq!(port, DONE_PORT);
                        break true;
                    }
                    Exit::Shutdown => break false,
                    Exit::Interrupted => {
                        assert!(
                            !timed_out.load(Ordering::SeqCst),
                            "timed out waiting for the guest"
                        );
                    }
                    exit => panic!("unexpected exit: {exit:?}"),
                }
            };
            drop(runner);
            drop(done);
            executed
        });

        let flag = vp.nested_state_header().unwrap().global_interrupt_flag();
        let vendor = if amd { Vendor::AMD } else { Vendor::INTEL };
        let result = check(&vendor, 0, || vp.nested_state_header());
        if executed {
            assert!(amd, "an Intel host ran CLGI");
            assert_eq!(flag, Some(false));
            assert!(
                matches!(result, Err(KvmError::GifClear { vp_index: 0 })),
                "{result:?}"
            );
        } else {
            assert_ne!(flag, Some(false));
            result.unwrap();
        }
    }
}
