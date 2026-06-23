// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VFIO container manager — shares containers across assigned devices.
//!
//! Instead of creating a separate VFIO container (and duplicate IOMMU page
//! tables) for every assigned device, this module manages a pool of containers
//! and reuses them across devices whose IOMMU groups are compatible.

// UNSAFETY: Implementing unsafe DmaTarget::map_dma for VFIO type1 IOMMU.
#![expect(unsafe_code)]

use anyhow::Context as _;
use inspect::{Inspect, InspectMut};
use membacking::DmaMapperClient;
use mesh::rpc::FailableRpc;
use mesh::rpc::RpcSend as _;
use pal_async::task::Spawn as _;
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::prelude::*;
use std::sync::Arc;

/// Implements [`membacking::DmaTarget`] for VFIO type1 IOMMU containers.
///
/// Translates sub-mapping events from the region manager into VFIO
/// `map_dma`/`unmap_dma` ioctls. The host VA needed for `pin_user_pages`
/// is provided by the region manager's `DmaMapper` wrapper.
struct VfioType1DmaTarget {
    container: Arc<vfio_sys::Container>,
}

impl membacking::DmaTarget for VfioType1DmaTarget {
    unsafe fn map_dma(&self, request: membacking::DmaMapRequest<'_>) -> anyhow::Result<()> {
        let vaddr = request
            .host_va
            .expect("VFIO type1 requires host VA (registered with needs_va=true)");
        let range = request.range;
        let _span = tracing::info_span!("vfio map", %range).entered();
        // SAFETY: The caller (DmaMapper in membacking) guarantees that the
        // host VA is backed and stable via eager mapping + VaMapper lifetime.
        let result = unsafe {
            self.container
                .map_dma(range.start(), vaddr, range.len(), request.writable)
                .context("VFIO DMA map failed")
        };
        if let Err(e) = &result {
            if request.mapping_type == membacking::MappingType::Device {
                // Device BAR memory may not be mappable into the IOMMU (e.g.,
                // if the kernel cannot pin device MMIO pages). This is not
                // fatal — it only means P2P DMA to this BAR won't work.
                tracelimit::warn_ratelimited!(
                    error = e.as_ref() as &dyn std::error::Error,
                    %range,
                    "failed to map device memory into VFIO container; \
                     P2P DMA to this region will not work"
                );
                return Ok(());
            }
        }
        result
    }

    fn unmap_dma(&self, range: memory_range::MemoryRange) -> anyhow::Result<()> {
        let _span = tracing::info_span!("vfio unmap", %range).entered();
        self.container
            .unmap_dma(range.start(), range.len())
            .context("VFIO DMA unmap failed")
    }
}

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
    /// Handle to the DMA mapper registration — removes the mapper from
    /// the region manager when dropped, unmapping all IOMMU entries.
    _dma_handle: membacking::DmaMapperHandle,
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
    /// Client for registering VFIO containers as DMA mappers.
    #[inspect(skip)]
    dma_mapper_client: DmaMapperClient,
    #[inspect(skip)]
    recv: mesh::Receiver<VfioManagerRpc>,
}

