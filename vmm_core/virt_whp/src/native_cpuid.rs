// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! WHP-native CPUID leaves 0 and 1 for partitions with the Hyper-V guest
//! interface.
//!
//! Such a partition enables CPUID exits so that OpenVMM can apply the
//! partition's CPUID table. Without an exit list, WHP sends every CPUID to
//! OpenVMM, so each one leaves the hypervisor and its latency follows host
//! scheduling. That latency reaches guest code that runs CPUID with interrupts
//! disabled. The mu_msvm UEFI firmware's SynIC timer and SINT interrupt
//! handlers run CPUID leaves 0 and 1 five times per interrupt, because EDK2's
//! local APIC library probes for TDX and for the APIC base MSR on each EOI.
//! When host contention makes those exits outlast the firmware's 10 ms timer
//! period, the next tick is pending when the handler's `RestoreTPL()` enables
//! interrupts, so it nests on the same stack. The nesting repeats until the
//! firmware's DXE stack overflows.
//!
//! [`NativeCpuidLeaves::configure`] keeps these leaves in the hypervisor.
//! Before setup, it programs the partition-wide bits that the partition's
//! CPUID table sets in leaves 0 and 1 with `CpuidResultList2`, and WHP keeps
//! each VP's initial APIC ID. Every other leaf that the table sets, or that
//! the exit handler adjusts for each VP, stays in the CPUID exit list. After
//! the table is built, [`NativeCpuidLeaves::verify`] checks that the guest
//! sees the same results as through exits.

use crate::Error;
use crate::WhpResultExt;
use std::ops::RangeInclusive;
use virt::CpuidLeaf;
use virt::CpuidLeafSet;
use virt::x86::topology::per_vp_cpuid_bits;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::x86::ApicMode;
use whp::abi::WHV_CPUID_OUTPUT;
use whp::abi::WHV_X64_CPUID_RESULT2;
use whp::abi::WHV_X64_CPUID_RESULT2_FLAGS;
use x86defs::cpuid::CpuidFunction;
use x86defs::cpuid::VersionAndFeaturesEbx;
use x86defs::cpuid::VersionAndFeaturesEcx;

/// The leaves that WHP answers without an exit.
const NATIVE_LEAVES: [u32; 2] = [
    CpuidFunction::VendorAndMaxFunction.0,
    CpuidFunction::VersionAndFeatures.0,
];

/// The leaves outside the hypervisor range that still exit: the leaves that a
/// partition's CPUID table sets, other than leaves 0 and 1, and the leaves
/// that the exit handler adjusts for each VP.
const EXIT_LEAVES: [u32; 7] = [
    CpuidFunction::CacheParameters.0,
    CpuidFunction::ExtendedTopologyEnumeration.0,
    CpuidFunction::CoreCrystalClockInformation.0,
    CpuidFunction::V2ExtendedTopologyEnumeration.0,
    CpuidFunction::ExtendedAddressSpaceSizes.0,
    CpuidFunction::CacheTopologyDefinition.0,
    CpuidFunction::ProcessorTopologyDefinition.0,
];

/// The hypervisor leaves that still exit: the Hyper-V interface leaves, which
/// the CPUID table masks, and the virtualization stack's leaves, which the VMM
/// adds. Each exit-list entry adds about 25 µs to partition setup, so the rest
/// of the hypervisor range stays in the hypervisor, and
/// [`NativeCpuidLeaves::verify`] rejects a CPUID table that sets one of those
/// leaves.
const EXIT_HYPERVISOR_LEAVES: [RangeInclusive<u32>; 2] = [
    hvdef::HV_CPUID_FUNCTION_HV_VENDOR_AND_MAX_FUNCTION..=0x4000_000f,
    hvdef::VIRTUALIZATION_STACK_CPUID_VENDOR..=0x4000_008f,
];

/// A partition whose CPUID leaves 0 and 1 WHP answers without exits.
#[derive(Debug)]
pub(crate) struct NativeCpuidLeaves {
    /// The CPUID exit list, sorted.
    exits: Vec<u32>,
}

