// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements a command line utility to generate IGVM files.

#![forbid(unsafe_code)]

#[cfg(not(test))]
crypto::ensure_single_backend!();

mod corim_signature;
mod file_loader;
mod identity_mapping;
mod measurement_diag;
mod platform_mask;
mod vp_context_builder;

use crate::corim_signature::detach_payload;
use crate::file_loader::IgvmLoader;
use crate::file_loader::LoaderIsolationType;
use crate::identity_mapping::Measurement;
use crate::identity_mapping::SnpMeasurement;
use crate::identity_mapping::TdxMeasurement;
use crate::identity_mapping::VbsMeasurement;
use crate::measurement_diag::log_measurement_diagnostic;
use anyhow::Context;
use anyhow::bail;
use clap::Parser;
use clap::ValueEnum;
use file_loader::IgvmLoaderRegister;
use file_loader::IgvmVtlLoader;
use igvm::IgvmFile;
use igvm::IgvmInitializationHeader;
use igvm::IgvmPlatformHeader;
use igvm::IgvmSerializer;
use igvm::corim::launch_measurement::LaunchMeasurement;
use igvm::corim::launch_measurement::MeasurementKind;
use igvm_defs::IGVM_FIXED_HEADER;
use igvm_defs::IgvmPlatformType;
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
use std::io::Seek;
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
    /// Dump CoRIM (Concise Reference Integrity Manifest) headers and payloads from an IGVM file.
    ///
    /// This command scans the IGVM variable headers for CoRIM-related entries and prints or
    /// extracts their contents. By default, all supported CoRIM headers for all platforms found
    /// in the file are dumped. Use `--header-type` and `--platform` to narrow the selection.
    ///
    /// A human-readable summary of the selected CoRIM headers is written to stdout. When
    /// `--output <dir>` is provided, the CoRIM payloads are also extracted to files in
    /// that directory and the file paths are reported.
    DumpCorim {
        /// Input IGVM file path to read CoRIM headers and payloads from.
        #[clap(short, long = "filepath")]
        file_path: PathBuf,
        /// Filter by CoRIM header type (e.g. document or signature). If not specified,
        /// all supported CoRIM header types in the IGVM file are included.
        #[clap(long, value_enum)]
        header_type: Option<CorimHeaderType>,
        /// Filter by platform type for which the CoRIM applies (see `Platform` enum).
        /// If not specified, CoRIM entries for all platforms present in the IGVM file
        /// are considered.
        #[clap(long, value_enum)]
        platform: Option<Platform>,
        /// Output directory to extract CoRIM payload data. For each matching CoRIM header,
        /// the payload is written to a file named `corim_{document,signature}_<platform>.<ext>`,
        /// e.g. `corim_document_vbs.cbor` or `corim_signature_snp.cose`.
        /// If omitted, payload contents are not written as files and are instead described
        /// in the textual output on stdout.
        #[clap(short, long)]
        output: Option<PathBuf>,
    },
    /// Build an IGVM file according to a manifest.
    ///
    /// Also emits per-platform sibling files next to `--output`:
    /// `<base>-{snp,tdx,vbs}.json` (legacy identity documents) and
    /// `<base>-{snp,tdx,vbs}.cbor` (CoRIM launch endorsements) for every
    /// measurable platform in the manifest.
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
        /// Override secure AVIC to disabled for debug SNP guest configs
        #[clap(long)]
        disable_secure_avic: bool,
    },
    /// Patch a CoRIM signature into an existing IGVM file for a given platform.
    ///
    /// The CoRIM document is generated automatically by `manifest` for every
    /// measurable platform, so this command only attaches the detached
    /// signature. Provide either a single bundled/signed CoRIM via
    /// `--corim-bundle` (the tool splits it and uses the detached signature;
    /// the IGVM file must already contain a matching CoRIM document) or an
    /// already-detached signature via `--corim-signature` (the document slot
    /// must already be populated in the IGVM file).
    ///
    /// What is verified: this command checks that the supplied signature
    /// is a well-formed COSE_Sign1 envelope using PS384 (COSE alg -38,
    /// RSA-PSS with SHA-384) and that the signature math validates against
    /// the IGVM-embedded CoRIM document, using the public key carried in
    /// the envelope's `x5chain` / `x5bag` header (RFC 9360).
    ///
    /// What is NOT verified: certificate-chain trust. The signing
    /// certificate is taken from the envelope at face value -- no
    /// validation against a trust root, no revocation check, no policy /
    /// EKU enforcement. The caller is responsible for ensuring the input
    /// signature originated from a trusted signer (e.g. by sourcing it
    /// only from a controlled signing pipeline). Verification here exists
    /// to catch accidental corruption and algorithm mismatches, not to
    /// establish trust.
    PatchCorimSignature {
        /// Input IGVM file path
        #[clap(short, long)]
        input: PathBuf,
        /// Output IGVM file path (can be the same as input to modify in place)
        #[clap(short, long)]
        output: PathBuf,
        /// Path to a bundled/signed CoRIM file (COSE_Sign1 with embedded payload).
        /// The tool will internally split it and use the detached signature;
        /// the IGVM file must already contain a matching CoRIM document.
        /// Mutually exclusive with `--corim-signature`.
        ///
        /// Only PS384 (COSE alg -38, RSA-PSS with SHA-384) signatures are
        /// accepted; other algorithms are rejected at verify time.
        #[clap(
            long,
            conflicts_with = "corim_signature",
            required_unless_present = "corim_signature"
        )]
        corim_bundle: Option<PathBuf>,
        /// Path to the CoRIM signature (COSE_Sign1 with nil payload) file.
        /// Requires that a corresponding document already exists in the
        /// file for the same compatibility mask.
        ///
        /// Only PS384 (COSE alg -38, RSA-PSS with SHA-384) signatures are
        /// accepted; other algorithms are rejected at verify time.
        #[clap(long)]
        corim_signature: Option<PathBuf>,
        /// Platform type for the CoRIM headers
        #[clap(long, value_enum)]
        platform: Platform,
    },
}

