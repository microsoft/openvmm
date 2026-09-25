// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! File-system helpers for snapshot artifacts, including their
//! platform-specific implementations: private files and directories,
//! no-replace renames and directory flushes, directory-relative artifact
//! access and file generations, exact-file hard links, sparse-aware clone and
//! copy with allocation accounting, and SHA-256 over file handles.

use super::format::validate_sha256;
use anyhow::Context;
use sha2::Digest;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

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

#[cfg(target_os = "linux")]
pub(super) fn create_hard_link_from_handle(
    source: &std::fs::File,
    directory: &std::fs::File,
    name: &str,
) -> std::io::Result<&'static str> {
    use nix::fcntl::AT_FDCWD;
    use nix::fcntl::AtFlags;
    use std::os::fd::AsRawFd;

    match nix::unistd::linkat(
        source,
        Path::new(""),
        directory,
        name,
        AtFlags::AT_EMPTY_PATH,
    ) {
        Ok(()) => return Ok("linkat-empty-path"),
        Err(error)
            if matches!(
                error,
                nix::errno::Errno::EPERM | nix::errno::Errno::EINVAL | nix::errno::Errno::ENOENT
            ) =>
        {
            tracing::debug!(
                error = &nix_error(error) as &dyn std::error::Error,
                "AT_EMPTY_PATH hard link is unavailable"
            );
        }
        Err(error) => return Err(nix_error(error)),
    }

    let source_path = PathBuf::from(format!("/proc/self/fd/{}", source.as_raw_fd()));
    // Following is required to link the descriptor's target rather than the
    // procfs symlink itself. The descriptor remains live, and the caller proves
    // the resulting device/inode against `source` before publication.
    nix::unistd::linkat(
        AT_FDCWD,
        &source_path,
        directory,
        name,
        AtFlags::AT_SYMLINK_FOLLOW,
    )
    .map(|()| "linkat-proc-fd")
    .map_err(nix_error)
}

#[cfg(windows)]
pub(super) fn create_hard_link_from_handle(
    source: &std::fs::File,
    directory: &std::fs::File,
    name: &str,
) -> std::io::Result<&'static str> {
    pal::windows::fs::relative::hard_link_relative(source, directory, std::ffi::OsStr::new(name))?;
    Ok("file-link-information")
}

#[cfg(not(any(target_os = "linux", windows)))]
pub(super) fn create_hard_link_from_handle(
    _source: &std::fs::File,
    _directory: &std::fs::File,
    _name: &str,
) -> std::io::Result<&'static str> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "exact-file hard links are unsupported on this platform",
    ))
}

#[cfg(target_os = "linux")]
pub(super) fn hard_link_is_unsupported(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(
            libc::EACCES
                | libc::EMLINK
                | libc::EINVAL
                | libc::ELOOP
                | libc::ENOENT
                | libc::ENOSYS
                | libc::EOPNOTSUPP
                | libc::EPERM
                | libc::EXDEV
        )
    )
}

#[cfg(windows)]
pub(super) fn hard_link_is_unsupported(error: &std::io::Error) -> bool {
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::Foundation::ERROR_FILE_SYSTEM_LIMITATION;
    use windows_sys::Win32::Foundation::ERROR_INVALID_FUNCTION;
    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows_sys::Win32::Foundation::ERROR_NOT_SAME_DEVICE;
    use windows_sys::Win32::Foundation::ERROR_NOT_SUPPORTED;
    use windows_sys::Win32::Foundation::ERROR_PRIVILEGE_NOT_HELD;
    use windows_sys::Win32::Foundation::ERROR_TOO_MANY_LINKS;

    matches!(
        error.raw_os_error(),
        Some(raw) if matches!(
            raw as u32,
            ERROR_ACCESS_DENIED
                | ERROR_FILE_SYSTEM_LIMITATION
                | ERROR_INVALID_FUNCTION
                | ERROR_INVALID_PARAMETER
                | ERROR_NOT_SAME_DEVICE
                | ERROR_NOT_SUPPORTED
                | ERROR_PRIVILEGE_NOT_HELD
                | ERROR_TOO_MANY_LINKS
        )
    )
}

