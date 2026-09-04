// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command-line tools for creating, inspecting, validating, and converting
//! VHDX virtual disk images.
//!
//! `vhdxtool` is a cross-platform frontend for the [`vhdx`] crate. It supports
//! dynamic, fixed, and differencing VHDX images through six commands:
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
//!
//! Run `vhdxtool --help` or `vhdxtool <command> --help` for command syntax and
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
//! paths to the parent. Reads and conversion currently treat unmapped child
//! ranges as zero; `check` resolves and validates parent chains but the tool
//! does not merge parent payload data into child reads.
//!
//! # Exit status
//!
//! Successful commands exit with status 0. Invalid arguments and operational
//! failures exit with status 1. `check` exits with status 2 when an image or
//! its parent chain is inconsistent. A dirty log is reported as a warning and
//! can be repaired with `replay`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod file;
mod util;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use guid::Guid;
use pal_async::DefaultPool;
use std::path::PathBuf;
use vhdx::AsyncFile;
use vhdx::CreateParams;
use vhdx::DiskType as VhdxDiskType;
use vhdx::OpenError;
use vhdx::OpenErrorKind;
use vhdx::ReadRange;
use vhdx::VhdxFile;
use vhdx::VhdxParent;
use vhdx::WriteRange;

use crate::file::BlockingFile;

#[derive(Clone, Copy, ValueEnum)]
enum DiskType {
    /// Allocate blocks as data is written.
    Dynamic,
    /// Allocate all data blocks when the image is created.
    Fixed,
    /// Read unallocated blocks from a parent VHDX.
    Differencing,
}

#[derive(Clone, Copy, ValueEnum)]
enum ImageFormat {
    /// A headerless byte-for-byte disk image.
    Raw,
    /// A VHDX virtual disk image.
    Vhdx,
}

/// Create, inspect, validate, and convert VHDX images.
#[derive(Parser)]
#[command(name = "vhdxtool")]
struct CliArgs {
    /// Show detailed progress and VHDX diagnostics.
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a dynamic, fixed, or differencing VHDX image.
    Create {
        /// Path of the VHDX image to create.
        file: PathBuf,
        /// Virtual disk size. Accepts binary suffixes such as K, M, G, and T.
        #[arg(long, value_parser = util::parse_size)]
        size: u64,
        /// VHDX allocation type.
        #[arg(long = "type", value_enum, default_value = "dynamic")]
        disk_type: DiskType,
        /// Parent VHDX path. Required for, and only valid with, differencing images.
        #[arg(long)]
        parent: Option<PathBuf>,
        /// VHDX payload block size. Must be a multiple of 1 MiB between 1 MiB
        /// and 256 MiB. Defaults to 2 MiB. Accepts binary size suffixes.
        #[arg(long, value_parser = util::parse_size)]
        block_size: Option<u64>,
        /// Logical sector size in bytes: 512 or 4096.
        #[arg(long)]
        logical_sector_size: Option<u32>,
        /// Physical sector size in bytes: 512 or 4096.
        #[arg(long)]
        physical_sector_size: Option<u32>,
        /// Alignment of the VHDX data region. Accepts binary size suffixes.
        #[arg(long, value_parser = util::parse_size)]
        block_alignment: Option<u64>,
        /// Data write GUID. A random GUID is generated when omitted.
        #[arg(long)]
        id: Option<String>,
        /// SCSI page 83 identifier GUID. A random GUID is generated when omitted.
        #[arg(long)]
        page83: Option<String>,
        /// Replace the output file if it already exists.
        #[arg(long)]
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
        #[arg(long)]
        output: PathBuf,
        /// Source format. Inferred from a .vhdx extension when omitted.
        #[arg(long, value_enum)]
        input_format: Option<ImageFormat>,
        /// Format of the converted image.
        #[arg(long, value_enum)]
        output_format: ImageFormat,
        /// Allocation type for VHDX output.
        #[arg(long = "type", value_enum, default_value = "dynamic")]
        disk_type: DiskType,
        /// Payload block size for VHDX output. Must be a multiple of 1 MiB
        /// between 1 MiB and 256 MiB. Defaults to 2 MiB. Accepts binary size
        /// suffixes.
        #[arg(long, value_parser = util::parse_size)]
        block_size: Option<u64>,
        /// Replace the output file if it already exists.
        #[arg(long)]
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
}

fn main() {
    let args = CliArgs::parse();
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
            size,
            disk_type,
            parent,
            block_size,
            logical_sector_size,
            physical_sector_size,
            block_alignment,
            id,
            page83,
            force,
        } => {
            create_image(CreateOptions {
                file,
                size,
                disk_type,
                parent,
                block_size,
                logical_sector_size,
                physical_sector_size,
                block_alignment,
                id,
                page83,
                force,
            })
            .await
        }
        Command::Info { file, json } => info(&file, json).await,
        Command::Map { file, json } => map(&file, json).await,
        Command::Convert {
            input,
            output,
            input_format,
            output_format,
            disk_type,
            block_size,
            force,
        } => {
            convert(
                &input,
                &output,
                input_format.unwrap_or_else(|| infer_format(&input)),
                output_format,
                disk_type,
                block_size,
                force,
                driver,
            )
            .await
        }
        Command::Check { file } => check(&file).await,
        Command::Replay { file, dry_run } => replay(&file, dry_run, driver).await,
    }
}

