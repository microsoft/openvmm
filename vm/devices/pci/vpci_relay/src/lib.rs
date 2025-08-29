// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtual PCI relay
//!
//! This module provides a virtual PCI relay for the OpenHCL paravisor. It
//! consumes VPCI buses from the host and relays them to the guest, filtering
//! them as needed.

#[cfg(target_os = "linux")]
pub mod linux_mmio;

use anyhow::Context as _;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::pci::PciConfigSpace;
use futures::FutureExt as _;
use futures::StreamExt as _;
use inspect::Inspect;
use inspect::InspectMut;
use memory_range::MemoryRange;
use pci_core::spec::hwid::HardwareIds;
use state_unit::StateUnits;
use std::future::poll_fn;
use std::sync::Arc;
use std::task::Poll;
use user_driver::DmaClient;
use vmbus_client::driver::OpenParams;
use vmbus_server::Guid;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateNotSupported;
use vmcore::vm_task::VmTaskDriverSource;
use vmcore::vpci_msi::VpciInterruptMapper;
use vmotherboard::ChipsetDevices;
use vmotherboard::DynamicDeviceUnit;
use vpci_client::MemoryAccess;
use vpci_client::VpciClient;
use vpci_client::VpciDevice;
use vpci_client::VpciDeviceRemoved;

/// Trait for creating memory access instances.
pub trait CreateMemoryAccess: 'static + Send + Sync {
    /// Creates a new memory access instance for the given guest physical address.
    fn create_memory_access(&self, gpa: u64) -> anyhow::Result<Box<dyn MemoryAccess>>;
}

/// The size of the MMIO region required for each VPCI device.
pub const VPCI_RELAY_MMIO_PER_DEVICE: u64 = vpci_client::MMIO_SIZE;

/// Virtual PCI relay.
#[derive(Inspect)]
pub struct VpciRelay {
    #[inspect(skip)]
    driver_source: VmTaskDriverSource,
    dma_client: Arc<dyn DmaClient>,
    #[inspect(skip)]
    new_buses: Vec<vmbus_client::OfferInfo>,
    #[inspect(skip)]
    bus_recv: mesh::Receiver<vmbus_client::OfferInfo>,
    #[inspect(skip)]
    vmbus: Arc<vmbus_server::VmbusServerControl>,
    #[inspect(iter_by_key)]
    devices: slab::Slab<RelayedDevice>,
    mmio_range: MemoryRange,
    #[inspect(skip)]
    mmio_access: Box<dyn CreateMemoryAccess>,
    #[inspect(iter_by_index)]
    allowed_devices: Vec<HardwareIds>,
}

#[derive(Inspect)]
struct RelayedDevice {
    bus_instance_id: Guid,
    bus_client: VpciClient,
    #[inspect(skip)]
    removed: VpciDeviceRemoved,
    #[inspect(skip)]
    bus_unit: DynamicDeviceUnit,
    #[inspect(skip)]
    device_unit: DynamicDeviceUnit,
    ready_to_remove: bool,
}

impl RelayedDevice {
    async fn remove(self) {
        self.bus_unit.remove().await;
        self.device_unit.remove().await;
        self.bus_client.shutdown().await;
    }
}

fn allows(a: &HardwareIds, b: &HardwareIds) -> bool {
    let HardwareIds {
        vendor_id,
        device_id,
        revision_id,
        prog_if,
        sub_class,
        base_class,
        type0_sub_vendor_id,
        type0_sub_system_id,
    } = *a;
    (vendor_id == !0 || vendor_id == b.vendor_id)
        && (device_id == !0 || device_id == b.device_id)
        && (revision_id == !0 || revision_id == b.revision_id)
        && (prog_if.0 == !0 || prog_if == b.prog_if)
        && (sub_class.0 == !0 || sub_class == b.sub_class)
        && (base_class.0 == !0 || base_class == b.base_class)
        && (type0_sub_vendor_id == !0 || type0_sub_vendor_id == b.type0_sub_vendor_id)
        && (type0_sub_system_id == !0 || type0_sub_system_id == b.type0_sub_system_id)
}

impl VpciRelay {
    /// Creates a new VPCI relay.
    pub fn new(
        driver_source: VmTaskDriverSource,
        offers: vmbus_client::ConnectResult,
        vmbus: Arc<vmbus_server::VmbusServerControl>,
        dma_client: Arc<dyn DmaClient>,
        mmio_range: MemoryRange,
        mmio_access: Box<dyn CreateMemoryAccess>,
    ) -> Self {
        Self {
            driver_source,
            dma_client,
            new_buses: offers.offers,
            bus_recv: offers.offer_recv,
            vmbus,
            devices: slab::Slab::new(),
            mmio_range,
            mmio_access,
            allowed_devices: Vec::new(),
        }
    }

    /// Adds an allowed device to the list. If one of the hardware ID is `!0`
    /// then it is treated as a wildcard.
    ///
    /// Note that if no devices are on the list, then all devices are allowed.
    pub fn add_allowed_device(&mut self, hw_ids: HardwareIds) {
        self.allowed_devices.push(hw_ids);
    }

