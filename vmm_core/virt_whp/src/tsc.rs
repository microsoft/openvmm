// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Restored-TSC support: the partition clock and its TSC model for restored
//! SMP partitions, TSC synchronization on restore, the CPUID leaves that
//! expose the exact TSC frequency, and the clock frequencies used by snapshot
//! restore.

use crate::Error;
use crate::LocalApicKind;
use crate::VtlPartition;
use crate::WhpPartitionInner;
use crate::WhpResultExt;
use inspect::Inspect;
use parking_lot::Mutex;
use x86defs::cpuid::CpuidFunction;

pub(crate) const IA32_TSC_ADJUST: u32 = 0x3b;
const REFERENCE_TIME_FREQUENCY: u128 = 10_000_000;

/// The TSC frequency and the restored-TSC clock of a partition.
#[derive(Inspect)]
pub(crate) struct PartitionClock {
    pub(crate) tsc_frequency_hz: u64,
    #[inspect(skip)]
    pub(crate) restored_tsc: Mutex<Option<RestoredTsc>>,
}

impl PartitionClock {
    pub(crate) fn new(tsc_frequency_hz: u64) -> Self {
        Self {
            tsc_frequency_hz,
            restored_tsc: Mutex::new(None),
        }
    }

    /// Checks that a saved TSC frequency matches this partition's.
    pub(crate) fn check_frequency(&self, frequency_hz: u64) -> Result<(), Error> {
        if frequency_hz != self.tsc_frequency_hz {
            return Err(Error::TscFrequencyMismatch {
                saved: frequency_hz,
                destination: self.tsc_frequency_hz,
            });
        }
        Ok(())
    }
}

/// Adds the CPUID leaves that expose the exact TSC frequency.
pub(crate) fn add_frequency_leaves(
    cpuid: Vec<virt::CpuidLeaf>,
    tsc_frequency_hz: u64,
    vtl0: &VtlPartition,
) -> Result<Vec<virt::CpuidLeaf>, Error> {
    let cpuid = virt::CpuidLeafSet::new(cpuid);
    let current_max_basic_leaf =
        cpuid.result(CpuidFunction::VendorAndMaxFunction.0, 0, &vtl0.cpuid(0, 0))[0];
    let mut cpuid = cpuid.into_leaves();
    cpuid.extend(virt::x86::tsc::tsc_frequency_cpuid_leaves(
        tsc_frequency_hz,
        current_max_basic_leaf,
    )?);
    Ok(cpuid)
}

impl WhpPartitionInner {
    /// Disables the restored-TSC clock before a partition reset.
    pub(crate) fn reset_restored_tsc(&self) -> Result<(), Error> {
        let mut restored_tsc = self.clock.restored_tsc.lock();
        if let Some(clock) = &*restored_tsc {
            clock.disable(&self.vtl0.whp)?;
        }
        *restored_tsc = None;
        Ok(())
    }

    /// Aligns the restored VP counters with partition time suspended, and
    /// enables the restored-TSC clock.
    pub(crate) fn advance_snapshot_time(&self) -> Result<(), Error> {
        if self.vps.len() <= 1 {
            return Ok(());
        }

        let partition = &self.vtl0.whp;
        partition
            .suspend_time()
            .for_op("suspend restored partition time")?;
        let tsc = synchronize_restored_tscs(partition, self.vps.len())?;
        if self.caps.nested_virt {
            return Ok(());
        }
        let leaf = CpuidFunction::ExtendedFeatures.0;
        let features = self.cpuid.result(leaf, 0, &self.vtl0.cpuid(leaf, 0));
        let tsc_adjust_supported =
            x86defs::cpuid::ExtendedFeatureSubleaf0Ebx::from(features[1]).tsc_adjust();
        let mut clock = self.clock.restored_tsc.lock();
        *clock = Some(RestoredTsc::enable(
            partition,
            tsc,
            self.clock.tsc_frequency_hz,
            self.vps.len(),
            tsc_adjust_supported,
            clock.as_ref(),
        )?);
        Ok(())
    }

    pub(crate) fn apic_frequency_hz(&self) -> Result<Option<u64>, Error> {
        let frequency = match &self.vtl0.lapic {
            LocalApicKind::Emulated(_) => virt_support_apic::TIMER_FREQUENCY,
            LocalApicKind::Offloaded => self
                .vtl0
                .whp
                .apic_frequency()
                .for_op("get APIC clock frequency")?,
        };
        Ok(Some(frequency))
    }
}

fn synchronize_restored_tscs(partition: &whp::Partition, vp_count: usize) -> Result<u64, Error> {
    let tsc = partition
        .vp(0)
        .get_register(whp::Register64::Tsc)
        .for_op("read restored BSP TSC")?;
    for vp_index in 1..vp_count as u32 {
        partition
            .vp(vp_index)
            .set_register(whp::Register64::Tsc, tsc)
            .for_op("synchronize restored VP TSC")?;
    }
    Ok(tsc)
}

pub(crate) struct RestoredTsc {
    clock: ReferenceTsc,
    original_exits: whp::abi::WHV_EXTENDED_VM_EXITS,
    original_msr_exits: whp::abi::WHV_X64_MSR_EXIT_BITMAP,
    pub(crate) tsc_adjust_supported: bool,
}

