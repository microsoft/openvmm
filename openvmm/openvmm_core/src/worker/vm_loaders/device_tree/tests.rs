// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use fdt::parser::Node;
use fdt::parser::Parser;
use serial_16550_resources::ComPort;
use test_with_tracing::test;
use vm_topology::processor::TopologyBuilder;
use vm_topology::processor::aarch64::Aarch64PlatformConfig;
use vm_topology::processor::aarch64::GicItsInfo;
use vm_topology::processor::aarch64::GicV2mInfo;

const RAM: &[MemoryRangeWithNode] = &[MemoryRangeWithNode {
    range: MemoryRange::new(0..0x4000_0000),
    vnode: 3,
}];
const COMS: &[UartId] = &[
    UartId::Com(ComPort::Com1),
    UartId::Com(ComPort::Com2),
    UartId::Com(ComPort::Com3),
    UartId::Com(ComPort::Com4),
];
const PL011S: &[UartId] = &[UartId::Pl0110, UartId::Pl0111];

fn mmio() -> ChipsetMmioRanges {
    ChipsetMmioRanges {
        low: MemoryRange::new(0xe000_0000..0xf000_0000),
        high: MemoryRange::new(0x10_0000_0000..0x11_0000_0000),
        vtl2: MemoryRange::new(0xd000_0000..0xe000_0000),
    }
}

fn x86(topology: &ProcessorTopology<X86Topology>) -> DeviceTreeBuilder<'_, X86Topology> {
    DeviceTreeBuilder::new(topology, RAM, COMS, &[])
        .with_igvm(mmio(), Vtl2BaseAddressType::File)
        .with_capacity(0x10000)
}

fn arm_topology(msi: GicMsiController, v2: bool) -> ProcessorTopology<Aarch64Topology> {
    TopologyBuilder::new_aarch64(Aarch64PlatformConfig {
        gic_distributor_base: 0xffff0000,
        gic_version: if v2 {
            GicVersion::V2 {
                cpu_interface_base: 0xfff00000,
            }
        } else {
            GicVersion::V3 {
                redistributors_base: 0xeff00000,
            }
        },
        gic_msi: msi,
        pmu_gsiv: Some(23),
        virt_timer_ppi: 20,
        gic_nr_irqs: 256,
    })
    .build(if v2 { 2 } else { 17 })
    .unwrap()
}

fn arm(topology: &ProcessorTopology<Aarch64Topology>) -> DeviceTreeBuilder<'_, Aarch64Topology> {
    DeviceTreeBuilder::new(topology, RAM, PL011S, &[])
        .with_linux_direct(mmio().low, mmio().high)
        .with_capacity(0x200000)
}

fn node<'a>(dt: &'a [u8], path: &str) -> Node<'a> {
    let mut node = Parser::new(dt).unwrap().root().unwrap();
    for component in path.split('/').filter(|part| !part.is_empty()) {
        node = node
            .children()
            .map(Result::unwrap)
            .find(|child| child.name == component)
            .unwrap();
    }
    node
}

fn u32_property(node: &Node<'_>, name: &str) -> Vec<u32> {
    let property = node.find_property(name).unwrap().unwrap();
    (0..property.data.len() / 4)
        .map(|i| property.read_u32(i).unwrap())
        .collect()
}

fn u64_property(node: &Node<'_>, name: &str) -> Vec<u64> {
    let property = node.find_property(name).unwrap().unwrap();
    (0..property.data.len() / 8)
        .map(|i| property.read_u64(i).unwrap())
        .collect()
}

fn string_property<'a>(node: &Node<'a>, name: &str) -> &'a str {
    node.find_property(name)
        .unwrap()
        .unwrap()
        .read_str()
        .unwrap()
}