    /// Wait for the relay to be ready. This might never return. This call is cancellable.
    pub async fn wait_ready(&mut self) {
        poll_fn(|cx| {
            if !self.new_buses.is_empty() {
                return Poll::Ready(());
            }
            if self.devices.iter_mut().any(|(_, dev)| {
                let p = dev.ready_to_remove || dev.removed.poll_unpin(cx).is_ready();
                if p {
                    dev.ready_to_remove = true;
                }
                p
            }) {
                return Poll::Ready(());
            }
            if let Poll::Ready(Some(bus)) = self.bus_recv.poll_next_unpin(cx) {
                self.new_buses.push(bus);
                return Poll::Ready(());
            }
            Poll::Pending
        })
        .await
    }

    /// Process any waiting activity. This call is not cancellable.
    pub async fn process(
        &mut self,
        chipset: &ChipsetDevices,
        units: &mut StateUnits,
    ) -> anyhow::Result<()> {
        let mut i = 0;
        while i < self.devices.len() {
            if self.devices[i].ready_to_remove {
                let dev = self.devices.remove(i);
                dev.remove().await;
            } else {
                i += 1;
            }
        }
        while let Some(bus) = self.new_buses.pop() {
            self.relay_vpci_bus(chipset, units, bus).await?;
        }
        Ok(())
    }

    async fn relay_vpci_bus(
        &mut self,
        chipset: &ChipsetDevices,
        state_units: &mut StateUnits,
        offer_info: vmbus_client::OfferInfo,
    ) -> anyhow::Result<()> {
        let entry = self.devices.vacant_entry();
        if (entry.key() as u64 + 1) * vpci_client::MMIO_SIZE > self.mmio_range.len() {
            anyhow::bail!("not enough MMIO space left");
        }

        let instance_id = offer_info.offer.instance_id;

        let mmio = self.mmio_access.create_memory_access(
            self.mmio_range.start() + (entry.key() as u64) * vpci_client::MMIO_SIZE,
        )?;

        let channel = vmbus_client::driver::open_channel(
            self.driver_source.simple(),
            offer_info,
            OpenParams {
                ring_pages: 20,
                ring_offset_in_pages: 10,
            },
            self.dma_client.as_ref(),
        )
        .await?;

        // FUTURE: handle more than one device. Note, though, that Hyper-V
        // doesn't really do this in practice.
        let (devices, _devices_recv) = mesh::channel();
        let (vpci_client, devices) =
            VpciClient::connect(self.driver_source.simple(), channel, mmio, devices).await?;

        let Some(vpci_device) = devices.into_iter().next() else {
            tracing::info!(%instance_id, "no device on VPCI bus");
            return Ok(());
        };

        let hw_ids = vpci_device.hw_ids();

        if !self.allowed_devices.is_empty()
            && !self.allowed_devices.iter().any(|d| allows(d, hw_ids))
        {
            tracing::warn!(%instance_id, vendor_id = hw_ids.vendor_id, device_id = hw_ids.device_id, "device not allowed on VPCI bus");
            return Ok(());
        }

        tracing::info!(%instance_id, vendor_id = hw_ids.vendor_id, device_id = hw_ids.device_id, "vpci relay device arrived");

        let (vpci_device, removed) = vpci_device
            .init()
            .await
            .context("failed to initialize vpci device")?;
        let vpci_device = Arc::new(vpci_device);

        let device_name = format!("assigned_device:vpci-{instance_id}");
        let (device_unit, device) = chipset
            .add_dyn_device(&self.driver_source, state_units, device_name, async |_| {
                Ok(RelayedVpciDevice(vpci_device.clone()))
            })
            .await?;

        let interrupt_mapper = VpciInterruptMapper::new(vpci_device);

        let (bus_unit, _) = {
            let vpci_bus_name = format!("vpci:{instance_id}");
            chipset
                .add_dyn_device(
                    &self.driver_source,
                    state_units,
                    vpci_bus_name,
                    async |mmio| {
                        let bus = vpci::bus::VpciBus::new(
                            &self.driver_source,
                            instance_id,
                            device,
                            mmio,
                            self.vmbus.as_ref(),
                            interrupt_mapper,
                        )
                        .await?;

                        anyhow::Ok(bus)
                    },
                )
                .await?
        };

        entry.insert(RelayedDevice {
            bus_instance_id: instance_id,
            bus_client: vpci_client,
            removed,
            bus_unit,
            device_unit,
            ready_to_remove: false,
        });

        state_units.start_stopped_units().await;
        Ok(())
    }
}

#[derive(InspectMut)]
#[inspect(transparent)]
struct RelayedVpciDevice(Arc<VpciDevice>);

impl ChipsetDevice for RelayedVpciDevice {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }
}

impl PciConfigSpace for RelayedVpciDevice {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        *value = self.0.read_cfg(offset);
        IoResult::Ok
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        self.0.write_cfg(offset, value);
        IoResult::Ok
    }
}

impl ChangeDeviceState for RelayedVpciDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {}
}

impl SaveRestore for RelayedVpciDevice {
    type SavedState = SavedStateNotSupported;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Err(SaveError::NotSupported)
    }

    fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
        match state {}
    }
}
