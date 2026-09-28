// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TSC support: the TSC deadline timer and TSC_ADJUST feature policy, the
//! CPUID leaves that expose the exact TSC frequency, the clock frequencies
//! used by snapshot restore, and restored-TSC synchronization.

use crate::Error;
use crate::ErrorInner;
use crate::KernelError;
use crate::MshvPartitionInner;
use crate::VcpuFdExt;
use hvdef::HvPartitionPropertyCode;
use hvdef::HvX64RegisterName;
use hvdef::hypercall::HvRegisterAssoc;
use mshv_ioctls::VcpuFd;
use mshv_ioctls::VmFd;
use x86defs::cpuid::CpuidFunction;

/// Returns the processor features (bank 1) to expose: the supported features
/// without the TSC deadline timer, and without TSC_ADJUST for a versioned CPU
/// contract.
fn supported_features1(versioned_cpu_contract: bool) -> hvdef::HvX64PartitionProcessorFeatures1 {
    super::supported_processor_features1()
        .with_tsc_deadline_tmr_support(false)
        .with_tsc_adjust_support(!versioned_cpu_contract)
}

/// Applies [`supported_features1`] to the partition creation arguments.
pub(super) fn with_features1(
    mut args: mshv_bindings::mshv_create_partition_v2,
    versioned_cpu_contract: bool,
) -> mshv_bindings::mshv_create_partition_v2 {
    let mut banks = args.pt_cpu_fbanks;
    banks[1] = !u64::from(supported_features1(versioned_cpu_contract));
    args.pt_cpu_fbanks = banks;
    args
}

/// Adds the CPUID leaves that expose the exact TSC frequency and that hide the
/// TSC deadline timer.
pub(super) fn add_cpuid_leaves(
    vmfd: &VmFd,
    cpuid: Vec<virt::CpuidLeaf>,
) -> Result<Vec<virt::CpuidLeaf>, Error> {
    let cpuid = virt::CpuidLeafSet::new(cpuid);
    let tsc_frequency_hz = vmfd
        .get_partition_property(HvPartitionPropertyCode::ProcessorClockFrequency.0)
        .map_err(|error| ErrorInner::GetPartitionProperty(error.into()))?;
    let current_max_basic_leaf = cpuid.result(CpuidFunction::VendorAndMaxFunction.0, 0, &[0; 4])[0];
    let mut cpuid = cpuid.into_leaves();
    cpuid.extend(virt::x86::tsc::tsc_frequency_cpuid_leaves(
        tsc_frequency_hz,
        current_max_basic_leaf,
    )?);
    cpuid.push(
        virt::CpuidLeaf::new(CpuidFunction::VersionAndFeatures.0, [0; 4]).masked([
            0,
            0,
            1 << 24,
            0,
        ]),
    );
    Ok(cpuid)
}

impl MshvPartitionInner {
    pub(super) fn tsc_frequency_hz(&self) -> Result<Option<u64>, Error> {
        Ok(Some(
            self.vmfd
                .get_partition_property(HvPartitionPropertyCode::ProcessorClockFrequency.0)
                .map_err(|error| ErrorInner::GetPartitionProperty(error.into()))?,
        ))
    }

    pub(super) fn set_tsc_frequency_hz(&self, frequency_hz: u64) -> Result<(), Error> {
        let destination = self
            .vmfd
            .get_partition_property(HvPartitionPropertyCode::ProcessorClockFrequency.0)
            .map_err(|error| ErrorInner::GetPartitionProperty(error.into()))?;
        if frequency_hz != destination {
            return Err(ErrorInner::TscFrequencyMismatch {
                saved: frequency_hz,
                destination,
            }
            .into());
        }
        Ok(())
    }

    /// Aligns the restored counters of the created application processors to
    /// the advanced BSP counter.
    pub(super) fn advance_snapshot_time(&self) -> Result<(), Error> {
        let aps = created_aps(
            self.vps
                .iter()
                .map(|vp| vp.created.load(std::sync::atomic::Ordering::Acquire)),
        );
        // Uncreated VPs are skipped silently, so record how many are aligned.
        tracing::info!(
            created_aps = aps.len(),
            vp_capacity = self.vps.len(),
            "aligning restored AP TSCs to the BSP"
        );
        if aps.is_empty() {
            return Ok(());
        }

        // Per-VP counter writes run at different host times. Freeze before
        // aligning them to the advanced BSP counter; the first VP run thaws time.
        self.freeze_time()?;
        synchronize_restored_tscs(&self.vmfd, &self.finalized()?.bsp_vcpufd, &aps)
    }

    pub(super) fn apic_frequency_hz(&self) -> Result<Option<u64>, Error> {
        Ok(Some(
            self.vmfd
                .get_partition_property(HvPartitionPropertyCode::ApicFrequency.0)
                .map_err(|error| ErrorInner::GetPartitionProperty(error.into()))?,
        ))
    }
}

