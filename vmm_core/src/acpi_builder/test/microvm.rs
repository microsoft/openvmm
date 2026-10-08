// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM MADT tests: SMP APIC IDs match the processor topology, and
//! level-triggered legacy IRQs get interrupt source overrides.

use super::*;
use vm_topology::processor::x86::X2ApicState;

#[test]
fn test_microvm_smp_madt_matches_processor_topology() {
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

#[test]
#[should_panic(expected = "legacy IRQ should be in range")]
fn test_madt_rejects_non_legacy_irq_override() {
    build_madt_with_level_triggered_irqs(&[16], false);
}

#[test]
#[should_panic(expected = "level-triggered IRQ should be unique")]
fn test_madt_rejects_duplicate_irq_override() {
    build_madt_with_level_triggered_irqs(&[5, 5], false);
}

#[test]
#[should_panic(expected = "level-triggered IRQ duplicates ACPI IRQ")]
fn test_madt_rejects_duplicate_acpi_irq_override() {
    build_madt_with_level_triggered_irqs(&[2], false);
}

#[test]
#[should_panic(expected = "level-triggered IRQ conflicts with PIT override")]
fn test_madt_rejects_conflicting_pit_irq_override() {
    build_madt_with_level_triggered_irqs(&[0], true);
}

fn build_madt_with_level_triggered_irqs(
    level_triggered_irqs: &'static [u32],
    enable_pit: bool,
) {
    let mem = new_mem();
    let topology = TopologyBuilder::new_x86().build(1).unwrap();
    let pcie = vec![];
    let mut builder = new_builder(&mem, &topology, &pcie);
    let AcpiArchConfig::X86 {
        level_triggered_irqs: configured_irqs,
        with_pit,
        ..
    } = &mut builder.arch
    else {
        unreachable!()
    };
    *configured_irqs = level_triggered_irqs;
    *with_pit = enable_pit;
    builder.build_madt();
}
