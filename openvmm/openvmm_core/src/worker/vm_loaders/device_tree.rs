// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Device trees for guest loaders, such as OpenHCL (IGVM) and Linux direct
//! boot.

use super::super::memory_layout::ChipsetMmioRanges;
use fdt::builder::Builder;
use fdt::builder::Nest;
use fdt::builder::StringId;
use memory_range::MemoryRange;
use openvmm_defs::config::Vtl2BaseAddressType;
use serial_uart_resources::UartId;
use thiserror::Error;
use vm_topology::memory::MemoryRangeWithNode;
use vm_topology::pcie::PcieHostBridge;
use vm_topology::processor::ArchTopology;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::aarch64::Aarch64Topology;
use vm_topology::processor::aarch64::GicMsiController;
use vm_topology::processor::aarch64::GicVersion;
use vm_topology::processor::x86::X86Topology;
use vmm_core::acpi_builder::AcpiSmmuConfig;

const STRING_TABLE_CAP: usize = 1024;

// Phandles that this writer assigns. They only need to be unique in the tree.
const PHANDLE_GIC: u32 = 1;
const PHANDLE_APB_PCLK: u32 = 2;
const PHANDLE_V2M: u32 = 3;
const PHANDLE_ITS: u32 = 4;
// SMMU instance N uses phandle PHANDLE_SMMU_BASE + N.
const PHANDLE_SMMU_BASE: u32 = 5;

// Interrupt-specifier cells from the ARM GIC binding and
// dt-bindings/interrupt-controller/irq.h.
const GIC_SPI: u32 = 0;
const GIC_PPI: u32 = 1;
const IRQ_TYPE_LEVEL_HIGH: u32 = 4;

#[derive(Debug, Error)]
pub enum DeviceTreeError {
    #[error("device tree capacity is too small or exceeds the FDT size limit")]
    Capacity,
    #[error("device tree encoding failed")]
    Fdt(#[from] fdt::builder::Error),
    #[error("console UART is not described in the device tree")]
    ConsoleNotInDeviceTree,
    #[error("duplicate UART in the device-tree UART list")]
    DuplicateUart,
    #[error("UART is not supported by this architecture")]
    UartArchitecture,
    #[error("command line contains an embedded NUL")]
    CommandLine,
    #[error("initrd end precedes its start")]
    InitrdRange,
    #[error("invalid ARM interrupt configuration")]
    Interrupt,
    #[error("GIC register ranges overflow or overlap")]
    GicRange,
    #[error("invalid SMMU configuration")]
    Smmu,
    #[error("invalid partition memory classification")]
    Memory,
    #[error("processor topology is empty or too large")]
    Processors,
}

/// Builds a device tree for IGVM or Linux direct boot.
///
/// `T` selects the architecture-specific content, and [`DeviceTreeBootType`]
/// selects the boot-specific content.
pub(crate) struct DeviceTreeBuilder<'a, T: DeviceTreeArch> {
    topology: &'a ProcessorTopology<T>,
    ram: &'a [MemoryRangeWithNode],
    dt_uarts: &'a [UartId],
    pcie: &'a [PcieHostBridge],
    capacity: usize,
    boot: DeviceTreeBootType<'a>,
    command_line: &'a str,
    console: Option<UartId>,
}

/// The boot path that receives the device tree.
pub(crate) enum DeviceTreeBootType<'a> {
    Igvm(IgvmBoot<'a>),
    LinuxDirect(LinuxDirectBoot<'a>),
}

/// Settings for the device tree that an IGVM launch uses.
pub(crate) struct IgvmBoot<'a> {
    pub chipset_mmio: ChipsetMmioRanges,
    pub vtl2_base_address: Vtl2BaseAddressType,
    pub protectable_ram: &'a [MemoryRange],
    pub vmbus_redirect: bool,
    pub entropy: Option<&'a [u8]>,
}

/// Settings for the device tree that a Linux kernel receives.
pub(crate) struct LinuxDirectBoot<'a> {
    pub low_mmio: MemoryRange,
    pub high_mmio: MemoryRange,
    pub initrd: Option<(u64, u64)>,
    pub smmus: &'a [AcpiSmmuConfig],
}

impl<'a, T: DeviceTreeArch> DeviceTreeBuilder<'a, T> {
    pub(crate) fn new(
        topology: &'a ProcessorTopology<T>,
        ram: &'a [MemoryRangeWithNode],
        dt_uarts: &'a [UartId],
        pcie: &'a [PcieHostBridge],
        capacity: usize,
        boot: DeviceTreeBootType<'a>,
    ) -> Self {
        Self {
            topology,
            ram,
            dt_uarts,
            pcie,
            capacity,
            boot,
            command_line: "",
            console: None,
        }
    }

    pub(crate) fn with_command_line(mut self, command_line: &'a str) -> Self {
        self.command_line = command_line;
        self
    }

    pub(crate) fn with_console(mut self, console: Option<UartId>) -> Self {
        self.console = console;
        self
    }

    fn validate(&self) -> Result<(), DeviceTreeError> {
        // Builder::new does not check the backing buffer before add_string
        // indexes its reserved string region.
        // The version 17 FDT header has ten 32-bit fields.
        let minimum = 40 + size_of::<fdt::ReserveEntry>() + STRING_TABLE_CAP;
        if self.capacity < minimum || self.capacity > u32::MAX as usize {
            return Err(DeviceTreeError::Capacity);
        }

        let processor_count = self.topology.vps().len();
        if processor_count == 0 || processor_count > u32::MAX as usize {
            return Err(DeviceTreeError::Processors);
        }

        if self.command_line.contains('\0') {
            return Err(DeviceTreeError::CommandLine);
        }

        if self
            .console
            .is_some_and(|uart| !self.dt_uarts.contains(&uart))
        {
            return Err(DeviceTreeError::ConsoleNotInDeviceTree);
        }
        for (index, uart) in self.dt_uarts.iter().enumerate() {
            if self.dt_uarts[..index].contains(uart) {
                return Err(DeviceTreeError::DuplicateUart);
            }
            if uart_is_mmio(*uart) != T::MMIO_UARTS {
                return Err(DeviceTreeError::UartArchitecture);
            }
        }

        if let DeviceTreeBootType::LinuxDirect(linux) = &self.boot {
            if linux.initrd.is_some_and(|(start, end)| start > end) {
                return Err(DeviceTreeError::InitrdRange);
            }
        }
        T::validate(self.topology, self.smmus(), self.pcie)
    }