#[cfg(not(any(target_os = "linux", windows)))]
pub(super) fn hard_link_is_unsupported(_error: &std::io::Error) -> bool {
    true
}

pub(super) fn verify_hard_link_identity(
    source: &std::fs::File,
    linked: &std::fs::File,
    expected_length: u64,
) -> anyhow::Result<()> {
    let source_metadata = source
        .metadata()
        .context("failed to re-inspect automatic snapshot RAM handle")?;
    let linked_metadata = linked
        .metadata()
        .context("failed to inspect linked snapshot memory")?;
    anyhow::ensure!(
        source_metadata.file_type().is_file() && linked_metadata.file_type().is_file(),
        "automatic snapshot RAM hard link does not resolve to regular files"
    );
    anyhow::ensure!(
        source_metadata.len() == expected_length && linked_metadata.len() == expected_length,
        "automatic snapshot RAM hard-link EOF does not match manifest ({expected_length} bytes)"
    );

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            source_metadata.dev() == linked_metadata.dev()
                && source_metadata.ino() == linked_metadata.ino(),
            "automatic snapshot RAM hard link has the wrong device or inode"
        );
    }
    #[cfg(windows)]
    {
        let source_identity = pal::windows::fs::relative::file_identity(source)
            .context("failed to query automatic snapshot RAM identity")?;
        let linked_identity = pal::windows::fs::relative::file_identity(linked)
            .context("failed to query linked snapshot RAM identity")?;
        anyhow::ensure!(
            source_identity == linked_identity && source_identity.end_of_file == expected_length,
            "automatic snapshot RAM hard link has the wrong file identity or EOF"
        );
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    anyhow::bail!("exact-file hard-link identity checks are unsupported on this platform");

    Ok(())
}

/// Sizes a newly created snapshot RAM backing.
///
/// Windows leaves the file non-sparse to avoid slow copy-on-write faults from
/// sparse files. Other supported platforms retain sparse allocation.
pub fn initialize_snapshot_memory_backing_file(
    file: &std::fs::File,
    size: u64,
) -> anyhow::Result<u64> {
    let metadata = file
        .metadata()
        .context("failed to inspect new snapshot memory backing")?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "snapshot memory backing handle is not a regular file"
    );
    anyhow::ensure!(
        metadata.len() == 0,
        "new snapshot memory backing is not empty"
    );

    size_empty_file(file, size, "snapshot memory backing")?;
    let allocated_bytes = allocated_file_bytes(file, size)
        .context("failed to inspect snapshot memory backing allocation")?;
    #[cfg(not(windows))]
    anyhow::ensure!(
        allocated_bytes == 0,
        "snapshot memory backing allocated {allocated_bytes} bytes while sizing to {size} bytes"
    );
    tracing::info!(
        logical_bytes = size,
        allocated_bytes,
        "initialized snapshot RAM backing"
    );
    Ok(allocated_bytes)
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

pub(super) struct OpenedSnapshotDirectory {
    pub(super) file: std::fs::File,
    path: PathBuf,
}

