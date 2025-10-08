// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Trait to model host-assisted MSI/MSI-X configuration (when using VPCI).

#![forbid(unsafe_code)]
#![expect(missing_docs)]

use async_trait::async_trait;
use inspect::Inspect;
use std::future::Future;
use std::sync::Arc;
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
pub trait MapVpciInterrupt: Send + Sync {
    fn register_interrupt(
        &self,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> impl Future<Output = Result<MsiAddressData, RegisterInterruptError>> + Send;

    fn unregister_interrupt(&self, address: u64, data: u32) -> impl Future<Output = ()> + Send;
}

#[async_trait]
trait DynMapVpciInterrupt: Send + Sync {
    async fn register_interrupt(
        &self,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<MsiAddressData, RegisterInterruptError>;

    async fn unregister_interrupt(&self, address: u64, data: u32);
}

#[async_trait]
impl<T: MapVpciInterrupt> DynMapVpciInterrupt for T {
    async fn register_interrupt(
        &self,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<MsiAddressData, RegisterInterruptError> {
        self.register_interrupt(vector_count, params).await
    }

    async fn unregister_interrupt(&self, address: u64, data: u32) {
        self.unregister_interrupt(address, data).await
    }
}

/// A type-erased [`MapVpciInterrupt`] trait object.
#[derive(Clone)]
pub struct VpciInterruptMapper(Arc<dyn DynMapVpciInterrupt>);

impl VpciInterruptMapper {
    /// Creates a new instance from `mapper`.
    pub fn new<T: 'static + MapVpciInterrupt>(mapper: Arc<T>) -> Self {
        Self(mapper)
    }
}

impl MapVpciInterrupt for VpciInterruptMapper {
    async fn register_interrupt(
        &self,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<MsiAddressData, RegisterInterruptError> {
        self.0.register_interrupt(vector_count, params).await
    }

    async fn unregister_interrupt(&self, address: u64, data: u32) {
        self.0.unregister_interrupt(address, data).await
    }
}

#[derive(Debug, Error)]
#[error("failed to register an interrupt")]
pub struct RegisterInterruptError(#[source] Box<dyn std::error::Error + Send + Sync>);

impl RegisterInterruptError {
    pub fn new(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self(err.into())
    }
}
