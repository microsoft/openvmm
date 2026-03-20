# Plan: ePCI (Emulated PCI) Support in mu_msvm

## Executive Summary

Today mu_msvm discovers PCI devices exclusively through **VPCI** (Virtual PCI over VMBus).
OpenVMM already has full ePCI support — Gen2 VMs expose PCIe devices via ECAM
(memory-mapped config space) with ACPI MCFG + SSDT tables.  The mu_msvm UEFI firmware
needs to be taught how to consume standard PCI config space (ECAM) in addition to the
existing VPCI path.  The good news: MU_BASECORE ships production-ready `PciHostBridgeDxe`
and `PciBusDxe` drivers.  The primary work is writing a platform-specific
`PciHostBridgeLib` and wiring up the build.

---

## Current Architecture

### OpenVMM Side (VMM — already working)

| Component | What it does | Key files |
|-----------|-------------|-----------|
| **PCIe Root Complex** | ECAM-based config space, multiple root ports | `vm/devices/pci/pcie/src/root.rs` |
| **MCFG table** | Tells guest where ECAM MMIO lives | `vmm_core/src/acpi_builder.rs` → `with_mcfg()` |
| **SSDT** | Declares `\_SB.PCI0` with `_HID=PNP0A08`, `_SEG`, `_BBN`, bus/MMIO ranges | `vm/acpi/src/ssdt.rs` → `add_pcie()` |
| **VMOD device** | Reserves ECAM ranges in ACPI so OS doesn't allocate over them | `vm/acpi/src/ssdt.rs` → `to_bytes()` |
| **MMIO layout** | Carves out low MMIO (ECAM + BAR window) and high MMIO from memory gaps | `openvmm/openvmm_entry/src/lib.rs` |

For Gen2 VMs, openvmm generates:
- **MCFG** table with `ecam_base`, `segment`, `start_bus`, `end_bus`
- **SSDT** with PCIe root complex ACPI devices (`PCI0`, `PCI1`, ...)
- Each root complex has ECAM MMIO region at `base + (bus<<20) + (dev<<15) + (fn<<12)`

### mu_msvm Side (UEFI firmware — needs changes)

| Component | Current state | Notes |
|-----------|--------------|-------|
| **VpcivscDxe** | Only PCI device provider today | Produces `EFI_PCI_IO_PROTOCOL` per VPCI device |
| **PlatformPei** | Parses MCFG from config blob, stores in `PcdMcfgPtr`/`PcdMcfgSize` | Already handles MCFG! |
| **AcpiPlatformDxe** | Installs MCFG table from config blob | Already installs MCFG |
| **PciHostBridgeDxe** | **Not included** in DSC/FDF | Available in MU_BASECORE |
| **PciBusDxe** | **Not included** in DSC/FDF | Available in MU_BASECORE |
| **PciHostBridgeLib** | **No platform implementation** | Null stub exists in MU_BASECORE |
| **PciLib** | `BasePciLibCf8` (legacy port I/O only) | Need ECAM-aware library |
| **NvmExpressDxe** | Binds to `EFI_PCI_IO_PROTOCOL` | Works with any provider — VPCI or ePCI |

---

## What Needs to Change

### Phase 1: mu_msvm — Platform PciHostBridgeLib (Core Work)

**New file: `MsvmPkg/Library/PciHostBridgeLib/PciHostBridgeLib.c`**

This is the main deliverable.  The library must implement two functions:

```c
PCI_ROOT_BRIDGE *
EFIAPI
PciHostBridgeGetRootBridges (OUT UINTN *Count);

VOID
EFIAPI
PciHostBridgeFreeRootBridges (PCI_ROOT_BRIDGE *Bridges, UINTN Count);
```

The implementation must:

