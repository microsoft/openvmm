// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VPCI device implementation for GIC-based VMs.

use crate::irqcon::ControlGic;
use parking_lot::Mutex;
use pci_core::msi::SignalMsi;
use std::collections::BTreeMap;
use std::collections::btree_map;
use std::ops::Range;
use std::sync::Arc;
use thiserror::Error;
use vmcore::vpci_msi::MapVpciInterrupt;
use vmcore::vpci_msi::MsiAddressData;
use vmcore::vpci_msi::RegisterInterruptError;
use vmcore::vpci_msi::VpciInterruptParameters;
use vmcore::vpci_msi::VpciInterruptState;

pub struct GicSoftwareDevice {
    irqcon: Arc<dyn ControlGic>,
    /// The registered interrupts, keyed by base SPI.
    interrupts: Mutex<BTreeMap<u32, GicInterrupt>>,
}

/// A registered interrupt.
///
/// The GIC routes an SPI by its distributor configuration, not by the MSI, so
/// the target processors have no effect on delivery. They are kept so that
/// [`MapVpciInterrupt::interrupt_state`] can report them.
struct GicInterrupt {
    multicast: bool,
    target_processors: Vec<u32>,
    /// The number of live registrations of this SPI.
    registrations: usize,
}

impl GicSoftwareDevice {
    pub fn new(irqcon: Arc<dyn ControlGic>) -> Self {
        Self {
            irqcon,
            interrupts: Default::default(),
        }
    }
}

#[derive(Debug, Error)]
enum GicInterruptError {
    #[error("invalid vector count {0}")]
    VectorCount(u32),
    #[error("invalid {count} vectors at {start}")]
    Vector { start: u32, count: u32 },
    #[error("invalid interrupt address {address:#x} or data {data:#x}")]
    Address { address: u64, data: u32 },
}

const SPI_RANGE: Range<u32> = 32..1020;

/// Validates a request for `vector_count` SPIs starting at `params.vector`.
fn new_interrupt(
    vector_count: u32,
    params: &VpciInterruptParameters<'_>,
) -> Result<GicInterrupt, GicInterruptError> {
    if !vector_count.is_power_of_two() {
        return Err(GicInterruptError::VectorCount(vector_count));
    }
    if params.vector < SPI_RANGE.start || params.vector.saturating_add(vector_count) > SPI_RANGE.end
    {
        return Err(GicInterruptError::Vector {
            start: params.vector,
            count: vector_count,
        });
    }
    Ok(GicInterrupt {
        multicast: params.multicast,
        target_processors: params.target_processors.to_vec(),
        registrations: 1,
    })
}

impl GicSoftwareDevice {
    /// Records a registration of `vector`. A later registration of the same
    /// SPI replaces the reported parameters and adds to the count.
    fn add(&self, vector: u32, mut interrupt: GicInterrupt) {
        let mut interrupts = self.interrupts.lock();
        if let Some(old) = interrupts.get(&vector) {
            interrupt.registrations += old.registrations;
        }
        interrupts.insert(vector, interrupt);
    }
}