#[test]
fn igvm_setter_order_and_reset() {
    let topology = TopologyBuilder::new_x86().build(2).unwrap();
    let protected = &[MemoryRange::new(0x2000..0x4000)];
    let first = x86(&topology)
        .with_command_line("test")
        .with_console(Some(UartId::Com(ComPort::Com3)))
        .with_igvm_protectable_ram(protected)
        .with_igvm_entropy(Some(&[4, 5]))
        .with_igvm_vmbus_redirect(true)
        .finish()
        .unwrap();
    let reordered = DeviceTreeBuilder::new(&topology, RAM, COMS, &[])
        .with_igvm_entropy(Some(&[4, 5]))
        .with_igvm_vmbus_redirect(true)
        .with_capacity(0x10000)
        .with_igvm_protectable_ram(protected)
        .with_console(Some(UartId::Com(ComPort::Com3)))
        .with_command_line("test")
        .with_igvm(mmio(), Vtl2BaseAddressType::File)
        .finish()
        .unwrap();
    assert_eq!(first, reordered);
    let reset = x86(&topology)
        .with_igvm(
            mmio(),
            Vtl2BaseAddressType::Vtl2Allocate { size: Some(0x4000) },
        )
        .with_command_line("old")
        .with_command_line("")
        .with_console(Some(UartId::Com(ComPort::Com3)))
        .with_console(None)
        .with_igvm_protectable_ram(protected)
        .with_igvm_protectable_ram(&[])
        .with_igvm_entropy(Some(&[1]))
        .with_igvm_entropy(None)
        .with_igvm_vmbus_redirect(true)
        .with_igvm_vmbus_redirect(false)
        .with_capacity(0)
        .with_capacity(0x10000)
        .with_igvm(mmio(), Vtl2BaseAddressType::File)
        .finish()
        .unwrap();
    assert_eq!(reset, x86(&topology).finish().unwrap());
    assert_eq!(
        u32_property(
            &node(&first, "/bus/vmbus-vtl2@d0000000"),
            "microsoft,message-connection-id"
        ),
        [0x800074]
    );
}

#[test]
fn linux_setter_order_and_initrd_reset() {
    let topology = arm_topology(GicMsiController::None, false);
    let first = arm(&topology)
        .with_linux_initrd(Some((100, 200)))
        .with_console(Some(UartId::Pl0111))
        .with_command_line("test")
        .finish()
        .unwrap();
    let reordered = DeviceTreeBuilder::new(&topology, RAM, PL011S, &[])
        .with_console(Some(UartId::Pl0111))
        .with_linux_initrd(Some((100, 200)))
        .with_command_line("test")
        .with_capacity(0x200000)
        .with_linux_direct(mmio().low, mmio().high)
        .finish()
        .unwrap();
    assert_eq!(first, reordered);
    let reset = arm(&topology)
        .with_linux_direct(MemoryRange::EMPTY, MemoryRange::EMPTY)
        .with_linux_direct(mmio().low, mmio().high)
        .with_linux_initrd(Some((200, 100)))
        .with_linux_initrd(None)
        .finish()
        .unwrap();
    assert_eq!(reset, arm(&topology).finish().unwrap());
    assert!(
        node(&reset, "/chosen")
            .find_property("linux,initrd-start")
            .unwrap()
            .is_none()
    );
    assert!(
        node(&reset, "/chosen")
            .find_property("linux,initrd-end")
            .unwrap()
            .is_none()
    );
    let equal = arm(&topology)
        .with_linux_initrd(Some((100, 100)))
        .finish()
        .unwrap();
    for name in ["linux,initrd-start", "linux,initrd-end"] {
        assert_eq!(u64_property(&node(&equal, "/chosen"), name), [100]);
    }
    assert!(matches!(
        arm(&topology).with_linux_initrd(Some((200, 100))).finish(),
        Err(DeviceTreeError::InitrdRange)
    ));
}

