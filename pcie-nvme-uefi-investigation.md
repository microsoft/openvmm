# ePCI NVMe UEFI Boot Investigation

## Status: PCI enumeration works, NvmExpressDxe binding fails

The entire PCI stack works correctly — PciBusDxe fully enumerates devices
and allocates BAR resources.  The failure is that `NvmExpressDxe` is never
connected to the NVMe device's `EFI_PCI_IO_PROTOCOL` handle during
`ConnectAll`.

## What works (confirmed via openvmm + firmware debug tracing)

### Firmware side (mu_msvm)
- `PciHostBridgeLib` reads MCFG (ptr=0x709218, size=60) and
  `PcieBarApertures` (1 entry) from config blob PCDs
- Returns 1 root bridge: Seg=0 Bus=0..255
  LowMmio=0xF4000000+0x4000000 HighMmio=0x1000000000+0x40000000
- `PciHostBridgeDxe` registers MMIO apertures in GCD (no conflicts)
- `NotifyPhase` full sequence completes:
  BeginBusAllocation → EndBusAllocation → BeginResourceAllocation →
  SubmitResources(Success) → AllocateResources → SetResources →
  EndResourceAllocation
- `StartPciDevices` called twice (ConnectAll loop iterates)

### PciBusDxe enumeration
- Finds root port at [00|00|00]: VID=1414 DID=C030 Class=060400 HeaderType=01
- Programs secondary bus number = 1 on root port
- Finds NVMe at [01|00|00]: VID=1414 DID=00A9 Class=010802 HeaderType=00
- BAR sizing completes for all 6 BARs (0xFFFFFFFF write/read/restore cycle)

### openvmm side (config space forwarding)
- Root complex routes bus 0 reads to root port config space
- Root port forwards bus 1 reads/writes to NVMe device via `pcie::port`
- Full BAR programming observed:
  - BAR0 (offset 0x10) = 0xF4000000  (low MMIO, 64KB NVMe register space)
  - BAR4 (offset 0x20) = 0xF4010000  (MSIX table)
  - Command register = 0x100027 (MMIO + bus master enabled)
  - Then re-disabled to 0x100010 (bus master only, awaiting driver bind)
- IRQ line programmed (offset 0x3C = 0xFF)

## What fails

**`NvmExpressDxe::Supported()` is never called.**

After `StartPciDevices` completes and PCI_IO protocol handles exist for the
NVMe device, `BmConnectAllDriversToAllControllers` does not call
`ConnectController` on the NVMe handle.  As a result, `NvmExpressDxe` never
gets a chance to bind, no `EFI_BLOCK_IO_PROTOCOL` is created, and the boot
manager finds no bootable device.

## Root cause analysis

`BmConnectAllDriversToAllControllers` works as follows:

```
do {
    HandleBuffer = LocateHandleBuffer(AllHandles);
    for each Handle in HandleBuffer:
        ConnectController(Handle, NULL, NULL, TRUE);  // recursive
    DispatchNewDrivers();
} while (new drivers dispatched);
```

PciBusDxe's `Start()` is called when `ConnectController` processes the
PCI host bridge handle.  During `Start()`, PciBusDxe enumerates PCI
devices and calls `RegisterPciDevice` which installs `EFI_PCI_IO_PROTOCOL`
on **new** handles.

These new NVMe handles are created mid-iteration.  The `Recursive=TRUE`
flag on `ConnectController` should connect child handles, but only if
PciBusDxe properly registers them as children of the root bridge handle
via `OpenProtocol(..., BY_CHILD_CONTROLLER)`.

**Hypothesis:** PciBusDxe creates the NVMe PCI_IO handle but does not
establish the parent-child relationship with the root bridge in a way that
`ConnectController(rootBridge, ..., TRUE)` recognises.  The second loop
iteration finds no new dispatched drivers (NvmExpressDxe was already
dispatched) and exits without connecting the NVMe handle.

## Diagnostics limitations

- The firmware AdvancedLogger buffer is read once via
  `PROCESS_EFI_DIAGNOSTICS` during early DXE.  Messages written after that
  point (most of PciBusDxe's flow, all of NvmExpressDxe) are captured in
  the buffer but only visible if the buffer is re-read.
- `efi_diagnostics_log_level = Full` enables all levels on the VMM reader,
  but the reader only processes what's in the buffer at read time.
- `DEBUG_ERROR` messages from our libraries are reliably captured because
  they're written before the diagnostics read.

## Next steps

1. **Verify the ConnectAll hypothesis.** Add a second `ConnectAll` call
   (or `ConnectController` on the NVMe handle specifically) in
   `DeviceBootManagerLib.c` after the existing `EfiBootManagerConnectAll()`.
   If NvmExpressDxe binds on the second call, the hypothesis is confirmed.

2. **Alternative: force connection in BDS.**  Modify
   `DeviceBootManagerLib.c` to call `gBS->ConnectController` on all
   `EFI_PCI_IO_PROTOCOL` handles after `ConnectAll` returns.  This is the
   minimal firmware fix.

3. **Investigate parent-child protocol relationships.**  Check whether
   `RegisterPciDevice` in PciBusDxe calls
   `OpenProtocol(..., BY_CHILD_CONTROLLER)` to register the NVMe handle as
   a child of the root bridge handle.  If not, `ConnectController(..., TRUE)`
   won't recursively connect it.

4. **Re-read diagnostics buffer.**  Add a second
   `PROCESS_EFI_DIAGNOSTICS` call in the boot failure path so we can capture
   the full PciBusDxe + NvmExpressDxe log.  This is a firmware change in
   the event log / diagnostics reporting code.

5. **Compare with VPCI NVMe flow.**  The existing VPCI NVMe boot works
   because `VpcivscDxe` creates PCI_IO handles during early DXE dispatch
   (before BDS), so they're present when ConnectAll first runs.  The ePCI
   path creates handles *during* ConnectAll, which is the timing difference.
