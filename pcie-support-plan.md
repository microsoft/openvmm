# Plan: PCIe (Emulated PCI) Support in mu_msvm

## Executive Summary

Today mu_msvm discovers PCI devices exclusively through **VPCI** (Virtual PCI over VMBus).
OpenVMM already has full PCIe support — Gen2 VMs expose PCIe devices via ECAM
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
| **NvmExpressDxe** | Binds to `EFI_PCI_IO_PROTOCOL` | Works with any provider — VPCI or PCIe |

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
   - `Supports` = 0 (no legacy PCI attributes needed)
   - `Attributes` = 0
   - `DmaAbove4G` = `TRUE` (VMs typically have >4 GB RAM)
   - `NoExtendedConfigSpace` = `FALSE` (PCIe requires 4096-byte config space via ECAM)
   - `ResourceAssigned` = `FALSE` (let PciBusDxe handle BAR allocation)
   - `Bus` aperture = `{start_bus, end_bus, 0}` (Translation = 0)
   - `Mem` aperture from `PcieBarApertures` entry `LowMmio` (per-bridge low MMIO)
   - `MemAbove4G` aperture from `PcieBarApertures` entry `HighMmio` (per-bridge high MMIO)
   - `PMem`/`PMemAbove4G` = empty (`{MAX_UINT64, 0}`) — no prefetchable distinction needed
   - `Io` = empty (`{MAX_UINT64, 0}`) — Gen2 VMs have no legacy I/O port space for PCIe
   - All `Translation` fields = 0 (identity mapping)
   - `AllocationAttributes` = `EFI_PCI_HOST_BRIDGE_COMBINE_MEM_PMEM` | `EFI_PCI_HOST_BRIDGE_MEM64_DECODE`
   - `DevicePath` = ACPI device path with HID `EISA_PNP_ID(0x0A08)`, UID = segment

   **Device path construction** (must be per-bridge, heap-allocated):
   ```c
   #pragma pack(1)
   typedef struct {
       ACPI_HID_DEVICE_PATH     AcpiDevicePath;
       EFI_DEVICE_PATH_PROTOCOL EndDevicePath;
   } EFI_PCI_ROOT_BRIDGE_DEVICE_PATH;
   #pragma pack()

   STATIC
   EFI_DEVICE_PATH_PROTOCOL *
   CreateRootBridgeDevicePath (
       UINT32  Uid
       )
   {
       EFI_PCI_ROOT_BRIDGE_DEVICE_PATH *DevicePath;

       DevicePath = AllocateCopyPool (
                        sizeof (EFI_PCI_ROOT_BRIDGE_DEVICE_PATH),
                        &(EFI_PCI_ROOT_BRIDGE_DEVICE_PATH) {
                            .AcpiDevicePath = {
                                .Header = {
                                    .Type    = ACPI_DEVICE_PATH,
                                    .SubType = ACPI_DP,
                                    .Length  = { sizeof (ACPI_HID_DEVICE_PATH), 0 },
                                },
                                .HID = EISA_PNP_ID (0x0A08),
                                .UID = Uid,
                            },
                            .EndDevicePath = {
                                .Type    = END_DEVICE_PATH_TYPE,
                                .SubType = END_ENTIRE_DEVICE_PATH_SUBTYPE,
                                .Length  = { sizeof (EFI_DEVICE_PATH_PROTOCOL), 0 },
                            },
                        }
                    );
       return (EFI_DEVICE_PATH_PROTOCOL *)DevicePath;
   }
   ```

   **Empty aperture convention**: MU_BASECORE uses `Base > Limit` to indicate an
   unused aperture.  Use `{MAX_UINT64, 0, 0}` (Base=MAX_UINT64, Limit=0) for
   `Io`, `PMem`, and `PMemAbove4G`.

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

**Registration in existing enums and tables** (required for the config blob parser to
reach the new type):

- **`BiosInterface.h`**: Add `UefiConfigPcieBarApertures = 0x28` to the config
  structure type enum.
