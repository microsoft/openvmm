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

The device state is a `mesh` protobuf message, `openvmm.SavedState`:

- `units` (field 1) — the saved state of each state unit that has any,
  collected via the `SaveRestore` trait;
- `inventory` (field 2) — the contract of every state unit, ordered by name,
  including the units that have no saved state.

Saved state from before OpenVMM recorded contracts has no inventory, and
restores without the inventory check. The [Save State](contrib/save-state.md)
compatibility rules (mesh tag stability, default values, forward/backward
compatibility) apply.

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
the input distributor) return `Ok(None)` and are omitted from `units`.

`units` is a `Vec<SavedStateUnit>`, where each entry pairs a unit name with its
opaque protobuf-encoded state blob. `inventory` is a `Vec<UnitContract>`,
which `StateUnits::save_with_inventory` captures from the same snapshot of the
unit set as the blobs, so a unit removed while the save runs appears in both if
it answered the save, and in neither if it did not.

### Unit contracts

A unit's contract is the part of the VM's contract that it contributes, which
the unit declares when it is added, through `UnitBuilder`:

| Field | Meaning |
|---|---|
| `name` | The unit's name, which keys its saved state |
| `stable_id` | The guest-visible identity of what the unit implements, such as `pcie-port:<port>` for a PCIe device; the name unless declared |
| `kind` | What the unit implements, such as its device's resource kind |
| `lifetime` | `Static` (created from the configuration), `Dynamic` (added while the VM ran, such as a hot-plugged device), or `Derived` (VMM infrastructure, such as `vmtime`, `chipset`, or `lines/*`) |
| `descriptors` | Guest-visible properties, as sorted `key=value` pairs |

`vmotherboard` derives a chipset device's descriptors from its registrations
and its configuration space, without device-specific code: `mmio` and `pio` for
each static region (`<label>@<base>+<length>`), `line` and `line.target` for its
interrupt lines and targets, `pci.bdf`, `pcie.port`, or `pcie.rciep` for its PCI
placement, and `pci.id`, `pci.class`, `pci.revision`, and `pci.subsystem` for
the identity that a PCI function reports.

### Restore matching rules

Before applying saved state, the VM worker checks the saved inventory with
`StateUnits::check_inventory`, matching units by name, whatever order either
VM registered them in:

| Scenario | Result |
|---|---|
| A unit's contract equals the recorded one | The unit matches |
| A recorded static or derived unit is missing | **Error** — missing |
| A unit that the inventory does not record | **Error** — unexpected |
| A unit's contract differs from the recorded one | **Error** — changed |
| The inventory records a name more than once | **Error** — recorded more than once |
| Saved state of a unit that the inventory does not record | **Error** — saved state without a record |
| A recorded dynamic unit is missing | Planned for re-creation |

`StateUnits::restore()` then dispatches each entry of `units` to the unit of
the same name. Before that, the VM worker re-creates the planned units that
are devices hot-plugged onto PCIe ports, from the devices that the restore
supplies for their ports, and then requires the inventory to match exactly; it
refuses saved state that plans to re-create any other unit. See
[PCIe save and restore](../reference/emulated/pcie/overview.md#save-and-restore).
Saved state without an inventory skips the check, and restore matches its
entries by name alone, as before.

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
  `virtio-net`, `virtio-pmem`, `virtio-rng`, and `virtio-rtc` override it to
  `true`.
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
