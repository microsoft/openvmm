# StorVSP

StorVSP is the VMBus SCSI controller emulator. It presents a virtual
SCSI adapter to the guest over a VMBus channel and translates SCSI
requests into calls against the shared disk backend abstraction.

## Overview

StorVSP implements the Hyper-V synthetic SCSI protocol — a
VMBus-based transport that carries SCSI CDBs (Command Descriptor
Blocks) between the guest's `storvsc` driver and the host. This
isn't a standard SCSI transport (like iSCSI or SAS); it's a
Hyper-V-specific wire format defined in
[`storvsp_protocol`](https://openvmm.dev/rustdoc/linux/storvsp_protocol/index.html).
The guest side (`storvsc`) is in the Linux kernel and Windows inbox
drivers.

Each SCSI path (channel / target / LUN) maps to an
[`AsyncScsiDisk`](https://openvmm.dev/rustdoc/linux/scsi_core/trait.AsyncScsiDisk.html)
implementation — typically
[`SimpleScsiDisk`](https://openvmm.dev/rustdoc/linux/scsidisk/struct.SimpleScsiDisk.html)
for hard drives or
[`SimpleScsiDvd`](https://openvmm.dev/rustdoc/linux/scsidisk/scsidvd/struct.SimpleScsiDvd.html)
for optical media. Those implementations parse the SCSI CDB and
translate it into
[`DiskIo`](https://openvmm.dev/rustdoc/linux/disk_backend/trait.DiskIo.html)
calls (read, write, flush, unmap).

## Key characteristics

- **Transport.** VMBus ring buffers with GPADL-backed memory.
- **Protocol.** Hyper-V SCSI (SRB-based), with version negotiation
  (Win6 through Blue).
- **Sub-channels.** StorVSP supports multiple VMBus sub-channels
  for parallel I/O, one worker per channel.
- **Hot-add / hot-remove.** SCSI devices can be attached and
  detached at runtime via `ScsiControllerRequest`.
- **Performance.** Poll-mode optimization — when pending I/O count
  exceeds `poll_mode_queue_depth`, switches from interrupt-driven
  to busy-poll for new requests, reducing guest exit frequency.
- **Crate.** [`storvsp`](https://openvmm.dev/rustdoc/linux/storvsp/index.html)

The [storage pipeline](../../architecture/devices/storage.md) page
covers the full frontend-to-backend architecture, including the SCSI
adapter layer and how `SimpleScsiDisk` translates CDB opcodes to
`DiskIo` calls.
