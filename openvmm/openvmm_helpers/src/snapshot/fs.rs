// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! File-system helpers for snapshot artifacts, including their
//! platform-specific implementations: private files and directories,
//! no-replace renames and directory flushes, and sparse-aware clone and copy
//! with allocation accounting.

use anyhow::Context;
#[cfg(windows)]
use std::io::Read;
#[cfg(windows)]
use std::io::Seek;
#[cfg(windows)]
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;

const COPY_BUFFER_SIZE: usize = 1024 * 1024;

pub(super) fn snapshot_parent(dir: &Path) -> &Path {
    match dir.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

pub(super) fn path_exists(path: &Path) -> anyhow::Result<bool> {
    match fs_err::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

pub(super) fn ensure_path_absent(path: &Path, description: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !path_exists(path)?,
        "{description} already exists: {}",
        path.display(),
    );
    Ok(())
}

pub(super) fn validate_directory(path: &Path, description: &str) -> anyhow::Result<()> {
    let metadata = fs_err::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {description} {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_dir(),
        "{description} is not a directory: {}",
        path.display(),
    );
    Ok(())
}

pub(super) fn create_private_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let descriptor: pal::windows::security::LocalSecurityDescriptor =
            "D:P(A;;FA;;;SY)(A;;FA;;;OW)".parse()?;
        pal::windows::security::create::create_directory_with_security(path, &descriptor)
    }
    #[cfg(not(windows))]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    #[cfg(not(windows))]
    return builder.create(path);
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(super) fn rename_no_replace(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let source_parent = snapshot_parent(source);
    let destination_parent = snapshot_parent(destination);
    anyhow::ensure!(
        source_parent == destination_parent,
        "snapshot staging and destination directories have different parents",
    );
    let source_name = source
        .file_name()
        .context("snapshot staging path must name a directory")?;
    let destination_name = destination
        .file_name()
        .context("snapshot destination must name a directory")?;
    let parent = std::fs::File::open(destination_parent).with_context(|| {
        format!(
            "failed to open snapshot parent directory {}",
            destination_parent.display()
        )
    })?;
    nix::fcntl::renameat2(
        &parent,
        source_name,
        &parent,
        destination_name,
        nix::fcntl::RenameFlags::RENAME_NOREPLACE,
    )?;
    Ok(())
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub(super) fn rename_no_replace(source: &Path, destination: &Path) -> anyhow::Result<()> {
    fs_err::rename(source, destination)?;
    Ok(())
}

fn create_file(path: &Path, description: &str) -> anyhow::Result<std::fs::File> {
    #[cfg(windows)]
    {
        let descriptor: pal::windows::security::LocalSecurityDescriptor =
            "D:P(A;;FA;;;SY)(A;;FA;;;OW)".parse()?;
        pal::windows::security::create::create_file_with_security(path, &descriptor)
            .with_context(|| format!("failed to create {description} at {}", path.display()))
    }
    #[cfg(not(windows))]
    let mut options = fs_err::OpenOptions::new();
    #[cfg(not(windows))]
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use fs_err::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(windows))]
    return options
        .open(path)
        .map(Into::into)
        .with_context(|| format!("failed to create {description} at {}", path.display()));
}

pub(super) fn write_bytes(path: &Path, bytes: &[u8], description: &str) -> anyhow::Result<()> {
    let mut file = create_file(path, description)?;
    file.write_all(bytes)
        .with_context(|| format!("failed to write {description}"))?;
    file.sync_all()
        .with_context(|| format!("failed to flush {description}"))?;
    Ok(())
}

