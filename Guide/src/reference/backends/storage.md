# Storage backends

Storage backends implement the
[`DiskIo`](https://openvmm.dev/rustdoc/linux/disk_backend/trait.DiskIo.html)
trait, the shared abstraction that all storage frontends use to read
and write data. A frontend holds a
[`Disk`](https://openvmm.dev/rustdoc/linux/disk_backend/struct.Disk.html)
handle and doesn't know what kind of backend is behind it — the same
frontend code works with a local file, a Linux block device, a remote
blob, or a layered composition of multiple backends.

## Backend catalog

| Backend | Crate | Wraps | Platform | Key characteristic |
|---------|-------|-------|----------|--------------------|
| FileDisk | [`disk_file`](https://openvmm.dev/rustdoc/linux/disk_file/index.html) | Host file | Cross-platform | Simplest backend. Blocking I/O via `unblock()`. |
| Vhd1Disk | [`disk_vhd1`](https://openvmm.dev/rustdoc/linux/disk_vhd1/index.html) | VHD1 fixed file | Cross-platform | Parses VHD footer for geometry. |
| VhdmpDisk | `disk_vhdmp` | Windows vhdmp driver | Windows | Dynamic and differencing VHD/VHDX. |
| BlobDisk | [`disk_blob`](https://openvmm.dev/rustdoc/linux/disk_blob/index.html) | HTTP / Azure Blob | Cross-platform | Read-only. HTTP range requests. |
| BlockDeviceDisk | [`disk_blockdevice`](https://openvmm.dev/rustdoc/linux/disk_blockdevice/index.html) | Linux block device | Linux | io_uring, resize via uevent, PR passthrough. |
| NvmeDisk | [`disk_nvme`](https://openvmm.dev/rustdoc/linux/disk_nvme/index.html) | Physical NVMe (VFIO) | Linux/Windows | User-mode NVMe driver. Resize via AEN. |
| StripedDisk | [`disk_striped`](https://openvmm.dev/rustdoc/linux/disk_striped/index.html) | Multiple Disks | Cross-platform | Stripes data across underlying disks. |

## Decorators

Decorators wrap another
[`Disk`](https://openvmm.dev/rustdoc/linux/disk_backend/struct.Disk.html)
and transform I/O in transit. Features compose by stacking decorators
without modifying the backends underneath.

| Decorator | Crate | Transform |
|-----------|-------|-----------|
| CryptDisk | [`disk_crypt`](https://openvmm.dev/rustdoc/linux/disk_crypt/index.html) | XTS-AES-256 encryption. Encrypts on write, decrypts on read. |
| DelayDisk | [`disk_delay`](https://openvmm.dev/rustdoc/linux/disk_delay/index.html) | Adds configurable latency to each I/O operation. |
| DiskWithReservations | [`disk_prwrap`](https://openvmm.dev/rustdoc/linux/disk_prwrap/index.html) | In-memory SCSI persistent reservation emulation. |

## Layered disks

A [`LayeredDisk`](https://openvmm.dev/rustdoc/linux/disk_layered/index.html)
composes multiple layers into a single `DiskIo` implementation. Each
layer tracks which sectors it has; reads fall through from top to
bottom until a layer has the requested data. This powers the
`memdiff:` and `mem:` CLI options.

Two layer implementations exist today:

- **RamDiskLayer** ([`disklayer_ram`](https://openvmm.dev/rustdoc/linux/disklayer_ram/index.html)) — ephemeral, in-memory.
- **SqliteDiskLayer** ([`disklayer_sqlite`](https://openvmm.dev/rustdoc/linux/disklayer_sqlite/index.html)) — persistent, file-backed (dev/test only).

The [storage pipeline](../architecture/devices/storage.md) page covers
the full architecture: how frontends, backends, decorators, and the
layered disk model connect, plus cross-cutting concerns like online
disk resize and virtual optical media.
