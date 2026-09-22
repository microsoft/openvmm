// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VHDX-specific image options and operations.

use crate::file::BlockingFile;
use crate::util;
use ::vhdx::AsyncFile;
use ::vhdx::CreateParams;
use ::vhdx::DiskType as VhdxDiskType;
use ::vhdx::OpenError;
use ::vhdx::OpenErrorKind;
use ::vhdx::ReadRange;
use ::vhdx::VhdxFile;
use ::vhdx::VhdxParent;
use ::vhdx::WriteRange;
use anyhow::Context;
use anyhow::Result;
use guid::Guid;
use serde::Serializer as _;
use serde::ser::SerializeSeq;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use vmm_cli::KeyValueFields;

#[derive(Clone, Copy)]
pub(crate) enum DiskType {
    /// Allocate blocks as data is written.
    Dynamic,
    /// Allocate all data blocks when the image is created.
    Fixed,
    /// Read unallocated blocks from a parent VHDX.
    Differencing,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum VhdxAllocation {
    #[default]
    Dynamic,
    Fixed,
}

impl std::str::FromStr for VhdxAllocation {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "dynamic" => Ok(Self::Dynamic),
            "fixed" => Ok(Self::Fixed),
            _ => anyhow::bail!("expected 'dynamic' or 'fixed'"),
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq, vmm_cli::KeyValueArgs)]
pub(crate) struct VhdxOutputFormatOptions {
    /// Allocation mode: `dynamic` or `fixed`. Defaults to `dynamic`.
    #[kv(default)]
    pub(crate) allocation: VhdxAllocation,
    /// Payload block size. Defaults to 2 MiB. Accepts binary size suffixes.
    pub(crate) block_size: Option<vmm_cli::MemorySize>,
}

#[derive(Debug, Default, PartialEq, Eq, vmm_cli::KeyValueArgs)]
pub(crate) struct VhdxCreateFormatOptions {
    #[kv(flatten)]
    pub(crate) output: VhdxOutputFormatOptions,
    /// Alignment of the data region. Accepts binary size suffixes.
    pub(crate) block_alignment: Option<vmm_cli::MemorySize>,
    /// SCSI page 83 identifier GUID. A random GUID is generated when omitted.
    pub(crate) page83: Option<Guid>,
}

#[derive(Debug, Default, PartialEq, Eq, vmm_cli::KeyValueArgs)]
pub(crate) struct VhdxConvertFormatOptions {
    #[kv(flatten)]
    pub(crate) output: VhdxOutputFormatOptions,
}

pub(crate) fn parse_create_format_options(value: Option<&str>) -> Result<VhdxCreateFormatOptions> {
    value
        .unwrap_or_default()
        .parse()
        .context("invalid VHDX create format options")
}

pub(crate) fn parse_convert_format_options(
    value: Option<&str>,
) -> Result<VhdxConvertFormatOptions> {
    value
        .unwrap_or_default()
        .parse()
        .context("invalid VHDX convert format options")
}

pub(crate) fn disk_type(allocation: VhdxAllocation, has_parent: bool) -> Result<DiskType> {
    anyhow::ensure!(
        !has_parent || allocation == VhdxAllocation::Dynamic,
        "a parent image cannot be combined with allocation=fixed"
    );
    Ok(if has_parent {
        DiskType::Differencing
    } else {
        match allocation {
            VhdxAllocation::Dynamic => DiskType::Dynamic,
            VhdxAllocation::Fixed => DiskType::Fixed,
        }
    })
}