/// IGVM platform types for CLI selection.
///
/// This is a CLI-friendly adapter for [`IgvmPlatformType`], which is an
/// `open_enum` and cannot derive clap's `ValueEnum` directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Platform {
    /// AMD SEV-SNP
    Snp,
    /// Intel TDX
    Tdx,
    /// VBS (Virtualization Based Security)
    Vbs,
}

impl From<Platform> for IgvmPlatformType {
    fn from(platform: Platform) -> Self {
        match platform {
            Platform::Snp => IgvmPlatformType::SEV_SNP,
            Platform::Tdx => IgvmPlatformType::TDX,
            Platform::Vbs => IgvmPlatformType::VSM_ISOLATION,
        }
    }
}

/// CoRIM header types for filtering
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum CorimHeaderType {
    /// CoRIM document header
    Document,
    /// CoRIM signature header
    Signature,
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
        Options::DumpCorim {
            file_path,
            header_type,
            platform,
            output,
        } => dump_corim_headers(&file_path, header_type, platform, output),
        Options::Manifest {
            manifest,
            resources,
            output,
            debug_validation,
            disable_secure_avic,
        } => {
            // Read the config from the JSON manifest path.
            let mut config: Config = serde_json::from_str(
                &fs_err::read_to_string(manifest).context("reading manifest")?,
            )
            .context("parsing manifest")?;

            if disable_secure_avic {
                for guest_config in &mut config.guest_configs {
                    if let ConfigIsolationType::Snp { secure_avic, .. } =
                        &mut guest_config.isolation_type
                    {
                        *secure_avic = SecureAvicType::Disabled;
                    }
                }
            }

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
        Options::PatchCorimSignature {
            input,
            output,
            corim_bundle,
            corim_signature,
            platform,
        } => patch_corim_signature(input, output, corim_bundle, corim_signature, platform),
    }
}

/// Per-config measurement metadata captured during the build loop and
/// consumed after merging to emit JSON identity documents and CoRIM
/// launch endorsements.
struct PlatformMeta {
    platform: IgvmPlatformType,
    svn: u32,
    debug_enabled: bool,
}

/// Build a sibling path of `output` named `<base>-<isolation><ext>`,
/// where `base` is `output`'s stem, `<isolation>` is derived from
/// `meta.platform`, and `ext` includes the leading dot
/// (e.g. `".json"` or `".cbor"`).
fn sibling_path(
    base: &std::ffi::OsStr,
    output: &std::path::Path,
    meta: &PlatformMeta,
    ext: &str,
) -> PathBuf {
    let isolation = platform_mask::isolation_label(meta.platform);
    let mut name = base.to_os_string();
    name.push("-");
    name.push(isolation);
    name.push(ext);
    output.with_file_name(name)
}