    fn smmus(&self) -> &'a [AcpiSmmuConfig] {
        match &self.boot {
            DeviceTreeBootType::Igvm(_) => &[],
            DeviceTreeBootType::LinuxDirect(linux) => linux.smmus,
        }
    }

    pub(crate) fn build(self) -> Result<Vec<u8>, DeviceTreeError> {
        self.validate()?;
        let mut buffer = vec![0; self.capacity];
        let mut fdt_builder = Builder::new(fdt::builder::BuilderConfig {
            blob_buffer: &mut buffer,
            string_table_cap: STRING_TABLE_CAP,
            memory_reservations: &[],
        })?;

        let p = Properties::new(&mut fdt_builder)?;
        let mut root = fdt_builder
            .start_node("")?
            .add_u32(p.address_cells, 2)?
            .add_u32(p.size_cells, 2)?;
        root = match &self.boot {
            DeviceTreeBootType::Igvm(_) => root.add_str(p.model, "microsoft,hyperv")?,
            DeviceTreeBootType::LinuxDirect(_) => root
                .add_str(p.model, "microsoft,openvmm")?
                .add_str(p.compatible, "microsoft,openvmm")?,
        };
        root = T::emit_root_properties(root, &p)?;

        let mut cpus = root
            .start_node("cpus")?
            .add_u32(p.address_cells, 1)?
            .add_u32(p.size_cells, 0)?;
        cpus = T::emit_cpus(self.topology, cpus, &p)?;
        root = cpus.end_node()?;

        let igvm_memory = match &self.boot {
            DeviceTreeBootType::Igvm(igvm) => {
                Some(partition_memory(self.ram, igvm.protectable_ram)?)
            }
            DeviceTreeBootType::LinuxDirect(_) => None,
        };
        match &self.boot {
            DeviceTreeBootType::Igvm(_) => {
                // Keep reverse order to exercise sorting in the partition consumer.
                for (range, vnode, kind) in igvm_memory.into_iter().flatten().rev() {
                    root = emit_memory(root, &p, range, Some((vnode, kind)))?;
                }
            }
            DeviceTreeBootType::LinuxDirect(_) => {
                for entry in self.ram {
                    root = emit_memory(root, &p, entry.range, None)?;
                }
            }
        }

        let smmus = self.smmus();
        root = T::emit_platform(self.topology, root, &p, smmus)?;
        root = emit_pcie(root, &p, self.topology, self.pcie, smmus)?;

        match &self.boot {
            DeviceTreeBootType::Igvm(igvm) => {
                root = emit_igvm_buses(
                    root,
                    &p,
                    igvm.chipset_mmio,
                    igvm.vmbus_redirect,
                    self.dt_uarts,
                )?;
                root = emit_chosen(root, &p, self.command_line, self.console, None)?;
                root = emit_openhcl(root, &p, igvm.vtl2_base_address, igvm.entropy)?;
            }
            DeviceTreeBootType::LinuxDirect(linux) => {
                root = emit_linux_bus(root, &p, linux.low_mmio, linux.high_mmio, self.dt_uarts)?;
                root = emit_chosen(root, &p, self.command_line, self.console, linux.initrd)?;
            }
        }

        let used = root.end_node()?.build(T::boot_cpu(self.topology))?;
        buffer.truncate(used);
        Ok(buffer)
    }
}

macro_rules! properties {
    ($($field:ident: $name:expr),* $(,)?) => {
        pub(crate) struct Properties {
            $($field: StringId,)*
        }

        impl Properties {
            fn new(fdt_builder: &mut Builder<'_>) -> Result<Self, fdt::builder::Error> {
                Ok(Self {
                    $($field: fdt_builder.add_string($name)?,)*
                })
            }
        }
    };
}

properties! {
    address_cells: "#address-cells",
    size_cells: "#size-cells",
    model: "model",
    compatible: "compatible",
    reg: "reg",
    ranges: "ranges",
    device_type: "device_type",
    status: "status",
    numa_node_id: "numa-node-id",
    igvm_type: igvm_defs::dt::IGVM_DT_IGVM_TYPE_PROPERTY,
    vtl: igvm_defs::dt::IGVM_DT_VTL_PROPERTY,
    connection_id: "microsoft,message-connection-id",
    bootargs: "bootargs",
    stdout_path: "stdout-path",
    initrd_start: "linux,initrd-start",
    initrd_end: "linux,initrd-end",
    enable_method: "enable-method",
    method: "method",
    interrupt_cells: "#interrupt-cells",
    interrupt_controller: "interrupt-controller",
    interrupt_names: "interrupt-names",
    interrupts: "interrupts",
    interrupt_parent: "interrupt-parent",
    always_on: "always-on",
    phandle: "phandle",
    clock_frequency: "clock-frequency",
    clock_output_names: "clock-output-names",
    clock_cells: "#clock-cells",
    clocks: "clocks",
    clock_names: "clock-names",
    current_speed: "current-speed",
    arm_periph_id: "arm,primecell-periphid",
    dma_coherent: "dma-coherent",
    bus_range: "bus-range",
    linux_pci_domain: "linux,pci-domain",
    msi_parent: "msi-parent",
    msi_controller: "msi-controller",
    arm_msi_base_spi: "arm,msi-base-spi",
    arm_msi_num_spis: "arm,msi-num-spis",
    iommu_cells: "#iommu-cells",
    iommu_map: "iommu-map",
    linux_pci_probe_only: "linux,pci-probe-only",
    memory_allocation_mode: "memory-allocation-mode",
    memory_size: "memory-size",
    mmio_size: "mmio-size",
    device_types: "device-types",
}

