# GB200 direct UVM on MSHV

This path implements Design A: Hyper-V is the only owner of the guest-visible
SMMUv3, while OpenVMM provides configuration, VFIO ownership, Hyper-V object
creation and ACPI firmware.

## Command line

ATS is disabled unless `ats` is explicitly present:

```text
--iommu id=iommu0 --direct-iommu iommu=iommu0 \
--smmu rc=rc0,accel,ats,ssid-bits=14 \
--vfio host=0008:06:00.0,port=rp0,iommu=iommu0
```

`ats` requires `accel`, a nonzero `ssid-bits`, and a VFIO device using a
`--direct-iommu` context on the selected root complex. `ssid-bits` may be 0-20.
An accelerated root complex does not instantiate OpenVMM's local `SmmuDevice`.

## Identities

The DIRECT vDEVICE identity is the Hyper-V logical device ID encoded from the
host physical segment and BDF. The Hyper-V vSID is the final guest requester
ID after PCI resource assignment. IORT publishes the full identity mapping
from every 16-bit guest BDF to the same vSID; compact sequential vSIDs are not
used.

One Hyper-V virtual IOMMU is created per accelerated root complex. A DIRECT
iommufd context may share its kernel DIRECT vIOMMU across devices, while each
device retains its own logical ID, vDEVICE, HWPT and guest vSID.

## Firmware

OpenVMM publishes a matching SMMUv3 IORT node. The PCI root ATS attribute is
set only when `ats` is requested. Each accelerated root complex also receives
a 64-KiB-aligned, identity-only MSI doorbell RMR with one mapping per DIRECT
device. The root complex uses `preserve_config`, because Linux ignores an RMR
whose firmware requester IDs are not preserved. Existing SRAT Generic
Initiator and coherent-memory entries remain unchanged.

## Lifecycle

Startup order is:

1. create the DIRECT kernel vIOMMU, per-device vDEVICE and DIRECT HWPT;
2. attach the VFIO cdev to the HWPT;
3. assign guest PCI resources;
4. create the Hyper-V virtual IOMMU and bind logical devices to identity vSIDs;
5. build firmware.

Failures unwind completed Hyper-V bindings. Direct cleanup explicitly issues
`VFIO_DEVICE_DETACH_IOMMUFD_PT` before destroying the HWPT, then destroys the
vDEVICE and the shared DIRECT vIOMMU after the last device. Failed detach or
destroy operations retain their object IDs and error state for retry. The VM fd
is held until the DIRECT manager is released.

Hyper-V currently has no virtual-IOMMU destroy hypercall. OpenVMM therefore
unbinds every logical device and relies on partition teardown to reclaim the
empty Hyper-V vIOMMU.

## Runtime dependencies

The OpenVMM source mirrors the reviewed, not-yet-upstream DIRECT kernel UAPI:

- `IOMMU_VIOMMU_TYPE_DIRECT = 3`;
- `IOMMU_HWPT_DATA_DIRECT = 3`;
- `IOMMU_HWPT_DIRECT_FLAG_ATS_PASID = 1 << 0`;
- `iommu_viommu_direct` carries the MSHV partition fd;
- VFIO attach/detach payloads include a zero PASID field for whole-device use.

A kernel without that exact UAPI will compile OpenVMM but cannot run this path.
This implementation is build/review only until the matching MSHV/iommufd and
Hyper-V ATS/PASID stack is deployed and validated on hardware.
