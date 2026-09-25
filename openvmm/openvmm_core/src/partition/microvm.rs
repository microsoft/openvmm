// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypervisor partition operations used by the microVM profile.

use super::ArchPartition;
use super::BasicPartitionStateAccess;
use virt::Partition;
use virt::PartitionAccessState;
use virt::PartitionMemoryMapper;

/// Backend partition operations required by the microVM lifecycle.
pub trait MicrovmPartition: Send + Sync {
    /// Returns the LAPIC interrupt clock frequency when available.
    fn apic_frequency_hz(&self) -> anyhow::Result<Option<u64>>;
}

impl<T> MicrovmPartition for T
where
    T: BasicPartitionStateAccess + ArchPartition + PartitionMemoryMapper + PartitionAccessState,
{
    fn apic_frequency_hz(&self) -> anyhow::Result<Option<u64>> {
        Ok(Partition::apic_frequency_hz(self)?)
    }
}
