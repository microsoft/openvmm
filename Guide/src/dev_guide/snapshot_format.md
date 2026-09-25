# Snapshot Format

This page documents the on-disk format used by OpenVMM snapshots, intended
for developers working on the save/restore subsystem.

## Directory layout

A snapshot is stored as a directory containing three required files and, for
a mounted microVM scratch capture, one paired scratch image:

```text
snapshot-dir/
├── manifest.bin   # Protobuf-encoded SnapshotManifest
├── state.bin      # Protobuf-encoded device saved state
├── memory.bin     # Exact automatic RAM file or independent supplied-RAM clone
└── scratch.img    # Optional paired microVM writable scratch image
```

## Manifest format

The manifest is a protobuf message defined as
[`SnapshotManifest`](https://openvmm.dev/rustdoc/linux/openvmm_helpers/snapshot/struct.SnapshotManifest.html)
in `openvmm/openvmm_helpers/src/snapshot.rs`, encoded using the `mesh`
crate's protobuf encoding.

New snapshots use manifest version 5. The legacy `state_sha256` and
`memory_sha256` protobuf tags remain reserved so version 2 manifests can be
decoded; versions 3 through 5 require both fields to be absent. Restore accepts
versions 2 through 4 for compatibility. Tiered microVM snapshots require version
5, which records capture tier, clone/resume policy, and consumed configuration
sections.

The default format is a local machine-state contract, not an authenticated
container. All versions receive the same regular-file, no-follow/no-reparse,
bounded decoding, exact-length, inventory, and machine-contract validation,
but the on-disk format does not authenticate same-length payload changes.
Versions 4 and 5 record the SHA-256 and exact length of `scratch.img`, because guest
RAM and a mounted writable filesystem must be restored as one exact pair.
Export or transport layers must provide broader integrity and authentication
outside this format.

The optional microVM network contract stores a versioned SHA-256 digest of its
bound egress policy. Encoding version 2 includes the static prefix, guest MAC,
and canonical ARP next-hop set in addition to the policy rules. An absent
encoding-version field denotes version 1, allowing network snapshots produced
before endpoint next-hop binding to retain their original digest semantics.

## Device state (`state.bin`)

The device state contains every device's saved state, collected via the
`SaveRestore` trait and encoded as a `mesh` protobuf message. The
[Save State](contrib/save-state.md) compatibility rules (mesh tag stability,
default values, forward/backward compatibility) apply.

## Memory (`memory.bin`)

For automatically allocated microVM RAM, `memory.bin` is a hard link to the
exact OpenVMM-owned backing handle. State and manifest are written and flushed
first; OpenVMM then flushes the stopped guest's shared RAM mappings and exact
backing handle, creates the link last, and verifies the linked handle's file
identity and EOF before the staging directory is renamed into place. Linux
prefers `linkat(AT_EMPTY_PATH)` and can use a `/proc/self/fd` link with a
device/inode proof. Windows uses handle-relative `FileLinkInformation` and
verifies `FILE_ID_INFO`. Filesystems that cannot create the link fall back to
an independent sparse-aware copy.

User-supplied RAM always uses the independent-copy path, never a hard link.
Clone support is used when available, with allocated-range or zero-scan copying
as a fallback on Linux; Windows uses a dense copy. Both paths use an
already-open handle rather than reopening its pathname, so replacing the source
path cannot substitute different bytes.
Successful guest-requested capture is terminal for the source. If publication
fails after the automatic link exists, OpenVMM removes and durably flushes the
private staging directory before allowing rollback; uncertain cleanup makes
the source terminate instead of resume.

Restore likewise opens the snapshot directory once and resolves its artifacts
relative to that handle. Windows uses read-only handles with `FILE_SHARE_READ`
only, rejects reparse points, compares `FILE_ID_INFO` and EOF before and after
creating the private COW section, and keeps the directory and artifact guards
in the VM worker until teardown. Linux keeps the exact `O_NOFOLLOW` directory
and regular-file descriptors and rejects observable metadata changes before
handoff, so renaming or replacing the original path cannot substitute another
generation. Linux file descriptors do not provide mandatory write exclusion;
deployments that need authenticated or write-proof local artifacts must add a
stronger mode such as a lease, fs-verity, or a verified artifact broker.

## Scratch (`scratch.img`)

The microVM block contract records every fixed role, access mode, geometry, and
immutable read-only layer digest. A paired capture copies the exact opened
writable scratch handle into the same staging directory after device queues
drain, verifies its SHA-256, and publishes it atomically with VM state. Restore
verifies the artifact before worker construction and makes a private copy for
each process, so repeated restores cannot mutate the snapshot.

A pre-mount capture instead records the `fresh` scratch policy and geometry,
contains no `scratch.img`, and requires a new matching scratch file on restore.

## Code references

- Manifest type and I/O: `openvmm/openvmm_helpers/src/snapshot.rs`
- Format validation, publication, restore-side access, file-system helpers,
  and the microVM machine contract: `openvmm/openvmm_helpers/src/snapshot/`
- Restore entry point: `prepare_snapshot_restore()` in
  `openvmm/openvmm_entry/src/snapshot_restore/prepare.rs`
- File-backed memory: `SharedMemoryFd` type alias in
  `openvmm/openvmm_defs/src/worker.rs`

## Device state architecture

Each VM component that participates in save/restore is registered as a
"state unit" with a unique string name via `StateUnits::add("name")`.
During save, every state unit receives a `StateRequest::Save`. Units that
have state return `Ok(Some(blob))`; units with no persistent state (e.g.
the input distributor) return `Ok(None)` and are omitted from `state.bin`.

The resulting `state.bin` contains a `Vec<SavedStateUnit>`, where each
entry pairs a unit name with its opaque protobuf-encoded state blob.

### Restore matching rules

During restore, `StateUnits::restore()` matches saved-state entries to
currently registered units **by name**:

| Scenario | Result |
|---|---|
| Names match exactly | State is dispatched to the unit |
| Saved entry has no matching unit | **Error** — `unknown unit name` |
| Unit exists with no saved entry | Unit is skipped (keeps default state) |
| Duplicate name in saved state | **Error** — `duplicate unit name` |

This means removing a device between save and restore will fail, but
adding a new device is allowed (it initialises to its power-on defaults).

### Unit naming conventions

- **Chipset devices** — registered via `arc_mutex_device("name")` in
  `vmotherboard`, e.g. `"pit"`, `"rtc"`, `"uefi"`, `"ide"`.
- **VMBus devices** — named `"{interface_name}:{instance_id}"`, e.g.
  `"StorageVsp:ba6163d9-..."`. The instance GUID makes each offer
  unique.
- **Infrastructure units** — `"vmtime"`, `"input"`, `"vmbus"`.

### Devices that do not support save/restore

Not all devices implement save/restore. Devices signal this in one of
two ways:

1. **`SaveError::NotSupported`** — the `save()` method returns this error.
   If any state unit does this, the entire save operation fails.
2. **`supports_save_restore() -> false`** (virtio) or
   `supports_save_restore() -> None` (VMBus) — transport-level check
   that causes the transport's `save()` to return
   `SaveError::NotSupported`.

Key unsupported categories:

- **PCIe** — `GenericPcieRootComplex`, `GenericPcieSwitch` return
  `SaveError::NotSupported`.
- **NVMe** — `NvmeController` returns `SaveError::NotSupported`.
- **Pass-through PCI** — `AssignedPciDevice`, `RelayedVpciDevice`.
- **VGA / GDMA** — marked `todo!()` (will panic on save).
- **Virtio devices** — the `VirtioDevice` trait defaults
  `supports_save_restore()` to `false`. `virtio-blk`, `virtio-console`,
  `virtio-pmem`, and `virtio-rng` override it to `true`. `virtio-net` enables
  it only for resources with an explicit static identity and feature contract,
  such as the microVM NIC; ordinary virtio-net resources remain
  disabled.
  The transport stores an opaque typed device-private payload in addition to
  common queue state. Devices with unsupported host-side session state, such
  as `virtio-9p`, leave save/restore disabled. `virtiofs` enables typed
  device-private state only for the constrained microVM HostFs profile;
  ordinary, aggregate, and SectionFs resources remain disabled.
- **Some VMBus devices** — `GuestCrashDevice`, `GuestEmulationDevice`,
  `VmbusSerialHost`, `Vmbfs` return `None` from
  `supports_save_restore()`.

## Extending the format

When adding new fields to `SnapshotManifest`, use the next available mesh
tag number. The protobuf encoding is forward-compatible: older readers will
ignore unknown fields. However, removing or reordering existing fields is a
breaking change. See [Save State](contrib/save-state.md) for the full set of
compatibility rules.

```admonish warning
Changing the mesh tag numbers of existing fields will break compatibility
with previously saved snapshots.
```
