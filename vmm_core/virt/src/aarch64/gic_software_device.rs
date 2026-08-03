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
            self.irqcon.pulse_spi_irq(data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    #[derive(Default)]
    struct TestGic {
        level: Mutex<Vec<(u32, bool)>>,
        pulses: Mutex<Vec<u32>>,
    }

    impl ControlGic for TestGic {
        fn set_spi_irq(&self, irq_id: u32, high: bool) {
            self.level.lock().push((irq_id, high));
        }

        fn pulse_spi_irq(&self, irq_id: u32) {
            self.pulses.lock().push(irq_id);
        }
    }

    #[test]
    fn msi_delivery_uses_spi_pulse_semantics() {
        let gic = Arc::new(TestGic::default());
        let device = GicSoftwareDevice::new(gic.clone());

        device.signal_msi(None, 0, 40);

        assert_eq!(*gic.pulses.lock(), [40]);
        assert!(gic.level.lock().is_empty());
    }
}
