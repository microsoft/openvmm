// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to build storage configuration from command line arguments.

use crate::VmResources;
use crate::cli_args::DiskCliKind;
use crate::cli_args::UnderhillDiskSource;
use crate::disk_open;
use anyhow::Context;
use guid::Guid;
use ide_resources::GuestMedia;
use ide_resources::IdeDeviceConfig;
use ide_resources::IdePath;
use nvme_resources::NamespaceDefinition;
use nvme_resources::NvmeControllerHandle;
use openvmm_defs::config::Config;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::LoadMode;
use openvmm_defs::config::PcieDeviceConfig;
use openvmm_defs::config::VpciDeviceConfig;
use scsidisk_resources::SimpleScsiDiskHandle;
use scsidisk_resources::SimpleScsiDvdHandle;
use std::collections::BTreeMap;
use storvsp_resources::ScsiControllerHandle;
use storvsp_resources::ScsiDeviceAndPath;
use storvsp_resources::ScsiPath;
use vm_resource::IntoResource;
use vtl2_settings_proto::Lun;
use vtl2_settings_proto::StorageController;
use vtl2_settings_proto::storage_controller;

pub(super) struct StorageBuilder {
    vtl0_ide_disks: Vec<IdeDeviceConfig>,
    vtl0_scsi_devices: Vec<ScsiDeviceAndPath>,
    vtl2_scsi_devices: Vec<ScsiDeviceAndPath>,
    vtl0_nvme_namespaces: Vec<NamespaceDefinition>,
    vtl2_nvme_namespaces: Vec<NamespaceDefinition>,
    pcie_nvme_controllers: BTreeMap<String, Vec<NamespaceDefinition>>,
    underhill_scsi_luns: Vec<Lun>,
    underhill_nvme_luns: Vec<Lun>,
    openhcl_vtl: Option<DeviceVtl>,
}

#[derive(Clone)]
pub enum DiskLocation {
    Ide(Option<u8>, Option<u8>),
    Scsi(Option<u8>),
    Nvme(Option<u32>, Option<String>),
}

impl From<UnderhillDiskSource> for DiskLocation {
    fn from(value: UnderhillDiskSource) -> Self {
        match value {
            UnderhillDiskSource::Scsi => Self::Scsi(None),
            UnderhillDiskSource::Nvme => Self::Nvme(None, None),
        }
    }
}

// Arbitrary but constant instance IDs to maintain the same device IDs
// across reboots.
const NVME_VTL0_INSTANCE_ID: Guid = guid::guid!("008091f6-9688-497d-9091-af347dc9173c");
/// The VTL2 NVMe controller instance ID used by OpenVMM.
pub const NVME_VTL2_INSTANCE_ID: Guid = guid::guid!("f9b90f6f-b129-4596-8171-a23481b8f718");
const SCSI_VTL0_INSTANCE_ID: Guid = guid::guid!("ba6163d9-04a1-4d29-b605-72e2ffb1dc7f");
/// The VTL2 SCSI controller instance ID used by OpenVMM.
pub const SCSI_VTL2_INSTANCE_ID: Guid = guid::guid!("73d3aa59-b82b-4fe7-9e15-e2b0b5575cf8");
/// The VTL0 SCSI controller instance ID used by OpenHCL to expose disks to VTL0.
pub const UNDERHILL_VTL0_SCSI_INSTANCE: Guid = guid::guid!("e1c5bd94-d0d6-41d4-a2b0-88095a16ded7");
const UNDERHILL_VTL0_NVME_INSTANCE: Guid = guid::guid!("09a59b81-2bf6-4164-81d7-3a0dc977ba65");

// PCIe controllers don't have VMBUS channel instance IDs the way VPCI
// enumerated controllers do but we still need to present different
// subsystem IDs to the guest and we want those to be somewhat reliable.
// Just hardcode a bunch for now.
const PCIE_NVME_SUBSYSTEM_IDS: [Guid; 16] = [
    guid::guid!("55bfb22d-3f6c-4d5a-8ed8-d779dbdae6b8"),
    guid::guid!("6e4fbff0-eefc-4982-9e09-faf2f185701e"),
    guid::guid!("5f429e81-06e4-4a5f-8763-1f589ce51f9d"),
    guid::guid!("9732c737-d78a-4c29-bc8c-72664b8fe970"),
    guid::guid!("8b561a94-6e13-4449-8b69-f37995b66a51"),
    guid::guid!("a17a3e14-9f12-426b-b48a-49c397cc0e5e"),
    guid::guid!("6e26115c-df74-432b-82a2-ced14fa80fa3"),
    guid::guid!("00335fd5-0967-45bf-abd0-1d2f46ab6f92"),
    guid::guid!("aeb1f8a9-f9e1-4177-84e2-3a31a73b57da"),
    guid::guid!("1a95b8bd-353e-41ff-8420-32c4173ef296"),
    guid::guid!("02613c53-23d1-4c0a-b3ab-90e8dc1bcec2"),
    guid::guid!("c95d1f3f-a89f-4727-bc16-6be1cbeed1ec"),
    guid::guid!("b2ded1f0-7a13-4c2a-83bd-d0156e3867a9"),
    guid::guid!("7f3ac17d-667f-470c-a441-6adcea9164a1"),
    guid::guid!("ca7d41a4-0337-47ee-990e-23140e652f47"),
    guid::guid!("5864e1e4-bb70-40d2-900c-2128034960d2"),
];

