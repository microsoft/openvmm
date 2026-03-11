# OpenHCL storage translation

OpenHCL maps storage offered into VTL2 onto the controller and disk
model that VTL0 sees. This page covers that mapping — the *outside*
of the shell. For the *inside* (how guest I/O flows from a storage
frontend through the SCSI adapter and disk backend abstraction to a
concrete backing store), see the
[storage pipeline](../devices/storage.md) page.

## Overview

OpenHCL storage translation sits between two different views of storage:

- the **backing device path** from the Root into VTL2
- the **guest-visible target** that OpenHCL exposes from VTL2 into VTL0

Those are related views, but they are not the same view. Two VMs can both show a SCSI disk to the guest while using different backing-device families and different OpenHCL code paths underneath.

## The two-axis model

The most important mental model for this topic is to ask two separate questions:

1. What kind of storage is offered into VTL2?
2. What kind of storage does VTL0 enumerate?

The first question is about the backing side. The second question is about the guest-visible controller and disk model. The page stays anchored on that split because it is the easiest way to avoid flattening every scenario into "the guest sees SCSI."

## High-level stack

```text
  ┌──────────────────────────────────────────────┐
  │  VTL0 guest                                  │
  │                                              │
  │  Guest OS                                    │
  │    └─ Storage controllers (IDE, SCSI, NVMe)  │
  │         └─ Disks (drives, LUNs, namespaces)  │
  └──────────────────────┬───────────────────────┘
                         │
  ┌──────────────────────┼───────────────────────┐
  │  VTL2 OpenHCL        │                       │
  │                      ▼                       │
  │  Guest-facing storage endpoints              │
  │    └─ Storage translation and mapping        │
  │         └─ Backing-device access             │
  └──────────────────────┬───────────────────────┘
                         │
  ┌──────────────────────┼───────────────────────┐
  │  Root / host         │                       │
  │                      ▼                       │
  │  Storage offered to VTL2 (vSCSI or NVMe)     │
  └──────────────────────────────────────────────┘
```

This diagram shows the architectural boundaries, not every runtime detail. The main point is that OpenHCL sits in the middle and owns the guest-visible storage shape.

## The mapping is not inherently 1:1

```text
  Guest-visible model                    Backing devices

  Controller (IDE, SCSI, or NVMe)
   ├── Disk 0 ─────────────────────────── Physical device A
   └── Disk 1 ──┬──────────────────────── Physical device B
                └──────────────────────── Physical device C
```

One guest-visible controller can own multiple guest-visible disks. One guest-visible disk can be backed by a single physical device or a striped set of physical devices. In the configuration model this appears as `PhysicalDevices::Single` versus `PhysicalDevices::Striped`.

## Main path families

| Path family | Root to VTL2 backing | VTL2 to VTL0 target | Why it matters |
|-------------|----------------------|---------------------|----------------|
| **NVMe to SCSI** | `NVMe` | SCSI | The Root offers NVMe to VTL2, and OpenHCL exposes a SCSI controller plus SCSI-addressed disks to VTL0. This is the most useful overview example because it exercises both the bus substrate and storage translation. |
| **vSCSI to SCSI** | `vSCSI` | SCSI | The guest-visible result can look similar to the row above, but the backing side, configuration surface, and code path differ. |
| **NVMe to NVMe** | `NVMe` | NVMe | The controller families match, but the backing interface and guest-visible interface are still separate layers with separate ownership. |
| **vSCSI or NVMe to IDE** | `vSCSI` or `NVMe` | IDE | IDE remains part of the guest-visible storage vocabulary, especially when boot or legacy expectations matter. |