impl MapVpciInterrupt for GicSoftwareDevice {
    async fn register_interrupt(
        &self,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<MsiAddressData, RegisterInterruptError> {
        let interrupt = new_interrupt(vector_count, params).map_err(RegisterInterruptError::new)?;
        self.add(params.vector, interrupt);
        Ok(MsiAddressData {
            address: 0,
            data: params.vector,
        })
    }

    async fn unregister_interrupt(&self, address: u64, data: u32) {
        let _ = address;
        if let btree_map::Entry::Occupied(mut entry) = self.interrupts.lock().entry(data) {
            entry.get_mut().registrations -= 1;
            if entry.get().registrations == 0 {
                entry.remove();
            }
        }
    }

    async fn interrupt_state(
        &self,
        msi: MsiAddressData,
    ) -> Result<VpciInterruptState, RegisterInterruptError> {
        let interrupts = self.interrupts.lock();
        let interrupt = (msi.address == 0)
            .then(|| interrupts.get(&msi.data))
            .flatten()
            .ok_or_else(|| RegisterInterruptError::new("interrupt not registered"))?;
        Ok(VpciInterruptState {
            vector: msi.data,
            multicast: interrupt.multicast,
            target_processors: interrupt.target_processors.clone(),
        })
    }

    async fn restore_interrupt(
        &self,
        msi: MsiAddressData,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<(), RegisterInterruptError> {
        let interrupt = new_interrupt(vector_count, params).map_err(RegisterInterruptError::new)?;
        if msi.address != 0 || msi.data != params.vector {
            return Err(RegisterInterruptError::new(GicInterruptError::Address {
                address: msi.address,
                data: msi.data,
            }));
        }
        self.add(msi.data, interrupt);
        Ok(())
    }

    fn supports_save_restore(&self) -> bool {
        true
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
mod tests {
    use super::GicSoftwareDevice;
    use crate::irqcon::ControlGic;
    use pal_async::async_test;
    use parking_lot::Mutex;
    use pci_core::msi::SignalMsi;
    use std::sync::Arc;
    use test_with_tracing::test;
    use vmcore::vpci_msi::MapVpciInterrupt;
    use vmcore::vpci_msi::MsiAddressData;
    use vmcore::vpci_msi::VpciInterruptParameters;
    use vmcore::vpci_msi::VpciInterruptState;

    /// Records every SPI asserted.
    #[derive(Default)]
    struct SpiRecorder(Mutex<Vec<u32>>);

    impl ControlGic for SpiRecorder {
        fn set_spi_irq(&self, irq_id: u32, high: bool) {
            assert!(high);
            self.0.lock().push(irq_id);
        }
    }

    fn new_device() -> (GicSoftwareDevice, Arc<SpiRecorder>) {
        let gic = Arc::new(SpiRecorder::default());
        (GicSoftwareDevice::new(gic.clone()), gic)
    }

    fn params(vector: u32) -> VpciInterruptParameters<'static> {
        VpciInterruptParameters {
            vector,
            multicast: true,
            target_processors: &[2, 1],
        }
    }

    fn spi(vector: u32) -> MsiAddressData {
        MsiAddressData {
            address: 0,
            data: vector,
        }
    }

    #[async_test]
    async fn gic_supports_save_restore() {
        assert!(new_device().0.supports_save_restore());
    }

    #[async_test]
    async fn gic_interrupt_state_round_trip() {
        let (old, _) = new_device();
        let msi = old.register_interrupt(2, &params(40)).await.unwrap();
        assert_eq!(msi, spi(40));
        let state = old.interrupt_state(msi).await.unwrap();
        assert_eq!(
            state,
            VpciInterruptState {
                vector: 40,
                multicast: true,
                target_processors: vec![2, 1],
            }
        );

        let (new, gic) = new_device();
        new.restore_interrupt(
            msi,
            2,
            &VpciInterruptParameters {
                vector: state.vector,
                multicast: state.multicast,
                target_processors: &state.target_processors,
            },
        )
        .await
        .unwrap();
        assert_eq!(new.interrupt_state(msi).await.unwrap(), state);
        assert!(
            new.interrupt_state(MsiAddressData {
                address: 0x1000,
                ..msi
            })
            .await
            .is_err()
        );
        new.signal_msi(None, 0, 41);
        assert_eq!(*gic.0.lock(), [41]);

        new.unregister_interrupt(msi.address, msi.data).await;
        assert!(new.interrupt_state(msi).await.is_err());
    }

    #[async_test]
    async fn gic_register_again_replaces() {
        let (dev, _) = new_device();
        dev.register_interrupt(1, &params(40)).await.unwrap();
        let again = VpciInterruptParameters {
            vector: 40,
            multicast: false,
            target_processors: &[3],
        };
        dev.register_interrupt(1, &again).await.unwrap();
        assert_eq!(
            dev.interrupt_state(spi(40)).await.unwrap(),
            VpciInterruptState {
                vector: 40,
                multicast: false,
                target_processors: vec![3],
            }
        );

        // Each registration holds the SPI until it is unregistered.
        dev.unregister_interrupt(0, 40).await;
        assert!(dev.interrupt_state(spi(40)).await.is_ok());
        dev.unregister_interrupt(0, 40).await;
        assert!(dev.interrupt_state(spi(40)).await.is_err());
    }

    #[async_test]
    async fn gic_duplicate_registrations_restore() {
        let (old, _) = new_device();
        let msi = old.register_interrupt(1, &params(40)).await.unwrap();
        assert_eq!(old.register_interrupt(1, &params(40)).await.unwrap(), msi);
        let state = old.interrupt_state(msi).await.unwrap();
        let restored = VpciInterruptParameters {
            vector: state.vector,
            multicast: state.multicast,
            target_processors: &state.target_processors,
        };

        let (new, _) = new_device();
        new.restore_interrupt(msi, 1, &restored).await.unwrap();
        new.restore_interrupt(msi, 1, &restored).await.unwrap();
        new.unregister_interrupt(msi.address, msi.data).await;
        assert_eq!(new.interrupt_state(msi).await.unwrap(), state);
        new.unregister_interrupt(msi.address, msi.data).await;
        assert!(new.interrupt_state(msi).await.is_err());
    }

    #[async_test]
    async fn gic_restore_validates() {
        let (dev, _) = new_device();
        dev.register_interrupt(1, &params(40)).await.unwrap();

        let refused = [
            // Address must be zero.
            (
                MsiAddressData {
                    address: 0x1000,
                    data: 50,
                },
                1,
                params(50),
            ),
            // Data must be the vector.
            (spi(51), 1, params(50)),
            // Outside the SPI range.
            (spi(31), 1, params(31)),
            (spi(1019), 2, params(1019)),
            // Not a power of two.
            (spi(52), 3, params(52)),
            (spi(52), 0, params(52)),
        ];
        for (msi, vector_count, params) in refused {
            assert!(
                dev.restore_interrupt(msi, vector_count, &params)
                    .await
                    .is_err(),
                "{msi:x?} {vector_count}"
            );
        }

        // What register_interrupt accepts, restore_interrupt accepts.
        dev.restore_interrupt(spi(33), 2, &params(33))
            .await
            .unwrap();
        dev.restore_interrupt(spi(1018), 2, &params(1018))
            .await
            .unwrap();
        assert!(dev.interrupt_state(spi(50)).await.is_err());
        assert!(dev.interrupt_state(spi(52)).await.is_err());
    }
}
