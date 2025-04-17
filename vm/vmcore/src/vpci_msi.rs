// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

//! Trait to model host-assisted MSI/MSI-X configuration (when using VPCI).

use inspect::Inspect;
use thiserror::Error;

/// An MSI address/data pair.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Inspect)]
pub struct MsiAddressData {
    /// The MSI address.
    #[inspect(hex)]
    pub address: u64,
    /// The data payload.
    #[inspect(hex)]
    pub data: u32,
}

pub struct VpciInterruptParameters<'a> {
    pub vector: u32,
    pub multicast: bool,
    pub target_processors: &'a [u32],
}

/// Trait to model host-assisted MSI/MSI-X configuration when using VPCI.
///
/// The VPCI model allows the guest to register interrupts with the host, and
/// have the host return an MSI (address, data) value to use to program the
/// MSI/MSI-X configuration within the device.
pub trait VpciInterruptMapper: Send + Sync {
    fn register_interrupt(
        &self,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<MsiAddressData, RegisterInterruptError>;

    fn unregister_interrupt(&self, address: u64, data: u32);
}

#[derive(Debug, Error)]
#[error("failed to register an interrupt")]
pub struct RegisterInterruptError(#[source] Box<dyn std::error::Error + Send + Sync>);

impl RegisterInterruptError {
    pub fn new(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self(err.into())
    }
}
