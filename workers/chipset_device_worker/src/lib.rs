// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A worker that runs chipset devices in a separate process.

// UNSAFETY: The guest_memory_proxy module implements GuestMemoryAccess which
// requires unsafe code to work with raw pointers for reading/writing guest memory.
#![expect(unsafe_code)]

use mesh::MeshPayload;

mod guestmem;
mod protocol;
mod proxy;
pub mod resolver;
pub mod worker;

/// Trait for registering dynamic resolvers needed for remote chipset devices.
pub trait RemoteDynamicResolvers: MeshPayload + Send + Sync + Clone + 'static {
    /// Worker ID string for this remote chipset device worker.
    const WORKER_ID_STR: &str;

    #[allow(async_fn_in_trait)]
    /// Register dynamic resolvers needed for remote chipset devices.
    async fn register_remote_dynamic_resolvers(
        self,
        resolver: &mut vm_resource::ResourceResolver,
    ) -> anyhow::Result<()>;
}