pub(super) fn copy_exact(
    source_file: &std::fs::File,
    destination_path: &Path,
    expected_length: u64,
    source_description: &str,
    destination_description: &str,
) -> anyhow::Result<()> {
    let source_metadata = source_file
        .metadata()
        .with_context(|| format!("failed to inspect {source_description}"))?;
    anyhow::ensure!(
        source_metadata.file_type().is_file(),
        "{source_description} handle is not a regular file"
    );
    let source_length = source_metadata.len();
    anyhow::ensure!(
        source_length == expected_length,
        "{source_description} size ({source_length} bytes) doesn't match manifest ({expected_length} bytes)",
    );
    let destination = create_file(destination_path, destination_description)?;
    let method = clone_or_copy(source_file, &destination, expected_length)
        .with_context(|| format!("failed to copy {source_description}"))?;

    anyhow::ensure!(
        source_file
            .metadata()
            .with_context(|| format!("failed to re-inspect {source_description}"))?
            .len()
            == expected_length,
        "{source_description} changed length while it was being copied",
    );
    anyhow::ensure!(
        destination
            .metadata()
            .with_context(|| format!("failed to inspect {destination_description}"))?
            .len()
            == expected_length,
        "{destination_description} length does not match {source_description}",
    );

    destination
        .sync_all()
        .with_context(|| format!("failed to flush {destination_description}"))?;
    let source_allocated_bytes = allocated_file_bytes(source_file, expected_length).ok();
    let allocated_bytes = allocated_file_bytes(&destination, expected_length).ok();
    tracing::info!(
        method,
        logical_bytes = expected_length,
        ?source_allocated_bytes,
        ?allocated_bytes,
        "published independent snapshot artifact"
    );
    Ok(())
}

#[cfg(not(windows))]
fn size_empty_file(file: &std::fs::File, length: u64, description: &str) -> anyhow::Result<()> {
    file.set_len(0)
        .with_context(|| format!("failed to reset {description}"))?;
    file.set_len(length)
        .with_context(|| format!("failed to size {description}"))?;
    anyhow::ensure!(
        file.metadata()
            .with_context(|| format!("failed to inspect {description}"))?
            .len()
            == length,
        "{description} has the wrong logical length after sizing"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn clone_or_copy(
    source: &std::fs::File,
    destination: &std::fs::File,
    length: u64,
) -> anyhow::Result<&'static str> {
    let clone_error = match pal::unix::fs::reflink(source, destination) {
        Ok(()) => return Ok("ficlone"),
        Err(error) => error,
    };
    tracing::debug!(
        error = &clone_error as &dyn std::error::Error,
        "FICLONE unavailable"
    );
    size_empty_file(destination, length, "snapshot clone destination")?;
    match linux_allocated_ranges(source, length) {
        Ok(ranges) => {
            copy_allocated_ranges(source, destination, length, &ranges)?;
            Ok("seek-data-hole")
        }
        Err(error) => {
            tracing::debug!(
                error = &error as &dyn std::error::Error,
                "SEEK_DATA/SEEK_HOLE unavailable"
            );
            copy_nonzero_data(source, destination, 0, length)?;
            Ok("zero-scan")
        }
    }
}

#[cfg(windows)]
fn clone_or_copy(
    source: &std::fs::File,
    destination: &std::fs::File,
    length: u64,
) -> anyhow::Result<&'static str> {
    let mut source = source
        .try_clone()
        .context("failed to duplicate snapshot memory source handle")?;
    source
        .seek(SeekFrom::Start(0))
        .context("failed to rewind snapshot memory source")?;
    let mut destination = destination
        .try_clone()
        .context("failed to duplicate snapshot memory destination handle")?;
    destination
        .seek(SeekFrom::Start(0))
        .context("failed to rewind snapshot memory destination")?;

    let limit = length
        .checked_add(1)
        .context("snapshot memory length cannot be bounded")?;
    let mut source = Read::by_ref(&mut source).take(limit);
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    loop {
        let count = source
            .read(&mut buffer)
            .context("failed to read snapshot memory source")?;
        if count == 0 {
            break;
        }
        destination
            .write_all(&buffer[..count])
            .context("failed to write dense snapshot memory destination")?;
        total = total
            .checked_add(count as u64)
            .context("snapshot memory length overflowed u64")?;
    }
    anyhow::ensure!(
        total == length,
        "snapshot memory changed while it was copied (expected {length} bytes, copied {total} bytes)",
    );
    Ok("dense-copy")
}

#[cfg(not(any(target_os = "linux", windows)))]
fn clone_or_copy(
    source: &std::fs::File,
    destination: &std::fs::File,
    length: u64,
) -> anyhow::Result<&'static str> {
    size_empty_file(destination, length, "snapshot clone destination")?;
    copy_nonzero_data(source, destination, 0, length)?;
    Ok("zero-scan")
}

