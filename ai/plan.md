# Virtio-blk Implementation Plan

## Problem Statement

Add a virtio-blk block device implementation to OpenVMM. The device should:
- Follow the OASIS VIRTIO v1.2 spec (Section 5.2) for the block device
- Support all the same disk backends as NVMe and Storvsp (file, VHD, ramdisk, layered, etc.)
- Be exposed as a PCI device via the existing virtio PCI transport
- Be testable via the petri integration test framework
- Follow the existing codebase patterns (VirtioDevice trait, resource resolution, etc.)

## Approach

Model the implementation after `virtio_net` (for the VirtioDevice trait pattern) and `nvme` (for
disk backend integration). The device will:
1. Implement the `VirtioDevice` trait directly (modern pattern)
2. Use `disk_backend::Disk` via `Resource<DiskHandleKind>` for disk backends
3. Translate virtio descriptor chains into `RequestBuffers` for disk I/O
4. Be wrapped in `VirtioPciDeviceHandle` for PCI transport

## Workplan

### Phase 1: Core Device Implementation

- [ ] **1.1 Create `virtio_blk` crate skeleton**
  - `vm/devices/virtio/virtio_blk/Cargo.toml`
  - `vm/devices/virtio/virtio_blk/src/lib.rs`
  - Add to workspace `Cargo.toml` members list

- [ ] **1.2 Define virtio-blk spec constants (`spec.rs`)**
  - Device ID (2), feature bits, request types
  - `VirtioBlkConfig` struct (config space layout)
  - `VirtioBlkReqHeader` struct (request header)
  - Status codes (OK=0, IOERR=1, UNSUPP=2)
  - Discard/write-zeroes segment structs

- [ ] **1.3 Implement the `VirtioDevice` trait (`lib.rs`)**
  - Device struct holding `Disk`, `GuestMemory`, `VmTaskDriver`
  - `traits()` → device_id=2, features, max_queues=1, config register length
  - `read_registers_u32` / `write_registers_u32` → config space (capacity, blk_size, etc.)
  - `enable()` → spawn per-queue worker tasks
  - `disable()` → stop workers, wait for outstanding I/O

- [ ] **1.4 Implement request processing worker**
  - Parse `VirtioBlkReqHeader` from first descriptor (type, sector)
  - For READ: create `RequestBuffers` from data descriptors, call `disk.read_vectored()`
  - For WRITE: create `RequestBuffers` from data descriptors, call `disk.write_vectored()`
  - For FLUSH: call `disk.sync_cache()`
  - For GET_ID: write 20-byte serial string to data descriptor
  - For DISCARD: call `disk.unmap()`
  - For WRITE_ZEROES: call `disk.write_vectored()` with zeros or `disk.unmap()`
  - Write status byte to final descriptor
  - Complete descriptor in used ring

- [ ] **1.5 Descriptor-to-RequestBuffers translation**
  - Convert `VirtioQueuePayload` (GPA + length) to `PagedRange` for `RequestBuffers`
  - Handle page boundary splitting (similar to NVMe's `prp.rs`)
  - Validate request bounds (sector + data_len <= capacity)

### Phase 2: Resource Plumbing

- [ ] **2.1 Add resource handle to `virtio_resources`**
  - Add `pub mod blk` to `vm/devices/virtio/virtio_resources/src/lib.rs`
  - Define `VirtioBlkHandle { disk: Resource<DiskHandleKind>, read_only: bool }`
  - Implement `ResourceId<VirtioDeviceHandle>` with ID `"virtio-blk"`
  - Add necessary deps to `virtio_resources/Cargo.toml`

- [ ] **2.2 Create resolver (`resolver.rs`)**
  - `VirtioBlkResolver` struct
  - `declare_static_async_resolver!` for `(VirtioDeviceHandle, VirtioBlkHandle)`
  - `AsyncResolveResource` impl: resolve disk handle, create device, return `ResolvedVirtioDevice`

- [ ] **2.3 Register resolver in `openvmm_resources`**
  - Add `virtio_blk::resolver::VirtioBlkResolver` to `register_static_resolvers!` in
    `openvmm/openvmm_resources/src/lib.rs`
  - Add `virtio_blk` dep to `openvmm/openvmm_resources/Cargo.toml`

### Phase 3: CLI/Configuration Integration

- [ ] **3.1 Add VirtioBlk to `DiskLocation` enum**
  - In `openvmm/openvmm_entry/src/storage_builder.rs`, add `VirtioBlk` variant
  - Add CLI argument handling (e.g. `--disk memdiff:file.vhdx,virtio-blk`)
  - Build `VpciDeviceConfig` with `VirtioPciDeviceHandle(VirtioBlkHandle.into_resource())`

- [ ] **3.2 Wire up the CLI argument parsing**
  - Find where disk location is parsed from CLI args (likely in `cli_args.rs` or similar)
  - Add `virtio-blk` as a valid disk target

### Phase 4: Petri Test Integration

- [ ] **4.1 Add VirtioBlk to petri storage types**
  - Add `VirtioBlk` variant to `VmbusStorageType` enum (or create separate mechanism)
    in `petri/src/vm/mod.rs`
  - Add handling in `petri/src/vm/openvmm/construct.rs` to generate `VpciDeviceConfig`
    with `VirtioPciDeviceHandle(VirtioBlkHandle.into_resource())`
  - Ensure `petri_disk_to_openvmm()` output works with `VirtioBlkHandle`

- [ ] **4.2 Write integration tests**
  - In `vmm_tests/vmm_tests/tests/tests/x86_64/storage.rs` (or new file):
  - Test: Boot Linux guest with virtio-blk disk, verify device appears as `/dev/vdX`
  - Test: Read/write data to virtio-blk disk from guest
  - Test: virtio-blk with ramdisk backend
  - Test: virtio-blk with file-backed disk
  - Test: virtio-blk read-only disk
  - Test: Flush/sync operations
  - Use same patterns as existing NVMe/SCSI tests (pipette agent, `new_test_vtl2_*` helpers)

### Phase 5: Unit Tests & Polish

- [ ] **5.1 Add unit tests**
  - Config space read/write tests
  - Request parsing tests (valid and malformed requests)
  - Feature negotiation tests
  - Bounds checking (sector out of range, etc.)

- [ ] **5.2 Add inspect support**
  - Implement `Inspect`/`InspectMut` for device state
  - Expose counters (read/write ops, bytes, errors)

- [ ] **5.3 Documentation**
  - Rustdoc for public APIs
  - Update Guide/ if needed (device support matrix, etc.)

## Notes

- See `ai/notes.md` for detailed codebase research and reference material
- The virtio PCI transport already exists; we just need to create the device and
  wrap it in `VirtioPciDeviceHandle`
- `RequestBuffers` bridge is the trickiest part — needs careful page alignment handling
- `VirtioDevice::enable()` receives `Resources` with queue params, features, interrupts;
  we spin up async worker tasks that poll the virtqueue for requests
- Guest memory is used both for reading descriptor data and for DMA (read/write disk data)
- Trust boundary: device must not panic on any guest input (malformed requests, OOB sectors, etc.)
