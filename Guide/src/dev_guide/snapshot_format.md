# Snapshot Format

This page documents the on-disk format used by OpenVMM snapshots, intended
for developers working on the save/restore subsystem.

## Directory layout

A snapshot is stored as a directory containing three files:

```text
snapshot-dir/
├── manifest.bin   # Protobuf-encoded SnapshotManifest
├── state.bin      # Protobuf-encoded device saved state
└── memory.bin     # Hard link to the guest memory backing file
```

## Manifest format

The manifest is a protobuf message defined as
[`SnapshotManifest`](https://openvmm.dev/rustdoc/linux/openvmm_helpers/snapshot/struct.SnapshotManifest.html)
in `openvmm/openvmm_helpers/src/snapshot.rs`, encoded using the `mesh`
crate's protobuf encoding.

## Device state (`state.bin`)

The device state is a `mesh` protobuf message containing:

- `units` — saved-state blobs returned by state units with mutable state
- `inventory` — the complete set of registered state-unit names, including
  units that returned no blob

The state is collected via the `SaveRestore` trait. The
[Save State](contrib/save-state.md) compatibility rules (mesh tag stability,
default values, forward/backward compatibility) apply.

## Memory (`memory.bin`)

`memory.bin` is a hard link to the file-backed guest RAM file. During a save,
`write_snapshot()` creates this hard link using `std::fs::hard_link`.

```admonish note
The hard-link approach means the memory backing file and snapshot directory
must reside on the same filesystem. If they are on different filesystems,
`write_snapshot` returns an error with a suggestion to place the backing
file inside the snapshot directory.
```

### Same-file detection

If the user passes `--memory file=<snapshot_dir>/memory.bin`, the source and
target of the hard link are the same file. The code detects this by
canonicalizing both paths and comparing them. When they match, the hard-link
step is skipped.

## Code references

- Manifest type and I/O: `openvmm/openvmm_helpers/src/snapshot.rs`
- Restore entry point: `prepare_snapshot_restore()` in
  `openvmm/openvmm_entry/src/lib.rs`
- File-backed memory: `SharedMemoryFd` type alias in
  `openvmm/openvmm_defs/src/worker.rs`

## Device state architecture

Each VM component that participates in save/restore is registered as a
"state unit" with a unique string name via `StateUnits::add("name")`.
During save, every state unit receives a `StateRequest::Save`. Units that
have state return `Ok(Some(blob))`; units with no persistent state (e.g.
the input distributor) return `Ok(None)` and are omitted from the `units`
field.

The `units` field is a `Vec<SavedStateUnit>`, where each entry pairs a unit
name with its opaque protobuf-encoded state blob. The `inventory` field is
captured from the same unit-set snapshot as those blobs. Names are written in
canonical order, but ordering is not part of snapshot compatibility.

### Restore matching rules

For a snapshot with an inventory, the VM worker first compares the saved and
currently registered names as unordered exact sets:

| Scenario | Result |
|---|---|
| Inventories contain exactly the same names | Validation succeeds |
| Missing or extra inventory name | **Error** — inventory mismatch |
| Saved inventory contains a duplicate name | **Error** — inventory mismatch |

After inventory validation, `StateUnits::restore()` dispatches entries from
the `units` field **by name**:

| Scenario | Result |
|---|---|
| Saved entry has a matching unit | State is dispatched to the unit |
| Saved entry has no matching unit | **Error** — `unknown unit name` |
| Inventory unit has no blob | Unit is skipped (no mutable state) |
| Duplicate name in saved state | **Error** — `duplicate unit name` |

Snapshots written before the `inventory` field was added decode with an empty
inventory and skip exact-set validation. For these legacy snapshots, an extra
current unit is accepted and keeps its initial state, while a saved entry
without a current unit still fails. The legacy exception preserves backward
compatibility; changing the device set is not a supported restore workflow.

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
