// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM MADT tests: SMP APIC IDs match the shared MP table, and
//! level-triggered legacy IRQs get interrupt source overrides.

use super::*;
use vm_topology::processor::x86::X2ApicState;

#[test]
fn test_microvm_smp_madt_matches_shared_mp_table() {
    for processor_count in [1, 2, 4, 8] {
        let mut topology_builder = TopologyBuilder::new_x86();
        topology_builder
            .vps_per_socket(processor_count)
            .smt_enabled(false)
            .x2apic(X2ApicState::Unsupported);
        let topology = topology_builder.build(processor_count).unwrap();
        let apic_ids = topology.vps_arch().map(|vp| vp.apic_id).collect::<Vec<_>>();

        let mem = new_mem();
        let pcie = vec![];
        let madt = new_builder(&mem, &topology, &pcie).build_madt();
        let madt_ids = MadtParser::new(&madt)
            .unwrap()
            .parse_apic_ids()
            .unwrap()
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();

        let mp_table = loader::mptable::build_config_table(&loader::mptable::MpTableConfig {
            apic_ids: &apic_ids,
            level_triggered_irqs: &[],
        })
        .unwrap();
        let mp_ids = mp_table[44..44 + apic_ids.len() * 20]
            .chunks_exact(20)
            .map(|entry| {
                assert_eq!(entry[0], 0);
                u32::from(entry[1])
            })
            .collect::<Vec<_>>();

        assert_eq!(mp_ids, apic_ids);
        assert_eq!(madt_ids, apic_ids);
    }
}

#[test]
fn test_madt_level_triggered_irq_override() {
    let mem = new_mem();
    let topology = TopologyBuilder::new_x86().build(1).unwrap();
    let pcie = vec![];
    let mut builder = new_builder(&mem, &topology, &pcie);
    let AcpiArchConfig::X86 {
        level_triggered_irqs,
        ..
    } = &mut builder.arch
    else {
        unreachable!()
    };
    *level_triggered_irqs = &[5];

    let madt = builder.build_madt();
    let expected = acpi_spec::madt::MadtInterruptSourceOverride::new(
        5,
        5,
        Some(InterruptPolarity::ActiveHigh),
        Some(InterruptTriggerMode::Level),
    );
    assert!(
        madt.windows(expected.as_bytes().len())
            .any(|bytes| bytes == expected.as_bytes())
    );
}
