// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TSC and clock support: the snapshot-downtime TSC offset adjustment, the
//! CPUID leaves that expose the exact TSC frequency, and the partition clock
//! controls used by snapshot restore.

use crate::KvmError;
use crate::KvmPartitionInner;
use std::time::Duration;
use virt::CpuidLeafSet;
use vm_topology::processor::x86::X86VpInfo;
use x86defs::cpuid::CpuidFunction;

/// KVM's in-kernel local APIC uses a fixed one-nanosecond bus cycle.
pub(super) const APIC_FREQUENCY_HZ: u64 = 1_000_000_000;

fn kvm_vcpu_id(vp_info: &X86VpInfo) -> u32 {
    vp_info.apic_id
}

/// Adds the CPUID leaves that expose the BSP's exact TSC frequency.
pub(super) fn add_frequency_leaves(
    vm: &kvm::Partition,
    bsp_vcpu_id: u32,
    cpuid: CpuidLeafSet,
) -> Result<CpuidLeafSet, KvmError> {
    let tsc_frequency_hz = vm.vp(bsp_vcpu_id).tsc_frequency_hz()?;
    let current_max_basic_leaf = cpuid.result(CpuidFunction::VendorAndMaxFunction.0, 0, &[0; 4])[0];
    let mut cpuid = cpuid.into_leaves();
    cpuid.extend(virt::x86::tsc::tsc_frequency_cpuid_leaves(
        tsc_frequency_hz,
        current_max_basic_leaf,
    )?);
    Ok(CpuidLeafSet::new(cpuid))
}

impl KvmPartitionInner {
    pub(super) fn tsc_frequency_hz(&self) -> Result<Option<u64>, KvmError> {
        let bsp_vcpu_id = kvm_vcpu_id(&self.bsp().vp_info);
        Ok(Some(self.kvm.vp(bsp_vcpu_id).tsc_frequency_hz()?))
    }

    pub(super) fn set_tsc_frequency_hz(&self, frequency_hz: u64) -> Result<(), KvmError> {
        for vp in &self.vps {
            self.kvm
                .vp(kvm_vcpu_id(&vp.vp_info))
                .set_tsc_frequency_hz(frequency_hz)?;
        }
        Ok(())
    }

    pub(super) fn advance_snapshot_time(&self, duration: Duration) -> Result<(), KvmError> {
        let clock = self.kvm.get_clock_ns()?;
        let delta =
            u64::try_from(duration.as_nanos()).map_err(|_| KvmError::SnapshotClockOverflow)?;
        let requested_clock = clock
            .clock
            .checked_add(delta)
            .ok_or(KvmError::SnapshotClockOverflow)?;
        self.kvm.set_clock_ns(requested_clock)?;
        Ok(())
    }
}

pub(crate) fn advance_tsc(vp: &kvm::Processor<'_>, cycles: u64) -> Result<(), KvmError> {
    // KVM can discard sub-second IA32_TSC writes as synchronization attempts.
    // Adjusting the offset also avoids introducing read/write latency skew.
    let offset = vp.tsc_offset()?;
    vp.set_tsc_offset(offset.wrapping_add(cycles))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::advance_tsc;
    use super::kvm_vcpu_id;
    use test_with_tracing::test;
    use virt::VpIndex;
    use vm_topology::processor::VpInfo;
    use vm_topology::processor::x86::X86VpInfo;

    #[test]
    fn kvm_vcpu_ids_follow_sparse_apic_ids() {
        let vp0 = X86VpInfo {
            base: VpInfo {
                vp_index: VpIndex::new(0),
                vnode: 0,
            },
            apic_id: 253,
        };
        let vp1 = X86VpInfo {
            base: VpInfo {
                vp_index: VpIndex::new(1),
                vnode: 1,
            },
            apic_id: 254,
        };

        assert_eq!(kvm_vcpu_id(&vp0), 253);
        assert_eq!(kvm_vcpu_id(&vp1), 254);
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn snapshot_tsc_downtime_advances_exact_cycles() {
        let kvm = kvm::Kvm::new().unwrap();
        for vp_count in [1, 2, 4, 8] {
            let mut partition = kvm.new_vm(kvm::VmType::Default).unwrap();
            for index in 0..vp_count {
                partition.add_vp(index).unwrap();
            }
            let frequency = partition.vp(0).tsc_frequency_hz().unwrap();
            for index in 0..vp_count {
                partition
                    .vp(index)
                    .set_msrs(&[(x86defs::X86X_MSR_TSC, frequency * 10)])
                    .unwrap();
            }
            for cycles in [frequency / 4, frequency * 2, 0, 1] {
                for index in 0..vp_count {
                    let vp = partition.vp(index);
                    let before = vp.tsc_offset().unwrap();
                    advance_tsc(&vp, cycles).unwrap();
                    assert_eq!(
                        vp.tsc_offset().unwrap(),
                        before.wrapping_add(cycles),
                        "VP {index} of {vp_count}, delta {cycles} cycles"
                    );
                }
            }
        }
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn snapshot_tsc_downtime_preserves_wrapping_per_vp_offsets() {
        let kvm = kvm::Kvm::new().unwrap();
        let mut partition = kvm.new_vm(kvm::VmType::Default).unwrap();
        let offsets = [u64::MAX - 10, 0, 1_000, 10_000];
        for (index, offset) in offsets.into_iter().enumerate() {
            partition.add_vp(index as u32).unwrap();
            partition.vp(index as u32).set_tsc_offset(offset).unwrap();
        }
        for (index, offset) in offsets.into_iter().enumerate() {
            let vp = partition.vp(index as u32);
            advance_tsc(&vp, 100).unwrap();
            assert_eq!(vp.tsc_offset().unwrap(), offset.wrapping_add(100));
        }
    }
}
