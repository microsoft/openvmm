// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::bnic_driver::BnicDriver;
use crate::gdma_driver::GdmaDriver;
use gdma_defs::GdmaDevId;
use gdma_defs::GdmaQueueType;
use std::mem::ManuallyDrop;
use user_driver::DeviceBacking;
use user_driver::memory::MemoryBlock;

/// A list of allocated device resources.
///
/// The list will be extended by methods that allocate device resources. The
/// list must be deallocated via a `destroy` method on `Vport` or `ManaDevice`.
///
/// If the arena is dropped without calling `destroy`, then device and host
/// resources will leak.
#[derive(Default)]
pub struct ResourceArena {
    resources: Vec<Resource>,
}

pub(crate) enum Resource {
    MemoryBlock(ManuallyDrop<MemoryBlock>),
    DmaRegion {
        dev_id: GdmaDevId,
        gdma_region: u64,
    },
    Eq {
        dev_id: GdmaDevId,
        eq_id: u32,
    },
    BnicQueue {
        dev_id: GdmaDevId,
        wq_type: GdmaQueueType,
        wq_obj: u64,
    },
}

impl ResourceArena {
    /// Creates a new empty resource arena.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true if the arena has no allocated resources.
    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }

    pub(crate) fn push(&mut self, resource: Resource) {
        self.resources.push(resource);
    }

    pub(crate) fn take_dma_region(&mut self, owned_gdma_region: u64) {
        let i = self
            .resources
            .iter()
            .rposition(|r| matches!(r, Resource::DmaRegion { gdma_region, .. } if *gdma_region == owned_gdma_region))
            .expect("gdma region must be in arena");
        self.resources.remove(i);
    }

    pub(crate) async fn destroy<T: DeviceBacking>(mut self, gdma: &mut GdmaDriver<T>) {
        for resource in self.resources.drain(..).rev() {
            let r = match resource {
                Resource::MemoryBlock(mem) => {
                    drop(ManuallyDrop::into_inner(mem));
                    Ok(())
                }
                Resource::DmaRegion {
                    dev_id,
                    gdma_region,
                } => gdma.destroy_dma_region(dev_id, gdma_region).await,
                Resource::Eq { dev_id, eq_id } => gdma.disable_eq(dev_id, eq_id).await,
                Resource::BnicQueue {
                    dev_id,
                    wq_type,
                    wq_obj,
                } => {
                    BnicDriver::new(gdma, dev_id)
                        .destroy_wq_obj(wq_type, wq_obj)
                        .await
                }
            };
            if let Err(err) = r {
                tracing::error!(
                    error = err.as_ref() as &dyn std::error::Error,
                    "failed to tear down resource"
                );
            }
        }
    }
}

impl Drop for ResourceArena {
    fn drop(&mut self) {
        if !self.resources.is_empty() {
            tracing::error!("leaking resources");
        }
    }
}
