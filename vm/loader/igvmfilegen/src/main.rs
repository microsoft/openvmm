// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements a command line utility to generate IGVM files.

#![forbid(unsafe_code)]

mod file_loader;
mod identity_mapping;
mod signed_measurement;
mod vp_context_builder;

use crate::file_loader::IgvmLoader;
use crate::file_loader::LoaderIsolationType;
use anyhow::Context;
use anyhow::bail;
use clap::Parser;
use file_loader::IgvmLoaderRegister;
use file_loader::IgvmVtlLoader;
use igvm::IgvmFile;
use igvm_defs::IGVM_FIXED_HEADER;
use igvm_defs::SnpPolicy;
use igvm_defs::TdxPolicy;
use igvmfilegen_config::Config;
use igvmfilegen_config::ConfigIsolationType;
use igvmfilegen_config::Image;
use igvmfilegen_config::LinuxImage;
use igvmfilegen_config::ResourceType;
use igvmfilegen_config::Resources;
use igvmfilegen_config::SecureAvicType;
use igvmfilegen_config::SnpInjectionType;
use igvmfilegen_config::UefiConfigType;
use loader::importer::Aarch64Register;
use loader::importer::GuestArch;
use loader::importer::GuestArchKind;
use loader::importer::ImageLoad;
use loader::importer::X86Register;
use loader::linux::InitrdConfig;
use loader::paravisor::CommandLineType;
use loader::paravisor::Vtl0Config;
use loader::paravisor::Vtl0Linux;
use std::ffi::CString;
use std::io::Seek;
use std::io::Write;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

#[derive(Parser)]
#[clap(name = "igvmfilegen", about = "Tool to generate IGVM files")]
enum Options {
    /// Dumps the contents of an IGVM file in a human-readable format
    // TODO: Move into its own tool.
    Dump {
        /// Dump file path
        #[clap(short, long = "filepath")]
        file_path: PathBuf,
    },
    /// Build an IGVM file according to a manifest
    Manifest {
        /// Config manifest file path
        #[clap(short, long = "manifest")]
        manifest: PathBuf,
        /// Resources file describing binary resources used to build the igvm
        /// file.
        #[clap(short, long = "resources")]
        resources: PathBuf,
        /// Output file path for the built igvm file
        #[clap(short = 'o', long)]
        output: PathBuf,
        /// Additional debug validation when building IGVM files
        #[clap(long)]
        debug_validation: bool,
    },
}

// TODO: Potential CLI flags:
//       --report: Dump additional data like what memory ranges were accepted with what values

fn main() -> anyhow::Result<()> {
    let opts = Options::parse();
    let filter = if std::env::var(EnvFilter::DEFAULT_ENV).is_ok() {
        EnvFilter::from_default_env()
    } else {
        EnvFilter::default().add_directive(LevelFilter::INFO.into())
    };
    tracing_subscriber::fmt()
        .log_internal_errors(true)
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();

    match opts {
        Options::Dump { file_path } => {
            let image = fs_err::read(file_path).context("reading input file")?;
            let fixed_header = IGVM_FIXED_HEADER::read_from_prefix(image.as_bytes())
                .expect("Invalid fixed header")
                .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)

            let igvm_data = IgvmFile::new_from_binary(&image, None).expect("should be valid");
            println!("Total file size: {} bytes\n", fixed_header.total_file_size);
            println!("{:#X?}", fixed_header);
            println!("{}", igvm_data);
            Ok(())
        }
        Options::Manifest {
            manifest,
            resources,
            output,
            debug_validation,
        } => {
            // Read the config from the JSON manifest path.
            let config: Config = serde_json::from_str(
                &fs_err::read_to_string(manifest).context("reading manifest")?,
            )
            .context("parsing manifest")?;

            // Read resources and validate that it covers the required resources
            // from the config.
            let resources: Resources = serde_json::from_str(
                &fs_err::read_to_string(resources).context("reading resources")?,
            )
            .context("parsing resources")?;

            let required_resources = config.required_resources();
            resources
                .check_required(&required_resources)
                .context("required resources not specified")?;

            tracing::info!(
                ?config,
                ?resources,
                "Building igvm file with given config and resources"
            );

            // Enable debug validation if specified or if running a debug build.
            match config.guest_arch {
                igvmfilegen_config::GuestArch::X64 => create_igvm_file::<X86Register>(
                    config,
                    resources,
                    debug_validation || cfg!(debug_assertions),
                    output,
                ),
                igvmfilegen_config::GuestArch::Aarch64 => create_igvm_file::<Aarch64Register>(
                    config,
                    resources,
                    debug_validation || cfg!(debug_assertions),
                    output,
                ),
            }
        }
    }
}