#[test]
fn capacity_is_required_and_exact() {
    let topology = TopologyBuilder::new_x86().build(2).unwrap();
    assert!(matches!(
        DeviceTreeBuilder::new(&topology, RAM, &[], &[])
            .with_igvm(mmio(), Vtl2BaseAddressType::File)
            .finish(),
        Err(DeviceTreeError::MissingCapacity)
    ));
    assert!(matches!(
        DeviceTreeBuilder::new(&topology, RAM, &[], &[])
            .with_capacity(0x10000)
            .finish(),
        Err(DeviceTreeError::MissingBootSettings)
    ));
    for capacity in [0, 1, 55, 56, 1079] {
        assert!(matches!(
            x86(&topology).with_capacity(capacity).finish(),
            Err(DeviceTreeError::Capacity)
        ));
    }
    let dt = x86(&topology).finish().unwrap();
    assert_eq!(x86(&topology).with_capacity(dt.len()).finish().unwrap(), dt);
    assert!(matches!(
        x86(&topology).with_capacity(dt.len() - 1).finish(),
        Err(DeviceTreeError::Fdt(fdt::builder::Error::OutOfSpace))
    ));
    let topology = arm_topology(GicMsiController::None, false);
    assert!(matches!(
        DeviceTreeBuilder::new(&topology, RAM, &[], &[])
            .with_capacity(0x10000)
            .finish(),
        Err(DeviceTreeError::MissingBootSettings)
    ));
    let dt = arm(&topology).finish().unwrap();
    assert_eq!(arm(&topology).with_capacity(dt.len()).finish().unwrap(), dt);
    assert!(arm(&topology).with_capacity(dt.len() - 1).finish().is_err());
}

#[test]
fn uart_inventory_and_selected_parent_paths() {
    let topology = TopologyBuilder::new_x86().build(2).unwrap();
    let without_console = x86(&topology).finish().unwrap();
    assert_eq!(node(&without_console, "/pio-bus").children().count(), 4);
    assert!(
        node(&without_console, "/chosen")
            .find_property("stdout-path")
            .unwrap()
            .is_none()
    );
    for uart in COMS {
        let dt = x86(&topology).with_console(Some(*uart)).finish().unwrap();
        let chosen = node(&dt, "/chosen");
        let path = string_property(&chosen, "stdout-path");
        assert!(path.starts_with("/pio-bus/serial@"));
        let serial = node(&dt, path);
        let (base, length, irq) = uart_resources(*uart);
        assert_eq!(u64_property(&serial, "reg"), [base, length]);
        assert_eq!(u64_property(&serial, "interrupts"), [u64::from(irq)]);
    }
    assert!(matches!(
        DeviceTreeBuilder::new(&topology, RAM, &[UartId::Com(ComPort::Com1)], &[])
            .with_igvm(mmio(), Vtl2BaseAddressType::File)
            .with_capacity(0x10000)
            .with_console(Some(UartId::Com(ComPort::Com3)))
            .finish(),
        Err(DeviceTreeError::ConsoleNotAttached)
    ));
    let topology = arm_topology(GicMsiController::None, false);
    for uart in PL011S {
        let dt = arm(&topology).with_console(Some(*uart)).finish().unwrap();
        let path = string_property(&node(&dt, "/chosen"), "stdout-path");
        assert!(path.starts_with("/openvmm/uart@"));
        let serial = node(&dt, path);
        let (base, length, spi) = uart_resources(*uart);
        assert_eq!(u64_property(&serial, "reg"), [base, length]);
        assert_eq!(
            u32_property(&serial, "interrupts"),
            [GIC_SPI, spi, IRQ_TYPE_LEVEL_HIGH]
        );
        assert_eq!(u32_property(&serial, "interrupt-parent"), [PHANDLE_GIC]);
    }
}

