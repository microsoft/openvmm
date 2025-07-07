// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver-related definitions for disk layer resources.

use super::DiskLayer;
use crate::LayerAttach;
use vm_resource::CanResolveTo;
use vm_resource::kind::DiskLayerHandleKind;
use vmcore::vm_task::VmTaskDriverSource;

impl CanResolveTo<ResolvedDiskLayer> for DiskLayerHandleKind {
    type Input<'a> = ResolveDiskLayerParameters<'a>;
}

/// Parameters used when resolving a disk layer resource.
#[derive(Copy, Clone)]
pub struct ResolveDiskLayerParameters<'a> {
    /// Whether the layer is being opened for read-only use.
    pub read_only: bool,
    /// The task driver source for the VM.
    pub driver_source: &'a VmTaskDriverSource,
}

/// A resolved [`DiskLayer`].
pub struct ResolvedDiskLayer(pub DiskLayer);

impl ResolvedDiskLayer {
    /// Returns a resolved disk wrapping a backing object.
    pub fn new<T: LayerAttach>(layer: T) -> Self {
        Self(DiskLayer::new(layer))
    }
}
