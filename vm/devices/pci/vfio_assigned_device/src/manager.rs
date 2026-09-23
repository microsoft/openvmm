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
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::prelude::*;
use std::sync::Arc;
use std::sync::Weak;
use vfio_sys::iommufd::ViommuAlloc;

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
        let vaddr = request.host_va;
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

        // Register as a DMA mapper. This target programs the IOMMU by host VA,
        // so it does not require a backing fd (needs_fd = false) and is
        // compatible with private RAM. The region manager replays all existing
        // active sub-mappings (guest RAM + any active device BARs) into this
        // container's IOMMU.
        let dma_handle = self
            .dma_mapper_client
            .add_dma_mapper(dma_target, false)
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

/// Intrinsic identity of a device BAR area used to key exported dmabufs.
///
/// `st_dev`/`st_ino` come from the VFIO cdev inode (disambiguating devices
/// that share one IOAS); `file_offset` is the BAR-region file offset that the
/// region manager stamps on the corresponding `Device` mapping. This is the
/// same value on both the exporter (BAR setup) and importer (`map_dma`) sides,
/// and — unlike a guest physical address — is stable across BAR moves and MMIO
/// enable/disable.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct DmaBufKey {
    st_dev: u64,
    st_ino: u64,
    file_offset: u64,
}

/// Per-IOAS registry mapping a device BAR area's intrinsic identity to an
/// exported VFIO dmabuf fd.
///
/// This lets `IommufdDmaTarget::map_dma` program peer-to-peer DMA to a device
/// BAR via `IOMMU_IOAS_MAP_FILE` (which pins the BAR's physical MMIO via the
/// PCI P2PDMA provider) instead of failing to pin MMIO pages by host VA. The
/// exporter (BAR setup in `lib.rs`) and the importer (`map_dma`) live in the
/// same crate and share one registry per IOAS via `Arc`; the legacy VFIO
/// type1 path has no registry, so dmabuf P2P is iommufd-only.
#[derive(Default)]
pub(crate) struct DmaBufRegistry {
    entries: Mutex<HashMap<DmaBufKey, OwnedFd>>,
}

impl DmaBufRegistry {
    /// Register an exported dmabuf for a BAR area, keyed by the exporting
    /// cdev's inode and the area's BAR-region file offset.
    pub(crate) fn register(&self, st_dev: u64, st_ino: u64, file_offset: u64, dmabuf: OwnedFd) {
        self.entries.lock().insert(
            DmaBufKey {
                st_dev,
                st_ino,
                file_offset,
            },
            dmabuf,
        );
    }

    /// Look up the dmabuf fd registered for a mapping's intrinsic identity and,
    /// while still holding the registry lock, invoke `f` with the raw fd.
    ///
    /// Holding the lock across `f` is what makes handing out a bare [`RawFd`]
    /// sound: [`Self::deregister_device`] (which drops the owning [`OwnedFd`],
    /// closing it) also takes this lock, so it cannot run — and the fd cannot
    /// be closed — for the duration of `f`. Callers therefore pass the ioctl
    /// that consumes the fd (e.g. `ioas_map_file`) as `f`.
    ///
    /// Returns `None` (without calling `f`) if no dmabuf is registered for the
    /// key, in which case the caller falls back to the host-VA mapping path.
    fn with_lookup<R>(
        &self,
        st_dev: u64,
        st_ino: u64,
        file_offset: u64,
        f: impl FnOnce(RawFd) -> R,
    ) -> Option<R> {
        let entries = self.entries.lock();
        let fd = entries.get(&DmaBufKey {
            st_dev,
            st_ino,
            file_offset,
        })?;
        Some(f(fd.as_raw_fd()))
    }

    /// Remove (and close) all dmabufs registered for a device's cdev inode.
    fn deregister_device(&self, st_dev: u64, st_ino: u64) {
        self.entries
            .lock()
            .retain(|k, _| k.st_dev != st_dev || k.st_ino != st_ino);
    }
}

/// Implements [`membacking::DmaTarget`] for iommufd IOAS-based DMA mapping.
///
/// Like `VfioType1DmaTarget`, this uses host virtual addresses for mapping,
/// but calls `IOMMU_IOAS_MAP`/`IOMMU_IOAS_UNMAP` on the iommufd fd instead
/// of `VFIO_IOMMU_MAP_DMA`/`VFIO_IOMMU_UNMAP_DMA` on a VFIO container fd.
struct IommufdDmaTarget {
    ctx: Arc<vfio_sys::iommufd::IommufdCtx>,
    ioas_id: u32,
    /// Registry of exported device-BAR dmabufs, shared with the devices on
    /// this IOAS, used to program peer-to-peer DMA to BAR MMIO by file.
    dmabuf_registry: Arc<DmaBufRegistry>,
}

