// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command-line tools for creating, inspecting, validating, and converting
//! VHDX virtual disk images.
//!
//! `openvmm-img` is a cross-platform frontend for the [`vhdx`] crate. It supports
//! dynamic, fixed, and differencing VHDX images through seven commands:
//!
//! - `create` creates a new image and records parent locator metadata for
//!   differencing disks.
//! - `info` reports image geometry, identifiers, allocation type, and parent
//!   information.
//! - `map` reports allocated and unallocated virtual disk ranges.
//! - `convert` copies raw or VHDX input into sparse raw, dynamic VHDX, or fixed
//!   VHDX output.
//! - `check` validates VHDX metadata and follows the complete differencing
//!   parent chain.
//! - `replay` replays a dirty VHDX write-ahead log and leaves the image clean.
//! - `format-options` displays the format-specific options accepted by `create`
//!   and `convert`.
//!
//! Run `openvmm-img --help` or `openvmm-img <command> --help` for command syntax and
//! option details.
//!
//! # I/O model
//!
//! The [`vhdx`] crate manages image metadata but returns payload mappings to
//! its caller. This tool performs the corresponding positional file I/O for
//! each mapping and completes every VHDX write guard after the payload has
//! been written. Conversion skips all-zero chunks so that dynamic VHDX and raw
//! output remain sparse where the host filesystem supports sparse files.
//!
//! Differencing disks store the parent's data-write GUID and, when available,
//! paths to the parent. `check` resolves and validates parent chains, but
//! conversion rejects differencing inputs because the tool does not merge
//! parent payload data into child reads.
//!
//! # Exit status
//!
//! Successful commands exit with status 0. Invalid arguments and operational
//! failures exit with status 1. `check` exits with status 2 when an image or
//! its parent chain is inconsistent. A dirty log is reported as a warning and
//! can be repaired with `replay`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod file;
mod util;
mod vhdx;

use ::vhdx::AsyncFile;
use ::vhdx::VhdxFile;
use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use pal_async::DefaultPool;
use std::path::PathBuf;

use crate::file::BlockingFile;
use crate::vhdx::CreateOptions;
use crate::vhdx::DiskType;
use crate::vhdx::InconsistentImage;
use crate::vhdx::VhdxAllocation;
use crate::vhdx::VhdxConvertFormatOptions;
use crate::vhdx::create_image;
use crate::vhdx::read_chunk as read_vhdx_chunk;
use crate::vhdx::write_chunk as write_vhdx_chunk;

#[cfg(test)]
use crate::vhdx::MapRun;
#[cfg(test)]
use crate::vhdx::VhdxCreateFormatOptions;
#[cfg(test)]
use crate::vhdx::check as test_check;
#[cfg(test)]
use crate::vhdx::disk_type as vhdx_disk_type;
#[cfg(test)]
use crate::vhdx::parse_convert_format_options as parse_vhdx_convert_format_options;
#[cfg(test)]
use crate::vhdx::parse_create_format_options as parse_vhdx_create_format_options;
#[cfg(test)]
use crate::vhdx::replay as test_replay;
#[cfg(test)]
use crate::vhdx::resolve_parent_path;
#[cfg(test)]
use crate::vhdx::visit_map_runs;
#[cfg(test)]
use ::vhdx::CreateParams;
#[cfg(test)]
use ::vhdx::DiskType as VhdxDiskType;
#[cfg(test)]
use ::vhdx::VhdxParent;
#[cfg(test)]
use guid::Guid;

#[derive(Clone, Copy, ValueEnum)]
enum ImageFormat {
    /// A headerless byte-for-byte disk image.
    Raw,
    /// A VHDX virtual disk image.
    Vhdx,
}

#[derive(Clone, Copy, ValueEnum)]
enum CreateFormat {
    /// A VHDX virtual disk image.
    Vhdx,
}