#[derive(Debug)]
struct InconsistentImage(String);

impl std::fmt::Display for InconsistentImage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for InconsistentImage {}

fn inconsistent(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(InconsistentImage(message.into()))
}

struct CreateOptions {
    file: PathBuf,
    size: u64,
    disk_type: DiskType,
    parent: Option<PathBuf>,
    block_size: Option<u64>,
    logical_sector_size: Option<u32>,
    physical_sector_size: Option<u32>,
    block_alignment: Option<u64>,
    id: Option<String>,
    page83: Option<String>,
    force: bool,
}

async fn create_image(options: CreateOptions) -> Result<()> {
    let is_differencing = matches!(options.disk_type, DiskType::Differencing);
    anyhow::ensure!(
        is_differencing == options.parent.is_some(),
        "--parent is required for differencing disks and invalid for other disk types"
    );

    let block_size = options.block_size.unwrap_or(0);
    let block_alignment = options.block_alignment.unwrap_or(0);
    let mut params = CreateParams {
        disk_size: options.size,
        block_size: u32::try_from(block_size).context("block size exceeds 4 GiB")?,
        logical_sector_size: options.logical_sector_size.unwrap_or(0),
        physical_sector_size: options.physical_sector_size.unwrap_or(0),
        disk_type: match options.disk_type {
            DiskType::Dynamic => VhdxDiskType::Dynamic,
            DiskType::Fixed => VhdxDiskType::Fixed,
            DiskType::Differencing => VhdxDiskType::Dynamic,
        },
        block_alignment: u32::try_from(block_alignment).context("block alignment exceeds 4 GiB")?,
        data_write_guid: parse_guid(options.id.as_deref(), "id")?,
        page_83_data: parse_guid(options.page83.as_deref(), "page83")?,
        ..Default::default()
    };

    if let Some(parent_path) = options.parent {
        let parent_file = BlockingFile::open(&parent_path, true)
            .with_context(|| format!("failed to open parent {}", parent_path.display()))?;
        let parent = VhdxFile::open(parent_file)
            .read_only()
            .await
            .context("failed to read parent VHDX")?;
        anyhow::ensure!(
            parent.disk_size() == options.size,
            "child size must match parent size ({})",
            parent.disk_size()
        );
        let child_directory = options
            .file
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        let absolute_parent = fs_err::canonicalize(&parent_path)
            .with_context(|| format!("failed to resolve parent {}", parent_path.display()))?;
        let relative_path = util::relative_path(child_directory, &absolute_parent).map(|path| {
            path.to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "\\")
        });
        // On Windows the parent may be on another drive, leaving no relative
        // path; the absolute path below is then the only locator. Elsewhere
        // there is no absolute locator, so a relative path is required.
        #[cfg(windows)]
        let relative_path = relative_path.ok();
        #[cfg(not(windows))]
        let relative_path = Some(relative_path?);
        let mut vhdx_parent = VhdxParent::new(parent.data_write_guid())?;
        if let Some(relative_path) = relative_path {
            vhdx_parent = vhdx_parent.with_relative_path(relative_path)?;
        }
        #[cfg(windows)]
        let vhdx_parent = {
            let absolute_path = absolute_parent.to_string_lossy();
            let absolute_path = if absolute_path.starts_with(r"\\?\") {
                absolute_path.into_owned()
            } else {
                format!(r"\\?\{}", absolute_path)
            };
            vhdx_parent.with_absolute_win32_path(absolute_path)?
        };
        params.disk_type = VhdxDiskType::Differencing(vhdx_parent);
    }

    let file = BlockingFile::create(&options.file, options.force)
        .with_context(|| format!("failed to create {}", options.file.display()))?;
    vhdx::create(&file, &mut params)
        .await
        .context("failed to create VHDX")?;
    println!(
        "Created {} ({})",
        options.file.display(),
        util::format_size(options.size)
    );
    Ok(())
}