/// Architecture-specific device-tree content.
pub(crate) trait DeviceTreeArch: ArchTopology + inspect::Inspect {
    /// Whether this architecture's UARTs are MMIO devices.
    const MMIO_UARTS: bool;

    /// Validates the architecture-specific configuration.
    fn validate(
        topology: &ProcessorTopology<Self>,
        smmus: &[AcpiSmmuConfig],
        pcie: &[PcieHostBridge],
    ) -> Result<(), DeviceTreeError>;

    /// Adds architecture-specific properties to the root node.
    fn emit_root_properties<'a>(
        root: Builder<'a, Nest<()>>,
        p: &Properties,
    ) -> Result<Builder<'a, Nest<()>>, fdt::builder::Error>;

    /// Adds properties and CPU nodes to the `cpus` node.
    fn emit_cpus<'a>(
        topology: &ProcessorTopology<Self>,
        cpus: Builder<'a, Nest<Nest<()>>>,
        p: &Properties,
    ) -> Result<Builder<'a, Nest<Nest<()>>>, fdt::builder::Error>;

    /// Adds architecture-specific platform nodes, such as interrupt
    /// controllers and timers.
    fn emit_platform<'a, N>(
        topology: &ProcessorTopology<Self>,
        root: Builder<'a, N>,
        p: &Properties,
        smmus: &[AcpiSmmuConfig],
    ) -> Result<Builder<'a, N>, fdt::builder::Error>;

    /// Adds architecture-specific interrupt and NUMA properties to a PCIe host
    /// bridge node.
    fn emit_pcie_properties<'a, N>(
        topology: &ProcessorTopology<Self>,
        node: Builder<'a, Nest<N>>,
        p: &Properties,
        bridge: &PcieHostBridge,
        smmus: &[AcpiSmmuConfig],
    ) -> Result<Builder<'a, Nest<N>>, fdt::builder::Error>;

    /// Returns the `boot_cpuid_phys` value for the FDT header.
    fn boot_cpu(topology: &ProcessorTopology<Self>) -> u32;
}

impl DeviceTreeArch for X86Topology {
    const MMIO_UARTS: bool = false;

    fn validate(
        _topology: &ProcessorTopology<Self>,
        smmus: &[AcpiSmmuConfig],
        _pcie: &[PcieHostBridge],
    ) -> Result<(), DeviceTreeError> {
        if !smmus.is_empty() {
            return Err(DeviceTreeError::Smmu);
        }
        Ok(())
    }

    fn emit_root_properties<'a>(
        root: Builder<'a, Nest<()>>,
        _p: &Properties,
    ) -> Result<Builder<'a, Nest<()>>, fdt::builder::Error> {
        Ok(root)
    }

    fn emit_cpus<'a>(
        topology: &ProcessorTopology<Self>,
        mut cpus: Builder<'a, Nest<Nest<()>>>,
        p: &Properties,
    ) -> Result<Builder<'a, Nest<Nest<()>>>, fdt::builder::Error> {
        for proc in topology.vps_arch() {
            cpus = cpus
                .start_node(&format!("cpu@{:x}", proc.base.vp_index.index() + 1))?
                .add_str(p.device_type, "cpu")?
                .add_u32(p.reg, proc.apic_id)?
                .add_u32(p.numa_node_id, proc.base.vnode)?
                .add_str(p.status, "okay")?
                .end_node()?;
        }
        Ok(cpus)
    }

    fn emit_platform<'a, N>(
        _topology: &ProcessorTopology<Self>,
        root: Builder<'a, N>,
        _p: &Properties,
        _smmus: &[AcpiSmmuConfig],
    ) -> Result<Builder<'a, N>, fdt::builder::Error> {
        Ok(root)
    }

    fn emit_pcie_properties<'a, N>(
        _topology: &ProcessorTopology<Self>,
        node: Builder<'a, Nest<N>>,
        p: &Properties,
        bridge: &PcieHostBridge,
        _smmus: &[AcpiSmmuConfig],
    ) -> Result<Builder<'a, Nest<N>>, fdt::builder::Error> {
        node.add_u32(p.numa_node_id, bridge.vnode.unwrap_or(0))
    }

    fn boot_cpu(topology: &ProcessorTopology<Self>) -> u32 {
        topology.vp_arch(virt::VpIndex::BSP).apic_id
    }
}

impl DeviceTreeArch for Aarch64Topology {
    const MMIO_UARTS: bool = true;

    fn validate(
        topology: &ProcessorTopology<Self>,
        smmus: &[AcpiSmmuConfig],
        pcie: &[PcieHostBridge],
    ) -> Result<(), DeviceTreeError> {
        validate_arm(topology, smmus, pcie)
    }

