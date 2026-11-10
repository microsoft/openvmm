// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCIe devices hot-plugged into a VM's hot-pluggable ports: building one for
//! a port, whether the guest hot-adds it or a restore re-creates it, and the
//! re-creation of the devices that a snapshot records as hot-plugged.
//!
//! A hot-plugged device's state unit records a dynamic
//! [contract](state_unit::UnitContract) whose stable identity is its port. A
//! restore whose VM lacks such a unit re-creates its device from a resource
//! that the restore supplies for that port, and attaches it to the port without
//! notifying the guest, whose view of the port the root complex's saved state
//! restores.

use super::LoadedVm;
use super::WeakMutexPciBusDevice;
use super::pcie_wiring;
#[cfg(guest_arch = "aarch64")]
use super::smmu_for_rc;
#[cfg(guest_arch = "x86_64")]
use super::x86_iommu_for_rc;
use anyhow::Context as _;
use chipset_device_resources::ErasedChipsetDevice;
use closeable_mutex::CloseableMutex;
use hvdef::Vtl;
use openvmm_defs::config::PcieDeviceConfig;
use openvmm_defs::worker::SavedState;
use pcie::root::GenericPcieRootComplex;
use std::sync::Arc;
use vm_resource::Resource;
use vm_resource::kind::PciDeviceHandleKind;

/// The name of the state unit of the device hot-plugged on `port_name`.
pub(super) fn hotplug_unit_name(port_name: &str) -> String {
    format!("pcie-hotplug:{port_name}")
}

/// A device built for a hot-pluggable PCIe port, not yet attached to it.
pub(super) struct PcieHotplugDevice {
    /// The root complex of the port.
    pub(super) root_complex: Arc<CloseableMutex<GenericPcieRootComplex>>,
    /// The port's devfn on the root complex's bus.
    pub(super) devfn: u8,
    /// The device's dynamic state unit.
    pub(super) unit: vmotherboard::DynamicDeviceUnit,
    /// The device.
    pub(super) device: Arc<CloseableMutex<ErasedChipsetDevice>>,
    /// The device as the port's link sees it.
    pub(super) bus_device: Box<dyn pci_bus::GenericPciBusDevice>,
}

impl LoadedVm {
    /// Builds the device of `resource` for the hot-pluggable PCIe port
    /// `port_name`, with a stopped dynamic state unit whose contract records
    /// the port, without attaching it to the port.
    pub(super) async fn build_pcie_hotplug_device(
        &mut self,
        port_name: &str,
        resource: Resource<PciDeviceHandleKind>,
    ) -> anyhow::Result<PcieHotplugDevice> {
        // Find the root complex and its index for the named port.
        let (rc_idx, root_complex) = self
            .inner
            .pcie_root_complexes
            .iter()
            .enumerate()
            .find(|(_, rc)| {
                rc.lock()
                    .downstream_ports()
                    .iter()
                    .any(|p| p.name.as_ref() == port_name)
            })
            .ok_or_else(|| anyhow::anyhow!("port '{}' not found in any root complex", port_name))?;
        let root_complex = root_complex.clone();

        // Get the devfn and bus_range from the port's config space emulator.
        let port = root_complex
            .lock()
            .downstream_ports()
            .into_iter()
            .find(|p| p.name.as_ref() == port_name)
            .expect("port was just found above");

        let segment = self.inner.pcie_host_bridges[rc_idx].segment;
        let msi_conn = pci_core::msi::MsiConnection::new();

        let pcie_ctx = pcie_wiring::build_device_wiring(pcie_wiring::PcieDeviceWiringParams {
            msi_platform: pcie_wiring::PcieMsiPlatform {
                partition: self.inner.partition.as_ref(),
                segment,
                processor_topology: &self.inner.processor_topology,
                #[cfg(guest_arch = "x86_64")]
                iommu: x86_iommu_for_rc(&self.inner.iommu_devices, rc_idx),
            },
            guest_memory: &self.inner.gm,
            bus_range: &port.bus_range,
            msi: &msi_conn,
            #[cfg(guest_arch = "aarch64")]
            smmu: smmu_for_rc(&self.inner.iommu_devices, rc_idx),
        });

        let mapper = self.inner.memory_manager.device_memory_mapper();
        let contract = vmotherboard::DynamicDeviceContract {
            stable_id: Some(vmotherboard::pcie_port_stable_id(port_name)),
            kind: resource.id().to_owned(),
        };
        let (unit, device) = self
            .inner
            .chipset_devices
            .add_dyn_device(
                &self.inner.driver_source,
                &self.state_units,
                hotplug_unit_name(port_name),
                contract,
                async |register_mmio| {
                    self.inner
                        .resolver
                        .resolve(
                            resource,
                            pci_resources::ResolvePciDeviceHandleParams {
                                dma_target: &pcie_ctx.dma_target,
                                register_mmio,
                                driver_source: &self.inner.driver_source,
                                doorbell_registration: self
                                    .inner
                                    .partition
                                    .clone()
                                    .into_doorbell_registration(Vtl::Vtl0),
                                shared_mem_mapper: Some(&mapper),
                            },
                        )
                        .await
                        .map(|r| r.0)
                        .map_err(|e| anyhow::anyhow!(e))
                },
            )
            .await?;

        // Connect the signal_msi and irqfd (possibly ITS-wrapped and/or
        // SMMU-wrapped).
        pcie_ctx.connect_to(&msi_conn);

        // Wrap the device as a GenericPciBusDevice for the port. Keep a strong
        // Arc to the device so the Weak stays valid.
        let weak_dev: std::sync::Weak<CloseableMutex<dyn chipset_device::ChipsetDevice>> =
            Arc::downgrade(
                &(device.clone() as Arc<CloseableMutex<dyn chipset_device::ChipsetDevice>>),
            );
        Ok(PcieHotplugDevice {
            root_complex,
            devfn: port.devfn,
            unit,
            device,
            bus_device: Box::new(WeakMutexPciBusDevice(weak_dev)),
        })
    }

