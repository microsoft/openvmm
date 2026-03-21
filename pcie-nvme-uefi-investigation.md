# ePCI NVMe UEFI Boot Investigation

## Status: RESOLVED — PCIe NVMe boot works with ECAM reservation fix

## Problem

Booting an OS from an NVMe device attached to an emulated PCIe root port
does not work.  The UEFI boot manager finds no bootable device.

## What works

All of the following have been confirmed via ECAM-level tracing on the
openvmm side and firmware diagnostics buffer captures.

### Config blob
- openvmm correctly builds the config blob with MCFG (60 bytes, 1 segment)
  and PcieBarApertures (1 entry).
- PlatformPei parses both and stores them in PCDs.
- PciHostBridgeLib reads MCFG, finds 1 root bridge:
  Seg=0, Bus=0..255, EcamBase=0xE4000000,
  LowMmio=0xF4000000+0x4000000, HighMmio=0x1000000000+0x40000000.

### ECAM intercept (openvmm)
- MMIO intercept mapped at 0xE4000000..0xF4000000 (256 MB for 256 buses).
- All ECAM reads/writes route correctly through
  `pcie::root` → `pcie::port` → NVMe device config space.

### PciBusDxe enumeration (374 reads, 97 writes observed)
- Root port at bus 0, dev 0: VID=1414 DID=C030 class=06040000 (PCI bridge).
- Programs secondary bus number = 1 on root port (offset 0x18).
- NVMe at bus 1, dev 0: VID=1414 DID=00A9 class=01080200.
- BAR0 sizing correct (0xFFFF0004 → 64 KB 64-bit MMIO).
- BAR0 programmed to 0xF4000000.
- Command register enabled (0x27), then disabled to 0x10.

### PCI_IO protocol installation
- After the first ConnectAll, 2 ePCI PCI_IO handles exist:
  - `[0]` Seg=0 Bus=0 Dev=0 Func=0  `PcieRoot(0x0)/Pci(0x0,0x0)` (root port)
  - `[1]` Seg=0 Bus=1 Dev=0 Func=0  `PcieRoot(0x0)/Pci(0x0,0x0)/Pci(0x0,0x0)` (NVMe)
- Confirmed via `LocateHandleBuffer(ByProtocol, gEfiPciIoProtocolGuid)`
  diagnostic added to `DeviceBootManagerUnableToBoot`.

### Double ConnectAll
- A second `EfiBootManagerConnectAll()` call was added to
  `DeviceBootManagerUnableToBoot()`.  This ensures NvmExpressDxe gets a
  chance to bind on handles created during the first pass.
- NvmExpressDxe's `Supported()` IS called during both ConnectAll passes.

## What fails

**NvmExpressDxe reads class code `[00,00,00]` from the NVMe PCI_IO handle.**

`Supported()` IS called on the correct NVMe PCI_IO handle (Bus=1, Dev=0).
It passes both DevicePath and PCI_IO OpenProtocol checks.  But the class
code read via `PciIo->Pci.Read(offset=0x09, count=3)` returns `[00,00,00]`
instead of `[02,08,01]` (NVMe).  Supported() returns `Unsupported`.

The root port handle (Bus=0, Dev=0) similarly reads `[02,FB,FF]` instead
of the expected `[00,04,06]` (PCI bridge).

## Root cause

**The UEFI synthetic video driver maps VRAM at GPA 0xE4000000, overlapping
the ECAM range.**

The firmware receives the ECAM ranges via MCFG and PcieBarApertures in the
config blob.  PlatformPei calls `HobAddMmioRange(EcamBase, EcamSize)` to
declare the ECAM range as `EFI_RESOURCE_MEMORY_MAPPED_IO`.  Despite this,
the synthvid driver later maps VRAM at 0xE4000000 — either by bypassing
the UEFI memory allocator or because the GCD doesn't properly prevent
allocation in MMIO-reserved regions.

When the guest sends a `VramLocation` message with GPA 0xE4000000, openvmm
maps host-backed framebuffer memory at that address.  This overrides the
ECAM MMIO intercept — subsequent reads at 0xE4000000 return framebuffer
data instead of triggering config space access.

Confirmed via region manager tracing:
```
NEW REGION OVERLAPS ECAM RANGE 0xE4000000..0xF4000000!
  range=0xe4000000-0xe4800000 name="framebuffer" priority=0x0
```

**Confirmed: guest page tables are correct, EPT is the problem.**

Page table walk from inside the guest:
```
VA=0xE4000000 CR3=0x4001000 PML4E=0x4002023 PDPTE=0xE3201023 PDE=0xE40000E3
-> 2MB large page, PA=0xE4000000 (identity mapped, Present+RW+Accessed)
```

Direct read at the identity-mapped PA returns `0xFFFB0200` (garbage), not
the expected config space data.  Since the guest page tables correctly
identity-map VA→PA, the issue must be in the hypervisor's second-level
address translation (EPT/NPT).

The hypervisor's EPT maps PA 0xE4000000 to host physical memory (which
contains firmware code/data) instead of leaving it unmapped to trigger an
EPT violation → MMIO intercept.  This mapping changes between PciBusDxe
enumeration (~0.12s, when ECAM reads work) and BDS (~0.22s, when they
return garbage).