impl membacking::DmaTarget for IommufdDmaTarget {
    unsafe fn map_dma(&self, request: membacking::DmaMapRequest<'_>) -> anyhow::Result<()> {
        let vaddr = request.host_va;
        let range = request.range;
        let iova = range.start();
        let user_va = vaddr as u64;
        let length = range.len();
        // Prefer map-by-file where possible. Guest RAM is memfd-backed, so the
        // kernel can pin the folios directly from the fd (no host VA pinning).
        // A device BAR is backed by the VFIO cdev fd — which is neither a
        // memfd nor a dmabuf — so it can only be mapped by file if the device
        // exported a dmabuf for this BAR area, looked up by the area's
        // intrinsic identity (cdev inode + file offset). Everything else
        // (private/anonymous RAM, or a BAR without an exported dmabuf) uses the
        // host VA path.
        //
        // `by_file` is `None` when there is no fd to map by file, meaning the
        // host-VA fallback below is used.
        let by_file = match request.mapping_type {
            membacking::MappingType::Ram => request.mappable.map(|mappable| {
                self.ctx.ioas_map_file(
                    self.ioas_id,
                    iova,
                    mappable.as_fd().as_raw_fd(),
                    request.file_offset,
                    length,
                    request.writable,
                )
            }),
            membacking::MappingType::Device => match request.mappable {
                Some(mappable) => {
                    let (st_dev, st_ino) = vfio_sys::fd_identity(mappable.as_fd())
                        .context("failed to stat VFIO cdev for dmabuf lookup")?;
                    // Perform the `ioas_map_file` while the registry lock is
                    // held (inside `with_lookup`), so the dmabuf fd cannot be
                    // deregistered/closed between lookup and use. The dmabuf
                    // covers exactly this BAR area starting at its own byte 0,
                    // so map from offset 0.
                    self.dmabuf_registry
                        .with_lookup(st_dev, st_ino, request.file_offset, |fd| {
                            self.ctx.ioas_map_file(
                                self.ioas_id,
                                iova,
                                fd,
                                0,
                                length,
                                request.writable,
                            )
                        })
                }
                None => None,
            },
        };
        let result = match by_file {
            Some(r) => r,
            // SAFETY: The caller (DmaMapper in membacking) guarantees that the
            // host VA is backed and stable via eager mapping + VaMapper
            // lifetime, satisfying the safety contract of `ioas_map`.
            None => unsafe {
                self.ctx
                    .ioas_map(self.ioas_id, iova, user_va, length, request.writable)
            },
        }
        .with_context(|| format!("failed to map {range} into iommufd IOAS"));
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
    /// Bind and attach a cdev device to this manager's page table.
    PrepareDevice {
        pci_id: String,
        cdev: File,
        /// The emulated SMMU this device sits behind, when it is behind an
        /// accel-capable SMMU and needs iommufd nesting. Devices behind the
        /// same SMMU (`Arc::ptr_eq`) share one vIOMMU. `None` for the plain
        /// identity-DMA path.
        vsmmu: Option<Arc<smmu::SmmuSharedState>>,
        direct_hwpt_flags: u32,
        /// The response half of the original RPC from the resolver.
        respond: FailableRpc<(), CdevPrepareResponse>,
        /// Dispatcher that owns the association state and caller response.
        completion_send: mesh::Sender<VfioCdevManagerRpc>,
    },
    /// Notify that a device has been dropped.
    RemoveDevice(u64),
    /// Inspect.
    Inspect(inspect::Deferred),
}

#[derive(Inspect)]
struct IoasManager {
    iommu_id: String,
    #[inspect(skip)]
    ctx: Arc<vfio_sys::iommufd::IommufdCtx>,
    mode: IommuManagerMode,
    #[inspect(skip)]
    accel_states: Vec<AccelStateEntry>,
    /// Registry of exported device-BAR dmabufs for this IOAS, shared with the
    /// DMA target and each device for peer-to-peer DMA by file.
    #[inspect(skip)]
    dmabuf_registry: Arc<DmaBufRegistry>,
    /// Device state retained until cleanup succeeds.
    #[inspect(with = "|x| inspect::iter_by_key(x.iter().map(|d| (&d.pci_id, &d.direct)))")]
    devices: Vec<CdevDeviceEntry>,
    #[inspect(skip)]
    next_device_id: u64,
    /// Next accelerated-state ID (unique within this manager).
    #[inspect(skip)]
    next_accel_state_id: u64,
    /// Spawns and polls the per-vIOMMU vEVENTQ tasks.
    #[inspect(skip)]
    driver: Arc<dyn pal_async::driver::SpawnDriver>,
    #[inspect(skip)]
    recv: mesh::Receiver<IoasManagerRpc>,
}

#[derive(Inspect)]
#[inspect(external_tag)]
enum IommuManagerMode {
    /// Traditional iommufd mode: every VFIO cdev is attached to one IOAS.
    Ioas {
        ioas_id: u32,
        /// Keeps the DMA mapper registered with the region manager.
        #[inspect(skip)]
        _dma_handle: membacking::DmaMapperHandle,
    },
    /// Direct mode: the kernel owns the DMA translation for each direct HWPT.
    Direct {
        /// Held until the shared vIOMMU is destroyed.
        #[inspect(skip)]
        vm_fd: File,
        viommu_id: Option<u32>,
        /// Remains set until vIOMMU destruction succeeds.
        destroy_viommu_when_unused: bool,
    },
}

/// Tracks a cdev device for inspect and cleanup.
struct CdevDeviceEntry {
    id: u64,
    pci_id: String,
    accel_state_id: Option<u64>,
    /// Direct state retained until cleanup succeeds.
    direct: Option<DirectDeviceObjects>,
}

#[cfg(test)]
fn remove_cdev_device(
    devices: &mut Vec<CdevDeviceEntry>,
    device_id: u64,
) -> Option<(CdevDeviceEntry, Option<u64>)> {
    let pos = devices.iter().position(|device| device.id == device_id)?;
    let entry = devices.swap_remove(pos);
    let unused_accel_state = entry.accel_state_id.filter(|&accel_state_id| {
        !devices
            .iter()
            .any(|device| device.accel_state_id == Some(accel_state_id))
    });
    Some((entry, unused_accel_state))
}

/// Cache entry mapping an emulated SMMU to its host vIOMMU.
///
/// The manager owns the accelerated state and nesting parent until the last
/// device referencing `id` is removed. The vSMMU is weak so this cache does not
/// keep the chipset device alive.
struct AccelStateEntry {
    id: u64,
    vsmmu: Weak<smmu::SmmuSharedState>,
    accel: Arc<crate::iommufd_nesting::SmmuAccelState>,
    parent: Arc<crate::iommufd_nesting::NestingParent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectCleanupAction {
    Detach,
    DestroyHwpt(u32),
    DestroyVdevice(u32),
    Complete,
}

#[derive(Inspect)]
struct DirectDeviceObjects {
    /// Retained for explicit detach.
    #[inspect(skip)]
    device_file: File,
    vdevice_id: Option<u32>,
    hwpt_id: Option<u32>,
    attached: bool,
    virt_id: u64,
    removal_requested: bool,
    cleanup_error: Option<String>,
}

impl DirectDeviceObjects {
    fn new(device_file: File, virt_id: u64) -> Self {
        Self {
            device_file,
            vdevice_id: None,
            hwpt_id: None,
            attached: false,
            virt_id,
            removal_requested: false,
            cleanup_error: None,
        }
    }