pub(crate) fn print_format_options() -> Result<()> {
    println!("VHDX create format options:");
    for option in VhdxCreateFormatOptions::options() {
        println!("  {}=<VALUE>\n      {}", option.key, option.help);
    }
    println!("\nVHDX convert output format options:");
    for option in VhdxConvertFormatOptions::options() {
        println!("  {}=<VALUE>\n      {}", option.key, option.help);
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct InconsistentImage(String);

impl std::fmt::Display for InconsistentImage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for InconsistentImage {}

fn inconsistent(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(InconsistentImage(message.into()))
}

pub(crate) struct CreateOptions {
    pub(crate) file: PathBuf,
    pub(crate) size: u64,
    pub(crate) disk_type: DiskType,
    pub(crate) parent: Option<PathBuf>,
    pub(crate) block_size: Option<u64>,
    pub(crate) logical_sector_size: Option<u32>,
    pub(crate) physical_sector_size: Option<u32>,
    pub(crate) block_alignment: Option<u64>,
    pub(crate) page83: Option<Guid>,
    pub(crate) force: bool,
}

pub(crate) async fn create_image(options: CreateOptions) -> Result<()> {
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
        page_83_data: options.page83.unwrap_or(Guid::ZERO),
        ..Default::default()
    };

    if let Some(parent_path) = options.parent {
        let parent_file = BlockingFile::open(&parent_path, true)
            .with_context(|| format!("failed to open parent {}", parent_path.display()))?;
        let parent = VhdxFile::open(parent_file)
            .read_only()
            .await
            .context("failed to read parent VHDX")?;
        let parent_logical_sector_size = parent.logical_sector_size();
        if let Some(logical_sector_size) = options.logical_sector_size {
            anyhow::ensure!(
                logical_sector_size == parent_logical_sector_size,
                "child logical sector size must match parent logical sector size ({parent_logical_sector_size})"
            );
        }
        params.logical_sector_size = parent_logical_sector_size;
        let child_directory = options
            .file
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let absolute_parent = fs_err::canonicalize(&parent_path)
            .with_context(|| format!("failed to resolve parent {}", parent_path.display()))?;
        let relative_path =
            util::relative_path(child_directory, &absolute_parent).and_then(|path| {
                let path = path
                    .to_str()
                    .context("relative parent path is not valid Unicode")?;
                #[cfg(unix)]
                anyhow::ensure!(
                    !path.contains('\\'),
                    "relative parent path contains a backslash"
                );
                Ok(path.replace(std::path::MAIN_SEPARATOR, "\\"))
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
            let absolute_path = absolute_parent
                .to_str()
                .context("absolute parent path is not valid Unicode")?;
            let absolute_path = if absolute_path.starts_with(r"\\?\") {
                absolute_path.to_owned()
            } else {
                format!(r"\\?\{}", absolute_path)
            };
            vhdx_parent.with_absolute_win32_path(absolute_path)?
        };
        params.disk_type = VhdxDiskType::Differencing(vhdx_parent);
    }

    let file = BlockingFile::create(&options.file, options.force)
        .with_context(|| format!("failed to create {}", options.file.display()))?;
    ::vhdx::create(&file, &mut params)
        .await
        .context("failed to create VHDX")?;
    println!(
        "Created {} ({})",
        options.file.display(),
        util::format_size(options.size)
    );
    Ok(())
}

pub(crate) async fn info(path: &Path, json: bool) -> Result<()> {
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
                "format": "vhdx",
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
        println!("Format:               VHDX");
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
pub(crate) struct MapRun {
    pub(crate) guest_offset: u64,
    pub(crate) length: u64,
    pub(crate) file_offset: Option<u64>,
}

pub(crate) async fn visit_map_runs(
    image: &VhdxFile<BlockingFile>,
    mut visit: impl FnMut(MapRun) -> Result<()>,
) -> Result<()> {
    let mut pending: Option<MapRun> = None;
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
            let run = MapRun {
                guest_offset,
                length: length as u64,
                file_offset,
            };
            if let Some(previous) = pending.as_mut()
                && previous.guest_offset + previous.length == run.guest_offset
                && match (previous.file_offset, run.file_offset) {
                    (None, None) => true,
                    (Some(previous_file), Some(file)) => previous_file + previous.length == file,
                    _ => false,
                }
            {
                previous.length += run.length;
            } else if let Some(previous) = pending.replace(run) {
                visit(previous)?;
            }
        }
        drop(guard);
        offset += length as u64;
    }
    if let Some(run) = pending {
        visit(run)?;
    }
    Ok(())
}

pub(crate) async fn map(path: &Path, json: bool) -> Result<()> {
    let file = BlockingFile::open(path, true)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let image = VhdxFile::open(file)
        .read_only()
        .await
        .context("failed to open VHDX")?;
    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());

    if json {
        {
            let mut serializer = serde_json::Serializer::pretty(&mut output);
            let mut sequence = serializer.serialize_seq(None)?;
            visit_map_runs(&image, |run| {
                sequence.serialize_element(&serde_json::json!({
                    "start": run.guest_offset,
                    "length": run.length,
                    "allocated": run.file_offset.is_some(),
                    "file_offset": run.file_offset,
                }))?;
                Ok(())
            })
            .await?;
            sequence.end()?;
        }
        writeln!(output)?;
    } else {
        writeln!(
            output,
            "{:<14} {:<14} {:<12} FILE OFFSET",
            "START", "LENGTH", "ALLOCATED"
        )?;
        visit_map_runs(&image, |run| {
            writeln!(
                output,
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
            )?;
            Ok(())
        })
        .await?;
    }
    Ok(())
}

pub(crate) async fn check(path: &Path) -> Result<()> {
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
                    "WARNING: {} requires log replay; run `openvmm-img replay {}`",
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
        if parent.logical_sector_size() != image.logical_sector_size() {
            return Err(inconsistent(format!(
                "parent logical sector size mismatch for {}: child uses {}, parent uses {}",
                current_path.display(),
                image.logical_sector_size(),
                parent.logical_sector_size()
            )));
        }
        current_path = fs_err::canonicalize(&parent_path)
            .with_context(|| format!("failed to resolve {}", parent_path.display()))?;
    }

    println!("OK: {}", path.display());
    Ok(())
}

pub(crate) fn resolve_parent_path(child_path: &Path, parent: &VhdxParent) -> Option<PathBuf> {
    let child_directory = child_path.parent().unwrap_or_else(|| Path::new("."));
    parent
        .candidate_paths(child_directory)
        .find(|candidate| candidate.is_file())
}

fn classify_open_error(path: &Path, error: OpenError) -> anyhow::Error {
    match error.kind() {
        OpenErrorKind::Corruption | OpenErrorKind::LogReplayRequired => {
            inconsistent(format!("{} is inconsistent: {error:#}", path.display()))
        }
        _ => anyhow::Error::new(error).context(format!("failed to open {}", path.display())),
    }
}

pub(crate) async fn replay(
    path: &Path,
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

pub(crate) async fn read_chunk(
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

pub(crate) async fn write_chunk(
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
