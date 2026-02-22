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
    fn signal_msi(&self, _rid: u32, address: u64, _data: u32) {
        let mut table = self.table.lock();
        let table = &mut *table;
        let index = InterruptTable::interrupt_index_from_address(address);
        if let Some(interrupt) = table.entries.get(index) {
            let target = interrupt.msi_params();
            self.target.signal_msi(0, target.address, target.data)
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
