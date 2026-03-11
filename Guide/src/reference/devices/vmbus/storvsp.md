# StorVSP

StorVSP is the VMBus SCSI controller emulator. It presents a virtual SCSI adapter to the guest over a VMBus channel and translates SCSI requests into calls against the shared disk backend abstraction.

## Overview

StorVSP implements a VMBus device that speaks the Hyper-V SCSI protocol. The guest's storage driver (`storvsc`) sends SCSI request packets through a VMBus ring buffer; StorVSP dequeues them and dispatches each request to the appropriate SCSI device.

Each SCSI path (channel / target / LUN) maps to an `AsyncScsiDisk` implementation — typically `SimpleScsiDisk` for hard drives or `SimpleScsiDvd` for optical media. Those implementations parse the SCSI CDB and translate it into `DiskIo` trait calls (read, write, flush, unmap).

## Key characteristics

- **Transport:** VMBus ring buffers with GPADL-backed memory.
- **Protocol:** Hyper-V SCSI (SRB-based).
- **Sub-channels:** StorVSP supports multiple VMBus sub-channels for parallel I/O, one worker per channel.
- **Hot-add / hot-remove:** SCSI devices can be attached and detached at runtime via `ScsiControllerRequest`.
- **Crate:** `storvsp/`

```admonish note title="See also"
[Storage Pipeline](../../architecture/devices/storage.md) for the full frontend-to-backend architecture, including the SCSI adapter layer and how `SimpleScsiDisk` translates CDB opcodes to `DiskIo` calls.
```