impl OpenedSnapshotDirectory {
    pub(super) fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_impl(path, false)
    }

    pub(super) fn open_for_publication(path: &Path) -> anyhow::Result<Self> {
        Self::open_impl(path, true)
    }

    fn open_impl(path: &Path, allow_changes: bool) -> anyhow::Result<Self> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
            use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
            use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
            use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;

            options
                .share_mode(if allow_changes {
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
                } else {
                    FILE_SHARE_READ
                })
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
        }
        #[cfg(not(windows))]
        let _ = allow_changes;
        let file = options
            .open(path)
            .with_context(|| format!("failed to open snapshot directory {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("failed to inspect snapshot directory {}", path.display()))?;
        anyhow::ensure!(
            metadata.file_type().is_dir(),
            "snapshot path is not a directory: {}",
            path.display(),
        );
        reject_windows_reparse_point(&metadata, "snapshot directory", path)?;
        Ok(Self {
            file,
            path: path.to_owned(),
        })
    }

    pub(super) fn open_regular_file(
        &self,
        name: &str,
        description: &str,
    ) -> anyhow::Result<std::fs::File> {
        #[cfg(target_os = "linux")]
        let file = {
            use nix::fcntl::OFlag;
            use nix::sys::stat::Mode;

            let fd = nix::fcntl::openat(
                &self.file,
                name,
                OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
                Mode::empty(),
            )
            .map_err(nix_error)
            .with_context(|| {
                format!(
                    "failed to open {description} at {}",
                    self.display_path(name).display()
                )
            })?;
            std::fs::File::from(fd)
        };
        #[cfg(windows)]
        let file = pal::windows::fs::relative::open_relative_read_only(
            &self.file,
            std::ffi::OsStr::new(name),
        )
        .with_context(|| {
            format!(
                "failed to open {description} at {}",
                self.display_path(name).display()
            )
        })?;
        #[cfg(not(any(target_os = "linux", windows)))]
        let file = open_regular_file_impl(&self.path.join(name), description)?;

        validate_opened_regular_file(&file, description, &self.display_path(name))?;
        Ok(file)
    }

    pub(super) fn open_regular_file_for_identity(
        &self,
        name: &str,
        description: &str,
    ) -> anyhow::Result<std::fs::File> {
        #[cfg(windows)]
        let file = pal::windows::fs::relative::open_relative_for_identity(
            &self.file,
            std::ffi::OsStr::new(name),
        )
        .with_context(|| {
            format!(
                "failed to open {description} at {}",
                self.display_path(name).display()
            )
        })?;
        #[cfg(not(windows))]
        let file = self.open_regular_file(name, description)?;

        validate_opened_regular_file(&file, description, &self.display_path(name))?;
        Ok(file)
    }

    pub(super) fn open_file_with_length(
        &self,
        name: &str,
        expected_length: u64,
        artifact_name: &str,
    ) -> anyhow::Result<std::fs::File> {
        let file = self.open_regular_file(name, artifact_name)?;
        let length = opened_file_generation(&file, artifact_name)?.length();
        anyhow::ensure!(
            length == expected_length,
            "{artifact_name} size ({length} bytes) doesn't match manifest ({expected_length} bytes)",
        );
        Ok(file)
    }

    pub(super) fn entry_names(&self) -> anyhow::Result<Vec<std::ffi::OsString>> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::OwnedFd;
            use std::os::unix::ffi::OsStrExt;

            let directory: OwnedFd = self
                .file
                .try_clone()
                .context("failed to duplicate snapshot directory handle")?
                .into();
            let mut directory = nix::dir::Dir::from_fd(directory).map_err(nix_error)?;
            let mut names = Vec::new();
            for entry in directory.iter() {
                let entry = entry.map_err(nix_error)?;
                let name = std::ffi::OsStr::from_bytes(entry.file_name().to_bytes());
                if name != "." && name != ".." {
                    names.push(name.to_owned());
                }
            }
            Ok(names)
        }
        #[cfg(windows)]
        {
            pal::windows::fs::relative::directory_entry_names(&self.file)
                .context("failed to enumerate opened snapshot directory")
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            fs_err::read_dir(&self.path)
                .with_context(|| {
                    format!(
                        "failed to enumerate snapshot directory {}",
                        self.path.display()
                    )
                })?
                .map(|entry| {
                    entry
                        .context("failed to inspect snapshot directory entry")
                        .map(|entry| entry.file_name())
                })
                .collect()
        }
    }

    pub(super) fn display_path(&self, name: impl AsRef<Path>) -> PathBuf {
        self.path.join(name)
    }

    pub(super) fn into_file(self) -> std::fs::File {
        self.file
    }
}

#[cfg(target_os = "linux")]
fn nix_error(error: nix::errno::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(error as i32)
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

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OpenedFileGeneration {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OpenedFileGeneration {
    volume_serial_number: u64,
    file_id: [u8; 16],
    length: u64,
}

#[cfg(not(any(target_os = "linux", windows)))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OpenedFileGeneration {
    length: u64,
}

impl OpenedFileGeneration {
    pub(super) fn length(self) -> u64 {
        self.length
    }
}

#[cfg(target_os = "linux")]
pub(super) fn opened_file_generation(
    file: &std::fs::File,
    description: &str,
) -> anyhow::Result<OpenedFileGeneration> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect opened {description}"))?;
    Ok(OpenedFileGeneration {
        device: metadata.dev(),
        inode: metadata.ino(),
        length: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    })
}

