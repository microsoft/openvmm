# OpenHCL Storage Configuration Model

The VTL2 settings model describes guest-visible storage controllers, child devices, and their backing devices.

## Overview

The OpenHCL storage settings surface is easiest to read as a small tree:

1. a guest-visible **controller**
2. one or more guest-visible **child devices** under that controller
3. one or more **backing devices** behind each child

The same tree appears in three places:

- the VTL2 settings JSON shown in [Running OpenHCL with Hyper-V](../../../user_guide/openhcl/run/hyperv.md)
- helper builders in `petri::vtl2_settings`
- runtime structs in `underhill_config`

This page lines those views up so you do not have to infer the schema from the JSON alone.

## One controller tree

```text
  StorageController (protocol + instance_id)
   ├── Lun / child 0
   │    └── PhysicalDevices::Single
   │         └── PhysicalDevice
   └── Lun / child 1
        └── PhysicalDevices::Striped
             ├── PhysicalDevice A
             └── PhysicalDevice B
```

The protobuf and JSON surface uses `StorageController` and `Lun`. The runtime view in `underhill_config` splits that same tree into concrete controller and child types such as `ScsiController`, `ScsiDisk`, `NvmeController`, and `NvmeNamespace`.

## Two naming layers

| Concept | JSON / protobuf surface | Runtime surface in `underhill_config` |
|---------|-------------------------|----------------------------------------|
| Guest-visible controller | `StorageController` | `IdeController`, `ScsiController`, `NvmeController` |
| Guest-visible child device | `Lun` | `IdeDisk`, `ScsiDisk`, `NvmeNamespace` |
| Backing wrapper | `PhysicalDevices` | `PhysicalDevices` |
| Backing device | `PhysicalDevice` | `PhysicalDevice` |

One non-obvious point is that the protobuf type keeps using the name `Lun` even when the guest-visible protocol is NVMe. At runtime, that same object becomes an `NvmeNamespace`, because the meaning of the child address depends on the controller protocol.

## `StorageController`

`StorageController` is the guest-visible controller object. It answers the question "what controller family will VTL0 enumerate?"

| Field | Meaning |
|-------|---------|
| `instance_id` | Guest-visible controller instance GUID |
| `protocol` | Guest-visible controller family: `IDE`, `SCSI`, or `NVMe` |
| `luns` | Child devices exposed under that controller |
| `io_queue_depth` | Optional queue-depth tuning for supported controllers |

The `protocol` field is the most important one architecturally. It does not describe the backing side. It describes what kind of controller the guest will see.

## `Lun`

`Lun` is the schema object for a guest-visible child device under a controller. The guest meaning of `location` depends on the controller protocol:

| Controller protocol | Guest meaning of the child | Relevant child fields |
|---------------------|----------------------------|-----------------------|
| `IDE` | IDE drive slot | `channel` + `location` |
| `SCSI` | SCSI LUN | `location` |
| `NVMe` | NVMe namespace | `location` |

Besides the child address, a `Lun` also carries the device identity strings and the backing-device description:

- `device_id`, `vendor_id`, `product_id`, `product_revision_level`, `serial_number`, `model_number`
- `physical_devices`
- `is_dvd`
- `chunk_size_in_kb`

For NVMe, `location` becomes the namespace ID at runtime. For IDE, `channel` is required because the guest-visible slot is a `(channel, location)` pair instead of a single number.

## `PhysicalDevices`

`PhysicalDevices` wraps the backing side of a child device. It tells OpenHCL whether the guest-visible child is backed by zero, one, or multiple physical devices.

| Shape | Meaning |
|-------|---------|
| `EmptyDrive` | No backing media. This is the empty-DVD case in the runtime model. |
| `Single { device }` | Exactly one backing `PhysicalDevice` |
| `Striped { devices, chunk_size_in_kb }` | Two or more backing `PhysicalDevice`s combined into one guest-visible child |

In the protobuf and JSON shape, `Single` uses the singular `device` field. `Striped` uses the plural `devices` field and takes the stripe size from `chunk_size_in_kb`.

## `PhysicalDevice`

`PhysicalDevice` describes one backing device offered into VTL2.

| Field | Meaning |
|-------|---------|
| `device_type` | Backing device family: `nvme` or `vscsi` |
| `device_path` | VMBus instance GUID of the backing controller or device |
| `sub_device_path` | Child inside that backing device |

The `petri::vtl2_settings` helpers describe this directly: both SCSI and NVMe backing devices are treated as VMBus devices, so `device_path` is the VMBus instance ID. The `sub_device_path` field selects the child inside that device:

- for `vscsi`, it is the host LUN
- for `nvme`, it is the namespace ID

