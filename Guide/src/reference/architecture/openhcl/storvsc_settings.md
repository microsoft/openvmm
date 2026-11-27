# User-mode StorVSC: Settings & Resolver Flow

The user-mode StorVSC path integrates with OpenHCL's settings, device
resolution, and dispatch infrastructure following the NvmeManager pattern.

For the overall architecture and scope, see the companion architecture
and scope document.

---

## How device resolution works in OpenHCL

OpenHCL uses a **resolver pattern** to map device configuration (from host
VTL0 settings) to concrete device implementations:

```text
Host settings (VTL0)
  -> VTL2 settings worker parses device configs
  -> ResourceResolver dispatches to registered resolvers
  -> Resolver creates device instances (e.g., NvmeDisk, StorvscDisk)
  -> Device instances registered with OpenHCL's device framework
```

### Key types

- **`ResourceResolver`** -- central registry of resolvers, keyed by `(Kind, Config)` types
- **`AsyncResolveResource<Kind, Config>`** -- trait implemented by resolvers
- **`DiskHandleKind`** -- the resource kind for all disk backends
- **`ResolvedDisk`** -- the output type wrapping a `DiskIo` implementation

---

## The NvmeManager pattern (current reference)

NvmeManager on current `main` is the canonical reference for how a device
manager and disk resolver works in OpenHCL.

### End-to-end: NVMe VFIO switch vs StorVSC usermode switch

This table shows the 1:1 correspondence between the two paths:

| Step | NVMe VFIO (`main`) | StorVSC Usermode | Notes |
|------|-------------------|-----------------|-------|
| **1. Env var** | `OPENHCL_NVME_VFIO=1` | `OPENHCL_STORVSC_USERMODE=1` | Same pattern |
| **2. Parsed** | `Options.nvme_vfio` | `Options.storvsc_usermode` | Same parse pattern |
| **3. Passed to worker** | `env_cfg.nvme_vfio` | `env_cfg.storvsc_usermode` | Adjacent fields |
| **4. Manager created** | `NvmeManager::new(...)` | `StorvscManager::new(...)` | Adjacent code blocks in worker.rs |
| **5. Resolver registered** | `add_async_resolver(...NvmeDiskConfig...)` | `add_async_resolver(...StorvscDiskConfig...)` | Same API |
| **6. Device routing** | `DeviceType::NVMe -> NvmeDiskConfig` | `DeviceType::VScsi -> StorvscDiskConfig` | Early return before async block |
| **7. Resolve** | `NvmeDiskResolver -> NvmeDisk` | `StorvscDiskResolver -> StorvscDisk` | Same trait pattern |
| **8. Dispatch** | `nvme_manager: Option<NvmeManager>` | `storvsc_manager: Option<StorvscManager>` | Inspect, shutdown, save |

### Key takeaway

The StorVSC usermode switch is a **near-exact mirror** of the NVMe VFIO
switch. Every step has a 1:1 correspondence. The new integration code is
placed structurally adjacent to the NVMe code in each file.

### NvmeManager architecture (as of current `main`)

- Actor-based: mesh RPC between manager and per-device workers
- Multi-file module under `openhcl/underhill_core/src/nvme_manager/`
- Per-device worker tasks for request serialisation
- Lock ordering documented in module docs

---

## StorvscManager design

`StorvscManager` (single file: `storvsc_manager.rs`) follows a simplified
variant of the NvmeManager pattern. This simpler approach is justified by
StorVSC's narrower scope -- fewer devices and a simpler lifecycle than
NVMe VFIO.

### Key differences from NvmeManager

| Aspect | NvmeManager | StorvscManager |
|--------|------------|----------------|
| Size | Multi-file module | Single file |
| Architecture | Actor-based (mesh RPC) | Actor-based (mesh RPC, simplified) |
| Device creation | Per-device worker task | Inline in worker run loop |
| Save/restore | Dedicated modules | Inline methods |
| Complexity driver | Multiple PCI devices, namespaces, FLR | One controller per VMBus channel |

### Resolution flow

```text
StorvscDiskResolver::resolve(config)
  -> manager.get_driver(config.instance_guid)
    -> If new: open VMBus UIO channel, create StorvscDriver, negotiate protocol
    -> If exists: return Arc<StorvscDriver>
  -> StorvscDisk::new(driver, config.lun).await   // async constructor pre-fetches metadata
  -> ResolvedDisk::new(disk)
```

`StorvscDisk::new()` is async -- it pre-fetches disk metadata (capacity,
sector size, disk ID) during construction rather than issuing blocking
calls. This avoids deadlocking the async runtime, since the synchronous
`DiskIo` trait methods can't await futures.

---

## Settings integration: how VScsi devices reach StorvscManager

### Current path (kernel StorVSC -- what `main` does today)

```text
1. Host sends VScsi device config via VTL2 settings
2. vtl2_settings_worker detects DeviceType::VScsi
3. get_vscsi_devname() waits for Linux kernel to discover SCSI host
4. Maps to /dev/sdX block device in sysfs
5. Device accessed via kernel hv_storvsc driver
```

### Usermode path

```text
1. Host sends VScsi device config via VTL2 settings  (same)
2. vtl2_settings_worker detects DeviceType::VScsi     (same)
3. IF storvsc_usermode:
   a. Early return with StorvscDiskConfig { instance_guid, lun }
   b. ResourceResolver routes to StorvscDiskResolver
   c. StorvscManager opens VMBus channel in usermode via UIO
   d. StorvscDisk (DiskIo) returned
4. ELSE: fall through to kernel path (existing behavior)
```

The early return is placed **before** the async `devname` block (mirroring
how the NVMe VFIO check is placed before the kernel device discovery path).
This placement avoids type-mismatch issues that would arise from returning
different resource types inside a shared match arm.

### The toggle mechanism

- **Env var:** `OPENHCL_STORVSC_USERMODE=1` (opt-in, not enabled by default)
- **Safe:** If not set, behavior is identical to today (kernel path)
- **Comparison:** Same pattern as `OPENHCL_NVME_VFIO=1` which is default-on for NVMe

---

## Component summary

The usermode StorVSC settings integration comprises:

**Storage crates:**
- `disk_storvsc` -- `DiskIo` implementation with async metadata pre-fetch
- `storvsc_driver` -- VMBus SCSI client with DMA, resize, save/restore support
- `scsi_defs` -- SCSI struct types for CDB construction
- `storvsp_protocol` -- wire format definitions
- `vmbus_user_channel` -- configurable ring buffer sizes

**OpenHCL integration:**
- `StorvscManager` -- resolver and lifecycle manager (single file)
- Settings routing in `vtl2_settings_worker` -- VScsi early return pattern
- Worker, dispatch, servicing, UIO, and boot cmdline integration across 7 files
- Unit tests for CDB formatting and SCSI request structures

For terminology (VTL0/VTL2, VMBus, UIO, etc.), see the glossary in the
architecture doc.