/// Handle for inspecting VFIO container manager state.
///
/// Inspecting this sends a deferred inspect request to the container manager
/// task, which reports the container/group/device topology.
#[derive(Clone, Inspect)]
pub struct VfioManagerClient {
    #[inspect(flatten, send = "VfioManagerRpc::Inspect")]
    sender: mesh::Sender<VfioManagerRpc>,
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

impl VfioContainerManager {
    /// Create a new container manager.
    pub fn new(dma_mapper_client: DmaMapperClient) -> Self {
        Self {
            containers: Vec::new(),
            groups: HashMap::new(),
            devices: Vec::new(),
            next_device_id: 0,
            next_container_id: 0,
            dma_mapper_client,
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

    /// Create a new container, set IOMMU type, register with the region
    /// manager for dynamic DMA mapping, and attach the group. Returns the
    /// container ID.
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

        let container = Arc::new(container);

        let dma_target: Arc<dyn membacking::DmaTarget> = Arc::new(VfioType1DmaTarget {
            container: container.clone(),
        });

        // Register as a DMA mapper — the region manager will create a
        // VaMapper internally (since needs_va is true) and replay all
        // existing active sub-mappings (guest RAM + any active device
        // BARs) into this container's IOMMU.
        let dma_handle = self
            .dma_mapper_client
            .add_dma_mapper(dma_target, true)
            .await
            .context("failed to register VFIO container with region manager")?;

        tracing::info!(
            pci_id,
            group_id,
            container_count = self.containers.len() + 1,
            "created new VFIO container"
        );

        let id = self.next_container_id;
        self.next_container_id += 1;
        self.containers.push(ContainerEntry {
            id,
            container,
            _dma_handle: dma_handle,
        });
        Ok(id)
    }

    pub(crate) fn client(&mut self) -> VfioManagerClient {
        VfioManagerClient {
            sender: self.recv.sender(),
        }
    }
}

// --- iommufd / cdev support ---

/// Implements [`membacking::DmaTarget`] for iommufd IOAS-based DMA mapping.
///
/// Like `VfioType1DmaTarget`, this uses host virtual addresses for mapping,
/// but calls `IOMMU_IOAS_MAP`/`IOMMU_IOAS_UNMAP` on the iommufd fd instead
/// of `VFIO_IOMMU_MAP_DMA`/`VFIO_IOMMU_UNMAP_DMA` on a VFIO container fd.
struct IommufdDmaTarget {
    ctx: Arc<vfio_sys::iommufd::IommufdCtx>,
    ioas_id: u32,
}

impl membacking::DmaTarget for IommufdDmaTarget {
    unsafe fn map_dma(&self, request: membacking::DmaMapRequest<'_>) -> anyhow::Result<()> {
        let vaddr = request
            .host_va
            .expect("iommufd IOAS map requires host VA (registered with needs_va=true)");
        let range = request.range;
        let iova = range.start();
        let user_va = vaddr as u64;
        let length = range.len();
        // SAFETY: The caller (DmaMapper in membacking) guarantees that the
        // host VA is backed and stable via eager mapping + VaMapper lifetime.
        let result = unsafe {
            self.ctx
                .ioas_map(self.ioas_id, iova, user_va, length, request.writable)
                .with_context(|| {
                    format!(
                        "iommufd IOAS DMA map failed: iova={iova:#x} user_va={user_va:#x} \
                         length={length:#x} ioas_id={}",
                        self.ioas_id
                    )
                })
        };
        if let Err(e) = &result {
            if request.mapping_type == membacking::MappingType::Device {
                // Device BAR memory may not be mappable into the IOMMU (e.g.,
                // if the kernel cannot pin device MMIO pages). This is not
                // fatal — it only means P2P DMA to this BAR won't work.
                tracelimit::warn_ratelimited!(
                    error = e.as_ref() as &dyn std::error::Error,
                    %range,
                    "failed to map device memory into iommufd IOAS; \
                     P2P DMA to this region will not work"
                );
                return Ok(());
            }
        }
        result
    }