IDE is a guest-visible target family, but it is not supported as a VTL2 backing device.

## Example: one SCSI target backed by one vSCSI device

The [Running OpenHCL with Hyper-V](../../../user_guide/openhcl/run/hyperv.md) page includes a concrete example of this shape:

```json
{
  "instance_id": "<GUEST_SCSI_CONTROLLER_GUID>",
  "protocol": "SCSI",
  "luns": [
    {
      "location": 15,
      "physical_devices": {
        "type": "single",
        "device": {
          "device_type": "vscsi",
          "device_path": "<HOST_SCSI_CONTROLLER_GUID>",
          "sub_device_path": 5
        }
      }
    }
  ]
}
```

Read this from top to bottom:

- `protocol: "SCSI"` says the guest-visible controller is SCSI
- `location: 15` says the child appears as guest LUN 15
- `device_type: "vscsi"` says the backing side is vSCSI
- `device_path` identifies the backing SCSI controller offered into VTL2
- `sub_device_path: 5` selects host LUN 5 on that controller

That is the clearest example of why the guest-visible target and the backing side are different layers.

## Example: striped backing

When a guest-visible child is striped, the guest-visible side can stay the same while the backing side fans out:

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

The schema treats stripe geometry as a property of the guest-visible child, not of the controller as a whole.

## Validation rules and limits

The runtime model and schema enforce several important constraints:

| Rule | Where it shows up |
|------|-------------------|
| IDE has 2 channels and 2 drive slots per channel | `underhill_config::IDE_NUM_CHANNELS` and `IDE_MAX_DRIVES_PER_CHANNEL` |
| SCSI supports up to 4 controllers and 64 LUN slots per controller | `underhill_config::SCSI_CONTROLLER_NUM` and `SCSI_LUN_NUM` |
| `Single` must carry exactly one backing device | `schema::v1` parses `device` and rejects extra entries |
| `Striped` must carry at least two backing devices | `schema::v1` rejects too few devices |
| NVMe namespace IDs cannot be `0` or `!0` | `schema::v1` validates `location` for NVMe |
| NVMe children cannot be DVDs | `schema::v1` rejects `is_dvd` on NVMe |
| IDE can be a guest-visible target but not a VTL2 backing source | `petri::vtl2_settings` and `openvmm_entry::storage_builder` reject IDE backing |

These rules are useful when debugging configuration failures because they tell you whether the problem is in controller selection, child addressing, or backing-device declaration.

## How OpenVMM fills this model

The [Running OpenHCL with OpenVMM](../../../user_guide/openhcl/run/openvmm.md) page shows where OpenVMM consumes this model. In code, `openvmm_entry::storage_builder` makes the two-axis split visible:

1. `add_underhill()` chooses a **source** backing device family and a **target** guest-visible controller family separately.
2. It derives `sub_device_path` from the source child it created in VTL2.
3. It emits `Lun` objects whose `physical_devices` point back to that source.
4. `build_underhill()` groups those `Lun`s under guest-visible `StorageController`s for SCSI or NVMe.

Read the configuration model as a description of the guest-visible tree plus a separate description of the backing tree under each child.

## Implementation map

| Component | Why read it | Source | Rustdoc |
|-----------|-------------|--------|---------|
| Underhill configuration model | Defines the runtime controller, child, and backing types | [`vm/devices/get/underhill_config/src/lib.rs`](https://github.com/microsoft/openvmm/tree/main/vm/devices/get/underhill_config/src/lib.rs) | [`underhill_config`](https://openvmm.dev/rustdoc/linux/underhill_config/index.html) |
| Schema parsing and validation | Shows how JSON and protobuf fields map into runtime structs and what errors are enforced | [`vm/devices/get/underhill_config/src/schema/v1.rs`](https://github.com/microsoft/openvmm/tree/main/vm/devices/get/underhill_config/src/schema/v1.rs) | [`underhill_config`](https://openvmm.dev/rustdoc/linux/underhill_config/index.html) |
| Test-side builders | Documents `device_path`, `sub_device_path`, single backing, and striped backing in a compact API | [`petri/src/vm/vtl2_settings.rs`](https://github.com/microsoft/openvmm/tree/main/petri/src/vm/vtl2_settings.rs) | [`petri`](https://openvmm.dev/rustdoc/linux/petri/index.html) |
| OpenVMM builder path | Shows how CLI shorthand is expanded into guest-visible `StorageController` and `Lun` objects | [`openvmm/openvmm_entry/src/storage_builder.rs`](https://github.com/microsoft/openvmm/tree/main/openvmm/openvmm_entry/src/storage_builder.rs) | [`openvmm_entry`](https://openvmm.dev/rustdoc/linux/openvmm_entry/index.html) |
