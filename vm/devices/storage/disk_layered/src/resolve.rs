// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver-related definitions for disk layer resources.

use super::DiskLayer;
use crate::AttachLayer;
use vm_resource::kind::DiskLayerHandleKind;
use vm_resource::CanResolveTo;

impl CanResolveTo<ResolvedDiskLayer> for DiskLayerHandleKind {
    type Input<'a> = ResolveDiskLayerParameters<'a>;
}

/// Parameters used when resolving a disk layer resource.
#[derive(Copy, Clone)]
pub struct ResolveDiskLayerParameters<'a> {
    /// Whether the layer is being opened for read-only use.
    pub read_only: bool,
    #[doc(hidden)]
    // Workaround for async_trait not working well with GAT input parameters
    // with missing lifetimes. Remove once we stop using async_trait for async
    // resolvers.
    pub _async_trait_workaround: &'a (),
}

/// A resolved [`DiskLayer`].
pub struct ResolvedDiskLayer(pub DiskLayer);

impl ResolvedDiskLayer {
    /// Returns a resolved disk wrapping a backing object.
    pub fn new<T: AttachLayer>(layer: T) -> Self {
        Self(DiskLayer::new(layer))
    }
}
