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
| `memory.bin`    | Memory backing file                         |

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

Start a VM with file-backed memory:

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
save-snapshot path/to/snapshot-dir
```

OpenVMM writes `manifest.bin`, `state.bin`, and a hard link to `memory.bin`
into the specified directory.

```admonish warning
After saving, the VM remains **paused** and resume is blocked. Resuming
would mutate guest RAM through `memory.bin`, corrupting the snapshot.
Use `shutdown` to exit OpenVMM after saving.
```

## Restoring a snapshot

To restore, pass the snapshot directory with `--restore-snapshot`:

```bash
cargo run -- \
  --uefi \
  --disk memdiff:file:path/to/disk.vhdx \
  --memory 4096 \
  --processors 4 \
  --restore-snapshot path/to/snapshot-dir
```

`--restore-snapshot` automatically opens `memory.bin` from the snapshot
directory, so `--memory-backing-file` should not be specified (the two
options are mutually exclusive).

```admonish note
The `--memory` and `--processors` values must match the values recorded in
the snapshot manifest. If they do not match, OpenVMM will report a
validation error and refuse to start.
```

## Limitations

- Snapshots are **not portable** across architectures (e.g., you cannot
  restore an x86_64 snapshot on aarch64)
- After restoring, `memory.bin` in the snapshot directory becomes the live
  guest RAM backing file and will be modified as the VM runs. To restore
  from the same snapshot multiple times, copy the snapshot directory before
  each restore.
- VMs using VPCI or PCIe devices do not currently support save/restore
- OpenHCL-based VMs do not currently support this snapshot mechanism
- VMs using PCAT firmware do not support save/restore
- `--memory` and `--processors` must be specified on restore and match the
  snapshot manifest values. A future version may read these from the snapshot
  automatically.