impl NativeCpuidLeaves {
    /// Programs `whp_config` so that WHP answers CPUID leaves 0 and 1 without
    /// exits, for a partition with the in-hypervisor APIC and enlightenments.
    ///
    /// Returns `None`, leaving every leaf to exit, when a VP's APIC ID differs
    /// from its index, the host's processor vendor is unknown, or WHP rejects
    /// the programming.
    pub(crate) fn configure(
        topology: &ProcessorTopology,
        whp_config: &mut whp::PartitionConfig,
    ) -> Option<Self> {
        // WHP's initial APIC ID is the VP index unless OpenVMM sets it, so
        // only then does WHP's leaf 1 report each VP's APIC ID.
        if topology
            .vps_arch()
            .any(|vp| vp.apic_id != vp.base.vp_index.index())
        {
            return None;
        }

        let results = native_results(topology, &host_cpuid)?;
        if let Err(err) =
            whp_config.set_property(whp::PartitionProperty::CpuidResultList2(&results))
        {
            tracing::info!(
                error = &err as &dyn std::error::Error,
                "WHP lacks CPUID results; CPUID leaves 0 and 1 exit"
            );
            return None;
        }

        // The results programmed above match the CPUID table, so leaves 0 and
        // 1 stay correct through exits if WHP rejects the exit list.
        let exits = exit_leaves();
        if let Err(err) = whp_config.set_property(whp::PartitionProperty::CpuidExitList(&exits)) {
            tracing::info!(
                error = &err as &dyn std::error::Error,
                "WHP rejects the CPUID exit list; CPUID leaves 0 and 1 exit"
            );
            return None;
        }

        Some(Self { exits })
    }

    /// Checks that every leaf of `cpuid`, the partition's CPUID table, reaches
    /// the guest: a leaf exits, or it is leaf 0 or 1 and `native`, WHP's
    /// result for VP 0, already matches the table outside the per-VP APIC
    /// identity bits.
    pub(crate) fn verify(
        &self,
        cpuid: &CpuidLeafSet,
        native: impl Fn(u32, u32) -> [u32; 4],
    ) -> Result<(), Error> {
        for leaf in cpuid.leaves() {
            if self.exits.binary_search(&leaf.function).is_ok() {
                continue;
            }
            if !NATIVE_LEAVES.contains(&leaf.function) {
                return Err(Error::NativeCpuidUnrouted(leaf.function));
            }
            let index = leaf.index.unwrap_or(0);
            let native = native(leaf.function, index);
            let expected = cpuid.result(leaf.function, index, &native);
            let per_vp = per_vp_cpuid_bits(leaf.function);
            if (0..4).any(|i| (native[i] ^ expected[i]) & !per_vp[i] != 0) {
                return Err(Error::NativeCpuidMismatch {
                    function: leaf.function,
                    native,
                    expected,
                });
            }
        }
        Ok(())
    }

    /// Checks that WHP reports `apic_id` as the initial APIC ID in VP
    /// `vp_index`'s leaf 1, which WHP answers without an exit.
    pub(crate) fn verify_apic_id(
        &self,
        vp: whp::Processor<'_>,
        vp_index: u32,
        apic_id: u32,
    ) -> Result<(), Error> {
        let output = vp
            .get_cpuid_output(CpuidFunction::VersionAndFeatures.0, 0)
            .for_op("get cpuid output")?;
        let native = VersionAndFeaturesEbx::from(output.Ebx).initial_apic_id();
        if u32::from(native) != apic_id & 0xff {
            return Err(Error::NativeCpuidApicId {
                vp_index,
                native: native.into(),
                apic_id,
            });
        }
        Ok(())
    }
}

/// Returns the leaf 1 result that reports `apic_mode`'s x2APIC support.
pub(crate) fn x2apic_leaf(apic_mode: ApicMode) -> CpuidLeaf {
    let mask = VersionAndFeaturesEcx::new().with_x2_apic(true).into();
    let value = match apic_mode {
        ApicMode::XApic => 0,
        ApicMode::X2ApicSupported | ApicMode::X2ApicEnabled => mask,
    };
    CpuidLeaf::new(CpuidFunction::VersionAndFeatures.0, [0, 0, value, 0]).masked([0, 0, mask, 0])
}

