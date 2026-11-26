// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VPCI device implementation for GIC-based VMs.

use crate::irqcon::ControlGic;
use pci_core::msi::SignalMsi;
use std::ops::Range;
use std::sync::Arc;
use thiserror::Error;
use vmcore::vpci_msi::MapVpciInterrupt;
use vmcore::vpci_msi::MsiAddressData;
use vmcore::vpci_msi::RegisterInterruptError;

pub struct GicSoftwareDevice {
    irqcon: Arc<dyn ControlGic>,
}

impl GicSoftwareDevice {
    pub fn new(irqcon: Arc<dyn ControlGic>) -> Self {
        Self { irqcon }
    }
}

#[derive(Debug, Error)]
enum GicInterruptError {
    #[error("invalid vector count {0}")]
    InvalidVectorCount(u32),
    #[error("invalid {count} vectors at {start}")]
    InvalidVector { start: u32, count: u32 },
}

const SPI_RANGE: Range<u32> = 32..1020;

impl MapVpciInterrupt for GicSoftwareDevice {
    async fn register_interrupt(
        &self,
        vector_count: u32,
        params: &vmcore::vpci_msi::VpciInterruptParameters<'_>,
    ) -> Result<MsiAddressData, RegisterInterruptError> {
        if !vector_count.is_power_of_two() {
            return Err(RegisterInterruptError::new(
                GicInterruptError::InvalidVectorCount(vector_count),
            ));
        }
        if params.vector < SPI_RANGE.start
            || params.vector.saturating_add(vector_count) > SPI_RANGE.end
        {
            return Err(RegisterInterruptError::new(
                GicInterruptError::InvalidVector {
                    start: params.vector,
                    count: vector_count,
                },
            ));
        }
        Ok(MsiAddressData {
            address: 0,
            data: params.vector,
        })
    }

    async fn unregister_interrupt(&self, address: u64, data: u32) {
        let _ = (address, data);
    }
}

impl SignalMsi for GicSoftwareDevice {
    fn signal_msi(&self, _devid: Option<u32>, _address: u64, data: u32) {
        if SPI_RANGE.contains(&data) {
            self.irqcon.set_spi_irq(data, true);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use pal_async::async_test;
    use parking_lot::Mutex;
    use test_with_tracing::test;
    use vmcore::vpci_msi::VpciInterruptParameters;

    /// Records every SPI assertion.
    #[derive(Default)]
    pub(crate) struct RecordGic(pub Mutex<Vec<(u32, bool)>>);

    impl ControlGic for RecordGic {
        fn set_spi_irq(&self, irq_id: u32, high: bool) {
            self.0.lock().push((irq_id, high));
        }
    }

    fn params(vector: u32) -> VpciInterruptParameters<'static> {
        VpciInterruptParameters {
            vector,
            multicast: false,
            target_processors: &[0],
        }
    }

    // Test: Register interrupt blocks that fit in the SPI range (IDs
    //       32-1019), blocks whose size isn't a power of two, and blocks that
    //       start below or run past the SPI range.
    // Expected: A valid block returns its first SPI as the MSI data; the
    //       others are rejected.
    #[async_test]
    async fn registration_requires_power_of_two_block_of_spis() {
        let device = GicSoftwareDevice::new(Arc::new(RecordGic::default()));

        let ok = device.register_interrupt(4, &params(64)).await.unwrap();
        assert_eq!((ok.address, ok.data), (0, 64));
        assert!(device.register_interrupt(1, &params(1019)).await.is_ok());

        for (count, vector) in [(0, 64), (3, 64), (1, 31), (2, 1019), (1, u32::MAX)] {
            assert!(
                device
                    .register_interrupt(count, &params(vector))
                    .await
                    .is_err(),
                "{count} vectors at {vector}"
            );
        }
        device.unregister_interrupt(0, 64).await;
    }

    // Test: Signal MSIs whose data is an SPI, a PPI (below 32), and an ID
    //       past the SPI range.
    // Expected: Only the SPI is asserted on the GIC.
    #[test]
    fn signal_asserts_only_spis() {
        let gic = Arc::new(RecordGic::default());
        let device = GicSoftwareDevice::new(gic.clone());

        for data in [64, 31, 1020] {
            device.signal_msi(None, 0, data);
        }

        assert_eq!(*gic.0.lock(), [(64, true)]);
    }
}