    fn unmap_dma(&self, range: memory_range::MemoryRange) -> anyhow::Result<()> {
        let _span = tracing::info_span!("iommufd unmap", %range).entered();
        self.ctx
            .ioas_unmap(self.ioas_id, range.start(), range.len())
            .context("iommufd IOAS DMA unmap failed")?;
        Ok(())
    }
}

// --- Per-iommu-context manager (IoasManager) ---

/// RPC messages for a per-iommu [`IoasManager`] task.
pub(crate) enum IoasManagerRpc {
    /// Bind and attach a cdev device to this manager's IOAS.
    PrepareDevice {
        pci_id: String,
        cdev: File,
        /// The response half of the original RPC from the resolver.
        respond: FailableRpc<(), CdevPrepareResponse>,
    },
    /// Notify that a device has been dropped.
    RemoveDevice(u64),
    /// Inspect.
    Inspect(inspect::Deferred),
}

/// Manages a single iommufd IOAS context for one `--iommu` instance.
///
/// Each `--iommu id=<name>` gets its own `IoasManager` task, which owns
/// the iommufd context, IOAS, and DMA mapper registration. Devices
/// referencing the same `--iommu` ID share one IOAS — one set of IOMMU
/// page tables, one DMA mapper registration. Devices on different
/// `--iommu` IDs are handled by separate `IoasManager` tasks concurrently.
#[derive(Inspect)]
struct IoasManager {
    iommu_id: String,
    #[inspect(skip)]
    ctx: Arc<vfio_sys::iommufd::IommufdCtx>,
    ioas_id: u32,
    /// Keeps the DMA mapper registered with the region manager.
    #[inspect(skip)]
    _dma_handle: membacking::DmaMapperHandle,
    /// Active devices on this IOAS.
    #[inspect(with = "|x| inspect::iter_by_key(x.iter().map(|d| (&d.pci_id, ())))")]
    devices: Vec<CdevDeviceEntry>,
    /// Next device ID (unique within this manager).
    #[inspect(skip)]
    next_device_id: u64,
    #[inspect(skip)]
    recv: mesh::Receiver<IoasManagerRpc>,
}

/// Tracks a cdev device for inspect and cleanup.
struct CdevDeviceEntry {
    id: u64,
    pci_id: String,
}

impl IoasManager {
    /// Create and initialize a new per-iommu manager.
    ///
    /// Allocates an IOAS on the given iommufd fd and registers it with
    /// the region manager for DMA mapping.
    async fn new(
        iommu_id: String,
        iommufd: File,
        dma_mapper_client: &DmaMapperClient,
        recv: mesh::Receiver<IoasManagerRpc>,
    ) -> anyhow::Result<Self> {
        let ctx = Arc::new(vfio_sys::iommufd::IommufdCtx::from_file(iommufd));
        let ioas_id = ctx
            .ioas_alloc()
            .context("failed to allocate iommufd IOAS")?;

        let dma_target: Arc<dyn membacking::DmaTarget> = Arc::new(IommufdDmaTarget {
            ctx: ctx.clone(),
            ioas_id,
        });
        let dma_handle = dma_mapper_client
            .add_dma_mapper(dma_target, true)
            .await
            .context("failed to register iommufd IOAS with region manager")?;

        tracing::info!(iommu_id, ioas_id, "created iommufd IOAS for iommu context");

        Ok(Self {
            iommu_id,
            ctx,
            ioas_id,
            _dma_handle: dma_handle,
            devices: Vec::new(),
            next_device_id: 0,
            recv,
        })
    }

    /// Run the per-iommu manager task, processing RPCs until the channel
    /// closes.
    async fn run(mut self) {
        while let Ok(rpc) = self.recv.recv().await {
            match rpc {
                IoasManagerRpc::PrepareDevice {
                    pci_id,
                    cdev,
                    respond,
                } => {
                    respond
                        .handle_failable(async |()| self.prepare_device(pci_id, cdev))
                        .await
                }
                IoasManagerRpc::RemoveDevice(device_id) => {
                    self.remove_device(device_id);
                }
                IoasManagerRpc::Inspect(deferred) => deferred.inspect(&self),
            }
        }
    }

    fn prepare_device(
        &mut self,
        pci_id: String,
        cdev_file: File,
    ) -> anyhow::Result<CdevPrepareResponse> {
        let cdev = vfio_sys::cdev::CdevDevice::from_file(cdev_file);

        // Bind the cdev device to this iommu context's iommufd.
        let devid = cdev
            .bind_iommufd(self.ctx.as_raw_fd())
            .context("failed to bind VFIO cdev to iommufd")?;

        // Attach the device to the shared IOAS.
        cdev.attach_ioas(self.ioas_id)
            .context("failed to attach cdev device to IOAS")?;

        let device_id = self.next_device_id;
        self.next_device_id += 1;

        self.devices.push(CdevDeviceEntry {
            id: device_id,
            pci_id: pci_id.clone(),
        });

        tracing::info!(
            pci_id,
            iommu_id = self.iommu_id,
            iommufd_devid = devid,
            ioas_id = self.ioas_id,
            device_id,
            "VFIO cdev device attached to IOAS"
        );

        Ok(CdevPrepareResponse {
            device: cdev.into_device(),
            iommufd_devid: devid,
            ioas_id: self.ioas_id,
            device_id,
            manager_send: self.recv.sender(),
        })
    }

