// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Software implementation of a VPCI-compatible device. Avoids using the
//! hypervisor device interface.

use hvdef::HvError;
use hvdef::HvResult;
use inspect::Inspect;
use parking_lot::Mutex;
use pci_core::msi::SignalMsi;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::btree_map;
use std::collections::hash_map;
use std::sync::Arc;
use thiserror::Error;
use vmcore::vpci_msi::MapVpciInterrupt;
use vmcore::vpci_msi::MsiAddressData;
use vmcore::vpci_msi::RegisterInterruptError;
use vmcore::vpci_msi::VpciInterruptParameters;
use vmcore::vpci_msi::VpciInterruptState;
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
    /// Keyed by the index encoded in the interrupt's address.
    #[inspect(iter_by_key)]
    entries: BTreeMap<u16, InterruptEntry>,
}

/// State for an individual VPCI interrupt for a device.
#[derive(Debug, Inspect)]
struct InterruptEntry {
    base_vector: u32,
    vector_count: u32,
    multicast: bool,
    /// The target processors as VP indexes. Never empty.
    #[inspect(iter_by_index)]
    target_processors: Vec<u32>,
    /// The APIC ID of the first target processor, which receives the
    /// interrupt.
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
    #[error("no free interrupt index")]
    TableFull,
    #[error("invalid interrupt address {address:#x} or data {data:#x}")]
    InvalidAddress { address: u64, data: u32 },
    #[error("interrupt index {0} is already in use")]
    IndexInUse(u16),
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
            entries: BTreeMap::new(),
        }
    }

    fn interrupt_address_from_index(index: u16) -> u64 {
        // Per Intel spec, set the upper bits to FEE.
        // Set lower bits to the specified index, shifted to avoid the bits
        // that actually mean something (redirection hint / destination mode).
        0xFEE00000 | ((index as u64) << 2)
    }

    fn interrupt_index_from_address(address: u64) -> u16 {
        (address >> 2) as u16
    }

    /// Returns the index `msi` names, if `msi` has the exact form
    /// [`Self::register_interrupt`] hands out.
    fn index_from_msi(msi: MsiAddressData) -> Option<u16> {
        let index = Self::interrupt_index_from_address(msi.address);
        (msi.address == Self::interrupt_address_from_index(index) && msi.data == 0).then_some(index)
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
            .get_mut(&index)
            .ok_or(InvalidRetargetParams::InvalidAddress(address))?;

        // Validate every processor before changing anything.
        let apic_ids = params
            .target_processors
            .iter()
            .map(|&vp_index| {
                apic_id_map
                    .get(vp_index as usize)
                    .copied()
                    .ok_or(InvalidRetargetParams::InvalidVirtualProcessor(vp_index))
            })
            .collect::<Result<Vec<_>, _>>()?;

        interrupt.base_vector = params.vector;
        interrupt.multicast = params.multicast;
        if let Some(&target_apic_id) = apic_ids.first() {
            interrupt.target_apic_id = target_apic_id;
            interrupt.target_processors = params.target_processors.to_vec();
        }

        Ok(())
    }

    fn new_entry(
        apic_id_map: &[u32],
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<InterruptEntry, InvalidInterruptParams> {
        let &[vp, ..] = params.target_processors else {
            return Err(InvalidInterruptParams::InvalidHypercallInput);
        };
        if vector_count == 0 {
            return Err(InvalidInterruptParams::InvalidHypercallInput);
        }
        Ok(InterruptEntry {
            base_vector: params.vector,
            vector_count,
            multicast: params.multicast,
            target_processors: params.target_processors.to_vec(),
            target_apic_id: *apic_id_map
                .get(vp as usize)
                .ok_or(InvalidInterruptParams::InvalidVirtualProcessor(vp))?,
        })
    }

    fn register_interrupt(
        &mut self,
        apic_id_map: &[u32],
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<MsiAddressData, InvalidInterruptParams> {
        let entry = Self::new_entry(apic_id_map, vector_count, params)?;
        let index = (0..=u16::MAX)
            .find(|i| !self.entries.contains_key(i))
            .ok_or(InvalidInterruptParams::TableFull)?;
        self.entries.insert(index, entry);
        let address = Self::interrupt_address_from_index(index);
        Ok(MsiAddressData { address, data: 0 })
    }

    fn restore_interrupt(
        &mut self,
        apic_id_map: &[u32],
        msi: MsiAddressData,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<(), InvalidInterruptParams> {
        let entry = Self::new_entry(apic_id_map, vector_count, params)?;
        let index = Self::index_from_msi(msi).ok_or(InvalidInterruptParams::InvalidAddress {
            address: msi.address,
            data: msi.data,
        })?;
        match self.entries.entry(index) {
            btree_map::Entry::Vacant(e) => {
                e.insert(entry);
                Ok(())
            }
            btree_map::Entry::Occupied(_) => Err(InvalidInterruptParams::IndexInUse(index)),
        }
    }

    fn interrupt_state(&self, msi: MsiAddressData) -> Option<VpciInterruptState> {
        let interrupt = self.entries.get(&Self::index_from_msi(msi)?)?;
        Some(VpciInterruptState {
            vector: interrupt.base_vector,
            multicast: interrupt.multicast,
            target_processors: interrupt.target_processors.clone(),
        })
    }

    fn unregister_interrupt(&mut self, address: u64, _data: u32) {
        let index = Self::interrupt_index_from_address(address);
        self.entries.remove(&index);
    }
}

impl SignalMsi for ApicSoftwareDevice {
    fn signal_msi(&self, _devid: Option<u32>, address: u64, _data: u32) {
        let mut table = self.table.lock();
        let table = &mut *table;
        let index = InterruptTable::interrupt_index_from_address(address);
        if let Some(interrupt) = table.entries.get(&index) {
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

    async fn interrupt_state(
        &self,
        msi: MsiAddressData,
    ) -> Result<VpciInterruptState, RegisterInterruptError> {
        self.table
            .lock()
            .interrupt_state(msi)
            .ok_or_else(|| RegisterInterruptError::new("interrupt not registered"))
    }

    async fn restore_interrupt(
        &self,
        msi: MsiAddressData,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<(), RegisterInterruptError> {
        self.table
            .lock()
            .restore_interrupt(&self.devices.apic_id_map, msi, vector_count, params)
            .map_err(RegisterInterruptError::new)
    }

    fn supports_save_restore(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::ApicSoftwareDevice;
    use super::ApicSoftwareDevices;
    use pal_async::async_test;
    use parking_lot::Mutex;
    use pci_core::msi::SignalMsi;
    use std::sync::Arc;
    use vmcore::vpci_msi::MapVpciInterrupt;
    use vmcore::vpci_msi::MsiAddressData;
    use vmcore::vpci_msi::VpciInterruptParameters;
    use vmcore::vpci_msi::VpciInterruptState;
    use x86defs::msi::MsiAddress;
    use x86defs::msi::MsiData;

    const DEVICE_ID: u64 = 7;

    /// VP index `i` has APIC id `APIC_IDS[i]`.
    const APIC_IDS: [u32; 4] = [10, 11, 12, 13];

    /// Records the (APIC id, vector) of every MSI delivered to the APIC.
    #[derive(Default)]
    struct ApicRecorder(Mutex<Vec<(u16, u8)>>);

    impl SignalMsi for ApicRecorder {
        fn signal_msi(&self, _devid: Option<u32>, address: u64, data: u32) {
            let address = MsiAddress::from(address as u32);
            let data = MsiData::from(data);
            self.0
                .lock()
                .push((address.virt_destination(), data.vector()));
        }
    }

    struct TestDevice {
        devices: ApicSoftwareDevices,
        device: ApicSoftwareDevice,
        apic: Arc<ApicRecorder>,
    }

    impl TestDevice {
        fn new() -> Self {
            let devices = ApicSoftwareDevices::new(APIC_IDS.to_vec());
            let apic = Arc::new(ApicRecorder::default());
            let device = devices.new_device(apic.clone(), DEVICE_ID).unwrap();
            Self {
                devices,
                device,
                apic,
            }
        }

        /// Signals `msi` and returns what reached the APIC, if anything.
        fn deliver(&self, msi: MsiAddressData) -> Option<(u16, u8)> {
            self.device.signal_msi(None, msi.address, msi.data);
            let mut delivered = self.apic.0.lock();
            assert!(delivered.len() <= 1);
            delivered.pop()
        }

        async fn register(&self, vector: u32, target_processors: &[u32]) -> MsiAddressData {
            self.device
                .register_interrupt(
                    1,
                    &VpciInterruptParameters {
                        vector,
                        multicast: false,
                        target_processors,
                    },
                )
                .await
                .unwrap()
        }

        async fn restore(&self, msi: MsiAddressData, state: &VpciInterruptState) {
            self.device
                .restore_interrupt(
                    msi,
                    1,
                    &VpciInterruptParameters {
                        vector: state.vector,
                        multicast: state.multicast,
                        target_processors: &state.target_processors,
                    },
                )
                .await
                .unwrap()
        }
    }

    fn index_address(index: u64) -> MsiAddressData {
        MsiAddressData {
            address: 0xfee00000 | (index << 2),
            data: 0,
        }
    }

    #[async_test]
    async fn apic_supports_save_restore() {
        assert!(TestDevice::new().device.supports_save_restore());
    }

    #[async_test]
    async fn apic_interrupt_state_reports_retarget() {
        let dev = TestDevice::new();
        let msi = dev.register(0x30, &[0]).await;
        assert_eq!(
            dev.device.interrupt_state(msi).await.unwrap(),
            VpciInterruptState {
                vector: 0x30,
                multicast: false,
                target_processors: vec![0],
            }
        );

        dev.devices
            .retarget_interrupt(
                DEVICE_ID,
                msi.address,
                msi.data,
                &VpciInterruptParameters {
                    vector: 0x50,
                    multicast: true,
                    target_processors: &[3, 1],
                },
            )
            .unwrap();
        assert_eq!(
            dev.device.interrupt_state(msi).await.unwrap(),
            VpciInterruptState {
                vector: 0x50,
                multicast: true,
                target_processors: vec![3, 1],
            }
        );

        // A retarget with no processors keeps the current targets.
        dev.devices
            .retarget_interrupt(
                DEVICE_ID,
                msi.address,
                msi.data,
                &VpciInterruptParameters {
                    vector: 0x51,
                    multicast: false,
                    target_processors: &[],
                },
            )
            .unwrap();
        assert_eq!(
            dev.device.interrupt_state(msi).await.unwrap(),
            VpciInterruptState {
                vector: 0x51,
                multicast: false,
                target_processors: vec![3, 1],
            }
        );

        dev.device.unregister_interrupt(msi.address, msi.data).await;
        assert!(dev.device.interrupt_state(msi).await.is_err());
    }

    #[async_test]
    async fn apic_table_restore_at_index() {
        let old = TestDevice::new();
        let msis = [
            old.register(0x30, &[0]).await,
            old.register(0x31, &[1]).await,
            old.register(0x32, &[2]).await,
        ];
        old.devices
            .retarget_interrupt(
                DEVICE_ID,
                msis[1].address,
                msis[1].data,
                &VpciInterruptParameters {
                    vector: 0x41,
                    multicast: false,
                    target_processors: &[3],
                },
            )
            .unwrap();
        old.device
            .unregister_interrupt(msis[0].address, msis[0].data)
            .await;

        let mut saved = Vec::new();
        for &msi in &msis[1..] {
            saved.push((msi, old.device.interrupt_state(msi).await.unwrap()));
        }

        let new = TestDevice::new();
        for (msi, state) in &saved {
            new.restore(*msi, state).await;
        }

        for &msi in &msis {
            assert_eq!(new.deliver(msi), old.deliver(msi));
        }
        assert_eq!(new.deliver(msis[0]), None);
        assert_eq!(new.deliver(msis[1]), Some((13, 0x41)));
        assert_eq!(new.deliver(msis[2]), Some((12, 0x32)));
        for (msi, state) in &saved {
            assert_eq!(&new.device.interrupt_state(*msi).await.unwrap(), state);
        }
    }

    #[async_test]
    async fn apic_restore_refuses_bad_input() {
        let dev = TestDevice::new();
        let msi = dev.register(0x30, &[0]).await;
        let good = VpciInterruptParameters {
            vector: 0x31,
            multicast: false,
            target_processors: &[1],
        };
        let free = index_address(5);

        let refused = [
            // Occupied index.
            (msi, 1, &good),
            // Data must be zero.
            (MsiAddressData { data: 1, ..free }, 1, &good),
            // Not an address register_interrupt hands out.
            (
                MsiAddressData {
                    address: free.address | 1,
                    ..free
                },
                1,
                &good,
            ),
            (
                MsiAddressData {
                    address: free.address | (1 << 32),
                    ..free
                },
                1,
                &good,
            ),
            (
                MsiAddressData {
                    address: free.address & !0xfff00000,
                    ..free
                },
                1,
                &good,
            ),
            // No vectors.
            (free, 0, &good),
            // No target, or an unknown target.
            (
                free,
                1,
                &VpciInterruptParameters {
                    target_processors: &[],
                    ..good
                },
            ),
            (
                free,
                1,
                &VpciInterruptParameters {
                    target_processors: &[4],
                    ..good
                },
            ),
        ];
        for (bad_msi, vector_count, params) in refused {
            assert!(
                dev.device
                    .restore_interrupt(bad_msi, vector_count, params)
                    .await
                    .is_err(),
                "{bad_msi:x?} {vector_count} {:?}",
                params.target_processors
            );
        }

        // The refusals changed nothing.
        assert_eq!(dev.deliver(msi), Some((10, 0x30)));
        assert_eq!(dev.deliver(free), None);
        dev.device.restore_interrupt(free, 1, &good).await.unwrap();
        assert_eq!(dev.deliver(free), Some((11, 0x31)));
    }

    #[async_test]
    async fn apic_allocation_skips_restored_indexes() {
        let dev = TestDevice::new();
        let state = VpciInterruptState {
            vector: 0x30,
            multicast: false,
            target_processors: vec![0],
        };
        dev.restore(index_address(0), &state).await;
        dev.restore(index_address(2), &state).await;
        assert_eq!(dev.register(0x31, &[0]).await, index_address(1));
        assert_eq!(dev.register(0x32, &[0]).await, index_address(3));
    }

    #[async_test]
    async fn apic_retarget_refusal_changes_nothing() {
        let dev = TestDevice::new();
        let msi = dev.register(0x30, &[0]).await;
        dev.devices
            .retarget_interrupt(
                DEVICE_ID,
                msi.address,
                msi.data,
                &VpciInterruptParameters {
                    vector: 0x50,
                    multicast: true,
                    target_processors: &[1, 4],
                },
            )
            .unwrap_err();
        assert_eq!(dev.deliver(msi), Some((10, 0x30)));
    }

    #[async_test]
    async fn apic_unregister_twice_is_harmless() {
        let dev = TestDevice::new();
        let msi = dev.register(0x30, &[0]).await;
        dev.device.unregister_interrupt(msi.address, msi.data).await;
        dev.device.unregister_interrupt(msi.address, msi.data).await;
        assert_eq!(dev.deliver(msi), None);
    }
}
