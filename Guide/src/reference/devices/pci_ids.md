# PCI IDs

This page records the PCI identifiers that OpenVMM reports for the PCI and PCIe
devices it emulates.

```admonish note
These IDs describe how a device advertises itself to the guest in PCI config
space. They are distinct from VMBus/VPCI instance GUIDs, which identify device
*instances* at runtime and are typically randomly generated per VM.
```

## Microsoft vendor ID (`0x1414`)

Most first-party OpenVMM devices report Microsoft's PCI vendor ID, `0x1414`.

Device IDs under this vendor ID are **allocated and centrally managed by
Microsoft**. You cannot pick an arbitrary device ID when adding a new emulated
device: a device ID has to be formally assigned so that it does not collide
with any other Microsoft product that shares this vendor ID. The values below
are the assignments OpenVMM currently uses.

These identifiers — the vendor ID, each device ID, and the default subsystem
ID — are all defined in one place, the `microsoft` module of the `pci_core`
crate (`vm/devices/pci/pci_core/src/microsoft.rs`), and referenced from each
device.

| Device ID | Device                      | Crate              |
| --------- | --------------------------- | ------------------ |
| `0x00BA`  | MANA/GDMA network adapter   | `gdma`             |
| `0x5353`  | VGA adapter                 | `vga`              |
| `0xC030`  | PCIe root port              | `pcie`             |
| `0xC031`  | PCIe upstream switch port   | `pcie`             |
| `0xC032`  | PCIe downstream switch port | `pcie`             |
| `0xC03E`  | NVMe controller             | `nvme`, `nvme_test`|

These devices all report a subsystem vendor ID and subsystem ID of `0`.

## Devices that report standardized IDs

Some devices emulate a real, standardized controller and therefore report the
hardware vendor/device ID that stock guest drivers bind to, rather than a
Microsoft-allocated ID:

- **virtio** devices use the virtio-over-PCI scheme: vendor ID `0x1AF4` and
  device ID `0x1040` plus the virtio device type. They report Microsoft's
  `0x1414` as the *subsystem vendor ID* to identify OpenVMM as the host
  environment, with a subsystem ID of `0x2000`. Source:
  `vm/devices/virtio/virtio/src/transport/pci.rs`.
- **AMD IOMMU** reports AMD's vendor ID `0x1022` and device ID `0x1451`
  (a family 17h / Zen IOMMU) so that the guest's AMD IOMMU driver binds to it.
  It reports Microsoft's `0x1414` as its subsystem vendor ID and `0x2000` as
  its subsystem ID to identify OpenVMM as the host environment. Source:
  `vm/devices/iommu/amd_iommu/src/lib.rs`.
