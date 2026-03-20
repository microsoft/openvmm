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
   - `Mem`/`MemAbove4G` apertures from config blob `PcieBarApertures` entry (per-bridge low/high MMIO)
   - `PMem`/`PMemAbove4G` = same as Mem ranges (no prefetchable distinction needed initially)
   - `Io` = `{0, 0xFFFF}` or restricted range
   - `Translation` = 0 (identity mapping)
   - `AllocationAttributes` = `EFI_PCI_HOST_BRIDGE_COMBINE_MEM_PMEM` | `EFI_PCI_HOST_BRIDGE_MEM64_DECODE`
   - Device path = ACPI HID `PNP0A08`, UID = segment

**MMIO Range Discovery Strategy:** The MCFG table tells us where config space is, but
we also need the MMIO apertures for BAR allocation.  Three options:

- **Option A:** Parse the SSDT `_CRS` from the ACPI table blob to extract
  `QWordMemory` descriptors per root bridge.  The openvmm SSDT already encodes low
  and high MMIO ranges.  Rejected — parsing AML in UEFI PEI/DXE is complex, fragile,
  and unnecessary when we control both sides of the protocol.
- **Option B (Recommended):** Add a new config blob entry type (`PcieBarApertures`,
  e.g. type `0x28`) that carries per-root-bridge MMIO apertures in a simple packed
  struct.  This requires a small openvmm change to emit the new entry alongside the
  existing MCFG entry, plus a matching parser in PlatformPei.  Clean, explicit, and
  trivial to parse.  See details below.
- **Option C:** Use the existing `PcdLowMmioGap*`/`PcdHighMmioGap*` PCDs as a single
  large aperture and let `PciBusDxe` handle sub-allocation.  Simpler but less precise —
  breaks with multiple root complexes.

#### Option B Details: `PcieBarApertures` Config Blob Entry

**New blob structure type:** `PcieBarApertures = 0x28` (next available after `Iort = 0x27`)

The entry follows the standard config blob convention: an 8-byte `UEFI_CONFIG_HEADER`
followed by a flat array of fixed-size per-bridge entries.  The entry count is derived
from the header length (same pattern as `UefiConfigMmioRanges`).  All fields are
naturally aligned to avoid packing issues.

**Wire format (little-endian, all fields naturally aligned):**

```
Config blob entry layout:

  ┌──────────────────────────────────────────┐
  │  UEFI_CONFIG_HEADER  (8 bytes)           │
  │    Type   = 0x28 (PcieBarApertures)      │
  │    Length = 8 + N * 48                    │
  ├──────────────────────────────────────────┤
  │  PCIE_BAR_APERTURE_ENTRY[0]  (48 bytes)  │
  ├──────────────────────────────────────────┤
  │  PCIE_BAR_APERTURE_ENTRY[1]  (48 bytes)  │
  ├──────────────────────────────────────────┤
  │  ...                                     │
  └──────────────────────────────────────────┘
```

**C struct definitions** (for `MsvmPkg/Include/BiosInterface.h`):

```c
//
// Per-root-bridge MMIO aperture descriptor.
// One entry per PCIe root bridge / host bridge segment.
// Matches by Segment number with the MCFG table entries.
//
// 48 bytes, all fields naturally aligned (no #pragma pack needed).
//
typedef struct _PCIE_BAR_APERTURE_ENTRY {
    UINT16  Segment;            // PCI segment number (matches MCFG)
    UINT8   StartBus;           // Lowest valid bus number
    UINT8   EndBus;             // Highest valid bus number
    UINT32  Reserved;           // Padding to 8-byte boundary; must be 0
    UINT64  LowMmioBase;        // Low MMIO window base address (below 4 GB)
    UINT64  LowMmioLength;      // Low MMIO window length in bytes
    UINT64  HighMmioBase;        // High MMIO window base address (above 4 GB)
    UINT64  HighMmioLength;      // High MMIO window length in bytes
} PCIE_BAR_APERTURE_ENTRY;      // sizeof = 48

typedef struct _UEFI_CONFIG_PCIE_BAR_APERTURES {
    UEFI_CONFIG_HEADER          Header;
    PCIE_BAR_APERTURE_ENTRY     Entries[];   // Variable-length array
} UEFI_CONFIG_PCIE_BAR_APERTURES;
```