/// Create an IGVM file from the specified config
fn create_igvm_file<R: IgvmfilegenRegister + GuestArch + 'static>(
    igvm_config: Config,
    resources: Resources,
    debug_validation: bool,
    output: PathBuf,
) -> anyhow::Result<()> {
    tracing::debug!(?igvm_config, "Creating IGVM file",);

    let mut igvm_file: Option<IgvmFile> = None;
    let mut map_files = Vec::new();
    let base_path = output.file_stem().unwrap();
    for config in igvm_config.guest_configs {
        // Max VTL must be 2 or 0.
        if config.max_vtl != 2 && config.max_vtl != 0 {
            bail!("max_vtl must be 2 or 0");
        }

        let isolation_string = match config.isolation_type {
            ConfigIsolationType::None => "none",
            ConfigIsolationType::Vbs { .. } => "vbs",
            ConfigIsolationType::Snp { .. } => "snp",
            ConfigIsolationType::Tdx { .. } => "tdx",
        };
        let loader_isolation_type = match config.isolation_type {
            ConfigIsolationType::None => LoaderIsolationType::None,
            ConfigIsolationType::Vbs { enable_debug } => LoaderIsolationType::Vbs { enable_debug },
            ConfigIsolationType::Snp {
                shared_gpa_boundary_bits,
                policy,
                enable_debug,
                injection_type,
                secure_avic,
            } => LoaderIsolationType::Snp {
                shared_gpa_boundary_bits,
                policy: SnpPolicy::from(policy).with_debug(enable_debug as u8),
                injection_type: match injection_type {
                    SnpInjectionType::Normal => vp_context_builder::snp::InjectionType::Normal,
                    SnpInjectionType::Restricted => {
                        vp_context_builder::snp::InjectionType::Restricted
                    }
                },
                secure_avic: match secure_avic {
                    SecureAvicType::Enabled => vp_context_builder::snp::SecureAvic::Enabled,
                    SecureAvicType::Disabled => vp_context_builder::snp::SecureAvic::Disabled,
                },
            },
            ConfigIsolationType::Tdx {
                enable_debug,
                sept_ve_disable,
            } => LoaderIsolationType::Tdx {
                policy: TdxPolicy::new()
                    .with_debug_allowed(enable_debug as u8)
                    .with_sept_ve_disable(sept_ve_disable as u8),
            },
        };

        // Max VTL of 2 implies paravisor.
        let with_paravisor = config.max_vtl == 2;

        // Use native VP context for Linux direct boot without isolation.
        let use_native_vp_context = matches!(config.image, Image::Linux(_))
            && matches!(loader_isolation_type, LoaderIsolationType::None);

        let mut loader =
            IgvmLoader::<R>::new(with_paravisor, loader_isolation_type, use_native_vp_context);

        load_image(&mut loader.loader(), &config.image, &resources)?;

        let igvm_output = loader
            .finalize(config.guest_svn)
            .context("finalizing loader")?;

        // Merge the loaded guest into the overall IGVM file.
        match &mut igvm_file {
            Some(file) => file
                .merge_simple(igvm_output.guest)
                .context("merging guest into overall igvm file")?,
            None => igvm_file = Some(igvm_output.guest),
        }

        map_files.push(igvm_output.map);

        if let Some(doc) = igvm_output.doc {
            // Write the measurement document to a file with the same name,
            // but with -[isolation].json extension.
            let doc_path = {
                let mut name = base_path.to_os_string();
                name.push("-");
                name.push(isolation_string);
                name.push(".json");
                output.with_file_name(name)
            };
            tracing::info!(
                path = %doc_path.display(),
                "Writing document json file",
            );
            let mut doc_file = fs_err::OpenOptions::new()
                .create(true)
                .write(true)
                .open(doc_path)
                .context("creating doc file")?;

            writeln!(
                doc_file,
                "{}",
                serde_json::to_string(&doc).expect("json string")
            )
            .context("writing doc file")?;
        }
    }

    let mut igvm_binary = Vec::new();
    let igvm_file = igvm_file.expect("should have an igvm file");
    igvm_file
        .serialize(&mut igvm_binary)
        .context("serializing igvm")?;

    // If enabled, perform additional validation.
    if debug_validation {
        debug_validate_igvm_file(&igvm_file, &igvm_binary);
    }

    // Write the IGVM file to the specified file path in the config.
    tracing::info!(
        path = %output.display(),
        "Writing output IGVM file",
    );
    fs_err::File::create(&output)
        .context("creating igvm file")?
        .write_all(&igvm_binary)
        .context("writing igvm file")?;

    // Write the map file display output to a file with the same name, but .map
    // extension.
    let map_path = {
        let mut name = output.file_name().expect("has name").to_owned();
        name.push(".map");
        output.with_file_name(name)
    };
    tracing::info!(
        path = %map_path.display(),
        "Writing output map file",
    );
    let mut map_file = fs_err::OpenOptions::new()
        .create(true)
        .write(true)
        .open(map_path)
        .context("creating map file")?;

    for map in map_files {
        writeln!(map_file, "{}", map).context("writing map file")?;
    }

    Ok(())
}