#[cfg(target_os = "linux")]
fn copy_allocated_ranges(
    source: &std::fs::File,
    destination: &std::fs::File,
    length: u64,
    ranges: &[(u64, u64)],
) -> anyhow::Result<()> {
    let mut cursor = 0_u64;
    for &(offset, range_length) in ranges {
        let end = offset
            .checked_add(range_length)
            .context("allocated range overflowed u64")?;
        anyhow::ensure!(
            offset >= cursor && range_length != 0 && end <= length,
            "invalid allocated range for sparse copy"
        );
        copy_nonzero_data(source, destination, offset, range_length)?;
        cursor = end;
    }
    Ok(())
}

#[cfg(not(windows))]
fn copy_nonzero_data(
    source: &std::fs::File,
    destination: &std::fs::File,
    offset: u64,
    length: u64,
) -> anyhow::Result<()> {
    let end = offset
        .checked_add(length)
        .context("snapshot copy range overflowed u64")?;
    let mut cursor = offset;
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    while cursor < end {
        let count = usize::try_from((end - cursor).min(buffer.len() as u64)).unwrap();
        let read = read_file_at(source, &mut buffer[..count], cursor)
            .context("failed to read snapshot source")?;
        anyhow::ensure!(
            read != 0,
            "snapshot source changed while it was being copied"
        );
        if buffer[..read].iter().any(|byte| *byte != 0) {
            write_file_all_at(destination, &buffer[..read], cursor)
                .context("failed to write snapshot destination")?;
        }
        cursor = cursor
            .checked_add(read as u64)
            .context("snapshot copy offset overflowed u64")?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn read_file_at(file: &std::fs::File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_at(file, buffer, offset)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::FileExt::seek_read(file, buffer, offset)
    }
}

#[cfg(not(windows))]
fn write_file_all_at(
    file: &std::fs::File,
    mut bytes: &[u8],
    mut offset: u64,
) -> std::io::Result<()> {
    while !bytes.is_empty() {
        #[cfg(unix)]
        let written = std::os::unix::fs::FileExt::write_at(file, bytes, offset)?;
        #[cfg(windows)]
        let written = std::os::windows::fs::FileExt::seek_write(file, bytes, offset)?;
        if written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "failed to write snapshot destination",
            ));
        }
        bytes = &bytes[written..];
        offset = offset
            .checked_add(written as u64)
            .ok_or_else(|| std::io::Error::other("snapshot write offset overflowed u64"))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_allocated_ranges(file: &std::fs::File, length: u64) -> std::io::Result<Vec<(u64, u64)>> {
    use std::os::fd::AsRawFd;

    let extent_file = std::fs::File::open(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    let mut ranges = Vec::new();
    let mut cursor = 0_u64;
    while cursor < length {
        let Some(data) = pal::unix::fs::seek_data(&extent_file, cursor)? else {
            break;
        };
        if data >= length {
            break;
        }
        let hole = pal::unix::fs::seek_hole(&extent_file, data)?.unwrap_or(length);
        if hole <= data || hole > length {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "filesystem returned an invalid sparse extent",
            ));
        }
        ranges.push((data, hole - data));
        cursor = hole;
    }
    Ok(ranges)
}

#[cfg(target_os = "linux")]
pub(super) fn allocated_file_bytes(file: &std::fs::File, _length: u64) -> anyhow::Result<u64> {
    use std::os::unix::fs::MetadataExt;

    file.metadata()?
        .blocks()
        .checked_mul(512)
        .context("allocated byte count overflowed u64")
}

