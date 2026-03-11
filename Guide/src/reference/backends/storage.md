# Storage backends

Storage backends implement the `DiskIo` trait, the shared abstraction that all storage frontends use to read and write data. A frontend holds a `Disk` handle and does not know what kind of backend is behind it — the same frontend code works with a local file, a Linux block device, a remote blob, or a layered composition of multiple backends.

## Backend catalog

| Backend | Crate | Wraps | Platform | Key characteristic |
|---------|-------|-------|----------|--------------------|
| FileDisk | `disk_file` | Host file | Cross-platform | Simplest backend. Blocking I/O via `unblock()`. |
| Vhd1Disk | `disk_vhd1` | VHD1 fixed file | Cross-platform | Parses VHD footer for geometry. |
| VhdmpDisk | `disk_vhdmp` | Windows vhdmp driver | Windows | Dynamic and differencing VHD/VHDX. |
| BlobDisk | `disk_blob` | HTTP / Azure Blob | Cross-platform | Read-only. HTTP range requests. |
| BlockDeviceDisk | `disk_blockdevice` | Linux block device | Linux | io_uring, resize via uevent, PR passthrough. |
| NvmeDisk | `disk_nvme` | Physical NVMe (VFIO) | Linux/Windows | User-mode NVMe driver. Resize via AEN. |
| StripedDisk | `disk_striped` | Multiple Disks | Cross-platform | Stripes data across underlying disks. |

## Decorators

Decorators wrap another `Disk` and transform I/O in transit. Features compose by stacking decorators without modifying the backends underneath.

| Decorator | Crate | Transform |
|-----------|-------|-----------|
| CryptDisk | `disk_crypt` | XTS-AES-256 encryption. Encrypts on write, decrypts on read. |
| DelayDisk | `disk_delay` | Adds configurable latency to each I/O operation. |
| DiskWithReservations | `disk_prwrap` | In-memory SCSI persistent reservation emulation. |

## Layered disks

A layered disk composes multiple layers into a single `DiskIo` implementation. Each layer tracks which sectors it has; reads fall through from top to bottom until a layer has the requested data. This powers the `memdiff:` and `mem:` CLI options.

Two layer implementations exist today:

- **RamDiskLayer** (`disklayer_ram`) — ephemeral, in-memory.
- **SqliteDiskLayer** (`disklayer_sqlite`) — persistent, file-backed (dev/test only).

```admonish note title="See also"
[Storage Pipeline](../architecture/devices/storage.md) for the full architecture: how frontends, backends, decorators, and the layered disk model connect, plus cross-cutting concerns like online disk resize and virtual optical media.
```
