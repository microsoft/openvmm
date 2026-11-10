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

Every snapshot uses manifest version 2 with format magic
`OPENVMM_SNAPSHOT_V2\0`. Restore rejects any other version or magic with
`E_SNAPSHOT_VERSION`, which asks for a recapture. It decodes the version,
field 1, and then the magic, field 10, each alone before the rest of the
manifest, since earlier formats give some fields other types: version 6, the
manifest of earlier releases, keeps its machine contract in field 11, and the
first version 2, which has no magic, keeps artifact digests in fields 9 and 10.
A microVM snapshot's machine contract carries the NVX time ABI records (the
time contract and the CPU profile record), and a host-driven standard-machine
snapshot has no machine contract. Tiered microVM snapshots record capture
tier, clone/resume policy, and consumed configuration sections.

The format is a local machine-state contract, not an authenticated container.
Every snapshot receives the same regular-file, no-follow/no-reparse, bounded
decoding, exact-length, inventory, and machine-contract validation, but the
on-disk format does not authenticate same-length payload changes. A
`workload-start` or `instance-checkpoint` snapshot binds its sandbox blocks,
the read-only layers and the paired `scratch.img`, which guest RAM and a
mounted writable filesystem must restore as one exact pair, to one
`generation` identity: a caller-authenticated immutable storage generation,
not a byte digest. Neither capture nor restore reads a block to identify it:
OpenVMM validates role, size, geometry, and the shared generation, and a
restore requires the generation that the snapshot recorded. Restore rejects
the `sha256` block identity of earlier releases, whose snapshots must be
recaptured. Export or transport layers must provide broader integrity and
authentication outside this format.

The optional microVM network contract stores the SHA-256 digest of its bound
egress policy in encoding version 1, which covers the static IPv4 and IPv6
identities, guest MAC, policy rules, canonical ARP next-hop set, host-loopback
action and proxy endpoint, and ingress action. A platform snapshot, or a
snapshot without a tier, whose digest is that of the complete deny-all policy
bound to its identity is a NIC template, whose restore may install another
policy with `--restore-network-policy`, and the format records nothing else for
it.

## Device state (`state.bin`)

The device state is a `mesh` protobuf message, `openvmm.SavedState`, of
version 2 of the saved-state schema, which the manifest records:

- `units` (field 1) — the saved state of each state unit that has any,
  collected via the `SaveRestore` trait;
- `inventory` (field 3) — the contract of every state unit, ordered by name,
  including the units that have no saved state.

Field 2 held the names of the state units in version 1 of the schema, whose
snapshots restore refuses. The [Save State](contrib/save-state.md)
compatibility rules (mesh tag stability, default values, forward/backward
compatibility) apply.

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
fails, OpenVMM removes the private staging directory, its manifest first, and
allows rollback only once it has proved the removal of any staged manifest, so
that restore cannot open what remains, and, once it has tried to create the
automatic link, of the complete directory. On Linux, proving a removal
includes flushing its directory. Uncertain cleanup makes the source terminate
instead of resume.

Restore likewise opens the snapshot directory once and resolves its artifacts
relative to that handle. Windows uses read-only handles with `FILE_SHARE_READ`
only, rejects reparse points, compares `FILE_ID_INFO` and EOF before and after
creating the private COW section, and keeps the directory and artifact guards
in the VM worker until teardown. Linux opens each artifact with `O_NOFOLLOW`
and `O_NONBLOCK`, so that a FIFO without a writer, for which a blocking open
would wait, reaches the check of the opened file's type and is refused like
any other file that is not a regular file, and then makes a regular file's
descriptor blocking again. It keeps the exact directory and regular-file
descriptors, and before handoff rejects a change of the device, inode, length,
or modification time of the opened manifest, state, or memory file, so
renaming or replacing the original path cannot substitute another generation.
A change of their links, mode, or owner alone passes, which lets a restore
open a snapshot before the capture's source, as it tears down, removes its RAM
file's temporary name, the other link of an automatic `memory.bin`.
Linux file descriptors do not provide mandatory write exclusion; deployments
that need authenticated or write-proof local artifacts must add a stronger mode
such as a lease, fs-verity, or a verified artifact broker.

## Scratch (`scratch.img`)

The microVM block contract records every fixed role, access mode, geometry,
identity kind, identity, and paired-scratch restore mode. A paired capture
copies the exact opened writable scratch handle into the same staging
directory after device queues drain and publishes it atomically with VM state.
Every bound block carries the same nonzero caller-authenticated storage
generation, and publication and restore check `scratch.img` structurally and
by exact size, without reading it to verify its content. A platform capture
binds no block, so its read-only layers are `unbound`.

Paired scratch defaults to `private-copy`, which preserves existing behavior
and may use a reflink or sparse copy. `copy-on-write` requires a filesystem
reflink and is restricted to reusable clone snapshots. `direct-claimed`
attaches the paired file only for a single-use resume snapshot after its
durable claim; the claimed artifact is then continuation storage rather than
an available snapshot. A missing restore-mode field retains `private-copy`
behavior.

A pre-mount capture instead records the `fresh` scratch policy and geometry,
contains no `scratch.img`, and requires a new scratch file of that geometry on
restore, unless `--restore-absent-scratch` declares the scratch absent:
OpenVMM then attaches a writable scratch device of that geometry with no file,
which reads zeros and fails every write.

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
the same name. The VM worker refuses saved state whose check plans to re-create
units.

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
  `virtio-pmem`, `virtio-rng`, and `virtio-rtc` override it to `true`.
  `virtio-net` enables it only for resources with an explicit static identity
  and feature contract, such as the microVM NIC; ordinary virtio-net resources
  remain disabled.
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
tag number other than 18, 19, and 20: upstream OpenVMM assigns field 18 to
the `snapshot_id` that its SaveVM API records, and reserves fields 19 and 20
for `compatibility_bundle_digest` and `memory_artifacts`.
The protobuf encoding is forward-compatible: older readers will
ignore unknown fields. However, removing or reordering existing fields is a
breaking change. See [Save State](contrib/save-state.md) for the full set of
compatibility rules.

```admonish warning
Changing the mesh tag numbers of existing fields will break compatibility
with previously saved snapshots.
```
