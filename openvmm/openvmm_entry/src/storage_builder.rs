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
use nvme_resources::NvmeControllerRequest;
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
use virtio_resources::VirtioPciDeviceHandle;
use virtio_resources::blk::VirtioBlkHandle;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::DiskHandleKind;
use vtl2_settings_proto::Lun;
use vtl2_settings_proto::StorageController;
use vtl2_settings_proto::storage_controller;

/// Namespace GUID for deriving deterministic GUIDs from controller names.
/// This is hashed together with the name via SHA-256 to produce a UUIDv8.
const OPENVMM_CONTROLLER_NS: Guid = guid::guid!("a3f1e2d4-5b6c-4a8d-9e0f-1234567890ab");

/// Derive a deterministic GUID from a name string using UUIDv8 (RFC 9562
/// §5.8) with SHA-256. The result is stable across Rust versions and
/// process restarts for the same input name.
pub(super) fn deterministic_guid(name: &str) -> Guid {
    let mut hasher = crypto::sha_256::Sha256::new();
    hasher.update(&OPENVMM_CONTROLLER_NS.data1.to_le_bytes());
    hasher.update(&OPENVMM_CONTROLLER_NS.data2.to_le_bytes());
    hasher.update(&OPENVMM_CONTROLLER_NS.data3.to_le_bytes());
    hasher.update(&OPENVMM_CONTROLLER_NS.data4);
    hasher.update(name.as_bytes());
    let hash = hasher.finish();
    let b = &hash[..16];
    Guid {
        data1: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        data2: u16::from_le_bytes([b[4], b[5]]),
        // Version 8
        data3: u16::from_le_bytes([b[6], b[7]]) & 0x0fff | 0x8000,
        data4: {
            let mut d = [b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]];
            // Variant 10
            d[0] = (d[0] & 0x3f) | 0x80;
            d
        },
    }
}

/// Transport for a named NVMe controller.
#[derive(Clone, PartialEq)]
pub enum NvmeControllerTransport {
    /// PCIe under a specific root port name.
    Pcie(String),
    /// VPCI with a specific instance GUID.
    Vpci(Guid),
}

struct NvmeControllerEntry {
    vtl: DeviceVtl,
    transport: NvmeControllerTransport,
    namespaces: Vec<NamespaceDefinition>,
    /// Optional pre-created channel for runtime namespace add/remove.
    requests: Option<mesh::Receiver<NvmeControllerRequest>>,
}

struct ScsiControllerEntry {
    vtl: DeviceVtl,
    instance_id: Guid,
    sub_channels: u16,
    devices: Vec<ScsiDeviceAndPath>,
}

/// A named controller entry (NVMe or SCSI).
enum ControllerEntry {
    Nvme(NvmeControllerEntry),
    Scsi(ScsiControllerEntry),
}

/// Protocol type for an OpenHCL-managed controller.
#[derive(Copy, Clone, PartialEq)]
pub enum OpenhclControllerType {
    Scsi,
    Nvme,
}

struct OpenhclControllerEntry {
    controller_type: OpenhclControllerType,
    instance_id: Guid,
    luns: Vec<Lun>,
}

/// Relay target for a disk routed through OpenHCL.
#[derive(Clone)]
pub struct RelayTarget {
    /// Name of the OpenHCL controller to expose the disk on.
    pub controller: String,
    /// Location on the target controller (LUN for SCSI, NSID for NVMe).
    /// Auto-assigned if `None`.
    pub location: Option<u32>,
}

pub(super) struct StorageBuilder {
    vtl0_ide_disks: Vec<IdeDeviceConfig>,
    vtl0_scsi_devices: Vec<ScsiDeviceAndPath>,
    vtl2_scsi_devices: Vec<ScsiDeviceAndPath>,
    vtl0_nvme_namespaces: Vec<NamespaceDefinition>,
    vtl2_nvme_namespaces: Vec<NamespaceDefinition>,
    controllers: BTreeMap<String, ControllerEntry>,
    openhcl_controllers: BTreeMap<String, OpenhclControllerEntry>,
    pcie_virtio_blk_disks: Vec<(String, VirtioBlkDisk)>,
    underhill_scsi_luns: Vec<Lun>,
    underhill_nvme_luns: Vec<Lun>,
    vtl0_virtio_blk_disks: Vec<VirtioBlkDisk>,
    openhcl_vtl: Option<DeviceVtl>,
}

struct VirtioBlkDisk {
    disk: Resource<DiskHandleKind>,
    read_only: bool,
}

