// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Generation strategy for a self-contained SNP Linux-direct IGVM.

use crate::file_loader::IgvmLoader;
use crate::file_loader::IgvmOutput;
use crate::file_loader::SnpLinuxDirectConfig;
use crate::vp_context_builder::snp::InjectionType;
use anyhow::Context;
use anyhow::ensure;
use chipset_resources::pm::DEFAULT_ACPI_IRQ;
use chipset_resources::pm::DEFAULT_PM_PIO_BASE;
use igvm_defs::SnpPolicy;
use igvmfilegen_config::LinuxImage;
use igvmfilegen_config::ResourceType;
use igvmfilegen_config::Resources;
use loader::importer::BootPageAcceptance;
use loader::importer::ImageLoad;
use loader::importer::X86Register;
use loader::linux::InitrdAddressType;
use loader::linux::InitrdConfig;
use loader_defs::linux::SNP_BOOT_SHIM_MAX_RANGES;
use loader_defs::linux::SNP_BOOT_SHIM_PARAMS_MAGIC;
use loader_defs::linux::SNP_BOOT_SHIM_PARAMS_VERSION;
use loader_defs::linux::SnpBootShimParams;
use loader_defs::linux::SnpBootShimRange;
use memory_range::MemoryRange;
use serial_16550_resources::ComPort;
use std::io::Seek;
use vm_topology::memory::MemoryLayout;
use vm_topology::pcie::PcieHostBridge;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::TopologyBuilder;
use vm_topology::processor::x86::X86Topology;
use vmm_core::acpi_builder::AcpiArchConfig;
use vmm_core::acpi_builder::AcpiTablesBuilder;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const PAGE_SIZE: u64 = igvm_defs::PAGE_SIZE_4K;
/// KVM hardcodes the initial VMSA at this GPA and measures it during launch
/// finish, after all userspace-provided launch-update pages.
///
/// Keep the BSP context at this address until KVM supports userspace-supplied
/// VMSA pages. MSHV can import the same context at this fixed address.
const KVM_VMSA_GPA: u64 = 0xffff_ffff_f000;

/// Inputs for the deterministic SNP Linux-direct guest layout.
pub struct BuildParams<'a> {
    /// The Linux payload configuration.
    pub linux: &'a LinuxImage,
    /// The processor count described by the embedded topology and ACPI tables.
    pub processor_count: u32,
    /// The number of measured 4-KiB RAM pages.
    pub memory_page_count: u64,
    /// The page-table address bit used as the SNP encryption bit.
    pub c_bit_position: u8,
    /// The SNP guest policy.
    pub policy: SnpPolicy,
    /// The kernel, optional initrd, and bootshim resources.
    pub resources: &'a Resources,
}

/// The fixed platform layout embedded in the bring-up IGVM.
struct FixedGuestLayout {
    memory: MemoryLayout,
    processors: ProcessorTopology<X86Topology>,
    pcie_host_bridges: Vec<PcieHostBridge>,
}

impl FixedGuestLayout {
    fn new(memory_page_count: u64, processor_count: u32) -> anyhow::Result<Self> {
        let memory_size = memory_page_count
            .checked_mul(PAGE_SIZE)
            .context("RAM size overflow")?;
        let memory = MemoryLayout::new(memory_size, &[], &[], &[], None)
            .context("building memory layout")?;
        let processors = TopologyBuilder::new_x86()
            .build(processor_count)
            .context("building processor topology")?;

        Ok(Self {
            memory,
            processors,
            pcie_host_bridges: Vec::new(),
        })
    }

    fn acpi_builder(&self) -> AcpiTablesBuilder<'_, X86Topology> {
        // This profile embeds OpenVMM's standard PC-compatible chipset
        // contract. The ACPI values must match the devices supplied by the
        // backend.
        //
        // TODO: Accept an external platform description when this bring-up
        // profile needs layouts other than the fixed OpenVMM defaults.
        AcpiTablesBuilder {
            processor_topology: &self.processors,
            mem_layout: &self.memory,
            cache_topology: None,
            pcie_host_bridges: &self.pcie_host_bridges,
            slit_info: None,
            generic_initiators: &[],
            arch: AcpiArchConfig::X86 {
                with_ioapic: true,
                with_pic: true,
                with_pit: true,
                with_psp: false,
                pm_base: DEFAULT_PM_PIO_BASE,
                acpi_irq: DEFAULT_ACPI_IRQ,
                iommu: None,
            },
        }
    }
}