/// Validate an in-memory IGVM file and the binary repr are equivalent.
// TODO: should live in the igvm crate
fn debug_validate_igvm_file(igvm_file: &IgvmFile, binary_file: &[u8]) {
    use igvm::IgvmDirectiveHeader;
    tracing::info!("Debug validation of serialized IGVM file.");

    let igvm_reserialized = IgvmFile::new_from_binary(binary_file, None).expect("should be valid");

    for (a, b) in igvm_file
        .platforms()
        .iter()
        .zip(igvm_reserialized.platforms().iter())
    {
        assert_eq!(a, b);
    }

    for (a, b) in igvm_file
        .initializations()
        .iter()
        .zip(igvm_reserialized.initializations().iter())
    {
        assert_eq!(a, b);
    }

    for (a, b) in igvm_file
        .directives()
        .iter()
        .zip(igvm_reserialized.directives().iter())
    {
        match (a, b) {
            (
                IgvmDirectiveHeader::PageData {
                    gpa: a_gpa,
                    flags: a_flags,
                    data_type: a_data_type,
                    data: a_data,
                    compatibility_mask: a_compmask,
                },
                IgvmDirectiveHeader::PageData {
                    gpa: b_gpa,
                    flags: b_flags,
                    data_type: b_data_type,
                    data: b_data,
                    compatibility_mask: b_compmask,
                },
            ) => {
                assert!(
                    a_gpa == b_gpa
                        && a_flags == b_flags
                        && a_data_type == b_data_type
                        && a_compmask == b_compmask
                );

                // data might not be the same length, as it gets padded out.
                for i in 0..b_data.len() {
                    if i < a_data.len() {
                        assert_eq!(a_data[i], b_data[i]);
                    } else {
                        assert_eq!(0, b_data[i]);
                    }
                }
            }
            (
                IgvmDirectiveHeader::ParameterArea {
                    number_of_bytes: a_number_of_bytes,
                    parameter_area_index: a_parameter_area_index,
                    initial_data: a_initial_data,
                },
                IgvmDirectiveHeader::ParameterArea {
                    number_of_bytes: b_number_of_bytes,
                    parameter_area_index: b_parameter_area_index,
                    initial_data: b_initial_data,
                },
            ) => {
                assert!(
                    a_number_of_bytes == b_number_of_bytes
                        && a_parameter_area_index == b_parameter_area_index
                );

                // initial_data might be padded out just like page data.
                for i in 0..b_initial_data.len() {
                    if i < a_initial_data.len() {
                        assert_eq!(a_initial_data[i], b_initial_data[i]);
                    } else {
                        assert_eq!(0, b_initial_data[i]);
                    }
                }
            }
            _ => assert_eq!(a, b),
        }
    }
}

