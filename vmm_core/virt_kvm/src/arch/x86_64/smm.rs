// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A vCPU's system management mode (SMM) state: a save's check of it, and its
//! reset when the activity is set.
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
//! Where KVM has an SMM address space and the VM has no private memory, OpenVMM
//! maps guest RAM into both of KVM's address spaces, the normal one and the one
//! that a vCPU uses while in SMM (see `KvmPartitionInner::smm_address_space`),
//! so that a guest that sends itself an SMI takes it: KVM writes the state that
//! the SMI interrupts to SMRAM at SMBASE, which is in RAM, through the SMM
//! address space, and the vCPU runs the guest's handler there until the handler
//! executes `RSM`. With RAM in the normal address space alone, KVM could not
//! write that state and marked the VM dead (`enter_smm` of Linux 6.6 through
//! 6.18). Measured on nvx-kvm-test, an Intel Xeon Platinum 8573C VM that runs
//! Linux 6.18.57, with RAM in memory slot 0 alone and an SMI pending: `KVM_RUN`
//! failed with `EIO`, and so did the next `KVM_GET_VCPU_EVENTS`. A save can
//! therefore also meet a vCPU in SMM, not only a stopped vCPU that holds an SMI,
//! and the saved state has no SMM state to hold it.
//!
//! A save refuses such a vCPU, which only the guest can bring out of SMM, so
//! that a capture rolls back and the guest may request it again once its
//! handler has executed `RSM`. Reading the events leaves the vCPU as it is: KVM
//! turns an SMI that it has been asked to inject into a pending one, which the
//! vCPU takes when it next runs, as it would have.
//!
//! A restore and a reset do the reverse. The state that they set describes a
//! vCPU outside SMM, with no SMI pending and no INIT latched, as a saved state
//! always does, but setting the events leaves the SMM state as it is, so a
//! restore or a reset of a vCPU that has already run would leave it in SMM, or
//! with its SMI pending. Setting the activity therefore takes the vCPU out of
//! SMM first.

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

/// Takes the stopped vCPU `vp` out of SMM, with no SMI pending and no INIT
/// latched, and leaves the rest of its events as they are: OpenVMM enables
/// neither `KVM_CAP_EXCEPTION_PAYLOAD` nor the triple-fault event, so the events
/// that KVM reports are the events that it takes.
///
/// KVM changes the SMM state only when the events that are set say that it is
/// valid (`KVM_VCPUEVENT_VALID_SMM`). It also replaces only the SMI that is
/// pending, not one that it has been asked to inject and has not yet made
/// pending, which only getting the events does, so this gets them first.
///
/// A vCPU in SMM cannot wait for a SIPI: until Linux 6.16, `KVM_SET_MP_STATE`
/// refuses to put it, or a vCPU with an SMI pending, in that state, and since
/// Linux 6.17, `KVM_RUN` refuses to run it. A reset puts every vCPU but the
/// first in that state, so the caller sets the MP state after this.
pub(super) fn leave(vp: &kvm::Processor<'_>) -> Result<(), KvmError> {
    let mut events = vp.get_vcpu_events()?;
    events.flags = kvm::KVM_VCPUEVENT_VALID_SMM;
    events.smi = Default::default();
    vp.set_vcpu_events(&events)?;
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
    use super::leave;
    use crate::KvmError;
    use crate::arch::x86_64::KvmMsi;
    use kvm::KVM_VCPUEVENT_VALID_SMM;
    use kvm::Kvm;
    use kvm::Partition;
    use kvm::VmType;
    use kvm::kvm_msi;
    use kvm::kvm_vcpu_events;
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

    #[test]
    #[ignore = "requires access to /dev/kvm"]
    fn leaving_smm_leaves_nothing_of_it() {
        let partition = partition();
        let vp = partition.vp(0);

        let mut events = vp.get_vcpu_events().unwrap();
        events.flags = KVM_VCPUEVENT_VALID_SMM;
        events.nmi.masked = 1;
        events.smi = Smi {
            smm: 1,
            pending: 1,
            smm_inside_nmi: 1,
            latched_init: 1,
        };
        vp.set_vcpu_events(&events).unwrap();
        let before = vp.get_vcpu_events().unwrap();
        assert_eq!(before.smi, events.smi);

        leave(&vp).unwrap();

        // The rest of the events are as they were.
        let after = vp.get_vcpu_events().unwrap();
        let expected = kvm_vcpu_events {
            smi: Smi::default(),
            ..before
        };
        assert_eq!(after, expected);
    }

    #[test]
    #[ignore = "requires access to /dev/kvm"]
    fn leaving_smm_drops_an_smi_whether_or_not_kvm_made_it_pending() {
        for made_pending in [false, true] {
            let partition = partition();
            let vp = partition.vp(0);
            send_smi(&partition);
            if made_pending {
                assert_eq!(vp.get_vcpu_events().unwrap().smi.pending, 1);
            }

            leave(&vp).unwrap();

            assert_eq!(
                vp.get_vcpu_events().unwrap().smi,
                Smi::default(),
                "made pending: {made_pending}"
            );
        }
    }
}

