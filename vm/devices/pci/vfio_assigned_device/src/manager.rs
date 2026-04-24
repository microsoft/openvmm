// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VFIO container manager — shares containers across assigned devices.
//!
//! Instead of creating a separate VFIO container (and duplicate IOMMU page
//! tables) for every assigned device, this module manages a pool of containers
//! and reuses them across devices whose IOMMU groups are compatible.

use anyhow::Context as _;
use guestmem::GuestMemory;
use inspect::{Inspect, InspectMut};
use mesh::rpc::FailableRpc;
use mesh::rpc::RpcSend as _;
use std::collections::HashMap;
use std::fs::File;
use std::sync::Arc;

/// RPC messages for the container manager task.
enum VfioManagerRpc {
    /// Prepare a container and group for a device, creating or reusing
    /// containers as needed. Returns a [`VfioDeviceBinding`] directly.
    ///
    /// Takes `(pci_id, group_file)` where `group_file` is a pre-opened
    /// `/dev/vfio/<group_id>` file descriptor.
    PrepareDevice(FailableRpc<(String, File), VfioDeviceBinding>),
    /// Notify that a device has been removed (fire-and-forget from Drop).
    RemoveDevice(u64),
    /// Inspect the container/group topology.
    Inspect(inspect::Deferred),
}

/// Owns the VFIO container, group, and manager channel for a single assigned
/// device. Notifies the container manager on drop so inspect stays accurate.
///
/// Fields are ordered so that the group drops before the container (Rust drops
/// fields in declaration order).
#[derive(Inspect)]
pub(crate) struct VfioDeviceBinding {
    #[inspect(skip)]
    device_id: u64,
    #[inspect(skip)]
    sender: mesh::Sender<VfioManagerRpc>,
    /// VFIO group handle — drops before container.
    #[inspect(skip)]
    group: Arc<vfio_sys::Group>,
    /// VFIO container handle — shared across devices.
    #[inspect(skip)]
    _container: Arc<vfio_sys::Container>,
    /// Container index — for inspect only.
    container_id: u64,
    /// IOMMU group ID — for inspect only.
    group_id: u64,
}

impl Drop for VfioDeviceBinding {
    fn drop(&mut self) {
        self.sender
            .send(VfioManagerRpc::RemoveDevice(self.device_id));
    }
}

impl VfioDeviceBinding {
    pub fn group(&self) -> &vfio_sys::Group {
        &self.group
    }
}

struct ContainerEntry {
    id: u64,
    container: Arc<vfio_sys::Container>,
}

/// Manages VFIO containers and groups, sharing containers across devices.
#[derive(InspectMut)]
#[inspect(extra = "Self::inspect_topology")]
pub(crate) struct VfioContainerManager {
    /// Active containers.
    #[inspect(skip)]
    containers: Vec<ContainerEntry>,
    /// Open groups keyed by IOMMU group ID.
    #[inspect(skip)]
    groups: HashMap<u64, GroupEntry>,
    /// Active devices.
    #[inspect(skip)]
    devices: Vec<DeviceEntry>,
    /// Next device ID to assign.
    #[inspect(skip)]
    next_device_id: u64,
    /// Next container ID to assign.
    #[inspect(skip)]
    next_container_id: u64,
    /// Guest memory handle for DMA mapping.
    guest_memory: GuestMemory,
    /// Cached guest memory regions + base VA, fetched lazily.
    #[inspect(skip)]
    dma_info: Option<DmaInfo>,
    #[inspect(skip)]
    recv: mesh::Receiver<VfioManagerRpc>,
}

/// Handle for inspecting VFIO container manager state.
///
/// Inspecting this sends a deferred inspect request to the container manager
/// task, which reports the container/group/device topology.
#[derive(Clone)]
pub struct VfioManagerClient {
    sender: mesh::Sender<VfioManagerRpc>,
}

impl Inspect for VfioManagerClient {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.sender.send(VfioManagerRpc::Inspect(req.defer()));
    }
}

impl VfioManagerClient {
    pub(crate) async fn prepare_device(
        &self,
        pci_id: String,
        group_file: File,
    ) -> anyhow::Result<VfioDeviceBinding> {
        Ok(self
            .sender
            .call_failable(VfioManagerRpc::PrepareDevice, (pci_id, group_file))
            .await?)
    }
}

/// Tracks a registered device for inspect and removal.
struct DeviceEntry {
    id: u64,
    pci_id: String,
    group_id: u64,
    container_id: u64,
}

struct GroupEntry {
    group: Arc<vfio_sys::Group>,
    container_id: u64,
}

