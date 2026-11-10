// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MMIO intercepts

use crate::ChipsetDevice;
use crate::io::IoError;
use crate::io::IoResult;
use std::future::Future;
use std::ops::RangeInclusive;
use std::pin::Pin;

/// Completion of an optional MMIO compare-and-exchange operation.
///
/// `Ok(Some(true))` indicates that the exchange occurred; `Ok(Some(false))`
/// indicates that the comparison failed. `Ok(None)` means the device did not
/// perform a write and requests the legacy unconditional write behavior.
/// A device must return an error, not `Ok(None)`, when an access requires
/// atomic semantics but cannot be performed.
pub type MmioCompareExchangeFuture =
    Pin<Box<dyn Future<Output = Result<Option<bool>, IoError>> + Send + 'static>>;

/// Memory-like atomic operations for an MMIO device.
pub trait MmioCompareExchange {
    /// Attempt an atomic compare-and-exchange at `addr`.
    ///
    /// The future must own any data it needs, since it is polled after the
    /// device lock is released. Implementations must make the comparison and
    /// write atomic with respect to all accesses to the same backing memory and
    /// provide sequentially consistent ordering, as for guest memory atomics.
    fn compare_exchange(
        &mut self,
        addr: u64,
        current: &[u8],
        new: &[u8],
    ) -> MmioCompareExchangeFuture;
}

/// Implemented by devices which use MMIO intercepts.
///
/// NOTE: Devices that wish to register objects (e.g: files, shared memory file
/// descriptors, etc...) into guest memory directly (thereby bypassing the need
/// for a MMIO intercepts) should obtain a reference to a
/// `guestmem::MemoryMapper` object.
pub trait MmioIntercept: ChipsetDevice {
    /// Dispatch an MMIO read to the device with the given address.
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult;
    /// Dispatch an MMIO write to the device with the given address.
    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult;

    /// Returns memory-like atomic operations, if supported.
    ///
    /// Support must remain stable for the device's lifetime, including across
    /// reset. For devices with mixed register and memory ranges, use
    /// `Ok(None)` from [`MmioCompareExchange::compare_exchange`] for addresses
    /// that require legacy device-MMIO behavior.
    fn supports_compare_exchange(&mut self) -> Option<&mut dyn MmioCompareExchange> {
        None
    }

    /// Report a set of static static mmio regions (region_name, gpa_range) that
    /// cannot be remapped at runtime and are always registered.
    ///
    /// _Note:_ This is a convenience method that makes it easy for simple
    /// devices to declare some fixed IO regions without having to do through
    /// the rigamarole of obtaining a reference to an instance of
    /// [`RegisterMmioIntercept`] + manually registering fixed ranges as part of
    /// device init.
    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u64>)] {
        &[]
    }
}

io_region!(RegisterMmioIntercept, ControlMmioIntercept, u64);

/// A zero sized type that has a no-op `impl` of [`RegisterMmioIntercept`].
///
/// As the name suggests, this type should be used when a [`ChipsetDevice`] is
/// hosted outside of a traditional "chipset" context, where some external code
/// is responsible for managing the device's MMIO intercepts.
///
/// e.g: A ChipsetDevice that supports PCI could potentially be reused inside a
/// wrapper type that intercepts PCI config space reads/writes, and takes care
/// of BAR management for the device.
///
/// That said, if you find yourself reaching for this type (outside the context
/// of a test), you're probably doing something wrong. Consider implementing a
/// proper chipset to host the device on instead.
pub struct ExternallyManagedMmioIntercepts;

impl RegisterMmioIntercept for ExternallyManagedMmioIntercepts {
    fn new_io_region(&mut self, _region_name: &str, _len: u64) -> Box<dyn ControlMmioIntercept> {
        Box::new(())
    }
}

impl ControlMmioIntercept for () {
    fn region_name(&self) -> &str {
        "(noop)"
    }
    fn map(&mut self, _addr: u64) {}
    fn unmap(&mut self) {}
    fn addr(&self) -> Option<u64> {
        None
    }
    fn len(&self) -> u64 {
        0
    }
    fn offset_of(&self, _addr: u64) -> Option<u64> {
        None
    }
}
