// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A resolver for the shared-disk primitive, which allows a single backing
//! store to be resolved exactly once and shared (by cheap clone) among multiple
//! independently-resolved consumers.
//!
//! This is used, for example, to attach the same disk to both an emulated IDE
//! drive and its storvsp accelerator channel without opening the backing store
//! twice. Opening once is required for backing stores that only support a
//! single open (such as an NVMe namespace opened exclusively).
//!
//! Unlike most disk resolvers, [`SharedDiskResolver`] is stateful and must be
//! registered per-VM with the [`vm_resource::ResourceResolver`] (via
//! [`vm_resource::ResourceResolver::add_async_resolver`]) rather than as a
//! static resolver, since it caches resolved disks for the lifetime of the VM's
//! resolver.

use crate::Disk;
use crate::resolve::ResolveDiskParameters;
use crate::resolve::ResolvedDisk;
use async_trait::async_trait;
use disk_backend_resources::SharedDiskHandle;
use disk_backend_resources::SharedDiskRefHandle;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;
use vm_resource::kind::DiskHandleKind;

/// A stateful resolver for [`SharedDiskHandle`] and [`SharedDiskRefHandle`].
///
/// Cheaply cloneable; all clones share the same underlying cache and key
/// allocator. Register the same instance for both handle types on a
/// [`ResourceResolver`].
#[derive(Clone, Default)]
pub struct SharedDiskResolver {
    disks: Arc<Mutex<HashMap<u64, Disk>>>,
    next_key: Arc<AtomicU64>,
}

/// Errors that can occur when resolving a shared disk.
#[derive(Debug, Error)]
pub enum SharedDiskError {
    /// Failed to resolve the underlying disk.
    #[error("failed to resolve shared disk backing")]
    Resolve(#[source] ResolveError),
    /// A shared-disk reference was resolved before its disk was made available.
    #[error("shared disk {0} has not been resolved yet")]
    NotResolved(u64),
}

impl SharedDiskResolver {
    /// Creates a new, empty resolver.
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocates a fresh key that is unique within this resolver.
    pub fn new_key(&self) -> u64 {
        self.next_key.fetch_add(1, Ordering::Relaxed)
    }

    /// Registers an already-resolved disk, returning the key that a
    /// [`SharedDiskRefHandle`] should use to reference it.
    ///
    /// This is used when the caller resolves the backing store itself (for
    /// example, OpenHCL resolves IDE disks inline) and wants to hand the
    /// resulting disk to a resource-resolved consumer.
    pub fn register(&self, disk: Disk) -> u64 {
        let key = self.new_key();
        self.disks.lock().insert(key, disk);
        key
    }
}

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, SharedDiskHandle> for SharedDiskResolver {
    type Output = ResolvedDisk;
    type Error = SharedDiskError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: SharedDiskHandle,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        // Carrier resolution is idempotent: reuse the disk if it was already
        // resolved or registered (e.g. by the OpenHCL inline path).
        {
            let disks = self.disks.lock();
            if let Some(disk) = disks.get(&resource.key) {
                return Ok(ResolvedDisk(disk.clone()));
            }
        }

        let resolved = resolver
            .resolve::<DiskHandleKind, _>(resource.inner, input)
            .await
            .map_err(SharedDiskError::Resolve)?;

        let disk = self
            .disks
            .lock()
            .entry(resource.key)
            .or_insert(resolved.0)
            .clone();

        Ok(ResolvedDisk(disk))
    }
}

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, SharedDiskRefHandle> for SharedDiskResolver {
    type Output = ResolvedDisk;
    type Error = SharedDiskError;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: SharedDiskRefHandle,
        _input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let disk = self
            .disks
            .lock()
            .get(&resource.key)
            .cloned()
            .ok_or(SharedDiskError::NotResolved(resource.key))?;

        Ok(ResolvedDisk(disk))
    }
}