#[cfg(windows)]
pub(super) fn opened_file_generation(
    file: &std::fs::File,
    description: &str,
) -> anyhow::Result<OpenedFileGeneration> {
    let identity = pal::windows::fs::relative::file_identity(file)
        .with_context(|| format!("failed to query {description} FILE_ID_INFO and EOF"))?;
    Ok(OpenedFileGeneration {
        volume_serial_number: identity.volume_serial_number,
        file_id: identity.file_id,
        length: identity.end_of_file,
    })
}

#[cfg(not(any(target_os = "linux", windows)))]
pub(super) fn opened_file_generation(
    file: &std::fs::File,
    description: &str,
) -> anyhow::Result<OpenedFileGeneration> {
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect opened {description}"))?;
    Ok(OpenedFileGeneration {
        length: metadata.len(),
    })
}

pub(super) fn read_bounded_open_file(
    file: &std::fs::File,
    maximum_size: u64,
    description: &str,
) -> anyhow::Result<Vec<u8>> {
    let generation = opened_file_generation(file, description)?;
    let length = generation.length();
    anyhow::ensure!(
        length <= maximum_size,
        "{description} is {length} bytes, exceeding the maximum of {maximum_size} bytes",
    );
    let capacity = usize::try_from(length).context("artifact length does not fit in usize")?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut reader = file
        .try_clone()
        .with_context(|| format!("failed to duplicate {description} handle"))?;
    reader
        .seek(SeekFrom::Start(0))
        .with_context(|| format!("failed to rewind {description}"))?;
    reader
        .take(maximum_size + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {description}"))?;
    anyhow::ensure!(
        bytes.len() as u64 == length && opened_file_generation(file, description)? == generation,
        "{description} changed while it was being read",
    );
    Ok(bytes)
}

/// Computes SHA-256 over an exact-length regular file handle.
pub fn file_sha256(
    file: &std::fs::File,
    expected_length: u64,
    description: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut file = file
        .try_clone()
        .with_context(|| format!("failed to duplicate {description} handle"))?;
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("failed to rewind {description}"))?;
    let actual_length = file
        .metadata()
        .with_context(|| format!("failed to inspect {description}"))?
        .len();
    anyhow::ensure!(
        actual_length == expected_length,
        "{description} size ({actual_length} bytes) doesn't match expected ({expected_length} bytes)"
    );
    let mut digest = sha2::Sha256::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    let mut total = 0_u64;
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {description}"))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        total = total
            .checked_add(count as u64)
            .context("file length overflowed u64 while hashing")?;
        anyhow::ensure!(
            total <= expected_length,
            "{description} grew while it was being hashed"
        );
    }
    anyhow::ensure!(
        total == expected_length,
        "{description} changed while it was being hashed"
    );
    Ok(digest.finalize().to_vec())
}

pub(super) fn verify_file_digest(
    file: &std::fs::File,
    expected_length: u64,
    expected_digest: &[u8],
    description: &str,
) -> anyhow::Result<()> {
    validate_sha256(expected_digest, description)?;
    anyhow::ensure!(
        file_sha256(file, expected_length, description)? == expected_digest,
        "{description} SHA-256 digest mismatch"
    );
    Ok(())
}

pub(super) fn open_file_with_length(
    path: &Path,
    expected_length: u64,
    artifact_name: &str,
) -> anyhow::Result<std::fs::File> {
    let file = open_regular_file(path, artifact_name)?;
    let length = opened_file_generation(&file, artifact_name)?.length();
    anyhow::ensure!(
        length == expected_length,
        "{artifact_name} size ({length} bytes) doesn't match manifest ({expected_length} bytes)",
    );
    Ok(file)
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
        initialize_snapshot_memory_backing_file(&source, MEMORY_SIZE).unwrap();
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
