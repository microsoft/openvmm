// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resources for a device that ignores accesses to specified regions.

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use std::ops::RangeInclusive;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vm_resource::ResourceId;

/// A handle to a device that ignores accesses to specified regions.
#[derive(MeshPayload, Default)]
pub struct MissingDevHandle {
    /// The port I/O regions ignored by this device, `(name, start, end_inclusive)`.
    pub pio: Vec<(String, u16, u16)>,
    /// The MMIO regions ignored by this device, `(name, start, end_inclusive)`.
    pub mmio: Vec<(String, u64, u64)>,
}

impl ResourceId<ChipsetDeviceHandleKind> for MissingDevHandle {
    const ID: &'static str = "missing-dev";
}

impl MissingDevHandle {
    /// Create an empty instance.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a PIO region to the device.
    pub fn claim_pio(mut self, region_name: impl Into<String>, range: RangeInclusive<u16>) -> Self {
        self.pio
            .push((region_name.into(), *range.start(), *range.end()));
        self
    }

    /// Add an MMIO region to the device.
    pub fn claim_mmio(
        mut self,
        region_name: impl Into<String>,
        range: RangeInclusive<u64>,
    ) -> Self {
        self.mmio
            .push((region_name.into(), *range.start(), *range.end()));
        self
    }
}
