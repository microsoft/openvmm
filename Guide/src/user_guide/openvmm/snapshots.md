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

Snapshots require **file-backed guest memory**. Pass `file=<PATH>` in the
`--memory` option when launching the VM so that guest RAM is written to a
file on disk rather than held in anonymous memory.

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
  --memory size=4096M,file=path/to/memory.bin
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
  --memory size=4096M \
  --processors 4 \
  --restore-snapshot path/to/snapshot-dir
```

`--restore-snapshot` automatically opens `memory.bin` from the snapshot
directory, so `file=...` should not be specified in `--memory` (the two
options are mutually exclusive).

```admonish note
The `--memory` and `--processors` values must match the values recorded in
the snapshot manifest. If they do not match, OpenVMM will report a
validation error and refuse to start.
```

## Device configuration on restore

The snapshot only stores device *state*, not device *configuration*. All
device flags (e.g. `--disk`, `--nic`, `--serial`, `--virtio-blk`, etc.)
must be specified on the restore command line exactly as they were when
the snapshot was saved — they are not read from the snapshot.

The snapshot manifest validates that `--memory`, `--processors`,
architecture, and page size match the values recorded at save time. However,
it does **not** record the list of CLI device flags. Instead, device
configuration compatibility is enforced at the state-unit level: each
emulated device saves its state under a unique name (e.g. `"pit"`,
`"vmbus"`, `"ide"`), and restore matches saved-state entries to the
currently instantiated devices by name.

The rules are:

| Scenario | Result |
|---|---|
| Device set matches exactly | Restore succeeds |
| Snapshot contains a device not in current config | **Restore fails** — unknown unit name |
| Current config has a device not in snapshot | Restore succeeds — device starts in its default/initial state |

In practice this means:

- You must pass the **same device flags** on restore as you did on save.
  Removing a device that was present at save time will cause restore to
  fail.
- Adding a *new* device that was not present at save time is technically
  allowed — the new device will start in its power-on default state.
  This is not tested and the device may not be functional, since the
  guest OS will not have enumerated or initialised it during boot.
  The supported path is to use the same device flags on save and restore.

```admonish warning
There is no single error message that tells you "your device configuration
changed". Instead you will see errors like `restore failed: unknown unit
name` when saved-state entries cannot be matched. If you see this, compare
your restore command line with the one used at save time.
```

## Device save/restore support

Not all devices support save/restore. If a VM includes a device that does
not support saving, the `save-snapshot` command will fail with
`SaveError::NotSupported`.

The following table summarises support for the device types relevant to
OpenVMM snapshots:

| Device | Bus | Save/Restore |
|---|---|---|
| PIT, PIC, I/O APIC, DMA | Chipset (ISA) | Yes |
| CMOS RTC, Power Management | Chipset (ISA) | Yes |
| i8042 (PS/2 keyboard/mouse) | Chipset (ISA) | Yes |
| Serial 16550 | Chipset (ISA) / PCI | Yes |
| UEFI firmware | Chipset (MMIO) | Yes |
| Framebuffer | Chipset (MMIO) | Yes |
| TPM | Chipset (MMIO) | Yes |
| IDE controller | PCI | Yes |
| PIIX4 bridges, bus, PM, RTC | PCI | Yes |
| Generic PCI bus | PCI | Yes |
| StorVsp (SCSI) | VMBus | Yes |
| NetVsp (NIC) | VMBus | Yes |
| Shutdown / Timesync / KVP ICs | VMBus | Yes |
| VMBus Keyboard / Mouse / Video | VMBus | Yes |
| Guest Emulation Log | VMBus | Yes |
| virtio-blk | Virtio (PCI/MMIO) | Yes |
| virtio-net | Virtio (PCI/MMIO) | Yes |
| virtio-pmem | Virtio (PCI/MMIO) | Yes |
| virtio-rng | Virtio (PCI/MMIO) | Yes |
| NVMe | PCI | **No** |
| VGA | PCI | **No** (`todo!()`) |
| GDMA (MANA network) | PCI | **No** (`todo!()`) |
| PCIe root complex / switch | PCIe | **No** |
| Assigned PCI (pass-through) | PCI | **No** |
| Relayed vPCI | PCI | **No** |
| PCAT BIOS firmware | Chipset (ISA) | **No** (see limitations) |
| virtio-9p, virtiofs | Virtio (PCI/MMIO) | **No** |
| virtio-console | Virtio (PCI/MMIO) | **No** |
| Guest Crash Device | VMBus | **No** |
| Guest Emulation Device (GED) | VMBus | **No** |
| VMBus serial (host) | VMBus | **No** |
| Vmbfs | VMBus | **No** |

```admonish tip
If you are unsure whether your VM configuration supports snapshots, try
issuing `save-snapshot` to a scratch directory. The save will fail
immediately with a clear error if any active device does not support it.
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