#[derive(Clone)]
pub enum DiskLocation {
    Ide(Option<u8>, Option<u8>),
    /// Implicit VTL0/VTL2 SCSI controller.
    Scsi(Option<u8>),
    /// Implicit VTL0/VTL2 VPCI NVMe controller.
    Nvme(Option<u32>),
    /// Named controller (NVMe or SCSI, resolved by name at add time).
    Named {
        controller: String,
        nsid: Option<u32>,
        lun: Option<u8>,
    },
    VirtioBlk(Option<String>),
}

impl From<UnderhillDiskSource> for DiskLocation {
    fn from(value: UnderhillDiskSource) -> Self {
        match value {
            UnderhillDiskSource::Scsi => Self::Scsi(None),
            UnderhillDiskSource::Nvme => Self::Nvme(None),
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

/// Template GUID for virtio-blk VPCI instance IDs. `data1` is set to the
/// disk index to produce a unique ID per device. The remaining fields are
/// an arbitrarily generated fixed value.
const VIRTIO_BLK_INSTANCE_ID_TEMPLATE: Guid = guid::guid!("00000000-a4e7-4b53-b702-1f42d938647e");

impl StorageBuilder {
    pub fn new(openhcl_vtl: Option<DeviceVtl>) -> Self {
        Self {
            vtl0_ide_disks: Vec::new(),
            vtl0_scsi_devices: Vec::new(),
            vtl2_scsi_devices: Vec::new(),
            vtl0_nvme_namespaces: Vec::new(),
            vtl2_nvme_namespaces: Vec::new(),
            controllers: BTreeMap::new(),
            openhcl_controllers: BTreeMap::new(),
            pcie_virtio_blk_disks: Vec::new(),
            underhill_scsi_luns: Vec::new(),
            underhill_nvme_luns: Vec::new(),
            vtl0_virtio_blk_disks: Vec::new(),
            openhcl_vtl,
        }
    }

    pub fn has_vtl0_nvme(&self) -> bool {
        !self.vtl0_nvme_namespaces.is_empty()
            || !self.underhill_nvme_luns.is_empty()
            || self.controllers.values().any(|entry| {
                matches!(
                    entry,
                    ControllerEntry::Nvme(nvme)
                        if nvme.vtl == DeviceVtl::Vtl0
                            && matches!(nvme.transport, NvmeControllerTransport::Vpci(_))
                            && !nvme.namespaces.is_empty()
                )
            })
    }

    /// Register a named NVMe controller.
    pub fn add_nvme_controller(
        &mut self,
        name: String,
        vtl: DeviceVtl,
        transport: NvmeControllerTransport,
        requests: Option<mesh::Receiver<NvmeControllerRequest>>,
    ) -> anyhow::Result<()> {
        if let Some(existing) = self.controllers.get(&name) {
            let kind = match existing {
                ControllerEntry::Nvme(_) => "NVMe",
                ControllerEntry::Scsi(_) => "SCSI",
            };
            anyhow::bail!(
                "cannot add NVMe controller '{name}': name already used by a {kind} controller"
            );
        }
        self.controllers.insert(
            name,
            ControllerEntry::Nvme(NvmeControllerEntry {
                vtl,
                transport,
                namespaces: Vec::new(),
                requests,
            }),
        );
        Ok(())
    }

    /// Register a named SCSI controller.
    pub fn add_scsi_controller(
        &mut self,
        name: String,
        vtl: DeviceVtl,
        instance_id: Guid,
        sub_channels: u16,
    ) -> anyhow::Result<()> {
        if let Some(existing) = self.controllers.get(&name) {
            let kind = match existing {
                ControllerEntry::Nvme(_) => "NVMe",
                ControllerEntry::Scsi(_) => "SCSI",
            };
            anyhow::bail!(
                "cannot add SCSI controller '{name}': name already used by a {kind} controller"
            );
        }
        self.controllers.insert(
            name,
            ControllerEntry::Scsi(ScsiControllerEntry {
                vtl,
                instance_id,
                sub_channels,
                devices: Vec::new(),
            }),
        );
        Ok(())
    }

    /// Register an OpenHCL-managed controller (relay target).
    pub fn add_openhcl_controller(
        &mut self,
        name: String,
        controller_type: OpenhclControllerType,
        instance_id: Guid,
    ) -> anyhow::Result<()> {
        if self.openhcl_controllers.contains_key(&name) {
            anyhow::bail!("duplicate OpenHCL controller name: '{name}'");
        }
        self.openhcl_controllers.insert(
            name,
            OpenhclControllerEntry {
                controller_type,
                instance_id,
                luns: Vec::new(),
            },
        );
        Ok(())
    }

    pub async fn add(
        &mut self,
        vtl: DeviceVtl,
        underhill: Option<UnderhillDiskSource>,
        relay: Option<RelayTarget>,
        target: DiskLocation,
        kind: &DiskCliKind,
        is_dvd: bool,
        read_only: bool,
    ) -> anyhow::Result<()> {
        if let Some(source) = underhill {
            if vtl != DeviceVtl::Vtl0 {
                anyhow::bail!("OpenHCL relay can only offer devices to VTL0");
            }
            self.add_underhill(source.into(), target, kind, is_dvd, read_only)
                .await?;
        } else if let Some(relay) = relay {
            self.add_relay(relay, target, kind, is_dvd, read_only)
                .await?;
        } else {
            self.add_inner(vtl, target, kind, is_dvd, read_only).await?;
        }
        Ok(())
    }

    /// Returns the "sub device path" for assigning this into Underhill, or
    /// `None` if Underhill can't use this device as a source.
    async fn add_inner(
        &mut self,
        vtl: DeviceVtl,
        target: DiskLocation,
        kind: &DiskCliKind,
        is_dvd: bool,
        read_only: bool,
    ) -> anyhow::Result<Option<u32>> {
        let disk = disk_open(kind, read_only || is_dvd).await?;
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
            DiskLocation::Nvme(nsid) => {
                let namespaces = match vtl {
                    DeviceVtl::Vtl0 => &mut self.vtl0_nvme_namespaces,
                    DeviceVtl::Vtl1 => anyhow::bail!("vtl1 vpci unsupported"),
                    DeviceVtl::Vtl2 => &mut self.vtl2_nvme_namespaces,
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
            DiskLocation::Named {
                controller,
                nsid,
                lun,
            } => match self.controllers.get_mut(&controller) {
                Some(ControllerEntry::Nvme(nvme)) => {
                    if lun.is_some() {
                        anyhow::bail!("`lun` is not valid for NVMe controller '{controller}'");
                    }
                    if is_dvd {
                        anyhow::bail!("dvd not supported with nvme");
                    }
                    let nsid = nsid.unwrap_or(nvme.namespaces.len() as u32 + 1);
                    if nvme.namespaces.iter().any(|ns| ns.nsid == nsid) {
                        anyhow::bail!(
                            "duplicate namespace ID {nsid} on NVMe controller '{controller}'"
                        );
                    }
                    nvme.namespaces.push(NamespaceDefinition {
                        nsid,
                        disk,
                        read_only,
                    });
                    Some(nsid)
                }
                Some(ControllerEntry::Scsi(scsi)) => {
                    if nsid.is_some() {
                        anyhow::bail!("`nsid` is not valid for SCSI controller '{controller}'");
                    }
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
                    let lun = lun.unwrap_or(scsi.devices.len() as u8);
                    if scsi.devices.iter().any(|d| d.path.lun == lun) {
                        anyhow::bail!("duplicate LUN {lun} on SCSI controller '{controller}'");
                    }
                    scsi.devices.push(ScsiDeviceAndPath {
                        path: ScsiPath {
                            path: 0,
                            target: 0,
                            lun,
                        },
                        device,
                    });
                    Some(lun.into())
                }
                None => {
                    anyhow::bail!("unknown controller: '{controller}'");
                }
            },
            DiskLocation::VirtioBlk(pcie_port) => {
                if vtl != DeviceVtl::Vtl0 {
                    anyhow::bail!("virtio-blk only supported for VTL0");
                }
                if is_dvd {
                    anyhow::bail!("dvd not supported with virtio-blk");
                }
                let vblk = VirtioBlkDisk { disk, read_only };
                if let Some(port) = pcie_port {
                    self.pcie_virtio_blk_disks.push((port, vblk));
                } else {
                    self.vtl0_virtio_blk_disks.push(vblk);
                }
                None
            }
        };
        Ok(location)
    }

    /// Add a disk relayed through OpenHCL. The disk is added to the source
    /// controller (identified by `target`) and an OpenHCL LUN entry is
    /// created on the relay target controller.
    async fn add_relay(
        &mut self,
        relay: RelayTarget,
        target: DiskLocation,
        kind: &DiskCliKind,
        is_dvd: bool,
        read_only: bool,
    ) -> anyhow::Result<()> {
        // Look up the source controller to determine VTL and instance ID.
        let (source_vtl, device_type, device_path) = match &target {
            DiskLocation::Named { controller, .. } => match self.controllers.get(controller) {
                Some(ControllerEntry::Nvme(nvme)) => {
                    let instance_id = match &nvme.transport {
                        NvmeControllerTransport::Vpci(guid) => *guid,
                        NvmeControllerTransport::Pcie(_) => {
                            anyhow::bail!("OpenHCL relay does not support PCIe source controllers");
                        }
                    };
                    (
                        nvme.vtl,
                        vtl2_settings_proto::physical_device::DeviceType::Nvme,
                        instance_id,
                    )
                }
                Some(ControllerEntry::Scsi(scsi)) => (
                    scsi.vtl,
                    vtl2_settings_proto::physical_device::DeviceType::Vscsi,
                    scsi.instance_id,
                ),
                None => {
                    anyhow::bail!("unknown source controller: '{controller}'");
                }
            },
            _ => {
                anyhow::bail!("`relay` requires a named source controller");
            }
        };

        // The relay model requires the source controller to be offered into
        // the OpenHCL VTL so that OpenHCL can intercept and re-expose it.
        let openhcl_vtl = self.openhcl_vtl.context("OpenHCL not configured")?;
        if source_vtl != openhcl_vtl {
            anyhow::bail!(
                "relay source controller must be assigned to {openhcl_vtl:?}, \
                 but it is assigned to {source_vtl:?}; add the `vtl2` option \
                 to the source controller"
            );
        }

        let sub_device_path = self
            .add_inner(source_vtl, target, kind, is_dvd, read_only)
            .await?
            .context("source device not supported by relay")?;

        // Look up the OpenHCL target controller and add the LUN.
        let openhcl = self
            .openhcl_controllers
            .get_mut(&relay.controller)
            .with_context(|| format!("unknown OpenHCL controller: '{}'", relay.controller))?;

        // NVMe namespace IDs are 1-based (0 is invalid per spec),
        // while SCSI LUNs are 0-based.
        let location = relay.location.unwrap_or_else(|| {
            let base = openhcl.luns.len() as u32;
            match openhcl.controller_type {
                OpenhclControllerType::Nvme => base + 1,
                OpenhclControllerType::Scsi => base,
            }
        });

        openhcl.luns.push(Lun {
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

    async fn add_underhill(
        &mut self,
        source: DiskLocation,
        target: DiskLocation,
        kind: &DiskCliKind,
        is_dvd: bool,
        read_only: bool,
    ) -> anyhow::Result<()> {
        let vtl = self.openhcl_vtl.context("OpenHCL not configured")?;
        let sub_device_path = self
            .add_inner(vtl, source.clone(), kind, is_dvd, read_only)
            .await?
            .context("source device not supported by OpenHCL")?;

        let (device_type, device_path) = match source {
            DiskLocation::Ide(_, _) => anyhow::bail!("ide source not supported for OpenHCL"),
            DiskLocation::Scsi(_) => (
                vtl2_settings_proto::physical_device::DeviceType::Vscsi,
                if vtl == DeviceVtl::Vtl2 {
                    SCSI_VTL2_INSTANCE_ID
                } else {
                    SCSI_VTL0_INSTANCE_ID
                },
            ),
            DiskLocation::Nvme(_) => (
                vtl2_settings_proto::physical_device::DeviceType::Nvme,
                if vtl == DeviceVtl::Vtl2 {
                    NVME_VTL2_INSTANCE_ID
                } else {
                    NVME_VTL0_INSTANCE_ID
                },
            ),
            DiskLocation::VirtioBlk(_) => {
                anyhow::bail!("OpenHCL relay not supported with virtio-blk")
            }
            DiskLocation::Named { .. } => {
                anyhow::bail!("use `relay` instead of `uh` with named controllers")
            }
        };

        let (luns, location) = match target {
            DiskLocation::Ide(_, _) => {
                anyhow::bail!("ide target currently not supported for OpenHCL (no PCAT support)")
            }
            DiskLocation::Scsi(lun) => {
                let lun = lun.unwrap_or(self.underhill_scsi_luns.len() as u8);
                (&mut self.underhill_scsi_luns, lun.into())
            }
            DiskLocation::Nvme(nsid) => {
                let nsid = nsid.unwrap_or(self.underhill_nvme_luns.len() as u32 + 1);
                (&mut self.underhill_nvme_luns, nsid)
            }
            DiskLocation::VirtioBlk(_) => {
                anyhow::bail!("OpenHCL relay not supported with virtio-blk")
            }
            DiskLocation::Named { .. } => {
                anyhow::bail!("use `relay` instead of `uh` with named controllers")
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

        // Emit named controllers.
        for (name, entry) in std::mem::take(&mut self.controllers) {
            match entry {
                ControllerEntry::Scsi(ctrl) => {
                    config.vmbus_devices.push((
                        ctrl.vtl,
                        ScsiControllerHandle {
                            instance_id: ctrl.instance_id,
                            max_sub_channel_count: ctrl.sub_channels,
                            devices: ctrl.devices,
                            io_queue_depth: None,
                            requests: None,
                            poll_mode_queue_depth: None,
                        }
                        .into_resource(),
                    ));
                }
                ControllerEntry::Nvme(ctrl) => {
                    let subsystem_id = deterministic_guid(&name);
                    match ctrl.transport {
                        NvmeControllerTransport::Pcie(port_name) => {
                            config.pcie_devices.push(PcieDeviceConfig {
                                port_name,
                                resource: NvmeControllerHandle {
                                    subsystem_id,
                                    namespaces: ctrl.namespaces,
                                    max_io_queues: 64,
                                    msix_count: 64,
                                    requests: ctrl.requests,
                                }
                                .into_resource(),
                            });
                        }
                        NvmeControllerTransport::Vpci(instance_id) => {
                            config.vpci_devices.push(VpciDeviceConfig {
                                vtl: ctrl.vtl,
                                instance_id,
                                resource: NvmeControllerHandle {
                                    subsystem_id,
                                    namespaces: ctrl.namespaces,
                                    max_io_queues: 64,
                                    msix_count: 64,
                                    requests: ctrl.requests,
                                }
                                .into_resource(),
                                vnode: None,
                            });

                            // Tell UEFI to try to enumerate VPCI devices since there
                            // might be an NVMe namespace to boot from.
                            if let LoadMode::Uefi {
                                enable_vpci_boot: vpci_boot,
                                ..
                            } = &mut config.load_mode
                            {
                                *vpci_boot = true;
                            }
                        }
                    }
                }
            }
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
                vnode: None,
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
                vnode: None,
            });
            resources.nvme_vtl2_rpc = Some(send);
        }

        for (i, vblk) in std::mem::take(&mut self.vtl0_virtio_blk_disks)
            .into_iter()
            .enumerate()
        {
            let mut instance_id = VIRTIO_BLK_INSTANCE_ID_TEMPLATE;
            instance_id.data1 = i as u32;
            config.vpci_devices.push(VpciDeviceConfig {
                vtl: DeviceVtl::Vtl0,
                instance_id,
                resource: VirtioPciDeviceHandle(
                    VirtioBlkHandle {
                        disk: vblk.disk,
                        read_only: vblk.read_only,
                    }
                    .into_resource(),
                )
                .into_resource(),
                vnode: None,
            });
        }

        for (port_name, vblk) in std::mem::take(&mut self.pcie_virtio_blk_disks) {
            config.pcie_devices.push(PcieDeviceConfig {
                port_name,
                resource: VirtioPciDeviceHandle(
                    VirtioBlkHandle {
                        disk: vblk.disk,
                        read_only: vblk.read_only,
                    }
                    .into_resource(),
                )
                .into_resource(),
            });
        }

        Ok(())
    }

    /// Generate VTL2 settings for storage devices offered to the guest via
    /// OpenHCL.
    pub fn build_openhcl_settings(&mut self, vmbus_redirect: bool) -> Vec<StorageController> {
        let mut storage_controllers = Vec::new();

        // Legacy implicit controllers.
        if !self.underhill_scsi_luns.is_empty() || vmbus_redirect {
            storage_controllers.push(StorageController {
                instance_id: UNDERHILL_VTL0_SCSI_INSTANCE.to_string(),
                protocol: storage_controller::StorageProtocol::Scsi.into(),
                luns: std::mem::take(&mut self.underhill_scsi_luns),
                io_queue_depth: None,
            });
        }

        if !self.underhill_nvme_luns.is_empty() {
            storage_controllers.push(StorageController {
                instance_id: UNDERHILL_VTL0_NVME_INSTANCE.to_string(),
                protocol: storage_controller::StorageProtocol::Nvme.into(),
                luns: std::mem::take(&mut self.underhill_nvme_luns),
                io_queue_depth: None,
            });
        }

        // Named OpenHCL controllers.
        for (_name, ctrl) in std::mem::take(&mut self.openhcl_controllers) {
            let protocol = match ctrl.controller_type {
                OpenhclControllerType::Scsi => storage_controller::StorageProtocol::Scsi,
                OpenhclControllerType::Nvme => storage_controller::StorageProtocol::Nvme,
            };
            storage_controllers.push(StorageController {
                instance_id: ctrl.instance_id.to_string(),
                protocol: protocol.into(),
                luns: ctrl.luns,
                io_queue_depth: None,
            });
        }

        storage_controllers
    }
}
