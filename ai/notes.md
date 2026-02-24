# Virtio-blk Implementation Notes

## Codebase Research

### Existing Virtio Device Architecture

**Location**: `vm/devices/virtio/`

Existing virtio devices:
- `virtio/` — core infrastructure (queue, transport PCI/MMIO, spec constants, resolve/resolver)
- `virtio_net/` — network device (most complete example; implements `VirtioDevice` trait directly)
- `virtio_p9/` — Plan 9 filesystem sharing
- `virtio_pmem/` — persistent memory
- `virtio_serial/` — serial/console
- `virtiofs/` — virtio filesystem
- `virtio_resources/` — resource handle definitions for all virtio devices (lightweight config structs)

**No virtio-blk exists yet.** `grep` for `virtio.?blk|virtio_blk|VirtioBlk` returns zero matches.

### Key Traits

1. **`VirtioDevice`** (in `vm/devices/virtio/virtio/src/common.rs`):
   - `traits() -> DeviceTraits` — returns device_id, features, max_queues, register length, shared memory
   - `read_registers_u32` / `write_registers_u32` — device-specific config space
   - `enable(resources: Resources)` — called when driver activates; receives queues, features, shared mem
   - `disable()` — called when driver resets

2. **`LegacyVirtioDevice`** — older trait with `get_work_callback` + `state_change`; wrapped via `LegacyWrapper<T>` into `VirtioDevice`. virtio_net uses the newer `VirtioDevice` directly.

3. **`DeviceTraits`** struct:
   ```rust
   pub struct DeviceTraits {
       pub device_id: u16,        // virtio device type (e.g. 2 for blk)
       pub device_features: VirtioDeviceFeatures,
       pub max_queues: u16,
       pub device_register_length: u32,
       pub shared_memory: DeviceTraitsSharedMemory,
   }
   ```

### Virtio Transport Layer

- **PCI transport**: `vm/devices/virtio/virtio/src/transport/pci.rs` — `VirtioPciDevice`
- **MMIO transport**: `vm/devices/virtio/virtio/src/transport/mmio.rs`
- Virtio devices are wrapped in `VirtioPciDeviceHandle` (from `virtio_resources`) for PCI exposure
- The `VirtioPciResolver` (in `virtio/src/resolver.rs`) resolves `VirtioPciDeviceHandle` → `ResolvedPciDevice`

### Resource Resolution Pattern

Each device has:
1. A **resource handle** (lightweight serializable config) in a `*_resources` crate
2. A **resolver** that takes the handle + input params and constructs the device

For virtio devices specifically:
- Handle type implements `ResourceId<VirtioDeviceHandle>` with a unique string ID
- Resolver implements `AsyncResolveResource<VirtioDeviceHandle, MyHandle>`
- Output is `ResolvedVirtioDevice(Box<dyn VirtioDevice>)`
- Input is `VirtioResolveInput { driver_source, guest_memory }`

The virtio PCI wrapper then wraps this into a PCI device via `VirtioPciDeviceHandle(Resource<VirtioDeviceHandle>)`.

### Disk Backend Abstraction

**Location**: `vm/devices/storage/disk_backend/`

Core trait: **`DiskIo`** (in `disk_backend/src/lib.rs`):
```rust
pub trait DiskIo: 'static + Send + Sync + Inspect {
    fn disk_type(&self) -> &str;
    fn sector_count(&self) -> u64;
    fn sector_size(&self) -> u32;
    fn disk_id(&self) -> Option<[u8; 16]>;
    fn physical_sector_size(&self) -> u32;
    fn is_fua_respected(&self) -> bool;
    fn is_read_only(&self) -> bool;
    fn unmap(&self, sector, count, block_level_only) -> Future<Result<(), DiskError>>;
    fn unmap_behavior(&self) -> UnmapBehavior;
    fn read_vectored(&self, buffers: &RequestBuffers, sector) -> Future<Result<(), DiskError>>;
    fn write_vectored(&self, buffers: &RequestBuffers, sector, fua) -> Future<Result<(), DiskError>>;
    fn sync_cache(&self) -> Future<Result<(), DiskError>>;
    fn wait_resize(&self, sector_count) -> Future<u64>;
}
```

Wrapper type: **`Disk`** (wraps `Arc<DiskInner>`, cheap to clone)

**Resource kind**: `DiskHandleKind` in `vm_resource/src/kind.rs`