    fn emit_root_properties<'a>(
        root: Builder<'a, Nest<()>>,
        p: &Properties,
    ) -> Result<Builder<'a, Nest<()>>, fdt::builder::Error> {
        root.add_u32(p.interrupt_parent, PHANDLE_GIC)
    }

    fn emit_cpus<'a>(
        topology: &ProcessorTopology<Self>,
        mut cpus: Builder<'a, Nest<Nest<()>>>,
        p: &Properties,
    ) -> Result<Builder<'a, Nest<Nest<()>>>, fdt::builder::Error> {
        cpus = cpus.add_str(p.compatible, "arm,armv8")?;
        for index in 0..topology.vps().len() {
            // Native direct boot uses the VP index, not the MPIDR.
            let mut cpu = cpus
                .start_node(&format!("cpu@{index}"))?
                .add_u32(p.reg, index as u32)?
                .add_str(p.device_type, "cpu")?
                .add_str(p.status, if index == 0 { "okay" } else { "disabled" })?;
            if topology.vps().len() > 1 {
                cpu = cpu.add_str(p.enable_method, "psci")?;
            }
            cpus = cpu.end_node()?;
        }
        Ok(cpus)
    }

    fn emit_platform<'a, N>(
        topology: &ProcessorTopology<Self>,
        root: Builder<'a, N>,
        p: &Properties,
        smmus: &[AcpiSmmuConfig],
    ) -> Result<Builder<'a, N>, fdt::builder::Error> {
        emit_arm_platform(root, p, topology, smmus)
    }

    fn emit_pcie_properties<'a, N>(
        topology: &ProcessorTopology<Self>,
        mut node: Builder<'a, Nest<N>>,
        p: &Properties,
        bridge: &PcieHostBridge,
        smmus: &[AcpiSmmuConfig],
    ) -> Result<Builder<'a, Nest<N>>, fdt::builder::Error> {
        node = node.add_u32(p.interrupt_parent, PHANDLE_GIC)?;
        match topology.gic_msi() {
            GicMsiController::Its(_) => node = node.add_u32(p.msi_parent, PHANDLE_ITS)?,
            GicMsiController::V2m(_) => node = node.add_u32(p.msi_parent, PHANDLE_V2M)?,
            GicMsiController::None => {}
        }
        if let Some(index) = smmus.iter().position(|smmu| smmu.rc_index == bridge.index) {
            node = node.add_u32_array(
                p.iommu_map,
                &[0, PHANDLE_SMMU_BASE + index as u32, 0, 0x10000],
            )?;
        }
        if bridge.preserve_boot_config {
            node = node.add_u32(p.linux_pci_probe_only, 1)?;
        }
        Ok(node)
    }

    fn boot_cpu(_topology: &ProcessorTopology<Self>) -> u32 {
        0
    }
}

fn partition_memory(
    ram: &[MemoryRangeWithNode],
    protectable: &[MemoryRange],
) -> Result<Vec<(MemoryRange, u32, u32)>, DeviceTreeError> {
    for range in ram
        .iter()
        .map(|r| r.range)
        .chain(protectable.iter().copied())
    {
        if !range.start().is_multiple_of(hvdef::HV_PAGE_SIZE)
            || !range.len().is_multiple_of(hvdef::HV_PAGE_SIZE)
        {
            return Err(DeviceTreeError::Memory);
        }
    }
    if ram
        .windows(2)
        .any(|pair| pair[0].range.end() > pair[1].range.start())
        || protectable
            .windows(2)
            .any(|pair| pair[0].start() > pair[1].start())
    {
        return Err(DeviceTreeError::Memory);
    }
    let mut memory = Vec::new();
    for (range, visibility) in memory_range::walk_ranges(
        ram.iter().map(|r| (r.range, r.vnode)),
        memory_range::flatten_ranges(protectable.iter().copied()).map(|r| (r, ())),
    ) {
        let (vnode, kind) = match visibility {
            memory_range::RangeWalkResult::Neither => continue,
            memory_range::RangeWalkResult::Left(vnode) => {
                (vnode, igvm_defs::MemoryMapEntryType::MEMORY)
            }
            memory_range::RangeWalkResult::Both(vnode, ()) => {
                (vnode, igvm_defs::MemoryMapEntryType::VTL2_PROTECTABLE)
            }
            memory_range::RangeWalkResult::Right(()) => return Err(DeviceTreeError::Memory),
        };
        memory.push((range, vnode, kind.0 as u32));
    }
    Ok(memory)
}

fn emit_memory<'a, N>(
    root: Builder<'a, N>,
    p: &Properties,
    range: MemoryRange,
    classification: Option<(u32, u32)>,
) -> Result<Builder<'a, N>, fdt::builder::Error> {
    let mut memory = root
        .start_node(&format!("memory@{:x}", range.start()))?
        .add_str(p.device_type, "memory")?
        .add_u64_array(p.reg, &[range.start(), range.len()])?;
    if let Some((vnode, kind)) = classification {
        memory = memory
            .add_u32(p.numa_node_id, vnode)?
            .add_u32(p.igvm_type, kind)?;
    }
    memory.end_node()
}

fn emit_pcie<'a, N, T: DeviceTreeArch>(
    mut root: Builder<'a, N>,
    p: &Properties,
    topology: &ProcessorTopology<T>,
    bridges: &[PcieHostBridge],
    smmus: &[AcpiSmmuConfig],
) -> Result<Builder<'a, N>, fdt::builder::Error> {
    for bridge in bridges {
        let mut node = root
            .start_node(&format!("pcie@{:x}", bridge.ecam_range.start()))?
            .add_str(p.compatible, "pci-host-ecam-generic")?
            .add_str(p.device_type, "pci")?
            .add_u64_array(p.reg, &[bridge.ecam_range.start(), bridge.ecam_range.len()])?
            .add_u32_array(
                p.bus_range,
                &[bridge.start_bus.into(), bridge.end_bus.into()],
            )?
            .add_u32(p.linux_pci_domain, bridge.segment.into())?
            .add_u32(p.address_cells, 3)?
            .add_u32(p.size_cells, 2)?
            .add_u32_array(p.ranges, &pcie_ranges(bridge))?;
        node = T::emit_pcie_properties(topology, node, p, bridge, smmus)?;
        root = node.end_node()?;
    }
    Ok(root)
}

/// Encodes the identity-mapped MMIO windows of a PCIe host bridge as a
/// device-tree `ranges` property.
///
/// Each entry is 7 cells: [pci-phys.hi, pci-phys.mid, pci-phys.lo,
/// cpu-phys.hi, cpu-phys.lo, size.hi, size.lo].
fn pcie_ranges(bridge: &PcieHostBridge) -> Vec<u32> {
    // PCI address space type bits (phys.hi bits 25:24).
    const PCI_SPACE_MEM32: u32 = 0x02000000; // 32-bit non-prefetchable MMIO
    const PCI_SPACE_MEM64: u32 = 0x03000000; // 64-bit prefetchable MMIO

    let mut ranges = Vec::with_capacity(14);
    for (space, window) in [
        (PCI_SPACE_MEM32, bridge.low_mmio),
        (PCI_SPACE_MEM64, bridge.high_mmio),
    ] {
        if !window.is_empty() {
            let start = window.start();
            let len = window.len();
            ranges.extend_from_slice(&[
                space,
                (start >> 32) as u32,
                start as u32,
                (start >> 32) as u32,
                start as u32,
                (len >> 32) as u32,
                len as u32,
            ]);
        }
    }
    ranges
}

