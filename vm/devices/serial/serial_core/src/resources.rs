// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serial resources, for use with [`vm_resource`].

use super::SerialIo;
use mesh::MeshPayload;
use pal_async::driver::Driver;
use vm_resource::CanResolveTo;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::SerialBackendHandle;

impl CanResolveTo<ResolvedSerialBackend> for SerialBackendHandle {
    type Input<'a> = ResolveSerialBackendParams<'a>;
}

/// Input parameters for serial backend resolution.
pub struct ResolveSerialBackendParams<'a> {
    /// The driver to use for polling IO.
    pub driver: Box<dyn Driver>,
    #[doc(hidden)]
    // Work around for async_trait not working well with GAT input parameters.
    // Remove once we stop using async_trait for async resolvers.
    pub _async_trait_workaround: &'a (),
}

/// A resolved [`SerialBackend`].
pub struct ResolvedSerialBackend(pub Box<dyn SerialBackend>);

impl<T: 'static + SerialBackend> From<T> for ResolvedSerialBackend {
    fn from(value: T) -> Self {
        Self(Box::new(value))
    }
}

/// Trait implemented by types that resolve from [`SerialBackendHandle`]. Provides a
/// [`SerialIo`] implementation but also provides for converting the type back
/// to a resource.
pub trait SerialBackend: Send {
    /// Reclaims the resource.
    fn into_resource(self: Box<Self>) -> Resource<SerialBackendHandle>;
    /// Gets the inner IO trait.
    fn as_io(&self) -> &dyn SerialIo;
    /// Gets the inner IO trait mutably.
    fn as_io_mut(&mut self) -> &mut dyn SerialIo;
    /// Gets the inner IO trait object.
    fn into_io(self: Box<Self>) -> Box<dyn SerialIo>;
}

impl<T: 'static + SerialIo + Into<Resource<SerialBackendHandle>>> SerialBackend for T {
    fn into_resource(self: Box<Self>) -> Resource<SerialBackendHandle> {
        (*self).into()
    }

    fn as_io(&self) -> &dyn SerialIo {
        self
    }

    fn as_io_mut(&mut self) -> &mut dyn SerialIo {
        self
    }

    fn into_io(self: Box<Self>) -> Box<dyn SerialIo> {
        self
    }
}

/// Handle for a disconnected serial backend.
#[derive(MeshPayload)]
pub struct DisconnectedSerialBackendHandle;

impl ResourceId<SerialBackendHandle> for DisconnectedSerialBackendHandle {
    const ID: &'static str = "disconnected";
}