Available disk backends (in `disk_backend_resources/src/lib.rs`):
- `FileDiskHandle` — file-backed
- `FixedVhd1DiskHandle` — VHD1
- `LayeredDiskHandle` — layered (with `DiskLayerHandleKind` layers including RAM, SQLite, file, etc.)
- `StripedDiskHandle` — striped across multiple disks
- `BlobDiskHandle` — HTTP blob backed
- `DiskWithReservationsHandle` — persistent reservation wrapper
- `DelayDiskHandle` — adds latency (for testing)
- `AutoFormattedDiskHandle` — NTFS auto-format

### How NVMe Uses Disk Backends

**NVMe** (`vm/devices/storage/nvme/`):
- PCI device, implements `PciDeviceHandleKind`
- Has `NvmeControllerHandle` with `namespaces: Vec<NamespaceDefinition>`
- Each `NamespaceDefinition` has `{ nsid, read_only, disk: Resource<DiskHandleKind> }`
- Resolver resolves each disk resource via `ResolveDiskParameters { read_only, driver_source }`
- Namespace wraps a `Disk` and does read/write/flush/unmap operations

### How Storvsp Uses Disk Backends

**Storvsp** (`vm/devices/storage/storvsp/`):
- VMBus device, implements `VmbusDeviceHandleKind`
- Has `ScsiControllerHandle` with `devices: Vec<ScsiDeviceAndPath>`
- Each device is a `Resource<ScsiDeviceHandleKind>` (typically `SimpleScsiDiskHandle` or `SimpleScsiDvdHandle`)
- `SimpleScsiDiskHandle` contains a `Resource<DiskHandleKind>` inside it
- SCSI adds a translation layer between SCSI commands and disk_backend operations

### CLI/Configuration Integration

**`openvmm/openvmm_entry/src/storage_builder.rs`**:
- `StorageBuilder` manages IDE, SCSI, NVMe disk configs
- Has `DiskLocation` enum: `Ide`, `Scsi`, `Nvme`
- Would need a new `VirtioBlk` variant

### Petri Test Framework

**Location**: `vmm_tests/vmm_tests/tests/tests/x86_64/storage.rs`

Key patterns:
- Tests use `PetriVmBuilder` to construct VMs
- NVMe disks added via `VpciDeviceConfig` with `NvmeControllerHandle`
- SCSI disks added via `ScsiControllerHandle` with `ScsiDeviceAndPath`
- Helper `new_test_vtl2_nvme_device()` creates NVMe configs with ramdisk or file backing
- Uses `LayeredDiskHandle::single_layer(RamDiskLayerHandle { len: Some(size) })` for test disks
- Tests verify disk presence in guest via pipette (SSH-like agent)
- Guest commands check `/sys/block/` for device enumeration

**`petri/src/vm/openvmm/construct.rs`**:
- Builds `Config` for OpenVMM with all devices
- Adds NVMe as VPCI devices, SCSI as VMBus devices
- Would need to add virtio-blk as VPCI devices

---

## Virtio-blk Spec Summary (OASIS VIRTIO v1.2, Section 5.2)

### Device ID
- Device ID: **2**
- PCI device ID: 0x1042 (VIRTIO_PCI_DEVICE_ID_BASE + device_id = 0x1040 + 2)

### Virtqueues
- **requestq** (index 0) — single request queue (more with MQ)

### Feature Bits
- `VIRTIO_BLK_F_SIZE_MAX` (1) — max segment size
- `VIRTIO_BLK_F_SEG_MAX` (2) — max segments per request
- `VIRTIO_BLK_F_GEOMETRY` (4) — disk geometry available
- `VIRTIO_BLK_F_RO` (5) — read-only device
- `VIRTIO_BLK_F_BLK_SIZE` (6) — block size available
- `VIRTIO_BLK_F_FLUSH` (9) — flush command supported
- `VIRTIO_BLK_F_TOPOLOGY` (10) — topology info available
- `VIRTIO_BLK_F_CONFIG_WCE` (11) — writeback/writethrough configurable
- `VIRTIO_BLK_F_MQ` (12) — multi-queue support
- `VIRTIO_BLK_F_DISCARD` (13) — discard/unmap support
- `VIRTIO_BLK_F_WRITE_ZEROES` (14) — write zeroes support