#[test]
fn invalid_uart_and_command_line() {
    let topology = TopologyBuilder::new_x86().build(2).unwrap();
    for (uarts, duplicate) in [
        (
            &[UartId::Com(ComPort::Com1), UartId::Com(ComPort::Com1)][..],
            true,
        ),
        (&[UartId::Pl0110][..], false),
    ] {
        let result = DeviceTreeBuilder::new(&topology, RAM, uarts, &[])
            .with_igvm(mmio(), Vtl2BaseAddressType::File)
            .with_capacity(0x10000)
            .finish();
        if duplicate {
            assert!(matches!(result, Err(DeviceTreeError::DuplicateUart)));
        } else {
            assert!(matches!(result, Err(DeviceTreeError::UartArchitecture)));
        }
    }
    assert!(matches!(
        x86(&topology).with_command_line("a\0b").finish(),
        Err(DeviceTreeError::CommandLine)
    ));
}

#[test]
fn memory_visibility_and_architecture_bindings() {
    let x86_topology = TopologyBuilder::new_x86().build(2).unwrap();
    let protected = [MemoryRange::new(0x2000..0x4000)];
    let dt = x86(&x86_topology)
        .with_igvm_protectable_ram(&protected)
        .finish()
        .unwrap();
    let cpus = node(&dt, "/cpus");
    for (cpu, vp) in cpus
        .children()
        .map(Result::unwrap)
        .zip(x86_topology.vps_arch())
    {
        assert_eq!(u32_property(&cpu, "reg"), [vp.apic_id]);
        assert_eq!(u32_property(&cpu, "numa-node-id"), [vp.base.vnode]);
    }
    let memories: Vec<_> = node(&dt, "/")
        .children()
        .map(Result::unwrap)
        .filter(|node| node.name.starts_with("memory@"))
        .collect();
    assert_eq!(
        memories.iter().map(|n| n.name).collect::<Vec<_>>(),
        ["memory@4000", "memory@2000", "memory@0"]
    );
    for memory in &memories {
        assert_eq!(u32_property(memory, "numa-node-id"), [3]);
    }
    assert_eq!(
        u32_property(&memories[1], igvm_defs::dt::IGVM_DT_IGVM_TYPE_PROPERTY),
        [igvm_defs::MemoryMapEntryType::VTL2_PROTECTABLE.0 as u32]
    );
    assert!(
        x86(&x86_topology)
            .with_igvm_protectable_ram(&[MemoryRange::new(0x4000_0000..0x4000_1000)])
            .finish()
            .is_err()
    );

    let topology = arm_topology(GicMsiController::None, false);
    // Only the RAM passed by the caller is visible on native Linux.
    let visible = [MemoryRangeWithNode {
        range: MemoryRange::new(0x1000..0x2000),
        vnode: 9,
    }];
    let dt = DeviceTreeBuilder::new(&topology, &visible, &[], &[])
        .with_linux_direct(mmio().low, mmio().high)
        .with_capacity(0x10000)
        .finish()
        .unwrap();
    let memory = node(&dt, "/memory@1000");
    assert_eq!(u64_property(&memory, "reg"), [0x1000, 0x1000]);
    assert!(memory.find_property("numa-node-id").unwrap().is_none());
    assert!(
        memory
            .find_property(igvm_defs::dt::IGVM_DT_IGVM_TYPE_PROPERTY)
            .unwrap()
            .is_none()
    );
    assert_eq!(u32_property(&node(&dt, "/cpus/cpu@16"), "reg"), [16]);
    assert_ne!(u64::from(topology.vps_arch().nth(16).unwrap().mpidr), 16);
    assert_eq!(string_property(&node(&dt, "/cpus/cpu@0"), "status"), "okay");
    assert_eq!(
        string_property(&node(&dt, "/cpus/cpu@16"), "status"),
        "disabled"
    );
    assert_eq!(u32_property(&node(&dt, "/timer"), "interrupts"), [1, 4, 8]);
    assert_eq!(u32_property(&node(&dt, "/pmu"), "interrupts"), [1, 7, 4]);
    assert_eq!(
        u32_property(&node(&dt, "/openvmm/vmbus"), "interrupts"),
        [1, openvmm_defs::config::DEFAULT_VMBUS_PPI - 16, 1]
    );
}