fn emit_chosen<'a, N>(
    root: Builder<'a, N>,
    p: &Properties,
    command_line: &str,
    console: Option<UartId>,
    initrd: Option<(u64, u64)>,
) -> Result<Builder<'a, N>, fdt::builder::Error> {
    let mut chosen = root
        .start_node("chosen")?
        .add_str(p.bootargs, command_line)?;
    if let Some((start, end)) = initrd {
        chosen = chosen
            .add_u64(p.initrd_start, start)?
            .add_u64(p.initrd_end, end)?;
    }
    if let Some(console) = console {
        let parent = if uart_is_mmio(console) {
            "openvmm"
        } else {
            "pio-bus"
        };
        chosen = chosen.add_str(
            p.stdout_path,
            &format!("/{parent}/{}", uart_node_name(console)),
        )?;
    }
    chosen.end_node()
}

fn emit_openhcl<'a, N>(
    root: Builder<'a, N>,
    p: &Properties,
    base: Vtl2BaseAddressType,
    entropy: Option<&[u8]>,
) -> Result<Builder<'a, N>, fdt::builder::Error> {
    let mut openhcl = root.start_node("openhcl")?;
    let mode = match base {
        Vtl2BaseAddressType::Vtl2Allocate { size } => {
            if let Some(size) = size {
                openhcl = openhcl.add_u64(p.memory_size, size)?;
            }
            openhcl = openhcl.add_u64(p.mmio_size, 128 * 1024 * 1024)?;
            "vtl2"
        }
        _ => "host",
    };
    openhcl = openhcl.add_str(p.memory_allocation_mode, mode)?;
    if let Some(entropy) = entropy {
        openhcl = openhcl
            .start_node("entropy")?
            .add_prop_array(p.reg, &[entropy])?
            .end_node()?;
    }
    openhcl
        .start_node("keep-alive")?
        .add_str(p.device_types, "nvme")?
        .end_node()?
        .end_node()
}

fn gic_registers(topology: &ProcessorTopology<Aarch64Topology>) -> [u64; 4] {
    let (dist_size, second_base, second_size) = match topology.gic_version() {
        GicVersion::V3 {
            redistributors_base,
        } => (
            aarch64defs::GIC_DISTRIBUTOR_SIZE,
            redistributors_base,
            aarch64defs::GIC_REDISTRIBUTOR_SIZE * topology.vps().len() as u64,
        ),
        GicVersion::V2 { cpu_interface_base } => (
            aarch64defs::GIC_V2_DISTRIBUTOR_SIZE,
            cpu_interface_base,
            aarch64defs::GIC_V2_CPU_INTERFACE_SIZE,
        ),
    };
    [
        topology.gic_distributor_base(),
        dist_size,
        second_base,
        second_size,
    ]
}

fn validate_arm(
    topology: &ProcessorTopology<Aarch64Topology>,
    smmus: &[AcpiSmmuConfig],
    bridges: &[PcieHostBridge],
) -> Result<(), DeviceTreeError> {
    let [dist_base, dist_size, second_base, second_size] = gic_registers(topology);
    let dist_end = dist_base
        .checked_add(dist_size)
        .ok_or(DeviceTreeError::GicRange)?;
    let second_end = second_base
        .checked_add(second_size)
        .ok_or(DeviceTreeError::GicRange)?;
    if dist_base < second_end && second_base < dist_end {
        return Err(DeviceTreeError::GicRange);
    }
    if !(16..32).contains(&topology.virt_timer_ppi())
        || topology
            .pmu_gsiv()
            .is_some_and(|ppi| !(16..32).contains(&ppi))
    {
        return Err(DeviceTreeError::Interrupt);
    }
    if smmus.len() > (u32::MAX - PHANDLE_SMMU_BASE) as usize {
        return Err(DeviceTreeError::Smmu);
    }
    for (index, smmu) in smmus.iter().enumerate() {
        if smmu.event_gsiv < 32
            || smmu.gerr_gsiv < 32
            || smmu.base.checked_add(0x2_0000).is_none()
            || smmus[..index]
                .iter()
                .any(|other| other.rc_index == smmu.rc_index || other.base == smmu.base)
            || !bridges.iter().any(|bridge| bridge.index == smmu.rc_index)
        {
            return Err(DeviceTreeError::Smmu);
        }
    }
    Ok(())
}