**This is an openvmm hypervisor memory management bug.**  The ECAM range
is in `pci_ecam_gaps` and should never have host RAM backing in the EPT.
Something in openvmm's memory manager is mapping host memory for the ECAM
range after initial boot.

## What is NOT the cause

- **Double ConnectAll.**  Early investigation hypothesized that a second
  `EfiBootManagerConnectAll()` was needed.  Testing confirmed that
  `ConnectController` with `recursive=TRUE` connects NvmExpressDxe to the
  NVMe handle during a single ConnectAll pass.  No firmware changes to
  DeviceBootManagerLib are needed.

- **NvmExpressDxe DevicePath leak.**  NvmExpressDxe Start() has an error
  path that doesn't close DevicePath.  This bug exists but was never
  triggered — Start() is never called when the real problem (framebuffer
  overlap) prevents Supported() from seeing correct class codes.

- **Extended config space (offset 0x100).**  The warning
  `LocatePciExpressCapabilityRegBlock: [00|00|00] failed to access config
  space at offset 0x100` only affects optional capabilities (ARI, SR-IOV).
  `RegisterPciDevice()` installs PCI_IO unconditionally.

- **PCI_IO not installed.**  PCI_IO IS installed (2 ePCI handles confirmed
  after ConnectAll: root port + NVMe, with correct device paths and BDF).

- **DevicePath Access Denied on NVMe handle.**  Earlier analysis found
  `Access Denied` on handle `E34E9398`, but that is a VMBus handle, NOT
  the NVMe PCI_IO handle.  The NVMe handle (`E34C6B98`) passes DevicePath
  and PCI_IO opens fine — it fails at the class code check.

- **NvmExpressDxe Start() DevicePath leak.**  Start() is never called on
  the NVMe handle because Supported() returns Unsupported (class code
  mismatch).  The leak fix is still correct but not the blocking issue.

## Next steps

### Immediate: Fix UEFI ECAM MMIO reservation (item 2)

VideoDxe (synthvid) allocates VRAM via:
```c
gDS->AllocateMemorySpace(EfiGcdAllocateAnySearchBottomUp,
                         EfiGcdMemoryTypeMemoryMappedIo, ...)
```
This searches bottom-up for free MMIO space.  PlatformPei's `HobAddMmioRange`
declares the ECAM range as `EfiGcdMemoryTypeMemoryMappedIo`, making it
AVAILABLE for allocation.  VideoDxe picks it because it's the lowest free
MMIO region.

**Fix:** In PlatformPei (Platform.c), after calling `HobAddMmioRange` for
ECAM and PCI BAR aperture ranges, those ranges must also be ALLOCATED
(reserved) so that `AllocateMemorySpace` won't hand them out.  This can be
done in DXE by calling:
```c
gDS->AllocateMemorySpace(EfiGcdAllocateAddress,
                         EfiGcdMemoryTypeMemoryMappedIo,
                         0, EcamSize, &EcamBase, gImageHandle, NULL);
```
for each ECAM and PCI BAR MMIO range.  PciHostBridgeDxe already does this
for BAR apertures (in `NotifyPhase`), but may not do it for the ECAM range
itself.  The ECAM range reservation should happen early in DXE — before
VideoDxe runs.

### Later: openvmm should reject framebuffer overlapping ECAM (item 1)

openvmm should validate that the guest's `VramLocation` GPA does not overlap
any registered MMIO intercept regions.  Currently `framebuffer.map(address)`
unconditionally maps.  Add a validation check in the video device or region
manager.

### Clean up

Remove temporary debug changes in openvmm and mu_msvm once the fix is
verified.

## Current mu_msvm changes

1. **DeviceBootManagerLib.c**
   - Second `EfiBootManagerConnectAll()` call.
   - `WriteBiosDevice(BiosConfigProcessEfiDiagnostics)` flush points.
   - PCI_IO handle enumeration diagnostic (logs handle address, BDF, path).

2. **NvmExpress.c**
   - DEBUG_ERROR tracing at Supported() entry, early-return paths, and Done.
   - DEBUG_ERROR tracing at Start() entry.
   - Fixed DevicePath leak on PCI_IO open failure path in Start().

3. **PciHostBridgeLib.c**
   - Promoted key messages to DEBUG_ERROR.

## Current openvmm changes (temporary debug)

1. **pcie::root** — Per-access ECAM read/write tracing (offset + value).
2. **firmware_uefi lib.rs** — `PROCESS_EFI_DIAGNOSTICS` uses
   `allow_reprocess = true` with `WATCHDOG_LOGS_PER_PERIOD` limit.
3. **uefi.rs** — Warn-level tracing for config blob MCFG/PcieBarApertures.
4. **pcie.rs test** — `enable_vpci_boot = false`.

## Test invocation

```bash
cargo xflowey vmm-tests-run \
  --filter "test(pcie_nvme_boot)" \
  --custom-uefi-firmware /mnt/d/ai/jolteon/mu_msvm/Build/MsvmX64/DEBUG_VS2022/FV/MSVM.fd \
  --dir /home/coo/ai/jolteon/openvmm/vmm_test_results
```

`--custom-uefi-firmware` is required — stock release firmware lacks the PCI
bus drivers.
