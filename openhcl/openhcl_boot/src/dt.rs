// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Module used to write the device tree used by the OpenHCL kernel and
//! usermode.

use crate::host_params::COMMAND_LINE_SIZE;
use crate::host_params::PartitionInfo;
use crate::host_params::shim_params::IsolationType;
use crate::memory::AddressSpaceManager;
use crate::memory::MAX_RESERVED_MEM_RANGES;
use crate::sidecar::SidecarConfig;
use crate::single_threaded::off_stack;
use arrayvec::ArrayString;
use arrayvec::ArrayVec;
use core::fmt;
use core::ops::Range;
use fdt::builder::Builder;
use fdt::builder::StringId;
use host_fdt_parser::GicInfo;
use host_fdt_parser::MemoryAllocationMode;
use host_fdt_parser::VmbusInfo;
use hvdef::Vtl;
use igvm_defs::dt::IGVM_DT_IGVM_TYPE_PROPERTY;
use loader_defs::shim::MemoryVtlType;
use memory_range::MemoryRange;
use memory_range::RangeWalkResult;
use memory_range::walk_ranges;
#[cfg(target_arch = "x86_64")]
use x86defs::tdx::RESET_VECTOR_PAGE;

/// AArch64 defines
mod aarch64 {
    // For compatibility with older hosts, use these legacy Hyper-V defaults if
    // GIC addresses aren't passed in via the host device tree
    pub const DEFAULT_GIC_DISTRIBUTOR_BASE: u64 = 0xFFFF_0000;
    pub const DEFAULT_GIC_REDISTRIBUTORS_BASE: u64 = 0xEFFE_E000;

    // The interrupt IDs (INTID's) in the ARM64 DeviceTree must be _relative_
    // to its base. See `gic_irq_domain_translate` in the Linux kernel, could not
    // find a specification for that.
    //
    // Architecturally, PPIs occupy INTID's in the [16..32) range. In DeviceTree,
    // the type of the interrupt is specified first (PPI) and then the _relative_ INTID:
    // for PPI INTID `27` `[GIC_PPI, 27-16, flags]` goes into the DT description.
    pub const VMBUS_INTID: u32 = 2; // Note: the hardware INTID will be 16 + 2
    pub const TIMER_INTID: u32 = 4; // Note: the hardware INTID will be 16 + 4

    /// The Hyper-V default PMU_GSIV value.
    pub const PMU_GSIV: u32 = 0x17;
    pub const PMU_GSIV_INT_INDEX: u32 = PMU_GSIV - 16;

    pub const GIC_PHANDLE: u32 = 1;
    pub const GIC_PPI: u32 = 1;
    pub const IRQ_TYPE_EDGE_FALLING: u32 = 2;
    pub const IRQ_TYPE_LEVEL_LOW: u32 = 8;
    pub const IRQ_TYPE_LEVEL_HIGH: u32 = 4;
}

