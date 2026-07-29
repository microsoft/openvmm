# Arm SMMUv3

OpenVMM emulates an Arm SMMUv3 IOMMU for aarch64 guests, attached to a PCIe
root complex. Devices behind a covered root complex present IOVAs rather
than GPAs, and the SMMU translates them using stream table entries (STEs)
and context descriptors (CDs) that the guest programs in its own memory.

One SMMU instance covers one root complex. A device's StreamID is its
guest-assigned RequesterID (bus/device/function), so the guest's stream
table is indexed directly by BDF.

See [`--smmu`](../../openvmm/management/cli.md#smmu-aarch64-only) for the
command-line syntax.

## Advertised capabilities

The emulated SMMU advertises a deliberately small feature set. Anything not
listed here reads as unsupported in the IDR registers:

| Feature | Value | Notes |
| --- | --- | --- |
| Stage 1 (`IDR0.S1P`) | supported | |
| Stage 2 (`IDR0.S2P`) | not supported | stage-2 STE configs are ILLEGAL |
| Translation table format | AArch64 only | |
| Endianness | little-endian only | |
| Translation granule | 4 KiB only | |
| Stream table format | linear only | no 2-level tables |
| StreamID size | 16 bits | |
| Substreams (SSID/PASID) | not supported | `IDR1.SSIDSIZE` = 0 |
| ATS (`IDR0.ATS`) | not supported | `ATC_INV` is an illegal command |
| Range invalidation (`IDR3.RIL`) | not supported | |
| Fault model | terminate | `STALL_MODEL` = 1, no stalling |
| Attribute override | not supported | `ATTR_TYPES_OVR`/`ATTR_PERMS_OVR` = 0 |

Because attribute override is not advertised, the STE fields that would
control incoming memory attributes and permissions (`MTCFG`, `MemAttr`,
`SHCFG`, `ALLOCCFG`, `NSCFG`, `PRIVCFG`, `INSTCFG`) are RES0 and ignored.

## Translation modes

Two independent things determine how a device's DMA is handled: whether the
SMMU is enabled, and what the device's STE says.

While `CR0.SMMUEN` is 0, `GBPA.ABORT` selects the global policy: transactions
either bypass (IOVA = GPA) or abort. `GBPA.ABORT` resets to 0, so DMA
bypasses before the guest brings the SMMU up — this preserves device access
during firmware and early boot.

Once enabled, each transaction's `STE.Config` decides:

| `STE.Config` | Behavior |
| --- | --- |
| `0b000` (and reserved `0b0xx`) | Abort, **no** event recorded |
| `0b100` | Bypass — IOVA = GPA |
| `0b101` | Stage-1 translate via the CD and its page tables |
| `0b11x` | ILLEGAL (stage 2 is not implemented) — abort, `C_BAD_STE` |

An invalid STE (`V` = 0) or an out-of-range StreamID also aborts and records
an event, per the SMMUv3 architecture.

## Software translation

By default the SMMU walks the guest's structures in software: it reads the
STE, then the CD, then walks the stage-1 page tables to produce a GPA for
each DMA access and MSI write. Faults are recorded to the guest's event
queue and signalled with the EVTQ interrupt.

This works for emulated devices, whose DMA already flows through the VMM.
It cannot work for a VFIO-assigned device, whose DMA never reaches the VMM
— so assigning a device behind a non-accelerated SMMU is rejected at
startup rather than silently ignoring the guest's translations.

## Accelerated translation

With `accel`, the SMMU delegates translation to the host IOMMU using
iommufd nesting. The guest's stage-1 tables are installed into the physical
SMMU as a nested domain whose parent is the stage-2 domain that maps guest
GPAs to host physical addresses. The physical SMMU then performs the full
IOVA → GPA → HPA walk in hardware.

The emulator still owns the SMMU programming interface. It decodes the
guest's STE and dispatches one of three dispositions per device:

| Guest STE | Host action |
| --- | --- |
| Abort / invalid / illegal | attach a shared nested abort page table |
| Bypass | attach a shared nested bypass page table (identity GPA→HPA) |
| Stage-1 translate | allocate a nested page table carrying the STE, attach it |

Every device is always attached to some page table under the vIOMMU — the
abort and bypass page tables are preallocated so a transition never has to
leave a device detached.

### Invalidation

TLB and configuration invalidation commands the guest writes to the command
queue are forwarded to the host. Consecutive forwardable commands accumulate
into a batch that is flushed as a single host invalidation at each
synchronization or configuration boundary, so a shootdown burst costs one
syscall instead of one per command.

Invalidation is scoped to the vIOMMU rather than to individual devices, so a
guest command is forwarded once regardless of how many devices sit behind
the SMMU.

### StreamID binding

The host needs the guest's StreamID to route invalidations and faults, but
the guest assigns bus numbers during PCIe enumeration, well after the device
is created. The StreamID is therefore captured from the first routed config
space write to the device, and rebound if the guest later renumbers the bus.
Until it is known, the device is left aborting.

A function level reset — through either the PCIe or the conventional PCI
Advanced Features capability — clears the binding across the reset and
re-establishes it afterwards.

### Requirements and constraints

- The host IOMMU must be an Arm SMMUv3 with iommufd nesting support, and
  must supply AArch64 page tables, little-endian walks, and the 4 KiB
  granule. Host capabilities are checked when the first device attaches.
- Assigned devices must use the VFIO cdev + iommufd path. The legacy
  group/container interface cannot express nested translation.
- All devices behind one accelerated SMMU must share a single iommufd
  context, because the emulated SMMU maps to exactly one host vIOMMU.
- The guest must boot with ACPI. The host IOMMU reserves an IOVA window for
  its MSI doorbell, and that window is described to the guest as an IORT
  Reserved Memory Range so the guest identity-maps it in stage 1. Without
  it, MSIs from assigned devices would not reach the host interrupt
  controller.

### Output address size

The advertised output address size (`IDR5.OAS`) cannot exceed the physical
SMMU's. With `oas=auto` the emulated SMMU adopts the host's OAS once the
first device attaches; with a fixed `oas=N`, a value larger than the host's
is rejected.

### Fault reporting

Faults raised by a stream that the physical SMMU is actively translating
originate in hardware, not in the emulator, so they must be relayed from the
host to the guest's event queue. That relay is not yet implemented, so
faults from accelerated streams are currently not visible to the guest.
