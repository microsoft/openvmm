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

/// The current routing of a registered interrupt, as returned by
/// [`MapVpciInterrupt::interrupt_state`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VpciInterruptState {
    /// The base vector.
    pub vector: u32,
    /// Whether the interrupt is delivered to all target processors.
    pub multicast: bool,
    /// The target processors, as VP indexes.
    pub target_processors: Vec<u32>,
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

    /// Returns the current routing of the interrupt registered as `msi`.
    ///
    /// This includes any retargeting the guest did after registration, so it
    /// can differ from the parameters passed to
    /// [`register_interrupt`](Self::register_interrupt).
    fn interrupt_state(
        &self,
        msi: MsiAddressData,
    ) -> impl Future<Output = Result<VpciInterruptState, RegisterInterruptError>> + Send {
        let _ = msi;
        async { Err(RegisterInterruptError::new("save not supported")) }
    }

    /// Registers an interrupt that must be addressed by exactly `msi`, as
    /// returned by an earlier [`register_interrupt`](Self::register_interrupt)
    /// that has since been lost, for example by restoring saved state.
    ///
    /// The guest has already programmed `msi` into the device, so the mapper
    /// must fail rather than pick a different address/data pair.
    fn restore_interrupt(
        &self,
        msi: MsiAddressData,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> impl Future<Output = Result<(), RegisterInterruptError>> + Send {
        let _ = (msi, vector_count, params);
        async { Err(RegisterInterruptError::new("restore not supported")) }
    }

    /// Returns true if [`interrupt_state`](Self::interrupt_state) and
    /// [`restore_interrupt`](Self::restore_interrupt) are implemented.
    fn supports_save_restore(&self) -> bool {
        false
    }
}

#[async_trait]
trait DynMapVpciInterrupt: Send + Sync {
    async fn register_interrupt(
        &self,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<MsiAddressData, RegisterInterruptError>;

    async fn unregister_interrupt(&self, address: u64, data: u32);

    async fn interrupt_state(
        &self,
        msi: MsiAddressData,
    ) -> Result<VpciInterruptState, RegisterInterruptError>;

    async fn restore_interrupt(
        &self,
        msi: MsiAddressData,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<(), RegisterInterruptError>;

    fn supports_save_restore(&self) -> bool;
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

    async fn interrupt_state(
        &self,
        msi: MsiAddressData,
    ) -> Result<VpciInterruptState, RegisterInterruptError> {
        self.interrupt_state(msi).await
    }

    async fn restore_interrupt(
        &self,
        msi: MsiAddressData,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<(), RegisterInterruptError> {
        self.restore_interrupt(msi, vector_count, params).await
    }

    fn supports_save_restore(&self) -> bool {
        self.supports_save_restore()
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

    async fn interrupt_state(
        &self,
        msi: MsiAddressData,
    ) -> Result<VpciInterruptState, RegisterInterruptError> {
        self.0.interrupt_state(msi).await
    }

    async fn restore_interrupt(
        &self,
        msi: MsiAddressData,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<(), RegisterInterruptError> {
        self.0.restore_interrupt(msi, vector_count, params).await
    }

    fn supports_save_restore(&self) -> bool {
        self.0.supports_save_restore()
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

#[cfg(test)]
mod tests {
    use super::MapVpciInterrupt;
    use super::MsiAddressData;
    use super::RegisterInterruptError;
    use super::VpciInterruptMapper;
    use super::VpciInterruptParameters;
    use pal_async::async_test;
    use std::sync::Arc;

    /// A mapper that implements only the required methods.
    struct NoSaveMapper;

    impl MapVpciInterrupt for NoSaveMapper {
        async fn register_interrupt(
            &self,
            _vector_count: u32,
            _params: &VpciInterruptParameters<'_>,
        ) -> Result<MsiAddressData, RegisterInterruptError> {
            Ok(MsiAddressData {
                address: 0xfee00000,
                data: 0,
            })
        }

        async fn unregister_interrupt(&self, _address: u64, _data: u32) {}
    }

    #[async_test]
    async fn default_mapper_does_not_support_save_restore() {
        let mapper = VpciInterruptMapper::new(Arc::new(NoSaveMapper));
        let params = VpciInterruptParameters {
            vector: 0x30,
            multicast: false,
            target_processors: &[0],
        };
        let msi = mapper.register_interrupt(1, &params).await.unwrap();
        assert!(!mapper.supports_save_restore());
        assert!(mapper.interrupt_state(msi).await.is_err());
        assert!(mapper.restore_interrupt(msi, 1, &params).await.is_err());
    }
}