    fn next_cleanup_action(&self) -> DirectCleanupAction {
        if self.attached {
            DirectCleanupAction::Detach
        } else if let Some(id) = self.hwpt_id {
            DirectCleanupAction::DestroyHwpt(id)
        } else if let Some(id) = self.vdevice_id {
            DirectCleanupAction::DestroyVdevice(id)
        } else {
            DirectCleanupAction::Complete
        }
    }

    fn mark_cleanup_success(&mut self, action: DirectCleanupAction) {
        match action {
            DirectCleanupAction::Detach => self.attached = false,
            DirectCleanupAction::DestroyHwpt(id) => {
                debug_assert_eq!(self.hwpt_id, Some(id));
                self.hwpt_id = None;
            }
            DirectCleanupAction::DestroyVdevice(id) => {
                debug_assert_eq!(self.vdevice_id, Some(id));
                self.vdevice_id = None;
            }
            DirectCleanupAction::Complete => {}
        }
        self.cleanup_error = None;
    }

    fn is_clean(&self) -> bool {
        self.next_cleanup_action() == DirectCleanupAction::Complete
    }
}

fn hyperv_logical_device_id(pci_id: &str) -> anyhow::Result<u64> {
    let (segment, rest) = pci_id.split_once(':').context("missing PCI segment")?;
    let (bus, rest) = rest.split_once(':').context("missing PCI bus")?;
    let (device, function) = rest.split_once('.').context("missing PCI function")?;
    let segment = u16::from_str_radix(segment, 16).context("invalid PCI segment")?;
    let bus = u8::from_str_radix(bus, 16).context("invalid PCI bus")?;
    let device = u8::from_str_radix(device, 16).context("invalid PCI device")?;
    let function = u8::from_str_radix(function, 16).context("invalid PCI function")?;
    anyhow::ensure!(device < 32, "PCI device must be less than 32");
    anyhow::ensure!(function < 8, "PCI function must be less than 8");
    Ok(((segment as u64) << 16) | ((bus as u64) << 8) | ((device as u64) << 3) | function as u64)
}

impl IoasManager {
    /// Create and initialize a new per-iommu manager.
    async fn new(
        iommu_id: String,
        iommufd: File,
        direct_vm_fd: Option<File>,
        dma_mapper_client: &DmaMapperClient,
        driver: Arc<dyn pal_async::driver::SpawnDriver>,
        recv: mesh::Receiver<IoasManagerRpc>,
    ) -> anyhow::Result<Self> {
        let ctx = Arc::new(vfio_sys::iommufd::IommufdCtx::from_file(iommufd));
        let dmabuf_registry = Arc::new(DmaBufRegistry::default());
        let mode = if let Some(vm_fd) = direct_vm_fd {
            tracing::info!(iommu_id, "created direct iommufd manager for iommu context");
            IommuManagerMode::Direct {
                vm_fd,
                viommu_id: None,
                destroy_viommu_when_unused: false,
            }
        } else {
            let ioas_id = ctx
                .ioas_alloc()
                .context("failed to allocate iommufd IOAS")?;
            let dma_target: Arc<dyn membacking::DmaTarget> = Arc::new(IommufdDmaTarget {
                ctx: ctx.clone(),
                ioas_id,
                dmabuf_registry: dmabuf_registry.clone(),
            });
            let dma_handle = dma_mapper_client
                .add_dma_mapper(dma_target, false)
                .await
                .context("failed to register iommufd IOAS with region manager")?;
            tracing::info!(iommu_id, ioas_id, "created iommufd IOAS for iommu context");
            IommuManagerMode::Ioas {
                ioas_id,
                _dma_handle: dma_handle,
            }
        };

        Ok(Self {
            iommu_id,
            ctx,
            mode,
            accel_states: Vec::new(),
            dmabuf_registry,
            devices: Vec::new(),
            next_device_id: 0,
            next_accel_state_id: 0,
            driver,
            recv,
        })
    }

    async fn run(mut self) {
        while let Ok(rpc) = self.recv.recv().await {
            if let Err(err) = self.retry_pending_direct_cleanup() {
                tracing::error!(
                    error = err.as_ref() as &dyn std::error::Error,
                    iommu_id = self.iommu_id,
                    "direct iommufd cleanup remains pending; state retained for retry"
                );
            }
            match rpc {
                IoasManagerRpc::PrepareDevice {
                    pci_id,
                    cdev,
                    vsmmu,
                    direct_hwpt_flags,
                    respond,
                    completion_send,
                } => {
                    let completion_vsmmu = vsmmu.clone();
                    let result = self.prepare_device(pci_id, cdev, vsmmu, direct_hwpt_flags);
                    completion_send.send(VfioCdevManagerRpc::PrepareComplete {
                        vsmmu: completion_vsmmu,
                        iommu_id: self.iommu_id.clone(),
                        result,
                        respond,
                    });
                }
                IoasManagerRpc::RemoveDevice(device_id) => {
                    if let Err(err) = self.remove_device(device_id) {
                        tracing::error!(
                            error = err.as_ref() as &dyn std::error::Error,
                            iommu_id = self.iommu_id,
                            device_id,
                            "direct iommufd cleanup failed; state retained for retry"
                        );
                    }
                }
                IoasManagerRpc::Inspect(deferred) => deferred.inspect(&self),
            }
        }

        for entry in &mut self.devices {
            if let Some(direct) = &mut entry.direct {
                direct.removal_requested = true;
            }
        }
        if let IommuManagerMode::Direct {
            destroy_viommu_when_unused,
            ..
        } = &mut self.mode
        {
            *destroy_viommu_when_unused = true;
        }
        for _ in 0..3 {
            if self.retry_pending_direct_cleanup().is_ok() {
                break;
            }
        }
    }

