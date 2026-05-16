// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use guestmem::GuestMemory;
use loader::importer::Aarch64Register;
use loader::importer::X86Register;
use loader::linux::AcpiConfig;
use loader::linux::CommandLineConfig;
use loader::linux::InitrdAddressType;
use loader::linux::InitrdConfig;
use loader::linux::RegisterConfig;
use loader::linux::ZeroPageConfig;
use openvmm_defs::config::DEFAULT_MMIO_GAPS_AARCH64;
use std::ffi::CString;
use std::io::Seek;
use thiserror::Error;
use vm_loader::Loader;
use vm_topology::memory::MemoryLayout;
use vm_topology::pcie::PcieHostBridge;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::aarch64::Aarch64Topology;
use zerocopy::IntoBytes;

#[derive(Debug, Error)]
#[error("device tree error: {0:?}")]
pub struct DtError(pub fdt::builder::Error);

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to read initrd file")]
    InitRd(#[source] std::io::Error),
    #[error("linux loader error")]
    Loader(#[source] loader::linux::Error),
    #[error("device tree error")]
    Dt(#[source] DtError),
    #[error("failed to write EFI/ACPI tables to guest memory")]
    Efi(#[source] guestmem::GuestMemoryError),
}

struct Aarch64EfiInfo {
    systab_addr: u64,
    mmap_addr: u64,
    mmap_size: u32,
    mmap_desc_size: u32,
    mmap_desc_ver: u32,
}

#[derive(Debug)]
pub struct KernelConfig<'a> {
    pub kernel: &'a std::fs::File,
    pub initrd: &'a Option<std::fs::File>,
    pub cmdline: &'a str,
    pub mem_layout: &'a MemoryLayout,
}

pub struct AcpiTables {
    /// The RDSP. Assumed to be given a whole page.
    pub rdsp: Vec<u8>,
    /// The remaining tables pointed to by the RDSP.
    pub tables: Vec<u8>,
}

#[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
pub fn load_linux_x86(
    cfg: &KernelConfig<'_>,
    gm: &GuestMemory,
    acpi_at_gpa: impl FnOnce(u64) -> AcpiTables,
) -> Result<Vec<X86Register>, Error> {
    const GDT_BASE: u64 = 0x1000;
    const CR3_BASE: u64 = 0x4000;
    const ZERO_PAGE_BASE: u64 = 0x2000;
    const CMDLINE_BASE: u64 = 0x3000;
    const ACPI_BASE: u64 = 0xe0000;

    let kaddr: u64 = 0x100000;
    let mut kernel_file = cfg.kernel;

    let (mut initrd_reader, initrd_size) = if let Some(mut initrd_file) = cfg.initrd.as_ref() {
        initrd_file.rewind().map_err(Error::InitRd)?;
        let size = initrd_file
            .seek(std::io::SeekFrom::End(0))
            .map_err(Error::InitRd)?;
        (Some(initrd_file), size)
    } else {
        (None, 0)
    };
    let initrd_config = initrd_reader.as_mut().map(|r| InitrdConfig {
        initrd_address: InitrdAddressType::AfterKernel,
        initrd: r,
        size: initrd_size,
    });

    let cmdline = CString::new(cfg.cmdline).unwrap();
    let cmdline_config = CommandLineConfig {
        address: CMDLINE_BASE,
        cmdline: &cmdline,
    };

    let register_config = RegisterConfig {
        gdt_address: GDT_BASE,
        page_table_address: CR3_BASE,
    };

    let acpi_tables = acpi_at_gpa(ACPI_BASE);

    // NOTE: The rdsp is given a whole page.
    let acpi_len = acpi_tables.tables.len() + 0x1000;
    let acpi_config = AcpiConfig {
        rdsp_address: ACPI_BASE,
        rdsp: &acpi_tables.rdsp,
        tables_address: ACPI_BASE + 0x1000,
        tables: &acpi_tables.tables,
    };

    let zero_page_config = ZeroPageConfig {
        address: ZERO_PAGE_BASE,
        mem_layout: cfg.mem_layout,
        acpi_base_address: ACPI_BASE,
        acpi_len,
    };

    let mut loader = Loader::new(gm.clone(), cfg.mem_layout, hvdef::Vtl::Vtl0);

    loader::linux::load_x86(
        &mut loader,
        &mut kernel_file,
        kaddr,
        initrd_config,
        cmdline_config,
        zero_page_config,
        acpi_config,
        register_config,
    )
    .map_err(Error::Loader)?;

    Ok(loader.initial_regs())
}

/// Returns the device tree blob.
/// NOTE: if need to use GICv2, then the interrupt level must include flags
/// derived from the number of CPUs for the PPI interrupts.
/// TODO: openvmm's command line should provide a device tree blob, optionally, too.
/// TODO: this is a large function, break it up.
/// TODO: disjoint from the VM configuration, must work key off of the VM configuration.
fn build_dt(
    cfg: &KernelConfig<'_>,
    _gm: &GuestMemory,
    enable_serial: bool,
    processor_topology: &ProcessorTopology<Aarch64Topology>,
    pcie_host_bridges: &[PcieHostBridge],
    initrd_start: u64,
    initrd_end: u64,
) -> Result<Vec<u8>, fdt::builder::Error> {
    // This ID forces the subset of PL011 known as the SBSA UART be used.
    const PL011_PERIPH_ID: u32 = 0x00041011;
    const PL011_BAUD: u32 = 115200;
    const PL011_SERIAL0_BASE: u64 = 0xEFFEC000;
    const PL011_SERIAL0_IRQ: u32 = 1;
    const PL011_SERIAL1_BASE: u64 = 0xEFFEB000;
    const PL011_SERIAL1_IRQ: u32 = 2;

    let num_cpus = processor_topology.vps().len();

    use vm_topology::processor::aarch64::GicMsiController;
    use vm_topology::processor::aarch64::GicVersion;

    let gic_dist_base: u64 = processor_topology.gic_distributor_base();
    let gic_dist_size: u64 = match processor_topology.gic_version() {
        GicVersion::V3 { .. } => aarch64defs::GIC_DISTRIBUTOR_SIZE,
        GicVersion::V2 { .. } => aarch64defs::GIC_V2_DISTRIBUTOR_SIZE,
    };
    let (gic_second_base, gic_second_size) = match processor_topology.gic_version() {
        GicVersion::V3 {
            redistributors_base,
        } => (
            redistributors_base,
            aarch64defs::GIC_REDISTRIBUTOR_SIZE * num_cpus as u64,
        ),
        GicVersion::V2 { cpu_interface_base } => {
            (cpu_interface_base, aarch64defs::GIC_V2_CPU_INTERFACE_SIZE)
        }
    };

    // With the default values, that will overlap with the GIC distributor range
    // if the number of VPs goes above `2048`. That is more than enough for the time being,
    // both for the Linux and the Windows guests. The debug assert below is for the time
    // when custom values are used.
    debug_assert!(
        !(gic_dist_base..gic_dist_base + gic_dist_size).contains(&gic_second_base)
            && !(gic_second_base..gic_second_base + gic_second_size).contains(&gic_dist_base)
    );

    let mut buffer = vec![0u8; 0x200000];

    let builder_config = fdt::builder::BuilderConfig {
        blob_buffer: &mut buffer,
        string_table_cap: 1024,
        memory_reservations: &[],
    };
    let mut builder = fdt::builder::Builder::new(builder_config)?;
    let p_address_cells = builder.add_string("#address-cells")?;
    let p_size_cells = builder.add_string("#size-cells")?;
    let p_model = builder.add_string("model")?;
    let p_reg = builder.add_string("reg")?;
    let p_device_type = builder.add_string("device_type")?;
    let p_status = builder.add_string("status")?;
    let p_compatible = builder.add_string("compatible")?;
    let p_ranges = builder.add_string("ranges")?;
    let p_enable_method = builder.add_string("enable-method")?;
    let p_method = builder.add_string("method")?;
    let p_bootargs = builder.add_string("bootargs")?;
    let p_stdout_path = builder.add_string("stdout-path")?;
    let p_initrd_start = builder.add_string("linux,initrd-start")?;
    let p_initrd_end = builder.add_string("linux,initrd-end")?;
    let p_interrupt_cells = builder.add_string("#interrupt-cells")?;
    let p_interrupt_controller = builder.add_string("interrupt-controller")?;
    let p_interrupt_names = builder.add_string("interrupt-names")?;
    let p_interrupts = builder.add_string("interrupts")?;
    let p_interrupt_parent = builder.add_string("interrupt-parent")?;
    let p_always_on = builder.add_string("always-on")?;
    let p_phandle = builder.add_string("phandle")?;
    let p_clock_frequency = builder.add_string("clock-frequency")?;
    let p_clock_output_names = builder.add_string("clock-output-names")?;
    let p_clock_cells = builder.add_string("#clock-cells")?;
    let p_clocks = builder.add_string("clocks")?;
    let p_clock_names = builder.add_string("clock-names")?;
    let p_current_speed = builder.add_string("current-speed")?;
    let p_arm_periph_id = builder.add_string("arm,primecell-periphid")?;
    let p_dma_coherent = builder.add_string("dma-coherent")?;
    let p_bus_range = builder.add_string("bus-range")?;
    let p_linux_pci_domain = builder.add_string("linux,pci-domain")?;
    let p_msi_parent = builder.add_string("msi-parent")?;
    let p_msi_controller = builder.add_string("msi-controller")?;
    let p_arm_msi_base_spi = builder.add_string("arm,msi-base-spi")?;
    let p_arm_msi_num_spis = builder.add_string("arm,msi-num-spis")?;

    // Property handle values.
    const PHANDLE_GIC: u32 = 1;
    const PHANDLE_APB_PCLK: u32 = 2;
    const PHANDLE_V2M: u32 = 3;
    const PHANDLE_ITS: u32 = 4;

    const GIC_SPI: u32 = 0;
    const GIC_PPI: u32 = 1;
    const IRQ_TYPE_LEVEL_LOW: u32 = 8;
    const IRQ_TYPE_LEVEL_HIGH: u32 = 4;
    const IRQ_TYPE_EDGE_RISING: u32 = 1;
    /// VMBus PPI offset for the DT `interrupts` property.
    const VMBUS_PPI_OFFSET: u32 = openvmm_defs::config::DEFAULT_VMBUS_PPI - 16;

    let mut root_builder = builder
        .start_node("")?
        .add_u32(p_address_cells, 2)?
        .add_u32(p_size_cells, 2)?
        .add_u32(p_interrupt_parent, PHANDLE_GIC)?
        .add_str(p_model, "microsoft,openvmm")?
        .add_str(p_compatible, "microsoft,openvmm")?;

    let mut cpu_builder = root_builder
        .start_node("cpus")?
        .add_str(p_compatible, "arm,armv8")?
        .add_u32(p_address_cells, 1)?
        .add_u32(p_size_cells, 0)?;

    // Add a CPU node for each cpu.
    for vp_index in 0..num_cpus {
        let name = format!("cpu@{}", vp_index);
        let mut cpu = cpu_builder
            .start_node(name.as_ref())?
            .add_u32(p_reg, vp_index as u32)?
            .add_str(p_device_type, "cpu")?;

        if num_cpus > 1 {
            cpu = cpu.add_str(p_enable_method, "psci")?;
        }

        if vp_index == 0 {
            cpu = cpu.add_str(p_status, "okay")?;
        } else {
            cpu = cpu.add_str(p_status, "disabled")?;
        }

        cpu_builder = cpu.end_node()?;
    }
    root_builder = cpu_builder.end_node()?;

    let psci = root_builder
        .start_node("psci")?
        .add_str(p_compatible, "arm,psci-0.2")?
        .add_str(p_method, "hvc")?;
    root_builder = psci.end_node()?;

    // Add a memory node for each RAM range.
    for mem_entry in cfg.mem_layout.ram() {
        let start = mem_entry.range.start();
        let len = mem_entry.range.len();
        let name = format!("memory@{:x}", start);
        let mut mem = root_builder.start_node(&name)?;
        mem = mem.add_str(p_device_type, "memory")?;
        mem = mem.add_u64_array(p_reg, &[start, len])?;
        root_builder = mem.end_node()?;
    }

    // Advanced Bus Peripheral Clock.
    root_builder = root_builder
        .start_node("apb-pclk")?
        .add_str(p_compatible, "fixed-clock")?
        .add_u32(p_clock_frequency, 24000000)?
        .add_str_array(p_clock_output_names, &["clk24mhz"])?
        .add_u32(p_clock_cells, 0)?
        .add_u32(p_phandle, PHANDLE_APB_PCLK)?
        .end_node()?;

    // ARM64 Generic Interrupt Controller.
    // GICv3 uses "arm,gic-v3"; GICv2 uses "arm,cortex-a15-gic".
    // GICv3 can have an ITS child for LPI-based MSIs; v2m is the
    // fallback for SPI-based MSIs (GICv2 or GICv3 without ITS).
    let gic_msi = processor_topology.gic_msi();
    let gic_compatible = match processor_topology.gic_version() {
        GicVersion::V3 { .. } => "arm,gic-v3",
        GicVersion::V2 { .. } => "arm,cortex-a15-gic",
    };
    let gic_node = root_builder
        .start_node(format!("intc@{gic_dist_base:x}").as_str())?
        .add_str(p_compatible, gic_compatible)?
        .add_u64_array(
            p_reg,
            &[
                gic_dist_base,
                gic_dist_size,
                gic_second_base,
                gic_second_size,
            ],
        )?
        .add_u32(p_address_cells, 2)?
        .add_u32(p_size_cells, 2)?
        .add_u32(p_interrupt_cells, 3)?
        .add_null(p_interrupt_controller)?
        .add_u32(p_phandle, PHANDLE_GIC)?
        .add_null(p_ranges)?;
    root_builder = match gic_msi {
        GicMsiController::Its(its) => gic_node
            .start_node(format!("its@{:x}", its.its_base).as_str())?
            .add_str(p_compatible, "arm,gic-v3-its")?
            .add_null(p_msi_controller)?
            .add_u64_array(p_reg, &[its.its_base, openvmm_defs::config::GIC_ITS_SIZE])?
            .add_u32(p_phandle, PHANDLE_ITS)?
            .end_node()?
            .end_node()?,
        GicMsiController::V2m(v2m) => gic_node
            .start_node(format!("v2m@{:x}", v2m.frame_base).as_str())?
            .add_str(p_compatible, "arm,gic-v2m-frame")?
            .add_null(p_msi_controller)?
            .add_u64_array(
                p_reg,
                &[v2m.frame_base, openvmm_defs::config::GIC_V2M_MSI_FRAME_SIZE],
            )?
            .add_u32(p_arm_msi_base_spi, v2m.spi_base)?
            .add_u32(p_arm_msi_num_spis, v2m.spi_count)?
            .add_u32(p_phandle, PHANDLE_V2M)?
            .end_node()?
            .end_node()?,
        GicMsiController::None => gic_node.end_node()?,
    };

    // ARM64 Architectural Timer.
    // The DT `interrupts` property uses the PPI offset (INTID - 16).
    assert!((16..32).contains(&processor_topology.virt_timer_ppi()));
    let virt_timer_ppi_offset = processor_topology.virt_timer_ppi() - 16;
    let timer = root_builder
        .start_node("timer")?
        .add_str(p_compatible, "arm,armv8-timer")?
        .add_u32(p_interrupt_parent, PHANDLE_GIC)?
        .add_str(p_interrupt_names, "virt")?
        .add_u32_array(
            p_interrupts,
            &[GIC_PPI, virt_timer_ppi_offset, IRQ_TYPE_LEVEL_LOW],
        )?
        .add_null(p_always_on)?;
    root_builder = timer.end_node()?;

    // Add PMU, if the interrupt is configured.
    if let Some(pmu_gsiv) = processor_topology.pmu_gsiv() {
        assert!((16..32).contains(&pmu_gsiv));
        let ppi_index = pmu_gsiv - 16;
        let pmu = root_builder
            .start_node("pmu")?
            .add_str(p_compatible, "arm,armv8-pmuv3")?
            .add_u32_array(p_interrupts, &[GIC_PPI, ppi_index, IRQ_TYPE_LEVEL_HIGH])?;
        root_builder = pmu.end_node()?;
    }

    // Add a PCIe host bridge node for each bridge.
    // PCI address space type bits (phys.hi bits 25:24).
    const PCI_SPACE_MEM32: u32 = 0x02000000; // 32-bit non-prefetchable MMIO
    const PCI_SPACE_MEM64: u32 = 0x03000000; // 64-bit prefetchable MMIO

    for bridge in pcie_host_bridges {
        let name = format!("pcie@{:x}", bridge.ecam_range.start());

        // The `ranges` property encodes translations from PCI MMIO address
        // space to CPU physical address space.  Each entry is 7 cells:
        //   [pci-phys.hi, pci-phys.mid, pci-phys.lo,
        //    cpu-phys.hi, cpu-phys.lo,
        //    size.hi, size.lo]
        let mut ranges: Vec<u32> = Vec::new();

        let low_start = bridge.low_mmio.start();
        let low_len = bridge.low_mmio.len();
        if low_len > 0 {
            ranges.extend_from_slice(&[
                PCI_SPACE_MEM32,
                0,
                low_start as u32,
                (low_start >> 32) as u32,
                (low_start & 0xFFFF_FFFF) as u32,
                (low_len >> 32) as u32,
                (low_len & 0xFFFF_FFFF) as u32,
            ]);
        }

        let high_start = bridge.high_mmio.start();
        let high_len = bridge.high_mmio.len();
        if high_len > 0 {
            ranges.extend_from_slice(&[
                PCI_SPACE_MEM64,
                (high_start >> 32) as u32,
                (high_start & 0xFFFF_FFFF) as u32,
                (high_start >> 32) as u32,
                (high_start & 0xFFFF_FFFF) as u32,
                (high_len >> 32) as u32,
                (high_len & 0xFFFF_FFFF) as u32,
            ]);
        }

        // No interrupt-map is provided because all devices use MSIs via the
        // ITS or v2m frame; legacy INTx routing is not supported.
        let mut node = root_builder
            .start_node(name.as_str())?
            .add_str(p_compatible, "pci-host-ecam-generic")?
            .add_str(p_device_type, "pci")?
            .add_u32(p_linux_pci_domain, bridge.segment as u32)?
            .add_u64_array(p_reg, &[bridge.ecam_range.start(), bridge.ecam_range.len()])?
            .add_u32_array(
                p_bus_range,
                &[bridge.start_bus as u32, bridge.end_bus as u32],
            )?
            .add_u32(p_address_cells, 3)?
            .add_u32(p_size_cells, 2)?
            .add_u32(p_interrupt_parent, PHANDLE_GIC)?
            .add_u32_array(p_ranges, &ranges)?;
        match gic_msi {
            GicMsiController::Its(_) => {
                node = node.add_u32(p_msi_parent, PHANDLE_ITS)?;
            }
            GicMsiController::V2m(_) => {
                node = node.add_u32(p_msi_parent, PHANDLE_V2M)?;
            }
            GicMsiController::None => {}
        }
        root_builder = node.end_node()?;
    }

    let mut soc = root_builder
        .start_node("openvmm")?
        .add_str(p_compatible, "simple-bus")?
        .add_u32(p_address_cells, 2)?
        .add_u32(p_size_cells, 2)?
        .add_null(p_ranges)?
        .add_u32(p_interrupt_parent, PHANDLE_GIC)?;

    if enable_serial {
        // Uses the scoped down "arm,sbsa-aurt" rather than the full "arm,pl011" device.
        for (serial_base, serial_interrupt) in [
            (PL011_SERIAL0_BASE, PL011_SERIAL0_IRQ),
            (PL011_SERIAL1_BASE, PL011_SERIAL1_IRQ),
        ] {
            let name = format!("uart@{:x}", serial_base);
            soc = soc
                .start_node(name.as_ref())?
                .add_str_array(p_compatible, &["arm,sbsa-uart", "arm,primecell"])?
                .add_str_array(p_clock_names, &["apb_pclk"])?
                .add_u32(p_clocks, PHANDLE_APB_PCLK)?
                .add_u32(p_interrupt_parent, PHANDLE_GIC)?
                .add_u64_array(p_reg, &[serial_base, 0x1000])?
                .add_u32(p_current_speed, PL011_BAUD)?
                .add_u32(p_arm_periph_id, PL011_PERIPH_ID)?
                .add_u32_array(
                    p_interrupts,
                    &[GIC_SPI, serial_interrupt, IRQ_TYPE_LEVEL_HIGH],
                )?
                .add_str(p_status, "okay")?
                .end_node()?;
        }
    }

    assert!(DEFAULT_MMIO_GAPS_AARCH64.len() == 2);
    let low_mmio_gap = DEFAULT_MMIO_GAPS_AARCH64[0];
    let high_mmio_gap = DEFAULT_MMIO_GAPS_AARCH64[1];
    soc = soc
        .start_node("vmbus")?
        .add_u32(p_address_cells, 2)?
        .add_u32(p_size_cells, 2)?
        .add_null(p_dma_coherent)?
        .add_u64_array(
            p_ranges,
            &[
                low_mmio_gap.start(),
                low_mmio_gap.len(),
                high_mmio_gap.start(),
                high_mmio_gap.len(),
            ],
        )?
        .add_str(p_compatible, "microsoft,vmbus")?
        .add_u32(p_interrupt_parent, PHANDLE_GIC)?
        .add_u32_array(
            p_interrupts,
            // Here 3 parameters are used as the "#interrupt-cells"
            // above specifies.
            &[GIC_PPI, VMBUS_PPI_OFFSET, IRQ_TYPE_EDGE_RISING],
        )?
        .end_node()?;

    root_builder = soc.end_node()?;

    let mut chosen = root_builder
        .start_node("chosen")?
        .add_str(p_bootargs, cfg.cmdline)?;
    chosen = chosen.add_u64(p_initrd_start, initrd_start)?;
    chosen = chosen.add_u64(p_initrd_end, initrd_end)?;
    if enable_serial {
        chosen = chosen.add_str(
            p_stdout_path,
            format!("/hvlite/uart@{PL011_SERIAL0_BASE:x}").as_str(),
        )?;
    }

    root_builder = chosen.end_node()?;

    let boot_cpu_id = 0;
    let dt_size = root_builder.end_node()?.build(boot_cpu_id)?;
    buffer.truncate(dt_size);

    Ok(buffer)
}

/// Write synthesized EFI and ACPI structures into guest memory.
///
/// On ARM64, the Linux kernel can discover devices via ACPI instead of a
/// device tree, but it still needs to enter via the EFI stub to find the
/// RSDP. We synthesize:
///   - An `EFI_SYSTEM_TABLE` pointing to an ACPI 2.0 configuration table
///     entry (the RSDP) and an RT Properties table (advertising no runtime
///     services).
///   - An EFI memory map describing the metadata, ACPI tables, and
///     conventional RAM regions.
///   - The ACPI tables themselves (RSDP, XSDT, FADT, MADT, GTDT, DSDT, etc.).
///
/// The companion [`build_stub_dt`] function then builds a minimal device tree
/// whose `/chosen` node carries `linux,uefi-system-table` and the memory map
/// pointers so that the kernel's EFI stub can locate these structures.
fn write_efi_and_acpi_tables(
    gm: &GuestMemory,
    efi_base: u64,
    rsdp_addr: u64,
    mem_layout: &MemoryLayout,
    acpi_tables: &vmm_core::acpi_builder::BuiltAcpiTables,
) -> Result<Aarch64EfiInfo, Error> {
    use memory_range::MemoryRange;
    use uefi_specs::uefi::boot::ACPI_20_TABLE_GUID;
    use uefi_specs::uefi::boot::EFI_2_70_SYSTEM_TABLE_REVISION;
    use uefi_specs::uefi::boot::EFI_MEMORY_DESCRIPTOR_VERSION;
    use uefi_specs::uefi::boot::EFI_MEMORY_WB;
    use uefi_specs::uefi::boot::EFI_RT_PROPERTIES_TABLE_GUID;
    use uefi_specs::uefi::boot::EFI_SYSTEM_TABLE_SIGNATURE;
    use uefi_specs::uefi::boot::EfiMemoryDescriptor;
    use uefi_specs::uefi::boot::EfiMemoryType;
    use uefi_specs::uefi::boot::EfiRtPropertiesTable;
    use uefi_specs::uefi::boot::EfiSystemTable;

    // Helper to align a value up to the given power-of-two alignment.
    fn align_up(val: u64, align: u64) -> u64 {
        (val + align - 1) & !(align - 1)
    }

    // --- ACPI tables ---
    let tables_addr = rsdp_addr + 0x1000;
    gm.write_at(rsdp_addr, &acpi_tables.rdsp)
        .map_err(Error::Efi)?;
    gm.write_at(tables_addr, &acpi_tables.tables)
        .map_err(Error::Efi)?;

    // --- EFI metadata (page 1): systab, config table, vendor, rt props ---
    // Page 0 is reserved for the memory map (written last).
    let mut cursor = efi_base + 0x1000;

    // EFI System Table
    let systab_addr = cursor;
    cursor += size_of::<EfiSystemTable>() as u64;

    // Configuration table entries (24 bytes each: 16-byte GUID + 8-byte pointer)
    const CONFIG_ENTRY_SIZE: u64 = 24;
    let num_config_entries: u64 = 2;
    let config_table_addr = cursor;
    cursor += num_config_entries * CONFIG_ENTRY_SIZE;

    // Firmware vendor string — NUL-terminated UTF-16LE
    let fw_vendor_addr = cursor;
    let fw_vendor: Vec<u8> = "OpenVMM\0"
        .encode_utf16()
        .flat_map(|c| c.to_le_bytes())
        .collect();
    cursor += fw_vendor.len() as u64;
    cursor = align_up(cursor, 8);

    // EFI RT Properties Table — tells the OS no runtime services are available.
    let rt_props_addr = cursor;
    let rt_props = EfiRtPropertiesTable::NONE_SUPPORTED;
    cursor += size_of::<EfiRtPropertiesTable>() as u64;

    // Compute how many pages the metadata region spans.
    let metadata_end = align_up(cursor, 0x1000);
    let metadata_pages = (metadata_end - efi_base) / 0x1000;
    assert!(
        cursor <= rsdp_addr,
        "EFI metadata ({cursor:#x}) overflows into ACPI tables region ({rsdp_addr:#x})",
    );

    // Now write everything.
    gm.write_at(rt_props_addr, rt_props.as_bytes())
        .map_err(Error::Efi)?;

    let mut config_entries = [0u8; 48];
    config_entries[0..16].copy_from_slice(ACPI_20_TABLE_GUID.as_bytes());
    config_entries[16..24].copy_from_slice(&rsdp_addr.to_le_bytes());
    config_entries[24..40].copy_from_slice(EFI_RT_PROPERTIES_TABLE_GUID.as_bytes());
    config_entries[40..48].copy_from_slice(&rt_props_addr.to_le_bytes());
    gm.write_at(config_table_addr, &config_entries)
        .map_err(Error::Efi)?;

    gm.write_at(fw_vendor_addr, &fw_vendor)
        .map_err(Error::Efi)?;

    let mut systab = EfiSystemTable {
        signature: EFI_SYSTEM_TABLE_SIGNATURE,
        revision: EFI_2_70_SYSTEM_TABLE_REVISION,
        header_size: size_of::<EfiSystemTable>() as u32,
        firmware_vendor: fw_vendor_addr,
        firmware_revision: 1,
        number_of_table_entries: num_config_entries,
        configuration_table: config_table_addr,
        ..Default::default()
    };
    // UEFI spec 4.2: CRC32 is computed over header_size bytes with crc32 zeroed.
    systab.crc32 = crc32fast::hash(systab.as_bytes());
    gm.write_at(systab_addr, systab.as_bytes())
        .map_err(Error::Efi)?;

    // --- Memory map (page 0) ---
    let mut mmap_entries: Vec<EfiMemoryDescriptor> = Vec::new();

    // EFI metadata region
    mmap_entries.push(EfiMemoryDescriptor {
        typ: EfiMemoryType::EFI_BOOT_SERVICES_DATA,
        _pad: 0,
        physical_start: efi_base,
        virtual_start: 0,
        number_of_pages: metadata_pages,
        attribute: EFI_MEMORY_WB,
    });

    // ACPI tables region
    let acpi_region_pages = {
        let total = 0x1000 + acpi_tables.tables.len() as u64;
        total.div_ceil(0x1000)
    };
    mmap_entries.push(EfiMemoryDescriptor {
        typ: EfiMemoryType::EFI_ACPI_RECLAIM_MEMORY,
        _pad: 0,
        physical_start: rsdp_addr,
        virtual_start: 0,
        number_of_pages: acpi_region_pages,
        attribute: EFI_MEMORY_WB,
    });

    // Conventional memory — one entry per RAM range, excluding the
    // EFI/ACPI reserved region to avoid overlapping memory map entries.
    let reserved_start = efi_base;
    let reserved_end = align_up(rsdp_addr + 0x1000 + acpi_tables.tables.len() as u64, 0x1000);
    let reserved = [MemoryRange::new(reserved_start..reserved_end)];
    for range in memory_range::subtract_ranges(mem_layout.ram().iter().map(|r| r.range), reserved) {
        mmap_entries.push(EfiMemoryDescriptor {
            typ: EfiMemoryType::EFI_CONVENTIONAL_MEMORY,
            _pad: 0,
            physical_start: range.start(),
            virtual_start: 0,
            number_of_pages: range.len() / 0x1000,
            attribute: EFI_MEMORY_WB,
        });
    }

    let mmap_addr = efi_base;
    let mmap_bytes: Vec<u8> = mmap_entries
        .iter()
        .flat_map(|e| e.as_bytes())
        .copied()
        .collect();
    let mmap_size = mmap_bytes.len() as u32;

    gm.write_at(mmap_addr, &mmap_bytes).map_err(Error::Efi)?;

    Ok(Aarch64EfiInfo {
        systab_addr,
        mmap_addr,
        mmap_size,
        mmap_desc_size: size_of::<EfiMemoryDescriptor>() as u32,
        mmap_desc_ver: EFI_MEMORY_DESCRIPTOR_VERSION,
    })
}

/// Build a "stub" device tree for ACPI-mode ARM64 direct boot.
///
/// Unlike the full device tree built by [`build_dt`], this DT contains no
/// hardware descriptions — no CPU nodes, no GIC, no timer, no devices.
/// Its only purpose is a `/chosen` node that tells the Linux EFI stub
/// where to find the EFI system table and memory map written by
/// [`write_efi_and_acpi_tables`]. The kernel then uses those EFI
/// structures to locate the ACPI RSDP and discovers all hardware through
/// ACPI tables instead of DT nodes.
fn build_stub_dt(
    cmdline: &str,
    initrd_start: u64,
    initrd_end: u64,
    efi_info: &Aarch64EfiInfo,
) -> Result<Vec<u8>, fdt::builder::Error> {
    let mut buffer = vec![0u8; 0x4000];

    let builder_config = fdt::builder::BuilderConfig {
        blob_buffer: &mut buffer,
        string_table_cap: 256,
        memory_reservations: &[],
    };
    let mut builder = fdt::builder::Builder::new(builder_config)?;
    let p_address_cells = builder.add_string("#address-cells")?;
    let p_size_cells = builder.add_string("#size-cells")?;
    let p_bootargs = builder.add_string("bootargs")?;
    let p_initrd_start = builder.add_string("linux,initrd-start")?;
    let p_initrd_end = builder.add_string("linux,initrd-end")?;
    let p_uefi_system_table = builder.add_string("linux,uefi-system-table")?;
    let p_uefi_mmap_start = builder.add_string("linux,uefi-mmap-start")?;
    let p_uefi_mmap_size = builder.add_string("linux,uefi-mmap-size")?;
    let p_uefi_mmap_desc_size = builder.add_string("linux,uefi-mmap-desc-size")?;
    let p_uefi_mmap_desc_ver = builder.add_string("linux,uefi-mmap-desc-ver")?;

    let root_builder = builder
        .start_node("")?
        .add_u32(p_address_cells, 2)?
        .add_u32(p_size_cells, 2)?;

    let chosen = root_builder
        .start_node("chosen")?
        .add_str(p_bootargs, cmdline)?
        .add_u64(p_initrd_start, initrd_start)?
        .add_u64(p_initrd_end, initrd_end)?
        .add_u64(p_uefi_system_table, efi_info.systab_addr)?
        .add_u64(p_uefi_mmap_start, efi_info.mmap_addr)?
        .add_u32(p_uefi_mmap_size, efi_info.mmap_size)?
        .add_u32(p_uefi_mmap_desc_size, efi_info.mmap_desc_size)?
        .add_u32(p_uefi_mmap_desc_ver, efi_info.mmap_desc_ver)?;

    let root_builder = chosen.end_node()?;

    let boot_cpu_id = 0;
    let dt_size = root_builder.end_node()?.build(boot_cpu_id)?;
    buffer.truncate(dt_size);

    Ok(buffer)
}

#[cfg_attr(not(guest_arch = "aarch64"), expect(dead_code))]
pub fn load_linux_arm64(
    cfg: &KernelConfig<'_>,
    gm: &GuestMemory,
    enable_serial: bool,
    processor_topology: &ProcessorTopology<Aarch64Topology>,
    pcie_host_bridges: &[PcieHostBridge],
    build_acpi: Option<impl FnOnce(u64) -> vmm_core::acpi_builder::BuiltAcpiTables>,
) -> Result<Vec<Aarch64Register>, Error> {
    let mut loader = Loader::new(gm.clone(), cfg.mem_layout, hvdef::Vtl::Vtl0);
    let mut kernel_file = cfg.kernel;

    let (mut initrd_reader, initrd_size) = if let Some(mut initrd_file) = cfg.initrd.as_ref() {
        initrd_file.rewind().map_err(Error::InitRd)?;
        let size = initrd_file
            .seek(std::io::SeekFrom::End(0))
            .map_err(Error::InitRd)?;
        (Some(initrd_file), size)
    } else {
        (None, 0)
    };

    // Data dependencies:
    // - DeviceTree carries the start address of the initrd.
    // - The linux loader loads the kernel, the initrd at the said address, and
    //   the device tree into the guest memory.
    //
    // Place the initrd at the bottom of guest memory + 16MB, and set the
    // minimum kernel address above it, aligned to the next 2MB boundary.
    let mem_start = cfg
        .mem_layout
        .ram()
        .first()
        .expect("must be at least one ram range")
        .range
        .start();
    const INITRD_OFFSET: u64 = 16 << 20; // 16 MB
    let initrd_start: u64 = mem_start + INITRD_OFFSET;
    let initrd_end: u64 = initrd_start + initrd_size;
    // Align the kernel to 2MB
    let kernel_minimum_start_address: u64 = (initrd_end + 0x1fffff) & !0x1fffff;

    let device_tree = if let Some(build_acpi) = build_acpi {
        // ACPI mode: write EFI + ACPI tables into guest memory, then build a
        // minimal "stub" DT that points the kernel's EFI stub at them. The
        // kernel discovers all devices through ACPI, not the DT.
        const EFI_OFFSET: u64 = 0x0080_0000; // 8 MB
        const ACPI_TABLES_OFFSET: u64 = 0x2000;
        const { assert!(EFI_OFFSET < INITRD_OFFSET) };
        let rsdp_addr = mem_start + EFI_OFFSET + ACPI_TABLES_OFFSET;
        let acpi_tables = build_acpi(rsdp_addr);
        let efi_info = write_efi_and_acpi_tables(
            gm,
            mem_start + EFI_OFFSET,
            rsdp_addr,
            cfg.mem_layout,
            &acpi_tables,
        )?;
        build_stub_dt(cfg.cmdline, initrd_start, initrd_end, &efi_info)
            .map_err(|e| Error::Dt(DtError(e)))?
    } else {
        build_dt(
            cfg,
            gm,
            enable_serial,
            processor_topology,
            pcie_host_bridges,
            initrd_start,
            initrd_end,
        )
        .map_err(|e| Error::Dt(DtError(e)))?
    };

    let initrd_config = initrd_reader.as_mut().map(|r| InitrdConfig {
        initrd_address: InitrdAddressType::Address(initrd_start),
        initrd: r,
        size: initrd_size,
    });

    let load_info = loader::linux::load_kernel_and_initrd_arm64(
        &mut loader,
        &mut kernel_file,
        kernel_minimum_start_address,
        initrd_config,
        Some(&device_tree),
    )
    .map_err(Error::Loader)?;

    // Set the registers separately so they won't conflict with the UEFI boot when
    // `load_kernel_and_initrd_arm64` is used for VTL2 direct kernel boot.
    loader::linux::set_direct_boot_registers_arm64(&mut loader, &load_info)
        .map_err(Error::Loader)?;

    Ok(loader.initial_regs())
}