impl RestoredTsc {
    pub(crate) fn enable(
        partition: &whp::Partition,
        tsc: u64,
        frequency_hz: u64,
        vp_count: usize,
        tsc_adjust_supported: bool,
        previous: Option<&Self>,
    ) -> Result<Self, Error> {
        let exits = partition
            .extended_vm_exits()
            .for_op("get restored VM exits")?;
        let msr_exits = partition
            .x64_msr_exit_bitmap()
            .for_op("get restored MSR exits")?;
        let reference_time = partition
            .reference_time()
            .for_op("get restored TSC epoch")?;
        let (original_exits, original_msr_exits) = previous
            .map(|previous| (previous.original_exits, previous.original_msr_exits))
            .unwrap_or((exits, msr_exits));

        partition
            .set_property(whp::PartitionProperty::X64MsrExitBitmap(
                msr_exits
                    | whp::abi::WHV_X64_MSR_EXIT_BITMAP::TscMsrRead
                    | whp::abi::WHV_X64_MSR_EXIT_BITMAP::TscMsrWrite,
            ))
            .for_op("intercept restored TSC MSRs")?;
        partition
            .set_property(whp::PartitionProperty::ExtendedVmExits(
                exits
                    | whp::abi::WHV_EXTENDED_VM_EXITS::X64RdtscExit
                    | whp::abi::WHV_EXTENDED_VM_EXITS::X64MsrExit,
            ))
            .for_op("intercept restored TSC reads")?;

        Ok(Self {
            clock: ReferenceTsc::new(tsc, reference_time, frequency_hz, vp_count),
            original_exits,
            original_msr_exits,
            tsc_adjust_supported,
        })
    }

    pub(crate) fn disable(&self, partition: &whp::Partition) -> Result<(), Error> {
        partition
            .set_property(whp::PartitionProperty::ExtendedVmExits(self.original_exits))
            .for_op("reset restored TSC instruction exits")?;
        partition
            .set_property(whp::PartitionProperty::X64MsrExitBitmap(
                self.original_msr_exits,
            ))
            .for_op("reset restored TSC MSR exits")?;
        Ok(())
    }

    pub(crate) fn read(&self, vp_index: u32, reference_time: u64) -> Option<u64> {
        self.clock.read(vp_index, reference_time)
    }

    pub(crate) fn restore(&mut self, vp_index: u32, tsc: u64, reference_time: u64) {
        self.clock.restore(vp_index, tsc, reference_time);
    }

    pub(crate) fn guest_write(&mut self, vp_index: u32) {
        self.clock.guest_write(vp_index);
    }
}

pub(crate) fn msr_index(exit_msr: u32, write: bool, rcx: u64) -> u32 {
    // WHP reports TSC_ADJUST and TSC_DEADLINE writes as TSC writes.
    // ECX retains the original selector.
    if write && exit_msr == x86defs::X86X_MSR_TSC {
        rcx as u32
    } else {
        exit_msr
    }
}

struct ReferenceTsc {
    frequency_hz: u64,
    offsets: Vec<Option<u64>>,
}

impl ReferenceTsc {
    fn new(tsc: u64, reference_time: u64, frequency_hz: u64, vp_count: usize) -> Self {
        let mut clock = Self {
            frequency_hz,
            offsets: Vec::new(),
        };
        let offset = tsc.wrapping_sub(clock.ticks(reference_time));
        clock.offsets.resize(vp_count, Some(offset));
        clock
    }

    fn ticks(&self, reference_time: u64) -> u64 {
        // Reference time is partition-wide, in 100ns units. The architectural
        // TSC wraps at 64 bits, including at frequencies requiring a wide product.
        (u128::from(reference_time) * u128::from(self.frequency_hz) / REFERENCE_TIME_FREQUENCY)
            as u64
    }

    fn read(&self, vp_index: u32, reference_time: u64) -> Option<u64> {
        self.offsets[vp_index as usize]
            .map(|offset| self.ticks(reference_time).wrapping_add(offset))
    }

    fn restore(&mut self, vp_index: u32, tsc: u64, reference_time: u64) {
        self.offsets[vp_index as usize] = Some(tsc.wrapping_sub(self.ticks(reference_time)));
    }

