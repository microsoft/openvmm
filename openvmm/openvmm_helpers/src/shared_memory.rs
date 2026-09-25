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
    file_to_shared_memory_fd(open_memory_backing_file_handle(path, size)?)
}

/// Opens and sizes a file backing guest RAM without converting its handle.
///
/// Snapshot capture uses this to give the worker a duplicate while retaining
/// the exact same underlying file for publication.
pub fn open_memory_backing_file_handle(
    path: &std::path::Path,
    size: u64,
) -> anyhow::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("failed to open memory backing file {}", path.display()))?;

    let existing_len = file.metadata()?.len();
    if existing_len == 0 {
        crate::snapshot::fs::initialize_snapshot_memory_backing_file(&file, size)
            .context("failed to initialize snapshot memory backing file")?;
    } else if existing_len != size {
        anyhow::bail!(
            "memory backing file {} has size {} bytes, expected {} bytes",
            path.display(),
            existing_len,
            size,
        );
    }

    Ok(file)
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