impl StorageBuilder {
    pub fn new(openhcl_vtl: Option<DeviceVtl>) -> Self {
        Self {
            vtl0_ide_disks: Vec::new(),
            vtl0_scsi_devices: Vec::new(),
            vtl2_scsi_devices: Vec::new(),
            vtl0_nvme_namespaces: Vec::new(),
            vtl2_nvme_namespaces: Vec::new(),
            pcie_nvme_controllers: BTreeMap::new(),
            underhill_scsi_luns: Vec::new(),
            underhill_nvme_luns: Vec::new(),
            openhcl_vtl,
        }
    }

    pub fn has_vtl0_nvme(&self) -> bool {
        !self.vtl0_nvme_namespaces.is_empty() || !self.underhill_nvme_luns.is_empty()
    }

    pub fn add(
        &mut self,
        vtl: DeviceVtl,
        underhill: Option<UnderhillDiskSource>,
        target: DiskLocation,
        kind: &DiskCliKind,
        is_dvd: bool,
        read_only: bool,
    ) -> anyhow::Result<()> {
        if let Some(source) = underhill {
            if vtl != DeviceVtl::Vtl0 {
                anyhow::bail!("underhill can only offer devices to vtl0");
            }
            self.add_underhill(source.into(), target, kind, is_dvd, read_only)?;
        } else {
            self.add_inner(vtl, target, kind, is_dvd, read_only)?;
        }
        Ok(())
    }

    /// Returns the "sub device path" for assigning this into Underhill, or
    /// `None` if Underhill can't use this device as a source.
    fn add_inner(
        &mut self,
        vtl: DeviceVtl,
        target: DiskLocation,
        kind: &DiskCliKind,
        is_dvd: bool,
        read_only: bool,
    ) -> anyhow::Result<Option<u32>> {
        let disk = disk_open(kind, read_only || is_dvd)?;
        let location = match target {
            DiskLocation::Ide(channel, device) => {
                let guest_media = if is_dvd {
                    GuestMedia::Dvd(
                        SimpleScsiDvdHandle {
                            media: Some(disk),
                            requests: None,
                        }
                        .into_resource(),
                    )
                } else {
                    GuestMedia::Disk {
                        disk_type: disk,
                        read_only,
                        disk_parameters: None,
                    }
                };

                let check = |c: u8, d: u8| {
                    channel.unwrap_or(c) == c
                        && device.unwrap_or(d) == d
                        && !self
                            .vtl0_ide_disks
                            .iter()
                            .any(|cfg| cfg.path.channel == c && cfg.path.drive == d)
                };

                let (channel, device) = (0..=1)
                    .flat_map(|c| std::iter::repeat(c).zip(0..=1))
                    .find(|&(c, d)| check(c, d))
                    .context("no free ide slots")?;

                if vtl != DeviceVtl::Vtl0 {
                    anyhow::bail!("ide only supported for VTL0");
                }
                self.vtl0_ide_disks.push(IdeDeviceConfig {
                    path: IdePath {
                        channel,
                        drive: device,
                    },
                    guest_media,
                });
                None
            }
            DiskLocation::Scsi(lun) => {
                let device = if is_dvd {
                    SimpleScsiDvdHandle {
                        media: Some(disk),
                        requests: None,
                    }
                    .into_resource()
                } else {
                    SimpleScsiDiskHandle {
                        disk,
                        read_only,
                        parameters: Default::default(),
                    }
                    .into_resource()
                };
                let devices = match vtl {
                    DeviceVtl::Vtl0 => &mut self.vtl0_scsi_devices,
                    DeviceVtl::Vtl1 => anyhow::bail!("vtl1 unsupported"),
                    DeviceVtl::Vtl2 => &mut self.vtl2_scsi_devices,
                };
                let lun = lun.unwrap_or(devices.len() as u8);
                devices.push(ScsiDeviceAndPath {
                    path: ScsiPath {
                        path: 0,
                        target: 0,
                        lun,
                    },
                    device,
                });
                Some(lun.into())
            }
            DiskLocation::Nvme(nsid, pcie_port) => {
                let namespaces = match (vtl, pcie_port) {
                    // VPCI
                    (DeviceVtl::Vtl0, None) => &mut self.vtl0_nvme_namespaces,
                    (DeviceVtl::Vtl1, None) => anyhow::bail!("vtl1 vpci unsupported"),
                    (DeviceVtl::Vtl2, None) => &mut self.vtl2_nvme_namespaces,
                    // PCIe
                    (DeviceVtl::Vtl0, Some(port)) => {
                        self.pcie_nvme_controllers.entry(port).or_default()
                    }
                    (DeviceVtl::Vtl1, Some(_)) => anyhow::bail!("vtl1 pcie unsupported"),
                    (DeviceVtl::Vtl2, Some(_)) => anyhow::bail!("vtl2 pcie unsupported"),
                };
                if is_dvd {
                    anyhow::bail!("dvd not supported with nvme");
                }
                let nsid = nsid.unwrap_or(namespaces.len() as u32 + 1);
                namespaces.push(NamespaceDefinition {
                    nsid,
                    disk,
                    read_only,
                });
                Some(nsid)
            }
        };
        Ok(location)
    }