    fn guest_write(&mut self, vp_index: u32) {
        // A guest explicitly programming one counter owns its new timeline.
        // Preserve WHP's native TSC/TSC_ADJUST and deadline-timer semantics.
        self.offsets[vp_index as usize] = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn tsc_adjust_write_uses_the_original_guest_msr_selector() {
        assert_eq!(msr_index(0x10, true, 0x10), 0x10);
        assert_eq!(msr_index(0x10, true, 0x3b), 0x3b);
        assert_eq!(msr_index(0x10, true, 0x6e0), 0x6e0);
        assert_eq!(msr_index(0x10, true, 0xffff_ffff_0000_003b), 0x3b);
        assert_eq!(msr_index(0x3b, false, 0x3b), 0x3b);
        assert_eq!(msr_index(0x6e0, false, 0x6e0), 0x6e0);
        assert_eq!(msr_index(0x10, false, 0x10), 0x10);
    }

    #[test]
    fn restored_counters_share_one_advancing_clock() {
        for frequency in [1_000_000_000, 2_793_439_000] {
            for count in [1, 2, 4, 8] {
                let clock = ReferenceTsc::new(123_456_789, 1234, frequency, count);
                let mut previous = 123_456_789;
                for reference in [1234, 1234, 1235, 1236, 10_001_234] {
                    for vp in 0..count as u32 {
                        let value = clock.read(vp, reference).unwrap();
                        assert!(value >= previous);
                        assert_eq!(clock.read(0, reference), Some(value));
                        previous = value;
                    }
                }
                assert_eq!(clock.read(0, 10_001_234), Some(123_456_789 + frequency));
            }
        }
    }

    #[test]
    fn counter_and_offset_arithmetic_wraps() {
        let clock = ReferenceTsc::new(u64::MAX - 100, 0, 1_000_000_000, 2);
        assert_eq!(clock.read(1, 2), Some(99));
        let clock = ReferenceTsc::new(1, 1000, 1_000_000_000, 2);
        assert_eq!(clock.read(0, 1000), Some(1));
        assert_eq!(clock.read(1, 1001), Some(101));
        let clock = ReferenceTsc::new(0, 0, 100_000_000, 1);
        assert_eq!(clock.read(0, u64::MAX), Some(u64::MAX.wrapping_mul(10)));
    }

    #[test]
    fn explicit_guest_writes_affect_only_the_target_counter() {
        let mut clock = ReferenceTsc::new(1000, 0, 1_000_000_000, 4);
        clock.guest_write(1);
        assert_eq!(clock.read(1, 10), None);
        for vp in [0, 2, 3] {
            assert_eq!(clock.read(vp, 10), Some(2000));
        }
        clock.restore(1, 7, 10);
        assert_eq!(clock.read(1, 10), Some(7));
        assert_eq!(clock.read(1, 11), Some(107));
        assert_eq!(clock.read(0, 11), Some(2100));
    }

    #[test]
    fn restored_reference_time_can_move_backwards() {
        let clock = ReferenceTsc::new(10_000, 100, 1_000_000_000, 2);
        assert_eq!(clock.read(0, 50), Some(5000));
        assert_eq!(clock.read(1, 100), Some(10_000));
    }

    #[test]
    #[ignore = "requires WHP"]
    fn restored_tscs_are_identical_while_partition_time_is_suspended() {
        for vp_count in [1, 2, 4, 8] {
            let mut config = whp::PartitionConfig::new().unwrap();
            config
                .set_property(whp::PartitionProperty::ProcessorCount(vp_count))
                .unwrap();
            let partition = config.create().unwrap();
            for vp_index in 0..vp_count {
                partition.create_vp(vp_index).create().unwrap();
            }

            partition.suspend_time().unwrap();
            let expected_tsc = 1_000_000_u64;
            for vp_index in 0..vp_count {
                partition
                    .vp(vp_index)
                    .set_register(
                        whp::Register64::Tsc,
                        expected_tsc + u64::from(vp_index) * 100_000,
                    )
                    .unwrap();
            }

            synchronize_restored_tscs(&partition, vp_count as usize).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));

            for vp_index in 0..vp_count {
                let actual_tsc = partition
                    .vp(vp_index)
                    .get_register(whp::Register64::Tsc)
                    .unwrap();
                assert_eq!(actual_tsc, expected_tsc, "VP {vp_index} TSC differs");
            }
        }
    }

    #[test]
    #[ignore = "requires WHP"]
    fn restored_tsc_intercepts_are_reversible() {
        let mut config = whp::PartitionConfig::new().unwrap();
        config
            .set_property(whp::PartitionProperty::ProcessorCount(2))
            .unwrap();
        let partition = config.create().unwrap();
        for vp in 0..2 {
            partition.create_vp(vp).create().unwrap();
        }
        partition.suspend_time().unwrap();
        let original_exits = partition.extended_vm_exits().unwrap();
        let original_msrs = partition.x64_msr_exit_bitmap().unwrap();
        let frequency = partition.tsc_frequency().unwrap();
        let clock = RestoredTsc::enable(&partition, 1_000_000, frequency, 2, false, None).unwrap();
        assert!(
            partition
                .extended_vm_exits()
                .unwrap()
                .is_set(whp::abi::WHV_EXTENDED_VM_EXITS::X64RdtscExit)
        );
        assert!(
            partition
                .x64_msr_exit_bitmap()
                .unwrap()
                .is_set(whp::abi::WHV_X64_MSR_EXIT_BITMAP::TscMsrRead)
        );
        let clock =
            RestoredTsc::enable(&partition, 2_000_000, frequency, 2, false, Some(&clock)).unwrap();
        clock.disable(&partition).unwrap();
        assert_eq!(partition.extended_vm_exits().unwrap().0, original_exits.0);
        assert_eq!(partition.x64_msr_exit_bitmap().unwrap().0, original_msrs.0);
    }
}