Entry count is derived: `N = (Header.Length - sizeof(UEFI_CONFIG_HEADER)) / sizeof(PCIE_BAR_APERTURE_ENTRY)`

**Rust struct** (for `vm/loader/src/uefi/config.rs`):

```rust
/// Per-root-bridge MMIO aperture descriptor for the config blob.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct PcieBarApertureEntry {
    pub segment: u16,
    pub start_bus: u8,
    pub end_bus: u8,
    pub reserved: u32,
    pub low_mmio_base: u64,
    pub low_mmio_length: u64,
    pub high_mmio_base: u64,
    pub high_mmio_length: u64,
}
```

**Serialization** (in `openvmm_core/src/worker/vm_loaders/uefi.rs`, after the MCFG entry):

```rust
if !pcie_host_bridges.is_empty() {
    let entries: Vec<u8> = pcie_host_bridges
        .iter()
        .flat_map(|b| {
            config::PcieBarApertureEntry {
                segment: b.segment,
                start_bus: b.start_bus,
                end_bus: b.end_bus,
                reserved: 0,
                low_mmio_base: b.low_mmio.start(),
                low_mmio_length: b.low_mmio.len(),
                high_mmio_base: b.high_mmio.start(),
                high_mmio_length: b.high_mmio.len(),
            }
            .as_bytes()
            .to_vec()
        })
        .collect();
    cfg.add_raw(config::BlobStructureType::PcieBarApertures, &entries);
}
```

**Parsing** (in `MsvmPkg/PlatformPei/Config.c`):

```c
case UefiConfigPcieBarApertures:
{
    UEFI_CONFIG_PCIE_BAR_APERTURES *apertures =
        (UEFI_CONFIG_PCIE_BAR_APERTURES *) header;
    UINT32 dataSize = header->Length - sizeof(UEFI_CONFIG_HEADER);

    //
    // Validate: data size must be a whole multiple of entry size,
    // and at least one entry must be present.
    //
    if (dataSize == 0 ||
        (dataSize % sizeof(PCIE_BAR_APERTURE_ENTRY)) != 0)
    {
        DEBUG((DEBUG_ERROR, "*** Malformed PcieBarApertures\n"));
        FAIL_FAST_UNEXPECTED_HOST_BEHAVIOR();
    }

    PEI_FAIL_FAST_IF_FAILED(
        PcdSet64S(PcdPcieBarAperturesPtr, (UINT64) apertures->Entries));
    PEI_FAIL_FAST_IF_FAILED(
        PcdSet32S(PcdPcieBarAperturesSize, dataSize));
    break;
}
```

**Consumption** (in `PciHostBridgeLib`):

```c
//
// Build PCI_ROOT_BRIDGE array by joining MCFG (bus/ECAM) with
// PcieBarApertures (MMIO windows), matched by Segment number.
//
PCIE_BAR_APERTURE_ENTRY *Apertures =
    (PCIE_BAR_APERTURE_ENTRY *)(UINTN) PcdGet64(PcdPcieBarAperturesPtr);
UINT32 ApertureCount =
    PcdGet32(PcdPcieBarAperturesSize) / sizeof(PCIE_BAR_APERTURE_ENTRY);

// For each MCFG segment, find matching aperture entry by Segment number
// and populate Mem / MemAbove4G from LowMmio / HighMmio fields.
```

**Design rationale:**

- **Flat array, no separate count field** — matches `UefiConfigMmioRanges` precedent;
  count derived from `Header.Length`.
- **48-byte entry with explicit `Reserved` padding** — keeps all `UINT64` fields at
  natural 8-byte alignment without `#pragma pack`.  Entry size is itself a multiple
  of 8, so entries are naturally aligned in the array.
