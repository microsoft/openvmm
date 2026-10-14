// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Supported command-line interface for creating and inspecting vTPM artifacts.

#[cfg(not(test))]
crypto::ensure_single_backend!();

use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;

#[derive(Clone, Debug, ValueEnum)]
enum KeyAlgorithm {
    Rsa,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum TpmVersion {
    #[default]
    #[value(name = "1.38")]
    V138,
    #[value(name = "1.85")]
    V185,
}

impl From<TpmVersion> for vtpm_util::TpmVersion {
    fn from(value: TpmVersion) -> Self {
        match value {
            TpmVersion::V138 => Self::V138,
            TpmVersion::V185 => Self::V185,
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "vtpm-util",
    about = "Tool to create and inspect vTPM artifacts."
)]
struct CmdArgs {
    /// Enable verbose logging (trace level).
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
#[command(rename_all = "kebab-case")]
enum Command {
    /// Creates a vTPM blob and stores it in a file.
    CreateVtpmBlob {
        #[arg(value_name = "path-to-blob-file")]
        path: String,
        /// Microsoft TPM reference implementation version.
        #[arg(long, value_enum, default_value_t)]
        tpm_version: TpmVersion,
    },
    /// Writes the SRK public key in TPM2B format.
    WriteSrk {
        #[arg(value_name = "path-to-vtpm-blob-file")]
        vtpm_blob_path: String,
        #[arg(value_name = "path-to-srk-out-file")]
        srk_out_path: String,
        /// Microsoft TPM reference implementation version.
        #[arg(long, value_enum, default_value_t)]
        tpm_version: TpmVersion,
    },
    /// Prints the TPM key name of an SRK public key file.
    PrintKeyName {
        #[arg(value_name = "path-to-srk-pub")]
        srk_pub_path: String,
    },
    /// Creates a random RSA key in TPM2 import blob format.
    CreateRandomKeyInTpm2ImportBlobFormat {
        #[arg(value_name = "algorithm")]
        algorithm: KeyAlgorithm,
        #[arg(value_name = "public-key")]
        public_key_file: String,
        #[arg(value_name = "output-file")]
        private_key_tpm2b_file: String,
    },
}

fn main() -> anyhow::Result<()> {
    let args = CmdArgs::parse();

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .log_internal_errors(true)
        .with_max_level(if args.verbose {
            tracing::Level::TRACE
        } else {
            tracing::Level::INFO
        })
        .init();

    match args.command {
        Command::CreateVtpmBlob { path, tpm_version } => {
            vtpm_util::create_vtpm_blob_file_with_version(&path, tpm_version.into())
        }
        Command::WriteSrk {
            vtpm_blob_path,
            srk_out_path,
            tpm_version,
        } => vtpm_util::write_srk_with_version(&vtpm_blob_path, &srk_out_path, tpm_version.into()),
        Command::PrintKeyName { srk_pub_path } => vtpm_util::print_key_name(&srk_pub_path),
        Command::CreateRandomKeyInTpm2ImportBlobFormat {
            algorithm,
            public_key_file,
            private_key_tpm2b_file,
        } => match algorithm {
            KeyAlgorithm::Rsa => vtpm_util::create_random_rsa_key_in_tpm2_import_blob_format(
                &public_key_file,
                &private_key_tpm2b_file,
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_blob_defaults_to_tpm_138() {
        let args = CmdArgs::try_parse_from(["vtpm-util", "create-vtpm-blob", "vtpm.blob"])
            .expect("arguments should parse");

        assert!(matches!(
            args.command,
            Command::CreateVtpmBlob {
                tpm_version: TpmVersion::V138,
                ..
            }
        ));
    }

    #[test]
    fn create_blob_accepts_tpm_185() {
        let args = CmdArgs::try_parse_from([
            "vtpm-util",
            "create-vtpm-blob",
            "vtpm.blob",
            "--tpm-version",
            "1.85",
        ])
        .expect("arguments should parse");

        assert!(matches!(
            args.command,
            Command::CreateVtpmBlob {
                tpm_version: TpmVersion::V185,
                ..
            }
        ));
    }
}