fn emit_arm_platform<'a, N>(
    mut root: Builder<'a, N>,
    p: &Properties,
    topology: &ProcessorTopology<Aarch64Topology>,
    smmus: &[AcpiSmmuConfig],
) -> Result<Builder<'a, N>, fdt::builder::Error> {
    root = root
        .start_node("psci")?
        .add_str(p.compatible, "arm,psci-0.2")?
        .add_str(p.method, "hvc")?
        .end_node()?;
    root = root
        .start_node("apb-pclk")?
        .add_str(p.compatible, "fixed-clock")?
        .add_u32(p.clock_frequency, 24000000)?
        .add_str_array(p.clock_output_names, &["clk24mhz"])?
        .add_u32(p.clock_cells, 0)?
        .add_u32(p.phandle, PHANDLE_APB_PCLK)?
        .end_node()?;
    let gic_compatible = match topology.gic_version() {
        GicVersion::V3 { .. } => "arm,gic-v3",
        GicVersion::V2 { .. } => "arm,cortex-a15-gic",
    };
    let gic = root
        .start_node(&format!("intc@{:x}", topology.gic_distributor_base()))?
        .add_str(p.compatible, gic_compatible)?
        .add_u64_array(p.reg, &gic_registers(topology))?
        .add_u32(p.address_cells, 2)?
        .add_u32(p.size_cells, 2)?
        .add_u32(p.interrupt_cells, 3)?
        .add_null(p.interrupt_controller)?
        .add_u32(p.phandle, PHANDLE_GIC)?
        .add_null(p.ranges)?;
    root = match topology.gic_msi() {
        GicMsiController::Its(its) => gic
            .start_node(&format!("its@{:x}", its.its_base))?
            .add_str(p.compatible, "arm,gic-v3-its")?
            .add_null(p.msi_controller)?
            .add_u64_array(p.reg, &[its.its_base, openvmm_defs::config::GIC_ITS_SIZE])?
            .add_u32(p.phandle, PHANDLE_ITS)?
            .end_node()?
            .end_node()?,
        GicMsiController::V2m(v2m) => gic
            .start_node(&format!("v2m@{:x}", v2m.frame_base))?
            .add_str(p.compatible, "arm,gic-v2m-frame")?
            .add_null(p.msi_controller)?
            .add_u64_array(
                p.reg,
                &[v2m.frame_base, openvmm_defs::config::GIC_V2M_MSI_FRAME_SIZE],
            )?
            .add_u32(p.arm_msi_base_spi, v2m.spi_base)?
            .add_u32(p.arm_msi_num_spis, v2m.spi_count)?
            .add_u32(p.phandle, PHANDLE_V2M)?
            .end_node()?
            .end_node()?,
        GicMsiController::None => gic.end_node()?,
    };
    for (index, smmu) in smmus.iter().enumerate() {
        root = root
            .start_node(&format!("smmu@{:x}", smmu.base))?
            .add_str(p.compatible, "arm,smmu-v3")?
            .add_u64_array(p.reg, &[smmu.base, 0x2_0000])?
            .add_u32_array(
                p.interrupts,
                &[
                    GIC_SPI,
                    smmu.event_gsiv - 32,
                    IRQ_TYPE_LEVEL_HIGH,
                    GIC_SPI,
                    smmu.gerr_gsiv - 32,
                    IRQ_TYPE_LEVEL_HIGH,
                ],
            )?
            .add_str_array(p.interrupt_names, &["eventq", "gerror"])?
            .add_u32(p.iommu_cells, 1)?
            .add_u32(p.phandle, PHANDLE_SMMU_BASE + index as u32)?
            .add_null(p.dma_coherent)?
            .end_node()?;
    }
    root = root
        .start_node("timer")?
        .add_str(p.compatible, "arm,armv8-timer")?
        .add_u32(p.interrupt_parent, PHANDLE_GIC)?
        .add_str(p.interrupt_names, "virt")?
        .add_u32_array(p.interrupts, &[GIC_PPI, topology.virt_timer_ppi() - 16, 8])?
        .add_null(p.always_on)?
        .end_node()?;
    if let Some(ppi) = topology.pmu_gsiv() {
        root = root
            .start_node("pmu")?
            .add_str(p.compatible, "arm,armv8-pmuv3")?
            .add_u32_array(p.interrupts, &[GIC_PPI, ppi - 16, IRQ_TYPE_LEVEL_HIGH])?
            .end_node()?;
    }
    Ok(root)
}

fn emit_igvm_buses<'a, N>(
    root: Builder<'a, N>,
    p: &Properties,
    mmio: ChipsetMmioRanges,
    redirect: bool,
    dt_uarts: &[UartId],
) -> Result<Builder<'a, N>, fdt::builder::Error> {
    let mut bus = root
        .start_node("bus")?
        .add_str(p.compatible, "simple-bus")?
        .add_u32(p.address_cells, 2)?
        .add_u32(p.size_cells, 2)?
        .add_null(p.ranges)?;
    let ranges_vtl0: Vec<_> = [mmio.low, mmio.high]
        .into_iter()
        .flat_map(|range| [range.start(), range.start(), range.len()])
        .collect();
    let ranges_vtl2 = if mmio.vtl2.is_empty() {
        vec![]
    } else {
        vec![mmio.vtl2.start(), mmio.vtl2.start(), mmio.vtl2.len()]
    };
    for (vtl, ranges, connection) in [
        (0, ranges_vtl0, 1),
        (2, ranges_vtl2, if redirect { 0x800074 } else { 4 }),
    ] {
        let name = match ranges.first() {
            Some(base) => format!("vmbus-vtl{vtl}@{base:x}"),
            None => format!("vmbus-vtl{vtl}"),
        };
        bus = bus
            .start_node(&name)?
            .add_u32(p.address_cells, 2)?
            .add_u32(p.size_cells, 2)?
            .add_str(p.compatible, "microsoft,vmbus")?
            .add_u64_array(p.ranges, &ranges)?
            .add_u32(p.vtl, vtl)?
            .add_u32(p.connection_id, connection)?
            .end_node()?;
    }
    let mut root = bus.end_node()?;
    if !dt_uarts.is_empty() {
        let mut pio = root
            .start_node("pio-bus")?
            .add_str(p.compatible, "x86-pio-bus")?
            .add_u32(p.address_cells, 1)?
            .add_u32(p.size_cells, 1)?
            .add_null(p.ranges)?;
        for &uart in dt_uarts {
            pio = emit_uart(pio, p, uart)?;
        }
        root = pio.end_node()?;
    }
    Ok(root)
}

fn emit_linux_bus<'a, N>(
    root: Builder<'a, N>,
    p: &Properties,
    low: MemoryRange,
    high: MemoryRange,
    dt_uarts: &[UartId],
) -> Result<Builder<'a, N>, fdt::builder::Error> {
    let mut bus = root
        .start_node("openvmm")?
        .add_str(p.compatible, "simple-bus")?
        .add_u32(p.address_cells, 2)?
        .add_u32(p.size_cells, 2)?
        .add_null(p.ranges)?
        .add_u32(p.interrupt_parent, PHANDLE_GIC)?;
    for &uart in dt_uarts {
        bus = emit_uart(bus, p, uart)?;
    }
    bus.start_node("vmbus")?
        .add_u32(p.address_cells, 2)?
        .add_u32(p.size_cells, 2)?
        .add_null(p.dma_coherent)?
        .add_u64_array(
            p.ranges,
            &[low.start(), low.len(), high.start(), high.len()],
        )?
        .add_str(p.compatible, "microsoft,vmbus")?
        .add_u32(p.interrupt_parent, PHANDLE_GIC)?
        .add_u32_array(
            p.interrupts,
            &[GIC_PPI, openvmm_defs::config::DEFAULT_VMBUS_PPI - 16, 1],
        )?
        .end_node()?
        .end_node()
}