- **`config.rs`**: Add `PcieBarApertures = 0x28` to the `BlobStructureType` enum.
- **`Config.c` — `StructureLengthTable[]`** (around line 1050): Extend the array to
  index `0x28`.  Currently the array only goes up to index `0x27` (`Iort`).  Without
  this extension, the bounds check at line 1073
  (`Header->Type >= sizeof(table)/sizeof(table[0])`) silently skips type `0x28`,
  and the parsing `switch` is never reached.  Add:
  ```c
  0, // UefiConfigPcieBarApertures — variable length, validated in case handler
  ```
- **`Config.c` — `PrintConfigStructure` debug switch** (around line 779): Add
  `case UefiConfigPcieBarApertures:` with a debug print for the entry count.

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
  │    Length = 8 + N * 40                    │
  ├──────────────────────────────────────────┤
  │  PCIE_BAR_APERTURE_ENTRY[0]  (40 bytes)  │
  ├──────────────────────────────────────────┤
  │  PCIE_BAR_APERTURE_ENTRY[1]  (40 bytes)  │
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
// 40 bytes, all fields naturally aligned (no #pragma pack needed).
//
// Layout:  [0:2] Segment, [2] StartBus, [3] EndBus, [4:8] Reserved,
//          [8:16] LowMmioBase, [16:24] LowMmioLength,
//          [24:32] HighMmioBase, [32:40] HighMmioLength
//
typedef struct _PCIE_BAR_APERTURE_ENTRY {
    UINT16  Segment;            // PCI segment number (matches MCFG)
    UINT8   StartBus;           // Lowest valid bus number
    UINT8   EndBus;             // Highest valid bus number
    UINT32  Reserved;           // Padding to 8-byte boundary; must be 0
    UINT64  LowMmioBase;        // Low MMIO window base address (below 4 GB)
    UINT64  LowMmioLength;      // Low MMIO window length in bytes
    UINT64  HighMmioBase;       // High MMIO window base address (above 4 GB)
    UINT64  HighMmioLength;     // High MMIO window length in bytes
} PCIE_BAR_APERTURE_ENTRY;      // sizeof = 40

typedef struct _UEFI_CONFIG_PCIE_BAR_APERTURES {
    UEFI_CONFIG_HEADER          Header;
    PCIE_BAR_APERTURE_ENTRY     Entries[];   // Variable-length array
} UEFI_CONFIG_PCIE_BAR_APERTURES;
```

Entry count is derived: `N = (Header.Length - sizeof(UEFI_CONFIG_HEADER)) / sizeof(PCIE_BAR_APERTURE_ENTRY)`

**MsvmPkg.dec PCD declarations** (assign token IDs following the highest existing
ID `0x606F`):

```ini
[PcdsDynamic]
  ## Pointer to PcieBarApertures data from config blob (array of PCIE_BAR_APERTURE_ENTRY)
  gMsvmPkgTokenSpaceGuid.PcdPcieBarAperturesPtr|0|UINT64|0x6070
  ## Size in bytes of PcieBarApertures data
  gMsvmPkgTokenSpaceGuid.PcdPcieBarAperturesSize|0|UINT32|0x6071
```

**DSC default values** — add to both `MsvmPkgX64.dsc` and `MsvmPkgAARCH64.dsc` in the
`[PcdsDynamicDefault]` section:

```ini
  gMsvmPkgTokenSpaceGuid.PcdPcieBarAperturesPtr|0
  gMsvmPkgTokenSpaceGuid.PcdPcieBarAperturesSize|0
```

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

DEBUG((DEBUG_INFO, "PciHostBridgeLib: %u MCFG entries, %u aperture entries\n",
       McfgEntryCount, ApertureCount));

// For each MCFG segment, find matching aperture entry by Segment number
// and populate Mem / MemAbove4G from LowMmio / HighMmio fields.
// After populating each PCI_ROOT_BRIDGE:
DEBUG((DEBUG_INFO, "  Bridge[%u]: Seg=%u Bus=%u..%u LowMmio=%016lx+%016lx HighMmio=%016lx+%016lx\n",
       i, Segment, StartBus, EndBus,
       LowMmioBase, LowMmioLength, HighMmioBase, HighMmioLength));
```

**Design rationale:**

- **Flat array, no separate count field** — matches `UefiConfigMmioRanges` precedent;
  count derived from `Header.Length`.
