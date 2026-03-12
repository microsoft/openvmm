# Snapshots

OpenVMM supports saving and restoring VM snapshots, allowing you to capture
the complete state of a running VM and resume it later.

## Overview

A snapshot captures three pieces of state:

- **Guest RAM** — the full contents of guest memory
- **Device state** — the saved state of all emulated devices
- **Manifest** — metadata describing the snapshot (architecture, memory size,
  VP count, page size, etc.)

These are stored as three files in a snapshot directory:

| File            | Contents                                    |
|-----------------|---------------------------------------------|
| `manifest.bin`  | Protobuf-encoded snapshot metadata          |
| `state.bin`     | Serialized device state                     |
| `memory.bin`    | Hard link to the memory backing file        |

## Prerequisites

Snapshots require **file-backed guest memory**. You must pass
`--memory-backing-file` when launching the VM so that guest RAM is written
to a file on disk rather than held in anonymous memory.

```admonish warning
The memory backing file and the snapshot directory must be on the **same
filesystem**. OpenVMM creates a hard link from the backing file to
`memory.bin` inside the snapshot directory, which does not work across
filesystem boundaries.
```

## Saving a snapshot

Start a VM with file-backed memory, then use the interactive console to save:

```bash
cargo run -- \
  --uefi \
  --disk memdiff:file:path/to/disk.vhdx \
  --memory-backing-file path/to/memory.bin \
  --memory 4096
```

Once the VM is running, open the interactive console and issue a save command,
specifying the output directory:

```text
save path/to/snapshot-dir
```

OpenVMM writes `manifest.bin`, `state.bin`, and a hard link to `memory.bin`
into the specified directory.

## Restoring a snapshot

To restore, pass the snapshot directory with `--restore`:

```bash
cargo run -- \
  --uefi \
  --disk memdiff:file:path/to/disk.vhdx \
  --memory-backing-file path/to/snapshot-dir/memory.bin \
  --memory 4096 \
  --processors 4 \
  --restore path/to/snapshot-dir
```

```admonish note
The `--memory` and `--processors` values must match the values recorded in
the snapshot manifest. If they do not match, OpenVMM will report a
validation error and refuse to start.
```

## Validation

On restore, OpenVMM validates that:

1. The snapshot architecture matches the host architecture
2. The `--memory` size matches `memory_size_bytes` in the manifest
3. The `--processors` count matches `vp_count` in the manifest
4. The `memory.bin` file size matches the manifest

If any check fails, OpenVMM exits with a descriptive error message.

## Limitations

- Snapshots are **not portable** across architectures (e.g., you cannot
  restore an x86_64 snapshot on aarch64)
- VMs using VPCI or PCIe devices do not currently support save/restore
- OpenHCL-based VMs do not currently support this snapshot mechanism
- PCAT firmware does not support save/restore