/// Create, inspect, validate, and convert disk images.
#[derive(Parser)]
#[command(name = "openvmm-img")]
struct CliArgs {
    /// Show detailed progress and diagnostics.
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a disk image.
    Create {
        /// Path of the disk image to create.
        file: PathBuf,
        /// Format of the image to create. Inferred from the file extension when omitted.
        #[arg(long, value_enum)]
        format: Option<CreateFormat>,
        /// Virtual disk size. Accepts binary suffixes such as K, M, G, and T.
        #[arg(long, value_parser = util::parse_size)]
        size: u64,
        /// Output-format-specific key-value options.
        #[arg(long, value_name = "KEY=VALUE,...")]
        format_options: Option<String>,
        /// Parent image path. Creates a differencing image.
        #[arg(long)]
        parent: Option<PathBuf>,
        /// Logical sector size in bytes: 512 or 4096.
        #[arg(long)]
        logical_sector_size: Option<u32>,
        /// Physical sector size in bytes: 512 or 4096.
        #[arg(long)]
        physical_sector_size: Option<u32>,
        /// Replace the output file if it already exists.
        #[arg(short, long)]
        force: bool,
    },
    /// Display VHDX geometry, identifiers, type, and parent information.
    Info {
        /// VHDX image to inspect.
        file: PathBuf,
        /// Emit machine-readable JSON instead of a human-readable summary.
        #[arg(long)]
        json: bool,
    },
    /// Display allocated and unallocated virtual disk ranges.
    Map {
        /// VHDX image whose allocation map will be displayed.
        file: PathBuf,
        /// Emit machine-readable JSON instead of a human-readable table.
        #[arg(long)]
        json: bool,
    },
    /// Convert raw and VHDX images, including VHDX allocation-type changes.
    Convert {
        /// Source image to convert.
        input: PathBuf,
        /// Path of the converted image.
        #[arg(short, long)]
        output: PathBuf,
        /// Source format. Inferred from a .vhdx extension when omitted.
        #[arg(long, value_enum)]
        input_format: Option<ImageFormat>,
        /// Format of the converted image.
        #[arg(long, value_enum)]
        output_format: ImageFormat,
        /// Output-format-specific key-value options.
        #[arg(long, value_name = "KEY=VALUE,...")]
        format_options: Option<String>,
        /// Replace the output file if it already exists.
        #[arg(short, long)]
        force: bool,
    },
    /// Validate VHDX metadata and the complete differencing parent chain.
    Check {
        /// VHDX image to validate.
        file: PathBuf,
    },
    /// Replay a dirty VHDX write-ahead log and leave the image clean.
    Replay {
        /// VHDX image whose log will be checked or replayed.
        file: PathBuf,
        /// Report whether replay is required without modifying the image.
        #[arg(long)]
        dry_run: bool,
    },
    /// Display the options accepted by an image format.
    FormatOptions {
        /// Image format whose options will be displayed. Lists formats when omitted.
        #[arg(value_enum)]
        format: Option<ImageFormat>,
    },
}

fn main() {
    let args = CliArgs::try_parse().unwrap_or_else(|error| {
        let exit_code = i32::from(error.use_stderr());
        if error.print().is_err() {
            std::process::exit(1);
        }
        std::process::exit(exit_code);
    });
    init_tracing(args.verbose);
    let result = DefaultPool::run_with(async |driver| run(args.command, &driver).await);
    if let Err(error) = result {
        eprintln!("Error: {error:#}");
        std::process::exit(if error.downcast_ref::<InconsistentImage>().is_some() {
            2
        } else {
            1
        });
    }
}

fn init_tracing(verbose: bool) {
    tracing_subscriber::fmt()
        .with_max_level(if verbose {
            tracing::Level::TRACE
        } else {
            tracing::Level::INFO
        })
        .init();
}

async fn run(command: Command, driver: &impl pal_async::task::Spawn) -> Result<()> {
    match command {
        Command::Create {
            file,
            format,
            size,
            format_options,
            parent,
            logical_sector_size,
            physical_sector_size,
            force,
        } => {
            let CreateFormat::Vhdx = format
                .map(Ok)
                .unwrap_or_else(|| infer_create_format(&file))?;
            let format_options = vhdx::parse_create_format_options(format_options.as_deref())?;
            create_image(CreateOptions {
                file,
                size,
                disk_type: vhdx::disk_type(format_options.output.allocation, parent.is_some())?,
                parent,
                block_size: format_options.output.block_size.map(|size| size.0),
                logical_sector_size,
                physical_sector_size,
                block_alignment: format_options.block_alignment.map(|size| size.0),
                page83: format_options.page83,
                force,
            })
            .await
        }
        Command::Info { file, json } => vhdx::info(&file, json).await,
        Command::Map { file, json } => vhdx::map(&file, json).await,
        Command::Convert {
            input,
            output,
            input_format,
            output_format,
            format_options,
            force,
        } => {
            let format_options = match output_format {
                ImageFormat::Raw => {
                    anyhow::ensure!(
                        format_options.is_none(),
                        "raw output does not accept --format-options"
                    );
                    VhdxConvertFormatOptions::default()
                }
                ImageFormat::Vhdx => vhdx::parse_convert_format_options(format_options.as_deref())?,
            };
            convert(
                &input,
                &output,
                input_format
                    .or_else(|| infer_format(&input))
                    .with_context(|| {
                        format!(
                            "cannot infer image format from {}; specify --input-format",
                            input.display()
                        )
                    })?,
                output_format,
                match format_options.output.allocation {
                    VhdxAllocation::Dynamic => DiskType::Dynamic,
                    VhdxAllocation::Fixed => DiskType::Fixed,
                },
                format_options.output.block_size.map(|size| size.0),
                force,
                driver,
            )
            .await
        }
        Command::Check { file } => vhdx::check(&file).await,
        Command::Replay { file, dry_run } => vhdx::replay(&file, dry_run, driver).await,
        Command::FormatOptions { format } => print_format_options(format),
    }
}

