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

    /// Aligns the restored VP counters to the advanced BSP counter.
    pub(super) fn advance_snapshot_time(&self) -> Result<(), Error> {
        if self.vps.len() <= 1 {
            return Ok(());
        }

        // Per-VP counter writes run at different host times. Freeze before
        // aligning them to the advanced BSP counter; the first VP run thaws time.
        self.freeze_time()?;
        synchronize_restored_tscs(&self.vmfd, &self.finalized()?.bsp_vcpufd, self.vps.len())
    }

    pub(super) fn apic_frequency_hz(&self) -> Result<Option<u64>, Error> {
        Ok(Some(
            self.vmfd
                .get_partition_property(HvPartitionPropertyCode::ApicFrequency.0)
                .map_err(|error| ErrorInner::GetPartitionProperty(error.into()))?,
        ))
    }
}

fn synchronize_restored_tscs(vmfd: &VmFd, bsp: &VcpuFd, vp_count: usize) -> Result<(), Error> {
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
    for vp_index in 1..vp_count as u32 {
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
    use crate::x86_64::partition_create_args;
    use mshv_ioctls::Mshv;
    use test_with_tracing::test;

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
    #[ignore = "requires /dev/mshv"]
    fn restored_tscs_are_identical_while_partition_time_is_frozen() {
        for vp_count in [1, 2, 4, 8] {
            let mshv = Mshv::new().unwrap();
            let vmfd = mshv.create_vm().unwrap();
            vmfd.initialize().unwrap();
            let vps: Vec<_> = (0..vp_count)
                .map(|index| vmfd.create_vcpu(index).unwrap())
                .collect();
            vmfd.set_partition_property(HvPartitionPropertyCode::TimeFreeze.0, 1)
                .unwrap();
            let expected_tsc = 1_000_000_u64;
            for (index, vp) in vps.iter().enumerate() {
                vp.set_hvdef_regs(&[HvRegisterAssoc::from((
                    HvX64RegisterName::Tsc,
                    expected_tsc + index as u64 * 100_000,
                ))])
                .unwrap();
            }

            synchronize_restored_tscs(&vmfd, &vps[0], vps.len()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));

            for (index, vp) in vps.iter().enumerate() {
                let mut registers = [HvRegisterAssoc::from((HvX64RegisterName::Tsc, 0_u64))];
                vp.get_hvdef_regs(&mut registers).unwrap();
                assert_eq!(
                    registers[0].value.as_u64(),
                    expected_tsc,
                    "VP {index} TSC differs with {vp_count} VPs"
                );
            }
        }
    }
}
