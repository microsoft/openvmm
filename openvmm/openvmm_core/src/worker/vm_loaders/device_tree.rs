// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Complete device trees for partition handoff and native Linux boot.

use super::super::memory_layout::ChipsetMmioRanges;
use fdt::builder::Builder;
use fdt::builder::Nest;
use fdt::builder::StringId;
use memory_range::MemoryRange;
use openvmm_defs::config::Vtl2BaseAddressType;
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
use vmm_core_defs::uart::UartId;

const STRING_TABLE_CAP: usize = 1024;
const PHANDLE_GIC: u32 = 1;
const PHANDLE_APB_PCLK: u32 = 2;
const PHANDLE_V2M: u32 = 3;
const PHANDLE_ITS: u32 = 4;
const PHANDLE_SMMU_BASE: u32 = 5;
const GIC_SPI: u32 = 0;
const GIC_PPI: u32 = 1;
const IRQ_TYPE_LEVEL_HIGH: u32 = 4;

#[cfg(test)]
mod tests;

#[derive(Debug, Error)]
pub enum DeviceTreeError {
    #[error("device tree capacity is required")]
    MissingCapacity,
    #[error("device tree boot settings are required")]
    MissingBootSettings,
    #[error("device tree capacity is too small or exceeds the FDT size limit")]
    Capacity,
    #[error("failed to allocate device tree buffer")]
    Allocation(#[source] std::collections::TryReserveError),
    #[error("device tree encoding failed")]
    Fdt(#[from] fdt::builder::Error),
    #[error("console UART is not in the device inventory")]
    ConsoleNotAttached,
    #[error("duplicate UART in device inventory")]
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

pub(crate) struct DeviceTreeBuilder<'a, T: ArchTopology> {
    topology: &'a ProcessorTopology<T>,
    ram: &'a [MemoryRangeWithNode],
    uarts: &'a [UartId],
    pcie: &'a [PcieHostBridge],
    command_line: &'a str,
    console: Option<UartId>,
    capacity: Option<usize>,
    igvm: Option<(ChipsetMmioRanges, Vtl2BaseAddressType)>,
    protectable_ram: &'a [MemoryRange],
    vmbus_redirect: bool,
    entropy: Option<&'a [u8]>,
    linux_mmio: Option<(MemoryRange, MemoryRange)>,
    initrd: Option<(u64, u64)>,
    smmus: &'a [AcpiSmmuConfig],
}

impl<'a, T: ArchTopology> DeviceTreeBuilder<'a, T> {
    pub(crate) fn new(
        topology: &'a ProcessorTopology<T>,
        ram: &'a [MemoryRangeWithNode],
        uarts: &'a [UartId],
        pcie: &'a [PcieHostBridge],
    ) -> Self {
        Self {
            topology,
            ram,
            uarts,
            pcie,
            command_line: "",
            console: None,
            capacity: None,
            igvm: None,
            protectable_ram: &[],
            vmbus_redirect: false,
            entropy: None,
            linux_mmio: None,
            initrd: None,
            smmus: &[],
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

    pub(crate) fn with_capacity(mut self, bytes: usize) -> Self {
        self.capacity = Some(bytes);
        self
    }

    fn serialize(self, architecture: Architecture<'_>) -> Result<Vec<u8>, DeviceTreeError> {
        let capacity = self.capacity.ok_or(DeviceTreeError::MissingCapacity)?;
        // Builder::new does not check the backing buffer before add_string
        // indexes its reserved string region.
        // The version 17 FDT header has ten 32-bit fields.
        let minimum = 40 + size_of::<fdt::ReserveEntry>() + STRING_TABLE_CAP;
        if capacity < minimum || capacity > u32::MAX as usize {
            return Err(DeviceTreeError::Capacity);
        }
        let arm = match architecture {
            Architecture::X86(_) => {
                self.igvm.ok_or(DeviceTreeError::MissingBootSettings)?;
                None
            }
            Architecture::Arm(topology) => {
                self.linux_mmio
                    .ok_or(DeviceTreeError::MissingBootSettings)?;
                validate_arm(topology, self.smmus, self.pcie)?;
                Some(topology)
            }
        };
        let processor_count = match architecture {
            Architecture::X86(topology) => topology.vps().len(),
            Architecture::Arm(topology) => topology.vps().len(),
        };
        if processor_count == 0 || processor_count > u32::MAX as usize {
            return Err(DeviceTreeError::Processors);
        }
        if self.command_line.contains('\0') {
            return Err(DeviceTreeError::CommandLine);
        }
        if self.initrd.is_some_and(|(start, end)| start > end) {
            return Err(DeviceTreeError::InitrdRange);
        }
        if self.console.is_some_and(|uart| !self.uarts.contains(&uart)) {
            return Err(DeviceTreeError::ConsoleNotAttached);
        }
        for (index, uart) in self.uarts.iter().enumerate() {
            if self.uarts[..index].contains(uart) {
                return Err(DeviceTreeError::DuplicateUart);
            }
            if uart_is_mmio(*uart) != arm.is_some() {
                return Err(DeviceTreeError::UartArchitecture);
            }
        }
        let memory = if arm.is_none() {
            Some(partition_memory(self.ram, self.protectable_ram)?)
        } else {
            None
        };
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(capacity)
            .map_err(DeviceTreeError::Allocation)?;
        buffer.resize(capacity, 0);
        let mut writer = Builder::new(fdt::builder::BuilderConfig {
            blob_buffer: &mut buffer,
            string_table_cap: STRING_TABLE_CAP,
            memory_reservations: &[],
        })?;
        let p = Properties::new(&mut writer)?;
        let mut root = writer
            .start_node("")?
            .add_u32(p.address_cells, 2)?
            .add_u32(p.size_cells, 2)?
            .add_str(
                p.model,
                if arm.is_some() {
                    "microsoft,openvmm"
                } else {
                    "microsoft,hyperv"
                },
            )?;
        if arm.is_some() {
            root = root
                .add_str(p.compatible, "microsoft,openvmm")?
                .add_u32(p.interrupt_parent, PHANDLE_GIC)?;
        }
        root = emit_cpus(root, &p, &architecture)?;
        if let Some(memory) = memory {
            // Keep reverse order to exercise sorting in the partition consumer.
            for (range, vnode, kind) in memory.into_iter().rev() {
                root = emit_memory(root, &p, range, Some((vnode, kind)))?;
            }
        } else {
            for entry in self.ram {
                root = emit_memory(root, &p, entry.range, None)?;
            }
        }
        if let Some(topology) = arm {
            root = emit_arm_platform(root, &p, topology, self.smmus)?;
        }
        root = emit_pcie(root, &p, self.pcie, arm, self.smmus)?;
        if let Some((mmio, _)) = self.igvm {
            root = emit_igvm_buses(root, &p, mmio, self.vmbus_redirect, self.uarts)?;
        }
        if let Some((low, high)) = self.linux_mmio {
            root = emit_linux_bus(root, &p, low, high, self.uarts)?;
        }
        root = emit_chosen(root, &p, self.command_line, self.console, self.initrd)?;
        if let Some((_, base)) = self.igvm {
            root = emit_openhcl(root, &p, base, self.entropy)?;
        }
        let boot_cpu = match architecture {
            Architecture::X86(topology) => topology.vp_arch(virt::VpIndex::BSP).apic_id,
            Architecture::Arm(_) => 0,
        };
        let used = root.end_node()?.build(boot_cpu)?;
        buffer.truncate(used);
        Ok(buffer)
    }
}

impl<'a> DeviceTreeBuilder<'a, X86Topology> {
    pub(crate) fn with_igvm(
        mut self,
        chipset_mmio: ChipsetMmioRanges,
        vtl2_base_address: Vtl2BaseAddressType,
    ) -> Self {
        self.igvm = Some((chipset_mmio, vtl2_base_address));
        self
    }

    pub(crate) fn with_igvm_protectable_ram(mut self, ram: &'a [MemoryRange]) -> Self {
        self.protectable_ram = ram;
        self
    }

    pub(crate) fn with_igvm_vmbus_redirect(mut self, enabled: bool) -> Self {
        self.vmbus_redirect = enabled;
        self
    }

    pub(crate) fn with_igvm_entropy(mut self, entropy: Option<&'a [u8]>) -> Self {
        self.entropy = entropy;
        self
    }

    pub(crate) fn finish(self) -> Result<Vec<u8>, DeviceTreeError> {
        let topology = self.topology;
        self.serialize(Architecture::X86(topology))
    }
}

impl<'a> DeviceTreeBuilder<'a, Aarch64Topology> {
    pub(crate) fn with_linux_direct(mut self, low: MemoryRange, high: MemoryRange) -> Self {
        self.linux_mmio = Some((low, high));
        self
    }

    pub(crate) fn with_linux_initrd(mut self, range: Option<(u64, u64)>) -> Self {
        self.initrd = range;
        self
    }

    pub(crate) fn with_linux_smmus(mut self, smmus: &'a [AcpiSmmuConfig]) -> Self {
        self.smmus = smmus;
        self
    }

    pub(crate) fn finish(self) -> Result<Vec<u8>, DeviceTreeError> {
        let topology = self.topology;
        self.serialize(Architecture::Arm(topology))
    }
}

#[derive(Clone, Copy)]
enum Architecture<'a> {
    X86(&'a ProcessorTopology<X86Topology>),
    Arm(&'a ProcessorTopology<Aarch64Topology>),
}

macro_rules! properties {
    ($($field:ident: $name:expr),* $(,)?) => {
        struct Properties {
            $($field: StringId,)*
        }

        impl Properties {
            fn new(writer: &mut Builder<'_>) -> Result<Self, fdt::builder::Error> {
                Ok(Self {
                    $($field: writer.add_string($name)?,)*
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

fn emit_cpus<'a>(
    root: Builder<'a, Nest<()>>,
    p: &Properties,
    architecture: &Architecture<'_>,
) -> Result<Builder<'a, Nest<()>>, fdt::builder::Error> {
    let mut cpus = root
        .start_node("cpus")?
        .add_u32(p.address_cells, 1)?
        .add_u32(p.size_cells, 0)?;
    match architecture {
        Architecture::X86(topology) => {
            for proc in topology.vps_arch() {
                cpus = cpus
                    .start_node(&format!("cpu@{:x}", proc.base.vp_index.index() + 1))?
                    .add_str(p.device_type, "cpu")?
                    .add_u32(p.reg, proc.apic_id)?
                    .add_u32(p.numa_node_id, proc.base.vnode)?
                    .add_str(p.status, "okay")?
                    .end_node()?;
            }
        }
        Architecture::Arm(topology) => {
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
        }
    }
    cpus.end_node()
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

fn emit_pcie<'a, N>(
    mut root: Builder<'a, N>,
    p: &Properties,
    bridges: &[PcieHostBridge],
    arm: Option<&ProcessorTopology<Aarch64Topology>>,
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
            .add_u32_array(p.ranges, &super::pcie::identity_ranges(bridge))?;
        if let Some(topology) = arm {
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
        } else {
            node = node.add_u32(p.numa_node_id, bridge.vnode.unwrap_or(0))?;
        }
        root = node.end_node()?;
    }
    Ok(root)
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
    if topology.vps().len() > u32::MAX as usize {
        return Err(DeviceTreeError::Processors);
    }
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
    uarts: &[UartId],
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
    if !uarts.is_empty() {
        let mut pio = root
            .start_node("pio-bus")?
            .add_str(p.compatible, "x86-pio-bus")?
            .add_u32(p.address_cells, 1)?
            .add_u32(p.size_cells, 1)?
            .add_null(p.ranges)?;
        for &uart in uarts {
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
    uarts: &[UartId],
) -> Result<Builder<'a, N>, fdt::builder::Error> {
    let mut bus = root
        .start_node("openvmm")?
        .add_str(p.compatible, "simple-bus")?
        .add_u32(p.address_cells, 2)?
        .add_u32(p.size_cells, 2)?
        .add_null(p.ranges)?
        .add_u32(p.interrupt_parent, PHANDLE_GIC)?;
    for &uart in uarts {
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