    fn prepare_device(
        &mut self,
        pci_id: String,
        cdev_file: File,
        vsmmu: Option<Arc<smmu::SmmuSharedState>>,
        direct_hwpt_flags: u32,
    ) -> anyhow::Result<CdevPrepareResponse> {
        let cdev = vfio_sys::cdev::CdevDevice::from_file(cdev_file);
        let devid = cdev
            .bind_iommufd(self.ctx.as_raw_fd())
            .context("failed to bind VFIO cdev to iommufd")?;

        let (nesting, accel_state_id, attach_id, direct_objects) =
            if matches!(&self.mode, IommuManagerMode::Direct { .. }) {
                anyhow::ensure!(
                    vsmmu.is_none(),
                    "direct cdev attach cannot also use OpenVMM-managed SMMU nesting"
                );
                let (attach_id, direct) =
                    self.prepare_direct_device(&pci_id, devid, &cdev, direct_hwpt_flags)?;
                (None, None, attach_id, Some(direct))
            } else {
                anyhow::ensure!(
                    direct_hwpt_flags == 0,
                    "direct HWPT flags were supplied for a normal IOAS device"
                );
                let ioas_id = match &self.mode {
                    IommuManagerMode::Ioas { ioas_id, .. } => *ioas_id,
                    IommuManagerMode::Direct { .. } => unreachable!(),
                };
                if let Some(vsmmu) = vsmmu {
                    let host_caps = crate::iommufd_nesting::query_host_caps(&self.ctx, devid)
                        .context("failed to query host SMMU capabilities")?;
                    let existing = self.accel_states.iter().find_map(|entry| {
                        let cached = entry.vsmmu.upgrade()?;
                        Arc::ptr_eq(&cached, &vsmmu).then(|| (entry.accel.clone(), entry.id))
                    });
                    let (accel_state, accel_state_id) = match existing {
                        Some(state) => state,
                        None => {
                            let (parent, viommu_id) = self.select_nesting_parent(devid)?;
                            let state = Arc::new(
                                crate::iommufd_nesting::SmmuAccelState::new(
                                    self.ctx.clone(),
                                    devid,
                                    parent.clone(),
                                    viommu_id,
                                    &vsmmu,
                                    self.driver.as_ref(),
                                )
                                .context("failed to create SMMU accel state")?,
                            );
                            let id = self.next_accel_state_id;
                            self.next_accel_state_id += 1;
                            self.accel_states.push(AccelStateEntry {
                                id,
                                vsmmu: Arc::downgrade(&vsmmu),
                                accel: state.clone(),
                                parent,
                            });
                            (state, id)
                        }
                    };
                    (
                        Some(NestingOutput {
                            accel_state,
                            host_caps,
                        }),
                        Some(accel_state_id),
                        ioas_id,
                        None,
                    )
                } else {
                    let attach_id = cdev
                        .attach_ioas(ioas_id)
                        .context("failed to attach cdev device to IOAS")?;
                    (None, None, attach_id, None)
                }
            };

        let device_id = self.next_device_id;
        self.next_device_id += 1;
        self.devices.push(CdevDeviceEntry {
            id: device_id,
            pci_id: pci_id.clone(),
            accel_state_id,
            direct: direct_objects,
        });

        tracing::info!(
            pci_id,
            iommu_id = self.iommu_id,
            iommufd_devid = devid,
            attach_id,
            device_id,
            needs_nesting = nesting.is_some(),
            direct = matches!(&self.mode, IommuManagerMode::Direct { .. }),
            "VFIO cdev device prepared"
        );

        Ok(CdevPrepareResponse {
            device: cdev.into_device(),
            iommufd_devid: devid,
            attach_id,
            removal: CdevDeviceRemoval {
                device_id,
                manager_send: self.recv.sender(),
            },
            dmabuf_registry: self.dmabuf_registry.clone(),
            nesting,
        })
    }

    /// Picks a nesting parent for `dev_id`, reusing the first one the host
    /// accepts and allocating a new one if none is compatible, and returns it
    /// with the vIOMMU allocated on it.
    ///
    /// The UAPI exposes no physical-IOMMU identity for an HWPT to compare
    /// directly. Endpoint sysfs links expose their IOMMU identity, but using
    /// those would require separately tracking which endpoint created each
    /// parent and would duplicate the kernel's authoritative compatibility
    /// check. Probe with `IOMMU_VIOMMU_ALLOC` instead, then allocate a parent
    /// from this device if no existing parent is compatible.
    fn select_nesting_parent(
        &mut self,
        dev_id: u32,
    ) -> anyhow::Result<(Arc<crate::iommufd_nesting::NestingParent>, u32)> {
        let mut parents = Vec::new();
        for entry in &self.accel_states {
            if !parents
                .iter()
                .any(|parent| Arc::ptr_eq(parent, &entry.parent))
            {
                parents.push(entry.parent.clone());
            }
        }
        for parent in parents {
            if let ViommuAlloc::Allocated(viommu_id) = parent.alloc_viommu(dev_id)? {
                return Ok((parent, viommu_id));
            }
        }

        let parent = Arc::new(crate::iommufd_nesting::NestingParent::new(
            self.ctx.clone(),
            dev_id,
            match &self.mode {
                IommuManagerMode::Ioas { ioas_id, .. } => *ioas_id,
                IommuManagerMode::Direct { .. } => unreachable!(),
            },
        )?);
        let ViommuAlloc::Allocated(viommu_id) = parent.alloc_viommu(dev_id)? else {
            // The parent was just built from this device's own IOMMU.
            anyhow::bail!("host rejected a vIOMMU on a nesting parent allocated for this device");
        };
        Ok((parent, viommu_id))
    }