/// A trait to specialize behavior of the file builder based on different
/// register types for different architectures. Different methods may need to be
/// called depending on the register type that represents the given architecture.
trait IgvmfilegenRegister: IgvmLoaderRegister + 'static {
    fn load_uefi(
        importer: &mut dyn ImageLoad<Self>,
        image: &[u8],
        config: loader::uefi::ConfigType,
    ) -> Result<loader::uefi::LoadInfo, loader::uefi::Error>;

    fn load_linux_kernel_and_initrd<F>(
        importer: &mut impl ImageLoad<Self>,
        kernel_image: &mut F,
        kernel_minimum_start_address: u64,
        initrd: Option<InitrdConfig<'_>>,
        device_tree_blob: Option<&[u8]>,
    ) -> Result<loader::linux::LoadInfo, loader::linux::Error>
    where
        F: std::io::Read + Seek,
        Self: GuestArch;

    fn load_linux_direct_boot_config(
        importer: &mut impl ImageLoad<Self>,
        load_info: &loader::linux::LoadInfo,
        command_line: &CString,
    ) -> anyhow::Result<()>
    where
        Self: GuestArch;

    fn load_openhcl<F>(
        importer: &mut dyn ImageLoad<Self>,
        kernel_image: &mut F,
        shim: &mut F,
        sidecar: Option<&mut F>,
        command_line: CommandLineType<'_>,
        initrd: Option<(&mut dyn loader::common::ReadSeek, u64)>,
        memory_page_base: Option<u64>,
        memory_page_count: u64,
        vtl0_config: Vtl0Config<'_>,
    ) -> Result<(), loader::paravisor::Error>
    where
        F: std::io::Read + Seek;
}

impl IgvmfilegenRegister for X86Register {
    fn load_uefi(
        importer: &mut dyn ImageLoad<Self>,
        image: &[u8],
        config: loader::uefi::ConfigType,
    ) -> Result<loader::uefi::LoadInfo, loader::uefi::Error> {
        loader::uefi::x86_64::load(importer, image, config)
    }

    fn load_linux_kernel_and_initrd<F>(
        importer: &mut impl ImageLoad<Self>,
        kernel_image: &mut F,
        kernel_minimum_start_address: u64,
        initrd: Option<InitrdConfig<'_>>,
        _device_tree_blob: Option<&[u8]>,
    ) -> Result<loader::linux::LoadInfo, loader::linux::Error>
    where
        F: std::io::Read + Seek,
    {
        loader::linux::load_kernel_and_initrd_x64(
            importer,
            kernel_image,
            kernel_minimum_start_address,
            initrd,
        )
    }