/// Returns the CPUID exit list, sorted.
fn exit_leaves() -> Vec<u32> {
    let mut exits: Vec<u32> = EXIT_LEAVES
        .into_iter()
        .chain(EXIT_HYPERVISOR_LEAVES.into_iter().flatten())
        .collect();
    exits.sort_unstable();
    exits.dedup();
    exits
}

/// Returns the `CpuidResultList2` results for leaves 0 and 1: the bits of
/// the partition's CPUID table that are partition-wide and known before
/// setup, where `host` reads the host's CPUID. Returns `None` if the host's
/// processor vendor is unknown.
///
/// Leaf 1 reports x2APIC support, the hypervisor, and the topology's
/// processors per package; WHP keeps each VP's initial APIC ID. Leaf 0
/// raises the maximum basic leaf to the TSC frequency leaf when the host's
/// is lower. WHP's maximum leaves are the host's, and [`NativeCpuidLeaves::verify`]
/// checks both results against the table after setup.
fn native_results(
    topology: &ProcessorTopology,
    host: &dyn Fn(u32, u32) -> [u32; 4],
) -> Option<Vec<WHV_X64_CPUID_RESULT2>> {
    let hypervisor = VersionAndFeaturesEcx::new()
        .with_hypervisor_present(true)
        .into();
    let mut leaves = vec![
        x2apic_leaf(topology.apic_mode()),
        CpuidLeaf::new(CpuidFunction::VersionAndFeatures.0, [0, 0, hypervisor, 0])
            .masked([0, 0, hypervisor, 0]),
    ];
    virt::x86::topology::topology_cpuid(topology, host, &mut leaves).ok()?;
    let cpuid = CpuidLeafSet::new(
        leaves
            .into_iter()
            .filter(|leaf| leaf.function == CpuidFunction::VersionAndFeatures.0)
            .collect(),
    );

    let mut results = Vec::new();
    let tsc_leaf = CpuidFunction::CoreCrystalClockInformation.0;
    if host(CpuidFunction::VendorAndMaxFunction.0, 0)[0] < tsc_leaf {
        results.push(result(
            CpuidFunction::VendorAndMaxFunction.0,
            [tsc_leaf, 0, 0, 0],
            [!0, 0, 0, 0],
        ));
    }
    for leaf in cpuid.leaves() {
        let per_vp = per_vp_cpuid_bits(leaf.function);
        let mask: [u32; 4] = std::array::from_fn(|i| leaf.mask[i] & !per_vp[i]);
        results.push(result(leaf.function, leaf.result, mask));
    }
    Some(results)
}

/// Returns a partition-wide `CpuidResultList2` result for `function` that
/// sets the bits of `value` in `mask`.
fn result(function: u32, value: [u32; 4], mask: [u32; 4]) -> WHV_X64_CPUID_RESULT2 {
    let output = |registers: [u32; 4]| WHV_CPUID_OUTPUT {
        Eax: registers[0],
        Ebx: registers[1],
        Ecx: registers[2],
        Edx: registers[3],
    };
    WHV_X64_CPUID_RESULT2 {
        Function: function,
        Index: 0,
        VpIndex: 0,
        Flags: WHV_X64_CPUID_RESULT2_FLAGS(0),
        Output: output(std::array::from_fn(|i| value[i] & mask[i])),
        Mask: output(mask),
    }
}

/// Returns the host's CPUID result.
fn host_cpuid(leaf: u32, subleaf: u32) -> [u32; 4] {
    // The CPUID instruction exists only on x86-64 hosts, the only hosts of
    // x86-64 WHP partitions.
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    {
        let result = core::arch::x86_64::__cpuid_count(leaf, subleaf);
        [result.eax, result.ebx, result.ecx, result.edx]
    }
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (leaf, subleaf);
        [0; 4]
    }
}

#[cfg(test)]
mod tests {
    use super::EXIT_HYPERVISOR_LEAVES;
    use super::NATIVE_LEAVES;
    use super::NativeCpuidLeaves;
    use super::exit_leaves;
    use super::native_results;
    use super::x2apic_leaf;
    use crate::Error;
    use virt::CpuidLeaf;
    use virt::CpuidLeafSet;
    use vm_topology::processor::ProcessorTopology;
    use vm_topology::processor::TopologyBuilder;
    use vm_topology::processor::x86::X2ApicState;
    use whp::abi::WHV_X64_CPUID_RESULT2;
    use x86defs::cpuid::VersionAndFeaturesEbx;
    use x86defs::cpuid::VersionAndFeaturesEcx;