fn open_linux_resources(
    linux: &LinuxImage,
    resources: &Resources,
) -> anyhow::Result<(fs_err::File, Option<fs_err::File>)> {
    let kernel_path = resources
        .get(ResourceType::LinuxKernel)
        .context("Linux kernel resource is missing")?;
    let kernel = fs_err::File::open(kernel_path)
        .with_context(|| format!("opening kernel {}", kernel_path.display()))?;

    let initrd = if linux.use_initrd {
        let initrd_path = resources
            .get(ResourceType::LinuxInitrd)
            .context("Linux initrd resource is missing")?;
        Some(
            fs_err::File::open(initrd_path)
                .with_context(|| format!("opening initrd {}", initrd_path.display()))?,
        )
    } else {
        None
    };

    Ok((kernel, initrd))
}

fn initrd_config(initrd: &mut Option<fs_err::File>) -> anyhow::Result<Option<InitrdConfig<'_>>> {
    let Some(initrd) = initrd else {
        return Ok(None);
    };
    let size = initrd
        .seek(std::io::SeekFrom::End(0))
        .context("measuring initrd")?;
    initrd.rewind().context("rewinding initrd")?;
    Ok(Some(InitrdConfig {
        initrd_address: InitrdAddressType::AfterKernel,
        initrd,
        size,
    }))
}

fn new_loader(
    policy: SnpPolicy,
    c_bit_position: u8,
    memory_page_count: u64,
) -> IgvmLoader<X86Register> {
    IgvmLoader::new_snp_linux_direct(SnpLinuxDirectConfig {
        policy,
        c_bit_mask: 1u64 << c_bit_position,
        ram_page_count: memory_page_count,
        vmsa_page: Some(KVM_VMSA_GPA / PAGE_SIZE),
        injection_type: InjectionType::Normal,
    })
}

/// Builds the deterministic topology, ACPI tables, Linux payload, and BSP
/// launch context embedded in the standalone SNP IGVM.
pub fn build(params: BuildParams<'_>) -> anyhow::Result<IgvmOutput> {
    let BuildParams {
        linux,
        processor_count,
        memory_page_count,
        c_bit_position,
        policy,
        resources,
    } = params;

    let layout = FixedGuestLayout::new(memory_page_count, processor_count)?;
    let acpi_builder = layout.acpi_builder();
    let (mut kernel, mut initrd) = open_linux_resources(linux, resources)?;
    let initrd_config = initrd_config(&mut initrd)?;
    let mut loader = new_loader(policy, c_bit_position, memory_page_count);
    let com1 = ComPort::Com1;

    let load_info = loader::linux::load_x86(
        &mut loader.loader(),
        &mut kernel,
        initrd_config,
        &linux.command_line,
        &layout.memory,
        |gpa| {
            let tables = acpi_builder.build_acpi_tables(gpa, |dsdt| {
                dsdt.add_apic();
                dsdt.add_uart(b"\\_SB.UAR1", b"COM1", 1, com1.io_port(), com1.irq().into());
                dsdt.add_rtc();
            });
            loader::linux::AcpiTables {
                rsdp: tables.rsdp,
                tables: tables.tables,
            }
        },
        None,
        Some(loader::linux::SnpBootConfig {
            c_bit: c_bit_position,
        }),
    )
    .context("loading direct-Linux image")?;

    let kernel_runtime_end = kernel_runtime_end(
        load_info.kernel.gpa,
        load_info.kernel.size,
        load_info
            .bzimage_setup_header
            .as_ref()
            .map(|header| (u64::from(header.pref_address), u64::from(header.init_size))),
    )?;
    let bootshim_ranges = load_bootshim_and_handoff(
        &mut loader,
        resources,
        load_info.kernel.entrypoint,
        loader::linux::ZERO_PAGE_BASE,
        memory_page_count,
        kernel_runtime_end,
    )?;

    let mut output = loader.finalize().context("finalizing SNP IGVM")?;
    for range in bootshim_ranges {
        output.map.report_range(
            MemoryRange::from_4k_gpn_range(range.start_gpn..range.start_gpn + range.page_count),
            "snp-bootshim-accepted-ram [PVALIDATE]",
        );
    }
    Ok(output)
}

