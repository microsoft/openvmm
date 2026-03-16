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

The manifest is a protobuf message defined as `SnapshotManifest` in
`openvmm/openvmm_helpers/src/snapshot.rs` (re-exported by
`openvmm/openvmm_entry/src/snapshot.rs`). It uses the `mesh` crate's
protobuf encoding.

| Field              | Type        | Mesh tag | Description                      |
|--------------------|-------------|----------|----------------------------------|
| `version`          | `u32`       | 1        | Manifest format version (currently 1)|
| `created_at`       | `Timestamp` | 2        | When the snapshot was created     |
| `openvmm_version`  | `String`    | 3        | OpenVMM version that created it   |
| `memory_size_bytes`| `u64`       | 4        | Guest RAM size in bytes           |
| `vp_count`         | `u32`       | 5        | Number of virtual processors      |
| `page_size`        | `u32`       | 6        | System page size in bytes         |
| `architecture`     | `String`    | 7        | `"x86_64"` or `"aarch64"`         |

## Device state (`state.bin`)

The device state is a `mesh::payload::message::ProtobufMessage` that has been
encoded with `mesh::payload::encode()`. On restore, it is decoded back to a
`ProtobufMessage` and passed to the VM worker to reconstruct device state.

The internal structure depends on the set of devices configured in the VM and
their individual save/restore implementations. Each device saves its own
state using the `SaveRestore` trait.

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
  (re-exported by `openvmm/openvmm_entry/src/snapshot.rs`)
- Restore entry point: `prepare_snapshot_restore()` in
  `openvmm/openvmm_entry/src/lib.rs`
- File-backed memory: `SharedMemoryFd` type alias in
  `openvmm/openvmm_defs/src/worker.rs`

## Extending the format

When adding new fields to `SnapshotManifest`, use the next available mesh
tag number. The protobuf encoding is forward-compatible: older readers will
ignore unknown fields. However, removing or reordering existing fields is a
breaking change.

```admonish warning
Changing the mesh tag numbers of existing fields will break compatibility
with previously saved snapshots.
```