fn print_format_options(format: Option<ImageFormat>) -> Result<()> {
    match format {
        None => {
            println!("Formats:");
            println!("  raw   No format-specific options");
            println!("  vhdx  VHDX creation and conversion options");
        }
        Some(ImageFormat::Raw) => println!("Raw images have no format-specific options."),
        Some(ImageFormat::Vhdx) => return vhdx::print_format_options(),
    }
    Ok(())
}

fn infer_format(path: &std::path::Path) -> Option<ImageFormat> {
    let extension = path.extension()?;
    if extension.eq_ignore_ascii_case("vhdx") {
        Some(ImageFormat::Vhdx)
    } else if extension.eq_ignore_ascii_case("raw") || extension.eq_ignore_ascii_case("img") {
        Some(ImageFormat::Raw)
    } else {
        None
    }
}

fn infer_create_format(path: &std::path::Path) -> Result<CreateFormat> {
    match infer_format(path) {
        Some(ImageFormat::Vhdx) => Ok(CreateFormat::Vhdx),
        Some(ImageFormat::Raw) => anyhow::bail!("creating raw images is not supported"),
        None => anyhow::bail!(
            "cannot infer image format from {}; specify --format",
            path.display()
        ),
    }
}

async fn convert(
    input_path: &std::path::Path,
    output_path: &std::path::Path,
    input_format: ImageFormat,
    output_format: ImageFormat,
    disk_type: DiskType,
    block_size: Option<u64>,
    force: bool,
    driver: &impl pal_async::task::Spawn,
) -> Result<()> {
    anyhow::ensure!(
        !matches!(disk_type, DiskType::Differencing),
        "convert output cannot be differencing without a parent"
    );
    anyhow::ensure!(
        !matches!(
            (input_format, output_format),
            (ImageFormat::Raw, ImageFormat::Raw)
        ),
        "raw-to-raw conversion is not supported"
    );

    match input_format {
        ImageFormat::Raw => {
            let input = BlockingFile::open(input_path, true)
                .with_context(|| format!("failed to open {}", input_path.display()))?;
            let disk_size = input
                .file_size()
                .await
                .context("failed to read raw file size")?;
            create_image(CreateOptions {
                file: output_path.to_owned(),
                size: disk_size,
                disk_type,
                parent: None,
                block_size,
                logical_sector_size: None,
                physical_sector_size: None,
                block_alignment: None,
                page83: None,
                force,
            })
            .await?;
            let output_file = BlockingFile::open(output_path, false)?;
            let output_payload = output_file.clone();
            let output = VhdxFile::open(output_file)
                .writable(driver)
                .await
                .context("failed to open output VHDX")?;
            copy_raw_to_vhdx(&input, &output, &output_payload).await?;
            output
                .close()
                .await
                .context("failed to close output VHDX")?;
        }
        ImageFormat::Vhdx => {
            let input_file = BlockingFile::open(input_path, true)
                .with_context(|| format!("failed to open {}", input_path.display()))?;
            let input_payload = input_file.clone();
            let input = VhdxFile::open(input_file)
                .read_only()
                .await
                .context("failed to open input VHDX")?;
            anyhow::ensure!(
                !input.has_parent(),
                "converting differencing VHDX inputs is not supported"
            );
            match output_format {
                ImageFormat::Raw => {
                    let output = BlockingFile::create(output_path, force)
                        .with_context(|| format!("failed to create {}", output_path.display()))?;
                    output
                        .set_file_size(input.disk_size())
                        .await
                        .context("failed to size raw output")?;
                    copy_vhdx_to_raw(&input, &input_payload, &output).await?;
                    output.flush().await.context("failed to flush raw output")?;
                }
                ImageFormat::Vhdx => {
                    create_image(CreateOptions {
                        file: output_path.to_owned(),
                        size: input.disk_size(),
                        disk_type,
                        parent: None,
                        block_size,
                        logical_sector_size: Some(input.logical_sector_size()),
                        physical_sector_size: Some(input.physical_sector_size()),
                        block_alignment: None,
                        page83: None,
                        force,
                    })
                    .await?;
                    let output_file = BlockingFile::open(output_path, false)?;
                    let output_payload = output_file.clone();
                    let output = VhdxFile::open(output_file)
                        .writable(driver)
                        .await
                        .context("failed to open output VHDX")?;
                    copy_vhdx_to_vhdx(&input, &input_payload, &output, &output_payload).await?;
                    output
                        .close()
                        .await
                        .context("failed to close output VHDX")?;
                }
            }
        }
    }
    println!(
        "Converted {} to {}",
        input_path.display(),
        output_path.display()
    );
    Ok(())
}