fn uart_is_mmio(uart: UartId) -> bool {
    matches!(uart, UartId::Pl0110 | UartId::Pl0111)
}

fn uart_resources(uart: UartId) -> (u64, u64, u32) {
    use serial_pl011_resources::PL011_SERIAL_SIZE;
    use serial_pl011_resources::PL011_SERIAL0_BASE;
    use serial_pl011_resources::PL011_SERIAL0_SPI;
    use serial_pl011_resources::PL011_SERIAL1_BASE;
    use serial_pl011_resources::PL011_SERIAL1_SPI;

    let com = match uart {
        UartId::Com(com) => com,
        UartId::Pl0110 => return (PL011_SERIAL0_BASE, PL011_SERIAL_SIZE, PL011_SERIAL0_SPI),
        UartId::Pl0111 => return (PL011_SERIAL1_BASE, PL011_SERIAL_SIZE, PL011_SERIAL1_SPI),
    };
    (
        com.io_port().into(),
        serial_16550_resources::COM_REGISTER_COUNT.into(),
        com.irq().into(),
    )
}

fn uart_node_name(uart: UartId) -> String {
    let (base, _, _) = uart_resources(uart);
    let prefix = if uart_is_mmio(uart) { "uart" } else { "serial" };
    format!("{prefix}@{base:x}")
}

