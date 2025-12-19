// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::BusIdPci;
use crate::BusIdPcieDownstreamPort;
use crate::BusIdPcieEnumerator;
use crate::chipset::PciConflict;
use crate::chipset::PciConflictReason;
use crate::chipset::PcieConflict;
use crate::chipset::PcieConflictReason;
use chipset_device::ChipsetDevice;
use closeable_mutex::CloseableMutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Weak;

/// An abstraction over a PCI bus implementation that is able to route accesses
/// to `Weak<CloseableMutex<dyn ChipsetDevice>>` devices.
pub trait RegisterWeakMutexPci: Send {
    /// Try to add a PCI device to the bus, reporting any conflicts.
    fn add_pci_device(
        &mut self,
        bus: u8,
        device: u8,
        function: u8,
        name: Arc<str>,
        dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
    ) -> Result<(), PciConflict>;
}

pub struct WeakMutexPciEntry {
    pub bdf: (u8, u8, u8),
    pub name: Arc<str>,
    pub dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
}

#[derive(Default)]
pub struct BusResolverWeakMutexPci {
    pub buses: HashMap<BusIdPci, Box<dyn RegisterWeakMutexPci>>,
    pub devices: HashMap<BusIdPci, Vec<WeakMutexPciEntry>>,
}

impl BusResolverWeakMutexPci {
    pub fn resolve(mut self) -> Result<(), Vec<PciConflict>> {
        let mut errs = Vec::new();

        for (bus_id, entries) in self.devices {
            for WeakMutexPciEntry { bdf, name, dev } in entries {
                let pci_bus = match self.buses.get_mut(&bus_id) {
                    Some(bus) => bus,
                    None => {
                        errs.push(PciConflict {
                            bdf,
                            conflict_dev: name.clone(),
                            reason: PciConflictReason::MissingBus,
                        });
                        continue;
                    }
                };

                let (bus, device, function) = bdf;
                match pci_bus.add_pci_device(bus, device, function, name, dev) {
                    Ok(()) => {}
                    Err(conflict) => {
                        errs.push(conflict);
                        continue;
                    }
                };
            }
        }

        if !errs.is_empty() { Err(errs) } else { Ok(()) }
    }
}

/// An abstraction over an upstream PCIe enumerator implementation that
/// is able to route accesses to `Weak<CloseableMutex<dyn ChipsetDevice>>`
/// devices via downstream ports.
pub trait RegisterWeakMutexPcie: Send {
    /// Try to add a PCIe device to the enumerator at the specified port,
    /// reporting any conflicts.
    fn add_pcie_device(
        &mut self,
        port: u8,
        name: Arc<str>,
        device: Weak<CloseableMutex<dyn ChipsetDevice>>,
    ) -> Result<(), PcieConflict>;

    /// Enumerate the downstream ports.
    fn downstream_ports(&self) -> Vec<(u8, Arc<str>)>;
}

pub struct WeakMutexPcieDeviceEntry {
    pub bus_id_port: BusIdPcieDownstreamPort,
    pub name: Arc<str>,
    pub dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
}

#[derive(Default)]
pub struct BusResolverWeakMutexPcie {
    pub enumerators: HashMap<BusIdPcieEnumerator, Box<dyn RegisterWeakMutexPcie>>,
    pub ports: HashMap<BusIdPcieDownstreamPort, (u8, BusIdPcieEnumerator)>,
    pub devices: Vec<WeakMutexPcieDeviceEntry>,
}

impl BusResolverWeakMutexPcie {
    pub fn resolve(mut self) -> Result<(), Vec<PcieConflict>> {
        let mut errs = Vec::new();

        for WeakMutexPcieDeviceEntry {
            bus_id_port,
            name,
            dev,
        } in self.devices
        {
            let (port_number, bus_id_enumerator) = match self.ports.get(&bus_id_port) {
                Some(v) => v,
                None => {
                    errs.push(PcieConflict {
                        conflict_dev: name.clone(),
                        reason: PcieConflictReason::MissingDownstreamPort,
                    });
                    continue;
                }
            };

            let enumerator = match self.enumerators.get_mut(bus_id_enumerator) {
                Some(enumerator) => enumerator,
                None => {
                    errs.push(PcieConflict {
                        conflict_dev: name.clone(),
                        reason: PcieConflictReason::MissingEnumerator,
                    });
                    continue;
                }
            };

            match enumerator.add_pcie_device(*port_number, name, dev) {
                Ok(()) => {}
                Err(conflict) => {
                    errs.push(conflict);
                    continue;
                }
            };
        }

        if !errs.is_empty() { Err(errs) } else { Ok(()) }
    }
}
