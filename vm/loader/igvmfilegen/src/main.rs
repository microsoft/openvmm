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
use std::io::Write;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use zerocopy::FromBytes;
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
            } => LoaderIsolationType::Snp {
                shared_gpa_boundary_bits,
                policy: SnpPolicy::from(policy).with_debug(enable_debug as u8),
                injection_type: match injection_type {
                    SnpInjectionType::Normal => vp_context_builder::snp::InjectionType::Normal,
                    SnpInjectionType::Restricted => {
                        vp_context_builder::snp::InjectionType::Restricted
                    }
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

        let mut loader = IgvmLoader::<R>::new(with_paravisor, loader_isolation_type);

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
        F: std::io::Read + std::io::Seek,
        Self: GuestArch;

    fn load_openhcl<F>(
        importer: &mut dyn ImageLoad<Self>,
        kernel_image: &mut F,
        shim: &mut F,
        sidecar: Option<&mut F>,
        command_line: CommandLineType<'_>,
        initrd: Option<&[u8]>,
        memory_page_base: Option<u64>,
        memory_page_count: u64,
        vtl0_config: Vtl0Config<'_>,
    ) -> Result<(), loader::paravisor::Error>
    where
        F: std::io::Read + std::io::Seek;
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
        F: std::io::Read + std::io::Seek,
    {
        loader::linux::load_kernel_and_initrd_x64(
            importer,
            kernel_image,
            kernel_minimum_start_address,
            initrd,
        )
    }

    fn load_openhcl<F>(
        importer: &mut dyn ImageLoad<Self>,
        kernel_image: &mut F,
        shim: &mut F,
        sidecar: Option<&mut F>,
        command_line: CommandLineType<'_>,
        initrd: Option<&[u8]>,
        memory_page_base: Option<u64>,
        memory_page_count: u64,
        vtl0_config: Vtl0Config<'_>,
    ) -> Result<(), loader::paravisor::Error>
    where
        F: std::io::Read + std::io::Seek,
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
        F: std::io::Read + std::io::Seek,
    {
        loader::linux::load_kernel_and_initrd_arm64(
            importer,
            kernel_image,
            kernel_minimum_start_address,
            initrd,
            device_tree_blob,
        )
    }

    fn load_openhcl<F>(
        importer: &mut dyn ImageLoad<Self>,
        kernel_image: &mut F,
        shim: &mut F,
        _sidecar: Option<&mut F>,
        command_line: CommandLineType<'_>,
        initrd: Option<&[u8]>,
        memory_page_base: Option<u64>,
        memory_page_count: u64,
        vtl0_config: Vtl0Config<'_>,
    ) -> Result<(), loader::paravisor::Error>
    where
        F: std::io::Read + std::io::Seek,
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
            load_linux(loader, linux, resources)?;
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

            let initrd = {
                let initrd_path = resources
                    .get(ResourceType::UnderhillInitrd)
                    .expect("validated present");
                Some(fs_err::read(initrd_path).context(format!(
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

            let initrd_slice = initrd.as_deref();

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
                initrd_slice,
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
    let initrd_vec = if use_initrd {
        let initrd_path = resources
            .get(ResourceType::LinuxInitrd)
            .expect("validated present");
        fs_err::read(initrd_path)
            .context(format!("reading vtl0 initrd at {}", initrd_path.display()))?
    } else {
        Vec::new()
    };
    let initrd = if initrd_vec.is_empty() {
        None
    } else {
        Some(InitrdConfig {
            initrd_address: loader::linux::InitrdAddressType::AfterKernel,
            initrd: &initrd_vec,
        })
    };
    let load_info = R::load_linux_kernel_and_initrd(loader, &mut kernel, 0, initrd, None)
        .context("loading linux kernel and initrd")?;
    Ok(load_info)
}
