// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run virtio devices over different transports

use crate::MAX_QUEUE_SIZE;
use chipset_device::io::deferred::DeferredRead;
use chipset_device::io::deferred::DeferredWrite;
use std::io;

pub(crate) mod core;
mod mmio;
mod pci;
pub(crate) mod saved_state;
mod task;

/// An MMIO access that arrived while the transport state machine was busy
/// (enable or disable in flight).  Stalled accesses are replayed in order
/// once the in-flight operation completes.
pub(crate) enum StalledIo {
    Read {
        address: u64,
        len: usize,
        deferred: DeferredRead,
    },
    Write {
        address: u64,
        data: [u8; 8],
        len: usize,
        deferred: DeferredWrite,
    },
}

/// Validate that a queue size returned by a device is acceptable for use by a
/// transport: non-zero, power of two, and within [`MAX_QUEUE_SIZE`].
///
/// Note that only split queues require a power of two size, but since we don't
/// know which type of queue the guest will select, the default queue size must
/// be a power of two to be compatible with both packed and split queues.
fn validate_queue_size(queue_index: u16, size: u16) -> io::Result<()> {
    if size == 0 || !size.is_power_of_two() || size > MAX_QUEUE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "invalid queue size {size} for queue {queue_index}: \
                 must be a power of two in 1..={MAX_QUEUE_SIZE}"
            ),
        ));
    }
    Ok(())
}

pub use mmio::VirtioMmioDevice;
pub use pci::PciInterruptModel;
pub use pci::VirtioPciDevice;