- **Matched by `Segment`** — the `PciHostBridgeLib` correlates aperture entries with
  MCFG entries by segment number.  `StartBus`/`EndBus` are included for cross-validation
  but the primary key is `Segment`.
- **Length not offset** — `LowMmioLength` / `HighMmioLength` instead of base + limit,
  consistent with the `MmioSizeInPages` pattern in `UefiConfigMmioRanges`.

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
  gMsvmPkgTokenSpaceGuid.PcdPcieBarAperturesPtr
  gMsvmPkgTokenSpaceGuid.PcdPcieBarAperturesSize
```

### Phase 2: DSC/FDF Wiring

**`MsvmPkgX64.dsc` changes:**

```ini
# In [LibraryClasses] section:
  PciHostBridgeLib|MsvmPkg/Library/PciHostBridgeLib/PciHostBridgeLib.inf
  PciSegmentLib|MdePkg/Library/PciSegmentLibSegmentInfo/BasePciSegmentLibSegmentInfo.inf
  PciSegmentInfoLib|MsvmPkg/Library/PciSegmentInfoLib/PciSegmentInfoLib.inf

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

### Phase 3: Multi-Segment PciSegmentLib

The existing mu_msvm uses `BasePciLibCf8` (legacy port I/O via 0xCF8/0xCFC) and has no
`PciSegmentLib` override (defaults to `BasePciSegmentLibPci`, which silently drops the
segment number and delegates to `PciLib`).  Neither supports ECAM or multiple segments.

`PciHostBridgeDxe` links against `PciSegmentLib` for config space access, so we need a
proper multi-segment ECAM implementation.

#### Approach: `BasePciSegmentLibSegmentInfo` + Custom `PciSegmentInfoLib`

MU_BASECORE ships `BasePciSegmentLibSegmentInfo` — a ready-made multi-segment
`PciSegmentLib` that resolves `Segment:Bus:Dev:Func:Reg` to an ECAM MMIO address.
It delegates segment-to-ECAM-base lookup to a separate `PciSegmentInfoLib` that
the platform provides.

**How it works internally:**

1. Caller invokes e.g. `PciSegmentRead32(PCI_SEGMENT_LIB_ADDRESS(Seg, Bus, Dev, Fn, Reg))`
2. `BasePciSegmentLibSegmentInfo` calls `GetPciSegmentInfo(&Count)` to get the
   segment table
3. Searches for the matching `SegmentNumber`, validates `Bus` is in range
4. Computes `MmioAddr = SegmentInfo->BaseAddress + PCI_ECAM_ADDRESS(Bus, Dev, Fn, Reg)`
5. Does `MmioRead32(MmioAddr)` — direct MMIO access to ECAM

The platform's job is implementing one function:

```c
/**
  Return an array of PCI_SEGMENT_INFO describing each PCIe segment.

  @param[out]  Count   Number of entries returned.
  @return  Pointer to a static/allocated array of PCI_SEGMENT_INFO.
**/
PCI_SEGMENT_INFO *
EFIAPI
GetPciSegmentInfo (
  OUT UINTN  *Count
  );
```

Where `PCI_SEGMENT_INFO` is:

```c
typedef struct {
    UINT16  SegmentNumber;     // PCIe segment (matches MCFG)
    UINT64  BaseAddress;       // ECAM base MMIO address for this segment
    UINT8   StartBusNumber;    // Lowest valid bus
    UINT8   EndBusNumber;      // Highest valid bus
} PCI_SEGMENT_INFO;
```

#### New file: `MsvmPkg/Library/PciSegmentInfoLib/PciSegmentInfoLib.c`

This library reads the same `PcdMcfgPtr` / `PcdMcfgSize` PCDs already populated by
PlatformPei and translates each MCFG `McfgSegmentBusRange` entry into a
`PCI_SEGMENT_INFO`.  The data is cached on first call.