fn parse_guid(value: Option<&str>, name: &str) -> Result<Guid> {
    value
        .map(|value| {
            value
                .parse()
                .with_context(|| format!("invalid --{name} GUID"))
        })
        .transpose()
        .map(|value| value.unwrap_or(Guid::ZERO))
}

async fn info(path: &std::path::Path, json: bool) -> Result<()> {
    let file = BlockingFile::open(path, true)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let image = VhdxFile::open(file)
        .read_only()
        .await
        .context("failed to open VHDX")?;
    let parent = image
        .parent_locator()
        .await
        .context("failed to read parent locator")?
        .map(|locator| locator.vhdx_parent())
        .transpose()
        .context("failed to interpret VHDX parent locator")?;
    let image_type = if image.has_parent() {
        "differencing"
    } else if image.is_fully_allocated() {
        "fixed"
    } else {
        "dynamic"
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "disk_size": image.disk_size(),
                "block_size": image.block_size(),
                "logical_sector_size": image.logical_sector_size(),
                "physical_sector_size": image.physical_sector_size(),
                "type": image_type,
                "has_parent": image.has_parent(),
                "parent_linkage": parent.as_ref().map(|parent| parent.linkage().to_string()),
                "relative_path": parent.as_ref().and_then(|parent| parent.relative_path()),
                "absolute_win32_path": parent.as_ref().and_then(|parent| parent.absolute_win32_path()),
                "volume_path": parent.as_ref().and_then(|parent| parent.volume_path()),
                "page_83_data": image.page_83_data().to_string(),
                "data_write_guid": image.data_write_guid().to_string(),
                "is_read_only": image.is_read_only(),
            }))?
        );
    } else {
        println!("File:                 {}", path.display());
        println!("Type:                 {image_type}");
        println!(
            "Disk size:            {} ({})",
            image.disk_size(),
            util::format_size(image.disk_size())
        );
        println!("Block size:           {}", image.block_size());
        println!("Logical sector size:  {}", image.logical_sector_size());
        println!("Physical sector size: {}", image.physical_sector_size());
        println!("Page 83 ID:           {}", image.page_83_data());
        println!("Data write GUID:      {}", image.data_write_guid());
        if let Some(parent) = parent {
            println!("Parent linkage:       {}", parent.linkage());
            println!(
                "Relative parent:      {}",
                parent.relative_path().unwrap_or("-")
            );
            println!(
                "Absolute parent:      {}",
                parent.absolute_win32_path().unwrap_or("-")
            );
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct MapRun {
    guest_offset: u64,
    length: u64,
    file_offset: Option<u64>,
}

async fn collect_map(image: &VhdxFile<BlockingFile>) -> Result<Vec<MapRun>> {
    let mut runs = Vec::new();
    let mut offset = 0;
    while offset < image.disk_size() {
        let length = (image.disk_size() - offset).min(image.block_size() as u64) as u32;
        let mut ranges = Vec::new();
        let guard = image
            .resolve_read(offset, length, &mut ranges)
            .await
            .context("failed to resolve VHDX allocation map")?;
        for range in ranges {
            let (guest_offset, length, file_offset) = match range {
                ReadRange::Data {
                    guest_offset,
                    length,
                    file_offset,
                } => (guest_offset, length, Some(file_offset)),
                ReadRange::Zero {
                    guest_offset,
                    length,
                }
                | ReadRange::Unmapped {
                    guest_offset,
                    length,
                } => (guest_offset, length, None),
            };
            append_map_run(&mut runs, guest_offset, length as u64, file_offset);
        }
        drop(guard);
        offset += length as u64;
    }
    Ok(runs)
}

fn append_map_run(
    runs: &mut Vec<MapRun>,
    guest_offset: u64,
    length: u64,
    file_offset: Option<u64>,
) {
    if let Some(previous) = runs.last_mut()
        && previous.guest_offset + previous.length == guest_offset
        && match (previous.file_offset, file_offset) {
            (None, None) => true,
            (Some(previous_file), Some(file)) => previous_file + previous.length == file,
            _ => false,
        }
    {
        previous.length += length;
        return;
    }
    runs.push(MapRun {
        guest_offset,
        length,
        file_offset,
    });
}

async fn map(path: &std::path::Path, json: bool) -> Result<()> {
    let file = BlockingFile::open(path, true)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let image = VhdxFile::open(file)
        .read_only()
        .await
        .context("failed to open VHDX")?;
    let runs = collect_map(&image).await?;

    if json {
        let values: Vec<_> = runs
            .iter()
            .map(|run| {
                serde_json::json!({
                    "start": run.guest_offset,
                    "length": run.length,
                    "allocated": run.file_offset.is_some(),
                    "file_offset": run.file_offset,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&values)?);
    } else {
        println!(
            "{:<14} {:<14} {:<12} FILE OFFSET",
            "START", "LENGTH", "ALLOCATED"
        );
        for run in runs {
            println!(
                "{:<14} {:<14} {:<12} {}",
                run.guest_offset,
                run.length,
                if run.file_offset.is_some() {
                    "yes"
                } else {
                    "no"
                },
                run.file_offset
                    .map(|offset| offset.to_string())
                    .unwrap_or_else(|| "-".to_string())
            );
        }
    }
    Ok(())
}

async fn check(path: &std::path::Path) -> Result<()> {
    let mut current_path = fs_err::canonicalize(path)
        .with_context(|| format!("failed to resolve {}", path.display()))?;
    let mut visited = std::collections::HashSet::new();

    loop {
        if !visited.insert(current_path.clone()) {
            return Err(inconsistent(format!(
                "parent chain contains a cycle at {}",
                current_path.display()
            )));
        }
        let file = BlockingFile::open(&current_path, true)
            .with_context(|| format!("failed to open {}", current_path.display()))?;
        let image = match VhdxFile::open(file).read_only().await {
            Ok(image) => image,
            Err(error) if error.kind() == OpenErrorKind::LogReplayRequired => {
                println!(
                    "WARNING: {} requires log replay; run `vhdxtool replay {}`",
                    current_path.display(),
                    current_path.display()
                );
                return Ok(());
            }
            Err(error) => return Err(classify_open_error(&current_path, error)),
        };
        if !image.has_parent() {
            break;
        }

        let parent_info = image
            .parent_locator()
            .await
            .map_err(|error| classify_open_error(&current_path, error))?
            .ok_or_else(|| inconsistent("differencing image has no parent locator"))?
            .vhdx_parent()
            .map_err(|error| inconsistent(format!("invalid VHDX parent locator: {error}")))?;
        let linkage = parent_info.linkage();
        let parent_path = resolve_parent_path(&current_path, &parent_info).ok_or_else(|| {
            inconsistent(format!(
                "parent of {} was not found",
                current_path.display()
            ))
        })?;
        let parent_file = BlockingFile::open(&parent_path, true)
            .with_context(|| format!("failed to open parent {}", parent_path.display()))?;
        let parent = VhdxFile::open(parent_file)
            .read_only()
            .await
            .map_err(|error| classify_open_error(&parent_path, error))?;
        if parent.data_write_guid() != linkage {
            return Err(inconsistent(format!(
                "parent linkage mismatch for {}: expected {}, found {}",
                current_path.display(),
                linkage,
                parent.data_write_guid()
            )));
        }
        current_path = fs_err::canonicalize(&parent_path)
            .with_context(|| format!("failed to resolve {}", parent_path.display()))?;
    }

    println!("OK: {}", path.display());
    Ok(())
}

fn resolve_parent_path(child_path: &std::path::Path, parent: &VhdxParent) -> Option<PathBuf> {
    let child_directory = child_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let mut candidates = Vec::new();
    if let Some(relative) = parent.relative_path() {
        candidates.push(child_directory.join(native_locator_path(relative)));
    }
    if let Some(volume) = parent.volume_path() {
        candidates.push(native_locator_path(volume));
    }
    if let Some(absolute) = parent.absolute_win32_path() {
        candidates.push(native_locator_path(absolute));
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

#[cfg(unix)]
fn native_locator_path(path: &str) -> PathBuf {
    PathBuf::from(path.replace('\\', "/"))
}

#[cfg(windows)]
fn native_locator_path(path: &str) -> PathBuf {
    PathBuf::from(path)
}

fn classify_open_error(path: &std::path::Path, error: OpenError) -> anyhow::Error {
    match error.kind() {
        OpenErrorKind::Corruption | OpenErrorKind::LogReplayRequired => {
            inconsistent(format!("{} is inconsistent: {error:#}", path.display()))
        }
        _ => anyhow::Error::new(error).context(format!("failed to open {}", path.display())),
    }
}

async fn replay(
    path: &std::path::Path,
    dry_run: bool,
    driver: &impl pal_async::task::Spawn,
) -> Result<()> {
    if dry_run {
        let file = BlockingFile::open(path, true)
            .with_context(|| format!("failed to open {}", path.display()))?;
        match VhdxFile::open(file).read_only().await {
            Ok(_) => println!("No replay required: {}", path.display()),
            Err(error) if error.kind() == OpenErrorKind::LogReplayRequired => {
                println!("Replay required: {}", path.display())
            }
            Err(error) => return Err(classify_open_error(path, error)),
        }
        return Ok(());
    }

    let file = BlockingFile::open(path, false)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let image = VhdxFile::open(file)
        .writable(driver)
        .await
        .map_err(|error| classify_open_error(path, error))?;
    image.close().await.context("failed to close VHDX")?;
    println!("Replay complete: {}", path.display());
    Ok(())
}

fn infer_format(path: &std::path::Path) -> ImageFormat {
    if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("vhdx"))
    {
        ImageFormat::Vhdx
    } else {
        ImageFormat::Raw
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
                id: None,
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
                        id: None,
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

async fn read_vhdx_chunk(
    image: &VhdxFile<BlockingFile>,
    payload: &BlockingFile,
    offset: u64,
    length: u32,
) -> Result<Vec<u8>> {
    let mut data = vec![0; length as usize];
    let mut ranges = Vec::new();
    let guard = image
        .resolve_read(offset, length, &mut ranges)
        .await
        .context("failed to resolve VHDX read")?;
    for range in ranges {
        if let ReadRange::Data {
            guest_offset,
            length,
            file_offset,
        } = range
        {
            let range_data = payload
                .read_into(file_offset, vec![0; length as usize])
                .await
                .context("failed to read VHDX payload")?;
            let start = (guest_offset - offset) as usize;
            data[start..start + length as usize].copy_from_slice(&range_data);
        }
    }
    drop(guard);
    Ok(data)
}

async fn write_vhdx_chunk(
    image: &VhdxFile<BlockingFile>,
    payload: &BlockingFile,
    offset: u64,
    data: &[u8],
) -> Result<()> {
    let length = u32::try_from(data.len()).context("copy chunk is too large")?;
    let mut ranges = Vec::new();
    let guard = image
        .resolve_write(offset, length, &mut ranges)
        .await
        .context("failed to resolve VHDX write")?;
    for range in ranges {
        match range {
            WriteRange::Data {
                guest_offset,
                length,
                file_offset,
            } => {
                let start = (guest_offset - offset) as usize;
                payload
                    .write_from(file_offset, data[start..start + length as usize].to_vec())
                    .await
                    .context("failed to write VHDX payload")?;
            }
            WriteRange::Zero {
                file_offset,
                length,
            } => payload
                .zero_range(file_offset, length as u64)
                .await
                .context("failed to zero VHDX payload")?,
        }
    }
    guard
        .complete()
        .await
        .context("failed to commit VHDX allocation")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            id: None,
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

        let error = check(&child_path).await.unwrap_err();
        assert!(error.downcast_ref::<InconsistentImage>().is_some());
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

        let error = check(&path).await.unwrap_err();
        assert!(error.downcast_ref::<InconsistentImage>().is_some());
    }

    #[pal_async::async_test]
    async fn check_and_replay_accept_clean_image(driver: pal_async::DefaultDriver) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("clean.vhdx");
        create_image(options(path.clone(), 4 * 1024 * 1024))
            .await
            .unwrap();

        check(&path).await.unwrap();
        replay(&path, true, &driver).await.unwrap();
        replay(&path, false, &driver).await.unwrap();
        check(&path).await.unwrap();
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