1. **Read the MCFG table** from `PcdMcfgPtr` (already populated by PlatformPei's `Config.c`)
2. **Parse each `McfgSegmentBusRange` entry** to extract:
   - ECAM base address
   - PCI segment number
   - Start/end bus numbers
3. **Construct a `PCI_ROOT_BRIDGE` struct** per segment with:
   - `Segment` = MCFG segment
   - `Bus` aperture = `{start_bus, end_bus}`
   - `Mem`/`MemAbove4G` apertures from MMIO ranges (derived from ACPI `_CRS` or config blob MMIO gap PCDs)
   - `PMem`/`PMemAbove4G` = same as Mem ranges (no prefetchable distinction needed initially)
   - `Io` = `{0, 0xFFFF}` or restricted range
   - `Translation` = 0 (identity mapping)
   - `AllocationAttributes` = `EFI_PCI_HOST_BRIDGE_COMBINE_MEM_PMEM` | `EFI_PCI_HOST_BRIDGE_MEM64_DECODE`
   - Device path = ACPI HID `PNP0A08`, UID = segment

**MMIO Range Discovery Strategy:** The MCFG table tells us where config space is, but
we also need the MMIO apertures for BAR allocation.  Three options:

- **Option A (Recommended):** Parse the SSDT `_CRS` from the ACPI table blob to extract
  `QWordMemory` descriptors per root bridge.  The openvmm SSDT already encodes low
  and high MMIO ranges.
- **Option B:** Add new UEFI config blob entries for per-root-bridge MMIO apertures
  (requires openvmm changes to the config blob protocol).
- **Option C:** Use the existing `PcdLowMmioGap*`/`PcdHighMmioGap*` PCDs as a single
  large aperture and let `PciBusDxe` handle sub-allocation.  Simpler but less precise.

**New file: `MsvmPkg/Library/PciHostBridgeLib/PciHostBridgeLib.inf`**

```ini
[Defines]
  INF_VERSION    = 0x00010005
  BASE_NAME      = PciHostBridgeLib
  MODULE_TYPE    = DXE_DRIVER
  LIBRARY_CLASS  = PciHostBridgeLib

[Sources]
  PciHostBridgeLib.c

[Packages]
  MdePkg/MdePkg.dec
  MdeModulePkg/MdeModulePkg.dec
  MsvmPkg/MsvmPkg.dec

[LibraryClasses]
  BaseMemoryLib
  DebugLib
  PcdLib
  MemoryAllocationLib
  DevicePathLib

[Pcd]
  gMsvmPkgTokenSpaceGuid.PcdMcfgPtr
  gMsvmPkgTokenSpaceGuid.PcdMcfgSize
  gMsvmPkgTokenSpaceGuid.PcdLowMmioGapBasePageNumber
  gMsvmPkgTokenSpaceGuid.PcdLowMmioGapSizeInPages
  gMsvmPkgTokenSpaceGuid.PcdHighMmioGapBasePageNumber
  gMsvmPkgTokenSpaceGuid.PcdHighMmioGapSizeInPages
```

### Phase 2: DSC/FDF Wiring

**`MsvmPkgX64.dsc` changes:**

```ini
# In [LibraryClasses] section:
  PciHostBridgeLib|MsvmPkg/Library/PciHostBridgeLib/PciHostBridgeLib.inf
  PciSegmentLib|MdePkg/Library/BasePciSegmentLibPci/BasePciSegmentLibPci.inf

# In [Components] section, add:
  MdeModulePkg/Bus/Pci/PciHostBridgeDxe/PciHostBridgeDxe.inf
  MdeModulePkg/Bus/Pci/PciBusDxe/PciBusDxe.inf
```

**`MsvmPkgX64.fdf` changes:**

```ini
# In the DXE firmware volume, add:
  INF MdeModulePkg/Bus/Pci/PciHostBridgeDxe/PciHostBridgeDxe.inf
  INF MdeModulePkg/Bus/Pci/PciBusDxe/PciBusDxe.inf
```

Same changes for `MsvmPkgAARCH64.dsc` / `MsvmPkgAARCH64.fdf`.

### Phase 3: PCI Segment and Express Library Support

The existing `BasePciLibCf8` only supports legacy port I/O config access (0xCF8/0xCFC).
For PCIe ECAM, we need:

**Option A (Best):** Use `MdePkg/Library/BasePciExpressLib/BasePciExpressLib.inf` and set:
```ini
  gEfiMdePkgTokenSpaceGuid.PcdPciExpressBaseAddress|<ecam_base_from_mcfg>
```
This works if there's a single PCI segment.  For multi-segment support, use
`PciSegmentLib` which routes to the correct ECAM base per segment.

**Option B (Multi-segment):** Use `BasePciSegmentLibPci` which is segment-aware and
routes through `PciLib`.  This requires a PciLib that understands ECAM, or a custom
`PciSegmentLib` that maps segments to ECAM base addresses from MCFG.

**Recommendation:** Start with single-segment support using `BasePciExpressLib` +
a PCD for the base address.  The `PciHostBridgeDxe` + `PciBusDxe` stack uses
`EFI_PCI_ROOT_BRIDGE_IO_PROTOCOL` internally (not `PciLib`), so the library choice
mostly affects other consumers.

### Phase 4: Memory Map Coordination

PlatformPei already registers MMIO gaps as `EfiMemoryMappedIO` via `HobAddMmioRange()`.
For ePCI we need to ensure:

1. **ECAM range is mapped as UC (uncacheable) MMIO** in the GCD.
   - PlatformPei should add the ECAM range from MCFG as an MMIO HOB.
   - Current code in `Config.c` parses MCFG but doesn't create HOBs for the ECAM range.
   - **Add:** After parsing MCFG, call `HobAddMmioRange()` for each ECAM segment's
     address range to ensure it's in the GCD as MMIO.

2. **BAR MMIO ranges** fall within the existing MMIO gaps, so they should already be
   covered by the current `PcdLowMmioGap*`/`PcdHighMmioGap*` HOBs.

### Phase 5: VPCI + ePCI Coexistence

Both paths must coexist.  A VM may have some devices on VPCI (e.g., passed-through
devices) and others on ePCI (e.g., emulated NVMe).  This works naturally because:

- `VpcivscDxe` binds to VMBus channels → produces `EFI_PCI_IO_PROTOCOL` for VPCI devices
- `PciBusDxe` scans ECAM config space → produces `EFI_PCI_IO_PROTOCOL` for ePCI devices
- Consumer drivers (`NvmExpressDxe`, etc.) bind to `EFI_PCI_IO_PROTOCOL` regardless of source
- No changes needed to VpcivscDxe or consumer drivers

**Potential conflict:** Both paths could expose the same device.  The VMM should ensure
a device is offered through exactly one path (VPCI or ePCI), never both.

---

## Detailed Work Items

### mu_msvm Changes

| # | Work Item | Files | Complexity |
|---|-----------|-------|------------|
| 1 | **Implement `PciHostBridgeLib`** | New: `MsvmPkg/Library/PciHostBridgeLib/` | Medium |
| 2 | **Register ECAM MMIO in PlatformPei** | Modify: `MsvmPkg/PlatformPei/Config.c` | Low |
| 3 | **Add PCI drivers to DSC** | Modify: `MsvmPkg/MsvmPkgX64.dsc`, `MsvmPkgAARCH64.dsc` | Low |
| 4 | **Add PCI drivers to FDF** | Modify: `MsvmPkg/MsvmPkgX64.fdf`, `MsvmPkgAARCH64.fdf` | Low |
| 5 | **Add PCI library instances to DSC** | Modify: DSC files (PciExpressLib, PciSegmentLib) | Low |
| 6 | **Set `PcdPciExpressBaseAddress` PCD** | Modify: DSC files or dynamic PCD in PlatformPei | Low |
| 7 | **Verify firmware volume size** | Check FDF — adding two DXE drivers increases image size | Low |
| 8 | **Test with NVMe on ePCI** | Integration test with openvmm | Medium |

### openvmm Changes (Likely None Required)

OpenVMM already provides everything needed:
- MCFG table in the config blob (consumed by PlatformPei)
- SSDT with `_CRS` for bus/MMIO ranges (consumed by AcpiPlatformDxe → OS)
- ECAM MMIO region properly configured
- PCIe root complex with hot-plug support

**One potential change:** If the SSDT with PCI root bridge info is generated by openvmm
but mu_msvm's `PciHostBridgeLib` needs MMIO apertures at DXE time (before OS-level ACPI
parsing), the config blob may need to carry per-root-bridge MMIO apertures explicitly.
Check whether the existing MMIO gap PCDs are sufficient, or whether the SSDT `_CRS`
data should be extractable from the config blob directly.

