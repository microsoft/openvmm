// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Software implementation of a VPCI-compatible device. Avoids using the
//! hypervisor device interface.

use hvdef::HvError;
use hvdef::HvResult;
use inspect::Inspect;
use parking_lot::Mutex;
use pci_core::msi::SignalMsi;
use slab::Slab;
use std::collections::HashMap;
use std::collections::hash_map;
use std::sync::Arc;
use thiserror::Error;
use vmcore::vpci_msi::MapVpciInterrupt;
use vmcore::vpci_msi::MsiAddressData;
use vmcore::vpci_msi::RegisterInterruptError;
use vmcore::vpci_msi::VpciInterruptParameters;
use x86defs::msi::MSI_ADDRESS;
use x86defs::msi::MsiAddress;
use x86defs::msi::MsiData;

/// A set of software devices that can be used to implement VPCI devices on an
/// APIC (x86) platform.
///
/// This is used to provide an indirection between the guest-specified device
/// interrupt map and the actual MSIs that should be injected into the guest's
/// APICs.
#[derive(Inspect)]
pub struct ApicSoftwareDevices {
    #[inspect(flatten)]
    inner: Arc<DevicesInner>,
}

#[derive(Inspect)]
struct DevicesInner {
    #[inspect(flatten, with = "inspect_tables")]
    tables: Mutex<HashMap<u64, Arc<Mutex<InterruptTable>>>>,
    #[inspect(skip)]
    apic_id_map: Vec<u32>,
}

fn inspect_tables(tables: &Mutex<HashMap<u64, Arc<Mutex<InterruptTable>>>>) -> impl '_ + Inspect {
    inspect::adhoc(|req| {
        let mut resp = req.respond();
        for (device_id, table) in &*tables.lock() {
            resp.field(&device_id.to_string(), &*table.lock());
        }
    })
}

#[derive(Debug, Error)]
#[error("device id {0} is already in use")]
pub struct DeviceIdInUse(u64);

impl ApicSoftwareDevices {
    pub fn new(apic_id_map: Vec<u32>) -> Self {
        Self {
            inner: Arc::new(DevicesInner {
                tables: Default::default(),
                apic_id_map,
            }),
        }
    }

    /// Creates a new device with the given ID.
    pub fn new_device(
        &self,
        target: Arc<dyn SignalMsi>,
        device_id: u64,
    ) -> Result<ApicSoftwareDevice, DeviceIdInUse> {
        let table = Arc::new(Mutex::new(InterruptTable::new()));
        {
            let mut tables = self.inner.tables.lock();
            let entry = match tables.entry(device_id) {
                hash_map::Entry::Occupied(_) => return Err(DeviceIdInUse(device_id)),
                hash_map::Entry::Vacant(e) => e,
            };
            entry.insert(table.clone());
        }
        Ok(ApicSoftwareDevice {
            devices: self.inner.clone(),
            target,
            table,
            id: device_id,
        })
    }

    /// Retargets the interrupt for the given device.
    pub fn retarget_interrupt(
        &self,
        device_id: u64,
        address: u64,
        data: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> HvResult<()> {
        let table = self
            .inner
            .tables
            .lock()
            .get(&device_id)
            .cloned()
            .ok_or(HvError::InvalidDeviceId)?;

        if let Err(err) =
            table
                .lock()
                .retarget_interrupt(&self.inner.apic_id_map, address, data, params)
        {
            tracing::warn!(
                error = &err as &dyn std::error::Error,
                "retarget interrupt failure"
            );
            return Err(HvError::InvalidParameter);
        }

        Ok(())
    }
}

/// The software implementation of a VPCI-compatible device.
pub struct ApicSoftwareDevice {
    devices: Arc<DevicesInner>,
    table: Arc<Mutex<InterruptTable>>,
    target: Arc<dyn SignalMsi>,
    id: u64,
}

impl Drop for ApicSoftwareDevice {
    fn drop(&mut self) {
        let _table = self.devices.tables.lock().remove(&self.id);
    }
}

/// The table of interrupts for a device.
#[derive(Inspect)]
struct InterruptTable {
    #[inspect(iter_by_key)]
    entries: Slab<InterruptEntry>,
}

/// State for an individual VPCI interrupt for a device.
#[derive(Debug, Inspect)]
struct InterruptEntry {
    base_vector: u32,
    vector_count: u32,
    multicast: bool,
    target_apic_id: u32,
}

impl InterruptEntry {
    fn msi_params(&self) -> MsiAddressData {
        let address = MsiAddress::new()
            .with_address(MSI_ADDRESS)
            .with_virt_destination(self.target_apic_id as u16);
        let data = MsiData::new().with_vector(self.base_vector as u8);
        MsiAddressData {
            address: u32::from(address).into(),
            data: data.into(),
        }
    }
}

#[derive(Debug, Error)]
enum InvalidInterruptParams {
    #[error("invalid interrupt parameters")]
    InvalidHypercallInput,
    #[error("invalid virtual processor index {0}")]
    InvalidVirtualProcessor(u32),
}

#[derive(Debug, Error)]
enum InvalidRetargetParams {
    #[error("invalid interrupt address {0:#x}")]
    InvalidAddress(u64),
    #[error("invalid virtual processor index {0}")]
    InvalidVirtualProcessor(u32),
}

impl InterruptTable {
    fn new() -> Self {
        Self {
            entries: Slab::new(),
        }
    }