- **40-byte entry with explicit `Reserved` padding** — keeps all `UINT64` fields at
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
# In [LibraryClasses.common.DXE_DRIVER] section (around line 252):
# These libraries are only consumed by DXE drivers.  PciSegmentInfoLib
# uses PcdGet64/PcdGet32 which require the DXE PCD protocol.
  PciHostBridgeLib|MsvmPkg/Library/PciHostBridgeLib/PciHostBridgeLib.inf
  PciSegmentLib|MdePkg/Library/PciSegmentLibSegmentInfo/BasePciSegmentLibSegmentInfo.inf
  PciSegmentInfoLib|MsvmPkg/Library/PciSegmentInfoLib/PciSegmentInfoLib.inf
  IoMmuLib|MdeModulePkg/Library/IoMmuLibNull/IoMmuLibNull.inf

# In [Components] section, add:
  MdeModulePkg/Bus/Pci/PciHostBridgeDxe/PciHostBridgeDxe.inf
  MdeModulePkg/Bus/Pci/PciBusDxe/PciBusDxe.inf
```

**Note:** Both `PciHostBridgeDxe` and `PciBusDxe` depend on `IoMmuLib` (MU_CHANGE).
mu_msvm has no IOMMU, so wire the null stub.  `DxeMemoryProtectionHobLib` is already
wired as a null instance in the existing DSC.

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
PlatformPei and translates each MCFG allocation entry into a
`PCI_SEGMENT_INFO`.  The data is cached on first call.

The MCFG allocation entry type has the unwieldy UEFI name
`EFI_ACPI_MEMORY_MAPPED_ENHANCED_CONFIGURATION_SPACE_BASE_ADDRESS_ALLOCATION_STRUCTURE`
(from `IndustryStandard/MemoryMappedConfigurationSpaceAccessTable.h`).  We typedef it
locally for readability:

```c
#include <IndustryStandard/MemoryMappedConfigurationSpaceAccessTable.h>

//
// Local typedef for readability.  The canonical UEFI name is unwieldy.
//
typedef EFI_ACPI_MEMORY_MAPPED_ENHANCED_CONFIGURATION_SPACE_BASE_ADDRESS_ALLOCATION_STRUCTURE
    MCFG_ALLOCATION_ENTRY;