#[derive(Debug)]
pub enum DtError {
    // Field is stored solely for logging via debug, not actually dead.
    Fdt(#[expect(dead_code)] fdt::builder::Error),
}

impl From<fdt::builder::Error> for DtError {
    fn from(err: fdt::builder::Error) -> Self {
        DtError::Fdt(err)
    }
}

macro_rules! format_fixed {
    ($n:expr, $($arg:tt)*) => {
        {
            let mut buf = ArrayString::<$n>::new();
            fmt::write(&mut buf, format_args!($($arg)*)).unwrap();
            buf
        }
    };
}

pub struct BootTimes {
    pub start: u64,
    pub end: u64,
}

/// Info needed about the current device tree being built to add the vmbus node.
#[derive(Clone, Copy)]
pub struct VmbusDeviceTreeInfo {
    p_address_cells: StringId,
    p_size_cells: StringId,
    p_compatible: StringId,
    p_ranges: StringId,
    p_vtl: StringId,
    p_vmbus_connection_id: StringId,
    p_dma_coherent: StringId,
    p_interrupt_parent: StringId,
    p_interrupts: StringId,
    interrupt_cell_value: Option<u32>,
}

/// Write a vmbus node to the device tree.
fn write_vmbus<'a, T>(
    parent: Builder<'a, T>,
    name: &str,
    vtl: Vtl,
    vmbus: &VmbusInfo,
    dt: VmbusDeviceTreeInfo,
) -> Result<Builder<'a, T>, DtError> {
    let VmbusDeviceTreeInfo {
        p_address_cells,
        p_size_cells,
        p_compatible,
        p_ranges,
        p_vtl,
        p_vmbus_connection_id,
        p_dma_coherent,
        p_interrupt_parent,
        p_interrupts,
        interrupt_cell_value,
    } = dt;

    let mut vmbus_builder = parent
        .start_node(name)?
        .add_u32(p_address_cells, 2)?
        .add_u32(p_size_cells, 2)?
        .add_null(p_dma_coherent)?
        .add_str(p_compatible, "microsoft,vmbus")?
        .add_u32(p_vtl, u8::from(vtl).into())?
        .add_u32(p_vmbus_connection_id, vmbus.connection_id)?;

    let mut mmio_ranges = ArrayVec::<u64, 6>::new();
    for entry in vmbus.mmio.iter() {
        mmio_ranges
            .try_extend_from_slice(&[entry.start(), entry.start(), entry.len()])
            .expect("should always fit");
    }
    vmbus_builder = vmbus_builder.add_u64_array(p_ranges, mmio_ranges.as_slice())?;

    if cfg!(target_arch = "aarch64") {
        vmbus_builder = vmbus_builder
            .add_u32(p_interrupt_parent, aarch64::GIC_PHANDLE)?
            .add_u32_array(
                p_interrupts,
                // Here 3 parameters are used as the "#interrupt-cells"
                // above specifies.
                &[
                    aarch64::GIC_PPI,
                    aarch64::VMBUS_INTID,
                    interrupt_cell_value.expect("must be set on aarch64"),
                ],
            )?;
    }

    Ok(vmbus_builder.end_node()?)
}

