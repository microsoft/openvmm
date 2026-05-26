# Storage pipeline

The storage stack carries guest I/O requests from a guest-visible
controller to a backing store and back. It's shared between OpenVMM
and OpenHCL. Every disk backend implements the
[`DiskIo`](https://openvmm.dev/rustdoc/linux/disk_backend/trait.DiskIo.html)
trait, and frontends hold a
[`Disk`](https://openvmm.dev/rustdoc/linux/disk_backend/struct.Disk.html)
wrapper — a cheap, cloneable handle to any backend. For the `DiskIo`
trait surface, method contracts, and error model, see the
[`disk_backend` rustdoc](https://openvmm.dev/rustdoc/linux/disk_backend/index.html).

## The pipeline

Every storage I/O flows through the same layered pipeline:

```text
  ┌──────────────────────────────────────────────────────────┐
  │  Guest I/O                                               │
  └────────────────────────┬─────────────────────────────────┘
                           │
  ┌────────────────────────┼─────────────────────────────────┐
  │  Frontend              │                                 │
  │  (NVMe · StorVSP · IDE)│                                 │
  └────┬───────────────────┼────────────────────┬────────────┘
       │                   │                    │
       │ NVMe: direct      │ SCSI / IDE         │
       │                   ▼                    │
       │       ┌────────────────────────┐       │
       │       │ SCSI adapter           │       │
       │       │ (SimpleScsiDisk /      │       │
       │       │  SimpleScsiDvd)        │       │
       │       └───────────┬────────────┘       │
       │                   │                    │
       ▼                   ▼                    ▼
  ┌──────────────────────────────────────────────────────────┐
  │  Disk  (DiskIo trait boundary)                           │
  └────────────────────────┬─────────────────────────────────┘
                           │
  ┌────────────────────────┼─────────────────────────────────┐
  │  Decorator wrappers    │  (optional: crypt · delay · PR) │
  └────────────────────────┼─────────────────────────────────┘
                           │
            ┌──────────────┴──────────────┐
            ▼                             ▼
  ┌──────────────────┐      ┌──────────────────────────────┐
  │  Backend         │      │  Layered disk                │
  │  (file · block   │      │  (optional: RAM + backing)   │
  │   device · blob  │      │    ├── Layer 0 (RAM/sqlite)  │
  │   · VHD · ...)   │      │    └── Layer 1 (backend)     │
  └──────────────────┘      └──────────────────────────────┘
```

Key vocabulary:

- **Frontend.** Speaks a guest-visible storage protocol and translates
  requests into `DiskIo` calls.
- **SCSI adapter.** For the SCSI and IDE paths, an intermediate layer
  ([`SimpleScsiDisk`](https://openvmm.dev/rustdoc/linux/scsidisk/struct.SimpleScsiDisk.html)
  or
  [`SimpleScsiDvd`](https://openvmm.dev/rustdoc/linux/scsidisk/scsidvd/struct.SimpleScsiDvd.html))
  that parses SCSI CDB opcodes before calling `DiskIo`.
- **Backend.** A `DiskIo` implementation that reads and writes to a
  specific backing store.
- **Decorator.** A `DiskIo` implementation that wraps another `Disk`
  and transforms I/O in transit (encryption, delay, persistent
  reservations).
- **Layered disk.** A `DiskIo` implementation composed of ordered
  layers with per-sector presence tracking.

## Frontends

Three frontends exist. Each speaks a different guest-visible protocol
but they all produce `DiskIo` calls on the backend side.

| Frontend | Protocol | Transport | Crate |
|----------|----------|-----------|-------|
| [NVMe](../../emulated/NVMe/overview.md) | NVMe 2.0 | PCI MMIO + MSI-X | [`nvme`](https://openvmm.dev/rustdoc/linux/nvme/index.html) |
| [StorVSP](../../devices/vmbus/storvsp.md) | SCSI CDB over VMBus | VMBus ring buffers | [`storvsp`](https://openvmm.dev/rustdoc/linux/storvsp/index.html) |
| [IDE](../../emulated/legacy_x86/ide.md) | ATA / ATAPI | PCI/ISA I/O ports + DMA | [`ide`](https://openvmm.dev/rustdoc/linux/ide/index.html) |

**NVMe** is the simplest path. The NVMe controller's namespace directly holds a `Disk`. NVM opcodes (READ, WRITE, FLUSH, DSM) map nearly 1:1 to `DiskIo` methods. The FUA bit from the NVMe write command is forwarded directly.

**StorVSP / SCSI** has a two-layer design. StorVSP handles the VMBus
transport — negotiation, ring buffer management,
[sub-channel allocation](../../devices/vmbus/storvsp_channels.md). It
dispatches each SCSI request to an
[`AsyncScsiDisk`](https://openvmm.dev/rustdoc/linux/scsi_core/trait.AsyncScsiDisk.html)
implementation. For hard drives, that's
[`SimpleScsiDisk`](https://openvmm.dev/rustdoc/linux/scsidisk/struct.SimpleScsiDisk.html),
which parses the SCSI CDB and translates it to `DiskIo` calls. For
optical drives, it's
[`SimpleScsiDvd`](https://openvmm.dev/rustdoc/linux/scsidisk/scsidvd/struct.SimpleScsiDvd.html).

**IDE** is the legacy path. ATA commands for hard drives call `DiskIo` directly. ATAPI commands for optical drives delegate to `SimpleScsiDvd` through an ATAPI-to-SCSI translation layer — the same DVD implementation that StorVSP uses. IDE also supports [enlightened INT13 commands](../../emulated/legacy_x86/ide.md#enlightened-io), a Microsoft-specific optimization that collapses the multi-exit register-programming sequence into a single VM exit.

## Backends

A backend is a `DiskIo` implementation that reads and writes to a specific backing store. Backends are interchangeable — swap one for another without changing the frontend. The frontend holds a `Disk` and doesn't know what's behind it. See the [storage backends](../../backends/storage.md) page for the full catalog and platform details.

## Decorators

A decorator is a `DiskIo` implementation that wraps another `Disk` and transforms I/O in transit. Features compose by stacking decorators without modifying backends:

```text
  CryptDisk
    └── BlockDeviceDisk
```

Three decorators exist: [`CryptDisk`](https://openvmm.dev/rustdoc/linux/disk_crypt/struct.CryptDisk.html) (XTS-AES-256 encryption), [`DelayDisk`](https://openvmm.dev/rustdoc/linux/disk_delay/struct.DelayDisk.html) (injected latency), and [`DiskWithReservations`](https://openvmm.dev/rustdoc/linux/disk_prwrap/struct.DiskWithReservations.html) (in-memory persistent reservation emulation). All three forward metadata (sector count, sector size, disk ID, `wait_resize`) to the inner disk unchanged. See the [storage backends](../../backends/storage.md) page for the decorator catalog.

## The layered disk model

A [`LayeredDisk`](https://openvmm.dev/rustdoc/linux/disk_layered/struct.LayeredDisk.html) is a `DiskIo` implementation composed of multiple layers, ordered from top to bottom. Each layer is a block device with per-sector *presence* tracking. This model powers diff disks, RAM overlays, and caching.

### Reads fall through

When a read arrives, the layered disk checks layers top-to-bottom. The first layer that has the requested sectors provides the data. Sectors not present in any layer are zeroed.

### Writes go to the top

Writes always go to the topmost layer. If that layer is configured with *write-through*, the write also propagates to the next layer.

### Read caching

A layer can be configured to cache read misses: when sectors are fetched from a lower layer, they're written back to the cache layer. This uses a `write_no_overwrite` operation to avoid overwriting sectors that were written between the read and the cache population.

### Layer implementations

Two concrete layers exist today:

- **RamDiskLayer** ([`disklayer_ram`](https://openvmm.dev/rustdoc/linux/disklayer_ram/index.html)) — ephemeral, in-memory. Data is stored in a `BTreeMap` keyed by sector number. Fast, but lost when the VM stops.
- **SqliteDiskLayer** ([`disklayer_sqlite`](https://openvmm.dev/rustdoc/linux/disklayer_sqlite/index.html)) — persistent, backed by a SQLite database (`.dbhd` file). Designed for dev/test scenarios — no stability guarantees on the on-disk format.

A full `Disk` can appear at the bottom of the stack as a fully-present layer (`DiskAsLayer`). This is the typical case: a RAM or sqlite layer on top of a file or block device.

### Worked example: `memdiff:file:disk.vhdx`

```text
  Layer 0: RamDiskLayer (empty, writable)
  Layer 1: DiskAsLayer wrapping FileDisk (fully present, read-only
           from the layered disk's perspective)
```

- Guest write → sector goes to the RAM layer.
- Guest read → check RAM; if the sector is present, return it. If absent, fall through to the file.
- Sectors absent from both layers → zero-filled.

Changes are ephemeral — they live in the RAM layer and are lost when the VM stops. The [Running OpenVMM](../../../user_guide/openvmm/run.md) page shows concrete `memdiff:` examples.

## How configuration becomes a concrete stack

The resource resolver connects configuration (CLI flags, VTL2 settings) to concrete backends. A resource *handle* describes what backend to use; a *resolver* creates it.

The storage resolver chain is recursive. An NVMe controller resolves each namespace's disk, which may be a layered disk, which resolves each layer in parallel, which may itself be a disk that needs resolving.

**Example:** `--disk memdiff:file:path/to/disk.vhdx`

1. CLI parses this into a `LayeredDiskHandle` with two layers:
   - Layer 0: `RamDiskLayerHandle { len: None, sector_size: None }` (RAM diff, inherits size and sector size from backing disk)
   - Layer 1: `DiskLayerHandle(FileDiskHandle(...))` (the file)
2. The layered disk resolver resolves both layers in parallel.
3. The RAM layer attaches on top of the file layer, inheriting its sector size and capacity.
4. The resulting `LayeredDisk` is wrapped in a `Disk` and handed to the NVMe namespace or SCSI controller.

For the OpenHCL settings model (`StorageController`, `Lun`, `PhysicalDevice`), see [Storage Translation](../openhcl/storage_translation.md) and [Storage Configuration Model](../openhcl/storage_configuration.md).

## Backend catalog

| Backend | Crate | Wraps | Platform | Note |
|---------|-------|-------|----------|------|
| FileDisk | [`disk_file`](https://openvmm.dev/rustdoc/linux/disk_file/index.html) | Host file | Cross-platform | Simplest backend |
| Vhd1Disk | [`disk_vhd1`](https://openvmm.dev/rustdoc/linux/disk_vhd1/index.html) | VHD1 fixed file | Cross-platform | Parses VHD footer |
| VhdmpDisk | `disk_vhdmp` | Windows vhdmp driver | Windows | Dynamic/differencing VHD/VHDX |
| BlobDisk | [`disk_blob`](https://openvmm.dev/rustdoc/linux/disk_blob/index.html) | HTTP / Azure Blob | Cross-platform | Read-only, HTTP range requests |
| BlockDeviceDisk | [`disk_blockdevice`](https://openvmm.dev/rustdoc/linux/disk_blockdevice/index.html) | Linux block device | Linux | io_uring, resize via uevent, PR passthrough |
| NvmeDisk | [`disk_nvme`](https://openvmm.dev/rustdoc/linux/disk_nvme/index.html) | Physical NVMe (VFIO) | Linux/Windows | User-mode NVMe driver, resize via AEN |
| StripedDisk | [`disk_striped`](https://openvmm.dev/rustdoc/linux/disk_striped/index.html) | Multiple Disks | Cross-platform | Data striping |

## Online disk resize

Disk resize is a cross-cutting concern that spans backends and frontends.

### Backend detection

Only two backends detect capacity changes at runtime:

- **BlockDeviceDisk** — listens for Linux uevent notifications on the block device. When the host resizes the device, a uevent fires, the backend re-queries the size via ioctl, and `wait_resize` completes.
- **NvmeDisk** — the user-mode NVMe driver monitors Async Event Notifications (AEN) from the physical controller and rescans namespace capacity.

All other backends default to never signaling (`wait_resize` returns `pending()`). Decorators and layered disks delegate `wait_resize` to the inner backend.

```admonish warning
`FileDisk` never signals resize. If you attach a file backend and resize the file at runtime, nothing happens — the guest won't be notified. Use `BlockDeviceDisk` or `NvmeDisk` for runtime resize.
```

### Frontend notification

Once a backend detects a resize, the frontend notifies the guest:

| Frontend | Mechanism | How it works |
|----------|-----------|-------------|
| NVMe | Async Event Notification | Background task per namespace calls `wait_resize`. On change, completes a queued AER command with a changed-namespace-list log page. Guest re-identifies the namespace. |
| StorVSP / SCSI | UNIT_ATTENTION | On the next SCSI command after a resize, `SimpleScsiDisk` detects the capacity change and returns CHECK_CONDITION with UNIT_ATTENTION / CAPACITY_DATA_CHANGED. Guest retries and re-reads capacity. |
| IDE | Not supported | IDE has no capacity-change notification mechanism. |

The resize path is the same in OpenHCL and standalone — `BlockDeviceDisk` detects the uevent from the host, `wait_resize` completes, and the frontend notifies the guest through the standard mechanism. No special paravisor-level interception.

## Virtual optical / DVD

DVD and CD-ROM drives use a different model from disk devices.

[`SimpleScsiDvd`](https://openvmm.dev/rustdoc/linux/scsidisk/scsidvd/struct.SimpleScsiDvd.html) implements `AsyncScsiDisk` and manages media state: a disk can be `Loaded` or `Unloaded`. Optical media always uses a 2048-byte sector size. The implementation handles optical-specific SCSI commands: `GET_EVENT_STATUS_NOTIFICATION`, `GET_CONFIGURATION`, `START_STOP_UNIT` (eject), and media change events.

### Eject

Two eject paths exist:

- **Guest-initiated** (SCSI `START_STOP_UNIT` with the load/eject flag): the DVD handler checks the prevent flag, replaces media with `Unloaded`, and calls `disk.eject()`. Once ejected via SCSI, the media is **permanently removed** for the VM lifetime.
- **Host-initiated** (`change_media` via the resolver's background task): can insert new media or remove existing media dynamically.

### Frontend support

| Frontend | DVD support | How |
|----------|-------------|-----|
| StorVSP / SCSI | Yes | `SimpleScsiDvd` implements `AsyncScsiDisk` directly. |
| IDE | Yes | ATAPI wraps `SimpleScsiDvd` through the ATAPI-to-SCSI layer. |
| NVMe | No | NVMe has no removable media concept. Explicitly rejected. |

### CLI

- `--disk file:my.iso,dvd` → SCSI optical drive.
- `--ide file:my.iso,dvd` → IDE optical drive (ATAPI).

The `dvd` flag implicitly sets `read_only = true`.

## `mem:` and `memdiff:` CLI mapping

Both CLI options map to the layered disk model:

- **`mem:1G`** creates a single-layer `LayeredDisk` with a `RamDiskLayer` sized to 1 GB. No backing disk — the RAM layer is the entire disk.
- **`memdiff:file:disk.vhdx`** creates a two-layer `LayeredDisk`: a `RamDiskLayer` (inheriting size from the backing disk) on top of the file. Writes go to the RAM layer; reads fall through to the file for sectors not yet written.

Both use `RamDiskLayerHandle` under the hood. The difference is `len: Some(size)` for `mem:` (standalone RAM disk with explicit size) vs. `len: None` for `memdiff:` (inherits from backing disk). The optional `sector_size` field (default `None`) lets you override the sector size; when `None`, it inherits from the lower layer or defaults to 512 bytes. The [Running OpenVMM](../../../user_guide/openvmm/run.md) page shows concrete examples.

## Controller identity and Azure disk classification

In Azure, which controller a disk sits on is a de facto compatibility boundary. Azure VMs present four SCSI controllers (this may change), each with a distinct instance ID. One controller carries the OS disk, resource (temporary) disk, and related infrastructure disks; a separate controller carries remote data disks. For Gen1 VMs, the IDE controllers logically replace that first SCSI controller, while data disks remain on SCSI.

Guest agents use controller identity to classify disks. The [azure-vm-utils udev rules](https://github.com/Azure/azure-vm-utils/blob/main/udev/80-azure-disk.rules) match on SCSI controller instance IDs to create stable symlinks under `/dev/disk/azure/`. Moving a disk from one StorVSP controller instance to another changes its classification and can break guest-side automation. For SCSI disk mapping details, see the [Azure disk mapping docs](https://learn.microsoft.com/en-us/azure/virtual-machines/windows/azure-to-guest-disk-mapping).

For NVMe, the mapping uses namespace IDs: NSID 1 is the OS disk, NSID 2+ are data disks (portal LUN = NSID − 2). On newer VM sizes (v7+), disks are split across multiple NVMe controllers by caching policy. NVMe is Gen2-only. See the [NVMe overview](https://learn.microsoft.com/en-us/azure/virtual-machines/nvme-overview) and [NVMe disk identification FAQ](https://learn.microsoft.com/en-us/azure/virtual-machines/enable-nvme-remote-faqs) for the full Azure perspective.

## Implementation map

| Component | Why read it | Source | Rustdoc |
|-----------|-------------|--------|---------|
| `disk_backend` | `DiskIo` trait, `Disk` wrapper, error model | [source](https://github.com/microsoft/openvmm/blob/main/vm/devices/storage/disk_backend/src/lib.rs) | [rustdoc](https://openvmm.dev/rustdoc/linux/disk_backend/index.html) |
| `disk_layered` | Layered disk, `LayerIo` trait, bitmap tracking | [source](https://github.com/microsoft/openvmm/blob/main/vm/devices/storage/disk_layered/src/lib.rs) | [rustdoc](https://openvmm.dev/rustdoc/linux/disk_layered/index.html) |
| `nvme` | NVMe controller emulator | [source](https://github.com/microsoft/openvmm/blob/main/vm/devices/storage/nvme/src/lib.rs) | [rustdoc](https://openvmm.dev/rustdoc/linux/nvme/index.html) |
| `storvsp` | VMBus SCSI controller | [source](https://github.com/microsoft/openvmm/blob/main/vm/devices/storage/storvsp/src/lib.rs) | [rustdoc](https://openvmm.dev/rustdoc/linux/storvsp/index.html) |
| `scsidisk` | SCSI CDB parser (`SimpleScsiDisk`, `SimpleScsiDvd`) | [source](https://github.com/microsoft/openvmm/blob/main/vm/devices/storage/scsidisk/src/lib.rs) | [rustdoc](https://openvmm.dev/rustdoc/linux/scsidisk/index.html) |
| `ide` | IDE controller emulator | [source](https://github.com/microsoft/openvmm/blob/main/vm/devices/storage/ide/src/lib.rs) | [rustdoc](https://openvmm.dev/rustdoc/linux/ide/index.html) |
| `scsi_core` | `AsyncScsiDisk` trait, `Request`, `ScsiResult` | [source](https://github.com/microsoft/openvmm/blob/main/vm/devices/storage/scsi_core/src/lib.rs) | [rustdoc](https://openvmm.dev/rustdoc/linux/scsi_core/index.html) |