/// Returns the VP indices of the created application processors, given
/// whether each VP of the topology was created. A restore-time VP prefix
/// leaves the suffix VPs uncreated, and the hypervisor rejects register access
/// to a VP that does not exist.
fn created_aps(created: impl IntoIterator<Item = bool>) -> Vec<u32> {
    created
        .into_iter()
        .enumerate()
        .skip(1)
        .filter_map(|(vp_index, created)| created.then_some(vp_index as u32))
        .collect()
}

fn synchronize_restored_tscs(vmfd: &VmFd, bsp: &VcpuFd, aps: &[u32]) -> Result<(), Error> {
    let mut registers = [HvRegisterAssoc::from((HvX64RegisterName::Tsc, 0_u64))];
    bsp.get_hvdef_regs(&mut registers)
        .map_err(ErrorInner::Register)?;

    #[repr(C)]
    struct SetTsc {
        header: hvdef::hypercall::GetSetVpRegisters,
        register: HvRegisterAssoc,
    }

    let mut input = SetTsc {
        header: hvdef::hypercall::GetSetVpRegisters {
            partition_id: 0,
            vp_index: 0,
            target_vtl: hvdef::hypercall::HvInputVtl::CURRENT_VTL,
            rsvd: [0; 3],
        },
        register: registers[0],
    };
    for &vp_index in aps {
        input.header.vp_index = vp_index;
        let mut args = mshv_bindings::mshv_root_hvcall {
            code: hvdef::HypercallCode::HvCallSetVpRegisters.0,
            in_sz: size_of::<SetTsc>() as u16,
            in_ptr: std::ptr::addr_of!(input) as u64,
            reps: 1,
            ..Default::default()
        };
        vmfd.hvcall(&mut args)
            .map_err(|error| ErrorInner::SynchronizeTsc {
                vp_index,
                error: error.into(),
            })?;
        if args.reps != 1 {
            return Err(ErrorInner::SynchronizeTsc {
                vp_index,
                error: KernelError::Kernel(std::io::Error::from_raw_os_error(libc::EINTR)),
            }
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LinuxMshv;
    use crate::x86_64::partition_create_args;
    use guestmem::GuestMemory;
    use hvdef::HvError;
    use hvdef::HypercallCode;
    use hvdef::Vtl;
    use mshv_ioctls::Mshv;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use std::borrow::Borrow;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;
    use virt::BindProcessor;
    use virt::Hypervisor;
    use virt::Partition;
    use virt::PartitionConfig;
    use virt::PartitionMemoryMapper;
    use virt::ProtoPartition;
    use virt::ProtoPartitionConfig;
    use vm_topology::memory::MemoryLayout;
    use vm_topology::processor::TopologyBuilder;
    use vm_topology::processor::x86::X2ApicState;
    use vmcore::vmtime::VmTime;
    use vmcore::vmtime::VmTimeKeeper;

    #[test]
    fn versioned_cpu_contract_does_not_expose_tsc_adjust() {
        for versioned_cpu_contract in [false, true] {
            let processor_features1 = supported_features1(versioned_cpu_contract);
            assert_eq!(
                processor_features1.tsc_adjust_support(),
                !versioned_cpu_contract
            );
            let args = with_features1(
                partition_create_args(&virt::ProtoPartitionIsolation::None, false, false).unwrap(),
                versioned_cpu_contract,
            );
            let pt_cpu_fbanks = args.pt_cpu_fbanks;
            assert_eq!(pt_cpu_fbanks[1], !u64::from(processor_features1));
        }
    }

    #[test]
    fn restored_tsc_synchronization_targets_only_created_aps() {
        // The BSP is the synchronization source, never a target.
        assert!(created_aps([true]).is_empty());
        assert!(created_aps([true, false, false, false]).is_empty());
        assert_eq!(created_aps([true, true, false, false]), [1]);
        assert_eq!(created_aps([true, true, true, true]), [1, 2, 3]);
    }

    fn create_frozen_partition(vp_count: u8) -> (VmFd, Vec<VcpuFd>) {
        let mshv = Mshv::new().unwrap();
        let vmfd = mshv.create_vm().unwrap();
        vmfd.initialize().unwrap();
        let vps: Vec<_> = (0..vp_count)
            .map(|index| vmfd.create_vcpu(index).unwrap())
            .collect();
        vmfd.set_partition_property(HvPartitionPropertyCode::TimeFreeze.0, 1)
            .unwrap();
        (vmfd, vps)
    }

    fn set_skewed_tscs(vps: &[impl Borrow<VcpuFd>], tsc: u64) {
        for (index, vp) in vps.iter().enumerate() {
            vp.borrow()
                .set_hvdef_regs(&[HvRegisterAssoc::from((
                    HvX64RegisterName::Tsc,
                    tsc + index as u64 * 100_000,
                ))])
                .unwrap();
        }
    }

    fn assert_tscs(vps: &[impl Borrow<VcpuFd>], expected_tsc: u64) {
        for (index, vp) in vps.iter().enumerate() {
            let mut registers = [HvRegisterAssoc::from((HvX64RegisterName::Tsc, 0_u64))];
            vp.borrow().get_hvdef_regs(&mut registers).unwrap();
            assert_eq!(
                registers[0].value.as_u64(),
                expected_tsc,
                "VP {index} TSC differs with {} VPs",
                vps.len()
            );
        }
    }

    #[test]
    #[ignore = "requires /dev/mshv"]
    fn restored_tscs_are_identical_while_partition_time_is_frozen() {
        for vp_count in [1, 2, 4, 8] {
            let (vmfd, vps) = create_frozen_partition(vp_count);
            let expected_tsc = 1_000_000_u64;
            set_skewed_tscs(&vps, expected_tsc);

            let aps = created_aps(std::iter::repeat_n(true, usize::from(vp_count)));
            synchronize_restored_tscs(&vmfd, &vps[0], &aps).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));

            assert_tscs(&vps, expected_tsc);
        }
    }

    #[test]
    #[ignore = "requires /dev/mshv"]
    fn restored_tsc_synchronization_skips_uncreated_suffix_vps() {
        const VP_CAPACITY: u8 = 8;
        for vp_count in [1, 2, 4] {
            let (vmfd, vps) = create_frozen_partition(vp_count);
            let expected_tsc = 1_000_000_u64;
            set_skewed_tscs(&vps, expected_tsc);

            // The hypervisor rejects register access to an uncreated VP.
            let uncreated = u32::from(vp_count);
            let failure = synchronize_restored_tscs(&vmfd, &vps[0], &[uncreated]).unwrap_err();
            match failure.0 {
                ErrorInner::SynchronizeTsc {
                    vp_index,
                    error: KernelError::Hypercall { code, error },
                } => {
                    assert_eq!(vp_index, uncreated);
                    assert_eq!(code, HypercallCode::HvCallSetVpRegisters);
                    assert_eq!(error, HvError::InvalidVpIndex);
                }
                other => panic!("unexpected error: {other:?}"),
            }

            let aps = created_aps((0..VP_CAPACITY).map(|index| index < vp_count));
            synchronize_restored_tscs(&vmfd, &vps[0], &aps).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));

            assert_tscs(&vps, expected_tsc);
        }
    }

    #[async_test]
    #[ignore = "requires /dev/mshv"]
    async fn restored_tsc_synchronization_aligns_only_bound_vps(driver: DefaultDriver) {
        const VP_CAPACITY: u32 = 4;
        let processor_topology = TopologyBuilder::new_x86()
            .x2apic(X2ApicState::Supported)
            .build(VP_CAPACITY)
            .unwrap();
        let mem_layout = MemoryLayout::new(0x400000, &[], &[], &[], None).unwrap();
        let vmtime_keeper = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
        let vmtime = vmtime_keeper.builder().build(&driver).await.unwrap();

        for vp_count in [1, 2, VP_CAPACITY] {
            let guest_memory = GuestMemory::allocate(mem_layout.end_of_ram() as usize);
            let mut mshv = LinuxMshv::new().unwrap();
            let (partition, mut binders) = mshv
                .new_partition(ProtoPartitionConfig {
                    processor_topology: &processor_topology,
                    hv_config: None,
                    vmtime: &vmtime,
                    isolation: virt::ProtoPartitionIsolation::None,
                    nested_virt: false,
                    user_mode_memory_faults: false,
                    lazy_memory_registration: false,
                    versioned_cpu_contract: false,
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
            let ram = guest_memory.inner_buf().unwrap();
            // SAFETY: the guest memory outlives the partition.
            unsafe {
                partition.memory_mapper(Vtl::Vtl0).map_range(
                    ram.as_ptr().cast_mut().cast(),
                    ram.len(),
                    0,
                    true,
                    true,
                )
            }
            .unwrap();
            partition.finalize_memory().unwrap();

            // A restore binds only the VPs that it runs, which creates them.
            for binder in &mut binders[..vp_count as usize] {
                binder.bind().unwrap();
            }
            let created: Vec<_> = partition
                .inner
                .vps
                .iter()
                .map(|vp| vp.created.load(Ordering::Acquire))
                .collect();
            let expected_created: Vec<_> = (0..VP_CAPACITY).map(|index| index < vp_count).collect();
            assert_eq!(created, expected_created);

            partition.inner.freeze_time().unwrap();
            let bound: Vec<&VcpuFd> =
                std::iter::once(&partition.inner.finalized().unwrap().bsp_vcpufd)
                    .chain(
                        binders[1..vp_count as usize]
                            .iter()
                            .map(|binder| binder.vcpufd.as_ref().unwrap()),
                    )
                    .collect();
            let expected_tsc = 1_000_000_u64;
            set_skewed_tscs(&bound, expected_tsc);

            partition.inner.advance_snapshot_time().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));

            assert_tscs(&bound, expected_tsc);
        }
    }
}