    fn prepare_direct_device(
        &mut self,
        pci_id: &str,
        devid: u32,
        cdev: &vfio_sys::cdev::CdevDevice,
        hwpt_flags: u32,
    ) -> anyhow::Result<(u32, DirectDeviceObjects)> {
        self.retry_pending_direct_cleanup()
            .context("cannot prepare a direct device while prior cleanup is pending")?;

        let virt_id = hyperv_logical_device_id(pci_id)
            .with_context(|| format!("invalid host PCI address {pci_id}"))?;
        anyhow::ensure!(
            !self
                .devices
                .iter()
                .filter_map(|entry| entry.direct.as_ref())
                .any(|direct| direct.virt_id == virt_id),
            "duplicate Hyper-V logical device ID {virt_id:#x} for {pci_id}"
        );

        let device_file = cdev
            .as_ref()
            .try_clone()
            .context("failed to duplicate VFIO cdev fd for explicit detach")?;
        let mut objects = DirectDeviceObjects::new(device_file, virt_id);

        let (existing_viommu, vm_fd) = match &self.mode {
            IommuManagerMode::Direct {
                vm_fd, viommu_id, ..
            } => (*viommu_id, vm_fd.as_raw_fd()),
            IommuManagerMode::Ioas { .. } => unreachable!(),
        };
        let created_viommu = existing_viommu.is_none();
        let viommu_id = if let Some(id) = existing_viommu {
            id
        } else {
            let id = self
                .ctx
                .viommu_alloc_direct(devid, vm_fd)
                .context("failed to allocate direct vIOMMU")?;
            if let IommuManagerMode::Direct { viommu_id, .. } = &mut self.mode {
                *viommu_id = Some(id);
            }
            id
        };

        let vdevice_id = match self.ctx.vdevice_alloc(viommu_id, devid, virt_id) {
            Ok(id) => id,
            Err(err) => {
                return self.direct_prepare_failed(
                    pci_id,
                    objects,
                    created_viommu,
                    err.context(format!(
                        "failed to allocate direct vDEVICE virt_id={virt_id:#x}"
                    )),
                );
            }
        };
        objects.vdevice_id = Some(vdevice_id);

        let hwpt_id = match self.ctx.hwpt_alloc_direct(devid, viommu_id, hwpt_flags) {
            Ok(id) => id,
            Err(err) => {
                return self.direct_prepare_failed(
                    pci_id,
                    objects,
                    created_viommu,
                    err.context("failed to allocate direct HWPT"),
                );
            }
        };
        objects.hwpt_id = Some(hwpt_id);

        let attached_id = match cdev.attach_ioas(hwpt_id) {
            Ok(id) => id,
            Err(err) => {
                return self.direct_prepare_failed(
                    pci_id,
                    objects,
                    created_viommu,
                    err.context("failed to attach cdev device to direct HWPT"),
                );
            }
        };
        objects.attached = true;
        if attached_id != hwpt_id {
            return self.direct_prepare_failed(
                pci_id,
                objects,
                created_viommu,
                anyhow::anyhow!(
                    "kernel replaced direct HWPT {hwpt_id} with unexpected object {attached_id}"
                ),
            );
        }

        tracing::info!(
            pci_id,
            iommu_id = self.iommu_id,
            iommufd_devid = devid,
            viommu_id,
            vdevice_id,
            virt_id,
            hwpt_id,
            hwpt_flags,
            "VFIO cdev device attached to direct HWPT"
        );
        Ok((hwpt_id, objects))
    }

    fn direct_prepare_failed<T>(
        &mut self,
        pci_id: &str,
        mut direct: DirectDeviceObjects,
        created_viommu: bool,
        primary: anyhow::Error,
    ) -> anyhow::Result<T> {
        direct.removal_requested = true;
        let cleanup_result = Self::cleanup_direct_objects(&self.ctx, &mut direct);
        if !direct.is_clean() {
            let device_id = self.next_device_id;
            self.next_device_id += 1;
            self.devices.push(CdevDeviceEntry {
                id: device_id,
                pci_id: pci_id.to_string(),
                accel_state_id: None,
                direct: Some(direct),
            });
        }
        if created_viommu {
            if let IommuManagerMode::Direct {
                destroy_viommu_when_unused,
                ..
            } = &mut self.mode
            {
                *destroy_viommu_when_unused = true;
            }
        }
        let viommu_result = self.try_destroy_unused_direct_viommu();

        match (cleanup_result, viommu_result) {
            (Ok(()), Ok(())) => Err(primary),
            (cleanup, viommu) => Err(primary.context(format!(
                "direct attach rollback remains pending (device cleanup: {}; vIOMMU cleanup: {})",
                cleanup
                    .err()
                    .map_or_else(|| "complete".to_string(), |e| format!("{e:#}")),
                viommu
                    .err()
                    .map_or_else(|| "complete".to_string(), |e| format!("{e:#}"))
            ))),
        }
    }

    fn cleanup_direct_objects(
        ctx: &vfio_sys::iommufd::IommufdCtx,
        direct: &mut DirectDeviceObjects,
    ) -> anyhow::Result<()> {
        loop {
            let action = direct.next_cleanup_action();
            let result = match action {
                DirectCleanupAction::Detach => {
                    let file = direct
                        .device_file
                        .try_clone()
                        .context("failed to duplicate VFIO cdev fd for detach")?;
                    vfio_sys::cdev::CdevDevice::from_file(file)
                        .detach_ioas()
                        .context("failed to explicitly detach direct HWPT")
                }
                DirectCleanupAction::DestroyHwpt(id) => ctx
                    .destroy(id)
                    .with_context(|| format!("failed to destroy direct HWPT {id}")),
                DirectCleanupAction::DestroyVdevice(id) => ctx
                    .destroy(id)
                    .with_context(|| format!("failed to destroy direct vDEVICE {id}")),
                DirectCleanupAction::Complete => return Ok(()),
            };
            match result {
                Ok(()) => direct.mark_cleanup_success(action),
                Err(err) => {
                    direct.cleanup_error = Some(format!("{err:#}"));
                    return Err(err);
                }
            }
        }
    }

