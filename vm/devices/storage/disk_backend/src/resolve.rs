// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver-related definitions for disk resources.

use crate::Disk;
use crate::DiskIo;
use crate::InvalidDisk;
use vm_resource::CanResolveTo;
use vm_resource::kind::DiskHandleKind;

impl CanResolveTo<ResolvedDisk> for DiskHandleKind {
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

/// A resolved [`Disk`].
pub struct ResolvedDisk(pub Disk);

impl ResolvedDisk {
    /// Returns a resolved disk wrapping a backing object.
    pub fn new<T: DiskIo>(disk: T) -> Result<Self, InvalidDisk> {
        Ok(Self(Disk::new(disk)?))
    }
}