```c
STATIC PCI_SEGMENT_INFO  *mSegmentInfo = NULL;
STATIC UINTN              mSegmentCount = 0;

PCI_SEGMENT_INFO *
EFIAPI
GetPciSegmentInfo (
  OUT UINTN  *Count
  )
{
    if (mSegmentInfo != NULL) {
        *Count = mSegmentCount;
        return mSegmentInfo;
    }

    //
    // Parse MCFG table from PCD (same data PlatformPei extracted from config blob)
    //
    UINT64  McfgPtr  = PcdGet64 (PcdMcfgPtr);
    UINT32  McfgSize = PcdGet32 (PcdMcfgSize);

    if (McfgPtr == 0 || McfgSize < sizeof(EFI_ACPI_DESCRIPTION_HEADER)) {
        *Count = 0;
        return NULL;
    }

    EFI_ACPI_DESCRIPTION_HEADER *McfgHdr = (EFI_ACPI_DESCRIPTION_HEADER *)(UINTN) McfgPtr;
    UINT32 DataLen = McfgHdr->Length - sizeof(EFI_ACPI_DESCRIPTION_HEADER) - 8; // 8 = MCFG reserved
    UINT32 EntryCount = DataLen / sizeof(MCFG_SEGMENT_BUS_RANGE);

    mSegmentInfo = AllocateZeroPool (EntryCount * sizeof(PCI_SEGMENT_INFO));
    ASSERT (mSegmentInfo != NULL);

    MCFG_SEGMENT_BUS_RANGE *Entries =
        (MCFG_SEGMENT_BUS_RANGE *)((UINT8 *)McfgHdr
            + sizeof(EFI_ACPI_DESCRIPTION_HEADER) + 8);

    for (UINT32 i = 0; i < EntryCount; i++) {
        mSegmentInfo[i].SegmentNumber  = Entries[i].PciSegmentGroupNumber;
        mSegmentInfo[i].BaseAddress    = Entries[i].BaseAddress;
        mSegmentInfo[i].StartBusNumber = Entries[i].StartBusNumber;
        mSegmentInfo[i].EndBusNumber   = Entries[i].EndBusNumber;
    }

    mSegmentCount = EntryCount;
    *Count = mSegmentCount;
    return mSegmentInfo;
}
```

#### New file: `MsvmPkg/Library/PciSegmentInfoLib/PciSegmentInfoLib.inf`

```ini
[Defines]
  INF_VERSION    = 0x00010005
  BASE_NAME      = PciSegmentInfoLib
  MODULE_TYPE    = BASE
  LIBRARY_CLASS  = PciSegmentInfoLib

[Sources]
  PciSegmentInfoLib.c

[Packages]
  MdePkg/MdePkg.dec
  MsvmPkg/MsvmPkg.dec

[LibraryClasses]
  BaseMemoryLib
  DebugLib
  PcdLib
  MemoryAllocationLib

[Pcd]
  gMsvmPkgTokenSpaceGuid.PcdMcfgPtr
  gMsvmPkgTokenSpaceGuid.PcdMcfgSize
```

#### DSC wiring

```ini
# In [LibraryClasses]:
  PciSegmentLib|MdePkg/Library/PciSegmentLibSegmentInfo/BasePciSegmentLibSegmentInfo.inf
  PciSegmentInfoLib|MsvmPkg/Library/PciSegmentInfoLib/PciSegmentInfoLib.inf

# PciLib still needed by some legacy consumers — keep BasePciLibCf8 for now,
# but it won't be used by the ECAM path (PciSegmentLib bypasses PciLib entirely).
  PciLib|MdePkg/Library/BasePciLibCf8/BasePciLibCf8.inf
```

**Why this is the right approach:**

- **Multi-segment from day one** — no single-segment PCD hack to rip out later.
- **Zero custom config space access code** — `BasePciSegmentLibSegmentInfo` handles
  all the MMIO read/write logic.  Our platform code only maps segment→ECAM base.
