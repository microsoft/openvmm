// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver-related definitions for disk resources.

use crate::SimpleDisk;
use std::sync::Arc;
use vm_resource::kind::DiskHandleKind;
use vm_resource::CanResolveTo;

impl CanResolveTo<ResolvedSimpleDisk> for DiskHandleKind {
    type Input<'a> = ResolveDiskParameters<'a>;
}

/// Parameters used when resolving a disk resource.
#[derive(Copy, Clone)]
pub struct ResolveDiskParameters<'a> {
    /// Whether the disk is being opened for read-only use.
    pub read_only: bool,
    #[doc(hidden)]
    // Workaround for async_trait not working well with GAT input parameters
    // with missing lifetimes. Remove once we stop using async_trait for async
    // resolvers.
    pub _async_trait_workaround: &'a (),
}

/// A resolved [`SimpleDisk`].
pub struct ResolvedSimpleDisk(pub Arc<dyn SimpleDisk>);

impl<T: 'static + SimpleDisk> From<T> for ResolvedSimpleDisk {
    fn from(value: T) -> Self {
        Self(Arc::new(value))
    }
}