// sizeof(MCFG_ALLOCATION_ENTRY) == 16:
//   UINT64  BaseAddress
//   UINT16  PciSegmentGroupNumber
//   UINT8   StartBusNumber
//   UINT8   EndBusNumber
//   UINT32  Reserved
```

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
    //
    // MCFG layout: ACPI header + 8 bytes reserved + array of allocation entries.
    // The 8-byte reserved field is defined by the ACPI MCFG specification.
    //
    UINT32 McfgReservedSize = 8;
    UINT32 DataLen = McfgHdr->Length - sizeof(EFI_ACPI_DESCRIPTION_HEADER) - McfgReservedSize;
    UINT32 EntryCount = DataLen / sizeof(MCFG_ALLOCATION_ENTRY);

    DEBUG((DEBUG_INFO, "PciSegmentInfoLib: %u segments from MCFG\n", EntryCount));

    mSegmentInfo = AllocateZeroPool (EntryCount * sizeof(PCI_SEGMENT_INFO));
    ASSERT (mSegmentInfo != NULL);

    MCFG_ALLOCATION_ENTRY *Entries =
        (MCFG_ALLOCATION_ENTRY *)((UINT8 *)McfgHdr
            + sizeof(EFI_ACPI_DESCRIPTION_HEADER) + McfgReservedSize);

    for (UINT32 i = 0; i < EntryCount; i++) {
        mSegmentInfo[i].SegmentNumber  = Entries[i].PciSegmentGroupNumber;
        mSegmentInfo[i].BaseAddress    = Entries[i].BaseAddress;
        mSegmentInfo[i].StartBusNumber = Entries[i].StartBusNumber;
        mSegmentInfo[i].EndBusNumber   = Entries[i].EndBusNumber;

        DEBUG((DEBUG_INFO, "  Segment[%u]: Seg=%u ECAM=%016lx Bus=%u..%u\n",
               i, Entries[i].PciSegmentGroupNumber, Entries[i].BaseAddress,
               Entries[i].StartBusNumber, Entries[i].EndBusNumber));
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
  MODULE_TYPE    = DXE_DRIVER
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
# In [LibraryClasses.common.DXE_DRIVER] (same section as Phase 2):
  PciSegmentLib|MdePkg/Library/PciSegmentLibSegmentInfo/BasePciSegmentLibSegmentInfo.inf
  PciSegmentInfoLib|MsvmPkg/Library/PciSegmentInfoLib/PciSegmentInfoLib.inf

# PciLib: needed by some legacy consumers that haven't been ported to PciSegmentLib.
# On X64, keep BasePciLibCf8 — it uses legacy I/O ports 0xCF8/0xCFC.
# On AARCH64, use BasePciExpressLib or a null stub instead — CF8/CFC I/O ports
# don't exist on ARM64 and BasePciLibCf8 would access non-existent I/O space.
# Audit all PciLib callers in mu_msvm to verify none are in the PCIe path.
  PciLib|MdePkg/Library/BasePciLibCf8/BasePciLibCf8.inf    # X64 only
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
For PCIe we need to ensure:

1. **ECAM range is mapped as UC (uncacheable) MMIO** in the GCD.
   - PlatformPei should add the ECAM range from MCFG as an MMIO HOB.
   - Current code in `Config.c` parses MCFG but doesn't create HOBs for the ECAM range.
   - **Add:** After parsing MCFG, call `HobAddMmioRange()` for each ECAM segment's
     address range to ensure it's in the GCD as MMIO.  `BasePciSegmentLibSegmentInfo`
     does raw `MmioRead32()` at ECAM addresses — if the region isn't in the GCD,
     DxeCore's page tables won't cover it, causing a page fault.

   **Concrete implementation** (in `Platform.c`, around line 705, after existing
   `HobAddMmioRange` calls):

   ```c
   //
   // Register ECAM MMIO ranges for each MCFG segment.
   // The MCFG BaseAddress is bus-0-relative: it represents the ECAM base
   // as if bus 0 were the first bus.  For segments with StartBus > 0
   // the actual MMIO region starts at BaseAddress + StartBus * 256 * 4096.
   //
   if (McfgPtr != 0 && McfgSize >= sizeof(EFI_ACPI_DESCRIPTION_HEADER)) {
       EFI_ACPI_DESCRIPTION_HEADER *McfgHdr =
           (EFI_ACPI_DESCRIPTION_HEADER *)(UINTN) McfgPtr;
       UINT32 McfgDataLen = McfgHdr->Length
           - sizeof(EFI_ACPI_DESCRIPTION_HEADER) - 8; // 8 = MCFG reserved
       UINT32 NumEntries = McfgDataLen / sizeof(MCFG_ALLOCATION_ENTRY);
       MCFG_ALLOCATION_ENTRY *Entries =
           (MCFG_ALLOCATION_ENTRY *)((UINT8 *)McfgHdr
               + sizeof(EFI_ACPI_DESCRIPTION_HEADER) + 8);

       for (UINT32 i = 0; i < NumEntries; i++) {
           UINT64 EcamBase = Entries[i].BaseAddress
               + (UINT64)Entries[i].StartBusNumber * 256 * 4096;
           UINT64 EcamSize =
               (UINT64)(Entries[i].EndBusNumber - Entries[i].StartBusNumber + 1)
               * 256 * 4096;
           HobAddMmioRange(EcamBase, EcamSize);
       }
   }
   ```

2. **BAR MMIO ranges** are handled by `PciHostBridgeDxe` automatically.  During
   initialization, it calls `gDS->AddMemorySpace(EfiGcdMemoryTypeMemoryMappedIo, ...)`
   for each `Mem` and `MemAbove4G` aperture from the `PCI_ROOT_BRIDGE` array.  No
   additional PEI HOBs are needed for BAR windows.

   **Note:** The BAR windows are carved *outside* the existing MMIO gaps (ECAM + low
   BAR windows grow downward from the gap start, high BAR windows grow upward past the
   gap end).  The `UefiConfigMmioRanges` blob only carries the original 2 gaps.  This
   is fine because `PciHostBridgeDxe` registers the BAR apertures in the GCD at DXE
   time — they don't need to be in the PEI memory map.

### Phase 5: VPCI + PCIe Coexistence

Both paths must coexist.  A VM may have some devices on VPCI (e.g., passed-through
devices) and others on PCIe (e.g., emulated NVMe).  This works naturally because:

- `VpcivscDxe` binds to VMBus channels → produces `EFI_PCI_IO_PROTOCOL` for VPCI devices
- `PciBusDxe` scans ECAM config space → produces `EFI_PCI_IO_PROTOCOL` for PCIe devices
- Consumer drivers (`NvmExpressDxe`, etc.) bind to `EFI_PCI_IO_PROTOCOL` regardless of source
- No changes needed to VpcivscDxe or consumer drivers

**Potential conflict:** Both paths could expose the same device.  The VMM should ensure
a device is offered through exactly one path (VPCI or PCIe), never both.

---

## Detailed Work Items

### mu_msvm Changes

| # | Work Item | Files | Complexity |
|---|-----------|-------|------------|
| 1 | **Implement `PciHostBridgeLib`** | New: `MsvmPkg/Library/PciHostBridgeLib/` | Medium |
| 2 | **Register ECAM MMIO in PlatformPei** | Modify: `MsvmPkg/PlatformPei/Config.c` | Low |
| 3 | **Add PCI drivers + IoMmuLib to DSC** | Modify: `MsvmPkg/MsvmPkgX64.dsc`, `MsvmPkgAARCH64.dsc` | Low |
| 4 | **Add PCI drivers to FDF** | Modify: `MsvmPkg/MsvmPkgX64.fdf`, `MsvmPkgAARCH64.fdf` | Low |
| 5 | **Add PCI library instances to DSC** | Modify: DSC files (PciSegmentLib, PciSegmentInfoLib, IoMmuLib) | Low |
| 6 | **Implement `PciSegmentInfoLib`** | New: `MsvmPkg/Library/PciSegmentInfoLib/` | Low |
| 7 | **Verify firmware volume size** | Check FDF — adding two DXE drivers increases image size | Low |
| 8 | **Add `PcieBarApertures` structs to BiosInterface.h** | Modify: `MsvmPkg/Include/BiosInterface.h` | Low |
| 9 | **Add `PcieBarApertures` PCDs to MsvmPkg.dec** | Modify: `MsvmPkg/MsvmPkg.dec` | Low |
| 10 | **Parse `PcieBarApertures` in PlatformPei** | Modify: `MsvmPkg/PlatformPei/Config.c` | Low |
| 11 | **VMM test: PCIe NVMe boot** | See Phase 6 | Medium |

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

## Boot Flow with PCIe

```
PEI Phase:
  PlatformPei
    → Config.c parses config blob
    → Extracts MCFG → PcdMcfgPtr / PcdMcfgSize
    → NEW: Extracts PcieBarApertures → PcdPcieBarAperturesPtr / PcdPcieBarAperturesSize
    → Extracts MMIO ranges → PcdLowMmioGap* / PcdHighMmioGap*
    → NEW: Creates MMIO HOBs for ECAM ranges from MCFG

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
    → Works identically regardless of PCIe or VPCI source

  VpcivscDxe (unchanged)
    → Continues to discover VPCI devices over VMBus
    → Produces EFI_PCI_IO_PROTOCOL for those devices