---

## Boot Flow with ePCI

```
PEI Phase:
  PlatformPei
    → Config.c parses config blob
    → Extracts MCFG → PcdMcfgPtr / PcdMcfgSize
    → Extracts MMIO ranges → PcdLowMmioGap* / PcdHighMmioGap*
    → NEW: Creates MMIO HOBs for ECAM ranges from MCFG
    → NEW: Sets PcdPciExpressBaseAddress from MCFG base

DXE Phase:
  PciHostBridgeDxe starts
    → Calls PciHostBridgeGetRootBridges()  [our new PciHostBridgeLib]
    → Library reads PcdMcfgPtr, parses MCFG entries
    → Returns PCI_ROOT_BRIDGE array with segment/bus/MMIO apertures
    → PciHostBridgeDxe registers EFI_PCI_HOST_BRIDGE_RESOURCE_ALLOCATION_PROTOCOL

  PciBusDxe starts
    → Consumes PCI host bridge protocol
    → Enumerates each root bridge's bus range via ECAM MMIO reads
    → For each device found: creates EFI_PCI_IO_PROTOCOL handle
    → Allocates MMIO BARs from root bridge apertures

  NvmExpressDxe (and other consumer drivers)
    → Binds to EFI_PCI_IO_PROTOCOL handles (from PciBusDxe OR VpcivscDxe)
    → Works identically regardless of ePCI or VPCI source

  VpcivscDxe (unchanged)
    → Continues to discover VPCI devices over VMBus
    → Produces EFI_PCI_IO_PROTOCOL for those devices
```