    fn add_underhill(
        &mut self,
        source: DiskLocation,
        target: DiskLocation,
        kind: &DiskCliKind,
        is_dvd: bool,
        read_only: bool,
    ) -> anyhow::Result<()> {
        let vtl = self.openhcl_vtl.context("openhcl not configured")?;
        let sub_device_path = self
            .add_inner(vtl, source.clone(), kind, is_dvd, read_only)?
            .context("source device not supported by underhill")?;

        let (device_type, device_path) = match source {
            DiskLocation::Ide(_, _) => anyhow::bail!("ide source not supported for Underhill"),
            DiskLocation::Scsi(_) => (
                vtl2_settings_proto::physical_device::DeviceType::Vscsi,
                if vtl == DeviceVtl::Vtl2 {
                    SCSI_VTL2_INSTANCE_ID
                } else {
                    SCSI_VTL0_INSTANCE_ID
                },
            ),
            DiskLocation::Nvme(_, Some(_)) => {
                anyhow::bail!("underhill does not support consuming pcie")
            }
            DiskLocation::Nvme(_, None) => (
                vtl2_settings_proto::physical_device::DeviceType::Nvme,
                if vtl == DeviceVtl::Vtl2 {
                    NVME_VTL2_INSTANCE_ID
                } else {
                    NVME_VTL0_INSTANCE_ID
                },
            ),
        };

        let (luns, location) = match target {
            // TODO: once openvmm supports VTL2 with PCAT VTL0, remove this restriction.
            DiskLocation::Ide(_, _) => {
                anyhow::bail!("ide target currently not supported for Underhill (no PCAT support)")
            }
            DiskLocation::Scsi(lun) => {
                let lun = lun.unwrap_or(self.underhill_scsi_luns.len() as u8);
                (&mut self.underhill_scsi_luns, lun.into())
            }
            DiskLocation::Nvme(_, Some(_)) => {
                anyhow::bail!("underhill does not support targeting pcie")
            }
            DiskLocation::Nvme(nsid, None) => {
                let nsid = nsid.unwrap_or(self.underhill_nvme_luns.len() as u32 + 1);
                (&mut self.underhill_nvme_luns, nsid)
            }
        };

        luns.push(Lun {
            location,
            device_id: Guid::new_random().to_string(),
            vendor_id: "OpenVMM".to_string(),
            product_id: "Disk".to_string(),
            product_revision_level: "1.0".to_string(),
            serial_number: "0".to_string(),
            model_number: "1".to_string(),
            physical_devices: Some(vtl2_settings_proto::PhysicalDevices {
                r#type: vtl2_settings_proto::physical_devices::BackingType::Single.into(),
                device: Some(vtl2_settings_proto::PhysicalDevice {
                    device_type: device_type.into(),
                    device_path: device_path.to_string(),
                    sub_device_path,
                }),
                devices: Vec::new(),
            }),
            is_dvd,
            ..Default::default()
        });

        Ok(())
    }