/// Tests against the host's KVM, with a partition whose RAM is mapped through
/// its memory mapper, as a VMM maps it, and a VP that runs the guest's SMI
/// handler.
#[cfg(test)]
mod ram_tests {
    use crate::Kvm;
    use crate::KvmPartition;
    use crate::KvmProcessorBinder;
    use crate::arch::x86_64::KvmProcessor;
    use guestmem::GuestMemory;
    use hvdef::Vtl;
    use pal_async::DefaultPool;
    use test_with_tracing::test;
    use virt::BindProcessor;
    use virt::Hypervisor;
    use virt::Partition;
    use virt::PartitionConfig;
    use virt::PartitionMemoryMapper;
    use virt::ProtoPartition;
    use virt::ProtoPartitionConfig;
    use virt::irqcon::DeliveryMode;
    use virt::irqcon::MsiRequest;
    use vm_topology::memory::MemoryLayout;
    use vm_topology::processor::TopologyBuilder;
    use vmcore::vmtime::VmTime;
    use vmcore::vmtime::VmTimeKeeper;

    /// The size of each range of RAM that a test maps: enough for KVM's default
    /// SMBASE, and the SMRAM above it.
    const RAM_LEN: usize = 0x40_0000;
    /// KVM's default SMBASE.
    const DEFAULT_SMBASE: u64 = 0x30000;
    /// Where KVM starts an SMI handler, from SMBASE.
    const SMI_HANDLER_OFFSET: u64 = 0x8000;
    /// `MSR_IA32_SMBASE`, which KVM lets its host set.
    const MSR_IA32_SMBASE: u32 = 0x9e;
    /// Where a VP runs when no SMI interrupts it.
    const CODE_GPA: u64 = 0x1000;
    /// The port that the code of `CODE_GPA` writes.
    const CODE_PORT: u16 = 0x82;

    /// Builds a KVM partition of one VP with no RAM mapped, and calls `f` with
    /// it and its VP binders.
    fn with_partition<R>(f: impl FnOnce(&KvmPartition, Vec<KvmProcessorBinder>) -> R) -> R {
        DefaultPool::run_with(async |driver| {
            let keeper = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
            let vmtime = keeper.builder().build(&driver).await.unwrap();
            let topology = TopologyBuilder::new_x86().build(1).unwrap();
            let mem_layout = MemoryLayout::new(RAM_LEN as u64, &[], &[], &[], None).unwrap();
            let guest_memory = GuestMemory::allocate(RAM_LEN);
            let (partition, binders) = Kvm::new()
                .unwrap()
                .new_partition(ProtoPartitionConfig {
                    processor_topology: &topology,
                    hv_config: None,
                    vmtime: &vmtime,
                    isolation: virt::ProtoPartitionIsolation::None,
                    nested_virt: false,
                    user_mode_memory_faults: false,
                    lazy_memory_registration: false,
                    time_abi: None,
                })
                .unwrap()
                .build(PartitionConfig {
                    mem_layout: &mem_layout,
                    guest_memory: &guest_memory,
                    cpuid: &[],
                    vtl0_alias_map: None,
                    fault_resolver: None,
                })
                .unwrap();
            f(&partition, binders)
        })
    }

