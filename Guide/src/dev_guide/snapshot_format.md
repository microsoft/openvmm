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

The device state contains every device's saved state, collected via the
`SaveRestore` trait and encoded as a `mesh` protobuf message. The
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

If the user passes `--memory-backing-file <snapshot_dir>/memory.bin`, the
source and target of the hard link are the same file. The code detects this
by canonicalizing both paths and comparing them. When they match, the
hard-link step is skipped.

## Validation on restore

The `validate_manifest()` function in `snapshot.rs` checks four fields
against the current VM configuration:

1. **Version** — must match the current `MANIFEST_VERSION` constant
2. **Architecture** — must match the guest architecture (from `guest_arch` cfg)
3. **Memory size** — must match the `--memory` CLI option
4. **VP count** — must match the `--processors` CLI option

After manifest validation, the code also verifies that `memory.bin` has the
expected file size.

## Code references

- Manifest type and I/O: `openvmm/openvmm_helpers/src/snapshot.rs`
- Restore entry point: `prepare_snapshot_restore()` in
  `openvmm/openvmm_entry/src/lib.rs`
- File-backed memory: `SharedMemoryFd` type alias in
  `openvmm/openvmm_defs/src/worker.rs`

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
