// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mesh worker definitions for the VM worker.

use crate::config::Config;
use crate::config::Hypervisor;
use crate::rpc::VmRpc;
use anyhow::Context;
use mesh::MeshPayload;
use mesh::payload::message::ProtobufMessage;
use mesh_worker::WorkerId;
use vmm_core_defs::HaltReason;

/// File descriptor (Unix) or handle (Windows) for file-backed guest RAM.
#[cfg(unix)]
pub type SharedMemoryFd = std::os::fd::OwnedFd;
/// File descriptor (Unix) or handle (Windows) for file-backed guest RAM.
#[cfg(windows)]
pub type SharedMemoryFd = std::os::windows::io::OwnedHandle;

/// Open (or create) a file to back guest RAM, and return the appropriate
/// fd/handle for use as shared memory.
///
/// If the file is newly created (size 0), it is extended to `size` bytes.
/// If it already exists with a different size, an error is returned.
pub fn open_memory_backing_file(
    path: &std::path::Path,
    size: u64,
) -> anyhow::Result<SharedMemoryFd> {
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

    file_to_shared_memory_fd(file.into())
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

pub const VM_WORKER: WorkerId<VmWorkerParameters> = WorkerId::new("VmWorker");

/// Launch parameters for the VM worker.
#[derive(MeshPayload)]
pub struct VmWorkerParameters {
    /// The hypervisor to use.
    pub hypervisor: Option<Hypervisor>,
    /// The initial configuration.
    pub cfg: Config,
    /// The saved state.
    pub saved_state: Option<ProtobufMessage>,
    /// File-backed guest RAM handle. When set, guest memory uses this
    /// fd/handle instead of allocating anonymous memory.
    pub shared_memory: Option<SharedMemoryFd>,
    /// The VM RPC channel.
    pub rpc: mesh::Receiver<VmRpc>,
    /// The notification channel.
    pub notify: mesh::Sender<HaltReason>,
}