    /// A range of RAM that a test maps into a partition, as a VMM does, and
    /// unmaps when it is dropped.
    struct Ram<'a> {
        partition: &'a KvmPartition,
        memory: GuestMemory,
        gpa: u64,
    }

    impl<'a> Ram<'a> {
        /// Maps new, zeroed RAM at `gpa`.
        fn new(partition: &'a KvmPartition, gpa: u64) -> Self {
            let mut ram = Self {
                partition,
                memory: GuestMemory::allocate(RAM_LEN),
                gpa,
            };
            ram.map(gpa);
            ram
        }

        /// Maps the RAM at `gpa`, which moves it if it is mapped elsewhere.
        fn map(&mut self, gpa: u64) {
            let buf = self.memory.inner_buf().unwrap();
            // SAFETY: the buffer stays allocated until `self` is dropped, which
            // unmaps it first.
            unsafe {
                self.partition.memory_mapper(Vtl::Vtl0).map_range(
                    buf.as_ptr().cast_mut().cast(),
                    buf.len(),
                    gpa,
                    true,
                    true,
                )
            }
            .unwrap();
            self.gpa = gpa;
        }

        fn write(&self, gpa: u64, data: &[u8]) {
            self.memory.write_at(gpa - self.gpa, data).unwrap();
        }

        /// Unmaps the RAM from the partition, which keeps its memory allocated.
        fn unmap(&self) -> anyhow::Result<()> {
            self.partition
                .memory_mapper(Vtl::Vtl0)
                .unmap_range(self.gpa, RAM_LEN as u64)
        }
    }

    impl Drop for Ram<'_> {
        fn drop(&mut self) {
            // A VM that KVM has marked dead fails this.
            let _ = self.unmap();
        }
    }

    /// Sends the VP an SMI as a device's MSI does, and checks that KVM holds it.
    fn send_smi(partition: &KvmPartition, vp: &KvmProcessor<'_>) {
        let msi = MsiRequest::new_x86(DeliveryMode::SMI, vp.inner.vp_info.apic_id, false, 0, false);
        partition.request_msi(Vtl::Vtl0, msi);
        assert_eq!(
            vp.kvm.get_vcpu_events().unwrap().smi.pending,
            1,
            "KVM does not hold the SMI"
        );
    }

    /// Runs the VP until its next write to a port, and returns the port.
    fn next_port_write(vp: &mut KvmProcessor<'_>) -> u16 {
        match vp.runner.run().unwrap() {
            kvm::Exit::IoOut { port, .. } => port,
            exit => panic!("unexpected exit: {exit:?}"),
        }
    }

    /// Whether the VP is in SMM.
    fn in_smm(vp: &KvmProcessor<'_>) -> bool {
        vp.kvm.get_vcpu_events().unwrap().smi.smm == 1
    }

    /// A guest SMI enters SMM on a partition whose RAM is mapped through its
    /// memory mapper: the VP runs the guest's handler at SMBASE, in RAM, and
    /// the VM stays alive. Before, the RAM was in KVM's address space 0 alone,
    /// so the SMM entry could not write the VP's state at SMBASE, and KVM marked
    /// the VM dead: the run failed, and so did every later ioctl of the VP.
    #[test]
    #[ignore = "requires /dev/kvm with SMM"]
    fn a_guest_smi_enters_smm_instead_of_killing_the_vm() {
        with_partition(|partition, mut binders| {
            let ram = Ram::new(partition, 0);
            // The handler reports itself on port 0x80 and loops, which parks
            // the VP in SMM.
            ram.write(DEFAULT_SMBASE + SMI_HANDLER_OFFSET, &[0xe7, 0x80, 0xeb, 0xfc]);
            let mut vp = binders[0].bind().unwrap();
            send_smi(partition, &vp);

            assert_eq!(next_port_write(&mut vp), 0x80, "the SMI's handler");
            // The VM is alive: the handler runs again, and a state ioctl of the
            // VP works, which reports it in SMM.
            assert_eq!(next_port_write(&mut vp), 0x80, "the SMI's handler again");
            assert!(in_smm(&vp), "the VP is not in SMM");
        })
    }

    /// RAM is in the SMM address space wherever it is mapped, since a guest can
    /// relocate SMBASE: an SMI enters SMM at SMBASE in each range of RAM, and a
    /// handler that executes `RSM` returns the VP to the code that the SMI
    /// interrupted. A range that is unmapped leaves nothing of itself in that
    /// address space, so another range can reuse its slot, and a range that is
    /// moved takes its mirror along.
    #[test]
    #[ignore = "requires /dev/kvm with SMM"]
    fn smm_has_the_ram_that_is_mapped() {
        with_partition(|partition, mut binders| {
            let low = Ram::new(partition, 0);
            let mut vp = binders[0].bind().unwrap();

            // The VP runs real-mode code that writes `CODE_PORT` in a loop,
            // wherever the SMIs leave it.
            low.write(CODE_GPA, &[0xe6, CODE_PORT as u8, 0xeb, 0xfc]);
            let mut sregs = vp.kvm.get_sregs().unwrap();
            sregs.cs.selector = 0;
            sregs.cs.base = 0;
            vp.kvm.set_sregs(&sregs).unwrap();
            vp.kvm
                .set_regs(&kvm::kvm_regs {
                    rip: CODE_GPA,
                    rsp: 0x8000,
                    rflags: 2,
                    ..Default::default()
                })
                .unwrap();

            // Sends an SMI to the VP with SMBASE at `smbase`, in `ram`, where
            // the handler reports itself on `port` and executes `RSM`.
            let mut take_smi = |ram: &Ram<'_>, smbase: u64, port: u8, what: &str| {
                ram.write(smbase + SMI_HANDLER_OFFSET, &[0xe7, port, 0x0f, 0xaa]);
                vp.kvm.set_msrs(&[(MSR_IA32_SMBASE, smbase)]).unwrap();
                send_smi(partition, &vp);
                assert_eq!(
                    next_port_write(&mut vp),
                    u16::from(port),
                    "{what}: the handler"
                );
                assert!(in_smm(&vp), "{what}: the VP is not in SMM");
                assert_eq!(next_port_write(&mut vp), CODE_PORT, "{what}: after RSM");
                assert!(!in_smm(&vp), "{what}: the VP is still in SMM after RSM");
            };

            take_smi(&low, DEFAULT_SMBASE, 0x80, "default SMBASE");

            let high = Ram::new(partition, 0x8000_0000);
            take_smi(&high, 0x8001_0000, 0x81, "SMBASE in a second range");
            // Unmap `high` and keep its memory allocated, so that the next range
            // has another host address: KVM would take a mirror of `high` that
            // is still there for a move of a range that has its address.
            high.unmap().unwrap();

            let mut other = Ram::new(partition, 0x9000_0000);
            take_smi(&other, 0x9001_0000, 0x83, "a range that reuses a slot");

            other.map(0xa000_0000);
            take_smi(&other, 0xa001_0000, 0x84, "a range that was moved");
        })
    }
}