    fn load_linux_direct_boot_config(
        importer: &mut impl ImageLoad<Self>,
        load_info: &loader::linux::LoadInfo,
        command_line: &CString,
    ) -> anyhow::Result<()> {
        use loader::importer::BootPageAcceptance;
        use loader::importer::IgvmParameterType;

        // GPA layout for ancillary boot data (page numbers). These are
        // placed in low memory below where the kernel is loaded (kernel
        // loads at GPA 0x100_0000 / 16MB by default for ELF kernels).
        const ZERO_PAGE_BASE: u64 = 0x1; // GPA 0x1000
        const GDT_BASE: u64 = 0x2; // GPA 0x2000
        const PAGE_TABLE_BASE: u64 = 0x3; // GPA 0x3000
        // Parameter areas for VMM-provided data.
        const CMDLINE_BASE: u64 = 0xa; // GPA 0xa000
        const CMDLINE_PAGES: u32 = 1;
        const MEMORY_MAP_BASE: u64 = 0xb; // GPA 0xb000
        const MEMORY_MAP_PAGES: u32 = 4;
        const MADT_BASE: u64 = 0xf; // GPA 0xf000
        const MADT_PAGES: u32 = 1;
        const SRAT_BASE: u64 = 0x10; // GPA 0x10000
        const SRAT_PAGES: u32 = 2;
        const SLIT_BASE: u64 = 0x12; // GPA 0x12000
        const SLIT_PAGES: u32 = 1;
        const PPTT_BASE: u64 = 0x13; // GPA 0x13000
        const PPTT_PAGES: u32 = 1;

        // Import GDT.
        loader::common::import_default_gdt(importer, GDT_BASE).context("importing GDT")?;

        // Build and import identity-mapped page tables (4GB).
        let page_table_address = PAGE_TABLE_BASE * hvdef::HV_PAGE_SIZE;
        let mut page_table_work_buffer: Vec<page_table::x64::PageTable> =
            vec![page_table::x64::PageTable::new_zeroed(); page_table::x64::PAGE_TABLE_MAX_COUNT];
        let mut page_table: Vec<u8> = vec![0; page_table::x64::PAGE_TABLE_MAX_BYTES];
        let page_table_builder = page_table::x64::IdentityMapBuilder::new(
            page_table_address,
            page_table::IdentityMapSize::Size4Gb,
            page_table_work_buffer.as_mut_slice(),
            page_table.as_mut_slice(),
        )
        .context("building page tables")?;
        let page_table_data = page_table_builder.build();
        let page_table_pages = page_table_data.len() as u64 / hvdef::HV_PAGE_SIZE;
        importer
            .import_pages(
                PAGE_TABLE_BASE,
                page_table_pages,
                "linux-pagetables",
                BootPageAcceptance::Exclusive,
                page_table_data,
            )
            .context("importing page tables")?;

        // Build a partial zero page with static kernel boot info.
        // The e820 map is left empty — the VMM provides memory layout
        // via the MemoryMap IGVM parameter.
        let cmdline_address = CMDLINE_BASE * hvdef::HV_PAGE_SIZE;
        let zero_page = loader_defs::linux::boot_params {
            hdr: loader_defs::linux::setup_header {
                type_of_loader: 0xff,
                boot_flag: 0xaa55.into(),
                header: 0x53726448.into(),
                cmd_line_ptr: (cmdline_address as u32).into(),
                cmdline_size: (command_line.as_bytes().len() as u32).into(),
                ramdisk_image: (load_info.initrd.as_ref().map(|i| i.gpa).unwrap_or(0) as u32)
                    .into(),
                ramdisk_size: (load_info.initrd.as_ref().map(|i| i.size).unwrap_or(0) as u32)
                    .into(),
                kernel_alignment: 0x100000.into(),
                ..FromZeros::new_zeroed()
            },
            ..FromZeros::new_zeroed()
        };
        importer
            .import_pages(
                ZERO_PAGE_BASE,
                1,
                "linux-zeropage",
                BootPageAcceptance::Exclusive,
                zero_page.as_bytes(),
            )
            .context("importing zero page")?;

        // Create IGVM parameter areas for VMM-provided data.
        let cmdline_area = importer
            .create_parameter_area(CMDLINE_BASE, CMDLINE_PAGES, "linux-cmdline")
            .context("creating cmdline parameter area")?;
        importer
            .import_parameter(cmdline_area, 0, IgvmParameterType::CommandLine)
            .context("importing cmdline parameter")?;

        let memory_map_area = importer
            .create_parameter_area(MEMORY_MAP_BASE, MEMORY_MAP_PAGES, "linux-memory-map")
            .context("creating memory map parameter area")?;
        importer
            .import_parameter(memory_map_area, 0, IgvmParameterType::MemoryMap)
            .context("importing memory map parameter")?;

        let madt_area = importer
            .create_parameter_area(MADT_BASE, MADT_PAGES, "linux-madt")
            .context("creating MADT parameter area")?;
        importer
            .import_parameter(madt_area, 0, IgvmParameterType::Madt)
            .context("importing MADT parameter")?;

        let srat_area = importer
            .create_parameter_area(SRAT_BASE, SRAT_PAGES, "linux-srat")
            .context("creating SRAT parameter area")?;
        importer
            .import_parameter(srat_area, 0, IgvmParameterType::Srat)
            .context("importing SRAT parameter")?;

        let slit_area = importer
            .create_parameter_area(SLIT_BASE, SLIT_PAGES, "linux-slit")
            .context("creating SLIT parameter area")?;
        importer
            .import_parameter(slit_area, 0, IgvmParameterType::Slit)
            .context("importing SLIT parameter")?;

        let pptt_area = importer
            .create_parameter_area(PPTT_BASE, PPTT_PAGES, "linux-pptt")
            .context("creating PPTT parameter area")?;
        importer
            .import_parameter(pptt_area, 0, IgvmParameterType::Pptt)
            .context("importing PPTT parameter")?;

        // Set VP registers for 64-bit long mode entry.
        let mut import_reg = |register| {
            importer
                .import_vp_register(register)
                .context("importing VP register")
        };

        import_reg(X86Register::Cr0(x86defs::X64_CR0_PG | x86defs::X64_CR0_PE))?;
        import_reg(X86Register::Cr3(page_table_address))?;
        import_reg(X86Register::Cr4(x86defs::X64_CR4_PAE))?;
        import_reg(X86Register::Efer(
            x86defs::X64_EFER_SCE
                | x86defs::X64_EFER_LME
                | x86defs::X64_EFER_LMA
                | x86defs::X64_EFER_NXE,
        ))?;
        import_reg(X86Register::Pat(x86defs::X86X_MSR_DEFAULT_PAT))?;
        import_reg(X86Register::Rip(load_info.kernel.entrypoint))?;
        import_reg(X86Register::Rsi(ZERO_PAGE_BASE * hvdef::HV_PAGE_SIZE))?;
        import_reg(X86Register::MtrrDefType(0xc00))?;
        import_reg(X86Register::MtrrFix64k00000(0x0606060606060606))?;
        import_reg(X86Register::MtrrFix16k80000(0x0606060606060606))?;

        Ok(())
    }