    /// Re-creates the devices that `state` records as hot-plugged and that
    /// the VM lacks, from the devices that the restore supplied for their
    /// ports, so that the VM's state units then satisfy the saved inventory.
    ///
    /// Each device is attached to its port without notifying the guest: the
    /// root complex's saved state restores what the guest saw of the port.
    /// Fails if the saved state records a dynamic unit that is not a PCIe
    /// device, a device whose port has no supplied device or a device of
    /// another kind, or if a supplied device's port has none.
    pub(super) async fn recreate_hotplugged_pcie_devices(
        &mut self,
        state: &SavedState,
    ) -> anyhow::Result<()> {
        let mut supplied = std::mem::take(&mut self.snapshot_restore.pcie_devices);
        let plan = self
            .state_units
            .check_inventory(&state.inventory, &state.units)
            .context("saved state does not match the VM's state units")?;
        for contract in &plan.recreate {
            let port_name = vmotherboard::pcie_port_of_stable_id(&contract.stable_id)
                .with_context(|| {
                    format!(
                        "saved state records the hot-plugged state unit {contract}, which only \
                         a PCIe port's device can re-create"
                    )
                })?;
            anyhow::ensure!(
                contract.name == hotplug_unit_name(port_name),
                "saved state records the hot-plugged PCIe device {contract} under an unexpected \
                 name"
            );
            let index = supplied
                .iter()
                .position(|device| device.port_name == port_name)
                .with_context(|| {
                    format!(
                        "saved state records a device hot-plugged on PCIe port {port_name}, \
                         {contract}; supply its device on that port to restore it"
                    )
                })?;
            let PcieDeviceConfig {
                port_name: _,
                resource,
            } = supplied.swap_remove(index);
            anyhow::ensure!(
                resource.id() == contract.kind,
                "the device supplied for PCIe port {port_name} is a {}, but the saved state's \
                 is a {}",
                resource.id(),
                contract.kind
            );
            let PcieHotplugDevice {
                root_complex,
                devfn,
                unit,
                device,
                bus_device,
            } = self.build_pcie_hotplug_device(port_name, resource).await?;
            let attached = root_complex
                .lock()
                .add_pcie_device(devfn, "hotplug-device", bus_device);
            if let Err(occupant) = attached {
                unit.remove().await;
                anyhow::bail!("PCIe port {port_name} is already occupied by {occupant}");
            }
            tracing::info!(
                port_name,
                unit = contract.name,
                "re-created hot-plugged PCIe device"
            );
            self.inner
                .pcie_hotplug_devices
                .push((port_name.to_owned(), unit, device));
        }
        anyhow::ensure!(
            supplied.is_empty(),
            "saved state records no hot-plugged device on PCIe ports {}",
            supplied
                .iter()
                .map(|device| device.port_name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        Ok(())
    }
}
