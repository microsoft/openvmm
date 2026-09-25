# Snapshot Format

This page documents the on-disk format used by OpenVMM snapshots, intended
for developers working on the save/restore subsystem.

## Directory layout

A snapshot is stored as a directory containing three files:

```text
snapshot-dir/
├── manifest.bin   # Protobuf-encoded SnapshotManifest
├── state.bin      # Protobuf-encoded device saved state
└── memory.bin     # Independent copy of the guest memory backing file
```

## Manifest format

The manifest is a protobuf message defined as
[`SnapshotManifest`](https://openvmm.dev/rustdoc/linux/openvmm_helpers/snapshot/struct.SnapshotManifest.html)
in `openvmm/openvmm_helpers/src/snapshot.rs`, encoded using the `mesh`
crate's protobuf encoding.

New snapshots use manifest version 3, which records a format magic, the
saved-state schema version and protobuf root type, and the exact length of
`state.bin`. The legacy `state_sha256` and `memory_sha256` protobuf tags remain
reserved so version 2 manifests can be decoded; version 3 requires both fields
to be absent. Restore accepts versions 2 and 3.

Reading a snapshot bounds `manifest.bin` to 1 MiB and `state.bin` to 256 MiB,
validates the manifest format before it reads `state.bin`, and requires
`state.bin` to have the length that the manifest records.

## Device state (`state.bin`)

The device state contains every device's saved state, collected via the
`SaveRestore` trait and encoded as a `mesh` protobuf message. The
[Save State](contrib/save-state.md) compatibility rules (mesh tag stability,
default values, forward/backward compatibility) apply.

## Memory (`memory.bin`)

`memory.bin` is an independent copy of the file-backed guest RAM, so resuming
or reusing the source VM cannot modify a published snapshot. Clone support is
used when available, with allocated-range or zero-scan copying as a fallback on
Linux; Windows uses a dense copy. The copy reads through an already-open handle
rather than reopening the backing file's path, so replacing the source path
cannot substitute different bytes.

## Publication

`write_snapshot()` writes and flushes `state.bin`, `memory.bin`, and
`manifest.bin` in a uniquely named, private staging directory next to the
destination, flushes the staging directory, and then renames it to the
destination in one operation that never replaces an existing path. The rename
is the commit point; the parent directory is flushed afterwards. A failure
before the commit removes the staging directory and leaves the destination
absent. The destination must not already exist.

## Code references

- Manifest type and I/O: `openvmm/openvmm_helpers/src/snapshot.rs`
- Format validation, publication, and file-system helpers:
  `openvmm/openvmm_helpers/src/snapshot/`
- Restore entry point: `prepare_snapshot_restore()` in
  `openvmm/openvmm_entry/src/lib.rs`
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
  `supports_save_restore()` to `false`. Only `virtio-blk`,
  `virtio-net`, `virtio-pmem`, and `virtio-rng` override it to `true`.
  Devices with host-side session state (`virtio-9p`, `virtiofs`,
  `virtio-console`) intentionally leave it `false`.
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
