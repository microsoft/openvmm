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
    /// Returns the NVX time ABI primitives, if the partition was built with a
    /// time ABI configuration.
    #[cfg(guest_arch = "x86_64")]
    fn time_abi(&self) -> Option<&dyn virt::time_abi::TimeAbiBackend>;
}

impl<T> MicrovmPartition for T
where
    T: BasicPartitionStateAccess + ArchPartition + PartitionMemoryMapper + PartitionAccessState,
{
    #[cfg(guest_arch = "x86_64")]
    fn time_abi(&self) -> Option<&dyn virt::time_abi::TimeAbiBackend> {
        Partition::time_abi(self)
    }
}