struct DmaInfo {
    regions: Vec<guestmem::ShareableRegion>,
    base_va: u64,
    va_size: u64,
}

impl VfioContainerManager {
    /// Create a new container manager.
    pub fn new(guest_memory: GuestMemory) -> Self {
        Self {
            containers: Vec::new(),
            groups: HashMap::new(),
            devices: Vec::new(),
            next_device_id: 0,
            next_container_id: 0,
            guest_memory,
            dma_info: None,
            recv: mesh::Receiver::new(),
        }
    }

    /// Run the container manager task, processing RPCs until the channel
    /// closes.
    pub async fn run(mut self) {
        while let Ok(rpc) = self.recv.recv().await {
            match rpc {
                VfioManagerRpc::PrepareDevice(rpc) => {
                    rpc.handle_failable(async |(pci_id, group_file)| {
                        self.prepare_device(pci_id, group_file).await
                    })
                    .await
                }
                VfioManagerRpc::RemoveDevice(device_id) => {
                    self.remove_device(device_id);
                }
                VfioManagerRpc::Inspect(deferred) => deferred.inspect(&mut self),
            }
        }
    }

    fn remove_device(&mut self, device_id: u64) {
        if let Some(pos) = self.devices.iter().position(|d| d.id == device_id) {
            let entry = self.devices.swap_remove(pos);
            tracing::info!(
                device_id,
                pci_id = entry.pci_id,
                group_id = entry.group_id,
                container_id = entry.container_id,
                "removing VFIO device"
            );

            // If no more devices reference this group, close it.
            let group_has_devices = self.devices.iter().any(|d| d.group_id == entry.group_id);
            if !group_has_devices {
                if let Some(removed) = self.groups.remove(&entry.group_id) {
                    tracing::info!(
                        group_id = entry.group_id,
                        "closing VFIO group (no remaining devices)"
                    );

                    // If no more groups reference this container, release it.
                    let container_has_groups = self
                        .groups
                        .values()
                        .any(|g| g.container_id == removed.container_id);
                    if !container_has_groups {
                        tracing::info!(
                            container_id = removed.container_id,
                            "closing VFIO container (no remaining groups)"
                        );
                        self.containers.retain(|c| c.id != removed.container_id);
                    }
                }
            }
        }
    }

    /// Allocate a device ID and register the device.
    fn register_device(&mut self, pci_id: String, group_id: u64, container_id: u64) -> u64 {
        let id = self.next_device_id;
        self.next_device_id += 1;
        self.devices.push(DeviceEntry {
            id,
            pci_id,
            group_id,
            container_id,
        });
        id
    }

    fn inspect_topology(&self, resp: &mut inspect::Response<'_>) {
        resp.child("container", |req| {
            let mut resp = req.respond();
            for ce in &self.containers {
                resp.child(&ce.id.to_string(), |req| {
                    let mut resp = req.respond();
                    resp.child("group", |req| {
                        let mut resp = req.respond();
                        for (&gid, entry) in &self.groups {
                            if entry.container_id == ce.id {
                                resp.child(&gid.to_string(), |req| {
                                    let mut resp = req.respond();
                                    resp.child("device", |req| {
                                        let mut resp = req.respond();
                                        for dev in &self.devices {
                                            if dev.group_id == gid {
                                                resp.field(&dev.pci_id, ());
                                            }
                                        }
                                    });
                                });
                            }
                        }
                    });
                });
            }
        });
    }

    async fn prepare_device(
        &mut self,
        pci_id: String,
        group_file: File,
    ) -> anyhow::Result<VfioDeviceBinding> {
        use std::os::unix::io::AsRawFd;

        tracing::info!(pci_id, "container manager: preparing VFIO device");

        // Resolve the VFIO group number from the fd path (e.g.
        // /proc/self/fd/N → /dev/vfio/42 → 42).
        let fd_path = std::fs::read_link(format!("/proc/self/fd/{}", group_file.as_raw_fd()))
            .context("failed to readlink VFIO group fd")?;
        let group_id: u64 = fd_path
            .file_name()
            .and_then(|n| n.to_str())
            .context("VFIO group fd path has no filename")?
            .parse()
            .with_context(|| format!("VFIO group fd path {:?} is not a group number", fd_path))?;

        // Group dedup: if this IOMMU group is already open, return the
        // existing group and its container.
        if let Some(entry) = self.groups.get(&group_id) {
            tracing::info!(
                pci_id,
                group_id,
                "reusing existing VFIO group and container"
            );
            let container_id = entry.container_id;
            let group = entry.group.clone();
            let container = self
                .find_container(container_id)
                .expect("container still active while group exists")
                .clone();
            let device_id = self.register_device(pci_id, group_id, container_id);
            return Ok(VfioDeviceBinding {
                device_id,
                sender: self.recv.sender(),
                group,
                _container: container,
                container_id,
                group_id,
            });
        }

        let group = vfio_sys::Group::from_file(group_file);

        anyhow::ensure!(
            group
                .status()
                .context("failed to check VFIO group status")?
                .viable(),
            "VFIO group {group_id} is not viable \
             (all devices in the group must be bound to vfio-pci)"
        );

        // Try to attach to an existing container (QEMU-style sharing loop).
        let container_id = 'find: {
            for ce in &self.containers {
                match group.try_set_container(&ce.container)? {
                    true => {
                        tracing::info!(
                            pci_id,
                            group_id,
                            "attached group to existing VFIO container"
                        );
                        break 'find ce.id;
                    }
                    false => continue,
                }
            }
            // No existing container accepted this group — create a new one.
            self.create_container_for_group(&group, group_id, &pci_id)
                .await?
        };