/// Writes the device tree blob into `buffer`.
pub fn write_dt(
    buffer: &mut [u8],
    partition_info: &PartitionInfo,
    address_space: &AddressSpaceManager,
    accepted_ranges: impl IntoIterator<Item = MemoryRange>,
    initrd: Range<u64>,
    cmdline: &ArrayString<COMMAND_LINE_SIZE>,
    sidecar: Option<&SidecarConfig<'_>>,
    boot_times: Option<BootTimes>,
    #[cfg_attr(target_arch = "aarch64", expect(unused_variables))]
    // isolation_type is unused on aarch64
    isolation_type: IsolationType,
) -> Result<(), DtError> {
    // First, the reservation map is built. That keyes off of the x86 E820 memory map.
    // The `/memreserve/` is used to tell the kernel that the reserved memory is RAM
    // but it is reserved. That way the kernel allows mapping it via `/dev/mem` without
    // inhibiting the cache thus disabling the unaligned access on some architectures.

    let mut memory_reservations =
        off_stack!(ArrayVec<fdt::ReserveEntry, MAX_RESERVED_MEM_RANGES>, ArrayVec::new_const());

    memory_reservations.extend(address_space.reserved_vtl2_ranges().map(|(r, _)| {
        fdt::ReserveEntry {
            address: r.start().into(),
            size: r.len().into(),
        }
    }));

    // Build the actual device tree.
    let builder_config = fdt::builder::BuilderConfig {
        blob_buffer: buffer,
        string_table_cap: 1024,
        memory_reservations: &memory_reservations,
    };
    let mut builder = Builder::new(builder_config)?;

    // These StringIds are common across many nodes.
    let p_address_cells = builder.add_string("#address-cells")?;
    let p_size_cells = builder.add_string("#size-cells")?;
    let p_reg = builder.add_string("reg")?;
    let p_reg_names = builder.add_string("reg-names")?;
    let p_device_type = builder.add_string("device_type")?;
    let p_status = builder.add_string("status")?;
    let p_compatible = builder.add_string("compatible")?;
    let p_ranges = builder.add_string("ranges")?;
    let p_numa_node_id = builder.add_string("numa-node-id")?;
    let p_reftime_boot_start = builder.add_string("reftime_boot_start")?;
    let p_reftime_boot_end = builder.add_string("reftime_boot_end")?;
    let p_reftime_sidecar_start = builder.add_string("reftime_sidecar_start")?;
    let p_reftime_sidecar_end = builder.add_string("reftime_sidecar_end")?;
    let p_vtl = builder.add_string(igvm_defs::dt::IGVM_DT_VTL_PROPERTY)?;
    let p_vmbus_connection_id = builder.add_string("microsoft,message-connection-id")?;
    let p_dma_coherent = builder.add_string("dma-coherent")?;
    let p_igvm_type = builder.add_string(IGVM_DT_IGVM_TYPE_PROPERTY)?;
    let p_openhcl_memory = builder.add_string("openhcl,memory-type")?;

    // These StringIds are used across multiple AArch64 nodes.
    //
    // TODO: If we add support for an associative map based add_string/add_prop
    // interface to the fdt builder, these explicit definitions would go away.
    // That would require either alloc support, or an alloc-free associative
    // datastructure.
    let p_interrupt_parent = builder.add_string("interrupt-parent")?;
    let p_interrupts = builder.add_string("interrupts")?;
    let p_enable_method = builder.add_string("enable-method")?;

    let num_cpus = partition_info.cpus.len();

    let mut root_builder = builder
        .start_node("")?
        .add_u32(p_address_cells, 2)?
        .add_u32(p_size_cells, 2)?
        .add_str(p_compatible, "microsoft,openvmm")?;

    if let Some(boot_times) = boot_times {
        let BootTimes { start, end } = boot_times;
        root_builder = root_builder
            .add_u64(p_reftime_boot_start, start)?
            .add_u64(p_reftime_boot_end, end)?;
    }

    if let Some(sidecar) = sidecar {
        root_builder = root_builder
            .add_u64(p_reftime_sidecar_start, sidecar.start_reftime)?
            .add_u64(p_reftime_sidecar_end, sidecar.end_reftime)?;
    }

    let hypervisor_builder = root_builder
        .start_node("hypervisor")?
        .add_str(p_compatible, "microsoft,hyperv")?;
    root_builder = hypervisor_builder.end_node()?;

    #[cfg(target_arch = "x86_64")]
    if isolation_type == IsolationType::Tdx {
        let mut mailbox_builder = root_builder
            .start_node("reserved-memory")?
            .add_u32(p_address_cells, 2)?
            .add_u32(p_size_cells, 1)?
            .add_null(p_ranges)?;

        let name = format_fixed!(32, "wakeup_table@{:x}", RESET_VECTOR_PAGE);
        let mailbox_addr_builder = mailbox_builder
            .start_node(name.as_ref())?
            .add_str(p_compatible, "intel,wakeup-mailbox")?
            .add_u32_array(p_reg, &[0x0, RESET_VECTOR_PAGE.try_into().unwrap(), 0x1000])?;

        mailbox_builder = mailbox_addr_builder.end_node()?;

        root_builder = mailbox_builder.end_node()?;
    }

    // For ARM v8, always specify two register cells, which can accommodate
    // higher number of VPs.
    let address_cells = if cfg!(target_arch = "aarch64") { 2 } else { 1 };
    let mut cpu_builder = root_builder
        .start_node("cpus")?
        .add_u32(p_address_cells, address_cells)?
        .add_u32(p_size_cells, 0)?;

    // Add a CPU node for each cpu.
    for (vp_index, cpu_entry) in partition_info.cpus.iter().enumerate() {
        let name = format_fixed!(32, "cpu@{}", vp_index + 1);

        let mut cpu = cpu_builder
            .start_node(name.as_ref())?
            .add_str(p_device_type, "cpu")?
            .add_u32(p_numa_node_id, cpu_entry.vnode)?;

        if cfg!(target_arch = "aarch64") {
            cpu = cpu
                .add_u64(p_reg, cpu_entry.reg)?
                .add_str(p_compatible, "arm,arm-v8")?;

            if num_cpus > 1 {
                cpu = cpu.add_str(p_enable_method, "psci")?;
            }

            if vp_index == 0 {
                cpu = cpu.add_str(p_status, "okay")?;
            } else {
                cpu = cpu.add_str(p_status, "disabled")?;
            }
        } else {
            cpu = cpu
                .add_u32(p_reg, cpu_entry.reg as u32)?
                .add_str(p_status, "okay")?;
        }

        cpu_builder = cpu.end_node()?;
    }
    root_builder = cpu_builder.end_node()?;

    if cfg!(target_arch = "aarch64") {
        let p_method = root_builder.add_string("method")?;
        let p_cpu_off = root_builder.add_string("cpu_off")?;
        let p_cpu_on = root_builder.add_string("cpu_on")?;
        let psci = root_builder
            .start_node("psci")?
            .add_str(p_compatible, "arm,psci-0.2")?
            .add_str(p_method, "hvc")?
            .add_u32(p_cpu_off, 1)?
            .add_u32(p_cpu_on, 2)?;
        root_builder = psci.end_node()?;
    }

    // Add a memory node for each VTL2 range.
    for mem_entry in partition_info.vtl2_ram.iter() {
        let name = format_fixed!(32, "memory@{:x}", mem_entry.range.start());
        let mut mem = root_builder.start_node(&name)?;
        mem = mem.add_str(p_device_type, "memory")?;
        mem = mem.add_u64_array(p_reg, &[mem_entry.range.start(), mem_entry.range.len()])?;
        mem = mem.add_u32(p_numa_node_id, mem_entry.vnode)?;
        root_builder = mem.end_node()?;
    }

    if cfg!(target_arch = "aarch64") {
        // ARM64 Generic Interrupt Controller aka GIC, v3.

        // Use legacy Hyper-V defaults if not specified in the host device tree.
        let default = GicInfo {
            gic_distributor_base: aarch64::DEFAULT_GIC_DISTRIBUTOR_BASE,
            gic_distributor_size: aarch64defs::GIC_DISTRIBUTOR_SIZE,
            gic_redistributors_base: aarch64::DEFAULT_GIC_REDISTRIBUTORS_BASE,
            gic_redistributors_size: aarch64defs::GIC_REDISTRIBUTOR_SIZE * num_cpus as u64,
            gic_redistributor_stride: aarch64defs::GIC_REDISTRIBUTOR_SIZE,
        };
        let gic = partition_info.gic.as_ref().unwrap_or(&default);

        // Validate sizes
        assert_eq!(gic.gic_distributor_size, default.gic_distributor_size);
        assert_eq!(gic.gic_redistributors_size, default.gic_redistributors_size);
        assert_eq!(
            gic.gic_redistributor_stride,
            default.gic_redistributor_stride
        );

        let p_interrupt_cells = root_builder.add_string("#interrupt-cells")?;
        let p_redist_regions = root_builder.add_string("#redistributor-regions")?;
        let p_redist_stride = root_builder.add_string("redistributor-stride")?;
        let p_interrupt_controller = root_builder.add_string("interrupt-controller")?;
        let p_phandle = root_builder.add_string("phandle")?;
        let p_interrupt_names = root_builder.add_string("interrupt-names")?;
        let p_always_on = root_builder.add_string("always-on")?;
        let name = format_fixed!(32, "intc@{}", gic.gic_distributor_base);
        let gicv3 = root_builder
            .start_node(name.as_ref())?
            .add_str(p_compatible, "arm,gic-v3")?
            .add_u32(p_redist_regions, 1)?
            .add_u64(p_redist_stride, gic.gic_redistributor_stride)?
            .add_u64_array(
                p_reg,
                &[
                    gic.gic_distributor_base,
                    gic.gic_distributor_size,
                    gic.gic_redistributors_base,
                    gic.gic_redistributors_size,
                ],
            )?
            .add_u32(p_address_cells, 2)?
            .add_u32(p_size_cells, 2)?
            .add_u32(p_interrupt_cells, 3)?
            .add_null(p_interrupt_controller)?
            .add_u32(p_phandle, aarch64::GIC_PHANDLE)?
            .add_null(p_ranges)?;
        root_builder = gicv3.end_node()?;

        // ARM64 Architectural Timer.
        let timer = root_builder
            .start_node("timer")?
            .add_str(p_compatible, "arm,armv8-timer")?
            .add_u32(p_interrupt_parent, aarch64::GIC_PHANDLE)?
            .add_str(p_interrupt_names, "virt")?
            .add_u32_array(
                p_interrupts,
                // Here 3 parameters are used as the "#interrupt-cells"
                // above specifies. The only interrupt employed is
                // the one for the virtualized environment, it is a
                // Private Peripheral Interrupt.
                &[
                    aarch64::GIC_PPI,
                    aarch64::TIMER_INTID,
                    aarch64::IRQ_TYPE_LEVEL_LOW,
                ],
            )?
            .add_null(p_always_on)?;
        root_builder = timer.end_node()?;

        // Add PMU.
        //
        // TODO: This may be insufficient to get perf to work as the kernel
        // prints:
        // `armv8-pmu pmu: hw perfevents: no irqs for PMU, sampling events not supported`
        //
        // Tracked by issue 1808.
        //
        // NOTE: The host may not provide this value in device tree, so use the
        // Hyper-V platform default if not provided.
        let pmu_gsiv_index = partition_info
            .pmu_gsiv
            .map(|gsiv| {
                assert!(
                    (16..32).contains(&gsiv),
                    "PMU GSIV must be a PPI in [16, 32) range"
                );
                gsiv - 16
            })
            .unwrap_or(aarch64::PMU_GSIV_INT_INDEX);
        let pmu = root_builder
            .start_node("pmu")?
            .add_str(p_compatible, "arm,armv8-pmuv3")?
            .add_u32_array(
                p_interrupts,
                &[
                    aarch64::GIC_PPI,
                    pmu_gsiv_index,
                    aarch64::IRQ_TYPE_LEVEL_HIGH,
                ],
            )?;
        root_builder = pmu.end_node()?;
    }

    // Linux requires vmbus to be under a simple-bus node.
    let mut simple_bus_builder = root_builder
        .start_node("bus")?
        .add_str(p_compatible, "simple-bus")?
        .add_u32(p_address_cells, 2)?
        .add_u32(p_size_cells, 2)?;
    simple_bus_builder = simple_bus_builder.add_prop_array(p_ranges, &[])?;

    let vmbus_info = VmbusDeviceTreeInfo {
        p_address_cells,
        p_size_cells,
        p_compatible,
        p_ranges,
        p_vtl,
        p_vmbus_connection_id,
        p_dma_coherent,
        p_interrupt_parent,
        p_interrupts,
        interrupt_cell_value: if cfg!(target_arch = "aarch64") {
            Some(aarch64::IRQ_TYPE_EDGE_FALLING)
        } else {
            None
        },
    };

    simple_bus_builder = write_vmbus(
        simple_bus_builder,
        "vmbus",
        Vtl::Vtl2,
        &partition_info.vmbus_vtl2,
        vmbus_info,
    )?;

    if let Some(sidecar) = sidecar {
        for node in sidecar.nodes {
            let name = format_fixed!(64, "sidecar@{:x}", node.control_page);
            simple_bus_builder = simple_bus_builder
                .start_node(&name)?
                .add_str(p_compatible, "microsoft,openhcl-sidecar")?
                .add_u64_array(
                    p_reg,
                    &[
                        node.control_page,
                        sidecar_defs::PAGE_SIZE as u64,
                        node.shmem_pages_base,
                        node.shmem_pages_size,
                    ],
                )?
                .add_str_array(p_reg_names, &["ctrl", "shmem"])?
                .end_node()?;
        }
    }

    root_builder = simple_bus_builder.end_node()?;

    if cfg!(target_arch = "aarch64") {
        let p_bootargs = root_builder.add_string("bootargs")?;
        let p_initrd_start = root_builder.add_string("linux,initrd-start")?;
        let p_initrd_end = root_builder.add_string("linux,initrd-end")?;

        let chosen = root_builder
            .start_node("chosen")?
            .add_str(p_bootargs, cmdline.as_str())?
            .add_u64(p_initrd_start, initrd.start)?
            .add_u64(p_initrd_end, initrd.end)?;
        root_builder = chosen.end_node()?;
    }

    // Add information used by openhcl usermode.
    let mut openhcl_builder = root_builder.start_node("openhcl")?;

    let p_isolation_type = openhcl_builder.add_string("isolation-type")?;
    let isolation_type = match partition_info.isolation {
        IsolationType::None => "none",
        IsolationType::Vbs => "vbs",
        IsolationType::Snp => "snp",
        IsolationType::Tdx => "tdx",
    };
    openhcl_builder = openhcl_builder.add_str(p_isolation_type, isolation_type)?;

    // Indicate what kind of memory allocation mode was done by the bootloader
    // to usermode.
    let p_memory_allocation_mode = openhcl_builder.add_string("memory-allocation-mode")?;
    match partition_info.memory_allocation_mode {
        MemoryAllocationMode::Host => {
            openhcl_builder = openhcl_builder.add_str(p_memory_allocation_mode, "host")?;
        }
        MemoryAllocationMode::Vtl2 {
            memory_size,
            mmio_size,
        } => {
            let p_memory_size = openhcl_builder.add_string("memory-size")?;
            let p_mmio_size = openhcl_builder.add_string("mmio-size")?;
            openhcl_builder = openhcl_builder.add_str(p_memory_allocation_mode, "vtl2")?;
            if let Some(memory_size) = memory_size {
                openhcl_builder = openhcl_builder.add_u64(p_memory_size, memory_size)?;
            }
            if let Some(mmio_size) = mmio_size {
                openhcl_builder = openhcl_builder.add_u64(p_mmio_size, mmio_size)?;
            }
        }
    }

    if let Some(data) = partition_info.vtl0_alias_map {
        let p_vtl0_alias_map = openhcl_builder.add_string("vtl0-alias-map")?;
        openhcl_builder = openhcl_builder.add_u64(p_vtl0_alias_map, data)?;
    }

    // Report the unified memory map to usermode describing which memory is
    // used by what.
    //
    // NOTE: Use a different device type for memory ranges, as the Linux kernel
    // will treat every device tree node with device type as memory, and attempt
    // to parse numa information from it.
    let memory_openhcl_type = "memory-openhcl";
    for (range, result) in walk_ranges(
        partition_info.partition_ram.iter().map(|r| (r.range, r)),
        address_space.vtl2_ranges(),
    ) {
        match result {
            RangeWalkResult::Left(entry) => {
                // This range is usable by VTL0.
                let name = format_fixed!(64, "memory@{:x}", range.start());
                openhcl_builder = openhcl_builder
                    .start_node(&name)?
                    .add_str(p_device_type, memory_openhcl_type)?
                    .add_u64_array(p_reg, &[range.start(), range.len()])?
                    .add_u32(p_numa_node_id, entry.vnode)?
                    .add_u32(p_igvm_type, entry.mem_type.0.into())?
                    .add_u32(p_openhcl_memory, MemoryVtlType::VTL0.0)?
                    .end_node()?;
            }
            RangeWalkResult::Both(partition_entry, vtl2_type) => {
                // This range is in use by VTL2. Indicate that.
                let name = format_fixed!(64, "memory@{:x}", range.start());
                openhcl_builder = openhcl_builder
                    .start_node(&name)?
                    .add_str(p_device_type, memory_openhcl_type)?
                    .add_u64_array(p_reg, &[range.start(), range.len()])?
                    .add_u32(p_numa_node_id, partition_entry.vnode)?
                    .add_u32(p_igvm_type, partition_entry.mem_type.0.into())?
                    .add_u32(p_openhcl_memory, vtl2_type.0)?
                    .end_node()?;
            }
            RangeWalkResult::Right(..) => {
                panic!("vtl2 range {:?} not contained in partition ram", range)
            }
            // Ignore ranges not described in either.
            RangeWalkResult::Neither => {}
        }
    }

    // Add mmio ranges for both VTL0 and VTL2.
    for entry in &partition_info.vmbus_vtl0.mmio {
        let name = format_fixed!(64, "memory@{:x}", entry.start());
        openhcl_builder = openhcl_builder
            .start_node(&name)?
            .add_str(p_device_type, memory_openhcl_type)?
            .add_u64_array(p_reg, &[entry.start(), entry.len()])?
            .add_u32(p_openhcl_memory, MemoryVtlType::VTL0_MMIO.0)?
            .end_node()?;
    }

    for entry in &partition_info.vmbus_vtl2.mmio {
        let name = format_fixed!(64, "memory@{:x}", entry.start());
        openhcl_builder = openhcl_builder
            .start_node(&name)?
            .add_str(p_device_type, memory_openhcl_type)?
            .add_u64_array(p_reg, &[entry.start(), entry.len()])?
            .add_u32(p_openhcl_memory, MemoryVtlType::VTL2_MMIO.0)?
            .end_node()?;
    }

    // Report accepted ranges underhil openhcl node.
    for range in accepted_ranges {
        let name = format_fixed!(64, "accepted-memory@{:x}", range.start());
        openhcl_builder = openhcl_builder
            .start_node(&name)?
            .add_u64_array(p_reg, &[range.start(), range.len()])?
            .end_node()?;
    }

    // Pass through host-provided entropy to the init process for seeding
    // the OpenHCL kernel random number generator
    if let Some(entropy) = &partition_info.entropy {
        openhcl_builder = openhcl_builder
            .start_node("entropy")?
            .add_prop_array(p_reg, &[entropy])?
            .end_node()?;
    }

    let root_builder = openhcl_builder.end_node()?;

    root_builder.end_node()?.build(partition_info.bsp_reg)?;
    Ok(())
}