#[cfg(windows)]
pub(super) fn allocated_file_bytes(file: &std::fs::File, length: u64) -> anyhow::Result<u64> {
    match pal::windows::fs::sparse::allocated_ranges(file, length) {
        Ok(ranges) => ranges
            .into_iter()
            .try_fold(0_u64, |total, (_, range_length)| {
                total
                    .checked_add(range_length)
                    .context("allocated byte count overflowed u64")
            }),
        Err(error) => {
            tracing::debug!(
                error = &error as &dyn std::error::Error,
                "allocated-range accounting is unavailable"
            );
            pal::windows::fs::sparse::allocation_size(file).map_err(anyhow::Error::new)
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
pub(super) fn allocated_file_bytes(file: &std::fs::File, _length: u64) -> anyhow::Result<u64> {
    Ok(file.metadata()?.len())
}

pub(super) fn open_regular_file(path: &Path, description: &str) -> anyhow::Result<std::fs::File> {
    open_regular_file_impl(path, description)
}

fn open_regular_file_impl(path: &Path, description: &str) -> anyhow::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    let file = options
        .open(path)
        .with_context(|| format!("failed to open {description} at {}", path.display()))?;
    validate_opened_regular_file(&file, description, path)?;
    Ok(file)
}

fn validate_opened_regular_file(
    file: &std::fs::File,
    description: &str,
    path: &Path,
) -> anyhow::Result<()> {
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect opened {description}"))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "{description} is not a regular file: {}",
        path.display(),
    );
    reject_windows_reparse_point(&metadata, description, path)
}

#[cfg(windows)]
fn reject_windows_reparse_point(
    metadata: &std::fs::Metadata,
    description: &str,
    path: &Path,
) -> anyhow::Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    anyhow::ensure!(
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "{description} is a reparse point: {}",
        path.display(),
    );
    Ok(())
}

#[cfg(not(windows))]
fn reject_windows_reparse_point(
    _metadata: &std::fs::Metadata,
    _description: &str,
    _path: &Path,
) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub(super) fn sync_directory(path: &Path) -> anyhow::Result<()> {
    fs_err::File::open(path)
        .with_context(|| format!("failed to open directory {} for flush", path.display()))?
        .sync_all()
        .with_context(|| format!("failed to flush directory {}", path.display()))
}

#[cfg(not(unix))]
pub(super) fn sync_directory(_path: &Path) -> anyhow::Result<()> {
    // Windows has no portable directory-flush equivalent. Artifact handles are
    // flushed before the same-volume directory rename, which remains the
    // publication boundary; crash durability of that rename is filesystem-defined.
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::Seek;
    use std::io::SeekFrom;

    #[test]
    fn sparse_copy_fallbacks_preserve_extents_and_clone_independence() {
        const MEMORY_SIZE: u64 = 16 * 1024 * 1024;
        const EXTENT_SIZE: u64 = 4096;
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("source.bin");
        let mut source = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(source_path)
            .unwrap();
        source.set_len(MEMORY_SIZE).unwrap();
        source.write_all(b"head").unwrap();
        source.seek(SeekFrom::End(-4)).unwrap();
        source.write_all(b"tail").unwrap();
        source.sync_all().unwrap();

        let mut expected = vec![0_u8; MEMORY_SIZE as usize];
        let tail_offset = expected.len() - 4;
        expected[..4].copy_from_slice(b"head");
        expected[tail_offset..].copy_from_slice(b"tail");

        let allocated_ranges = [(0, EXTENT_SIZE), (MEMORY_SIZE - EXTENT_SIZE, EXTENT_SIZE)];
        for fallback in ["allocated-ranges", "zero-scan"] {
            let path = dir.path().join(format!("{fallback}.bin"));
            let destination = create_file(&path, fallback).unwrap();
            size_empty_file(&destination, MEMORY_SIZE, fallback).unwrap();
            match fallback {
                "allocated-ranges" => {
                    copy_allocated_ranges(&source, &destination, MEMORY_SIZE, &allocated_ranges)
                        .unwrap()
                }
                "zero-scan" => copy_nonzero_data(&source, &destination, 0, MEMORY_SIZE).unwrap(),
                _ => unreachable!(),
            }
            destination.sync_all().unwrap();
            let allocated = allocated_file_bytes(&destination, MEMORY_SIZE).unwrap();
            assert!(
                allocated < MEMORY_SIZE / 4,
                "{fallback} allocated {allocated} bytes for a {MEMORY_SIZE}-byte file",
            );
        }

        source.seek(SeekFrom::Start(0)).unwrap();
        source.write_all(b"xxxx").unwrap();
        source.sync_all().unwrap();
        for fallback in ["allocated-ranges", "zero-scan"] {
            assert_eq!(
                std::fs::read(dir.path().join(format!("{fallback}.bin"))).unwrap(),
                expected,
                "{fallback}"
            );
        }
    }
}