/// Build a `LaunchMeasurement` template for the given platform's
/// launch measurement at the configured guest SVN.
fn build_endorsement_corim(meta: &PlatformMeta) -> anyhow::Result<LaunchMeasurement> {
    let mut le = LaunchMeasurement::for_platform(meta.platform)
        .context("starting CoRIM launch endorsement")?;
    le.set_measurement(MeasurementKind::Launch)
        .context("setting CoRIM launch measurement kind")?;
    le.endorse(meta.svn as u64)
        .with(MeasurementKind::Launch)
        .context("selecting CoRIM measurement in CES triple")?
        .finish()
        .context("finalizing CoRIM CES triple")?;
    Ok(le)
}

/// Build a JSON identity document for a platform measurement.
///
/// The digest is produced by `IgvmSerializer::measurement_for(platform)`
/// which contractually returns 48 bytes for SNP/TDX and 32 bytes for
/// VBS; a length mismatch is an in-tree invariant violation and panics.
/// `platform` is restricted to the three measurable platforms by the
/// only call site (`create_igvm_file`'s `platform_metas` loop); any
/// other value is `unreachable!`.
fn build_endorsement_json(
    platform: IgvmPlatformType,
    digest: &[u8],
    svn: u32,
    debug_enabled: bool,
) -> Measurement {
    match platform {
        IgvmPlatformType::SEV_SNP => {
            let ld: [u8; 48] = digest.try_into().expect("SNP launch digest is 48 bytes");
            Measurement::Snp(SnpMeasurement::new(ld, svn, debug_enabled))
        }
        IgvmPlatformType::TDX => {
            let mrtd: [u8; 48] = digest.try_into().expect("TDX MRTD is 48 bytes");
            Measurement::Tdx(TdxMeasurement::new(mrtd, svn, debug_enabled))
        }
        IgvmPlatformType::VSM_ISOLATION => {
            let boot_digest: [u8; 32] = digest.try_into().expect("VBS boot digest is 32 bytes");
            Measurement::Vbs(VbsMeasurement::new(boot_digest, svn, debug_enabled))
        }
        other => {
            unreachable!("build_endorsement_json called for non-measurable platform {other:?}")
        }
    }
}