### Config Space (struct virtio_blk_config)
```c
struct virtio_blk_config {
    le64 capacity;          // in 512-byte sectors
    le32 size_max;          // max segment size (if SIZE_MAX)
    le32 seg_max;           // max segments (if SEG_MAX)
    struct virtio_blk_geometry {
        le16 cylinders;
        u8 heads;
        u8 sectors;
    } geometry;             // (if GEOMETRY)
    le32 blk_size;          // block size (if BLK_SIZE)
    struct virtio_blk_topology {
        u8 physical_block_exp;
        u8 alignment_offset;
        le16 min_io_size;
        le32 opt_io_size;
    } topology;             // (if TOPOLOGY)
    u8 writeback;           // (if CONFIG_WCE)
    u8 unused0;
    le16 num_queues;        // (if MQ)
    le32 max_discard_sectors;    // (if DISCARD)
    le32 max_discard_seg;        // (if DISCARD)
    le32 discard_sector_alignment; // (if DISCARD)
    le32 max_write_zeroes_sectors; // (if WRITE_ZEROES)
    le32 max_write_zeroes_seg;     // (if WRITE_ZEROES)
    u8 write_zeroes_may_unmap;     // (if WRITE_ZEROES)
    u8 unused1[3];
};
```

### Request Format
```c
struct virtio_blk_req {
    le32 type;
    le32 reserved;
    le64 sector;
    u8 data[];     // for read/write: data payload
    u8 status;     // VIRTIO_BLK_S_OK=0, _IOERR=1, _UNSUPP=2
};
```

Request types:
- `VIRTIO_BLK_T_IN` (0) — read
- `VIRTIO_BLK_T_OUT` (1) — write
- `VIRTIO_BLK_T_FLUSH` (4) — flush
- `VIRTIO_BLK_T_GET_ID` (8) — get device ID (20-byte ASCII string)
- `VIRTIO_BLK_T_DISCARD` (11) — discard/unmap
- `VIRTIO_BLK_T_WRITE_ZEROES` (13) — write zeroes

### Descriptor Layout
For read: header (device-readable) + data (device-writable) + status (device-writable)
For write: header (device-readable) + data (device-readable) + status (device-writable)

---

## Cloud-Hypervisor Reference

Cloud-hypervisor's `virtio-devices/src/block.rs` shows:
- Uses `block` crate for request parsing (`Request::parse`)
- `BlockEpollHandler` per-queue with epoll-based event loop
- Supports async IO via `AsyncIo` trait
- Tracks inflight requests in a `VecDeque`
- Has rate limiting support
- Counters for read/write bytes, ops, latency
- Feature negotiation includes RO, FLUSH, BLK_SIZE, TOPOLOGY, MQ, etc.
- Serial number via `build_serial()` (20-byte ASCII)
- Config space read/write for `VirtioBlockConfig`

Key design patterns from cloud-hypervisor:
1. Parse request header from first descriptor
2. Validate request type and permissions (RO check)
3. Execute IO asynchronously
4. Write status byte to last descriptor
5. Complete descriptor in used ring
6. Signal guest via interrupt

---

## Resolver Registration

**`openvmm/openvmm_resources/src/lib.rs`** is the central registration point:
- Uses `vm_resource::register_static_resolvers!` macro
- Virtio device resolvers listed under "// Virtio devices" section
- Our new `VirtioBlkResolver` will be added here
- Also need dependency in `openvmm/openvmm_resources/Cargo.toml`

### Resolver Pattern (virtio_net as reference)

```rust
// In resolver.rs:
pub struct VirtioBlkResolver;

declare_static_async_resolver! {
    VirtioBlkResolver,
    (VirtioDeviceHandle, VirtioBlkHandle),
}

impl AsyncResolveResource<VirtioDeviceHandle, VirtioBlkHandle> for VirtioBlkResolver {
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;
    // resolve() creates the device from the handle
}
```

### RequestBuffers Bridge

`scsi_buffers::RequestBuffers` wraps `PagedRange<'a>` + `GuestMemory` + `is_write`.
NVMe creates `RequestBuffers` from PRP lists → `PagedRange`.
For virtio-blk, we need to convert virtio descriptor chain GPAs into a `PagedRange`.

`PagedRange` requires: offset, total_len, and a slice of GPNs (guest page numbers).
The virtio payload has `VirtioQueuePayload { address: u64, length: u32, writeable: bool }`.
We'll need to convert these GPA-based descriptors into page-aligned `PagedRange` entries.

