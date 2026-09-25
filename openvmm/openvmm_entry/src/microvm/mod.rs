// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM machine profile support for the OpenVMM entry point.

mod config;
mod launch;
mod restore;

pub(crate) use config::MicrovmConfigBuilder;
pub(crate) use launch::MicrovmLaunch;
pub(crate) use restore::ExpectedRestoreContract;
pub(crate) use restore::MicrovmRestore;
pub(crate) use restore::prepare_restore;
pub(crate) use restore::validate_restore_contract;

use chipset_resources::microvm::MicrovmSnapshotBoundaryRequest;

/// Host-side microVM resources produced while building the VM configuration
/// and consumed by the snapshot, restore, and teardown paths.
#[derive(Default)]
pub(crate) struct MicrovmResources {
    /// Guest-requested snapshot boundaries from the snapshot-request port.
    pub(crate) snapshot_requests: Option<mesh::Receiver<MicrovmSnapshotBoundaryRequest>>,
}