/// Write a per-platform endorsement artifact (JSON identity document or
/// CoRIM document) to a sibling of `output` whose name is
/// `<base>-<isolation><ext>`. `ext` must include the leading dot.
fn write_endorsement(
    base: &std::ffi::OsStr,
    output: &std::path::Path,
    meta: &PlatformMeta,
    ext: &str,
    bytes: &[u8],
) -> anyhow::Result<()> {
    let path = sibling_path(base, output, meta, ext);
    tracing::info!(
        path = %path.display(),
        size = bytes.len(),
        "Writing endorsement file",
    );
    fs_err::write(&path, bytes).context("writing endorsement file")?;
    Ok(())
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
    let mut platform_metas: Vec<PlatformMeta> = Vec::new();
    let base_path = output.file_stem().unwrap();
    for config in igvm_config.guest_configs {
        // Max VTL must be 2 or 0.
        if config.max_vtl != 2 && config.max_vtl != 0 {
            bail!("max_vtl must be 2 or 0");
        }

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

        // Track measurement metadata for measurable platforms so the
        // post-merge step can look up the digest from `IgvmSerializer`
        // and emit a JSON identity document + CoRIM launch endorsement.
        //
        // Each measurable platform type may appear at most once across
        // all guest configs: the post-merge step keys both the
        // `IgvmSerializer::measurement_for(platform)` lookup and the
        // `<base>-<isolation>.{cbor,json}` sibling filenames purely by
        // platform type, so a duplicate would silently overwrite the
        // earlier artifacts and could pair the wrong svn/debug bit with
        // the merged measurement. Fail fast here with a clear error.
        let platform = match &loader_isolation_type {
            LoaderIsolationType::Snp { .. } => Some(IgvmPlatformType::SEV_SNP),
            LoaderIsolationType::Tdx { .. } => Some(IgvmPlatformType::TDX),
            LoaderIsolationType::Vbs { .. } => Some(IgvmPlatformType::VSM_ISOLATION),
            LoaderIsolationType::None => None,
        };
        if let Some(platform) = platform
            && platform_metas.iter().any(|m| m.platform == platform)
        {
            bail!(
                "manifest contains more than one guest config for measurable platform {platform:?}; \
                 at most one is supported because endorsement artifacts and the post-merge \
                 measurement lookup are keyed by platform type"
            );
        }
        match &loader_isolation_type {
            LoaderIsolationType::Snp { policy, .. } => {
                platform_metas.push(PlatformMeta {
                    platform: IgvmPlatformType::SEV_SNP,
                    svn: config.guest_svn,
                    debug_enabled: policy.debug() == 1,
                });
            }
            LoaderIsolationType::Tdx { policy } => {
                platform_metas.push(PlatformMeta {
                    platform: IgvmPlatformType::TDX,
                    svn: config.guest_svn,
                    debug_enabled: policy.debug_allowed() == 1,
                });
            }
            LoaderIsolationType::Vbs { enable_debug } => {
                platform_metas.push(PlatformMeta {
                    platform: IgvmPlatformType::VSM_ISOLATION,
                    svn: config.guest_svn,
                    debug_enabled: *enable_debug,
                });
            }
            LoaderIsolationType::None => {}
        }

        // Max VTL of 2 implies paravisor.
        let with_paravisor = config.max_vtl == 2;

        let mut loader = IgvmLoader::<R>::new(with_paravisor, loader_isolation_type);

        load_image(&mut loader.loader(), &config.image, &resources)?;

        let igvm_output = loader.finalize().context("finalizing loader")?;

        // Merge the loaded guest into the overall IGVM file.
        match &mut igvm_file {
            Some(file) => file
                .merge_simple(igvm_output.guest)
                .context("merging guest into overall igvm file")?,
            None => igvm_file = Some(igvm_output.guest),
        }

        map_files.push(igvm_output.map);
    }

    let Some(igvm_file) = igvm_file else {
        bail!("manifest contained no guest configs");
    };

    // Construct the serializer once on the merged IGVM file. This eagerly
    // computes the launch measurement for every measurable platform and
    // serves as the single source of truth for the digest used in both
    // the JSON identity documents and the generated CoRIM documents.
    let mut serializer = IgvmSerializer::new(&igvm_file).context("constructing IGVM serializer")?;

    // For each measurable platform: log the diagnostic, attach a CoRIM
    // launch endorsement, then write both sibling files. The CoRIM is
    // attached before any sibling file is written so a failure in
    // `add_corim` leaves no half-written artifact set on disk.
    for meta in &platform_metas {
        // Snapshot the digest so the immutable borrow on `serializer`
        // ends before the `&mut serializer` call to `add_corim` below.
        let (digest, compatibility_mask) = {
            let m = serializer.measurement_for(meta.platform).with_context(|| {
                format!("no measurement computed for platform {:?}", meta.platform)
            })?;
            (m.digest.clone(), m.compatibility_mask)
        };

        // Emit the platform-specific launch-measurement diagnostic
        // structure (VBS signed data, SNP ID block, TDX MRTD) for human
        // inspection. The digest itself does not depend on these inputs.
        log_measurement_diagnostic(
            meta.platform,
            &digest,
            meta.svn,
            meta.debug_enabled,
            serializer.file(),
            compatibility_mask,
        );

        let corim = build_endorsement_corim(meta)?;
        let corim_bytes = serializer
            .add_corim(meta.platform, corim.build())
            .context("adding CoRIM document to IGVM serializer")?
            .to_vec();

        // Write the CoRIM document first since it is the new artifact
        // downstream signing tooling keys on; the JSON identity document
        // is the legacy companion.
        write_endorsement(base_path, &output, meta, ".cbor", &corim_bytes)?;

        let json = build_endorsement_json(meta.platform, &digest, meta.svn, meta.debug_enabled);
        let mut json_bytes =
            serde_json::to_vec(&json).expect("serializing measurement JSON cannot fail");
        json_bytes.push(b'\n');
        write_endorsement(base_path, &output, meta, ".json", &json_bytes)?;
    }

    let mut igvm_binary = Vec::new();
    serializer
        .serialize(&mut igvm_binary)
        .context("serializing igvm")?;

    // If enabled, perform additional validation by round-tripping the
    // serialized binary through the parser and re-serializer.
    if debug_validation {
        debug_validate_igvm_file(&igvm_binary);
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
    let mut map_file = fs_err::File::create(map_path).context("creating map file")?;

    for map in map_files {
        writeln!(map_file, "{}", map).context("writing map file")?;
    }

    Ok(())
}

/// Dump CoRIM headers from an IGVM file.
fn dump_corim_headers(
    file_path: &std::path::Path,
    header_type_filter: Option<CorimHeaderType>,
    platform_filter: Option<Platform>,
    output_dir: Option<PathBuf>,
) -> anyhow::Result<()> {
    let image = fs_err::read(file_path).context("reading input file")?;

    // Parse the IGVM file using the igvm crate's structured API.
    let igvm_file = IgvmFile::new_from_binary(&image, None).context("parsing IGVM file")?;

    let fixed_header = IGVM_FIXED_HEADER::read_from_prefix(image.as_slice())
        .map_err(|_| anyhow::anyhow!("Invalid IGVM file: cannot read fixed header"))?
        .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)

    println!("IGVM File: {}", file_path.display());
    println!("Total file size: {} bytes", fixed_header.total_file_size);
    println!();

    // The output directory is created lazily on the first matching header
    // so a filter that matches nothing doesn't leave an empty directory
    // behind.
    let mut output_dir_created = false;

    let platforms = igvm_file.platforms();

    // Print the supported platform table
    if !platforms.is_empty() {
        println!("Supported Platforms:");
        for header in platforms {
            match header {
                IgvmPlatformHeader::SupportedPlatform(info) => {
                    println!(
                        "  {:?} -> compatibility_mask 0x{:X}",
                        info.platform_type, info.compatibility_mask
                    );
                }
            }
        }
        println!();
    }

    // Convert platform filter to compatibility mask using the file's actual mapping
    let platform_mask_filter = platform_filter
        .map(|p| platform_mask::lookup_compatibility_mask(platforms, IgvmPlatformType::from(p)))
        .transpose()?;

    // Iterate through initialization headers looking for CoRIM entries
    let mut document_count: usize = 0;
    let mut signature_count: usize = 0;

    for header in igvm_file.initializations() {
        let (kind, label, extension, compatibility_mask, payload) = match header {
            IgvmInitializationHeader::CorimDocument {
                compatibility_mask,
                document,
            } => (
                CorimHeaderType::Document,
                "Document",
                "cbor",
                *compatibility_mask,
                document.as_slice(),
            ),
            IgvmInitializationHeader::CorimSignature {
                compatibility_mask,
                signature,
            } => (
                CorimHeaderType::Signature,
                "Signature",
                "cose",
                *compatibility_mask,
                signature.as_slice(),
            ),
            _ => continue,
        };

        let show_type = header_type_filter.is_none_or(|t| t == kind);
        let show_platform = platform_mask_filter.is_none_or(|mask| compatibility_mask & mask != 0);

        if !show_type || !show_platform {
            continue;
        }

        match kind {
            CorimHeaderType::Document => document_count += 1,
            CorimHeaderType::Signature => signature_count += 1,
        }

        let platform_name = platform_mask::platform_name_for_mask(platforms, compatibility_mask);

        println!("CoRIM {label} ({platform_name}):");
        println!(
            "  Compatibility Mask: 0x{compatibility_mask:X} ({})",
            platform_mask::format_platform_mask(platforms, compatibility_mask)
        );
        println!("  Size: {} bytes", payload.len());

        if let Some(ref dir) = output_dir {
            if !output_dir_created {
                fs_err::create_dir_all(dir).context("creating output directory")?;
                output_dir_created = true;
            }
            let file_prefix = label.to_lowercase();
            let output_file = dir.join(format!("corim_{file_prefix}_{platform_name}.{extension}"));
            fs_err::write(&output_file, payload)
                .with_context(|| format!("writing {label} payload to {}", output_file.display()))?;
            println!("  Output: {}", output_file.display());
        }
        println!();
    }

    if document_count == 0 && signature_count == 0 {
        println!("No CoRIM headers found matching the specified filters.");
    } else {
        println!(
            "Summary: {} document header(s), {} signature header(s)",
            document_count, signature_count
        );
    }

    Ok(())
}