const COPY_CHUNK_SIZE: u64 = 1024 * 1024;

async fn copy_raw_to_vhdx(
    input: &BlockingFile,
    output: &VhdxFile<BlockingFile>,
    output_payload: &BlockingFile,
) -> Result<()> {
    let mut offset = 0;
    while offset < output.disk_size() {
        let length = (output.disk_size() - offset).min(COPY_CHUNK_SIZE) as usize;
        let data = input
            .read_into(offset, vec![0; length])
            .await
            .context("failed to read raw input")?;
        if data.iter().any(|byte| *byte != 0) {
            write_vhdx_chunk(output, output_payload, offset, &data).await?;
        }
        offset += length as u64;
        tracing::debug!(
            bytes = offset,
            total = output.disk_size(),
            "conversion progress"
        );
    }
    Ok(())
}

async fn copy_vhdx_to_raw(
    input: &VhdxFile<BlockingFile>,
    input_payload: &BlockingFile,
    output: &BlockingFile,
) -> Result<()> {
    let mut offset = 0;
    while offset < input.disk_size() {
        let length = (input.disk_size() - offset).min(COPY_CHUNK_SIZE) as u32;
        let data = read_vhdx_chunk(input, input_payload, offset, length).await?;
        if data.iter().any(|byte| *byte != 0) {
            output
                .write_from(offset, data)
                .await
                .context("failed to write raw output")?;
        }
        offset += length as u64;
        tracing::debug!(
            bytes = offset,
            total = input.disk_size(),
            "conversion progress"
        );
    }
    Ok(())
}

