// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A save's check of a vCPU's system management mode (SMM) state.
//!
//! A vCPU is in SMM from the moment that it takes a system management
//! interrupt (SMI) until it executes `RSM`. KVM holds an SMI that reaches a
//! vCPU until the vCPU next runs, as a pending SMI, which the vCPU can take
//! only then. The saved VP state has no SMM state, and the activity element
//! reads none of what KVM reports in the `smi` field of `KVM_GET_VCPU_EVENTS`:
//! whether the vCPU is in SMM, whether an SMI is pending, whether the SMI
//! interrupted an NMI, and whether an INIT is latched behind SMM. A restore
//! starts a vCPU outside SMM, with no SMI pending, so a save of a vCPU with an
//! SMI pending would lose the SMI, and a save of a vCPU in SMM would run the
//! registers of the SMM handler as ordinary code, and never return to the code
//! that the SMI interrupted.
//!
//! OpenVMM maps guest RAM only in KVM's normal address space, and none in its
//! SMM address space. With RAM mapped that way, KVM cannot write the state that
//! an SMI interrupts to SMRAM when a vCPU takes the SMI, and marks the VM dead
//! (`enter_smm` of Linux 6.6 through 6.18). Measured on nvx-kvm-test, an Intel
//! Xeon Platinum 8573C VM that runs Linux 6.18.57, with RAM in memory slot 0
//! alone and an SMI pending: `KVM_RUN` failed with `EIO`, and so did the next
//! `KVM_GET_VCPU_EVENTS`. A save can therefore meet an SMI only while a stopped
//! vCPU holds it. A change that maps memory for SMM lets a guest reach SMM, and
//! must revisit this module, and whether the saved state needs SMM state.
//!
//! A save refuses such a vCPU, which only the guest can bring out of SMM, so
//! that a capture rolls back and the guest may request it again. Reading the
//! events leaves the vCPU as it is: KVM turns an SMI that it has been asked to
//! inject into a pending one, which the vCPU takes when it next runs, as it
//! would have.

use crate::KvmError;

/// Refuses a save of the VP `vp_index` while `smi`, the `smi` field of its
/// `KVM_GET_VCPU_EVENTS`, reports it in SMM or with an SMI pending.
///
/// A latched INIT is the capture's check of INIT and SIPI, not this one's.
pub(super) fn check(
    vp_index: u32,
    smi: &kvm::kvm_vcpu_events__bindgen_ty_4,
) -> Result<(), KvmError> {
    if smi.smm != 0 || smi.pending != 0 {
        return Err(KvmError::SmmState { vp_index });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::check;
    use crate::KvmError;
    use kvm::kvm_vcpu_events__bindgen_ty_4 as Smi;
    use test_with_tracing::test;

    #[test]
    fn a_vp_in_smm_or_with_an_smi_pending_is_refused() {
        for smi in [
            Smi {
                smm: 1,
                ..Default::default()
            },
            Smi {
                pending: 1,
                ..Default::default()
            },
            Smi {
                smm: 1,
                pending: 1,
                smm_inside_nmi: 1,
                latched_init: 1,
            },
        ] {
            let err = check(3, &smi).unwrap_err();
            assert!(
                matches!(err, KvmError::SmmState { vp_index: 3 }),
                "{smi:?}: {err:?}"
            );
        }
    }

    #[test]
    fn a_vp_outside_smm_with_no_smi_pending_is_saved() {
        check(0, &Smi::default()).unwrap();
    }
}

/// Tests against the host's KVM, with a VP that never runs.
#[cfg(test)]
mod kvm_tests {
    use super::check;
    use crate::KvmError;
    use crate::arch::x86_64::KvmMsi;
    use kvm::KVM_VCPUEVENT_VALID_SMM;
    use kvm::Kvm;
    use kvm::Partition;
    use kvm::VmType;
    use kvm::kvm_msi;
    use kvm::kvm_vcpu_events__bindgen_ty_4 as Smi;
    use test_with_tracing::test;
    use virt::irqcon::DeliveryMode;
    use virt::irqcon::MsiRequest;

    /// A VM of one VP, whose APIC ID is 0, with a split interrupt controller.
    fn partition() -> Partition {
        kvm::init();
        let mut partition = Kvm::new().unwrap().new_vm(VmType::Default).unwrap();
        partition.enable_split_irqchip(24).unwrap();
        partition.add_vp(0).unwrap();
        // An MSI reaches a new VP only once KVM has its LAPIC state, since KVM
        // builds its table of destinations from that.
        let vp = partition.vp(0);
        let mut lapic = [0; 1024];
        vp.get_lapic(&mut lapic).unwrap();
        vp.set_lapic(&lapic).unwrap();
        partition
    }

    /// Sends the VP an SMI as a device's MSI does.
    fn send_smi(partition: &Partition) {
        let KvmMsi {
            address_lo,
            address_hi,
            data,
        } = KvmMsi::new(MsiRequest::new_x86(DeliveryMode::SMI, 0, false, 0, false)).unwrap();
        partition
            .request_msi(&kvm_msi {
                address_lo,
                address_hi,
                data,
                flags: 0,
                devid: 0,
                pad: [0; 12],
            })
            .unwrap();
    }

    #[test]
    #[ignore = "requires access to /dev/kvm"]
    fn an_smi_that_a_vp_has_not_taken_is_refused() {
        let partition = partition();
        let vp = partition.vp(0);
        check(0, &vp.get_vcpu_events().unwrap().smi).unwrap();

        send_smi(&partition);

        // Reading the events does not take the SMI, so every read reports it.
        for _ in 0..2 {
            let smi = vp.get_vcpu_events().unwrap().smi;
            assert_eq!((smi.smm, smi.pending), (0, 1));
            let err = check(0, &smi).unwrap_err();
            assert!(matches!(err, KvmError::SmmState { vp_index: 0 }), "{err:?}");
        }
    }

    #[test]
    #[ignore = "requires access to /dev/kvm"]
    fn a_vp_in_smm_is_refused() {
        let partition = partition();
        let vp = partition.vp(0);

        for (smm_inside_nmi, latched_init) in [(0, 0), (1, 1)] {
            let mut events = vp.get_vcpu_events().unwrap();
            events.flags = KVM_VCPUEVENT_VALID_SMM;
            events.smi = Smi {
                smm: 1,
                pending: 0,
                smm_inside_nmi,
                latched_init,
            };
            vp.set_vcpu_events(&events).unwrap();

            let smi = vp.get_vcpu_events().unwrap().smi;
            assert_eq!((smi.smm, smi.pending), (1, 0));
            let err = check(0, &smi).unwrap_err();
            assert!(matches!(err, KvmError::SmmState { vp_index: 0 }), "{err:?}");
        }
    }
}
