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

[`vmservice.proto`]: https://github.com/microsoft/openvmm/blob/main/openvmm/openvmm_ttrpc_vmservice/src/vmservice.proto
