// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A worker that runs chipset devices in a separate process.

// UNSAFETY: The guest_memory_proxy module implements GuestMemoryAccess which
// requires unsafe code to work with raw pointers for reading/writing guest memory.
#![expect(unsafe_code)]

use mesh::MeshPayload;

/// Guest memory proxy for remote access.
mod guestmem;
/// The internal protocol for communications between the proxy and the device wrapper.
mod protocol;
/// The proxy for communicating with a remote chipset device.
mod proxy;
/// The resolver for remote chipset devices.
pub mod resolver;
/// The worker implementation.
pub mod worker;

/// Trait for registering dynamic resolvers needed for remote chipset devices.
pub trait RemoteDynamicResolvers: MeshPayload + Send + Sync + Clone + 'static {
    #[allow(async_fn_in_trait)]
    /// Register dynamic resolvers needed for remote chipset devices.
    async fn register_remote_dynamic_resolvers(
        self,
        resolver: &mut vm_resource::ResourceResolver,
    ) -> anyhow::Result<()>;
}
