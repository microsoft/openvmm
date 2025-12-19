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
use std::io::Read;
use std::io::Seek;
use thiserror::Error;
use vm_loader::Loader;
use vm_topology::memory::MemoryLayout;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::aarch64::Aarch64Topology;

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

    let mut initrd = Vec::new();
    if let Some(mut initrd_file) = cfg.initrd.as_ref() {
        initrd_file.rewind().map_err(Error::InitRd)?;
        initrd_file
            .read_to_end(&mut initrd)
            .map_err(Error::InitRd)?;
    }

    let initrd_config = InitrdConfig {
        initrd_address: InitrdAddressType::AfterKernel,
        initrd: &initrd,
    };

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
        if !initrd.is_empty() {
            Some(initrd_config)
        } else {
            None
        },
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

    let gic_dist_base: u64 = processor_topology.gic_distributor_base();
    let gic_dist_size: u64 = aarch64defs::GIC_DISTRIBUTOR_SIZE;
    let gic_redist_base: u64 = processor_topology.gic_redistributors_base();
    let gic_redist_size: u64 = aarch64defs::GIC_REDISTRIBUTOR_SIZE * num_cpus as u64;

    // With the default values, that will overlap with the GIC distributor range
    // if the number of VPs goes above `2048`. That is more than enough for the time being,
    // both for the Linux and the Windows guests. The debug assert below is for the time
    // when custom values are used.
    debug_assert!(
        !(gic_dist_base..gic_dist_base + gic_dist_size).contains(&gic_redist_base)
            && !(gic_redist_base..gic_redist_base + gic_redist_size).contains(&gic_dist_base)
    );

    let mut buffer = vec![0u8; hvdef::HV_PAGE_SIZE as usize * 256];

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

    // Property handle values.
    const PHANDLE_GIC: u32 = 1;
    const PHANDLE_APB_PCLK: u32 = 2;

    const GIC_SPI: u32 = 0;
    const GIC_PPI: u32 = 1;
    const IRQ_TYPE_LEVEL_LOW: u32 = 8;
    const IRQ_TYPE_LEVEL_HIGH: u32 = 4;
    const IRQ_TYPE_EDGE_FALLING: u32 = 2;
    const VMBUS_INTID: u32 = 2; // Note: the hardware INTID will be 16 + 2

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

    // ARM64 Generic Interrupt Controller aka GIC, v3.
    let gicv3 = root_builder
        .start_node(format!("intc@{gic_dist_base:x}").as_str())?
        .add_str(p_compatible, "arm,gic-v3")?
        .add_u64_array(
            p_reg,
            &[
                gic_dist_base,
                gic_dist_size,
                gic_redist_base,
                gic_redist_size,
            ],
        )?
        .add_u32(p_address_cells, 2)?
        .add_u32(p_size_cells, 2)?
        .add_u32(p_interrupt_cells, 3)?
        .add_null(p_interrupt_controller)?
        .add_u32(p_phandle, PHANDLE_GIC)?
        .add_null(p_ranges)?;
    root_builder = gicv3.end_node()?;

    // ARM64 Architectural Timer.
    const HYPERV_VIRT_TIMER_PPI: u32 = 4; // relative to PPI base of 16
    let timer = root_builder
        .start_node("timer")?
        .add_str(p_compatible, "arm,armv8-timer")?
        .add_u32(p_interrupt_parent, PHANDLE_GIC)?
        .add_str(p_interrupt_names, "virt")?
        .add_u32_array(
            p_interrupts,
            &[GIC_PPI, HYPERV_VIRT_TIMER_PPI, IRQ_TYPE_LEVEL_LOW],
        )?
        .add_null(p_always_on)?;
    root_builder = timer.end_node()?;

    // Add PMU, if the interrupt is non-zero.
    let pmu_gsiv = processor_topology.pmu_gsiv();
    if pmu_gsiv != 0 {
        // TODO: This assumes the GSIV is a PPI. On all platforms, that seems to
        // be the case today.
        assert!((16..32).contains(&pmu_gsiv));
        let ppi_index = pmu_gsiv - 16;
        let pmu = root_builder
            .start_node("pmu")?
            .add_str(p_compatible, "arm,armv8-pmuv3")?
            .add_u32_array(p_interrupts, &[GIC_PPI, ppi_index, IRQ_TYPE_LEVEL_HIGH])?;
        root_builder = pmu.end_node()?;
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
            &[GIC_PPI, VMBUS_INTID, IRQ_TYPE_EDGE_FALLING],
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
    root_builder.end_node()?.build(boot_cpu_id)?;

    Ok(buffer)
}

#[cfg_attr(not(guest_arch = "aarch64"), expect(dead_code))]
pub fn load_linux_arm64(
    cfg: &KernelConfig<'_>,
    gm: &GuestMemory,
    enable_serial: bool,
    processor_topology: &ProcessorTopology<Aarch64Topology>,
) -> Result<Vec<Aarch64Register>, Error> {
    let mut loader = Loader::new(gm.clone(), cfg.mem_layout, hvdef::Vtl::Vtl0);
    let mut kernel_file = cfg.kernel;
    let mut initrd = Vec::new();
    if let Some(mut initrd_file) = cfg.initrd.as_ref() {
        initrd_file.rewind().map_err(Error::InitRd)?;
        initrd_file
            .read_to_end(&mut initrd)
            .map_err(Error::InitRd)?;
    }

    // Data dependencies:
    // - DeviceTree carries the start address of the initrd.
    // - The linux loader loads the kernel, the initrd at the said address,
    //   and the device tree into the guest memory.
    //
    // Thus, we first start with planning the memory layout where
    // some space at the loader bottom is reserved for the initrd.

    let load_bottom_addr: u64 = 16 << 20;
    let initrd_start: u64 = load_bottom_addr;
    let initrd_end: u64 = initrd_start + initrd.len() as u64;
    // Align the kernel to 2MB
    let kernel_minimum_start_address: u64 = (initrd_end + 0x1fffff) & !0x1fffff;

    let device_tree = build_dt(
        cfg,
        gm,
        enable_serial,
        processor_topology,
        initrd_start,
        initrd_end,
    )
    .map_err(|e| Error::Dt(DtError(e)))?;
    let load_info = loader::linux::load_kernel_and_initrd_arm64(
        &mut loader,
        &mut kernel_file,
        kernel_minimum_start_address,
        if !initrd.is_empty() {
            Some(InitrdConfig {
                initrd_address: InitrdAddressType::Address(initrd_start),
                initrd: &initrd,
            })
        } else {
            None
        },
        Some(&device_tree),
    )
    .map_err(Error::Loader)?;

    // Set the registers separately so they won't conflict with the UEFI boot when
    // `load_kernel_and_initrd_arm64` is used for VTL2 direct kernel boot.
    loader::linux::set_direct_boot_registers_arm64(&mut loader, &load_info)
        .map_err(Error::Loader)?;

    Ok(loader.initial_regs())
}