    fn load_openhcl<F>(
        importer: &mut dyn ImageLoad<Self>,
        kernel_image: &mut F,
        shim: &mut F,
        sidecar: Option<&mut F>,
        command_line: CommandLineType<'_>,
        initrd: Option<(&mut dyn loader::common::ReadSeek, u64)>,
        memory_page_base: Option<u64>,
        memory_page_count: u64,
        vtl0_config: Vtl0Config<'_>,
    ) -> Result<(), loader::paravisor::Error>
    where
        F: std::io::Read + Seek,
    {
        loader::paravisor::load_openhcl_x64(
            importer,
            kernel_image,
            shim,
            sidecar,
            command_line,
            initrd,
            memory_page_base,
            memory_page_count,
            vtl0_config,
        )
    }
}

impl IgvmfilegenRegister for Aarch64Register {
    fn load_uefi(
        importer: &mut dyn ImageLoad<Self>,
        image: &[u8],
        config: loader::uefi::ConfigType,
    ) -> Result<loader::uefi::LoadInfo, loader::uefi::Error> {
        loader::uefi::aarch64::load(importer, image, config)
    }

    fn load_linux_kernel_and_initrd<F>(
        importer: &mut impl ImageLoad<Self>,
        kernel_image: &mut F,
        kernel_minimum_start_address: u64,
        initrd: Option<InitrdConfig<'_>>,
        device_tree_blob: Option<&[u8]>,
    ) -> Result<loader::linux::LoadInfo, loader::linux::Error>
    where
        F: std::io::Read + Seek,
    {
        loader::linux::load_kernel_and_initrd_arm64(
            importer,
            kernel_image,
            kernel_minimum_start_address,
            initrd,
            device_tree_blob,
        )
    }