    const INTEL: [u32; 3] = [0x756e_6547, 0x6c65_746e, 0x4965_6e69];
    const AMD: [u32; 3] = [0x6874_7541, 0x444d_4163, 0x6974_6e65];

    /// A Skylake-SP host: maximum basic leaf 0x16, GenuineIntel.
    fn skylake(leaf: u32, _subleaf: u32) -> [u32; 4] {
        match leaf {
            0 => [0x16, INTEL[0], INTEL[1], INTEL[2]],
            _ => [0; 4],
        }
    }

    /// A host whose maximum basic leaf precedes the TSC frequency leaf.
    fn haswell(leaf: u32, _subleaf: u32) -> [u32; 4] {
        match leaf {
            0 => [0xd, INTEL[0], INTEL[1], INTEL[2]],
            _ => [0; 4],
        }
    }

    /// An AMD host as an AMD EPYC 7763 reports it through WHP: maximum basic
    /// leaf 0xd, AuthenticAMD, and four caches in leaf 0x8000001D.
    fn milan(leaf: u32, subleaf: u32) -> [u32; 4] {
        match (leaf, subleaf) {
            (0, _) => [0xd, AMD[0], AMD[1], AMD[2]],
            (0x8000_001d, 0) => [0x121, 0x01c0_003f, 0x3f, 0],
            (0x8000_001d, 1) => [0x122, 0x01c0_003f, 0x3f, 0],
            (0x8000_001d, 2) => [0x143, 0x01c0_003f, 0x3ff, 2],
            (0x8000_001d, 3) => [0x163, 0x03c0_003f, 0x7fff, 1],
            _ => [0; 4],
        }
    }

    fn topology(vps: u32, x2apic: X2ApicState) -> ProcessorTopology {
        TopologyBuilder::new_x86()
            .vps_per_socket(vps)
            .x2apic(x2apic)
            .build(vps)
            .unwrap()
    }

    fn output(result: &WHV_X64_CPUID_RESULT2) -> ([u32; 4], [u32; 4]) {
        let registers = |o: whp::abi::WHV_CPUID_OUTPUT| [o.Eax, o.Ebx, o.Ecx, o.Edx];
        (registers(result.Output), registers(result.Mask))
    }

    /// The partition's CPUID table as `WhpPartitionInner` builds it on `host`
    /// from WHP's leaf 0, plus its hypervisor leaves.
    fn table(
        topology: &ProcessorTopology,
        host: &dyn Fn(u32, u32) -> [u32; 4],
        native_max: u32,
    ) -> CpuidLeafSet {
        let mut leaves = vec![
            x2apic_leaf(topology.apic_mode()),
            CpuidLeaf::new(1, [0, 0, 1 << 31, 0]).masked([0, 0, 1 << 31, 0]),
            crate::cpu_contract::mask_gpa_pinning_enlightenment(),
            CpuidLeaf::new(hvdef::VIRTUALIZATION_STACK_CPUID_VENDOR, [1, 2, 3, 4]),
            CpuidLeaf::new(hvdef::VIRTUALIZATION_STACK_CPUID_PROPERTIES, [1, 2, 3, 4]),
        ];
        virt::x86::topology::topology_cpuid(topology, host, &mut leaves).unwrap();
        leaves
            .extend(virt::x86::tsc::tsc_frequency_cpuid_leaves(2_200_000_000, native_max).unwrap());
        CpuidLeafSet::new(leaves)
    }