    fn remove_device(&mut self, device_id: u64) {
        if let Some(pos) = self.devices.iter().position(|d| d.id == device_id) {
            let entry = self.devices.swap_remove(pos);
            tracing::info!(
                device_id,
                pci_id = entry.pci_id,
                iommu_id = self.iommu_id,
                "removing cdev device"
            );
        }
    }
}

// --- Cdev dispatcher (VfioCdevManager) ---

/// RPC messages for the cdev dispatcher.
pub(crate) enum VfioCdevManagerRpc {
    /// Bind a cdev device to an IOAS, spawning a per-iommu manager if
    /// this is the first device for the given iommu ID.
    PrepareDevice(FailableRpc<CdevPrepareRequest, CdevPrepareResponse>),
    /// Inspect.
    Inspect(inspect::Deferred),
}

/// Request payload for `PrepareDevice`.
pub(crate) struct CdevPrepareRequest {
    pub pci_id: String,
    pub cdev: File,
    pub iommufd: File,
    pub iommu_id: String,
}

/// Response payload for `PrepareDevice`.
pub(crate) struct CdevPrepareResponse {
    pub device: vfio_sys::Device,
    pub iommufd_devid: u32,
    pub ioas_id: u32,
    pub device_id: u64,
    /// Sender to the per-iommu manager for drop notification.
    pub manager_send: mesh::Sender<IoasManagerRpc>,
}

/// Dispatches cdev device requests to per-iommu [`IoasManager`] tasks.
///
/// Unlike the legacy [`VfioContainerManager`] which makes cross-device
/// sharing decisions, the cdev dispatcher simply routes each device to
/// the manager for its `--iommu` ID. Each per-iommu manager runs as a
/// separate task, so devices on different `--iommu` contexts are
/// prepared concurrently.
pub(crate) struct VfioCdevManager {
    /// Per-iommu manager senders, keyed by `--iommu` ID.
    managers: HashMap<String, mesh::Sender<IoasManagerRpc>>,
    /// DMA mapper client, cloned for each new per-iommu manager.
    dma_mapper_client: DmaMapperClient,
    /// Spawner for per-iommu manager tasks.
    spawner: Arc<dyn pal_async::task::Spawn>,
    /// Per-iommu manager tasks (kept alive).
    tasks: Vec<pal_async::task::Task<()>>,
    recv: mesh::Receiver<VfioCdevManagerRpc>,
}

/// Client handle for the `VfioCdevManager` dispatcher.
#[derive(Clone, Inspect)]
pub struct VfioCdevManagerClient {
    #[inspect(flatten, send = "VfioCdevManagerRpc::Inspect")]
    sender: mesh::Sender<VfioCdevManagerRpc>,
}

impl VfioCdevManagerClient {
    pub(crate) async fn prepare_device(
        &self,
        req: CdevPrepareRequest,
    ) -> anyhow::Result<CdevPrepareResponse> {
        Ok(self
            .sender
            .call_failable(VfioCdevManagerRpc::PrepareDevice, req)
            .await?)
    }
}

impl VfioCdevManager {
    /// Create a new cdev dispatcher.
    pub fn new(
        spawner: Arc<dyn pal_async::task::Spawn>,
        dma_mapper_client: DmaMapperClient,
    ) -> Self {
        Self {
            managers: HashMap::new(),
            dma_mapper_client,
            spawner,
            tasks: Vec::new(),
            recv: mesh::Receiver::new(),
        }
    }

    /// Run the dispatcher, routing device requests to per-iommu managers.
    pub async fn run(mut self) {
        while let Ok(rpc) = self.recv.recv().await {
            match rpc {
                VfioCdevManagerRpc::PrepareDevice(rpc) => {
                    let (req, respond) = rpc.split();
                    self.route_prepare(req, respond).await;
                }
                VfioCdevManagerRpc::Inspect(deferred) => {
                    deferred.respond(|resp| {
                        for (iommu_id, sender) in &self.managers {
                            resp.child(iommu_id, |req| {
                                sender.send(IoasManagerRpc::Inspect(req.defer()));
                            });
                        }
                    });
                }
            }
        }
    }

