// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Supported command-line interface for creating and inspecting vTPM artifacts.

use clap::Parser;
use clap::Subcommand;

#[derive(Parser, Debug)]
#[command(
    name = "vtpm_util",
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
    },
    /// Writes the SRK public key in TPM2B format.
    WriteSrk {
        #[arg(value_name = "path-to-vtpm-blob-file")]
        vtpm_blob_path: String,
        #[arg(value_name = "path-to-srk-out-file")]
        srk_out_path: String,
    },
    /// Prints the TPM key name of an SRK public key file.
    PrintKeyName {
        #[arg(value_name = "path-to-srk-pub")]
        srk_pub_path: String,
    },
    /// Creates a random RSA or ECC key in TPM2 import blob format.
    CreateRandomKeyInTpm2ImportBlobFormat {
        #[arg(value_name = "algorithm")]
        algorithm: String,
        #[arg(value_name = "public-key")]
        public_key_file: String,
        #[arg(value_name = "output-file")]
        private_key_tpm2b_file: String,
    },
}

fn main() {
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
        Command::CreateVtpmBlob { path } => vtpm_util::create_vtpm_blob_file(&path),
        Command::WriteSrk {
            vtpm_blob_path,
            srk_out_path,
        } => vtpm_util::write_srk(&vtpm_blob_path, &srk_out_path),
        Command::PrintKeyName { srk_pub_path } => vtpm_util::print_key_name(&srk_pub_path),
        Command::CreateRandomKeyInTpm2ImportBlobFormat {
            algorithm,
            public_key_file,
            private_key_tpm2b_file,
        } => vtpm_util::create_random_key_in_tpm2_import_blob_format(
            &algorithm,
            &public_key_file,
            &private_key_tpm2b_file,
        ),
    }
}