/// Places the bootshim and its measured handoff page.
///
/// The Linux loader first imports the kernel, initrd, boot metadata, and SNP
/// special pages. The bootshim is placed at the first page after both those
/// imports and the kernel's runtime image. Its parameter page follows the
/// bootshim. The BSP starts at the bootshim entry point with RSI pointing to
/// that parameter page.
///
/// The parameter page lists every gap in configured RAM that has no measured
/// page-data directive. The bootshim makes those pages private, validates
/// them, and then enters Linux with RSI restored to the Linux zero page.
fn load_bootshim_and_handoff(
    loader: &mut IgvmLoader<X86Register>,
    resources: &Resources,
    linux_entry: u64,
    linux_zero_page: u64,
    memory_page_count: u64,
    kernel_runtime_end: u64,
) -> anyhow::Result<Vec<SnpBootShimRange>> {
    let shim_base = align_up_to_page(loader.next_available_gpa()?.max(kernel_runtime_end));

    let bootshim_path = resources
        .get(ResourceType::SnpBootshim)
        .context("SNP bootshim resource is missing")?;
    let mut bootshim = fs_err::File::open(bootshim_path)
        .with_context(|| format!("opening SNP bootshim {}", bootshim_path.display()))?;
    let shim_load_info = loader::elf::load_static_elf(
        &mut loader.loader(),
        &mut bootshim,
        0,
        shim_base,
        false,
        BootPageAcceptance::Exclusive,
        "snp-bootshim",
    )
    .context("loading SNP bootshim")?;

    let params_gpa = align_up_to_page(shim_load_info.next_available_address);
    let params_page = params_gpa / PAGE_SIZE;
    ensure!(
        params_page < memory_page_count,
        "SNP bootshim parameter page lies outside configured RAM"
    );

    let bootshim_ranges = loader
        .unimported_ram_ranges([params_page])?
        .into_iter()
        .map(|range| SnpBootShimRange {
            start_gpn: range.start,
            page_count: range.end - range.start,
        })
        .collect::<Vec<_>>();

    let bootshim_params = build_bootshim_params(
        linux_entry,
        linux_zero_page,
        memory_page_count * PAGE_SIZE,
        &bootshim_ranges,
    )?;
    {
        let mut importer = loader.loader();
        importer
            .import_pages(
                params_page,
                1,
                "snp-bootshim-params",
                BootPageAcceptance::Exclusive,
                bootshim_params.as_bytes(),
            )
            .context("importing SNP bootshim parameters")?;
        importer.import_vp_register(X86Register::Rip(shim_load_info.entrypoint))?;
        importer.import_vp_register(X86Register::Rsi(params_gpa))?;
    }

    Ok(bootshim_ranges)
}

fn align_up_to_page(value: u64) -> u64 {
    value
        .checked_add(PAGE_SIZE - 1)
        .expect("page alignment overflow")
        & !(PAGE_SIZE - 1)
}

fn kernel_runtime_end(
    load_gpa: u64,
    image_size: u64,
    bzimage_runtime: Option<(u64, u64)>,
) -> anyhow::Result<u64> {
    let (runtime_gpa, runtime_size) = bzimage_runtime
        .map(|(preferred_gpa, init_size)| (load_gpa.max(preferred_gpa), init_size))
        .unwrap_or((load_gpa, image_size));
    runtime_gpa
        .checked_add(runtime_size)
        .context("Linux runtime image end overflow")
}