    /// WHP's result after the programmed results, for a host with
    /// `native_max` and a VP with APIC ID `apic_id`.
    fn native(
        results: &[WHV_X64_CPUID_RESULT2],
        native_max: u32,
        apic_id: u8,
    ) -> impl Fn(u32, u32) -> [u32; 4] {
        let results = results.to_vec();
        move |leaf, _subleaf| {
            let mut value = match leaf {
                0 => [native_max, INTEL[0], INTEL[1], INTEL[2]],
                1 => [
                    0x50654,
                    VersionAndFeaturesEbx::new()
                        .with_initial_apic_id(apic_id)
                        .with_clflush_line_size(8)
                        .into(),
                    VersionAndFeaturesEcx::new().with_sse3(true).into(),
                    0x0f8b_fbff,
                ],
                _ => [0; 4],
            };
            for result in results.iter().filter(|r| r.Function == leaf) {
                let (output, mask) = output(result);
                for i in 0..4 {
                    value[i] = value[i] & !mask[i] | output[i];
                }
            }
            value
        }
    }

    #[test]
    fn exit_list_routes_the_table_and_per_vp_leaves() {
        let exits = exit_leaves();
        assert!(exits.is_sorted());
        for leaf in NATIVE_LEAVES.into_iter().chain([0x4000_0010, 0x4000_00ff]) {
            assert!(exits.binary_search(&leaf).is_err(), "leaf {leaf:#x}");
        }
        for leaf in [0x4, 0xb, 0x15, 0x1f, 0x8000_0008, 0x8000_001d, 0x8000_001e]
            .into_iter()
            .chain(EXIT_HYPERVISOR_LEAVES.into_iter().flatten())
        {
            assert!(exits.binary_search(&leaf).is_ok(), "leaf {leaf:#x}");
        }
        assert_eq!(exits.len(), 39);
    }

    #[test]
    fn leaf_1_results_keep_the_per_vp_apic_id() {
        let topology = topology(2, X2ApicState::Supported);
        let results = native_results(&topology, &skylake).unwrap();
        let [leaf_1] = results.as_slice() else {
            panic!("expected only leaf 1: {results:?}");
        };
        let (value, mask) = output(leaf_1);
        assert_eq!(leaf_1.Function, 1);
        let lps = VersionAndFeaturesEbx::new().with_lps_per_package(0xff);
        assert_eq!(mask[1], u32::from(lps));
        assert_eq!(
            VersionAndFeaturesEbx::from(value[1]).lps_per_package(),
            topology.reserved_vps_per_socket() as u8
        );
        let ecx = VersionAndFeaturesEcx::from(value[2]);
        assert!(ecx.x2_apic() && ecx.hypervisor_present());
        assert_eq!(
            mask[2],
            u32::from(
                VersionAndFeaturesEcx::new()
                    .with_x2_apic(true)
                    .with_hypervisor_present(true)
            )
        );
        assert_eq!([mask[0], mask[3]], [0, 0]);
    }

    #[test]
    fn xapic_partitions_clear_the_x2apic_bit() {
        let results = native_results(&topology(2, X2ApicState::Unsupported), &skylake).unwrap();
        let (value, mask) = output(&results[0]);
        assert!(VersionAndFeaturesEcx::from(mask[2]).x2_apic());
        assert!(!VersionAndFeaturesEcx::from(value[2]).x2_apic());
    }

    #[test]
    fn leaf_0_reaches_the_tsc_leaf_only_when_the_host_stops_short() {
        let topology = topology(2, X2ApicState::Supported);
        assert!(
            native_results(&topology, &skylake)
                .unwrap()
                .iter()
                .all(|r| r.Function != 0)
        );
        let results = native_results(&topology, &haswell).unwrap();
        let leaf_0 = results.iter().find(|r| r.Function == 0).unwrap();
        assert_eq!(output(leaf_0), ([0x15, 0, 0, 0], [!0, 0, 0, 0]));
    }

    #[test]
    fn verify_accepts_the_programmed_partition() {
        for (host, native_max) in [
            (skylake as fn(u32, u32) -> [u32; 4], 0x16),
            (haswell, 0xd),
            (milan, 0xd),
        ] {
            let topology = topology(4, X2ApicState::Supported);
            let results = native_results(&topology, &host).unwrap();
            let native_leaves = NativeCpuidLeaves {
                exits: exit_leaves(),
            };
            let reported_max = native(&results, native_max, 0)(0, 0)[0];
            let cpuid = table(&topology, &host, reported_max);
            for apic_id in [0, 3] {
                native_leaves
                    .verify(&cpuid, native(&results, native_max, apic_id))
                    .unwrap();
            }
        }
    }