    fn load_linux_direct_boot_config(
        importer: &mut impl ImageLoad<Self>,
        load_info: &loader::linux::LoadInfo,
        _command_line: &CString,
    ) -> anyhow::Result<()> {
        loader::linux::set_direct_boot_registers_arm64(importer, load_info)
            .context("loading aarch64 linux direct boot registers")?;
        Ok(())
    }

    fn load_openhcl<F>(
        importer: &mut dyn ImageLoad<Self>,
        kernel_image: &mut F,
        shim: &mut F,
        _sidecar: Option<&mut F>,
        command_line: CommandLineType<'_>,
        initrd: Option<(&mut dyn loader::common::ReadSeek, u64)>,
        memory_page_base: Option<u64>,
        memory_page_count: u64,
        vtl0_config: Vtl0Config<'_>,
    ) -> Result<(), loader::paravisor::Error>
    where
        F: std::io::Read + Seek,
    {
        loader::paravisor::load_openhcl_arm64(
            importer,
            kernel_image,
            shim,
            command_line,
            initrd,
            memory_page_base,
            memory_page_count,
            vtl0_config,
        )
    }
}

/// Load an image.
fn load_image<'a, R: IgvmfilegenRegister + GuestArch + 'static>(
    loader: &mut IgvmVtlLoader<'_, R>,
    config: &'a Image,
    resources: &'a Resources,
) -> anyhow::Result<()> {
    tracing::debug!(?config, "loading into VTL0");

    match *config {
        Image::None => {
            // Nothing is loaded.
        }
        Image::Uefi { config_type } => {
            load_uefi(loader, resources, config_type)?;
        }
        Image::Linux(ref linux) => {
            let load_info = load_linux(loader, linux, resources)?;
            R::load_linux_direct_boot_config(loader, &load_info, &linux.command_line)?;
        }
        Image::Openhcl {
            ref command_line,
            static_command_line,
            memory_page_base,
            memory_page_count,
            uefi,
            ref linux,
        } => {
            if uefi && linux.is_some() {
                anyhow::bail!("cannot include both UEFI and Linux images in OpenHCL image");
            }

            let kernel_path = resources
                .get(ResourceType::UnderhillKernel)
                .expect("validated present");
            let mut kernel = fs_err::File::open(kernel_path).context(format!(
                "reading underhill kernel image at {}",
                kernel_path.display()
            ))?;

            let mut initrd = {
                let initrd_path = resources
                    .get(ResourceType::UnderhillInitrd)
                    .expect("validated present");
                Some(fs_err::File::open(initrd_path).context(format!(
                    "reading underhill initrd at {}",
                    initrd_path.display()
                ))?)
            };

            let shim_path = resources
                .get(ResourceType::OpenhclBoot)
                .expect("validated present");
            let mut shim = fs_err::File::open(shim_path)
                .context(format!("reading underhill shim at {}", shim_path.display()))?;

            let mut sidecar =
                if let Some(sidecar_path) = resources.get(ResourceType::UnderhillSidecar) {
                    Some(fs_err::File::open(sidecar_path).context("reading AP kernel")?)
                } else {
                    None
                };

            let initrd_info = if let Some(ref mut f) = initrd {
                let size = f.seek(std::io::SeekFrom::End(0))?;
                f.rewind()?;
                Some((f as &mut dyn loader::common::ReadSeek, size))
            } else {
                None
            };

            // TODO: While the paravisor supports multiple things that can be
            // loaded in VTL0, we don't yet have updated file builder config for
            // that.
            //
            // Since the host performs PCAT loading, each image that supports
            // UEFI also supports PCAT boot. A future file builder config change
            // will make this more explicit.
            let vtl0_load_config = if uefi {
                let mut inner_loader = loader.nested_loader();
                let load_info = load_uefi(&mut inner_loader, resources, UefiConfigType::None)?;
                let vp_context = inner_loader.take_vp_context();
                Vtl0Config {
                    supports_pcat: loader.loader().arch() == GuestArchKind::X86_64,
                    supports_uefi: Some((load_info, vp_context)),
                    supports_linux: None,
                }
            } else if let Some(linux) = linux {
                let load_info = load_linux(&mut loader.nested_loader(), linux, resources)?;
                Vtl0Config {
                    supports_pcat: false,
                    supports_uefi: None,
                    supports_linux: Some(Vtl0Linux {
                        command_line: &linux.command_line,
                        load_info,
                    }),
                }
            } else {
                Vtl0Config {
                    supports_pcat: false,
                    supports_uefi: None,
                    supports_linux: None,
                }
            };

            let command_line = if static_command_line {
                CommandLineType::Static(command_line)
            } else {
                CommandLineType::HostAppendable(command_line)
            };

            R::load_openhcl(
                loader,
                &mut kernel,
                &mut shim,
                sidecar.as_mut(),
                command_line,
                initrd_info,
                memory_page_base,
                memory_page_count,
                vtl0_load_config,
            )
            .context("underhill kernel loader")?;
        }
    };

    Ok(())
}

