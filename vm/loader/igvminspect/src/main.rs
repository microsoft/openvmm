// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A command line tool for inspecting IGVM files.
//!
//! Provides `dump` and `extract` subcommands for examining IGVM files.
//! For generating IGVM files, see `igvmfilegen`.

#![forbid(unsafe_code)]

mod extract;
mod image;

use anyhow::Context;
use clap::Parser;
use igvm::IgvmFile;
use igvm_defs::IGVM_FIXED_HEADER;
pub(crate) use image::read_igvm_image;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use zerocopy::FromBytes;

#[derive(Parser)]
#[clap(name = "igvminspect", about = "Tool to inspect IGVM files")]
enum Options {
    /// Dumps the contents of an IGVM file in a human-readable format
    Dump {
        /// IGVM file or firmware resource DLL to dump
        #[clap(short, long = "filepath")]
        file_path: PathBuf,
    },
    /// Extract the constituent parts of an IGVM file into a directory tree
    Extract {
        /// IGVM file or firmware resource DLL to extract
        #[clap(short, long)]
        file: PathBuf,
        /// Map file (.bin.map) for the IGVM file
        #[clap(short, long)]
        map: Option<PathBuf>,
        /// New output directory to write the extracted parts into (must not exist)
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
        Options::Dump { file_path } => dump_igvm_file(&file_path, std::io::stdout().lock()),
        Options::Extract { file, map, output } => {
            extract::extract_igvm_file(&file, map.as_deref(), &output)
        }
    }
}

fn dump_igvm_file(file_path: &Path, mut output: impl Write) -> anyhow::Result<()> {
    let image = read_igvm_image(file_path)?;
    let (fixed_header, _) = IGVM_FIXED_HEADER::read_from_prefix(image.as_slice())
        .map_err(|e| anyhow::anyhow!("invalid IGVM fixed header: {e}"))?;
    let igvm_data = IgvmFile::new_from_binary(&image, None)
        .with_context(|| format!("parsing IGVM file {}", file_path.display()))?;

    writeln!(
        output,
        "Total file size: {} bytes\n\n{:#X?}\n{}",
        fixed_header.total_file_size, fixed_header, igvm_data
    )
    .context("writing IGVM dump")
}
