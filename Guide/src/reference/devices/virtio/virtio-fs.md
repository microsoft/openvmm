# virtio-fs

OpenVMM can expose a host directory to a Linux guest through `virtio-fs`.

## Standard machine

For the standard machine, `--virtio-fs` creates a HostFs device with a
caller-selected tag and host path:

```bash
openvmm --virtio-fs myfs,path/to/share
```

Mount it in the guest with the same tag:

```bash
mount -t virtiofs myfs /mnt/share
```

Standard-machine virtio-fs may use the normal PCI, VPCI, or MMIO placement
rules. It does not support snapshot and restore.

## microVM

The microVM profile reserves one fixed virtio-fs slot. Without `--mount`, the
slot is guest-discoverable but dormant and has no HostFs backend or filesystem
policy. Configure an active attachment with:

```bash
openvmm --machine microvm \
  --mount /mnt/share,path/to/share,ro \
  --mount-deny path/to/share/secrets \
  --kernel path/to/vmlinux --initrd path/to/initramfs.cpio.gz
```

The format is `GUEST_TARGET,HOST_PATH[,ro|rw]`. The default mode is `ro`;
read-write access must be explicit. The profile fixes the remaining
guest-visible configuration:

| Property | Value |
|---|---|
| Stable ID | `fs:microvm0` |
| Tag | `microvm` |
| Transport | virtio-mmio at `0xd0001000` |
| Interrupt | IRQ 6 |
| Queues | One high-priority and one request queue |
| Features | Indirect descriptors, event index, version 1, access-platform |
| DAX window | None |
| Cache policy | Zero entry and attribute lifetimes |
| File policy | Direct I/O |
| Maximum write | 1 MiB payload plus protocol headers |

For an active cold-boot attachment, the profile adds `virtfs_dir`,
`virtfs_tag`, and `virtfs_mode` bootstrap tokens to the kernel command line.
The fixed transport is always discoverable. Active attachment policy and the
canonical absolute host path become snapshot-authoritative.
Repeat `--mount-deny` to hide existing host files or directories. OpenVMM
canonicalizes each entry relative to the export and rejects paths outside the
root, the root itself, overlapping entries, symlink/reparse components, and
nested-mount crossings before opening the device.

`SectionFs`, aggregate roots, alternate tags, PCI transport, DAX, and extra
queues are not part of the microVM profile.

## Snapshot attachments

A microVM snapshot stores FUSE negotiation, namespace identifiers, lookup
counts, reopenable handles, directory cookies, and virtqueue progress. It
does not copy the host directory or serialize native file descriptors and
Windows handles.

An open directory continues from its bounded captured entry snapshot, so
later host additions do not appear midway through that enumeration. New
lookups and newly opened directories still observe the live host tree.

Restoring a snapshot captured with an active attachment requires a fresh
`--mount` argument and the exact same denied-path set, canonical host path,
guest target, and mode. Identity validation remains independent: before any vCPU starts,
OpenVMM pins the supplied root and validates its saved root and object
identities. Missing, moved, replaced, ambiguous, or no-longer-reopenable
objects fail restore.

A snapshot captured without `--mount` records the fixed slot as dormant. It
may restore without an attachment, or bind a new `--mount` attachment. Because
execution resumes after the cold-boot mount hook, the guest must mount the
newly attached backend explicitly:

```bash
mkdir -p /mnt/share
mount -t virtiofs microvm /mnt/share
```

Snapshots created before the dormant-slot capability cannot add a restore-time
attachment and fail with a compatibility error.

```admonish warning
An ordinary host directory is live external state. Host changes after capture
can be visible after restore or make identity validation fail. Restoring the
same read-write VM snapshot does not roll the host directory back.
Quiesce external host writers when deterministic replay is required.
```

The snapshot contract uses the `live-revalidate` policy. Immutable filesystem
generations and private writable clones are not currently exposed.

Snapshot destinations, restore directories, and explicit guest-memory backing
files must be outside the exported host tree. OpenVMM rejects configurations
that would expose guest RAM or snapshot files through virtio-fs.

## Security model

Guest FUSE requests, paths, and saved aliases are untrusted. HostFs rejects
absolute and parent-relative aliases, does not follow symbolic links or
Windows reparse points while resolving saved objects, and enforces read-only
mode before invoking a host mutation.

Denied paths are enforced in the server namespace rather than by guest mount
layout. Lookup and mutation operations reject denied prefixes, directory
enumeration omits their names, and denied root object identities reject
hard-link, junction, and bind-mount aliases. Mounting the same virtio-fs tag at
another guest path does not change the policy.

```admonish warning
The current cross-platform `LxVolume` interface does not provide fully
handle-relative component walking for every operation. Do not allow an
untrusted host process to concurrently replace or rename directories inside
the export; quiesce external namespace mutation during capture and restore.
```

Host filesystem behavior differs where Windows cannot represent a POSIX
operation. Unsupported operations return a Linux error rather than reporting
false success.

## Code references

- Device implementation:
  `vm/devices/virtio/virtiofs/`
- FUSE session implementation:
  `vm/devices/support/fs/fuse/`
- Resource contract:
  `vm/devices/virtio/virtio_resources/src/lib.rs` and
  `vm/devices/virtio/virtio_resources/src/fs/microvm.rs`
- microVM composition and restore attachment validation:
  `openvmm/openvmm_entry/src/microvm/filesystem.rs` and
  `openvmm/openvmm_entry/src/ttrpc/microvm.rs`
- Snapshot manifest and microVM filesystem contract:
  `openvmm/openvmm_helpers/src/snapshot.rs` and
  `openvmm/openvmm_helpers/src/snapshot/microvm.rs`
- [`virtiofs` rustdoc](https://openvmm.dev/rustdoc/linux/virtiofs/index.html)