async fn copy_vhdx_to_vhdx(
    input: &VhdxFile<BlockingFile>,
    input_payload: &BlockingFile,
    output: &VhdxFile<BlockingFile>,
    output_payload: &BlockingFile,
) -> Result<()> {
    let mut offset = 0;
    while offset < input.disk_size() {
        let length = (input.disk_size() - offset).min(COPY_CHUNK_SIZE) as u32;
        let data = read_vhdx_chunk(input, input_payload, offset, length).await?;
        if data.iter().any(|byte| *byte != 0) {
            write_vhdx_chunk(output, output_payload, offset, &data).await?;
        }
        offset += length as u64;
        tracing::debug!(
            bytes = offset,
            total = input.disk_size(),
            "conversion progress"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn parent_lookup_ignores_windows_paths() {
        let directory = tempfile::tempdir().unwrap();
        let parent_path = directory.path().join("parent.vhdx");
        let child_path = directory.path().join("child.vhdx");
        std::fs::write(&parent_path, []).unwrap();
        let parent = VhdxParent::new(Guid::new_random())
            .unwrap()
            .with_volume_path(parent_path.to_str().unwrap())
            .unwrap()
            .with_absolute_win32_path(parent_path.to_str().unwrap())
            .unwrap();
        assert_eq!(resolve_parent_path(&child_path, &parent), None);
        let parent = parent.with_relative_path(r".\parent.vhdx").unwrap();
        assert_eq!(resolve_parent_path(&child_path, &parent), Some(parent_path));
    }

    #[test]
    fn parses_short_and_long_output_and_force_options() {
        for (output_option, force_option) in [("-o", "-f"), ("--output", "--force")] {
            let args = CliArgs::try_parse_from([
                "openvmm-img",
                "create",
                "disk.vhdx",
                "--size",
                "4M",
                force_option,
            ])
            .unwrap();
            assert!(matches!(
                args.command,
                Command::Create {
                    format: None,
                    force: true,
                    ..
                }
            ));

            let args = CliArgs::try_parse_from([
                "openvmm-img",
                "convert",
                "disk.raw",
                output_option,
                "disk.vhdx",
                "--output-format",
                "vhdx",
                force_option,
            ])
            .unwrap();
            assert!(matches!(
                args.command,
                Command::Convert { output, force: true, .. }
                    if output.as_os_str() == "disk.vhdx"
            ));
        }
    }

    #[test]
    fn infers_create_format_from_extension() {
        assert!(matches!(
            infer_create_format(std::path::Path::new("disk.VHDX")),
            Ok(CreateFormat::Vhdx)
        ));
        assert!(matches!(
            infer_format(std::path::Path::new("disk.raw")),
            Some(ImageFormat::Raw)
        ));
        assert!(matches!(
            infer_format(std::path::Path::new("disk.img")),
            Some(ImageFormat::Raw)
        ));
        let error = infer_create_format(std::path::Path::new("disk.unknown"))
            .err()
            .expect("unknown extension should fail");
        assert!(error.to_string().contains("specify --format"));
        assert!(infer_format(std::path::Path::new("disk.unknown")).is_none());
    }

    #[test]
    fn format_options_accepts_an_optional_format() {
        let args = CliArgs::try_parse_from(["openvmm-img", "format-options"]).unwrap();
        assert!(matches!(
            args.command,
            Command::FormatOptions { format: None }
        ));

        let args = CliArgs::try_parse_from(["openvmm-img", "format-options", "vhdx"]).unwrap();
        assert!(matches!(
            args.command,
            Command::FormatOptions {
                format: Some(ImageFormat::Vhdx)
            }
        ));
    }

    #[test]
    fn parses_vhdx_format_options_by_command() {
        let create = parse_vhdx_create_format_options(Some(
            "allocation=fixed,block_size=4M,block_alignment=1G,page83=00112233-4455-6677-8899-aabbccddeeff",
        ))
        .unwrap();
        assert_eq!(create.output.allocation, VhdxAllocation::Fixed);
        assert_eq!(
            create.output.block_size,
            Some(vmm_cli::MemorySize(4 * 1024 * 1024))
        );
        assert_eq!(
            create.block_alignment,
            Some(vmm_cli::MemorySize(1024 * 1024 * 1024))
        );
        assert_eq!(
            create.page83,
            Some("00112233-4455-6677-8899-aabbccddeeff".parse().unwrap())
        );

        let convert =
            parse_vhdx_convert_format_options(Some("allocation=fixed,block_size=4M")).unwrap();
        assert_eq!(convert.output.allocation, VhdxAllocation::Fixed);
        assert_eq!(
            convert.output.block_size,
            Some(vmm_cli::MemorySize(4 * 1024 * 1024))
        );
        assert!(parse_vhdx_convert_format_options(Some("block_alignment=1G")).is_err());
        assert!(
            parse_vhdx_convert_format_options(Some("page83=00112233-4455-6677-8899-aabbccddeeff"))
                .is_err()
        );
    }

    #[test]
    fn vhdx_format_options_validate_defaults_and_parent_allocation() {
        assert_eq!(
            parse_vhdx_create_format_options(None).unwrap(),
            VhdxCreateFormatOptions::default()
        );
        assert!(parse_vhdx_create_format_options(Some("allocation=other")).is_err());
        assert!(parse_vhdx_create_format_options(Some("block_size=1M,block_size=2M")).is_err());
        assert!(matches!(
            vhdx_disk_type(VhdxAllocation::Dynamic, true).unwrap(),
            DiskType::Differencing
        ));
        assert!(vhdx_disk_type(VhdxAllocation::Fixed, true).is_err());
    }

    async fn collect_map(image: &VhdxFile<BlockingFile>) -> Result<Vec<MapRun>> {
        let mut runs = Vec::new();
        visit_map_runs(image, |run| {
            runs.push(run);
            Ok(())
        })
        .await?;
        Ok(runs)
    }

    fn options(file: PathBuf, size: u64) -> CreateOptions {
        CreateOptions {
            file,
            size,
            disk_type: DiskType::Dynamic,
            parent: None,
            block_size: None,
            logical_sector_size: None,
            physical_sector_size: None,
            block_alignment: None,
            page83: None,
            force: false,
        }
    }

    #[pal_async::async_test]
    async fn creates_differencing_image_with_parent_locator() {
        let directory = tempfile::tempdir().unwrap();
        let parent_path = directory.path().join("parent.vhdx");
        let child_path = directory.path().join("child.vhdx");
        let size = 4 * 1024 * 1024;

        create_image(options(parent_path.clone(), size))
            .await
            .unwrap();
        let parent = VhdxFile::open(BlockingFile::open(&parent_path, true).unwrap())
            .read_only()
            .await
            .unwrap();
        let parent_linkage = parent.data_write_guid();

        create_image(CreateOptions {
            disk_type: DiskType::Differencing,
            parent: Some(parent_path.clone()),
            ..options(child_path.clone(), size)
        })
        .await
        .unwrap();

        let child = VhdxFile::open(BlockingFile::open(&child_path, true).unwrap())
            .read_only()
            .await
            .unwrap();
        assert!(child.has_parent());
        let parent = child
            .parent_locator()
            .await
            .unwrap()
            .unwrap()
            .vhdx_parent()
            .unwrap();
        assert_eq!(parent.linkage(), parent_linkage);
        assert_eq!(parent.relative_path(), Some("parent.vhdx"));
        #[cfg(unix)]
        assert_eq!(parent.absolute_win32_path(), None);
        #[cfg(windows)]
        assert!(
            parent
                .absolute_win32_path()
                .is_some_and(|path| path.starts_with(r"\\?\"))
        );
    }

    #[pal_async::async_test]
    async fn differencing_image_inherits_parent_logical_sector_size() {
        let directory = tempfile::tempdir().unwrap();
        let parent_path = directory.path().join("parent.vhdx");
        let child_path = directory.path().join("child.vhdx");
        let parent_size = 4 * 1024 * 1024;
        let child_size = 2 * 1024 * 1024;

        create_image(CreateOptions {
            logical_sector_size: Some(4096),
            ..options(parent_path.clone(), parent_size)
        })
        .await
        .unwrap();
        create_image(CreateOptions {
            disk_type: DiskType::Differencing,
            parent: Some(parent_path),
            ..options(child_path.clone(), child_size)
        })
        .await
        .unwrap();

        let child = VhdxFile::open(BlockingFile::open(&child_path, true).unwrap())
            .read_only()
            .await
            .unwrap();
        assert_eq!(child.disk_size(), child_size);
        assert_eq!(child.logical_sector_size(), 4096);
    }

    #[pal_async::async_test]
    async fn differencing_image_rejects_logical_sector_size_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let parent_path = directory.path().join("parent.vhdx");
        let child_path = directory.path().join("child.vhdx");
        let size = 4 * 1024 * 1024;

        create_image(CreateOptions {
            logical_sector_size: Some(4096),
            ..options(parent_path.clone(), size)
        })
        .await
        .unwrap();
        let error = create_image(CreateOptions {
            disk_type: DiskType::Differencing,
            parent: Some(parent_path),
            logical_sector_size: Some(512),
            ..options(child_path.clone(), size)
        })
        .await
        .unwrap_err();

        assert!(format!("{error:#}").contains("logical sector size must match parent"));
        assert!(!child_path.exists());
    }

    #[cfg(unix)]
    #[pal_async::async_test]
    async fn rejects_non_unicode_parent_locator_path() {
        use std::os::unix::ffi::OsStringExt;

        let directory = tempfile::tempdir().unwrap();
        let original_parent_path = directory.path().join("parent.vhdx");
        let parent_path = directory
            .path()
            .join(std::ffi::OsString::from_vec(b"parent-\xff.vhdx".to_vec()));
        let child_path = directory.path().join("child.vhdx");
        let size = 4 * 1024 * 1024;

        create_image(options(original_parent_path.clone(), size))
            .await
            .unwrap();
        std::fs::rename(original_parent_path, &parent_path).unwrap();

        let error = create_image(CreateOptions {
            disk_type: DiskType::Differencing,
            parent: Some(parent_path),
            ..options(child_path, size)
        })
        .await
        .unwrap_err();

        assert!(
            format!("{error:#}").contains("relative parent path is not valid Unicode"),
            "unexpected error: {error:#}"
        );
    }

    #[cfg(unix)]
    #[pal_async::async_test]
    async fn rejects_backslashes_in_parent_locator_path() {
        for relative_path in [r"parent\name.vhdx", r"parent\dir/parent.vhdx"] {
            let directory = tempfile::tempdir().unwrap();
            let parent_path = directory.path().join(relative_path);
            let child_path = directory.path().join("child.vhdx");
            let size = 4 * 1024 * 1024;
            std::fs::create_dir_all(parent_path.parent().unwrap()).unwrap();
            create_image(options(parent_path.clone(), size))
                .await
                .unwrap();

            let error = create_image(CreateOptions {
                disk_type: DiskType::Differencing,
                parent: Some(parent_path),
                ..options(child_path.clone(), size)
            })
            .await
            .unwrap_err();

            assert!(
                format!("{error:#}").contains("relative parent path contains a backslash"),
                "unexpected error: {error:#}"
            );
            assert!(!child_path.exists());
        }
    }

    #[pal_async::async_test]
    async fn fixed_image_is_zeroed_and_last_sector_write_does_not_grow(
        driver: pal_async::DefaultDriver,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fixed.vhdx");
        let size = 2 * 1024 * 1024 + 512;
        create_image(CreateOptions {
            disk_type: DiskType::Fixed,
            ..options(path.clone(), size)
        })
        .await
        .unwrap();

        let file = BlockingFile::open(&path, false).unwrap();
        let payload = file.clone();
        let file_size = file.file_size().await.unwrap();
        let image = VhdxFile::open(file).writable(&driver).await.unwrap();
        assert!(image.is_fully_allocated());
        let runs = collect_map(&image).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].guest_offset, 0);
        assert_eq!(runs[0].length, size);
        let payload_offset = runs[0].file_offset.unwrap();
        assert_eq!(file_size, payload_offset + 4 * 1024 * 1024);
        assert!(
            read_vhdx_chunk(&image, &payload, 0, size as u32)
                .await
                .unwrap()
                .iter()
                .all(|byte| *byte == 0)
        );
        let last_sector = vec![0x5a; 512];
        write_vhdx_chunk(&image, &payload, size - 512, &last_sector)
            .await
            .unwrap();
        image.close().await.unwrap();
        assert_eq!(payload.file_size().await.unwrap(), file_size);

        let image = VhdxFile::open(payload.clone()).read_only().await.unwrap();
        assert_eq!(collect_map(&image).await.unwrap(), runs);
        assert_eq!(
            read_vhdx_chunk(&image, &payload, size - 512, 512)
                .await
                .unwrap(),
            last_sector
        );
    }

    #[pal_async::async_test]
    async fn empty_dynamic_image_maps_as_one_unallocated_run() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("empty.vhdx");
        let size = 4 * 1024 * 1024;
        create_image(options(path.clone(), size)).await.unwrap();

        let image = VhdxFile::open(BlockingFile::open(&path, true).unwrap())
            .read_only()
            .await
            .unwrap();
        assert_eq!(
            collect_map(&image).await.unwrap(),
            vec![MapRun {
                guest_offset: 0,
                length: size,
                file_offset: None,
            }]
        );
    }

    #[pal_async::async_test]
    async fn map_distinguishes_allocated_and_unallocated_blocks(driver: pal_async::DefaultDriver) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sparse.vhdx");
        let size = 4 * 1024 * 1024;
        create_image(CreateOptions {
            block_size: Some(1024 * 1024),
            ..options(path.clone(), size)
        })
        .await
        .unwrap();

        let file = BlockingFile::open(&path, false).unwrap();
        let payload = file.clone();
        let image = VhdxFile::open(file).writable(&driver).await.unwrap();
        write_vhdx_chunk(&image, &payload, 0, &[0x5a; 4096])
            .await
            .unwrap();
        image.close().await.unwrap();

        let image = VhdxFile::open(BlockingFile::open(&path, true).unwrap())
            .read_only()
            .await
            .unwrap();
        let runs = collect_map(&image).await.unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].guest_offset, 0);
        assert_eq!(runs[0].length, 1024 * 1024);
        assert!(runs[0].file_offset.is_some());
        assert_eq!(runs[1].guest_offset, 1024 * 1024);
        assert_eq!(runs[1].length, 3 * 1024 * 1024);
        assert_eq!(runs[1].file_offset, None);
    }

    #[pal_async::async_test]
    async fn check_classifies_missing_parent_as_inconsistent() {
        let directory = tempfile::tempdir().unwrap();
        let parent_path = directory.path().join("parent.vhdx");
        let child_path = directory.path().join("child.vhdx");
        let size = 4 * 1024 * 1024;
        create_image(options(parent_path.clone(), size))
            .await
            .unwrap();
        create_image(CreateOptions {
            disk_type: DiskType::Differencing,
            parent: Some(parent_path.clone()),
            ..options(child_path.clone(), size)
        })
        .await
        .unwrap();
        std::fs::remove_file(parent_path).unwrap();

        let error = test_check(&child_path).await.unwrap_err();
        assert!(error.downcast_ref::<InconsistentImage>().is_some());
    }

    #[pal_async::async_test]
    async fn check_classifies_mismatched_logical_sector_size_as_inconsistent() {
        let directory = tempfile::tempdir().unwrap();
        let parent_path = directory.path().join("parent.vhdx");
        let child_path = directory.path().join("child.vhdx");
        let size = 4 * 1024 * 1024;

        create_image(CreateOptions {
            logical_sector_size: Some(4096),
            ..options(parent_path.clone(), size)
        })
        .await
        .unwrap();
        let parent = VhdxFile::open(BlockingFile::open(&parent_path, true).unwrap())
            .read_only()
            .await
            .unwrap();
        let vhdx_parent = VhdxParent::new(parent.data_write_guid())
            .unwrap()
            .with_relative_path("parent.vhdx")
            .unwrap();
        let child_file = BlockingFile::create(&child_path, false).unwrap();
        ::vhdx::create(
            &child_file,
            &mut CreateParams {
                disk_size: size,
                logical_sector_size: 512,
                disk_type: VhdxDiskType::Differencing(vhdx_parent),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let error = test_check(&child_path).await.unwrap_err();
        assert!(error.downcast_ref::<InconsistentImage>().is_some());
        assert!(format!("{error:#}").contains("logical sector size mismatch"));
    }

    #[pal_async::async_test]
    async fn check_classifies_corrupted_image_as_inconsistent() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("corrupt.vhdx");
        create_image(options(path.clone(), 4 * 1024 * 1024))
            .await
            .unwrap();
        BlockingFile::open(&path, false)
            .unwrap()
            .write_from(0, vec![0; 8])
            .await
            .unwrap();

        let error = test_check(&path).await.unwrap_err();
        assert!(error.downcast_ref::<InconsistentImage>().is_some());
    }

    #[pal_async::async_test]
    async fn check_and_replay_accept_clean_image(driver: pal_async::DefaultDriver) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("clean.vhdx");
        create_image(options(path.clone(), 4 * 1024 * 1024))
            .await
            .unwrap();

        test_check(&path).await.unwrap();
        test_replay(&path, true, &driver).await.unwrap();
        test_replay(&path, false, &driver).await.unwrap();
        test_check(&path).await.unwrap();
    }

    #[pal_async::async_test]
    async fn convert_rejects_differencing_input(driver: pal_async::DefaultDriver) {
        let directory = tempfile::tempdir().unwrap();
        let parent_path = directory.path().join("parent.vhdx");
        let child_path = directory.path().join("child.vhdx");
        let output_path = directory.path().join("output");
        let size = 4 * 1024 * 1024;
        create_image(options(parent_path.clone(), size))
            .await
            .unwrap();
        let file = BlockingFile::open(&parent_path, false).unwrap();
        let payload = file.clone();
        let parent = VhdxFile::open(file).writable(&driver).await.unwrap();
        write_vhdx_chunk(&parent, &payload, 0, &[0x5a; 4096])
            .await
            .unwrap();
        parent.close().await.unwrap();
        create_image(CreateOptions {
            disk_type: DiskType::Differencing,
            parent: Some(parent_path),
            ..options(child_path.clone(), size)
        })
        .await
        .unwrap();

        for output_format in [ImageFormat::Raw, ImageFormat::Vhdx] {
            for force in [false, true] {
                let original = b"existing output must remain intact";
                if force {
                    std::fs::write(&output_path, original).unwrap();
                }
                let error = convert(
                    &child_path,
                    &output_path,
                    ImageFormat::Vhdx,
                    output_format,
                    DiskType::Dynamic,
                    None,
                    force,
                    &driver,
                )
                .await
                .unwrap_err();
                assert!(
                    format!("{error:#}")
                        .contains("converting differencing VHDX inputs is not supported"),
                    "unexpected error: {error:#}"
                );
                if force {
                    assert_eq!(std::fs::read(&output_path).unwrap(), original);
                    std::fs::remove_file(&output_path).unwrap();
                } else {
                    assert!(!output_path.exists());
                }
            }
        }
    }

    #[pal_async::async_test]
    async fn raw_vhdx_raw_round_trip(driver: pal_async::DefaultDriver) {
        let directory = tempfile::tempdir().unwrap();
        let raw_path = directory.path().join("input.raw");
        let vhdx_path = directory.path().join("middle.vhdx");
        let output_path = directory.path().join("output.raw");
        let mut data = vec![0; 4 * 1024 * 1024];
        data[4096..8192].fill(0x5a);
        std::fs::write(&raw_path, &data).unwrap();

        convert(
            &raw_path,
            &vhdx_path,
            ImageFormat::Raw,
            ImageFormat::Vhdx,
            DiskType::Dynamic,
            None,
            false,
            &driver,
        )
        .await
        .unwrap();
        convert(
            &vhdx_path,
            &output_path,
            ImageFormat::Vhdx,
            ImageFormat::Raw,
            DiskType::Dynamic,
            None,
            false,
            &driver,
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read(output_path).unwrap(), data);
    }
}
