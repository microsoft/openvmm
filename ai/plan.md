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

- [x] **1.1 Create `virtio_blk` crate skeleton**
- [x] **1.2 Define virtio-blk spec constants (`spec.rs`)**
- [x] **1.3 Implement the `VirtioDevice` trait (`lib.rs`)**
- [x] **1.4 Implement request processing worker**
- [x] **1.5 Descriptor-to-RequestBuffers translation**

### Phase 2: Resource Plumbing

- [x] **2.1 Add resource handle to `virtio_resources`**
- [x] **2.2 Create resolver (`resolver.rs`)**
- [x] **2.3 Register resolver in `openvmm_resources`**

### Phase 3: CLI/Configuration Integration

- [x] **3.1 Add VirtioBlk to `DiskLocation` enum and StorageBuilder**
- [x] **3.2 Add `--virtio-blk` CLI argument**

### Phase 4: Petri Test Integration

- [x] **4.1 Add VirtioBlk to petri storage types**
- [x] **4.2 Write integration test (`virtio_blk_device`)**

### Phase 5: Build & Verification

- [x] **5.1 All affected crates build cleanly**
- [x] **5.2 `cargo xtask fmt --fix` passes**
- [x] **5.3 `cargo doc` passes**

## Notes

- See `ai/notes.md` for detailed codebase research and reference material
- The virtio PCI transport already exists; we just need to create the device and
  wrap it in `VirtioPciDeviceHandle`
- `RequestBuffers` bridge is the trickiest part — needs careful page alignment handling
- `VirtioDevice::enable()` receives `Resources` with queue params, features, interrupts;
  we spin up async worker tasks that poll the virtqueue for requests
- Guest memory is used both for reading descriptor data and for DMA (read/write disk data)
- Trust boundary: device must not panic on any guest input (malformed requests, OOB sectors, etc.)