fn emit_uart<'a, N>(
    parent: Builder<'a, N>,
    p: &Properties,
    uart: UartId,
) -> Result<Builder<'a, N>, fdt::builder::Error> {
    let (base, length, interrupt) = uart_resources(uart);
    let mut node = parent
        .start_node(&uart_node_name(uart))?
        // The partition PIO binding deliberately retains 64-bit reg cells.
        .add_u64_array(p.reg, &[base, length])?
        .add_u32(p.current_speed, 115200)?;
    if uart_is_mmio(uart) {
        node = node
            .add_str_array(p.compatible, &["arm,sbsa-uart", "arm,primecell"])?
            .add_str_array(p.clock_names, &["apb_pclk"])?
            .add_u32(p.clocks, PHANDLE_APB_PCLK)?
            .add_u32(p.interrupt_parent, PHANDLE_GIC)?
            // Select the SBSA subset of PL011.
            .add_u32(p.arm_periph_id, 0x00041011)?
            .add_u32_array(p.interrupts, &[GIC_SPI, interrupt, IRQ_TYPE_LEVEL_HIGH])?
            .add_str(p.status, "okay")?;
    } else {
        node = node
            .add_str(p.compatible, "ns16550")?
            .add_u32(p.clock_frequency, 0)?
            .add_u64_array(p.interrupts, &[interrupt.into()])?;
    }
    node.end_node()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdt::parser::Node;
    use fdt::parser::Parser;
    use serial_16550_resources::ComPort;
    use test_with_tracing::test;
    use vm_topology::processor::TopologyBuilder;
    use vm_topology::processor::aarch64::Aarch64PlatformConfig;
    use vm_topology::processor::aarch64::GicItsInfo;

    const RAM: &[MemoryRangeWithNode] = &[MemoryRangeWithNode {
        range: MemoryRange::new(0..0x4000_0000),
        vnode: 0,
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

    fn arm_platform() -> Aarch64PlatformConfig {
        Aarch64PlatformConfig {
            gic_distributor_base: 0xffff_0000,
            gic_version: GicVersion::V3 {
                redistributors_base: 0xeff0_0000,
            },
            gic_msi: GicMsiController::Its(GicItsInfo {
                its_base: 0x6000_0000,
            }),
            pmu_gsiv: Some(23),
            virt_timer_ppi: 20,
            gic_nr_irqs: 256,
        }
    }

    fn pcie_bridge() -> PcieHostBridge {
        PcieHostBridge {
            index: 4,
            segment: 7,
            start_bus: 32,
            end_bus: 47,
            ecam_range: MemoryRange::new(0x8000_0000..0x8100_0000),
            low_mmio: MemoryRange::new(0x9000_0000..0xa000_0000),
            high_mmio: MemoryRange::new(0x12_0000_0000..0x14_0000_0000),
            cxl: None,
            vnode: None,
            preserve_bars: false,
            preserve_boot_config: true,
        }
    }

    fn smmu() -> AcpiSmmuConfig {
        AcpiSmmuConfig {
            rc_index: 4,
            segment: 7,
            base: 0x7000_0000,
            event_gsiv: 35,
            gerr_gsiv: 36,
            reserved_iova_ranges: vec![],
        }
    }

    fn igvm_boot() -> IgvmBoot<'static> {
        IgvmBoot {
            chipset_mmio: mmio(),
            vtl2_base_address: Vtl2BaseAddressType::File,
            protectable_ram: &[],
            vmbus_redirect: false,
            entropy: None,
        }
    }

    fn linux_boot() -> LinuxDirectBoot<'static> {
        LinuxDirectBoot {
            low_mmio: mmio().low,
            high_mmio: mmio().high,
            initrd: None,
            smmus: &[],
        }
    }

    fn igvm(
        topology: &ProcessorTopology<X86Topology>,
        capacity: usize,
    ) -> DeviceTreeBuilder<'_, X86Topology> {
        DeviceTreeBuilder::new(
            topology,
            RAM,
            COMS,
            &[],
            capacity,
            DeviceTreeBootType::Igvm(igvm_boot()),
        )
    }

    fn linux<'a>(
        topology: &'a ProcessorTopology<Aarch64Topology>,
        bridges: &'a [PcieHostBridge],
        smmus: &'a [AcpiSmmuConfig],
    ) -> DeviceTreeBuilder<'a, Aarch64Topology> {
        DeviceTreeBuilder::new(
            topology,
            RAM,
            PL011S,
            bridges,
            0x10000,
            DeviceTreeBootType::LinuxDirect(LinuxDirectBoot {
                smmus,
                ..linux_boot()
            }),
        )
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

    fn stdout_path(dt: &[u8]) -> &str {
        node(dt, "/chosen")
            .find_property("stdout-path")
            .unwrap()
            .unwrap()
            .read_str()
            .unwrap()
    }

    #[test]
    fn console_path_selects_device_tree_uart() {
        let x86 = TopologyBuilder::new_x86().build(2).unwrap();
        for &uart in COMS {
            let dt = igvm(&x86, 0x10000)
                .with_console(Some(uart))
                .build()
                .unwrap();
            let path = stdout_path(&dt);
            assert!(path.starts_with("/pio-bus/serial@"));
            let (base, length, irq) = uart_resources(uart);
            let serial = node(&dt, path);
            assert_eq!(u64_property(&serial, "reg"), [base, length]);
            assert_eq!(u64_property(&serial, "interrupts"), [u64::from(irq)]);
        }

        let arm = TopologyBuilder::new_aarch64(arm_platform())
            .build(2)
            .unwrap();
        for &uart in PL011S {
            let dt = linux(&arm, &[], &[])
                .with_console(Some(uart))
                .build()
                .unwrap();
            let path = stdout_path(&dt);
            assert!(path.starts_with("/openvmm/uart@"));
            let (base, length, spi) = uart_resources(uart);
            let serial = node(&dt, path);
            assert_eq!(u64_property(&serial, "reg"), [base, length]);
            assert_eq!(
                u32_property(&serial, "interrupts"),
                [GIC_SPI, spi, IRQ_TYPE_LEVEL_HIGH]
            );
        }

        assert!(matches!(
            DeviceTreeBuilder::new(
                &x86,
                RAM,
                &COMS[..2],
                &[],
                0x10000,
                DeviceTreeBootType::Igvm(igvm_boot()),
            )
            .with_console(Some(UartId::Com(ComPort::Com3)))
            .build(),
            Err(DeviceTreeError::ConsoleNotInDeviceTree)
        ));
    }

    #[test]
    fn tree_fits_exact_capacity() {
        let topology = TopologyBuilder::new_x86().build(2).unwrap();
        let dt = igvm(&topology, 0x10000).build().unwrap();
        assert_eq!(igvm(&topology, dt.len()).build().unwrap(), dt);
        assert!(matches!(
            igvm(&topology, dt.len() - 1).build(),
            Err(DeviceTreeError::Fdt(fdt::builder::Error::OutOfSpace))
        ));
    }

    #[test]
    fn arm_pcie_references_msi_and_iommu() {
        let topology = TopologyBuilder::new_aarch64(arm_platform())
            .build(2)
            .unwrap();
        let bridges = [pcie_bridge()];
        let smmus = [smmu()];
        let dt = linux(&topology, &bridges, &smmus).build().unwrap();

        let pcie = node(&dt, "/pcie@80000000");
        let its = node(&dt, "/intc@ffff0000/its@60000000");
        let smmu = node(&dt, "/smmu@70000000");
        assert_eq!(u32_property(&pcie, "bus-range"), [32, 47]);
        assert_eq!(u32_property(&pcie, "linux,pci-domain"), [7]);
        assert_eq!(
            u32_property(&pcie, "ranges"),
            [
                0x0200_0000,
                0,
                0x9000_0000,
                0,
                0x9000_0000,
                0,
                0x1000_0000,
                0x0300_0000,
                0x12,
                0,
                0x12,
                0,
                2,
                0,
            ]
        );
        assert_eq!(
            u32_property(&pcie, "msi-parent"),
            u32_property(&its, "phandle")
        );
        assert_eq!(
            u32_property(&pcie, "iommu-map"),
            [0, u32_property(&smmu, "phandle")[0], 0, 0x10000]
        );
        assert_eq!(u32_property(&pcie, "linux,pci-probe-only"), [1]);
    }

    #[test]
    fn arm_rejects_invalid_configuration() {
        let overlapping_gic = Aarch64PlatformConfig {
            gic_distributor_base: 0xeff0_0000,
            ..arm_platform()
        };
        let platforms = [
            (
                Aarch64PlatformConfig {
                    virt_timer_ppi: 15,
                    ..arm_platform()
                },
                DeviceTreeError::Interrupt,
            ),
            (
                Aarch64PlatformConfig {
                    pmu_gsiv: Some(32),
                    ..arm_platform()
                },
                DeviceTreeError::Interrupt,
            ),
            (overlapping_gic, DeviceTreeError::GicRange),
        ];
        for (platform, expected) in platforms {
            // Supply VPs directly so that the topology builder's own platform
            // checks do not hide the device-tree validation.
            let valid = TopologyBuilder::new_aarch64(arm_platform())
                .build(2)
                .unwrap();
            let topology = TopologyBuilder::new_aarch64(platform)
                .build_with_vp_info(valid.vps_arch())
                .unwrap();
            let error = linux(&topology, &[], &[]).build().unwrap_err();
            assert_eq!(
                std::mem::discriminant(&error),
                std::mem::discriminant(&expected),
                "{error:?}"
            );
        }

        let topology = TopologyBuilder::new_aarch64(arm_platform())
            .build(2)
            .unwrap();
        let bridges = [pcie_bridge()];
        // An SMMU needs an SPI and a matching host bridge.
        for smmu in [
            AcpiSmmuConfig {
                event_gsiv: 31,
                ..smmu()
            },
            AcpiSmmuConfig {
                rc_index: 5,
                ..smmu()
            },
        ] {
            assert!(matches!(
                linux(&topology, &bridges, &[smmu]).build(),
                Err(DeviceTreeError::Smmu)
            ));
        }
    }
}
