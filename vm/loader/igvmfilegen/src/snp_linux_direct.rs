// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Generation strategy for a self-contained SNP Linux-direct IGVM.

use crate::file_loader::IgvmLoader;
use crate::file_loader::IgvmOutput;
use crate::file_loader::SnpLinuxDirectConfig;
use crate::vp_context_builder::snp::InjectionType;
use anyhow::Context;
use chipset_resources::pm::DEFAULT_ACPI_IRQ;
use chipset_resources::pm::DEFAULT_PM_PIO_BASE;
use igvm_defs::SnpPolicy;
use igvmfilegen_config::LinuxImage;
use igvmfilegen_config::ResourceType;
use igvmfilegen_config::Resources;
use loader::importer::X86Register;
use loader::linux::InitrdAddressType;
use loader::linux::InitrdConfig;
use serial_16550_resources::ComPort;
use std::io::Seek;
use vm_topology::memory::MemoryLayout;
use vm_topology::pcie::PcieHostBridge;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::TopologyBuilder;
use vm_topology::processor::x86::X86Topology;
use vmm_core::acpi_builder::AcpiArchConfig;
use vmm_core::acpi_builder::AcpiTablesBuilder;

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
    /// The kernel and optional initrd resources.
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
        import_all_ram: true,
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

    loader::linux::load_x86(
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

    loader.finalize().context("finalizing SNP IGVM")
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

    const COMPATIBILITY_MASK: u32 = 1;
    const TEST_POLICY: u64 = 0x30000;
    const TEST_C_BIT_MASK: u64 = 1 << 51;

    fn test_loader(ram_page_count: u64) -> IgvmLoader<X86Register> {
        IgvmLoader::new_snp_linux_direct(SnpLinuxDirectConfig {
            policy: SnpPolicy::from(TEST_POLICY),
            c_bit_mask: TEST_C_BIT_MASK,
            ram_page_count,
            vmsa_page: Some(KVM_VMSA_GPA / PAGE_SIZE),
            injection_type: InjectionType::Normal,
            import_all_ram: true,
        })
    }

    fn import_test_registers(importer: &mut dyn ImageLoad<X86Register>) {
        importer
            .import_vp_register(X86Register::Rip(0x100000))
            .unwrap();
        importer
            .import_vp_register(X86Register::Cr3(0x4000 | TEST_C_BIT_MASK))
            .unwrap();
        importer
            .import_vp_register(X86Register::Cr0(x86defs::X64_CR0_PE | x86defs::X64_CR0_PG))
            .unwrap();
        importer
            .import_vp_register(X86Register::Cr4(x86defs::X64_CR4_PAE))
            .unwrap();
        importer
            .import_vp_register(X86Register::Efer(
                x86defs::X64_EFER_LME | x86defs::X64_EFER_LMA,
            ))
            .unwrap();
    }

    fn assert_serialized_igvm_shape(igvm: &IgvmFile, ram_page_count: u64) {
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

        assert_eq!(covered_pages.len(), ram_page_count as usize);
        assert_eq!(covered_pages.first(), Some(&0));
        assert_eq!(covered_pages.last(), Some(&(ram_page_count - 1)));
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
    fn vmsa_uses_c_bit_model() {
        let mut context =
            SnpHardwareContext::new_linux_direct(TEST_C_BIT_MASK, InjectionType::Normal);
        for register in [
            X86Register::Rip(0x100000),
            X86Register::Cr3(0x4000 | TEST_C_BIT_MASK),
            X86Register::Cr0(x86defs::X64_CR0_PE | x86defs::X64_CR0_PG),
            X86Register::Cr4(x86defs::X64_CR4_PAE),
            X86Register::Efer(x86defs::X64_EFER_LME | x86defs::X64_EFER_LMA),
        ] {
            context.import_vp_register(register);
        }
        let vmsa = context.vmsa();
        assert_eq!(vmsa.rip, 0x100000);
        assert_ne!(vmsa.cr3 & TEST_C_BIT_MASK, 0);
        assert_ne!(vmsa.cr0 & x86defs::X64_CR0_ET, 0);
        assert_ne!(vmsa.cr4 & x86defs::X64_CR4_MCE, 0);
        assert!(vmsa.sev_features.snp());
        assert!(!vmsa.sev_features.vtom());
        assert!(!vmsa.sev_features.debug_swap());
        assert_eq!(vmsa.virtual_tom, 0);
        assert_eq!(vmsa.rflags, 2);
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
    fn complete_ram_has_one_directive_per_page() {
        let mut loader = test_loader(4);
        {
            let mut importer = loader.loader();
            importer
                .import_pages(1, 1, "secret", BootPageAcceptance::SecretsPage, &[])
                .unwrap();
            importer
                .import_pages(2, 1, "cpuid", BootPageAcceptance::CpuidPage, &[])
                .unwrap();
            import_test_registers(&mut importer);
        }
        let output = loader.finalize().unwrap();
        let directives = output.guest.directives();

        assert_eq!(directives.len(), 6);
        assert!(matches!(
            directives[0],
            IgvmDirectiveHeader::RequiredMemory { .. }
        ));
        assert!(matches!(
            directives[2],
            IgvmDirectiveHeader::PageData {
                data_type: IgvmPageDataType::NORMAL,
                ref data,
                ..
            } if data.is_empty()
        ));
        assert!(matches!(
            directives[3],
            IgvmDirectiveHeader::PageData {
                data_type: IgvmPageDataType::SECRETS,
                ..
            }
        ));
        assert!(matches!(
            directives[4],
            IgvmDirectiveHeader::PageData {
                data_type: IgvmPageDataType::CPUID_DATA,
                ..
            }
        ));
        assert!(matches!(
            directives[5],
            IgvmDirectiveHeader::PageData {
                data_type: IgvmPageDataType::NORMAL,
                ..
            }
        ));
        assert!(matches!(
            directives[1],
            IgvmDirectiveHeader::SnpVpContext {
                gpa: KVM_VMSA_GPA,
                ..
            }
        ));
        assert_serialized_igvm_shape(&output.guest, 4);
    }

    #[test]
    fn complete_ram_emits_only_the_bsp_vmsa() {
        let mut loader = test_loader(1);
        import_test_registers(&mut loader.loader());
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
            import_test_registers(&mut importer);
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
}