- **Data reuse** — reads the same MCFG PCD already populated by PlatformPei, no new
  config blob entries needed (segment info is in MCFG, MMIO apertures in
  `PcieBarApertures`).
- **No `PcdPciExpressBaseAddress` needed** — the per-segment base address comes from
  MCFG, not a global PCD.  We can remove work item 6 from the table.

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
| 5 | **Add PCI library instances to DSC** | Modify: DSC files (PciSegmentLib, PciSegmentInfoLib) | Low |
| 6 | **Implement `PciSegmentInfoLib`** | New: `MsvmPkg/Library/PciSegmentInfoLib/` | Low |
| 7 | **Verify firmware volume size** | Check FDF — adding two DXE drivers increases image size | Low |
| 8 | **VMM test: ePCI NVMe boot** | New: `vmm_tests/.../multiarch/pcie.rs` | Medium |

### openvmm Changes (Config Blob Extension Required)

OpenVMM already provides MCFG + SSDT + ECAM + PCIe root complex support.  The one
required change is emitting a new `PcieBarApertures` config blob entry (type `0x28`)
so mu_msvm can discover per-root-bridge MMIO apertures without parsing AML:

| # | Work Item | Files | Complexity |
|---|-----------|-------|------------|
| 1 | **Add `PcieBarApertures` blob type** | `vm/loader/src/uefi/config.rs` | Low |
| 2 | **Emit entry in UEFI loader** | `openvmm/openvmm_core/src/worker/vm_loaders/uefi.rs` | Low |

The data comes directly from `PcieHostBridge::low_mmio` and `PcieHostBridge::high_mmio`
which are already available at config blob construction time — the same fields used to
build the SSDT `_CRS` descriptors.

---

## Boot Flow with ePCI

