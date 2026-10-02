# gRPC / ttrpc

To enable a gRPC or ttrpc management interface, pass `--rpc`. This spawns an
OpenVMM process acting as an RPC server on the given Unix socket:

```bash
--rpc path=/path/to/openvmm.sock[,transport=<TRANSPORT>]
```

`transport` selects which wire protocol the server accepts:

* `auto` (default) — auto-detect ttrpc vs. gRPC per connection
* `ttrpc` — accept ttrpc clients only
* `grpc` — accept gRPC clients only

For example, to accept ttrpc clients only:

```bash
--rpc path=/path/to/openvmm.sock,transport=ttrpc
```

Here is a list of supported RPCs:

```admonish note title="API reference"
The API continues to evolve, and compatibility between releases is not
guaranteed. The [`vmservice.proto`] file is the authoritative API definition.
The list below summarizes the available RPCs; some definitions may be added
before their implementation is connected end to end.
```

* CreateVM
* TeardownVM
* PauseVM
* ResumeVM
* SaveVM
* RestoreVM
* WaitVM
* CapabilitiesVM
* PropertiesVM
* ModifyResource
* AddPcieDevice
* RemovePcieDevice
* AddVpciDevice
* RemoveVpciDevice
* Quit

`SaveVM` is a compound operation that pauses the VM and writes `manifest.bin`,
`state.bin`, and `memory.bin` to the requested directory. The VM must use the
`MemoryConfig.backing_file_path` setting. The currently supported
implementation hard-links that backing file into the snapshot, and the VM
remains paused and cannot be resumed afterward. Each save generates a unique
snapshot ID (a GUID, as 16 bytes) that is stored in `manifest.bin` and
returned in `SaveVmResult.snapshot_id`.

`RestoreVM` creates a VM from a snapshot directory and a compatible
`VMConfig`. Device configuration is not stored in the snapshot and must be
supplied by the caller. `memory.bin` is used directly as mutable guest memory,
so `MemoryConfig.backing_file_path` must not be set. If
`expected_snapshot_id` is provided, the restore fails unless it matches the
snapshot ID in `manifest.bin`. The restored snapshot's ID is returned in
`RestoreVmResult.snapshot_id`. Set `resume` to start the restored VM
immediately; otherwise it is created paused.

`AddVpciDevice` dynamically exposes a PCI device to VTL0 over Hyper-V VPCI.
The VM must have Hyper-V enlightenments and VMBus enabled, and the host
hypervisor backend must support virtual devices. The caller supplies the
guest-visible instance ID in `AddVpciDeviceRequest.instance_id` and uses the
same ID for `RemoveVpciDevice`. The response is empty. Removing an unknown
or previously removed instance ID returns an error.

Unlike `AddPcieDevice`, VPCI does not require a root complex or a predeclared
hotplug-capable PCIe port. `AddPcieDevice` remains available when standard PCIe
hotplug semantics or a non-VPCI host backend is required.

## VFIO and accelerated SMMU

On Linux, declare named host iommufd contexts in `VMConfig.iommufds`, then
reference a context with `VfioDevice.iommufd_id`. The server opens
`/dev/iommu` and uses VFIO cdev assignment. Devices referencing the same ID
share the context and its DMA address space. The host device must already
be bound to `vfio-pci`.

Context IDs must be non-empty and unique within the VM. An empty or unknown
device reference is an error, as is unavailable iommufd support. Omitting
`iommufd_id` selects legacy VFIO group/container assignment; failures on the
iommufd path do not fall back to legacy assignment.

Contexts are declared at VM creation and retained until teardown, even when
unused or after their last device is removed. `AddPcieDevice` uses the same
`VfioDevice` message and can reference any declared context. There is no RPC
to add or remove contexts, and no client file descriptor is needed.

For AArch64 guests, configure a guest-visible SMMUv3 with
`PcieRootComplex.iommu.smmu`. This is separate from the host iommufd context.
The following protobuf text-format fragment shows PCIe configuration for an
accelerated SMMU and an assigned device; add the VM's boot, memory, and
processor configuration to form a complete `VMConfig`:

```text
iommufds { id: "iommu0" }
pcie {
	root_complexes {
		name: "rc0"
		end_bus: 255
		low_mmio: 67108864
		high_mmio: 1073741824
		iommu { smmu { accel: true } }
		root_ports {
			name: "rp0"
			hotplug: true
			attached {
				device {
					vfio {
						host_pci_address: "0000:01:00.0"
						iommufd_id: "iommu0"
					}
				}
			}
		}
	}
}
```

`accel: true` enables hardware nested translation and requires ACPI, a
nesting-capable host SMMUv3, and hypervisor support for the assigned-device
MSI IOVA reservation. VFIO devices behind the SMMU must use a single shared
iommufd context. With `accel` false, the SMMU uses software translation and
does not support VFIO assignment behind it. Omitting `iommu` leaves the root
complex without a guest-visible IOMMU.

`SmmuConfig.oas_bits` selects a fixed output address size in bits. Omit it for
the CLI's `oas=auto` policy: initially 48 bits, adopting the physical SMMU's
width when an accelerated device attaches before VM start. VM start freezes the
advertised width; subsequent hot-add must be compatible with it. A fixed width
cannot exceed the physical SMMU's width with acceleration. SMMU configuration
cannot be changed at runtime. See [the CLI reference](cli.md) and [Arm
SMMUv3](../../emulated/iommu/smmuv3.md) for platform requirements.

[`vmservice.proto`]: https://github.com/microsoft/openvmm/blob/main/openvmm/openvmm_ttrpc_vmservice/src/vmservice.proto
