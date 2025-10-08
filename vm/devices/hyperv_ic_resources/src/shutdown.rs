// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for the shutdown IC.

use mesh::MeshPayload;
use mesh::rpc::Rpc;
use vm_resource::ResourceId;
use vm_resource::kind::VmbusDeviceHandleKind;

/// A handle to a shutdown IC.
#[derive(MeshPayload)]
pub struct ShutdownIcHandle {
    /// The channel by which to receive shutdown requests.
    pub recv: mesh::Receiver<ShutdownRpc>,
}

impl ResourceId<VmbusDeviceHandleKind> for ShutdownIcHandle {
    const ID: &'static str = "shutdown_ic";
}

/// An RPC request to the shutdown IC.
#[derive(MeshPayload)]
pub enum ShutdownRpc {
    /// Wait for the shutdown IC to be ready.
    ///
    /// Returns a receiver that closes when the IC is no longer ready.
    ///
    /// FUTURE: this should instead be a `Sender` that is used to send the
    /// shutdown request. Do this once `Sender` has a mechanism to wait on the
    /// receiver to be closed.
    WaitReady(Rpc<(), mesh::OneshotReceiver<()>>),
    /// Send a shutdown request to the guest.
    Shutdown(Rpc<ShutdownParams, ShutdownResult>),
}

/// Guest shutdown parameters.
#[derive(Debug, MeshPayload)]
pub struct ShutdownParams {
    /// The type of power state change.
    pub shutdown_type: ShutdownType,
    /// Whether to force a shutdown.
    pub force: bool,
}

/// The shutdown type.
#[derive(Debug, MeshPayload)]
pub enum ShutdownType {
    /// Power off the VM.
    PowerOff,
    /// Reboot the VM.
    Reboot,
    /// Hibernate the VM.
    Hibernate,
}

/// The result of a shutdown request.
#[derive(MeshPayload, Debug, PartialEq)]
pub enum ShutdownResult {
    /// The shutdown has been initiated.
    Ok,
    /// The IC is not ready to send shutdown requests.
    NotReady,
    /// A shutdown is already in progress.
    AlreadyInProgress,
    /// The shutdown failed with the given error code.
    Failed(u32),
}