    fn retry_pending_direct_cleanup(&mut self) -> anyhow::Result<()> {
        let mut errors = Vec::new();
        let mut index = 0;
        while index < self.devices.len() {
            let removal_requested = self.devices[index]
                .direct
                .as_ref()
                .is_some_and(|direct| direct.removal_requested);
            if !removal_requested {
                index += 1;
                continue;
            }

            let result = {
                let direct = self.devices[index].direct.as_mut().expect("checked above");
                Self::cleanup_direct_objects(&self.ctx, direct)
            };
            if let Err(err) = result {
                errors.push(format!("{}: {err:#}", self.devices[index].pci_id));
                index += 1;
            } else {
                let entry = self.devices.swap_remove(index);
                if entry.direct.is_some() {
                    if let IommuManagerMode::Direct {
                        destroy_viommu_when_unused,
                        ..
                    } = &mut self.mode
                    {
                        *destroy_viommu_when_unused = true;
                    }
                }
            }
        }

        if let Err(err) = self.try_destroy_unused_direct_viommu() {
            errors.push(format!("shared direct vIOMMU: {err:#}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(errors.join("; "))
        }
    }

    fn try_destroy_unused_direct_viommu(&mut self) -> anyhow::Result<()> {
        if self.devices.iter().any(|entry| entry.direct.is_some()) {
            return Ok(());
        }
        let id = match &self.mode {
            IommuManagerMode::Direct {
                viommu_id: Some(id),
                destroy_viommu_when_unused: true,
                ..
            } => *id,
            _ => return Ok(()),
        };
        self.ctx
            .destroy(id)
            .with_context(|| format!("failed to destroy shared direct vIOMMU {id}"))?;
        if let IommuManagerMode::Direct {
            viommu_id,
            destroy_viommu_when_unused,
            ..
        } = &mut self.mode
        {
            *viommu_id = None;
            *destroy_viommu_when_unused = false;
        }
        Ok(())
    }

    fn remove_device(&mut self, device_id: u64) -> anyhow::Result<()> {
        let Some(pos) = self.devices.iter().position(|entry| entry.id == device_id) else {
            return Ok(());
        };
        if let Some(direct) = &mut self.devices[pos].direct {
            direct.removal_requested = true;
            Self::cleanup_direct_objects(&self.ctx, direct)?;
        }
        let entry = self.devices.swap_remove(pos);
        if let Some(accel_state_id) = entry.accel_state_id {
            if !self
                .devices
                .iter()
                .any(|device| device.accel_state_id == Some(accel_state_id))
            {
                let pos = self
                    .accel_states
                    .iter()
                    .position(|state| state.id == accel_state_id)
                    .expect("device accelerated state must remain cached until removal");
                self.accel_states.swap_remove(pos);
            }
        }
        if entry.direct.is_some() {
            if let IommuManagerMode::Direct {
                destroy_viommu_when_unused,
                ..
            } = &mut self.mode
            {
                *destroy_viommu_when_unused = true;
            }
            self.try_destroy_unused_direct_viommu()?;
        }
        tracing::info!(
            device_id,
            pci_id = entry.pci_id,
            iommu_id = self.iommu_id,
            "removed cdev device"
        );
        Ok(())
    }
}

// --- Cdev dispatcher (VfioCdevManager) ---

/// RPC messages for the cdev dispatcher.
pub(crate) enum VfioCdevManagerRpc {
    /// Bind a cdev device to an IOAS, spawning a per-iommu manager if
    /// this is the first device for the given iommu ID.
    PrepareDevice(FailableRpc<CdevPrepareRequest, CdevPrepareResponse>),
    /// Complete a tentative vSMMU-to-manager association.
    PrepareComplete {
        vsmmu: Option<Arc<smmu::SmmuSharedState>>,
        iommu_id: String,
        result: anyhow::Result<CdevPrepareResponse>,
        respond: FailableRpc<(), CdevPrepareResponse>,
    },
    /// Inspect.
    Inspect(inspect::Deferred),
}

/// Request payload for `PrepareDevice`.
pub(crate) struct CdevPrepareRequest {
    pub pci_id: String,
    pub cdev: File,
    pub iommufd: File,
    pub iommu_id: String,
    /// The emulated SMMU this device sits behind, when it is behind an
    /// accel-capable SMMU and needs iommufd nested translation. When set,
    /// the IoasManager allocates the S2 parent HWPT, queries host SMMU
    /// capabilities, and creates (or reuses, matched by `Arc::ptr_eq`) the
    /// per-vSMMU vIOMMU. The resolver guarantees that one vSMMU is routed to
    /// only one `IoasManager`; a vSMMU cannot span independent IOAS contexts.
    pub vsmmu: Option<Arc<smmu::SmmuSharedState>>,
    pub direct_vm_fd: Option<File>,
    pub direct_hwpt_flags: u32,
}

/// Response payload for `PrepareDevice`.
pub(crate) struct CdevPrepareResponse {
    pub device: vfio_sys::Device,
    pub iommufd_devid: u32,
    pub attach_id: u32,
    /// Removal notification transferred into the device binding on success.
    removal: CdevDeviceRemoval,
    /// Registry of exported device-BAR dmabufs for this IOAS, shared so the
    /// device can register its BAR dmabufs for peer-to-peer DMA.
    pub dmabuf_registry: Arc<DmaBufRegistry>,
    /// Nesting output for iommufd nested S1. Present when the device was
    /// prepared with `vsmmu: Some(_)`.
    pub nesting: Option<NestingOutput>,
}

/// Notifies the per-IOAS manager exactly once when device ownership ends.
struct CdevDeviceRemoval {
    device_id: u64,
    manager_send: mesh::Sender<IoasManagerRpc>,
}

impl Drop for CdevDeviceRemoval {
    fn drop(&mut self) {
        self.manager_send
            .send(IoasManagerRpc::RemoveDevice(self.device_id));
    }
}

/// iommufd nested-translation objects returned from [`IoasManager`] when a
/// device is behind an accel-capable SMMU.
///
/// The manager owns the iommufd-side lifecycle (vIOMMU, S2 parent, host caps
/// query); the resolver consumes this to wire the device into the emulated
/// SMMU.
pub(crate) struct NestingOutput {
    /// Shared per-vSMMU accel state (vIOMMU + S2 parent), reused across all
    /// devices behind the same emulated SMMU and host IOMMU context.
    pub accel_state: Arc<crate::iommufd_nesting::SmmuAccelState>,
    /// Host SMMU capabilities, to finalize the emulated SMMU's parameters.
    pub host_caps: smmu::HostSmmuCaps,
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
    managers: HashMap<String, IommuManagerEntry>,
    /// vSMMU-to-manager associations, serialized by this actor.
    vsmmu_associations: VsmmuAssociations,
    /// DMA mapper client, cloned for each new per-iommu manager.
    dma_mapper_client: DmaMapperClient,
    /// Spawner for per-iommu manager tasks, and driver for the vEVENTQ fds
    /// they poll.
    spawner: Arc<dyn pal_async::driver::SpawnDriver>,
    /// Per-iommu manager tasks (kept alive).
    tasks: Vec<pal_async::task::Task<()>>,
    recv: mesh::Receiver<VfioCdevManagerRpc>,
}

#[derive(Default)]
struct VsmmuAssociations(Vec<VsmmuAssociation>);

struct VsmmuAssociation {
    vsmmu: Weak<smmu::SmmuSharedState>,
    iommu_id: String,
    in_flight: usize,
    committed: bool,
}

impl VsmmuAssociations {
    fn begin(&mut self, vsmmu: &Arc<smmu::SmmuSharedState>, iommu_id: &str) -> anyhow::Result<()> {
        self.0.retain(|entry| entry.vsmmu.strong_count() != 0);
        if let Some(entry) = self
            .0
            .iter_mut()
            .find(|entry| entry.vsmmu.as_ptr() == Arc::as_ptr(vsmmu))
        {
            anyhow::ensure!(
                entry.iommu_id == iommu_id,
                "SMMU is already associated with IOMMU context {:?}; it cannot also use {:?}",
                entry.iommu_id,
                iommu_id
            );
            entry.in_flight += 1;
        } else {
            self.0.push(VsmmuAssociation {
                vsmmu: Arc::downgrade(vsmmu),
                iommu_id: iommu_id.to_owned(),
                in_flight: 1,
                committed: false,
            });
        }
        Ok(())
    }

    fn complete(&mut self, vsmmu: &Arc<smmu::SmmuSharedState>, iommu_id: &str, success: bool) {
        let index = self
            .0
            .iter()
            .position(|entry| entry.vsmmu.as_ptr() == Arc::as_ptr(vsmmu))
            .expect("completed vSMMU preparation must have an association");
        let entry = &mut self.0[index];
        assert_eq!(entry.iommu_id, iommu_id);
        assert_ne!(entry.in_flight, 0);
        entry.in_flight -= 1;
        entry.committed |= success;
        if entry.in_flight == 0 && !entry.committed {
            self.0.swap_remove(index);
        }
    }
}

struct IommuManagerEntry {
    sender: mesh::Sender<IoasManagerRpc>,
    /// Prevent mixing direct and IOAS devices under the same --iommu ID.
    direct: bool,
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
        spawner: Arc<dyn pal_async::driver::SpawnDriver>,
        dma_mapper_client: DmaMapperClient,
    ) -> Self {
        Self {
            managers: HashMap::new(),
            vsmmu_associations: VsmmuAssociations::default(),
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
                VfioCdevManagerRpc::PrepareComplete {
                    vsmmu,
                    iommu_id,
                    result,
                    respond,
                } => {
                    if let Some(vsmmu) = vsmmu {
                        self.vsmmu_associations
                            .complete(&vsmmu, &iommu_id, result.is_ok());
                    }
                    match result {
                        Ok(response) => respond.complete(Ok(response)),
                        Err(error) => respond.fail(error),
                    }
                }
                VfioCdevManagerRpc::Inspect(deferred) => {
                    deferred.respond(|resp| {
                        for (iommu_id, sender) in &self.managers {
                            resp.child(iommu_id, |req| {
                                sender.sender.send(IoasManagerRpc::Inspect(req.defer()));
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
            vsmmu,
            direct_vm_fd,
            direct_hwpt_flags,
        } = req;

        if let Some(vsmmu) = &vsmmu {
            if let Err(error) = self.vsmmu_associations.begin(vsmmu, &iommu_id) {
                respond.fail(error);
                return;
            }
        }
        let direct = direct_vm_fd.is_some();
        let sender = match self.managers.entry(iommu_id.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => {
                let entry = e.into_mut();
                if entry.direct != direct {
                    if let Some(vsmmu) = &vsmmu {
                        self.vsmmu_associations.complete(vsmmu, &iommu_id, false);
                    }
                    respond.fail(anyhow::anyhow!(
                        "iommu={iommu_id} cannot mix direct and IOAS cdev devices"
                    ));
                    return;
                }
                &mut entry.sender
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                let mut ioas_recv: mesh::Receiver<IoasManagerRpc> = mesh::Receiver::new();
                let sender = ioas_recv.sender();

                let mgr = match IoasManager::new(
                    iommu_id.clone(),
                    iommufd,
                    direct_vm_fd,
                    &self.dma_mapper_client,
                    self.spawner.clone(),
                    ioas_recv,
                )
                .await
                .with_context(|| {
                    format!("failed to initialize iommufd IOAS manager for iommu={iommu_id}")
                }) {
                    Ok(mgr) => mgr,
                    Err(e) => {
                        if let Some(vsmmu) = &vsmmu {
                            self.vsmmu_associations.complete(vsmmu, &iommu_id, false);
                        }
                        respond.fail(e);
                        return;
                    }
                };

                let task = self
                    .spawner
                    .spawn(format!("vfio-ioas-{iommu_id}"), mgr.run());
                self.tasks.push(task);
                &mut e.insert(IommuManagerEntry { sender, direct }).sender
            }
        };

        // Forward to the per-iommu manager task. The manager will
        // complete the respond half after the bind/attach ioctls.
        sender.send(IoasManagerRpc::PrepareDevice {
            pci_id,
            cdev,
            vsmmu,
            direct_hwpt_flags,
            respond,
            completion_send: self.recv.sender(),
        });
    }

    pub(crate) fn client(&mut self) -> VfioCdevManagerClient {
        VfioCdevManagerClient {
            sender: self.recv.sender(),
        }
    }
}

/// The iommufd-related state from a cdev device binding, kept alive for the
/// lifetime of the assigned device.
///
/// Notifies the per-iommu manager on drop so device counts are accurate.
#[derive(Inspect)]
pub(crate) struct VfioCdevBindingState {
    /// Host PCI address used for diagnostics and dmabuf registration.
    pci_id: String,
    /// iommufd device ID returned when the VFIO cdev was bound.
    iommufd_devid: u32,
    attach_id: u32,
    /// Manager removal notification, transferred from the prepare response.
    #[inspect(skip)]
    _removal: CdevDeviceRemoval,
    /// Registry of exported device-BAR dmabufs for this device's IOAS.
    #[inspect(skip)]
    dmabuf_registry: Arc<DmaBufRegistry>,
    /// The `(st_dev, st_ino)` of this device's VFIO cdev, set once BAR
    /// dmabufs are registered, so they can be deregistered on drop.
    #[inspect(skip)]
    dmabuf_inode: Option<(u64, u64)>,
}

impl VfioCdevBindingState {
    /// Split a dispatcher response into the shared VFIO device handle and the
    /// binding state.
    ///
    /// The device is wrapped in an `Arc` so a single fd can be shared by the
    /// PCI emulation and, for a nested device, the iommufd stream backend —
    /// mirroring QEMU's single `vbasedev->fd` and avoiding a `dup`.
    pub(crate) fn from_response(
        resp: CdevPrepareResponse,
        pci_id: String,
    ) -> (Arc<vfio_sys::Device>, Self) {
        (
            Arc::new(resp.device),
            Self {
                pci_id,
                iommufd_devid: resp.iommufd_devid,
                attach_id: resp.attach_id,
                _removal: resp.removal,
                dmabuf_registry: resp.dmabuf_registry,
                dmabuf_inode: None,
            },
        )
    }
}

impl Drop for VfioCdevBindingState {
    fn drop(&mut self) {
        if let Some((st_dev, st_ino)) = self.dmabuf_inode {
            self.dmabuf_registry.deregister_device(st_dev, st_ino);
        }
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

impl VfioBinding {
    /// Returns the per-IOAS dmabuf registry for a cdev/iommufd binding, or
    /// `None` for the legacy group/type1 path (which has no registry — dmabuf
    /// P2P is iommufd-only).
    pub(crate) fn dmabuf_registry(&self) -> Option<&Arc<DmaBufRegistry>> {
        match self {
            VfioBinding::Cdev(state) => Some(&state.dmabuf_registry),
            VfioBinding::Group(_) => None,
        }
    }

    /// Records the VFIO cdev inode under which BAR dmabufs were registered, so
    /// they are deregistered when the binding drops. No-op for the group path.
    pub(crate) fn set_dmabuf_inode(&mut self, inode: (u64, u64)) {
        if let VfioBinding::Cdev(state) = self {
            state.dmabuf_inode = Some(inode);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_vsmmu() -> Arc<smmu::SmmuSharedState> {
        let device = smmu::SmmuDevice::new(
            0,
            guestmem::GuestMemory::allocate(0x1000),
            &smmu::SmmuConfig {
                sidsize: 16,
                oas_policy: smmu::SmmuOasPolicy::Fixed(40),
                accel: true,
            },
            None,
            None,
        );
        device.shared_state().clone()
    }

    #[test]
    fn vsmmu_association_releases_after_all_preparations_fail() {
        let vsmmu = make_vsmmu();
        let mut associations = VsmmuAssociations::default();
        associations.begin(&vsmmu, "iommu0").unwrap();
        associations.begin(&vsmmu, "iommu0").unwrap();
        assert!(associations.begin(&vsmmu, "iommu1").is_err());
        associations.complete(&vsmmu, "iommu0", false);
        assert!(associations.begin(&vsmmu, "iommu1").is_err());
        associations.complete(&vsmmu, "iommu0", false);
        associations.begin(&vsmmu, "iommu1").unwrap();
        associations.complete(&vsmmu, "iommu1", true);
    }

    #[test]
    fn vsmmu_association_stays_committed_after_success() {
        let vsmmu = make_vsmmu();
        let mut associations = VsmmuAssociations::default();
        associations.begin(&vsmmu, "iommu0").unwrap();
        associations.complete(&vsmmu, "iommu0", true);
        assert!(associations.begin(&vsmmu, "iommu1").is_err());
        associations.begin(&vsmmu, "iommu0").unwrap();
        associations.complete(&vsmmu, "iommu0", false);
        assert!(associations.begin(&vsmmu, "iommu1").is_err());
    }

    #[pal_async::async_test]
    async fn cdev_device_removal_notifies_manager() {
        let mut recv = mesh::Receiver::new();
        let removal = CdevDeviceRemoval {
            device_id: 42,
            manager_send: recv.sender(),
        };
        drop(removal);
        let IoasManagerRpc::RemoveDevice(device_id) = recv.recv().await.unwrap() else {
            panic!("unexpected manager message");
        };
        assert_eq!(device_id, 42);
    }

    #[test]
    fn cdev_accel_state_released_on_last_device_each_hotplug_cycle() {
        let mut devices = vec![
            CdevDeviceEntry {
                id: 1,
                pci_id: "device1".into(),
                accel_state_id: Some(10),
                direct: None,
            },
            CdevDeviceEntry {
                id: 2,
                pci_id: "device2".into(),
                accel_state_id: Some(10),
                direct: None,
            },
        ];
        assert_eq!(remove_cdev_device(&mut devices, 1).unwrap().1, None);
        assert_eq!(remove_cdev_device(&mut devices, 2).unwrap().1, Some(10));
        devices.push(CdevDeviceEntry {
            id: 3,
            pci_id: "device3".into(),
            accel_state_id: Some(11),
            direct: None,
        });
        assert_eq!(remove_cdev_device(&mut devices, 3).unwrap().1, Some(11));
        assert!(devices.is_empty());
    }

    #[test]
    fn hyperv_identity_uses_physical_segment_and_bdf() {
        assert_eq!(hyperv_logical_device_id("0008:06:00.0").unwrap(), 0x8_0600);
        assert_eq!(hyperv_logical_device_id("0001:7f:1f.7").unwrap(), 0x1_7fff);
        assert!(hyperv_logical_device_id("0001:7f:20.0").is_err());
    }

    #[test]
    fn direct_cleanup_order_is_retryable() {
        let file = File::open("/dev/null").unwrap();
        let mut direct = DirectDeviceObjects::new(file, 0x8_0600);
        direct.vdevice_id = Some(11);
        direct.hwpt_id = Some(12);
        direct.attached = true;
        assert_eq!(direct.next_cleanup_action(), DirectCleanupAction::Detach);
        assert_eq!(direct.next_cleanup_action(), DirectCleanupAction::Detach);
        direct.mark_cleanup_success(DirectCleanupAction::Detach);
        assert_eq!(
            direct.next_cleanup_action(),
            DirectCleanupAction::DestroyHwpt(12)
        );
        direct.mark_cleanup_success(DirectCleanupAction::DestroyHwpt(12));
        assert_eq!(
            direct.next_cleanup_action(),
            DirectCleanupAction::DestroyVdevice(11)
        );
        direct.mark_cleanup_success(DirectCleanupAction::DestroyVdevice(11));
        assert!(direct.is_clean());
    }
}
