// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CLI for preparing and inspecting pre-provisioned vTPM NVRAM state blobs.
//!
//! The blob produced by `prepare` is what the vTPM device reads out of the VMGS
//! `TPM_NVRAM` (v1.38) or `TPM_185_NVRAM` (v1.85) file at boot, so it can be
//! written into a VMGS with `vmgstool write`:
//!
//! ```text
//! tpm_utils prepare --tpm-version 1.85 -o vtpm.blob
//! vmgstool write --file-path disk.vmgs --file-id TPM_185_NVRAM --data-path vtpm.blob
//! ```

#![forbid(unsafe_code)]

#[cfg(feature = "tpm")]
mod engine;
#[cfg(feature = "tpm")]
mod inspect;
#[cfg(feature = "tpm")]
mod provision;

#[cfg(feature = "tpm")]
mod cli {
    use crate::inspect;
    use crate::provision;
    use crate::provision::AkCertIndexKind;
    use anyhow::Context as _;
    use clap::Parser;
    use clap::Subcommand;
    use std::path::PathBuf;
    use tpm_resources::TpmVersion;

    #[derive(Copy, Clone, Debug, clap::ValueEnum)]
    enum Version {
        #[value(name = "1.38", alias = "138")]
        V138,
        #[value(name = "1.85", alias = "185")]
        V185,
    }

    impl From<Version> for TpmVersion {
        fn from(version: Version) -> Self {
            match version {
                Version::V138 => TpmVersion::V138,
                Version::V185 => TpmVersion::V185,
            }
        }
    }

    #[derive(Parser)]
    #[command(name = "tpm_utils", about, long_about = None)]
    struct Options {
        /// Print the TPM library's trace output.
        #[clap(long, short, global = true)]
        verbose: bool,

        #[clap(subcommand)]
        command: Command,
    }

    #[derive(Subcommand)]
    enum Command {
        /// Provision a TPM and export its NVRAM state.
        Prepare {
            /// TPM reference implementation to provision against.
            #[clap(long, value_enum)]
            tpm_version: Version,

            /// Where to write the NVRAM blob.
            #[clap(long, short)]
            output: PathBuf,

            /// Size of the NVRAM region. Defaults to the size the reference
            /// implementation was compiled for.
            #[clap(long)]
            nvram_size: Option<usize>,

            /// Skip creating the AK.
            #[clap(long)]
            no_ak: bool,

            /// Skip creating the SRK.
            #[clap(long)]
            no_srk: bool,

            /// Ownership of the AK cert NV index.
            #[clap(long, value_enum, default_value = "owner")]
            ak_cert_index: AkCertIndexKind,

            /// File holding the AK cert to write into the NV index. When
            /// omitted, the index is created but left uninitialized.
            #[clap(long)]
            ak_cert: Option<PathBuf>,

            /// Size of the AK cert NV index. Defaults to the AK cert size, or
            /// 4096 when no AK cert is supplied.
            #[clap(long)]
            ak_cert_index_size: Option<u16>,

            /// Password authorization for a platform-created AK cert index.
            #[clap(long, default_value_t = 0, value_parser = parse_auth_value)]
            auth_value: u64,

            /// Create the small-vTPM mitigation marker NV index.
            #[clap(long)]
            mitigation_marker: bool,
        },

        /// Print a summary of an existing NVRAM blob.
        Inspect {
            /// TPM reference implementation the blob was produced for.
            #[clap(long, value_enum)]
            tpm_version: Version,

            /// The NVRAM blob to inspect.
            blob: PathBuf,
        },
    }

    fn parse_auth_value(s: &str) -> Result<u64, std::num::ParseIntError> {
        match s.strip_prefix("0x") {
            Some(hex) => u64::from_str_radix(hex, 16),
            None => s.parse(),
        }
    }

    pub fn run() -> anyhow::Result<()> {
        let options = Options::parse();

        if options.verbose {
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(std::io::stderr)
                .init();
        }

        match options.command {
            Command::Prepare {
                tpm_version,
                output,
                nvram_size,
                no_ak,
                no_srk,
                ak_cert_index,
                ak_cert,
                ak_cert_index_size,
                auth_value,
                mitigation_marker,
            } => {
                let version = TpmVersion::from(tpm_version);

                anyhow::ensure!(
                    ak_cert_index != AkCertIndexKind::None
                        || (ak_cert.is_none() && ak_cert_index_size.is_none()),
                    "--ak-cert and --ak-cert-index-size have no effect with \
                     --ak-cert-index none"
                );

                let ak_cert = ak_cert
                    .map(|path| fs_err::read(&path))
                    .transpose()
                    .context("failed to read the AK cert")?;

                let blob = provision::provision(&provision::ProvisionParams {
                    version,
                    nvram_size: nvram_size
                        .unwrap_or_else(|| crate::engine::default_nvram_size(version)),
                    ak: !no_ak,
                    srk: !no_srk,
                    ak_cert_index,
                    ak_cert_index_size,
                    ak_cert,
                    auth_value,
                    mitigation_marker,
                })?;

                fs_err::write(&output, &blob).context("failed to write the NVRAM blob")?;

                println!("wrote {} bytes to {}", blob.len(), output.display());
                println!(
                    "write it into a VMGS with: vmgstool write --file-path <vmgs> --file-id {:?} --data-path {}",
                    version.to_nvram_vmgs_file_id(),
                    output.display()
                );
            }
            Command::Inspect { tpm_version, blob } => {
                let data = fs_err::read(&blob).context("failed to read the NVRAM blob")?;
                inspect::inspect(tpm_version.into(), &data)?;
            }
        }

        Ok(())
    }
}

#[cfg(feature = "tpm")]
fn main() -> anyhow::Result<()> {
    cli::run()
}

#[cfg(not(feature = "tpm"))]
fn main() {
    eprintln!("tpm_utils was built without the `tpm` feature and cannot do anything");
    std::process::exit(1);
}