```
PEI Phase:
  PlatformPei
    → Config.c parses config blob
    → Extracts MCFG → PcdMcfgPtr / PcdMcfgSize
    → NEW: Extracts PcieBarApertures → PcdPcieBarAperturesPtr / PcdPcieBarAperturesSize
    → Extracts MMIO ranges → PcdLowMmioGap* / PcdHighMmioGap*
    → NEW: Creates MMIO HOBs for ECAM ranges from MCFG
    → NEW: Sets PcdPciExpressBaseAddress from MCFG base (only needed if legacy PciLib consumers remain)

DXE Phase:
  PciHostBridgeDxe starts
    → Calls PciHostBridgeGetRootBridges()  [our new PciHostBridgeLib]
    → Library reads PcdMcfgPtr, parses MCFG entries
    → Library reads PcdPcieBarAperturesPtr for per-bridge MMIO apertures
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

## Phase 6: VMM Integration Test — Alpine Boot over ePCI NVMe

After Phases 1–5 are implemented in mu_msvm, add a VMM test that validates end-to-end
ePCI NVMe boot through UEFI.  This follows the existing petri test patterns in
`vmm_tests/vmm_tests/tests/tests/multiarch/pcie.rs`.

### What the test validates

- mu_msvm UEFI enumerates the PCIe root complex via ECAM (PciHostBridgeDxe + PciBusDxe)
- NvmExpressDxe binds to the ePCI NVMe device (not VPCI)
- UEFI boots Alpine Linux from the ePCI NVMe disk
- Guest OS sees the NVMe device and completes boot to userspace

### Test location

`vmm_tests/vmm_tests/tests/tests/multiarch/pcie.rs` — alongside the existing
`pcie_root_emulation` test.

### Petri framework changes needed

The current `BootDeviceType::Nvme` path routes through VMBus (VPCI NVMe).  Booting
from an NVMe attached to a PCIe root port requires a **new boot device flow**:

1. **New `BootDeviceType` variant** (or manual config via `modify_backend`):
   The boot disk must be attached as a `PcieDeviceConfig` with an `NvmeControllerHandle`
   resource, targeting a named root port — **not** as a `VpciDeviceConfig`.

2. **UEFI boot order**: mu_msvm's `NvmExpressDxe` already binds to any
   `EFI_PCI_IO_PROTOCOL` handle.  The UEFI boot manager should pick up the ePCI NVMe
   disk automatically, but the `enable_vpci_boot` config flag may need to be set (or
   a parallel `enable_pcie_boot` flag added) to ensure the UEFI boot manager includes
   ePCI NVMe in the boot order.

### Sketch of the test

```rust
/// Boot Alpine Linux from an NVMe device on an emulated PCIe root port,
/// validated through mu_msvm UEFI firmware (not VPCI).
#[openvmm_test(
    uefi_x64(vhd(alpine_3_23_x64)),
    uefi_aarch64(vhd(alpine_3_23_aarch64))
)]
async fn pcie_nvme_uefi_boot(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    const ECAM_SIZE: u64 = 256 * 1024 * 1024;      // 256 MB
    const LOW_MMIO_SIZE: u64 = 64 * 1024 * 1024;    // 64 MB
    const HIGH_MMIO_SIZE: u64 = 1024 * 1024 * 1024;  // 1 GB

    let (vm, agent) = config
        // Override boot device to None — we'll attach the boot disk
        // manually as an ePCI NVMe device instead of using the default
        // VPCI/VMBus path.
        .with_boot_device_type(petri::BootDeviceType::None)
        .modify_backend(|b| {
            b.with_custom_config(|c| {
                // Carve out PCIe address space from the MMIO gaps
                let low_mmio_start = c.memory.mmio_gaps[0].start();
                let high_mmio_end = c.memory.mmio_gaps[1].end();
                let pcie_low = MemoryRange::new(
                    low_mmio_start - LOW_MMIO_SIZE..low_mmio_start,
                );
                let pcie_high = MemoryRange::new(
                    high_mmio_end..high_mmio_end + HIGH_MMIO_SIZE,
                );
                let ecam_range = MemoryRange::new(
                    pcie_low.start() - ECAM_SIZE..pcie_low.start(),
                );

                c.memory.pci_ecam_gaps.push(ecam_range);
                c.memory.pci_mmio_gaps.push(pcie_low);
                c.memory.pci_mmio_gaps.push(pcie_high);

                // Single root complex with one port for the NVMe device
                c.pcie_root_complexes.push(PcieRootComplexConfig {
                    index: 0,
                    name: "rc0".into(),
                    segment: 0,
                    start_bus: 0,
                    end_bus: 255,
                    ecam_range,
                    low_mmio: pcie_low,
                    high_mmio: pcie_high,
                    ports: vec![PcieRootPortConfig {
                        name: "rp0".into(),
                        hotplug: false,
                    }],
                });

                // Attach NVMe controller with the boot disk to root port rp0.
                // The boot VHD must be wired in here — this requires the petri
                // framework to expose the resolved boot disk artifact so it can
                // be used as the NVMe namespace backing.
                c.pcie_devices.push(PcieDeviceConfig {
                    port_name: "rp0".into(),
                    resource: NvmeControllerHandle {
                        subsystem_id: guid::guid!("a0b1c2d3-e4f5-6789-abcd-ef0123456789"),
                        max_io_queues: 64,
                        msix_count: 64,
                        namespaces: vec![NamespaceDefinition {
                            nsid: 1,
                            disk: boot_disk_handle,  // resolved from test artifact
                            read_only: false,
                        }],
                        requests: None,
                    }
                    .into_resource(),
                });
            })
        })
        .run()
        .await?;

    // If we get here, UEFI successfully:
    //   1. Enumerated the PCIe root complex via ECAM
    //   2. Found the NVMe device via PciBusDxe
    //   3. Loaded NvmExpressDxe which bound to the ePCI NVMe
    //   4. Booted Alpine from the NVMe disk
    //   5. Pipette agent started in the guest

    // Verify the NVMe device is visible from guest userspace
    let sh = agent.unix_shell();
    let lspci = cmd!(sh, "lspci").read().await?;
    assert!(
        lspci.contains("Non-Volatile memory controller"),
        "NVMe device not visible in guest: {lspci}"
    );

    // Verify the NVMe block device exists
    let nvme_devices = cmd!(sh, "ls /dev/nvme*").read().await?;
    assert!(
        nvme_devices.contains("nvme0"),
        "NVMe block device not found: {nvme_devices}"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
```

### Implementation notes

- **Boot disk plumbing**: The main challenge is getting the test's boot VHD artifact
  wired into the `NvmeControllerHandle` as a namespace disk.  Today, petri's
  `add_boot_disk()` handles this internally for VMBus paths.  For ePCI boot we need
  either:
  - A new `BootDeviceType::PcieNvme` variant that petri handles natively, or
  - Manual disk wiring in `modify_backend` (which requires access to the resolved
    boot disk `File` handle — may need a small petri API addition).
- **UEFI boot order**: mu_msvm needs to include ePCI NVMe devices in the UEFI boot
  manager's device list.  If `NvmExpressDxe` binds to the `EFI_PCI_IO_PROTOCOL`
  produced by `PciBusDxe`, the `EFI_BLOCK_IO_PROTOCOL` chain should cause the boot
  manager to pick it up automatically.  Verify this works without needing an explicit
  boot variable.
- **`enable_vpci_boot`**: This config flag currently tells mu_msvm to include VPCI
  NVMe in the boot order.  For ePCI NVMe boot, either this flag covers both paths
  (since both produce `EFI_PCI_IO_PROTOCOL`) or a separate flag is needed.

### Running the test

```bash
# Run just the ePCI NVMe boot test
cargo xflowey vmm-tests-run \
    --filter "test(pcie_nvme_uefi_boot)" \
    --dir /tmp/vmm-tests-epci

# Run all PCIe tests (includes existing root_emulation + new boot test)
cargo xflowey vmm-tests-run \
    --filter "test(pcie)" \
    --dir /tmp/vmm-tests-epci
```

### Additional test cases (post-initial validation)

| Test | Description |
|------|-------------|
| **`pcie_nvme_uefi_boot`** | Core: Alpine boot from ePCI NVMe through UEFI (above) |
| **`pcie_vpci_coexistence`** | Boot from VPCI SCSI, verify ePCI NVMe also visible as secondary disk |
| **`pcie_multi_segment_enumeration`** | Multiple root complexes with different segments, verify all enumerated |
| **`pcie_nvme_windows_boot`** | Windows boot from ePCI NVMe (heavier, but validates full driver stack) |

---

## Testing Strategy (Summary)

1. **VMM Test — ePCI NVMe Boot** (Phase 6): Automated petri test that boots Alpine
   over ePCI NVMe through mu_msvm UEFI.  This is the primary validation gate.

2. **Coexistence Test**: Boot with VPCI SCSI (default) + ePCI NVMe as secondary.
   Verify both disks are visible in UEFI shell and guest.

3. **Multi-Segment Test**: Configure openvmm with multiple PCIe root complexes
   (different segments).  Verify all are enumerated by mu_msvm.

4. **OS Boot Test**: Windows Server boot from ePCI NVMe — validates full EFI driver
   stack including MSI/MSI-X and DMA.

---

## Summary

The core work is a single new library (`PciHostBridgeLib`) in mu_msvm plus DSC/FDF
wiring changes.  All other components — the generic PCI bus drivers, the VMM-side ECAM
emulation, the ACPI tables, and the consumer drivers — already exist and work.  This is
a well-scoped, medium-complexity change with high confidence of success.

Validation is via a VMM integration test (Phase 6) using the existing petri framework —
boot Alpine Linux from an NVMe device attached to an emulated PCIe root port through
mu_msvm UEFI.  The test follows the same patterns as the existing `pcie_root_emulation`
test in `vmm_tests/vmm_tests/tests/tests/multiarch/pcie.rs`.