```

---

## Risks and Considerations

1. **MSI/MSI-X Interrupt Routing**: In PCIe, MSI address/data are programmed directly
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
   should NOT be used for PCIe config access — ensure it's only used by legacy code paths.

---

## Development Debugging

During development, firmware debug output is the primary diagnostic tool.  mu_msvm's
Advanced Logger feeds `DEBUG((...))` output to the VMM's diagnostics service, which
emits tracing events visible in the test harness.

**Log level filtering:** The default `EfiDiagnosticsLogLevelType::Default` filters out
`DEBUG_INFO` messages.  During development, override this in tests:

```rust
c.efi_diagnostics_log_level = EfiDiagnosticsLogLevelType::Full;
```

No firmware rebuild is needed — `PcdDebugPrintErrorLevel` (`0x804FEF4B`) already
includes `DEBUG_INFO` on the firmware side.  The filter is VMM-side only.

**What to expect at `DEBUG_INFO` level:** `PciHostBridgeDxe` and `PciBusDxe` are
verbose: they log root bridge apertures, bus scanning progress, BAR resource allocation,
and device discovery.  The new `PciHostBridgeLib` and `PciSegmentInfoLib` code should
add `DEBUG_INFO` statements for:

- Number of MCFG entries found
- Per-bridge: segment number, bus range, ECAM base
- Per-bridge: low MMIO base+length, high MMIO base+length
- Number of `PcieBarApertures` entries and segment matching results

**Hard debugging:** For difficult-to-diagnose issues, build mu_msvm with
`DEBUGLIB_SERIAL=1` and `DEBUG_NOISY=1` for verbose COM1 serial output.  This bypasses
the Advanced Logger entirely and writes directly to the serial port.

---

## Phase 6: VMM Integration Tests

### Testing philosophy

The existing `pcie_root_emulation` test already validates that the VMM's PCIe ECAM
emulation works — guest OSes (Linux, Windows) can enumerate root ports after boot.
What's **new and untested** is mu_msvm's firmware-level PCI enumeration: the
PciHostBridgeDxe → PciBusDxe → NvmExpressDxe chain running inside UEFI.

The right approach is a single, high-value integration test: **boot an OS entirely
from an PCIe NVMe device**.  If the OS boots, it proves the full chain:
`PciHostBridgeDxe` → `PciBusDxe` → `NvmExpressDxe` → UEFI boot manager → OS boot.
This validates every layer of the mu_msvm changes in one shot.

> **Future improvement:** Once `guest_test_uefi` supports runtime test selection
> (the binary currently runs all tests unconditionally — see the TODO in
> `guest_test_uefi/src/uefi/tests/mod.rs`), a lightweight UEFI-app check for
> `EFI_PCI_ROOT_BRIDGE_IO_PROTOCOL` handles would be a good addition for
> faster iteration during development.

### PCIe NVMe boot test

This test validates the full chain: `PciHostBridgeDxe` → `PciBusDxe` → `NvmExpressDxe`
→ UEFI boot manager → OS boot.  If the OS boots from an PCIe NVMe device, every
layer of the mu_msvm PCI enumeration code is proven correct.

This requires routing the boot disk through `PcieDeviceConfig` instead of
`VpciDeviceConfig`.  The `StorageBuilder` already has the plumbing for this via
`DiskLocation::Nvme(nsid, Some(port_name))`, which populates
`pcie_nvme_controllers` → `config.pcie_devices`.

**Petri change needed:** Add `BootDeviceType::PcieNvme` that:
1. Creates a `PcieRootComplexConfig` with a single root port
2. Routes the boot disk through `DiskLocation::Nvme(1, Some("rp0"))` instead of
   `DiskLocation::Nvme(1, None)`
3. The `StorageBuilder` handles the rest — it already knows how to build
   `NvmeControllerHandle` and push to `config.pcie_devices`

```rust
/// Boot an OS entirely from an NVMe device on an emulated PCIe root port.
/// No VPCI boot device — mu_msvm must enumerate the PCIe NVMe via ECAM,
/// bind NvmExpressDxe, and boot from it.
#[openvmm_test(
    uefi_x64(vhd(ubuntu_2404_server_x64)),
    uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
)]
async fn pcie_nvme_boot(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    const ECAM_SIZE: u64 = 256 * 1024 * 1024;
    const LOW_MMIO_SIZE: u64 = 64 * 1024 * 1024;
    const HIGH_MMIO_SIZE: u64 = 1024 * 1024 * 1024;

    let os_flavor = config.os_flavor();
    let (vm, agent) = config
        .with_boot_device_type(BootDeviceType::PcieNvme)
        .modify_backend(|b| {
            b.with_custom_config(|c| {
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
                // Boot disk is automatically attached to "rp0" by
                // BootDeviceType::PcieNvme via StorageBuilder.
            })
        })
        .run()
        .await?;

    // If we get here, mu_msvm successfully:
    //   1. Enumerated the PCIe root complex via ECAM
    //   2. PciBusDxe found the NVMe device
    //   3. NvmExpressDxe bound to the PCIe NVMe
    //   4. UEFI boot manager booted the OS from PCIe NVMe
    //   5. Pipette agent started in guest

    // Verify the NVMe device is visible from guest
    let guest_devices = parse_guest_pci_devices(os_flavor, &agent).await?;
    let nvme_count = guest_devices
        .iter()
        .filter(|d| d.class_code == 0x010802)
        .count();
    assert!(nvme_count >= 1, "NVMe controller not visible in guest");

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
```

### Petri changes for `BootDeviceType::PcieNvme`

The infrastructure for PCIe NVMe routing already exists in `StorageBuilder`:

```rust
// In openvmm_entry/src/storage_builder.rs, the DiskLocation enum already has:
DiskLocation::Nvme(nsid, pcie_port) => {
    match (vtl, pcie_port) {
        (DeviceVtl::Vtl0, None) => /* VPCI path */,
        (DeviceVtl::Vtl0, Some(port)) => /* PCIe path — already implemented! */,
    }
}
```

The petri change is adding a new variant to `BootDeviceType` that passes
`Some("rp0")` as the PCIe port name instead of `None`:

```rust
// In petri/src/vm/mod.rs:
pub enum BootDeviceType {
    // ...existing variants...
    PcieNvme,  // Boot from NVMe attached to PCIe root port "rp0"
}

// In the boot device routing logic:
BootDeviceType::PcieNvme => {
    // Route boot disk to PCIe NVMe on port "rp0"
    self.add_storage_disk(
        DiskLocation::Nvme(1, Some("rp0".into())),
        boot_disk,
        false, // not read-only
    )
}
```

### Running the tests

```bash
# Run the PCIe NVMe boot test
cargo xflowey vmm-tests-run \
    --filter "test(pcie_nvme_boot)" \
    --dir /tmp/vmm-tests-pcie

# Run all PCIe tests
cargo xflowey vmm-tests-run \
    --filter "test(pcie)" \
    --dir /tmp/vmm-tests-pcie
```

### Test progression

| Order | Test | What it proves | Requires |
|-------|------|---------------|----------|
| 1 | **`pcie_nvme_boot`** | Full boot chain: ECAM → PciBusDxe → NvmExpressDxe → OS boot | `BootDeviceType::PcieNvme` in petri |
| 2 | **`pcie_multi_segment`** (future) | Multiple root complexes with different segments | No new infra |
| 3 | **`pcie_nvme_windows_boot`** (future) | Windows boot from PCIe NVMe | Windows VHD artifact |

---

## Testing Strategy (Summary)

1. **PCIe NVMe boot** (`pcie_nvme_boot`): Boot an OS from an NVMe device on a PCIe
   root port.  Validates the full chain: `PciHostBridgeDxe` → `PciBusDxe` →
   `NvmExpressDxe` → OS boot.  Requires adding `BootDeviceType::PcieNvme`
   to petri (small change — the `StorageBuilder` already has the PCIe NVMe routing).
   This is the primary validation — if the OS boots, every layer works.

2. **Multi-segment** (future): Multiple root complexes, different segments.

3. **Windows boot** (future): Windows from PCIe NVMe — validates full driver stack.

> **Future:** A `guest_test_uefi` PCI root bridge check would be valuable for fast
> iteration during development, once `guest_test_uefi` supports runtime test selection.

---

## Summary

The core work is a single new library (`PciHostBridgeLib`) in mu_msvm plus DSC/FDF
wiring changes and a small openvmm config blob extension (`PcieBarApertures`).  All
other components — the generic PCI bus drivers, the VMM-side ECAM emulation, the ACPI
tables, and the consumer drivers — already exist and work.  This is a well-scoped,
medium-complexity change with high confidence of success.

Validation uses a single high-value integration test: an PCIe NVMe boot test that
proves the full firmware → OS boot chain (`PciHostBridgeDxe` → `PciBusDxe` →
`NvmExpressDxe` → OS boot).  The VMM-side ECAM emulation is already proven by the
existing `pcie_root_emulation` test.