/// Patch a CoRIM signature into an existing IGVM file.
fn patch_corim_signature(
    input: PathBuf,
    output: PathBuf,
    corim_bundle: Option<PathBuf>,
    corim_signature: Option<PathBuf>,
    platform: Platform,
) -> anyhow::Result<()> {
    let igvm_data = fs_err::read(&input)
        .with_context(|| format!("reading input IGVM file at {}", input.display()))?;

    let platform_type = IgvmPlatformType::from(platform);

    // The CoRIM document is expected to already be embedded in the IGVM file
    // for the target platform (auto-generated at build time). `corim_signature::patch`
    // looks it up internally and verifies `signature_data` against it before
    // mutating the file. The issuer certificate is carried in the
    // signature's COSE protected header (x5chain / x5bag, RFC 9360); no
    // separate cert input needed.
    //
    // When the user supplies a `--corim-bundle`, that bundle's embedded
    // payload is the document the signature was actually produced over;
    // we forward it so `patch` can sanity-check it against the
    // IGVM-embedded document and surface a targeted mismatch error
    // instead of an opaque cryptographic verify failure.
    let (signature_data, bundle_document) = if let Some(bundle_path) = &corim_bundle {
        let bundle_data = fs_err::read(bundle_path)
            .with_context(|| format!("reading bundled CoRIM file at {}", bundle_path.display()))?;

        let detached = detach_payload(&bundle_data).context("splitting bundled CoRIM")?;
        tracing::info!(
            path = %bundle_path.display(),
            bundle_size = bundle_data.len(),
            document_size = detached.document.len(),
            signature_size = detached.signature.len(),
            "Split bundled signed CoRIM into document and detached signature"
        );
        (detached.signature, Some(detached.document))
    } else {
        let path = corim_signature.as_ref().expect(
            "caller ensures at least one of --corim-bundle or --corim-signature is provided",
        );
        let sig = fs_err::read(path)
            .with_context(|| format!("reading CoRIM signature file at {}", path.display()))?;
        (sig, None)
    };

    tracing::info!(
        input = %input.display(),
        output = %output.display(),
        bundle = ?corim_bundle,
        signature = ?corim_signature,
        platform = ?platform,
        "Patching CoRIM signature into IGVM file"
    );

    let patched_igvm = corim_signature::patch(
        &igvm_data,
        &signature_data,
        platform_type,
        bundle_document.as_deref(),
    )?;

    // Write output file atomically: write to a temporary file in the same
    // directory, then rename into place. This prevents a crash or
    // interruption during the write from corrupting the output (which may
    // be the same file as the input for in-place edits). Same-volume
    // renames are atomic on both POSIX and Windows (`std::fs::rename`
    // uses `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`).
    let temp_path = {
        // Sibling temp file; not `with_extension`, which would replace any existing ext.
        let mut s = output.as_os_str().to_owned();
        s.push(".tmp");
        PathBuf::from(s)
    };

    tracing::info!(
        path = %output.display(),
        size = patched_igvm.len(),
        "Writing patched IGVM file"
    );
    fs_err::write(&temp_path, &patched_igvm)
        .with_context(|| format!("writing temporary IGVM file at {}", temp_path.display()))?;

    fs_err::rename(&temp_path, &output).with_context(|| {
        format!(
            "renaming temporary file {} to {}",
            temp_path.display(),
            output.display()
        )
    })?;

    Ok(())
}

/// Validate that the serialized IGVM file round-trips through the parser
/// and re-serializer producing identical structural headers.
// TODO: should live in the igvm crate
fn debug_validate_igvm_file(binary_file: &[u8]) {
    use igvm::IgvmDirectiveHeader;
    tracing::info!("Debug validation of serialized IGVM file.");

    let igvm_file =
        IgvmFile::new_from_binary(binary_file, None).expect("first parse should succeed");

    let mut reserialized = Vec::new();
    igvm_file
        .serialize(&mut reserialized)
        .expect("re-serialize should succeed");

    let igvm_reserialized =
        IgvmFile::new_from_binary(&reserialized, None).expect("re-parse should succeed");

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
