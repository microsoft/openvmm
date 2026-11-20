// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for file-backed shared guest memory.

use anyhow::Context;
use openvmm_defs::worker::SharedMemoryFd;

/// Open (or create) a file to back guest RAM, and return the appropriate
/// fd/handle for use as shared memory.
///
/// If the file is newly created (size 0), it is extended to `size` bytes.
/// If it already exists with a different size, an error is returned.
pub fn open_memory_backing_file(
    path: &std::path::Path,
    size: u64,
) -> anyhow::Result<SharedMemoryFd> {
    file_to_shared_memory_fd(open_memory_backing_file_raw(path, size)?)
}

/// Like [`open_memory_backing_file`], but returns the opened file without
/// converting it to a shared memory handle, so the caller can inspect it
/// first.
pub fn open_memory_backing_file_raw(
    path: &std::path::Path,
    size: u64,
) -> anyhow::Result<std::fs::File> {
    let file = fs_err::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;

    let existing_len = file.metadata()?.len();
    if existing_len == 0 {
        file.set_len(size)
            .context("failed to set memory backing file size")?;
    } else if existing_len != size {
        anyhow::bail!(
            "memory backing file {} has size {} bytes, expected {} bytes",
            path.display(),
            existing_len,
            size,
        );
    }

    Ok(file.into())
}

/// Returns whether two opened files are the same underlying file, including
/// through hard links. Fails if either file's identity cannot be determined.
pub fn same_open_file(a: &std::fs::File, b: &std::fs::File) -> std::io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let (a, b) = (a.metadata()?, b.metadata()?);
        Ok(a.dev() == b.dev() && a.ino() == b.ino())
    }
    #[cfg(windows)]
    {
        use pal::windows::fs::relative::file_identity;
        let (a, b) = (file_identity(a)?, file_identity(b)?);
        Ok(a.volume_serial_number == b.volume_serial_number && a.file_id == b.file_id)
    }
}

/// Convert a `std::fs::File` to the platform-appropriate shared memory handle.
pub fn file_to_shared_memory_fd(file: std::fs::File) -> anyhow::Result<SharedMemoryFd> {
    #[cfg(unix)]
    {
        use std::os::unix::io::OwnedFd;
        Ok(OwnedFd::from(file))
    }
    #[cfg(windows)]
    {
        // On Windows, MapViewOfFile needs a section handle, not a raw file
        // handle. sparse_mmap has a helper that calls CreateFileMappingW.
        Ok(sparse_mmap::new_mappable_from_file(&file, true, false)?)
    }
}
