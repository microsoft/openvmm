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
* WaitVM
* CapabilitiesVM
* PropertiesVM
* ModifyResource
* AddPcieDevice
* RemovePcieDevice
* AddVpciDevice
* RemoveVpciDevice
* Quit

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

## microVM snapshots

`CreateVMRequest.microvm_snapshot` exposes the microVM capture and
restore flow over both transports:

* `destination_path` configures guest-requested capture. Supply a microVM
  configuration, including `DirectBoot` kernel/initrd files, memory, and the
  processor count. The path must not exist. `quiesce_timeout_ms` defaults to
  five seconds when zero.
  `memory_capacity_bytes` optionally reserves an immutable, 128-MiB-aligned
  RAM capacity while keeping the configured base memory as the exact
  `memory.bin` payload and initial Linux direct e820 map.
* `restore_path` selects manifest-authoritative restore. `config` may be
  absent, or may contain only the matching microVM profile, an optional exact
  processor-count assertion, serial port 0 host
  attachment, matching `DevicesConfig.virtio_console` and
  `DevicesConfig.virtiofs_config` attachments, and guest power actions.
  Guest-visible boot, memory, processor topology, other device, NUMA, and PCIe
  fields are rejected. A saved listener is reconstructed from the manifest; a saved
  client requires the matching path configuration.
* `restore_entropy` requests fresh entropy and is valid only with
  `restore_path`. Every microVM process also receives a fresh, non-serialized
  16-byte generation ID through the fixed portb selector. On restore, the ID is
  the first 16 bytes of the entropy packet.
* `restore_processor_count` requests restore-time activation of the contiguous
  VP prefix `0..count-1`. Zero preserves legacy behavior. A nonzero value is
  valid only for a snapshot that advertises processor activation, implies fresh entropy and the
  post-restore gate, and must satisfy the snapshot's boot-online and immutable
  capacity bounds. `ProcessorConfig.processor_count`, when present, remains an
  exact capacity assertion.
* `restore_memory_bytes` selects a 128-MiB-aligned total RAM target between the
  snapshot base and immutable capacity. Zero selects the base. Expansion uses
  fresh private zeroed backing, implies fresh restore packet delivery and the
  post-restore repair gate, and is rejected for legacy snapshots. An explicit
  base-size value emits restore packet V3 with zero expansion ranges; zero
  preserves V1/V2 packet selection.
* `restore_gate_timeout_ms` bounds gated guest repair. Zero selects the
  60-second default; a nonzero value is valid only with `restore_path`.
* `restore_ready_path` names an existing Unix domain socket on Linux or a
  `//./pipe/...` named pipe on Windows. `ResumeVM` writes and flushes exactly
  `OPENVMM_RESTORE_READY_V1\n` after all fatal restore startup work completes.
  For a gated restore, this occurs after guest repair succeeds and host input
  is re-enabled, while the restored vCPU remains stopped. Signaling failure makes `ResumeVM` fail and tears down the
  managed VM. The peer must accept and read concurrently with `ResumeVM`;
  Windows flush completion waits until the complete frame has been consumed.

On cold boot, `DevicesConfig.virtio_console` may configure one microVM Unix
socket or named-pipe endpoint. Listener mode recreates the path on restore.
Client mode is required and uses a five-second connection timeout
before vCPUs start. The device uses MMIO `0xd0002000`, IRQ 7, and selects
`hvc1`; its canonical path and policy become the stable restore attachment.

`DevicesConfig.virtiofs_config` may bind one HostFs attachment to the fixed
microVM slot. Set `tag` to `microvm`, supply `root_path`, and set
`guest_mount_target` to an absolute Linux path. `read_write=false` selects the
default read-only policy. Restoring an active attachment requires the exact
same canonical root path, tag, guest target, and access mode, and the live root
must retain its saved identity. A snapshot captured with the slot dormant may
omit the attachment or supply a new one; after `ResumeVM`, the guest explicitly
mounts tag `microvm`. The fixed device uses MMIO `0xd0001000`, IRQ 6, one
request queue, and no DAX window.

Standard-machine attachments reject the microVM mount fields, and microVM
attachments reject `read_only=true` before opening device resources. Use
`read_write` to select a microVM attachment's access mode.

Capture and restore paths are mutually exclusive. A successful capture halts
the managed source VM at the committed boundary and terminates the OpenVMM
source process; clients observe the transport closing. Restore creates a private
copy-on-write RAM view and leaves the VM paused until `ResumeVM`.
The readiness endpoint is a process-local orchestration attachment and is not
part of saved state. Each successful restore publishes one event; validation,
attachment, or worker-start failure publishes none.

`VMConfig.MICROVM` is numeric value 2, matching the microVM ABI version, and
accepts exactly 1, 2, 4, or 8 processors. Numeric value 1 is not a valid profile
and is rejected before host resources are opened. RPC construction is currently
blockless; role-bearing sandbox blocks remain CLI-only. Snapshot ABI and boot
layout remain value 2, which identifies the Linux-direct MP-table layout. Value
1 snapshots are unsupported.

The API has the same KVM/MSHV/WHP backend, no-block device, artifact integrity,
and security restrictions documented under [`--snapshot-destination`].

[`vmservice.proto`]: https://github.com/microsoft/openvmm/blob/main/openvmm/openvmm_ttrpc_vmservice/src/vmservice.proto
[`--snapshot-destination`]: ./cli.md