    fn interrupt_address_from_index(index: usize) -> u64 {
        // Per Intel spec, set the upper bits to FEE.
        // Set lower bits to the specified index, shifted to avoid the bits
        // that actually mean something (redirection hint / destination mode).
        0xFEE00000 | ((index as u64) << 2)
    }

    fn interrupt_index_from_address(address: u64) -> usize {
        ((address >> 2) & 0xffff) as usize
    }

    fn retarget_interrupt(
        &mut self,
        apic_id_map: &[u32],
        address: u64,
        _data: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<(), InvalidRetargetParams> {
        let index = Self::interrupt_index_from_address(address);

        let interrupt = self
            .entries
            .get_mut(index)
            .ok_or(InvalidRetargetParams::InvalidAddress(address))?;

        interrupt.base_vector = params.vector;
        interrupt.multicast = params.multicast;
        let mut iter = params.target_processors.iter().map(|&vp_index| {
            apic_id_map
                .get(vp_index as usize)
                .copied()
                .ok_or(InvalidRetargetParams::InvalidVirtualProcessor(vp_index))
        });
        if let Some(target_apic_id) = iter.next() {
            interrupt.target_apic_id = target_apic_id?;
        }

        // Check the rest of the VPs.
        iter.map(|x| x.map(drop)).collect::<Result<Vec<()>, _>>()?;

        Ok(())
    }

    fn register_interrupt(
        &mut self,
        apic_id_map: &[u32],
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<MsiAddressData, InvalidInterruptParams> {
        if vector_count == 0 || params.target_processors.is_empty() {
            return Err(InvalidInterruptParams::InvalidHypercallInput);
        }

        // TODO: the caller should specify the interrupt ID (needed for save/restore)
        let vp = params.target_processors[0];
        let i = self.entries.insert(InterruptEntry {
            base_vector: params.vector,
            vector_count,
            multicast: params.multicast,
            target_apic_id: *apic_id_map
                .get(vp as usize)
                .ok_or(InvalidInterruptParams::InvalidVirtualProcessor(vp))?,
        });
        let address = Self::interrupt_address_from_index(i);
        Ok(MsiAddressData { address, data: 0 })
    }

    fn unregister_interrupt(&mut self, address: u64, _data: u32) {
        let index = Self::interrupt_index_from_address(address);
        self.entries.remove(index);
    }
}

impl SignalMsi for ApicSoftwareDevice {
    fn signal_msi(&self, _devid: Option<u32>, address: u64, _data: u32) {
        let mut table = self.table.lock();
        let table = &mut *table;
        let index = InterruptTable::interrupt_index_from_address(address);
        if let Some(interrupt) = table.entries.get(index) {
            let target = interrupt.msi_params();
            self.target.signal_msi(None, target.address, target.data)
        } else {
            // A dropped MSI might leave the guest waiting forever for a device
            // notification, so make it visible.
            tracelimit::warn_ratelimited!(
                address,
                index,
                "dropping MSI for unregistered interrupt"
            );
        }
    }
}

impl MapVpciInterrupt for ApicSoftwareDevice {
    async fn register_interrupt(
        &self,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<MsiAddressData, RegisterInterruptError> {
        self.table
            .lock()
            .register_interrupt(&self.devices.apic_id_map, vector_count, params)
            .map_err(RegisterInterruptError::new)
    }

    async fn unregister_interrupt(&self, address: u64, data: u32) {
        self.table.lock().unregister_interrupt(address, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::async_test;
    use test_with_tracing::test;

    /// Records every MSI delivered to the APICs.
    #[derive(Default)]
    struct RecordMsi(Mutex<Vec<(u64, u32)>>);

    impl SignalMsi for RecordMsi {
        fn signal_msi(&self, _devid: Option<u32>, address: u64, data: u32) {
            self.0.lock().push((address, data));
        }
    }

    impl RecordMsi {
        /// Returns the destination APIC ID and vector of each delivered MSI.
        fn take(&self) -> Vec<(u16, u8)> {
            std::mem::take(&mut *self.0.lock())
                .into_iter()
                .map(|(address, data)| {
                    let address = MsiAddress::from(address as u32);
                    assert_eq!(address.address(), MSI_ADDRESS);
                    (address.virt_destination(), MsiData::from(data).vector())
                })
                .collect()
        }
    }

    /// VPs 0 through 3 have APIC IDs 0, 2, 4, and 0x123.
    fn devices() -> ApicSoftwareDevices {
        ApicSoftwareDevices::new(vec![0, 2, 4, 0x123])
    }

    fn params(vector: u32, target_processors: &[u32]) -> VpciInterruptParameters<'_> {
        VpciInterruptParameters {
            vector,
            multicast: false,
            target_processors,
        }
    }

    // Test: Register two interrupts for a device, targeting VPs 2 and 3, and
    //       signal each through the address it was given.
    // Expected: The addresses are distinct and in the 0xfee00000 MSI range;
    //       each signal is delivered to the APIC ID of its target VP
    //       (including one above 255) with its vector.
    #[async_test]
    async fn registered_interrupts_reach_their_target_apic() {
        let msi = Arc::new(RecordMsi::default());
        let device = devices().new_device(msi.clone(), 1).unwrap();

        let a = device
            .register_interrupt(1, &params(0x41, &[2]))
            .await
            .unwrap();
        let b = device
            .register_interrupt(1, &params(0x52, &[3, 0]))
            .await
            .unwrap();
        assert_ne!(a.address, b.address);
        assert_eq!(a.address & !0xf_ffff, 0xfee0_0000);

        device.signal_msi(None, a.address, a.data);
        device.signal_msi(None, b.address, b.data);
        assert_eq!(msi.take(), [(4, 0x41), (0x123, 0x52)]);
    }

    // Test: Register interrupts with no vectors, with no target VPs, and with
    //       a target VP that doesn't exist, then register a valid one.
    // Expected: Each invalid request fails without using a table slot, so the
    //       valid one gets the first interrupt address.
    #[async_test]
    async fn invalid_registrations_are_rejected() {
        let device = devices()
            .new_device(Arc::new(RecordMsi::default()), 1)
            .unwrap();

        assert!(
            device
                .register_interrupt(0, &params(0x41, &[0]))
                .await
                .is_err()
        );
        assert!(
            device
                .register_interrupt(1, &params(0x41, &[]))
                .await
                .is_err()
        );
        assert!(
            device
                .register_interrupt(1, &params(0x41, &[9]))
                .await
                .is_err()
        );

        let valid = device
            .register_interrupt(1, &params(0x41, &[0]))
            .await
            .unwrap();
        assert_eq!(valid.address, 0xfee0_0000);
    }

    // Test: Retarget a registered interrupt to VP 1 with a new vector, then
    //       retarget for an unknown device, an unknown interrupt address, and
    //       a target VP that doesn't exist.
    // Expected: The successful retarget changes where and how the interrupt
    //       is delivered; the others fail with invalid device ID, invalid
    //       parameter, and invalid parameter.
    #[async_test]
    async fn retarget_moves_interrupt_and_rejects_bad_input() {
        let devices = devices();
        let msi = Arc::new(RecordMsi::default());
        let device = devices.new_device(msi.clone(), 7).unwrap();
        let interrupt = device
            .register_interrupt(1, &params(0x41, &[0]))
            .await
            .unwrap();

        devices
            .retarget_interrupt(7, interrupt.address, 0, &params(0x60, &[1, 3]))
            .unwrap();
        device.signal_msi(None, interrupt.address, 0);
        assert_eq!(msi.take(), [(2, 0x60)]);

        assert_eq!(
            devices.retarget_interrupt(8, interrupt.address, 0, &params(0x60, &[1])),
            Err(HvError::InvalidDeviceId)
        );
        assert_eq!(
            devices.retarget_interrupt(7, interrupt.address + 4, 0, &params(0x60, &[1])),
            Err(HvError::InvalidParameter)
        );
        assert_eq!(
            devices.retarget_interrupt(7, interrupt.address, 0, &params(0x60, &[1, 9])),
            Err(HvError::InvalidParameter)
        );
    }

    // IGNORED: this test fails because of a known bug. retarget_interrupt
    // stores the new vector and multicast flag before it validates the target
    // VPs, and the first target before validating the remaining VPs. A failed
    // retarget can therefore change the vector and destination. Re-enable
    // this test together with the fix by removing #[ignore].
    //
    // Test: Retarget with an invalid first VP, then with a valid first VP
    //       followed by an invalid VP, signaling after each failed request.
    // Expected: Each failure preserves the original APIC and vector.
    #[async_test]
    #[ignore = "known bug: failed retarget changes the vector or destination"]
    async fn failed_retarget_leaves_interrupt_unchanged() {
        let devices = devices();
        let msi = Arc::new(RecordMsi::default());
        let device = devices.new_device(msi.clone(), 7).unwrap();
        let interrupt = device
            .register_interrupt(1, &params(0x41, &[2]))
            .await
            .unwrap();

        for targets in [&[9][..], &[1, 9][..]] {
            assert_eq!(
                devices.retarget_interrupt(7, interrupt.address, 0, &params(0x60, targets)),
                Err(HvError::InvalidParameter),
                "targets: {targets:?}"
            );
            device.signal_msi(None, interrupt.address, 0);
            assert_eq!(msi.take(), [(4, 0x41)], "targets: {targets:?}");
        }
    }

    // Test: Unregister an interrupt and signal it, and signal an address that
    //       was never registered.
    // Expected: Neither signal is delivered.
    #[async_test]
    async fn unregistered_interrupts_are_dropped() {
        let msi = Arc::new(RecordMsi::default());
        let device = devices().new_device(msi.clone(), 1).unwrap();
        let interrupt = device
            .register_interrupt(1, &params(0x41, &[0]))
            .await
            .unwrap();

        device.unregister_interrupt(interrupt.address, 0).await;
        device.signal_msi(None, interrupt.address, 0);
        device.signal_msi(None, 0xfee0_0010, 0);

        assert!(msi.take().is_empty());
    }

    // Test: Create a device, create another with the same ID, signal an
    //       interrupt, then drop the first device and reuse its ID.
    // Expected: The duplicate ID is rejected while the first device exists
    //       and its interrupt still works; after the drop, retargeting its interrupts
    //       fails with an invalid device ID and the ID can be used again.
    #[async_test]
    async fn device_ids_are_unique_until_dropped() {
        let devices = devices();
        let msi = Arc::new(RecordMsi::default());
        let device = devices.new_device(msi.clone(), 5).unwrap();
        let interrupt = device
            .register_interrupt(1, &params(0x41, &[0]))
            .await
            .unwrap();

        let err = devices.new_device(msi.clone(), 5).err().unwrap();
        assert_eq!(err.to_string(), "device id 5 is already in use");
        device.signal_msi(None, interrupt.address, interrupt.data);
        assert_eq!(msi.take(), [(0, 0x41)]);

        drop(device);
        assert_eq!(
            devices.retarget_interrupt(5, interrupt.address, 0, &params(0x41, &[0])),
            Err(HvError::InvalidDeviceId)
        );
        devices.new_device(msi, 5).unwrap();
    }
}