fn load_uefi<R: IgvmfilegenRegister + GuestArch + 'static>(
    loader: &mut IgvmVtlLoader<'_, R>,
    resources: &Resources,
    config_type: UefiConfigType,
) -> Result<loader::uefi::LoadInfo, anyhow::Error> {
    let image_path = resources
        .get(ResourceType::Uefi)
        .expect("validated present");
    let image = fs_err::read(image_path)
        .context(format!("reading uefi image at {}", image_path.display()))?;
    let config = match config_type {
        UefiConfigType::None => loader::uefi::ConfigType::None,
        UefiConfigType::Igvm => loader::uefi::ConfigType::Igvm,
    };
    let load_info = R::load_uefi(loader, &image, config).context("uefi loader")?;
    Ok(load_info)
}

fn load_linux<R: IgvmfilegenRegister + GuestArch + 'static>(
    loader: &mut IgvmVtlLoader<'_, R>,
    config: &LinuxImage,
    resources: &Resources,
) -> Result<loader::linux::LoadInfo, anyhow::Error> {
    let LinuxImage {
        use_initrd,
        command_line: _,
    } = *config;
    let kernel_path = resources
        .get(ResourceType::LinuxKernel)
        .expect("validated present");
    let mut kernel = fs_err::File::open(kernel_path).context(format!(
        "reading vtl0 kernel image at {}",
        kernel_path.display()
    ))?;
    let mut initrd_file = if use_initrd {
        let initrd_path = resources
            .get(ResourceType::LinuxInitrd)
            .expect("validated present");
        Some(
            fs_err::File::open(initrd_path)
                .context(format!("reading vtl0 initrd at {}", initrd_path.display()))?,
        )
    } else {
        None
    };
    let initrd = if let Some(ref mut f) = initrd_file {
        let size = f.seek(std::io::SeekFrom::End(0))?;
        f.rewind()?;
        Some(InitrdConfig {
            initrd_address: loader::linux::InitrdAddressType::AfterKernel,
            initrd: f,
            size,
        })
    } else {
        None
    };
    let load_info = R::load_linux_kernel_and_initrd(loader, &mut kernel, 0, initrd, None)
        .context("loading linux kernel and initrd")?;
    Ok(load_info)
}