        let group = Arc::new(group);
        let device_id = self.register_device(pci_id, group_id, container_id);
        self.groups.insert(
            group_id,
            GroupEntry {
                group: group.clone(),
                container_id,
            },
        );

        Ok(VfioDeviceBinding {
            device_id,
            sender: self.recv.sender(),
            group,
            _container: self
                .find_container(container_id)
                .expect("container just created or found")
                .clone(),
            container_id,
            group_id,
        })
    }

    fn find_container(&self, id: u64) -> Option<&Arc<vfio_sys::Container>> {
        self.containers
            .iter()
            .find(|c| c.id == id)
            .map(|c| &c.container)
    }

    /// Create a new container, set IOMMU type, map guest RAM, and attach the
    /// group. Returns the container ID.
    async fn create_container_for_group(
        &mut self,
        group: &vfio_sys::Group,
        group_id: u64,
        pci_id: &str,
    ) -> anyhow::Result<u64> {
        let container = vfio_sys::Container::new().context("failed to open VFIO container")?;

        group
            .set_container(&container)
            .context("failed to set VFIO container")?;

        container
            .set_iommu(vfio_sys::IommuType::Type1v2)
            .context("failed to set VFIO IOMMU type to Type1v2 (IOMMU required)")?;

        // Lazily fetch guest memory regions on first use.
        if self.dma_info.is_none() {
            self.dma_info = Some(self.fetch_dma_info().await?);
        }
        let dma_info = self.dma_info.as_ref().unwrap();

        // Identity-map guest RAM (IOVA == GPA) for device DMA access.
        for region in &dma_info.regions {
            let gpa = region.guest_address;
            let size = region.size;
            let gpa_end = gpa
                .checked_add(size)
                .context("guest memory region overflows u64")?;
            anyhow::ensure!(
                gpa_end <= dma_info.va_size,
                "guest memory region {:#x}..{:#x} exceeds mapping size {:#x}",
                gpa,
                gpa_end,
                dma_info.va_size
            );
            let vaddr = dma_info.base_va + gpa;
            container.map_dma(gpa, vaddr, size).with_context(|| {
                format!(
                    "failed to map DMA for guest memory region {:#x}..{:#x}",
                    gpa, gpa_end
                )
            })?;
            tracing::debug!(gpa, size, vaddr, "mapped guest RAM for VFIO DMA");
        }

        tracing::info!(
            pci_id,
            group_id,
            container_count = self.containers.len() + 1,
            "created new VFIO container"
        );

        let container = Arc::new(container);
        let id = self.next_container_id;
        self.next_container_id += 1;
        self.containers.push(ContainerEntry { id, container });
        Ok(id)
    }

    async fn fetch_dma_info(&self) -> anyhow::Result<DmaInfo> {
        let sharing = self
            .guest_memory
            .sharing()
            .context("VFIO requires shareable guest memory")?;

        let regions = sharing
            .get_regions()
            .await
            .map_err(|e| anyhow::anyhow!(e))
            .context("failed to get shareable guest memory regions")?;

        let (base_va, va_size) = self
            .guest_memory
            .full_mapping()
            .context("VFIO DMA mapping requires linearly mapped guest memory")?;

        Ok(DmaInfo {
            regions,
            base_va: base_va as u64,
            va_size: va_size as u64,
        })
    }

    pub(crate) fn client(&mut self) -> VfioManagerClient {
        VfioManagerClient {
            sender: self.recv.sender(),
        }
    }
}