    #[test]
    fn verify_rejects_a_table_leaf_that_does_not_exit() {
        let topology = topology(2, X2ApicState::Supported);
        let native_leaves = NativeCpuidLeaves {
            exits: exit_leaves(),
        };
        let results = native_results(&topology, &skylake).unwrap();
        for function in [7, 0x4000_0010] {
            let cpuid = CpuidLeafSet::new(vec![
                CpuidLeaf::new(function, [0, 1, 0, 0]).masked([0, 1, 0, 0]),
            ]);
            assert!(matches!(
                native_leaves.verify(&cpuid, native(&results, 0x16, 0)),
                Err(Error::NativeCpuidUnrouted(f)) if f == function
            ));
        }
    }

    #[test]
    fn verify_rejects_a_leaf_1_bit_that_whp_does_not_report() {
        let topology = topology(2, X2ApicState::Supported);
        let native_leaves = NativeCpuidLeaves {
            exits: exit_leaves(),
        };
        let cpuid = table(&topology, &skylake, 0x16);
        // WHP without the programmed results reports no processors per
        // package.
        assert!(matches!(
            native_leaves.verify(&cpuid, native(&[], 0x16, 0)),
            Err(Error::NativeCpuidMismatch { function: 1, .. })
        ));
    }

    #[test]
    fn verify_rejects_a_maximum_basic_leaf_below_the_tsc_leaf() {
        // The host reports leaf 0x16, so leaf 0 is not programmed, but WHP
        // reports a lower maximum basic leaf.
        let topology = topology(2, X2ApicState::Supported);
        let native_leaves = NativeCpuidLeaves {
            exits: exit_leaves(),
        };
        let results = native_results(&topology, &skylake).unwrap();
        let cpuid = table(&topology, &skylake, 0xd);
        assert!(matches!(
            native_leaves.verify(&cpuid, native(&results, 0xd, 0)),
            Err(Error::NativeCpuidMismatch { function: 0, .. })
        ));
    }

    #[test]
    fn unknown_vendors_keep_every_leaf_exiting() {
        let unknown = |leaf: u32, _subleaf: u32| match leaf {
            0 => [0x16, 1, 2, 3],
            _ => [0; 4],
        };
        assert!(native_results(&topology(2, X2ApicState::Supported), &unknown).is_none());
    }

    /// Programs a two-VP partition and checks what
    /// `WHvGetVirtualProcessorCpuidOutput` returns for leaves 0 and 1.
    #[test]
    #[ignore = "requires WHP"]
    fn whp_answers_the_programmed_leaves() {
        let topology = topology(2, X2ApicState::Supported);
        let mut config = whp::PartitionConfig::new().unwrap();
        config
            .set_property(whp::PartitionProperty::ProcessorCount(2))
            .unwrap();
        config
            .set_property(whp::PartitionProperty::LocalApicEmulationMode(
                whp::abi::WHvX64LocalApicEmulationModeX2Apic,
            ))
            .unwrap();
        config
            .set_property(whp::PartitionProperty::ExtendedVmExits(
                whp::abi::WHV_EXTENDED_VM_EXITS::X64CpuidExit,
            ))
            .unwrap();
        let native_leaves = NativeCpuidLeaves::configure(&topology, &mut config)
            .expect("WHP accepts the CPUID results and the exit list");
        let partition = config.create().unwrap();
        for vp in 0..2 {
            partition.create_vp(vp).create().unwrap();
        }
        for vp in 0..2 {
            let output = partition.vp(vp).get_cpuid_output(1, 0).unwrap();
            let ebx = VersionAndFeaturesEbx::from(output.Ebx);
            let ecx = VersionAndFeaturesEcx::from(output.Ecx);
            assert_eq!(ebx.initial_apic_id(), vp as u8);
            assert_eq!(
                ebx.lps_per_package(),
                topology.reserved_vps_per_socket() as u8
            );
            assert!(ecx.x2_apic() && ecx.hypervisor_present());
            native_leaves
                .verify_apic_id(partition.vp(vp), vp, vp)
                .unwrap();
        }
        let max = partition.vp(0).get_cpuid_output(0, 0).unwrap().Eax;
        assert!(max >= 0x15, "maximum basic leaf {max:#x}");
    }
}