## Petri Test Construction

**`petri/src/vm/openvmm/construct.rs`** function `vmbus_storage_controllers_to_openvmm`:
- Maps `VmbusStorageType::Scsi` → ScsiControllerHandle (VMBus device)
- Maps `VmbusStorageType::Nvme` → NvmeControllerHandle (VPCI device)
- Need to add `VmbusStorageType::VirtioBlk` (or a separate mechanism, since virtio-blk
  is a PCI device, not VMBus)

**`petri/src/vm/mod.rs`** has `VmbusStorageType` enum — currently `Scsi` and `Nvme`.
Need to add `VirtioBlk` variant.

**Petri disk helper** `petri_disk_to_openvmm()` converts petri `Disk` enum to `Resource<DiskHandleKind>`.
This already works for any disk type; we just need to wire it into the virtio-blk handle.

## Linux Test Kernel Config (virtio-related)

Extracted from `/home/john/src/openvmm/.packages/underhill-deps-private/x64/vmlinux` (IKCONFIG):

```
CONFIG_VIRTIO_BLK=y           # ✅ Built-in (not module)
CONFIG_VIRTIO_PCI=y           # ✅ PCI transport built-in
CONFIG_VIRTIO_PCI_LIB=y
CONFIG_VIRTIO_PCI_LEGACY=y
CONFIG_VIRTIO_PCI_LIB_LEGACY=y
CONFIG_BLK_DEV_NVME is not set  # NVMe is NOT enabled
CONFIG_BLK_DEV_SD=y           # SCSI disk built-in
CONFIG_BLK_DEV_SR=y           # SCSI CD-ROM built-in
```

Virtio-blk devices will appear as `/dev/vdX` in the guest (vda, vdb, etc.).
The test kernel is ready for virtio-blk testing with no modifications needed.

## Design Decisions

1. **Use `VirtioDevice` directly** (like virtio_net), not `LegacyVirtioDevice`.
   The modern trait gives us direct control over queue management and is the preferred pattern.

2. **Disk backend**: Use `disk_backend::Disk` directly via `Resource<DiskHandleKind>`,
   same as NVMe. This gives us all the same backends automatically (file, VHD, ramdisk, layered, etc.)

3. **RequestBuffers bridge**: Need to translate virtio descriptor chains into `scsi_buffers::RequestBuffers`
   for `Disk::read_vectored`/`write_vectored`. NVMe does similar with PRP→RequestBuffers in its `prp.rs`.

4. **Initial feature set**: SIZE_MAX, SEG_MAX, BLK_SIZE, FLUSH, RO, TOPOLOGY, DISCARD, WRITE_ZEROES.
   Omit MQ and CONFIG_WCE initially for simplicity.

5. **Config space**: Populated from `Disk` metadata (sector_count, sector_size, physical_sector_size, etc.)

6. **Save/restore**: Not in initial implementation, but design should accommodate it.

## Files to Create/Modify (Summary)

### New files:
- `vm/devices/virtio/virtio_blk/Cargo.toml`
- `vm/devices/virtio/virtio_blk/src/lib.rs` — device implementation
- `vm/devices/virtio/virtio_blk/src/resolver.rs` — resolver
- `vm/devices/virtio/virtio_blk/src/spec.rs` — virtio-blk spec constants/structs

### Modify existing:
- `vm/devices/virtio/virtio_resources/src/lib.rs` — add `pub mod blk` with `VirtioBlkHandle`
- `vm/devices/virtio/virtio_resources/Cargo.toml` — add `disk_backend_resources` dep (for DiskHandleKind)
- `Cargo.toml` (workspace root) — add `virtio_blk` member
- `openvmm/openvmm_resources/src/lib.rs` — register `VirtioBlkResolver`
- `openvmm/openvmm_resources/Cargo.toml` — add `virtio_blk` dependency
- `openvmm/openvmm_entry/src/storage_builder.rs` — add `VirtioBlk` variant to `DiskLocation`
- `petri/src/vm/mod.rs` — add `VirtioBlk` to `VmbusStorageType` (or equivalent mechanism)
- `petri/src/vm/openvmm/construct.rs` — handle VirtioBlk in config construction
- `vmm_tests/vmm_tests/tests/tests/x86_64/storage.rs` — add virtio-blk tests
