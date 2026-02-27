// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A command line tool for inspecting IGVM files.
//!
//! Provides `dump` and `extract` subcommands for examining IGVM files.
//! For generating IGVM files, see `igvmfilegen`.

#![forbid(unsafe_code)]

mod extract;

use anyhow::Context;
use clap::Parser;
use igvm::IgvmFile;
use igvm_defs::IGVM_FIXED_HEADER;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

#[derive(Parser)]
#[clap(name = "igvminspect", about = "Tool to inspect IGVM files")]
enum Options {
    /// Dumps the contents of an IGVM file in a human-readable format
    Dump {
        /// Dump file path
        #[clap(short, long = "filepath")]
        file_path: PathBuf,
    },
    /// Extract the constituent parts of an IGVM file into a directory tree
    Extract {
        /// IGVM file to extract
        #[clap(short, long)]
        file: PathBuf,
        /// Map file (.bin.map) for the IGVM file
        #[clap(short, long)]
        map: Option<PathBuf>,
        /// Output directory to write the extracted parts into
        #[clap(short, long)]
        output: PathBuf,
    },
}

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
            let (fixed_header, _) = IGVM_FIXED_HEADER::read_from_prefix(image.as_bytes())
                .map_err(|e| anyhow::anyhow!("invalid IGVM fixed header: {e}"))?;

            let igvm_data = IgvmFile::new_from_binary(&image, None)
                .map_err(|e| anyhow::anyhow!("failed to parse IGVM file: {e:?}"))?;
            println!("Total file size: {} bytes\n", fixed_header.total_file_size);
            println!("{:#X?}", fixed_header);
            println!("{}", igvm_data);
            Ok(())
        }
        Options::Extract { file, map, output } => {
            extract::extract_igvm_file(&file, map.as_deref(), &output)
        }
    }
}