Architecture-wise, IDE belongs in the same model as SCSI and NVMe. The important limitation is path-specific, not universal. The current OpenVMM + OpenHCL settings helper doesn't wire IDE targets, so the OpenVMM-specific examples in this page set focus on SCSI and NVMe guest targets. That's narrower than the full architecture. IDE still matters for standalone OpenVMM configurations and for Hyper-V scenarios, including Gen1 or PCAT boot paths that run with OpenHCL on Hyper-V. For those scenarios, see [VM Configurations: Gen1 vs Gen2 Equivalents](../../../user_guide/openvmm/vm_configurations.md), [Running OpenHCL with Hyper-V](../../../user_guide/openhcl/run/hyperv.md), and [Hyper-V VM generations](https://learn.microsoft.com/en-us/windows-server/virtualization/hyper-v/plan/Should-I-create-a-generation-1-or-2-virtual-machine-in-Hyper-V).

## How VTL0 enumerates storage

What VTL0 sees is configurable. OpenHCL does not decide on its own what devices to present. The chosen configuration determines which controllers, offers, and emulators are exposed to the guest.

VTL0 does not see an unstructured pool of disks. It enumerates storage through controller families:

- IDE controllers
- SCSI controllers exposed over VMBus storvsp
- NVMe controllers exposed over vPCI

That controller boundary matters because ownership is coarse-grained. If OpenHCL owns a SCSI controller, it owns the LUNs beneath it. If OpenHCL owns an NVMe controller, it owns the namespaces beneath it. The same rule applies when the Root rather than OpenHCL owns the controller.

### VMBus-exposed storage devices

Generation 1 and Generation 2 VMs can both have VMBus devices. On Windows, `vmbus.sys` consumes channel offers and causes guest-visible devices to appear. Driver binding then happens against the offered device identity.

Storvsp-backed SCSI follows that model. The same guest-visible storage offer can come from the Hyper-V host, from OpenVMM's storvsp implementation, or from OpenHCL's storvsp path in storage translation scenarios. See [`vm/devices/storage/storvsp/src/lib.rs`](https://github.com/microsoft/openvmm/blob/main/vm/devices/storage/storvsp/src/lib.rs).

### PCI-enumerated IDE devices

PCI-enumerated devices are different. This is where the IDE emulator sits. The guest discovers the controller through PCI enumeration rather than through a VMBus offer, and then loads the appropriate IDE driver stack against that PCI-visible device.

Inside OpenVMM and OpenHCL, the IDE front end still feeds into the shared lower-level storage machinery. Read IDE as a guest-visible controller choice, not as a separate backing-device universe[^toaster]. See [`vm/devices/storage/ide/src/lib.rs`](https://github.com/microsoft/openvmm/blob/main/vm/devices/storage/ide/src/lib.rs).

### Why this distinction matters for OpenHCL

In OpenHCL scenarios, VMBus offers can be directed to either VTL0 or VTL2. PCI-emulated devices need their emulator to live on the side that owns the PCI model, so IDE placement has different constraints from storvsp-backed VMBus storage.

The same ownership issue matters for VMBus storage controllers too. Some guest setups and guest scripts assume that several disks belong to one synthetic SCSI controller, for example the OS disk, BEK disk, ISO, and resource disk. Because storvsp is the controller, the Root and VTL2 cannot both partially own that same guest-visible controller. One side has to be the single owner of the controller and its LUN layout. That is one reason the vSCSI-to-SCSI path matters: OpenHCL can consume vSCSI on the backing side while still presenting one coherent guest-visible SCSI controller model to VTL0.

## Concrete example: NVMe backing to SCSI guest target

The [Running OpenHCL with OpenVMM](../../../user_guide/openhcl/run/openvmm.md) page uses the following shorthand in its "Assigning NVME devices to VTL2" example:

```bash
cargo run -- \
  --hv --vtl2 \
  --igvm path/to/openhcl.igvm \
  --vmbus-redirect \
  --disk mem:1G,uh-nvme
```

This command offers NVMe into VTL2 and lets OpenHCL expose guest-visible storage through its own controller model.

The guest-visible and backing sides remain distinct in the VTL2 settings model:

```json
{
  "dynamic": {
    "storage_controllers": [
      {
        "protocol": "SCSI",
        "luns": [
          {
            "location": 0,
            "physical_devices": {
              "type": "single",
              "device": {
                "device_type": "nvme",
                "device_path": "<VMBUS_INSTANCE_GUID>",
                "sub_device_path": 1
              }
            }
          }
        ]
      }
    ]
  }
}
```

The important split is:

- the **guest-visible controller** (`protocol: "SCSI"`)
- the **guest-visible child location** (`location: 0`)
- the **backing physical device** (`device_type`, `device_path`, and `sub_device_path`)

If the guest-visible disk is striped, the backing side changes shape while the guest-visible side can stay the same:

```json
{
  "physical_devices": {
    "type": "striped",
    "devices": [
      { "device_type": "nvme", "device_path": "<GUID_A>", "sub_device_path": 1 },
      { "device_type": "nvme", "device_path": "<GUID_B>", "sub_device_path": 1 }
    ]
  },
  "chunk_size_in_kb": 128
}
```

```admonish note
The [NVMe Emulator](../../emulated/NVMe/overview.md) guide notes that the OpenVMM NVMe emulator can serve I/O workloads, but pragmatically is only used by OpenVMM for test scenarios today. Keep that test-oriented context in mind when using `uh-nvme` examples from the OpenVMM flow.
```

## Concrete example: vSCSI backing to SCSI guest target

The [Running OpenHCL with OpenVMM](../../../user_guide/openhcl/run/openvmm.md) page shows the vSCSI backing case:

```bash
cargo run -- \
  --hv --vtl2 \
  --igvm path/to/openhcl.igvm \
  --vmbus-redirect \
  --disk file:ubuntu.img,uh
```

The guest-visible controller family (SCSI) happens to match the backing family (also SCSI via storvsp), but the controller instances are different. The Root owns the backing SCSI controller. OpenHCL owns the guest-visible SCSI controller. The `sub_device_path` in the VTL2 settings maps a host LUN on the backing controller to a guest LUN on the guest-visible controller.

In the VTL2 settings model, this path looks like:

```json
{
  "dynamic": {
    "storage_controllers": [
      {
        "protocol": "SCSI",
        "luns": [
          {
            "location": 0,
            "physical_devices": {
              "type": "single",
              "device": {
                "device_type": "vscsi",
                "device_path": "<HOST_SCSI_CONTROLLER_GUID>",
                "sub_device_path": 0
              }
            }
          }
        ]
      }
    ]
  }
}
```

The important difference from the NVMe example is `device_type`: `"vscsi"` instead of `"nvme"`. Because both the backing and guest-visible sides are SCSI, this path can look like a no-op, but OpenHCL still owns the guest-visible controller separately from the host. That separation is why the guest can see a different LUN layout, different device identity strings, and a different controller instance GUID than what the host offered.

This is also the path used in the [Hyper-V storage relay tutorial](../../../user_guide/openhcl/run/hyperv.md#using-openhcl-to-relay-storage), which walks through the full setup including controller creation, VMBus redirect, and VTL2 settings JSON.

## Implementation map

| Component | Why read it | Source |
|-----------|-------------|---------------------|
| OpenVMM Underhill storage builder | Separates the backing device family from the guest-visible target and groups resulting children under guest-visible controllers | [`add_underhill()` and `build_underhill()`](https://github.com/microsoft/openvmm/blob/main/openvmm/openvmm_entry/src/storage_builder.rs#L249-L483) |
| Petri VTL2 settings helpers | Shows the builder-side model for backing devices, LUNs, and storage controllers | [`Vtl2StorageBackingDeviceBuilder`, `Vtl2LunBuilder`, and `Vtl2StorageControllerBuilder`](https://github.com/microsoft/openvmm/blob/main/petri/src/vm/vtl2_settings.rs#L19-L253) |
| Underhill configuration model | Defines `PhysicalDevice`, `PhysicalDevices`, and the runtime controller and child types | [`PhysicalDevice`, `PhysicalDevices`, and controller structs`](https://github.com/microsoft/openvmm/blob/main/vm/devices/get/underhill_config/src/lib.rs#L27-L207) |
| Underhill configuration schema | Shows how `Lun` objects are parsed into `Single`, `Striped`, and protocol-specific child types | [`impl ParseSchema<crate::PhysicalDevices> for Lun`](https://github.com/microsoft/openvmm/blob/main/vm/devices/get/underhill_config/src/schema/v1.rs#L233-L375) |
| OpenHCL runtime storage worker | Shows where `Single` and `Striped` backing are materialized into runtime disk handles and where `sub_device_path` is resolved | [`make_disk_type_from_physical_devices()` and `make_disk_type_from_physical_device()`](https://github.com/microsoft/openvmm/blob/main/openhcl/underhill_core/src/dispatch/vtl2_settings_worker.rs#L925-L1042) |

[^toaster]: For a Windows-oriented introduction to guest device enumeration and driver binding, the [WDF Toaster sample](https://github.com/microsoft/Windows-driver-samples/tree/main/general/toaster) is a useful companion reference.
