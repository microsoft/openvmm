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
* ShutdownVM
* WaitVM
* CapabilitiesVM
* PropertiesVM
* ModifyResource
* AddPcieDevice
* RemovePcieDevice
* AddVpciDevice
* RemoveVpciDevice
* Quit

## Virtio-console file output

`VmService` can write a virtio-console stream directly to a host file. Set the
console's `SerialBackend` to `SerialFile`:

```text
pcie {
  root_complexes {
    name: "rc0"
    root_ports {
      name: "console"
      attached { device { virtio { console {
        backend { file {
          path: "path/to/console.log"
          append: false
        } }
      } } } }
    }
  }
}
```

The file is opened while `CreateVM` processes the device configuration, so an
invalid path or permission failure causes `CreateVM` to fail. With `append`
unset or `false`, OpenVMM creates or truncates the file. With `append: true`,
OpenVMM preserves existing contents and appends new guest output.

The backend remains open for the console lifetime. OpenVMM does not rotate or
limit the file, so the management client owns retention. `SerialRelay` remains
available when the client needs an interactive or externally managed stream.
The backend writes synchronously to the file, without an intermediate socket,
named pipe, or background copy task. Successful writes complete before returning
to the serial device, so there is no background output to drain during teardown.
A slow or stalled file write can block the calling device thread. After
`TeardownVM` or `Quit` completes, the management client can read the completed
output. Writes do not force a disk synchronization and do not guarantee that the
output survives a host crash or power failure.
If a write fails, OpenVMM logs the error and disconnects the file backend.
Further console output is discarded; the backend does not automatically retry
or reopen the file.

## Graceful guest shutdown

`ShutdownVM` requests a cooperative guest power-off, non-forced by default.
It describes the operation, not a particular shutdown mechanism, and remains
separate from resource teardown.

`ShutdownVMRequest` has two fields:

* `mechanism`: `MECHANISM_UNSPECIFIED` (the default) selects Hyper-V IC, as
  does explicit `MECHANISM_HYPERV_IC`. The default is not automatic mechanism
  selection. Unknown values return `INVALID_ARGUMENT` without issuing a
  shutdown request.
* `force`: defaults to `false`. Setting it to `true` passes the guest-side
  force flag to the IC. Its interpretation depends on the guest; it may
  discard unsaved application data and does not guarantee shutdown completion.
  It never requests a host-side hard power-off. Linux guests may handle
  forced and non-forced IC requests identically.

An empty request preserves the default non-forced Hyper-V IC behavior.
For example, this JSON request explicitly selects IC and sets its force flag:

```json
{
  "mechanism": "MECHANISM_HYPERV_IC",
  "force": true
}
```

Success means the request was issued successfully, not that the guest will
shut down or has powered off. Mechanism-specific rejection errors are reported
when available.
Call `WaitVM` to observe the guest halt, then `TeardownVM` to release resources.
`WaitVM` observes any halt, not only a clean power-off; check the halt reason
reported by `PropertiesVM` when distinguishing shutdown from a crash matters.
`TeardownVM` and `Quit` do not request a graceful guest shutdown themselves.
Guest power-event policies still apply; retain the default shutdown `HALT`
policy for this sequence.

Currently, the only implemented mechanism is the Hyper-V shutdown integration
component (IC). The VM must be running, Hyper-V enlightenments and VMBus must
be enabled, and the guest must support and service the request. API-created
VMs expose the shutdown component when both features are enabled. This
implementation waits for the guest's acknowledgement before returning success.
ACPI power-button shutdown is not yet implemented by this RPC. If added, it
would not imply a guest acknowledgement or guarantee that the guest's
power-button policy initiates shutdown.

The request has a maximum wait of 30 seconds, shortened by the caller's
deadline. Set a shorter RPC timeout on the client, not in
`ShutdownVMRequest`. For example, a Rust `mesh_rpc` client can limit the
acknowledgement wait to five seconds:

```rust
let result = client
    .call()
    .timeout(Some(std::time::Duration::from_secs(5)))
    .start(
        vmservice::Vm::ShutdownVm,
        vmservice::ShutdownVmRequest::default(),
    )
    .await;
```

For gRPC clients, set the call's deadline/timeout using the client library.
This limits waiting for the shutdown request response, not completion of the
guest's shutdown sequence. Give `WaitVM` its own deadline for that wait.
Cancelling or timing out does not undo a request already delivered to the
guest and never triggers a fallback to another mechanism or hard power-off.

With the current Hyper-V IC implementation, a missing, paused, or halted VM,
disabled Hyper-V/VMBus, or an IC that reports not-ready returns
`FAILED_PRECONDITION`. An IC request already in progress returns
`ALREADY_EXISTS`; a guest rejection returns `INTERNAL` with the guest status
code. Caller cancellation and deadline expiry are reported as `CANCELLED` and
`DEADLINE_EXCEEDED`.

## VPCI hotplug

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

## Extending the API

When editing `vmservice.proto`, follow the
[Protocol Buffers enum conventions](../../../dev_guide/contrib/code.md#protocol-buffers-enums).

[`vmservice.proto`]: https://github.com/microsoft/openvmm/blob/main/openvmm/openvmm_ttrpc_vmservice/src/vmservice.proto