    /// Route a prepare request to the per-iommu manager, spawning one
    /// if needed. Initializes the per-iommu manager inline on first use
    /// so that init failures are reported directly to the caller.
    ///
    /// The actual bind/attach ioctls are forwarded to the per-iommu
    /// manager task via fire-and-forget send, so the dispatcher is
    /// immediately free to handle the next request. This allows devices
    /// on different `--iommu` contexts to be prepared concurrently.
    async fn route_prepare(
        &mut self,
        req: CdevPrepareRequest,
        respond: FailableRpc<(), CdevPrepareResponse>,
    ) {
        let CdevPrepareRequest {
            pci_id,
            cdev,
            iommufd,
            iommu_id,
        } = req;

        let sender = match self.managers.entry(iommu_id.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let mut ioas_recv: mesh::Receiver<IoasManagerRpc> = mesh::Receiver::new();
                let sender = ioas_recv.sender();

                let mgr = match IoasManager::new(
                    iommu_id.clone(),
                    iommufd,
                    &self.dma_mapper_client,
                    ioas_recv,
                )
                .await
                .with_context(|| {
                    format!("failed to initialize iommufd IOAS manager for iommu={iommu_id}")
                }) {
                    Ok(mgr) => mgr,
                    Err(e) => {
                        respond.fail(e);
                        return;
                    }
                };

                let task = self
                    .spawner
                    .spawn(format!("vfio-ioas-{iommu_id}"), mgr.run());
                self.tasks.push(task);
                e.insert(sender)
            }
        };

        // Forward to the per-iommu manager task. The manager will
        // complete the respond half after the bind/attach ioctls.
        sender.send(IoasManagerRpc::PrepareDevice {
            pci_id,
            cdev,
            respond,
        });
    }

    pub(crate) fn client(&mut self) -> VfioCdevManagerClient {
        VfioCdevManagerClient {
            sender: self.recv.sender(),
        }
    }
}

/// Binding for a VFIO device opened via the cdev + iommufd path.
///
/// Analogous to [`VfioDeviceBinding`] for the legacy group path.
/// Notifies the per-iommu manager on drop so device counts stay accurate.
#[derive(Inspect)]
pub(crate) struct VfioCdevBinding {
    /// PCI BDF address on the host.
    pci_id: String,
    /// VFIO cdev device — provides config space, BAR, IRQ ioctls.
    #[inspect(skip)]
    device: vfio_sys::Device,
    /// iommufd device ID (from `VFIO_DEVICE_BIND_IOMMUFD`).
    iommufd_devid: u32,
    /// IOAS ID this device is attached to.
    ioas_id: u32,
    /// Device ID assigned by the per-iommu manager (for drop notification).
    #[inspect(skip)]
    device_id: u64,
    /// Sender to the per-iommu manager for drop notification.
    #[inspect(skip)]
    manager_send: mesh::Sender<IoasManagerRpc>,
}

impl VfioCdevBinding {
    /// Create from a dispatcher response.
    pub(crate) fn from_response(resp: CdevPrepareResponse, pci_id: String) -> Self {
        Self {
            pci_id,
            device: resp.device,
            iommufd_devid: resp.iommufd_devid,
            ioas_id: resp.ioas_id,
            device_id: resp.device_id,
            manager_send: resp.manager_send,
        }
    }

    /// Consume the binding and split into the `Device` (for constructing
    /// `VfioAssignedPciDevice`) and the remaining binding state (for
    /// lifetime management). The state's `Drop` impl notifies the per-iommu
    /// manager when the device is released.
    pub fn into_parts(self) -> (vfio_sys::Device, VfioCdevBindingState) {
        let Self {
            pci_id,
            device,
            iommufd_devid,
            ioas_id,
            device_id,
            manager_send,
        } = self;
        (
            device,
            VfioCdevBindingState {
                pci_id,
                iommufd_devid,
                ioas_id,
                device_id,
                manager_send,
            },
        )
    }
}

/// The iommufd-related state from a [`VfioCdevBinding`], kept alive for
/// the lifetime of the assigned device.
///
/// Notifies the per-iommu manager on drop so device counts are accurate.
#[derive(Inspect)]
pub(crate) struct VfioCdevBindingState {
    pci_id: String,
    iommufd_devid: u32,
    ioas_id: u32,
    #[inspect(skip)]
    device_id: u64,
    #[inspect(skip)]
    manager_send: mesh::Sender<IoasManagerRpc>,
}

impl Drop for VfioCdevBindingState {
    fn drop(&mut self) {
        self.manager_send
            .send(IoasManagerRpc::RemoveDevice(self.device_id));
    }
}

/// Wrapper enum for either legacy group or cdev iommufd binding.
///
/// Kept as a field on `VfioAssignedPciDevice` to hold the underlying
/// fd/handle resources alive for the device's lifetime.
#[derive(Inspect)]
#[inspect(external_tag)]
pub(crate) enum VfioBinding {
    Group(VfioDeviceBinding),
    Cdev(VfioCdevBindingState),
}
