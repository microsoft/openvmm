// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A task + RPC client for interacting with a shared VMGS instance.

#![forbid(unsafe_code)]

mod broker;
mod client;
pub mod non_volatile_store;
pub mod resolver;

pub use client::VmgsClient;
pub use client::VmgsClientError;

use crate::broker::VmgsBrokerTask;
use pal_async::task::Spawn;
use pal_async::task::Task;

/// Given a fully-initialized VMGS instance, return a VMGS broker task +
/// clonable VmgsClient
pub fn spawn_vmgs_broker(spawner: impl Spawn, vmgs: vmgs::Vmgs) -> (VmgsClient, Task<()>) {
    let (control_send, control_recv) = mesh::mpsc_channel();

    let process_loop_handle = spawner.spawn("vmgs-broker", async move {
        VmgsBrokerTask::new(vmgs).run(control_recv).await
    });

    (
        VmgsClient {
            control: control_send,
        },
        process_loop_handle,
    )
}

/// A wrapper around [`VmgsClient`] that restricts its API down to operations
/// that perform no storage IO.
///
/// This types is useful for keeping performance-sensitive code "honest" by
/// making it harder for future refactors to accidentally introduce VMGS IO into
/// performance hotpaths.
#[derive(inspect::Inspect)]
#[inspect(transparent)]
pub struct VmgsThinClient(VmgsClient);

impl VmgsThinClient {
    /// Restrict an existing [`VmgsClient`] to only non-IO operations.
    pub fn new(vmgs_client: VmgsClient) -> Self {
        Self(vmgs_client)
    }

    /// See [`VmgsClient::save`]
    pub async fn save(&self) -> Result<vmgs::save_restore::state::SavedVmgsState, VmgsClientError> {
        self.0.save().await
    }
}