---

## Risks and Considerations

1. **MSI/MSI-X Interrupt Routing**: In ePCI, MSI address/data are programmed directly
   into the device config space by PciBusDxe.  OpenVMM's PCIe root complex already
   handles this.  However, UEFI drivers typically run in polling mode during boot, so
   MSI setup is primarily needed for OS handoff.  Verify that `PciBusDxe` correctly
   programs MSI capabilities.

2. **DMA / Bus Mastering**: `PciBusDxe` provides `EFI_PCI_IO_PROTOCOL.Map()` and
   `Unmap()` for DMA.  In a non-isolated VM this is identity-mapped.  For isolated VMs
   (SNP/TDX), DMA buffers must be host-visible — this may need coordination with
   `EfiHvDxe` memory visibility controls.

3. **Firmware Volume Size**: Adding `PciHostBridgeDxe` + `PciBusDxe` increases the
   DXE firmware volume.  Current DXE FV is 5 MB — should be sufficient, but verify.

4. **AARCH64 Support**: The same approach works for ARM64 since PCIe ECAM is
   architecture-neutral.  However, interrupt routing differs (GIC ITS vs APIC).
   `PciBusDxe` is arch-agnostic, but MSI configuration may need attention.

5. **Config Blob Protocol**: OpenVMM provides MCFG in the config blob.  Verify that the
   MCFG entries match the actual ECAM MMIO regions.  The openvmm MCFG builder adjusts
   the base address to reflect bus 0 (`base - start_bus * 0x100000`), which is the
   standard MCFG convention.

6. **No I/O Port Space**: Gen2 VMs may not have legacy I/O port space.  `PciBusDxe`
   should handle this gracefully if `Io` aperture is empty.  The `BasePciLibCf8` library
   should NOT be used for ePCI config access — ensure it's only used by legacy code paths.

---

## Testing Strategy

1. **Unit Test**: Build mu_msvm with the new drivers, boot in openvmm with an NVMe
   device on a PCIe root complex (not VPCI).  Verify NVMe driver binds and disk
   is accessible in UEFI shell.

2. **Coexistence Test**: Boot with both VPCI NVMe and ePCI NVMe.  Verify both
   disks are visible.

3. **Multi-Segment Test**: Configure openvmm with multiple PCIe root complexes
   (different segments).  Verify all are enumerated.

4. **OS Boot Test**: Install and boot a guest OS (Windows/Linux) from an ePCI NVMe
   disk through mu_msvm.

---

## Summary

The core work is a single new library (`PciHostBridgeLib`) in mu_msvm plus DSC/FDF
wiring changes.  All other components — the generic PCI bus drivers, the VMM-side ECAM
emulation, the ACPI tables, and the consumer drivers — already exist and work.  This is
a well-scoped, medium-complexity change with high confidence of success.
