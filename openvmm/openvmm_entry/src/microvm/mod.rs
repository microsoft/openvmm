// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM machine profile support for the OpenVMM entry point.

mod config;
mod console;
mod filesystem;
mod launch;
mod network;
mod restore;

pub(crate) use config::MicrovmConfigBuilder;
pub(crate) use console::MicrovmConsoleSocketCleanup;
pub(crate) use filesystem::validate_microvm_filesystem_private_storage;
pub(crate) use launch::MicrovmLaunch;
pub(crate) use restore::ExpectedRestoreContract;
pub(crate) use restore::MicrovmRestore;
pub(crate) use restore::prepare_restore;
pub(crate) use restore::validate_restore_contract;

use crate::storage_builder::microvm::MicrovmSandboxBlockSource;
use chipset_resources::microvm::MicrovmSnapshotBoundaryRequest;
use openvmm_helpers::snapshot::microvm::SnapshotAttachment;
use std::path::PathBuf;

/// Host-side microVM resources produced while building the VM configuration
/// and consumed by the snapshot, restore, and teardown paths.
#[derive(Default)]
pub(crate) struct MicrovmResources {
    /// Guest-requested snapshot boundaries from the snapshot-request port.
    pub(crate) snapshot_requests: Option<mesh::Receiver<MicrovmSnapshotBoundaryRequest>>,
    /// Snapshot sources of the fixed-role sandbox blocks, in role order.
    pub(crate) sandbox_block_sources: Vec<MicrovmSandboxBlockSource>,
    /// Snapshot identity of the boot virtio-console endpoint.
    pub(crate) console_attachment: Option<SnapshotAttachment>,
    /// Removes the boot console Unix socket on teardown.
    pub(crate) console_socket_cleanup: Option<MicrovmConsoleSocketCleanup>,
    /// Snapshot identity of the portable network attachment.
    pub(crate) network_attachment: Option<SnapshotAttachment>,
    /// Snapshot identity of the live filesystem root.
    pub(crate) filesystem_attachment: Option<SnapshotAttachment>,
    /// Canonical host path of the live filesystem root.
    pub(crate) filesystem_root_path: Option<PathBuf>,
}