    pub fn build_config(
        &mut self,
        config: &mut Config,
        resources: &mut VmResources,
        scsi_sub_channels: u16,
    ) -> anyhow::Result<()> {
        config.ide_disks.append(&mut self.vtl0_ide_disks);

        // Add an empty VTL0 SCSI controller even if there are no configured disks.
        if !self.vtl0_scsi_devices.is_empty() || config.vmbus.is_some() {
            let (send, recv) = mesh::channel();
            config.vmbus_devices.push((
                DeviceVtl::Vtl0,
                ScsiControllerHandle {
                    instance_id: SCSI_VTL0_INSTANCE_ID,
                    max_sub_channel_count: scsi_sub_channels,
                    devices: std::mem::take(&mut self.vtl0_scsi_devices),
                    io_queue_depth: None,
                    requests: Some(recv),
                    poll_mode_queue_depth: None,
                }
                .into_resource(),
            ));
            resources.scsi_rpc = Some(send);
        }

        if !self.vtl2_scsi_devices.is_empty() {
            if config
                .hypervisor
                .with_vtl2
                .as_ref()
                .is_none_or(|c| c.vtl0_alias_map)
            {
                anyhow::bail!("must specify --vtl2 and --no-alias-map to offer disks to VTL2");
            }
            config.vmbus_devices.push((
                DeviceVtl::Vtl2,
                ScsiControllerHandle {
                    instance_id: SCSI_VTL2_INSTANCE_ID,
                    max_sub_channel_count: scsi_sub_channels,
                    devices: std::mem::take(&mut self.vtl2_scsi_devices),
                    io_queue_depth: None,
                    requests: None,
                    poll_mode_queue_depth: None,
                }
                .into_resource(),
            ));
        }

        if !self.vtl0_nvme_namespaces.is_empty() {
            config.vpci_devices.push(VpciDeviceConfig {
                vtl: DeviceVtl::Vtl0,
                instance_id: NVME_VTL0_INSTANCE_ID,
                resource: NvmeControllerHandle {
                    subsystem_id: NVME_VTL0_INSTANCE_ID,
                    namespaces: std::mem::take(&mut self.vtl0_nvme_namespaces),
                    max_io_queues: 64,
                    msix_count: 64,
                    requests: None,
                }
                .into_resource(),
            });

            // Tell UEFI to try to enumerate VPCI devices since there might be
            // an NVMe namespace to boot from.
            if let LoadMode::Uefi {
                enable_vpci_boot: vpci_boot,
                ..
            } = &mut config.load_mode
            {
                *vpci_boot = true;
            }
        }

        if config
            .hypervisor
            .with_vtl2
            .as_ref()
            .is_none_or(|c| c.vtl0_alias_map)
        {
            if !self.vtl2_nvme_namespaces.is_empty() {
                anyhow::bail!("must specify --vtl2 and --no-alias-map to offer disks to VTL2");
            }
        } else {
            // If VTL2 is being used, always add an NVMe controller, even
            // if there are no namespaces, to allow for hot-plugging.
            let (send, recv) = mesh::channel();
            config.vpci_devices.push(VpciDeviceConfig {
                vtl: DeviceVtl::Vtl2,
                instance_id: NVME_VTL2_INSTANCE_ID,
                resource: NvmeControllerHandle {
                    subsystem_id: NVME_VTL2_INSTANCE_ID,
                    namespaces: std::mem::take(&mut self.vtl2_nvme_namespaces),
                    max_io_queues: 64,
                    msix_count: 64,
                    requests: Some(recv),
                }
                .into_resource(),
            });
            resources.nvme_vtl2_rpc = Some(send);
        }

        let owned_pcie_controllers = std::mem::take(&mut self.pcie_nvme_controllers);
        if owned_pcie_controllers.len() > PCIE_NVME_SUBSYSTEM_IDS.len() {
            anyhow::bail!(
                "too many PCIe nvme controllers, max supported: {}",
                PCIE_NVME_SUBSYSTEM_IDS.len()
            );
        }
        for ((port_name, namespaces), subsystem_id) in owned_pcie_controllers
            .into_iter()
            .zip(PCIE_NVME_SUBSYSTEM_IDS)
        {
            config.pcie_devices.push(PcieDeviceConfig {
                port_name,
                resource: NvmeControllerHandle {
                    subsystem_id,
                    namespaces,
                    max_io_queues: 64,
                    msix_count: 64,
                    requests: None,
                }
                .into_resource(),
            });
        }

        Ok(())
    }

    /// Generate VTL2 settings for storage devices offered to the guest via
    /// OpenHCL.
    pub fn build_underhill(&self) -> Vec<StorageController> {
        let mut storage_controllers = Vec::new();
        // Always create a SCSI controller, even if no LUNs configured, to allow hot-plugging
        let controller = StorageController {
            instance_id: UNDERHILL_VTL0_SCSI_INSTANCE.to_string(),
            protocol: storage_controller::StorageProtocol::Scsi.into(),
            luns: self.underhill_scsi_luns.clone(),
            io_queue_depth: None,
        };
        storage_controllers.push(controller);

        if !self.underhill_nvme_luns.is_empty() {
            let controller = StorageController {
                instance_id: UNDERHILL_VTL0_NVME_INSTANCE.to_string(),
                protocol: storage_controller::StorageProtocol::Nvme.into(),
                luns: self.underhill_nvme_luns.clone(),
                io_queue_depth: None,
            };
            storage_controllers.push(controller);
        }

        storage_controllers
    }
}
