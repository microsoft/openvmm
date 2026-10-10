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

The microVM profile defines one fixed virtio-fs slot. A cold boot always
exposes it; without `--mount` or `--mount-aggregate`, it is guest-discoverable
but dormant and has no HostFs backend or filesystem policy. Configure an
active attachment with:

```bash
openvmm --machine microvm \
  --mount /mnt/share,path/to/share,ro \
  --mount-deny path/to/share/secrets \
  --kernel path/to/vmlinux --initrd path/to/initramfs.cpio.gz
```

The format is `GUEST_TARGET,HOST_PATH[,ro|rw]`. The default mode is `ro`;
read-write access must be explicit. `--mount` may appear once. The profile
fixes the remaining guest-visible configuration:

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
| Symbolic links | `rw`: created with the exact target; `ro`: `EROFS` |

### Several host directories

To share several host directories, or single host files, attach them to the
slot as one aggregate instead of `--mount`. `--mount-aggregate GUEST_TARGET`
sets where the guest mounts the aggregate's root, and each
`--mount-child NAME,HOST_PATH[,ro|rw][,file]` adds a host directory as the
root's child directory `NAME`, or, with `file`, a regular file as the child
file `NAME` (see [Single files](#single-files)), with its own access mode:

```bash
openvmm --machine microvm \
  --mount-aggregate /run/shares \
  --mount-child workspace,path/to/workspace,rw \
  --mount-child toolcache,path/to/toolcache,ro \
  --kernel path/to/vmlinux --initrd path/to/initramfs.cpio.gz
```

The guest sees the children in command-line order. A name is 1 to 64 ASCII
letters, digits, `.`, `_`, or `-`, and must be unique; an aggregate has 1 to
256 children. The mode, and the `file` flag after it, follow the last
commas, so a host path may contain commas when the mode is given.

The aggregate's root is synthetic. It is read-only, it belongs to the guest's
root user, and its mode is `0500`, so only that user can enter it; the guest
is expected to bind-mount each child where its workloads need it. Every
modification of the root fails with `EROFS`. Each child is a separate HostFs
volume that enforces its own access mode and policy, so a guest cannot write
to a read-only child through any path. A rename between two children fails
with `EXDEV`, as one across mounts does. A hard link between two children
fails with `EROFS` when its destination is read-only, as Linux reports a
read-only destination first, and with `EXDEV` otherwise. Inode numbers are
namespaced per child, which makes a collision between children unlikely but
not impossible.

OpenVMM rejects host paths that equal, contain, or are contained in one
another (compared after canonicalization and by root object identity),
because one child could otherwise reach files that another child denies or
exposes with a different access mode. On Linux, OpenVMM also compares the
filesystem sources that each path reaches, from `/proc/self/mountinfo`,
including the mounts below it, so a bind mount or nested mount can't expose
part of one child as, or inside, another. Each `--mount-deny`,
`--mount-allow`, and `--mount-write` path must be absolute, and it applies to
the child whose directory contains it. `--mount-owner` applies to every
child; with `caller`, every child directory and file must have the same
owner.

#### Single files

With the `file` flag, a `--mount-child` names a regular file instead of a
directory. The child is then the file itself: the aggregate's root lists it as
a regular file, which the guest bind-mounts onto a file, and nothing else of
the file's host directory is exposed, neither its other entries nor the
directory's attributes. The child's mode applies to the whole file:

```bash
openvmm --machine microvm \
  --mount-aggregate /run/shares \
  --mount-child settings,path/to/config/settings.json,ro,file \
  --mount-child output,path/to/results/output.txt,rw,file \
  --kernel path/to/vmlinux --initrd path/to/initramfs.cpio.gz
```

- The flag, not the host object, decides the child's kind: OpenVMM refuses a
  file child whose host path is not a regular file, and a directory child
  whose host path is not a directory, so a directory that replaces a shared
  file is never exported in its place.
- A read-only file rejects every modification with `EROFS`, whichever guest
  mount reaches it. A read-write file can be written and truncated, but it
  cannot be removed or renamed, because its parent is the aggregate's
  read-only root, so a guest program that saves by renaming a new file over
  the old one fails.
- Nothing lies below a file, so a lookup through it fails with `ENOTDIR`, and
  so does creating an entry through a read-write file; a read-only one
  rejects the creation with `EROFS` first. A hard link of the file into
  another child fails like any link between children.
- A file overlaps only a directory child that contains it, or another child
  that names the same file, such as through a hard link. Files of one
  directory may be separate children, with different modes, and a file may
  be a child next to a directory child, or lie directly in a volume's root.
- No `--mount-deny`, `--mount-allow`, or `--mount-write` path may name a file
  child. `--mount` shares only a directory, because the guest mounts the
  device's root, which is always a directory.

HostFs opens the file's directory and resolves the file by its name for each
request, as it resolves a path inside a shared directory, but the guest
receives no node of the directory, and HostFs also hides the directory's
root behind the file. A host program that replaces the file by renaming
another file over it therefore replaces what the guest reads. If anything but
a regular file appears at the file's name, HostFs refuses every request on
the child with `EACCES`.

For an active cold-boot attachment, the profile adds `virtfs_dir`,
`virtfs_tag`, and `virtfs_mode` bootstrap tokens to the kernel command line.
For an aggregate, `virtfs_mode` is `rw` when any child is read-write, so the
guest's mount can carry the read-write children, and the profile adds
`virtfs_aggregate=1`, so the guest can tell the aggregate's root from a
single shared directory. Active attachment policies and their canonical
absolute host paths become snapshot-authoritative.

### Access policy

Three repeatable options refine what the guest can see and modify inside a
share. Each names an existing host file or directory. A relative path is
relative to the host directory of `--mount`; with `--mount-aggregate`, the
path must be absolute, and it applies to the child whose directory contains
it.

- `--mount-deny <HOST_PATH>` hides the path and everything below it.
- `--mount-allow <HOST_PATH>` exposes the path and everything below it again,
  inside a denied path. The nearest denied or allowed path that contains an
  allowed path must be a denied path, and the nearest one that contains a
  denied path must be an allowed path, if any, so denied paths may nest inside
  allowed paths.
- `--mount-write <HOST_PATH>`, on a read-write share, makes the path and
  everything below it one of the only parts of the share that the guest can
  modify. The rest of the share is read-only. Writable paths must not
  overlap or lie in a hidden part of the share. A read-only share rejects
  them.

```bash
openvmm --machine microvm \
  --mount /tmp/gh-aw,path/to/gh-aw,rw \
  --mount-deny path/to/gh-aw/mcp-logs \
  --mount-allow path/to/gh-aw/mcp-logs/payloads \
  --mount-write path/to/gh-aw/agent \
  --kernel path/to/vmlinux --initrd path/to/initramfs.cpio.gz
```

A hidden directory on the way from a denied path to an allowed path is
*traverse-only*. The guest can look it up and list it, but the listing shows
only the directories that lead to allowed paths. Every other name in it fails
with `EACCES`, and the guest cannot modify the directory or its entries. In
the example above, the guest sees `mcp-logs` with only `payloads` in it.
Allowed paths follow the share's write policy: here `payloads` is read-only
because it is not inside a writable path, but without `--mount-write` it
would be writable like the rest of the read-write share.

The guest's modifications of a traverse-only directory or its entries, and
with `--mount-write` of anything outside the writable paths, fail with
`EROFS` before HostFs touches the host:
creating, linking, renaming, or removing an entry; writing, truncating, or
opening a file for writing; and changing attributes or extended attributes.
Moving an entry into or out of a writable path changes a read-only directory,
so it also fails with `EROFS`. A hard link to a read-only file from inside a
writable path would make the file writable, so it fails with `EXDEV`, as a
link across mounts does. An object with several names is writable only if
every name that the guest has used for it is writable.

OpenVMM canonicalizes each policy path relative to its export and rejects
paths that do not exist, paths outside the roots, a root as an allowed or
writable path, duplicates,
redundant or contradictory combinations, symlink/reparse components, and
nested-mount crossings before opening the device. A name in a policy path may
contain spaces, but it may not begin or end with one, and other whitespace,
backslashes, and colons are rejected. HostFs also requires every
traverse-only path to be a directory when it attaches the share.

`--mount-deny` may name the root of the `--mount` or of a `--mount-child`
when `--mount-allow` paths expose parts of it. The root is then
traverse-only: the guest sees only the allowed paths and the directories that
lead to them, and it can modify nothing in the root itself. HostFs pins the
root object to the root, so it hides that object at any other name, and other
denied paths are allowed only inside allowed paths. This exposes chosen files
of a directory without the rest of it:

```bash
openvmm --machine microvm \
  --mount-aggregate /run/shares \
  --mount-child 0,path/to/tools,ro \
  --mount-deny path/to/tools \
  --mount-allow path/to/tools/config.json \
  --kernel path/to/vmlinux --initrd path/to/initramfs.cpio.gz
```

```admonish warning
The policy applies to names inside the export. A host hard link or bind
mount that makes a read-only object reachable inside a writable path also
makes that object writable, and one that makes part of a denied path
reachable elsewhere exposes that part. HostFs pins the objects of denied
paths and traverse-only directories, so it rejects those objects themselves
at any other name, but not their contents. Do not place such aliases inside
an export.
```

A read-write attachment lets the guest create symbolic links, so ordinary
build tools (package managers, virtual environments, and language
toolchains) work in a live share. HostFs stores each target byte for byte and
never follows a link while resolving a host path; the guest kernel resolves
links in its own namespace. A target that names an absolute host path, a
location above the export, or a denied path therefore cannot reach host data
outside the policy. Opening a link object fails with `ELOOP`, and on Linux
changing its mode fails with `EOPNOTSUPP`. On Windows, HostFs always creates
WSL-style links, which Windows path resolution never follows, so Windows tools
see guest-created links as inaccessible files. Host software that later reads
the exported directory must treat guest-created links as untrusted.

### Host identity of guest operations

`--mount-owner` selects the host identity that performs the guest's
operations:

- `vmm` (the default): every operation runs as the OpenVMM process, which
  therefore owns every file and directory that the guest creates. The guest
  enforces permissions against the ownership and mode bits that HostFs
  reports.
- `caller`: each operation runs as the UID and GID that the guest kernel
  reports for its caller, so a workload's files are owned by its own numeric
  identity on the host, and the host also enforces permissions for that
  identity. Guest UID 0 and GID 0 are squashed to the owner and group of the
  export root, so the guest can create neither root-owned nor setuid-root
  host files. OpenVMM therefore refuses an export root owned by UID 0 or
  GID 0.

```bash
openvmm --machine microvm \
  --mount /mnt/share,path/to/share,rw \
  --mount-owner caller \
  --kernel path/to/vmlinux --initrd path/to/initramfs.cpio.gz
```

In `caller` mode, the request queue worker switches only its own thread's
credentials for one request at a time: it sets the filesystem UID and GID
with `setfsuid` and `setfsgid`, drops every supplementary group other than
that GID, and clears the effective capabilities, then restores all of them
before it handles anything else. Session negotiation, `FORGET`, and handle
release do not access host files and run unchanged. Assuming an identity
other than OpenVMM's own, or dropping OpenVMM's other supplementary groups,
needs `CAP_SETUID` and `CAP_SETGID`, for example as ambient capabilities of
an unprivileged OpenVMM process. Without them, only a caller whose UID and
GID match OpenVMM's filesystem UID and GID can run, and only if OpenVMM has
no other supplementary groups. A request that cannot run as its caller,
because a credential switch fails or the capabilities are missing, fails
with `EPERM`; HostFs never falls back to OpenVMM's identity, groups, or
capabilities. `caller` mode requires a Linux host. Windows has no
per-request POSIX identity to switch to, so OpenVMM rejects
`--mount-owner caller` there.

```admonish warning
The guest kernel reports each caller's identity, and guest root may assume
any identity inside the guest. Guest root and a compromised guest kernel can
therefore act as any nonzero host UID and GID inside the export, including
leaving setuid files owned by it. Export only trees that every such identity
may modify, and keep the export on a host filesystem mounted `nosuid` when
host users might execute guest-created files. Grant OpenVMM only
`CAP_SETUID` and `CAP_SETGID`: these capabilities let it assume any host
identity.
```

`SectionFs`, alternate tags, PCI transport, DAX, and extra queues are not part
of the microVM profile.

## Snapshot attachments

A microVM snapshot stores FUSE negotiation, namespace identifiers, lookup
counts, reopenable handles, directory cookies, and virtqueue progress. It
does not copy the host directory or serialize native file descriptors and
Windows handles.

An open directory continues from its bounded captured entry snapshot, so
later host additions do not appear midway through that enumeration. New
lookups and newly opened directories still observe the live host tree.

Restoring a snapshot captured with an active attachment requires a fresh
`--mount` argument with the exact same denied, allowed, and writable paths,
canonical host path, guest target, mode, and `--mount-owner` mode. Identity
validation remains independent: before any vCPU
starts, OpenVMM pins the supplied root and validates its saved root and object
identities. Missing, moved, replaced, ambiguous, or no-longer-reopenable
objects fail restore. A saved symbolic link is revalidated as the link itself,
without being followed, and an alias whose ancestor has become a link fails
restore. A handle that the guest held open for writing must still be on a
writable path.

A snapshot of an aggregate records each child's name, canonical host path,
root identity, access mode, and denied, allowed, and writable paths, and the
attachment's identity is the SHA-256 of the children's names and root
identities. Restoring it requires `--mount-aggregate` with the same guest
target and the same `--mount-child` options in the same order: the same names,
canonical host paths, and modes, with the same policy paths and
`--mount-owner` mode. OpenVMM revalidates each child's root and saved objects
as it does a single directory's. The aggregate's root lists the children in
the same order, so its directory handles and cookies stay valid.

The device state of an attachment with allowed or writable paths records its
complete access policy, and a restore must supply the same one. OpenVMM
releases that predate these paths reject that state rather than restore the
share without them. The device state of an aggregate records its children
and their policies, and releases that predate aggregates reject it.

`--mount-owner caller` applies only to guest requests. Capture and restore
still revalidate saved names and reopen saved handles as the OpenVMM process.
Unless OpenVMM may bypass file permissions, as root may, capture therefore
fails while the guest holds a name inside a directory that OpenVMM cannot
search. Restore fails when OpenVMM cannot reopen a saved handle with its saved
access, such as a handle open for writing on a file that a guest caller owns.
OpenVMM releases that predate `--mount-owner` reject the device state of a
`caller` attachment rather than restore it as `vmm`.

A snapshot captured without `--mount` records the slot as dormant. It may
restore without an attachment, or bind one new `--mount` attachment to the
slot, but not an aggregate. Because execution resumes after the cold-boot
mount hook, the guest must mount the newly attached backend explicitly:

```bash
mkdir -p /mnt/share
mount -t virtiofs microvm /mnt/share
```

Snapshots created before the dormant-slot capability cannot add a restore-time
attachment and fail with a compatibility error. Snapshots that an earlier
OpenVMM captured with a second virtio-fs slot, `fs:microvm1`, cannot be
restored.

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
Windows reparse points while resolving guest or saved objects, and enforces
read-only mode before invoking a host mutation.

On Linux, every host operation opens the parent directory of its path with
`openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`, or with one
`O_NOFOLLOW` open per component where `openat2` is unavailable, and applies
the operation to the final component without following it. Extended
attributes and file system statistics use that pinned directory rather than
an absolute host path. Neither a guest-created link nor a concurrent rename
can therefore redirect an operation outside the export. On Windows, the guest
can only create WSL-style links, which are never followed, and HostFs rejects
any ancestor that is a symbolic link or reparse point before each operation.

Denied paths are enforced in the server namespace rather than by guest mount
layout. Lookup and mutation operations reject denied prefixes, directory
enumeration omits their names, and denied root object identities reject
hard-link, junction, and bind-mount aliases. A traverse-only directory's object
is accepted only at its own path, and only as a directory, so neither a host
rename nor another name for it exposes its hidden entries. A guest-created
link cannot reach a denied path because the guest resolves it and every
resulting host lookup applies the same policy. Writable paths are enforced the
same way: every mutation checks the names that it changes and the object that
it modifies against the policy before the host operation. Mounting the same
virtio-fs tag at another guest path, or remounting it read-write, does not
change the policy, and each share's policy applies only to requests for its
own tag.

```admonish warning
On Windows, the check for host-created NT symbolic links and junctions in
ancestor components is not handle-relative. Do not let an untrusted host
process create reparse points or replace directories inside a Windows export
while a VM runs. On every host, quiesce external namespace mutation during
capture and restore.
```

Host filesystem behavior differs where Windows cannot represent a POSIX
operation. Unsupported operations return a Linux error rather than reporting
false success.

## Code references

- Device implementation:
  `vm/devices/virtio/virtiofs/`
- Share access policy:
  `vm/devices/virtio/virtiofs/src/microvm/policy.rs`
- Per-thread filesystem credentials:
  `vm/devices/support/fs/lxutil/src/unix/credentials.rs`
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
