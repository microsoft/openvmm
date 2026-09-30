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
| Substreams (`IDR1.SSIDSIZE`) | configurable with `ssidsize=auto|N` | `auto`: host-derived with `accel`, otherwise 0 |
| ATS (`IDR0.ATS`) | host-derived with `accel` | otherwise `ATC_INV` is illegal |
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

Bypass still checks address size. An address exceeding `IDR5.OAS` aborts
without an event when the SMMU is disabled. With the SMMU enabled, STE bypass
or default-substream bypass (`S1DSS = 1`) instead records a stage-1
`F_ADDR_SIZE` event. The default-substream bypass does not fetch a CD.

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

### Host-derived capabilities

The `ssidsize=auto|N` option controls the advertised SubstreamID width
(`IDR1.SSIDSIZE`). `auto` is the default: software mode uses 0, while
cold-plug VFIO binding under `accel` adopts the physical SMMU's width.
A fixed `N` must be from 0 to 20; 0 disables substreams, and a nonzero value
advertises that width even in software mode. Under `accel`, a fixed width
must not exceed the host SMMU's width.

Cold-plug VFIO binding also discovers the physical SMMU's ATS support.
ATS has no separate configuration option.
The first device start freezes the capabilities. PCI resource
assignment already starts and stops devices with guest VPs held stopped,
before firmware construction, so firmware reads stable capabilities without
a separate finalization step.

IORT describes the root complex's ATS support, independently of the SMMU's
`IDR0.ATS`. OpenVMM's current platform policy advertises ATS on a root complex
when its SMMU advertises it. Firmware construction derives this attribute
from the SMMU state; it does not change or freeze capabilities.
Firmware reloads use the same frozen capabilities rather than rediscovering
the host.

With `ssidsize=auto`, an accelerated SMMU with no cold-plug device keeps SSID
disabled. Without a cold-plug device, ATS also stays disabled. Later
attachment validates against the frozen capabilities; it cannot enable a
feature the guest was not initially offered.
Failed capability validation leaves the contract unchanged.

SMMU SSID width is not an endpoint's PASID capability. Endpoints have their
own limits and PCI capability discovery; SMMU advertisement does not fabricate
an endpoint capability.

When substreams are advertised, software DMA from emulated devices still
carries no SubstreamID. With `S1CDMax > 0`, it honors the STE's default-substream
policy: terminate, bypass stage 1, or translate through CD 0. With `S1CDMax = 0`,
the format and default-substream fields are ignored and translation uses the
single CD. The reserved `S1Fmt = 3` encoding behaves as linear, and
`S1DSS = 3` behaves as terminate.

The `smmu_software_ssidsize_aarch64_tcg` VMM test boots Linux with a software
SMMU advertising 14 SSID bits, checks the guest-visible width, and exercises
emulated NVMe DMA in a translating domain. It runs in the AArch64 TCG
incubator without an SSID-capable host SMMU. The endpoints do not advertise
PASID, so this covers untagged, single-CD DMA with nonzero `SSIDSIZE`, not
selection between PASID-tagged substreams.

As required by SMMUv3's STE validity rules, both software and nested paths
reject a CD table size exceeding the advertised SSID width, or a two-level
CD format when substreams are enabled (`CD2L` is not advertised). These are
STE-wide configuration errors, including for untagged transactions; they
are checked before the default-substream policy.

With ATS, nested STEs retain `EATS` and the emulator forwards `ATC_INV` in
command-queue order. The Linux backend supports Full ATS and ATS UR, not
S1-only ATS. That restriction is enforced by the backend, not by the shared
STE classifier: a rejected nested attachment is left aborting, while
software DMA does not acquire a host-specific ATS restriction.
Linux enables physical `CR0.ATSCHK` when ATS is supported;
iommufd controls per-stream ATS through `EATS`. The virtual register
acknowledges ATSCHK, but clearing it cannot disable the host's checking.
Emulated devices do not issue ATS requests or cache ATS translations.

Reset preserves the frozen capabilities. Saved state records them, and
restore rejects a different capability contract before restoring registers.
A successful restore freezes the capabilities without starting devices or
reassigning PCI resources.
Snapshots predating capability recording are accepted only when SSID and ATS
remain disabled.

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
- The guest must boot with ACPI. One MSI IOVA range is shared between the
  physical-SMMU implementation and the guest: the backend uses it for assigned
  device MSI writes, and the IORT describes the same range as Reserved Memory
  so the guest identity-maps it in stage 1. Linux's Arm SMMU driver currently
  requires its fixed 128 MiB--129 MiB range. A hypervisor-backed SMMU can let
  OpenVMM select the range and pass its base during partition creation.
  Without the matching RMR, assigned-device MSIs can fault in guest stage 1.

### Output address size

The advertised output address size (`IDR5.OAS`) cannot exceed the physical
SMMU's. With `oas=auto`, a cold-plug accelerated device causes the emulated
SMMU to adopt the host's OAS. Device start freezes the advertised value,
including the temporary start for PCI resource assignment before firmware
construction. A device hotplugged after that point must support the frozen
OAS; attachment fails rather than changing a capability the guest may
already have observed. With a fixed `oas=N`, a value larger than the host's
is rejected.

### Fault reporting

Faults raised by a stream the physical SMMU is actively translating originate
in hardware, not in the emulator, so the VMM never sees the transaction that
caused them. The host reports them on a virtual event queue attached to the
vIOMMU; the VMM drains that queue and writes each record into the guest's
event queue unchanged, since the host has already expressed the StreamID in
the guest's namespace.

If the host virtual event queue itself overflows, the VMM logs the loss and
continues forwarding the records that remain available. It cannot represent
that loss as a guest event queue overflow, because `EVENTQ_PROD.OVFLG`
specifically means the guest's event queue was full.
