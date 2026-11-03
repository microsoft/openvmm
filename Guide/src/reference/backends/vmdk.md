# VMDK support

The `disk_vmdk` crate (`vm/devices/storage/disk_vmdk/`) provides OpenVMM's
VMDK integration. Its common entry point returns a concrete disk resource.
OpenVMM currently supports the read-only `monolithicFlat` and
`twoGbMaxExtentFlat` VMDK disk types on every supported host platform.

## Disk type support

| Disk type or feature | Support | Access | Notes |
|----------------------|---------|--------|-------|
| `monolithicFlat` | Supported | Read-only | Exactly one `FLAT` extent |
| `twoGbMaxExtentFlat` | Supported | Read-only | One or more ordered `FLAT` extents |
| `monolithicSparse` | Not supported | — | Sparse grains are not implemented |
| `twoGbMaxExtentSparse` | Not supported | — | Sparse grains are not implemented |
| `streamOptimized` | Not supported | — | Compressed grains are not implemented |
| Parent-backed chains | Not supported | — | Parent descriptors are rejected |

Creating VMDKs and using direct I/O with them are also not supported.

## Architecture

The public `open_vmdk` function is the common VMDK boundary. The current
implementation delegates to the flat disk-type opener, which produces a
flat-specific resource:

```text
open_vmdk
    |
    +-- supported flat disk type
          |
          +-- FlatVmdkDiskHandle
                |
                +-- FlatVmdkResolver
                      |
                      +-- FlatVmdkDisk
```

The opener validates the descriptor and opens its sources before constructing
the `FlatVmdkDiskHandle`. The VM worker receives that resource and uses
`FlatVmdkResolver` to repeat the layout validation and construct the
`FlatVmdkDisk` implementation of `DiskIo`.

This boundary keeps disk-type-specific validation and resources out of storage
frontends. Additional VMDK disk types can provide their own resources and
resolvers without changing the frontend interfaces.

## Flat VMDK

### Descriptor format

The `createType` descriptor field selects the virtual disk type. The flat
implementation accepts version 1 descriptors selecting `monolithicFlat` or
`twoGbMaxExtentFlat`. Extent lines may use the `RW` or `RDONLY` access mode,
but OpenVMM opens every source read-only regardless of the declared mode.

Zero-filled, inaccessible, sparse, and parent-backed extents are rejected.

```text
# Disk DescriptorFile
version=1
parentCID=ffffffff
createType="twoGbMaxExtentFlat"

# Extent description
RW 2048 FLAT "metadata.img" 0
RDONLY 4096 FLAT "payload.img" 8
```

The virtual disk concatenates extents in descriptor order. In this example,
the first 2048 virtual sectors come from sector 0 of `metadata.img`, followed
by 4096 sectors starting at sector 8 of `payload.img`.

The first line must be `# Disk DescriptorFile`. Every other line starting with
`#`, including section markers, are treated as a comment. Lines starting with an access
mode are considered to be extents and parsed as such.

### Descriptor and source paths

Descriptor and source paths may be absolute or relative:

- A relative descriptor path is resolved against the process's current
  working directory.
- A relative source path is resolved against the directory containing the
  descriptor.
- Source paths written in a descriptor must be quoted.

The descriptor and every source must be regular files. Each extent must fit
within its source file. Multiple extents may use different ranges of the same
source file; extents that name the same path share one open file.

### Resource transport

Opening a flat VMDK uses the following resource flow:

1. Open, read, and parse the descriptor, which is limited to 1 MiB.
2. Open each distinct source path read-only.
3. Convert the ordered extents and open files into a `FlatVmdkDiskHandle`.
4. Pass that handle directly to the VM worker in single-process mode, or
   transport it through mesh in multi-process mode.
5. Revalidate the complete layout in the worker before exposing the disk.

Transporting open files preserves the identity of the opened sources. The
VM worker revalidates the untrusted handle instead of reopening paths.

```admonish warning
In multi-process mode, one mesh message can carry at most 64 file handles,
shared with other resources in that message. A VMDK with more than 64 distinct
sources requires `--single-process`; one near the limit may require it as well.
```

### I/O behavior

#### Sector geometry

- Extent counts and source offsets use fixed 512-byte sectors.
- The backend reports 512-byte logical and physical sector sizes.
- Virtual capacity and source lengths are fixed when the disk is opened.
- Online resize is not supported.

#### Reads

- A request may cross any number of extent boundaries. OpenVMM splits it into
  source-file ranges.
- On Linux, eligible requests containing a whole number of sectors use the
  VMDK-local byte-oriented `io_uring` reader when native setup succeeds.
  These reads transfer data directly into lockable guest memory, including
  when a backing file ends partway through a sector.
  Large requests use multiple native submissions when needed to respect
  kernel buffer-count and length limits.
- Other reads use positional file I/O on a blocking worker. OpenVMM assembles
  the request in a host buffer and then copies it into guest memory.
  At most 1 MiB of temporary-buffer capacity is retained per disk for reuse;
  larger temporary buffers are released after the read.
- Separate guest read requests may execute concurrently.
- Host file access is buffered. Direct I/O is not enabled.
- If a source ends partway through its final sector, the unread portion is
  returned as zeroes.
- If a source is truncated after opening, reads of the removed data fail with
  an I/O error.

#### Read-only behavior

- Writes are always rejected.
- Cache synchronization and unmap requests are harmless no-ops because the
  sources are read-only.

## Using VMDK with OpenVMM

File disks are writable by default, so a supported VMDK attached without an
overlay must use the `ro` flag:

```bash
cargo run -- --uefi \
  --vmbus-scsi id=scsi0 \
  --disk file:path/to/disk.vmdk,ro,on=scsi0
```

## Related pages

- [Storage backends](./storage.md) — catalog of all storage backends
- [Storage pipeline](../architecture/devices/storage.md) — how frontends,
  backends, and layers connect
- [Running OpenVMM](../../user_guide/openvmm/run.md) — VM launch examples
- [`disk_vmdk` rustdoc](https://openvmm.dev/rustdoc/linux/disk_vmdk/index.html)
