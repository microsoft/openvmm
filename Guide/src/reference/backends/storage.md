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
| VhdxDisk | [`vhdx`](vhdx.md) | VHDX file | Cross-platform | Pure-Rust VHDX parser. Dynamic, fixed, and differencing. |
| BlobDisk | [`disk_blob`](https://openvmm.dev/rustdoc/linux/disk_blob/index.html) | HTTP / Azure Blob | Cross-platform | Read-only. HTTP range requests. |
| BlockDeviceDisk | [`disk_blockdevice`](https://openvmm.dev/rustdoc/linux/disk_blockdevice/index.html) | Linux block device or file | Linux | io_uring, resize via uevent, PR passthrough. Default for raw files on Linux in both OpenHCL and OpenVMM. |
| NvmeDisk | [`disk_nvme`](https://openvmm.dev/rustdoc/linux/disk_nvme/index.html) | Physical NVMe (VFIO) | Linux/Windows | User-mode NVMe driver. Resize via AEN. |
| StripedDisk | [`disk_striped`](https://openvmm.dev/rustdoc/linux/disk_striped/index.html) | Multiple Disks | Cross-platform | Stripes data across underlying disks. |

On Windows, `disk_vhdmp` forwards guest discard through
[`RawSCSIVirtualDisk`](https://learn.microsoft.com/en-us/windows/win32/api/virtdisk/nf-virtdisk-rawscsivirtualdisk)
using the existing virtual-disk handle, without exposing a host disk device.
It queries SCSI provisioning and block-limit pages before enabling UNMAP,
and only promises zero reads when the driver advertises that guarantee.
Discard makes space eligible for reclamation; shrinking the backing file may
still require a separate compaction operation.

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

## Test disks

Use [`disk_hook`](https://openvmm.dev/rustdoc/linux/disk_hook/index.html)
as a dev-dependency when a test needs a configurable disk or I/O hooks.
`HookDisk::new` creates a sparse RAM disk using `disklayer_ram`, and
`HookDisk::wrap` decorates an existing `Disk` while forwarding its
capabilities. Use `new` directly instead of wrapping a RAM disk.

Configure a `HookDisk`, then call `into_disk` and keep the returned `Disk`
in fixtures. Cloning that handle is cheap and shares storage and hook
state. The crate re-exports `Disk` and `DiskError`, so callers that
need to name those types do not need a direct `disk_backend` dependency.

`with_sync_hook` and `with_async_hook` install a callback for recording
operations or injecting failures. Either method replaces any previously
installed hook. Hooks run before each operation, receive operation
parameters rather than disk contents, and can return a `DiskError` to
prevent the operation. Returning `Ok(())` continues normal I/O.

Metadata overrides change what the frontend sees, not the backing
disk's capabilities. In particular, only advertise zeroing unmaps if
the backend implements them. Ignored unmaps are not forwarded by `Disk`
and do not invoke the hook.
