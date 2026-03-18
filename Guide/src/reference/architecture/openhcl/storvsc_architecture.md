# User-mode StorVSC: Architecture & Scope

This document covers the architecture and scope for replacing the Linux
kernel `hv_storvsc` driver in OpenHCL with a usermode Rust implementation
([#273](https://github.com/microsoft/openvmm/issues/273)).

---

## Architecture overview

### Motivation

OpenHCL currently uses the Linux kernel's `hv_storvsc` driver for synthetic
SCSI storage. This causes:

- Expensive VTL0-to-kernel address mapping (especially on arm64)
- Unwanted IO during boot/servicing (kernel storage stack discovers block devices)
- Double-buffering for non-page-aligned guest IOs
- Limited instrumentation (can't easily modify kernel driver)

### Solution

Replace the kernel StorVSC with a **usermode Rust implementation** in OpenVMM,
running inside OpenHCL's VTL2.

### System context

```text
+----------------------------------+     +------------------------------+
|  Windows Host                    |     |  OpenHCL (VTL2)              |
|                                  |     |                              |
|  storvsp (unchanged)             |     |  StorvscManager (resolver)   |
|    |                             |     |    |                         |
|    |  vStor protocol             |     |  disk_storvsc (DiskIo impl)  |
|    |  over VMBus                 |     |    |                         |
|    +-----------------------------+-----+  storvsc_driver (VMBus SCSI) |
|                                  |     |                              |
+----------------------------------+     +------------------------------+
                                                    |
                                          Guest block device (VTL0)
```

### Trust boundaries

The VMBus channel carries data from the Windows host into VTL2. However,
storvsp is a trusted Windows component, and the protocol parsing in
`storvsc_driver` follows the same patterns as other VMBus device drivers
in OpenVMM (e.g., the NVMe driver). No untrusted data crosses this boundary.
Protocol parsing uses zerocopy safe deserialisation (`FromBytes`, `IntoBytes`)
and returns typed errors (`PacketError`) on malformed packets rather than
panicking. Both `disk_storvsc` (`#![forbid(unsafe_code)]`) and
`storvsc_driver` (0 unsafe blocks) are entirely safe Rust.

The VTL0-to-VTL2 boundary is handled by the existing OpenHCL disk subsystem
-- `disk_storvsc` sits behind the same `DiskIo` trait used by all disk
backends, so it inherits the same trust model.

---

## Component map

### Layer 1: `storvsc_driver` -- VMBus SCSI Client

Speaks the vStor/storvsp protocol over VMBus. Handles 5-step protocol
negotiation, sends SCSI requests, correlates completions via slab. Supports
DMA buffer allocation, disk resize notifications, and save/restore for
servicing. One instance per SCSI controller, shared across all LUNs.

Crate path: `vm/devices/storage/storvsc_driver/`

### Layer 2: `disk_storvsc` -- Disk Backend

Implements the `DiskIo` trait (standard OpenVMM disk interface). Translates
disk operations (read, write, sync, unmap) into SCSI CDBs via
`storvsc_driver`. One instance per disk (LUN). Uses an async constructor
to pre-fetch disk metadata, avoiding `block_on` deadlocks when called
from within the async runtime.

Crate path: `vm/devices/storage/disk_storvsc/`

### Layer 3: `StorvscManager` -- Resolver

Manages shared `StorvscDriver` instances per VMBus SCSI controller.
Implements `AsyncResolveResource` for the OpenHCL resource resolver.
Supports save/restore for servicing. Uses a mesh RPC actor pattern
matching NvmeManager's approach.

During save, the driver task is stopped and any pending (in-flight) SCSI
transactions are cancelled with `StorvscCompleteReason::SaveRestore`,
which lets upper layers retry after restore. The serialised state
(`StorvscManagerSavedState`) contains a vec of per-controller entries,
each holding the controller instance GUID plus a
`StorvscDriverSavedState` (negotiated protocol version, sub-channel
count, and whether negotiation completed). On restore, the manager
recreates drivers from these entries and resumes the worker task.

```text
  StorvscDiskResolver              StorvscManagerWorker (actor)
    |                                |
    |-- GetDriver(guid) -----------> |  recv.next().await
    |                                |  match req {
    |                                |    GetDriver(rpc) => get_or_create driver
    |                                |    Save(rpc) => serialize driver state
    |                                |    Shutdown => drain + stop all drivers
    |                                |  }
    |<-- Arc<StorvscDriver> -------- |
```

File path: `openhcl/underhill_core/src/storvsc_manager.rs`

### Supporting changes

The integration touches settings, dispatch, servicing, UIO, and VMBus layers
across several OpenHCL files. Key changes:

- `options.rs` -- `OPENHCL_STORVSC_USERMODE` env var parsing
- `worker.rs` -- manager creation + resolver registration (adjacent to NvmeManager code)
- `vtl2_settings_worker.rs` -- VScsi device routing to `StorvscDiskConfig`
- `dispatch/mod.rs` -- inspect, shutdown, save integration
- `servicing.rs` -- `StorvscSavedState` at mesh(10004)
- `openhcl_boot/main.rs` -- env var on kernel cmdline (disabled by default)
- `underhill_init/src/lib.rs` -- SCSI VMBus class GUID registered for UIO

---

## Comparison: kernel vs usermode StorVSC

| Aspect | Kernel path (current) | Usermode path |
|--------|----------------------|---------------|
| Driver code | Linux kernel `hv_storvsc` module | `storvsc_driver` (Rust) |
| Disk backend | Kernel block layer `/dev/sdX` | `disk_storvsc` (Rust) |
| Manager/resolver | N/A (kernel auto-discover) | `StorvscManager` following `NvmeManager` pattern |
| Device discovery | sysfs uevent | Settings-driven via `ResourceResolver` |
| VMBus channel | Kernel VMBus driver | UIO usermode channel |
| DMA model | Kernel manages buffers + double-copy | Direct GPA-to-VA mapping, no double-copy |
| Save/restore | Not supported (kernel state opaque) | `StorvscSavedState` protobuf serialization |
| Instrumentation | Limited (kernel tracepoints) | Full Rust `tracing` + `Inspect` support |
| Activation | Always on | `OPENHCL_STORVSC_USERMODE=1` (opt-in) |

---

## Data flow

### SCSI command lifecycle

```text
1. Guest OS (VTL0) issues read/write
2. OpenHCL disk subsystem calls DiskIo::read_vectored() / write_vectored()
3. disk_storvsc builds SCSI CDB (READ16/WRITE16), locks guest memory
4. storvsc_driver wraps CDB in storvsp_protocol::ScsiRequest, sends over VMBus
5. VMBus transport -> Windows host storvsp -> physical storage
6. Completion returns via VMBus -> storvsc_driver matches by transaction ID
7. disk_storvsc returns Result to caller
```

### Settings integration flow

```text
1. OPENHCL_STORVSC_USERMODE=1 on kernel cmdline (opt-in)
2. Options::storvsc_usermode = true
3. worker.rs: StorvscManager created, StorvscDiskResolver registered
4. vtl2_settings_worker.rs: VScsi devices routed to StorvscDiskConfig
5. Resolver -> StorvscManager -> gets/creates shared storvsc_driver
6. StorvscDisk (DiskIo) returned to disk subsystem
```

The settings integration -- how device configs flow from host through
the resolver to concrete device instances -- is covered in a separate
settings flow document.

---

## Scope boundaries

### In scope

| Item | Description |
|------|------------|
| `disk_storvsc` crate | `DiskIo` backend that translates disk operations into SCSI CDBs via `storvsc_driver` |
| `StorvscManager` integration | Resolver and lifecycle manager following the NvmeManager pattern |
| Settings and dispatch wiring | Env var toggle, VScsi device routing, servicing support |
| Unit tests | Coverage for SCSI CDB formatting, request structures, protocol negotiation |
| Integration tests | VM boots and completes basic IO with usermode path enabled |
| Performance sanity check | Basic IOPS comparison vs kernel path |
| Save/restore (servicing) | Servicing round-trip with usermode storvsc state |

### Out of scope

| Item | Rationale |
|------|-----------|
| Subchannel support ([#1511](https://github.com/microsoft/openvmm/issues/1511)) | Complex multi-queue design. Subchannels allow multi-queue IO, needed for performance parity with the kernel driver's multi-channel support. |
| Default enablement | Keep opt-in until proven stable |
| hv_storvsc kernel module removal | Not removing from rootfs until default enabled |
| Full performance parity | Requires subchannels (out of scope) |
| Windows guest validation | Linux guests only; Windows guest testing deferred until the core path is stable |

---

## Key design decisions

### Single-level vs multi-level actor

Both StorvscManager and NvmeManager use mesh RPC actors -- a worker task
that receives `Request` messages and processes them sequentially. NvmeManager
adds a second level of per-device worker tasks to serialize per-device
operations. StorvscManager doesn't need this because StorVSC has a simpler
device model: one controller per VMBus channel, no namespace hierarchy, no
function-level resets.

### Why usermode Rust over patching the kernel driver?

The Linux kernel `hv_storvsc` module has four structural limitations that
can't be fixed by patching it:
1. **Address mapping cost** -- the kernel must map VTL0 guest physical
   addresses through the kernel's page tables. On AArch64, this is
   expensive. A usermode driver can map GPAs directly into VTL2 VA space.
2. **Block device discovery** -- the kernel driver creates `/dev/sdX` devices
   and runs the full block layer discovery stack, causing unwanted IO at
   boot and servicing. Usermode resolves disks on-demand via settings, no
   enumeration.
3. **Save/restore opacity** -- the kernel driver's internal state is opaque
   to VTL2 usermode. Servicing can't save and restore it. A Rust driver
   serialises its state via protobuf (`StorvscDriverSavedState`).
4. **Instrumentation** -- kernel tracepoints are limited and hard to
   correlate with usermode telemetry. A Rust driver participates in the
   same `tracing` and `Inspect` framework used by the rest of OpenHCL.

### Why mirror NvmeManager's resolver/actor pattern?

A simpler alternative would be to create `StorvscDriver` instances directly
in `vtl2_settings_worker` when VScsi devices are configured. NvmeManager's
actor pattern was chosen instead because:
- **Shared drivers** -- multiple disks (LUNs) share one controller driver.
  The manager caches drivers by instance GUID and hands out `Arc` clones.
  Without it, each resolver call would need to coordinate creation itself.
- **Save/restore** -- the manager owns the driver map, so it can iterate
  all drivers to save and restore state during servicing.
- **Shutdown** -- the actor processes a `Shutdown` request that drains all
  drivers, giving a clean teardown path.
- **Inspect** -- the actor exposes driver state to the diagnostic framework.

These are the same reasons NvmeManager exists. Reusing a proven pattern
reduces design risk and keeps the storage subsystem consistent.

### All-or-nothing toggle and UIO channel binding

When `OPENHCL_STORVSC_USERMODE=1`, all VScsi controllers are routed to the
usermode path. Mixed mode (some controllers kernel, some usermode) is not
supported, matching NVMe VFIO's approach.

The SCSI VMBus class GUID is registered with `uio_hv_generic` in
`underhill_init` before kernel modules load, ensuring UIO claims the
channels before `hv_storvsc.ko`. The NVMe VFIO path uses the same
pattern (register VFIO before module load). The registration is
conditional on the env var so the kernel path still works when usermode
is disabled.

---

## Validation approach

### Inner loop (CI)
- Unit tests for `disk_storvsc` CDB formatting and SCSI request structures
- Unit tests for `storvsc_driver` (negotiate, enumerate bus, request/response)
- `cargo check -p underhill_core` passes cleanly

### Outer loop (pending boot constraint resolution)
- Boot test: VM boots with `OPENHCL_STORVSC_USERMODE=1`, finds boot disk
- Basic IO: read/write/sync/unmap operations complete successfully
- Servicing: save/restore round-trip preserves storvsc state
- Performance: basic IOPS comparison vs kernel path (sanity check, not parity target)

---

## References

| Resource | Link |
|----------|------|
| Tracking issue | [#273](https://github.com/microsoft/openvmm/issues/273) |
| Subchannel tracking issue | [#1511](https://github.com/microsoft/openvmm/issues/1511) |
| NvmeManager (reference pattern) | [openhcl/underhill_core/src/nvme_manager/](https://github.com/microsoft/openvmm/tree/main/openhcl/underhill_core/src/nvme_manager/) |

---

## Glossary

| Term | Meaning |
|------|---------|
| VTL0 / VTL2 | Virtual Trust Levels. VTL0 is the guest OS; VTL2 is the secure OpenHCL paravisor environment |
| VMBus | Hyper-V virtual machine bus for synthetic device communication |
| UIO | Userspace I/O -- Linux mechanism to expose device resources to usermode drivers |
| GPA | Guest Physical Address -- memory address in the guest's physical address space |
| `DiskIo` | OpenVMM trait for disk backends (read, write, sync, unmap) |
| mesh RPC | OpenVMM's actor framework for async inter-component communication |