fn build_bootshim_params(
    linux_entry: u64,
    linux_zero_page: u64,
    ram_end: u64,
    ranges: &[SnpBootShimRange],
) -> anyhow::Result<SnpBootShimParams> {
    ensure!(
        ranges.len() <= SNP_BOOT_SHIM_MAX_RANGES,
        "sparse SNP layout requires {} bootshim ranges, but the parameter page supports at most {}",
        ranges.len(),
        SNP_BOOT_SHIM_MAX_RANGES
    );
    let mut params = SnpBootShimParams::new_zeroed();
    params.magic = SNP_BOOT_SHIM_PARAMS_MAGIC;
    params.version = SNP_BOOT_SHIM_PARAMS_VERSION;
    params.range_count = ranges
        .len()
        .try_into()
        .context("SNP bootshim range count does not fit in u32")?;
    params.linux_entry = linux_entry;
    params.linux_zero_page = linux_zero_page;
    params.ram_end = ram_end;
    params.ranges[..ranges.len()].copy_from_slice(ranges);
    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vp_context_builder::VpContextBuilder;
    use crate::vp_context_builder::snp::SnpHardwareContext;
    use igvm::IgvmDirectiveHeader;
    use igvm::IgvmFile;
    use igvm::IgvmInitializationHeader;
    use igvm::IgvmPlatformHeader;
    use igvm::IgvmSerializer;
    use igvm_defs::IgvmPageDataType;
    use igvm_defs::IgvmPlatformType;
    use loader::importer::BootPageAcceptance;
    use loader::importer::ImageLoad;
    use std::collections::BTreeSet;
    use test_with_tracing::test;
    use zerocopy::FromBytes;

    const COMPATIBILITY_MASK: u32 = 1;
    const TEST_POLICY: u64 = 0x30000;
    const TEST_C_BIT_MASK: u64 = 1 << 51;
    const TEST_SHIM_ENTRY: u64 = 0x100000;

    fn test_loader(ram_page_count: u64) -> IgvmLoader<X86Register> {
        IgvmLoader::new_snp_linux_direct(SnpLinuxDirectConfig {
            policy: SnpPolicy::from(TEST_POLICY),
            c_bit_mask: TEST_C_BIT_MASK,
            ram_page_count,
            vmsa_page: Some(KVM_VMSA_GPA / PAGE_SIZE),
            injection_type: InjectionType::Normal,
        })
    }

    fn import_test_registers(importer: &mut dyn ImageLoad<X86Register>, params_gpa: u64) {
        for register in [
            X86Register::Rip(TEST_SHIM_ENTRY),
            X86Register::Rsi(params_gpa),
            X86Register::Cr3(0x4000 | TEST_C_BIT_MASK),
            X86Register::Cr0(x86defs::X64_CR0_PE | x86defs::X64_CR0_PG),
            X86Register::Cr4(x86defs::X64_CR4_PAE),
            X86Register::Efer(x86defs::X64_EFER_LME | x86defs::X64_EFER_LMA),
        ] {
            importer.import_vp_register(register).unwrap();
        }
    }

    fn assert_serialized_igvm_shape(
        igvm: &IgvmFile,
        ram_page_count: u64,
        expected_pages: &[u64],
        expected_handoff: (u64, u64),
    ) {
        let serializer = IgvmSerializer::new(igvm).unwrap();
        let expected_launch_digest = serializer
            .measurement_for(IgvmPlatformType::SEV_SNP)
            .unwrap()
            .digest
            .clone();
        let mut binary = Vec::new();
        serializer.serialize(&mut binary).unwrap();
        let reparsed = IgvmFile::new_from_binary(&binary, Some(igvm::IsolationType::Snp)).unwrap();

        assert_eq!(reparsed.platforms().len(), 1);
        let IgvmPlatformHeader::SupportedPlatform(platform) = &reparsed.platforms()[0];
        assert_eq!(platform.platform_type, IgvmPlatformType::SEV_SNP);
        assert_eq!(platform.highest_vtl, 0);
        assert_eq!(platform.shared_gpa_boundary, 0);
        assert!(reparsed.initializations().iter().any(|header| matches!(
            header,
            IgvmInitializationHeader::GuestPolicy {
                policy: TEST_POLICY,
                compatibility_mask: COMPATIBILITY_MASK,
            }
        )));

        let mut covered_pages = BTreeSet::new();
        let mut required_memory_count = 0;
        let mut next_vmsa_index = 0;
        let mut secrets_count = 0;
        let mut cpuid_count = 0;

        for directive in reparsed.directives() {
            match directive {
                IgvmDirectiveHeader::PageData {
                    gpa,
                    flags,
                    data_type,
                    data,
                    ..
                } => {
                    assert!(gpa.is_multiple_of(PAGE_SIZE));
                    let page = gpa / PAGE_SIZE;
                    assert!(page < ram_page_count);
                    assert!(covered_pages.insert(page));
                    assert!(!flags.shared() && !flags.unmeasured());
                    assert!(data.len() <= PAGE_SIZE as usize);
                    match *data_type {
                        IgvmPageDataType::SECRETS => secrets_count += 1,
                        IgvmPageDataType::CPUID_DATA => cpuid_count += 1,
                        IgvmPageDataType::NORMAL => {}
                        unexpected => panic!("unexpected PageData type {unexpected:?}"),
                    }
                }
                IgvmDirectiveHeader::SnpVpContext {
                    gpa,
                    vp_index,
                    vmsa,
                    ..
                } => {
                    assert_eq!(*gpa, KVM_VMSA_GPA);
                    assert_eq!(u32::from(*vp_index), next_vmsa_index);
                    assert_eq!(vmsa.rip, expected_handoff.0);
                    assert_eq!(vmsa.rsi, expected_handoff.1);
                    assert!(vmsa.sev_features.snp());
                    assert!(!vmsa.sev_features.vtom());
                    assert_eq!(vmsa.virtual_tom, 0);
                    assert_ne!(vmsa.cr3 & TEST_C_BIT_MASK, 0);
                    assert_ne!(vmsa.cr0 & x86defs::X64_CR0_ET, 0);
                    assert_ne!(vmsa.cr4 & x86defs::X64_CR4_MCE, 0);
                    assert_eq!(vmsa.rflags, u64::from(x86defs::RFlags::at_reset()));
                    assert_eq!(vmsa.dr6, 0xffff_0ff0);
                    assert_eq!(vmsa.dr7, 0x400);
                    assert_eq!(vmsa.x87_fcw, x86defs::xsave::INIT_FCW);
                    assert_eq!(vmsa.mxcsr, x86defs::xsave::DEFAULT_MXCSR);
                    next_vmsa_index += 1;
                }
                IgvmDirectiveHeader::RequiredMemory {
                    gpa,
                    number_of_bytes,
                    vtl2_protectable,
                    ..
                } => {
                    assert_eq!(*gpa, 0);
                    assert_eq!(u64::from(*number_of_bytes), ram_page_count * PAGE_SIZE);
                    assert!(!vtl2_protectable);
                    required_memory_count += 1;
                }
                unexpected => panic!("unexpected directive in fixed SNP IGVM: {unexpected:?}"),
            }
        }

        assert_eq!(
            covered_pages,
            expected_pages.iter().copied().collect::<BTreeSet<_>>()
        );
        assert_eq!(required_memory_count, 1);
        assert_eq!(next_vmsa_index, 1);
        assert_eq!(secrets_count, 1);
        assert_eq!(cpuid_count, 1);

        let measured_digest = IgvmSerializer::new(&reparsed)
            .unwrap()
            .measurement_for(IgvmPlatformType::SEV_SNP)
            .unwrap()
            .digest
            .clone();
        assert_eq!(measured_digest, expected_launch_digest);
    }

    #[test]
    fn fixed_layout_supports_one_and_many_processors() {
        for processor_count in [1, 4] {
            let layout = FixedGuestLayout::new(64, processor_count).unwrap();
            assert_eq!(layout.processors.vp_count(), processor_count);
            assert_eq!(layout.memory.ram().len(), 1);
        }
    }

    #[test]
    fn bzimage_runtime_uses_preferred_address_above_load_address() {
        assert_eq!(
            kernel_runtime_end(0x100000, 0x200000, Some((0x400000, 0x300000))).unwrap(),
            0x700000
        );
    }

    #[test]
    fn bzimage_runtime_uses_load_address_above_preferred_address() {
        assert_eq!(
            kernel_runtime_end(0x400000, 0x200000, Some((0x100000, 0x300000))).unwrap(),
            0x700000
        );
    }

    #[test]
    fn raw_kernel_runtime_uses_image_size() {
        assert_eq!(
            kernel_runtime_end(0x100000, 0x200000, None).unwrap(),
            0x300000
        );
    }

    #[test]
    fn rejects_kernel_runtime_end_overflow() {
        assert!(kernel_runtime_end(u64::MAX, 1, None).is_err());
        assert!(kernel_runtime_end(0, 0, Some((u64::MAX, 1))).is_err());
    }

    #[test]
    fn vmsa_uses_c_bit_model() {
        let params_gpa = 0x6000;
        let mut context =
            SnpHardwareContext::new_linux_direct(TEST_C_BIT_MASK, InjectionType::Normal);
        for register in [
            X86Register::Rip(TEST_SHIM_ENTRY),
            X86Register::Rsi(params_gpa),
            X86Register::Cr3(0x4000 | TEST_C_BIT_MASK),
            X86Register::Cr0(x86defs::X64_CR0_PE | x86defs::X64_CR0_PG),
            X86Register::Cr4(x86defs::X64_CR4_PAE),
            X86Register::Efer(x86defs::X64_EFER_LME | x86defs::X64_EFER_LMA),
        ] {
            context.import_vp_register(register);
        }

        let vmsa = context.vmsa();
        assert_eq!(vmsa.rip, TEST_SHIM_ENTRY);
        assert_eq!(vmsa.rsi, params_gpa);
        assert_ne!(vmsa.cr3 & TEST_C_BIT_MASK, 0);
        assert_ne!(vmsa.cr0 & x86defs::X64_CR0_ET, 0);
        assert_ne!(vmsa.cr4 & x86defs::X64_CR4_MCE, 0);
        assert!(vmsa.sev_features.snp());
        assert!(!vmsa.sev_features.vtom());
        assert!(!vmsa.sev_features.debug_swap());
        assert_eq!(vmsa.virtual_tom, 0);
        assert_eq!(vmsa.rflags, u64::from(x86defs::RFlags::at_reset()));
        assert_eq!(vmsa.dr6, 0xffff_0ff0);
        assert_eq!(vmsa.dr7, 0x400);
        assert_eq!(vmsa.tr.limit, 0xffff);
        assert_eq!(vmsa.tr.attrib, 0x8b);
        assert_eq!(vmsa.ldtr.limit, 0xffff);
        assert_eq!(vmsa.ldtr.attrib, 0x82);
        assert_eq!(vmsa.idtr.limit, 0xffff);
        assert_eq!(vmsa.x87_fcw, x86defs::xsave::INIT_FCW);
        assert_eq!(vmsa.mxcsr, x86defs::xsave::DEFAULT_MXCSR);
    }

    #[test]
    fn sparse_igvm_serializes_and_preserves_measurement() {
        const RAM_PAGE_COUNT: u64 = 8;
        const PARAMS_PAGE: u64 = 6;
        let params_gpa = PARAMS_PAGE * PAGE_SIZE;
        let mut loader = test_loader(RAM_PAGE_COUNT);
        {
            let mut importer = loader.loader();
            importer
                .import_pages(1, 1, "secrets", BootPageAcceptance::SecretsPage, &[])
                .unwrap();
            importer
                .import_pages(2, 1, "cpuid", BootPageAcceptance::CpuidPage, &[])
                .unwrap();
        }

        let ranges = loader.unimported_ram_ranges([PARAMS_PAGE]).unwrap();
        assert_eq!(ranges, [0..1, 3..6, 7..8]);
        let ranges = ranges
            .iter()
            .map(|range| SnpBootShimRange {
                start_gpn: range.start,
                page_count: range.end - range.start,
            })
            .collect::<Vec<_>>();
        let params = build_bootshim_params(
            0x200000,
            loader::linux::ZERO_PAGE_BASE,
            RAM_PAGE_COUNT * PAGE_SIZE,
            &ranges,
        )
        .unwrap();
        {
            let mut importer = loader.loader();
            importer
                .import_pages(
                    PARAMS_PAGE,
                    1,
                    "snp-bootshim-params",
                    BootPageAcceptance::Exclusive,
                    params.as_bytes(),
                )
                .unwrap();
            import_test_registers(&mut importer, params_gpa);
        }

        let output = loader.finalize().unwrap();
        let mut imported_pages = Vec::new();
        let mut required_memory = None;
        let mut handoff = None;
        let mut serialized_params = None;
        for directive in output.guest.directives() {
            match directive {
                IgvmDirectiveHeader::PageData {
                    gpa,
                    data_type,
                    data,
                    ..
                } => {
                    imported_pages.push(*gpa / PAGE_SIZE);
                    if *gpa == params_gpa {
                        assert_eq!(*data_type, IgvmPageDataType::NORMAL);
                        let mut page = [0; PAGE_SIZE as usize];
                        page[..data.len()].copy_from_slice(data);
                        serialized_params =
                            Some(SnpBootShimParams::read_from_bytes(&page).unwrap());
                    }
                }
                IgvmDirectiveHeader::RequiredMemory {
                    gpa,
                    number_of_bytes,
                    ..
                } => required_memory = Some((*gpa, *number_of_bytes)),
                IgvmDirectiveHeader::SnpVpContext { vmsa, .. } => {
                    handoff = Some((vmsa.rip, vmsa.rsi));
                }
                _ => {}
            }
        }

        imported_pages.sort_unstable();
        assert_eq!(imported_pages, [1, 2, PARAMS_PAGE]);
        assert_eq!(
            required_memory,
            Some((0, (RAM_PAGE_COUNT * PAGE_SIZE) as u32))
        );
        assert_eq!(handoff, Some((TEST_SHIM_ENTRY, params_gpa)));
        assert_eq!(serialized_params, Some(params));
        assert_serialized_igvm_shape(
            &output.guest,
            RAM_PAGE_COUNT,
            &[1, 2, PARAMS_PAGE],
            (TEST_SHIM_ENTRY, params_gpa),
        );
    }

    #[test]
    fn sparse_image_emits_only_the_bsp_vmsa() {
        let mut loader = test_loader(1);
        import_test_registers(&mut loader.loader(), 0x2000);

        let output = loader.finalize().unwrap();
        let vp_indexes = output
            .guest
            .directives()
            .iter()
            .filter_map(|directive| match directive {
                IgvmDirectiveHeader::SnpVpContext { vp_index, .. } => Some(*vp_index),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(vp_indexes, [0]);
    }

    #[test]
    fn overlapping_import_does_not_mutate_existing_pages() {
        let mut loader = test_loader(4);
        {
            let mut importer = loader.loader();
            importer
                .import_pages(1, 1, "first", BootPageAcceptance::Exclusive, &[0xaa])
                .unwrap();
            let error = importer
                .import_pages(
                    0,
                    2,
                    "overlap",
                    BootPageAcceptance::Exclusive,
                    &[0xbb; PAGE_SIZE as usize * 2],
                )
                .unwrap_err();
            assert!(error.to_string().contains("overlaps"));
            import_test_registers(&mut importer, 0x2000);
        }

        let output = loader.finalize().unwrap();
        let page = output
            .guest
            .directives()
            .iter()
            .find_map(|directive| match directive {
                IgvmDirectiveHeader::PageData { gpa, data, .. } if *gpa == PAGE_SIZE => Some(data),
                _ => None,
            })
            .unwrap();
        assert_eq!(page, &[0xaa]);
    }

    #[test]
    fn rejects_too_many_bootshim_ranges() {
        let ranges = vec![
            SnpBootShimRange {
                start_gpn: 0,
                page_count: 1,
            };
            SNP_BOOT_SHIM_MAX_RANGES + 1
        ];
        assert!(
            build_bootshim_params(0x100000, 0x2000, 0x200000, &ranges)
                .unwrap_err()
                .to_string()
                .contains("supports at most")
        );
    }

    #[test]
    #[should_panic(expected = "page alignment overflow")]
    fn bootshim_placement_overflow_panics() {
        align_up_to_page(u64::MAX);
    }
}
