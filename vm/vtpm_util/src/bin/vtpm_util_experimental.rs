// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Experimental command-line interface for vTPM development and diagnostics.

use clap::Parser;
use clap::Subcommand;

#[derive(Parser, Debug)]
#[command(
    name = "vtpm_util_experimental",
    about = "Experimental vTPM development and diagnostic tools."
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
    /// Writes the SRK template in Ubuntu-compatible format.
    WriteSrkTemplate {
        #[arg(value_name = "path-to-template-file")]
        template_path: String,
    },
    /// Recreates the SRK from a vTPM blob to verify deterministic generation.
    RecreateSrk {
        #[arg(value_name = "path-to-vtpm-blob-file")]
        vtpm_blob_path: String,
    },
    /// Prints information about a public key in DER format.
    PrintDer {
        #[arg(value_name = "path-to-public-key-der")]
        public_key_path: String,
    },
    /// Prints information about a private key in TPM2B import format.
    PrintTpm2b {
        #[arg(value_name = "path-to-private-key-tpm2b")]
        private_key_path: String,
    },
    /// Tests whether DER public and TPM2B private keys form a keypair.
    TestTpm2bImportKeys {
        #[arg(value_name = "path-to-public-key-der")]
        public_key_file: String,
        #[arg(value_name = "path-to-private-key-tpm2b")]
        private_key_file: String,
    },
    /// Imports a sealed key blob into an existing vTPM blob.
    TpmImport {
        #[arg(value_name = "path-to-vtpm-blob-file")]
        vtpm_blob_path: String,
        #[arg(value_name = "path-to-sealed-key-file")]
        sealed_key_path: String,
    },
    /// Exports a newly generated TPM key as a sealed key blob.
    TpmKeyExport {
        #[arg(value_name = "path-to-vtpm-blob-file")]
        vtpm_blob_path: String,
        #[arg(value_name = "key-handle-or-persistent-handle")]
        key_handle: String,
        #[arg(value_name = "path-to-sealed-key-output-file")]
        sealed_key_output_path: String,
    },
    /// Starts a TPM socket server using a vTPM blob as backing state.
    SocketServer {
        #[arg(value_name = "path-to-vtpm-blob-file")]
        vtpm_blob_path: String,
        #[arg(value_name = "host:port")]
        bind_addr: String,
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
        Command::WriteSrkTemplate { template_path } => {
            vtpm_util::write_srk_template(&template_path)
        }
        Command::RecreateSrk { vtpm_blob_path } => vtpm_util::recreate_srk(&vtpm_blob_path),
        Command::PrintDer { public_key_path } => vtpm_util::print_public_key_der(&public_key_path),
        Command::PrintTpm2b { private_key_path } => {
            vtpm_util::print_tpm2b_import_content(&private_key_path)
        }
        Command::TestTpm2bImportKeys {
            public_key_file,
            private_key_file,
        } => vtpm_util::test_tpm2b_import_keys(&public_key_file, &private_key_file),
        Command::TpmImport {
            vtpm_blob_path,
            sealed_key_path,
        } => vtpm_util::import_sealed_key_blob_into_vtpm(&vtpm_blob_path, &sealed_key_path),
        Command::TpmKeyExport {
            vtpm_blob_path,
            key_handle: _,
            sealed_key_output_path,
        } => vtpm_util::export_tpm_key_as_sealed_blob(&vtpm_blob_path, &sealed_key_output_path),
        Command::SocketServer {
            vtpm_blob_path,
            bind_addr,
        } => vtpm_util::start_tpm_socket_server(&vtpm_blob_path, &bind_addr),
    }
}
