// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared PCIe bus range tracking.
//!
//! An [`AssignedBusRange`] holds the segment-local bus range
//! `(secondary_bus, subordinate_bus)` assigned to the PCIe port that owns a
//! device. It is updated automatically by
//! [`ConfigSpaceType1Emulator`](crate::cfg_space_emu::ConfigSpaceType1Emulator)
//! when the guest writes bus number registers, and on restore/reset.
//!
//! Consumers (ITS wrappers, SMMU) compose a full device identity from the
//! bus range plus the device's BDF. The segment number is not included
//! here — it is a static property of the root complex and is held
//! separately by the consumer.

use std::sync::Arc;
use std::sync::atomic::AtomicU16;
use std::sync::atomic::Ordering;

/// Segment-local bus range assigned to a PCIe downstream port.
///
/// Stores a packed `(secondary_bus, subordinate_bus)` as an atomic u16,
/// updated when the PCIe port's bus numbers change. The segment number
/// is not included here — it is a static property of the root complex
/// and is held separately by the consumer (e.g., ITS wrappers).
///
/// Clone is cheap (just an `Arc` bump).
#[derive(Clone, Debug)]
pub struct AssignedBusRange(Arc<AtomicU16>);

impl Default for AssignedBusRange {
    fn default() -> Self {
        Self::new()
    }
}

impl AssignedBusRange {
    /// Creates a new bus range initialized to zero.
    pub fn new() -> Self {
        Self(Arc::new(AtomicU16::new(0)))
    }

    /// Updates the bus range for the downstream port.
    pub fn set_bus_range(&self, secondary: u8, subordinate: u8) {
        self.0.store(
            (secondary as u16) << 8 | subordinate as u16,
            Ordering::Relaxed,
        );
    }

    /// Returns the current `(secondary_bus, subordinate_bus)`.
    pub fn bus_range(&self) -> (u8, u8) {
        let v = self.0.load(Ordering::Relaxed);
        ((v >> 8) as u8, v as u8)
    }

    /// Composes an ITS device ID from the current bus range, segment, and
    /// an optional per-device BDF override.
    ///
    /// Returns `None` if the secondary bus has not been assigned yet (still 0).
    /// When `devid` is `None`, defaults to `(secondary_bus, dev 0, fn 0)`.
    /// Logs a rate-limited warning and returns `None` if the BDF's bus
    /// number falls outside the port's assigned range.
    pub fn compose_its_devid(&self, segment: u16, devid: Option<u32>) -> Option<u32> {
        let (secondary, subordinate) = self.bus_range();
        if secondary == 0 {
            return None;
        }
        let bdf = devid.unwrap_or((secondary as u32) << 8);
        let bus = (bdf >> 8) as u8;
        if bus < secondary || bus > subordinate {
            tracelimit::warn_ratelimited!(bus, secondary, subordinate, "BDF out of port bus range");
            return None;
        }
        Some((segment as u32) << 16 | (bdf & 0xFFFF))
    }
}