fn bridge() -> PcieHostBridge {
    PcieHostBridge {
        index: 4,
        segment: 7,
        start_bus: 32,
        end_bus: 47,
        ecam_range: MemoryRange::new(0x8000_0000..0x8100_0000),
        low_mmio: MemoryRange::new(0x9000_0000..0xa000_0000),
        high_mmio: MemoryRange::new(0x12_0000_0000..0x14_0000_0000),
        cxl: None,
        vnode: Some(3),
        preserve_bars: false,
        preserve_boot_config: true,
    }
}

#[test]
fn gic_msi_smmu_and_pcie_bindings() {
    let bridges = [bridge()];
    let smmus = [AcpiSmmuConfig {
        rc_index: 4,
        segment: 7,
        base: 0x7000_0000,
        event_gsiv: 35,
        gerr_gsiv: 36,
        reserved_iova_ranges: vec![],
    }];
    for (msi, v2, child, phandle) in [
        (GicMsiController::None, false, None, None),
        (
            GicMsiController::Its(GicItsInfo {
                its_base: 0x6000_0000,
            }),
            false,
            Some("its@60000000"),
            Some(PHANDLE_ITS),
        ),
        (
            GicMsiController::V2m(GicV2mInfo {
                frame_base: 0x6000_0000,
                spi_base: 64,
                spi_count: 64,
            }),
            true,
            Some("v2m@60000000"),
            Some(PHANDLE_V2M),
        ),
    ] {
        let topology = arm_topology(msi, v2);
        let build = || {
            DeviceTreeBuilder::new(&topology, RAM, &[], &bridges)
                .with_linux_direct(mmio().low, mmio().high)
                .with_capacity(0x10000)
        };
        let dt = build().with_linux_smmus(&smmus).finish().unwrap();
        let pcie = node(&dt, "/pcie@80000000");
        assert_eq!(u32_property(&pcie, "bus-range"), [32, 47]);
        assert_eq!(u32_property(&pcie, "linux,pci-domain"), [7]);
        assert_eq!(
            u32_property(&pcie, "ranges"),
            super::super::pcie::identity_ranges(&bridges[0])
        );
        assert_eq!(u64_property(&pcie, "reg"), [0x8000_0000, 0x100_0000]);
        assert_eq!(u32_property(&pcie, "linux,pci-probe-only"), [1]);
        assert_eq!(
            u32_property(&pcie, "iommu-map"),
            [0, PHANDLE_SMMU_BASE, 0, 0x10000]
        );
        assert!(pcie.find_property("numa-node-id").unwrap().is_none());
        let gic = node(&dt, "/intc@ffff0000");
        assert_eq!(u64_property(&gic, "reg"), gic_registers(&topology));
        if let Some(child) = child {
            let controller = node(&dt, &format!("/intc@ffff0000/{child}"));
            assert_eq!(u32_property(&controller, "phandle"), [phandle.unwrap()]);
            assert_eq!(u32_property(&pcie, "msi-parent"), [phandle.unwrap()]);
        } else {
            assert!(pcie.find_property("msi-parent").unwrap().is_none());
        }
        assert_eq!(
            u32_property(&node(&dt, "/smmu@70000000"), "interrupts"),
            [0, 3, 4, 0, 4, 4]
        );
        let cleared = build()
            .with_linux_smmus(&smmus)
            .with_linux_smmus(&[])
            .finish()
            .unwrap();
        assert_eq!(cleared, build().finish().unwrap());
        assert!(
            node(&cleared, "/pcie@80000000")
                .find_property("iommu-map")
                .unwrap()
                .is_none()
        );
        assert!(
            !node(&cleared, "/")
                .children()
                .any(|n| n.unwrap().name.starts_with("smmu@"))
        );
    }
}
