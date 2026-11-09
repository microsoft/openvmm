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

## Snapshots (SaveVM and RestoreVM)

`SaveVM` is a compound operation that pauses the VM and writes `manifest.bin`,
`state.bin`, and `memory.bin` to the requested directory. The VM must use the
`MemoryConfig.backing_file_path` setting. The currently supported
implementation hard-links that backing file into the snapshot, so after a
successful save the VM can't be resumed and should be torn down. Each save
generates a unique snapshot ID (a GUID string) that is stored in
`manifest.bin` and returned in `SaveVMResponse.snapshot_id`.
Cancelling `SaveVM` only stops the caller from waiting; the save may still
complete.

`RestoreVM` creates a VM from a snapshot directory and a compatible
`VMConfig`. Device configuration is not stored in the snapshot and must be
supplied by the caller. `memory.bin` is used directly as mutable guest memory,
so `MemoryConfig.backing_file_path` must not be set, and a snapshot can be
restored only once. If `expected_snapshot_id` is provided, the restore fails
unless it matches the snapshot ID in `manifest.bin`. An empty value is treated
as not provided, so the ID isn't checked. The restored snapshot's
ID is returned in `RestoreVMResponse.snapshot_id`. Set `resume` to start the
restored VM immediately; otherwise it is created paused.

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

Success means IC acknowledgement or ACPI event injection, not that the guest
has halted. Neither mechanism falls back to the other or forcibly powers off
the VM.

See `ShutdownVM` and `ShutdownVMRequest` in [`vmservice.proto`] for prerequisites,
request fields, shutdown lifecycle.

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

## Hvsocket relay listeners

Use `ModifyResource` with `hvsocket_listener` to relay a host Unix socket to a
guest hvsocket service on VTL0. VMBus and hvsocket support must be available.
Specify either a `service_id` GUID or a `vsock_port`; both identify the same
service when the GUID is the vsock port's hvsocket mapping.

For example, this protobuf text-format `ModifyResourceRequest` adds a listener:

```text
type: ADD
hvsocket_listener {
    path: "path/to/service.sock"
    vsock_port: 50001
}
```

Clients send application data immediately. These listeners do not require a
hybrid-vsock `CONNECT` request and do not send an `OK` response.

Only one listener per service is supported. `ADD` fails if the path already
exists, including a stale socket file; it does not replace or remove that path.
The caller owns socket-file cleanup, including after a failed `ADD` that
successfully bound the path before failing to register the listener.

To remove a listener, send `REMOVE` with the same service selector; `path` is
ignored. Removal waits for the listening socket to close, but established relay
connections remain active. Removing an unknown service returns an error.
Remove the socket file yourself before reusing its path.

## Consomme NIC modifications

Use `ModifyResource` with `type: UPDATE` and `nic_config.nic_id` to modify
an existing Consomme NIC. Select `nic_config.consomme` and supply either
`ports`, `add_dns_record`, or `create_virtual_address`. DNS and virtual-address
operations cannot be combined with port modifications or used at VM creation.
They are not supported for other NIC backends.

For example, this protobuf text-format request adds a static DNS A record:

```text
type: UPDATE
nic_config {
    nic_id: "11111111-1111-1111-1111-111111111111"
    consomme {
        add_dns_record { name: "host.example" address: "192.0.2.1" }
    }
}
```

To allocate a guest-visible alias for a host IPv4 or IPv6 address, use
`create_virtual_address { destination: "127.0.0.1" }` instead. The allocated
address is returned in
`ModifyResourceResponse.virtual_address.virtual_address`. Repeating the
operation for the same destination returns the same alias.

DNS and port changes return a `ModifyResourceResponse` without a result.
Port forwarding retains its existing semantics: `UPDATE` binds the supplied
ports and `REMOVE` unbinds them. DNS and virtual-address operations only
support `UPDATE`.

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
width when an accelerated device is available during VM construction. Later
device attachments must support the resulting advertised width. A fixed width
cannot exceed the physical SMMU's width with acceleration.

`SmmuConfig.ssid_bits` selects a fixed SubstreamID width from 0 to 20.
Explicit zero disables substreams. Omitting the field selects `ssidsize=auto`:
zero in software mode, or the cold-plug host SMMU's width under acceleration.
Later device attachments must support the resulting advertised width. A fixed
width must not exceed the host's width under acceleration.

SMMU configuration cannot be changed at runtime. See [the CLI reference](cli.md)
and [Arm SMMUv3](../../emulated/iommu/smmuv3.md) for platform requirements.

## Extending the API

When editing `vmservice.proto`, follow the
[Protocol Buffers enum conventions](../../../dev_guide/contrib/code.md#protocol-buffers-enums).

## microVM machines

`VMConfig.machine_profile` `MICROVM`, or a `CreateVMRequest.microvm_snapshot`
with a `restore_path`, creates a microVM. The server converts the request to
the options of an `openvmm --machine microvm` command line, and then creates,
captures, and restores the microVM with the command line's code for those
options. Each field takes the value of its option, as the following table
shows, and that option's rules validate it, so a microVM request accepts
exactly the settings that the command line accepts, and an error names the
option.

| Field | Command-line option |
| --- | --- |
| `MemoryConfig.memory_mb` | `--memory size=` |
| `MemoryConfig.backing_file_path` | `--memory file=` |
| `ProcessorConfig.processor_count` | `--processors` |
| `DirectBoot.kernel_path`, `initrd_path`, and `kernel_cmdline` | `--kernel`, `--initrd`, and `--cmdline` |
| `VMConfig.cpu_profile` | `--cpu-profile` |
| `VMConfig.nested_virt`, `guest_watchdog`, and an SNP `isolation_config` | `--nested-virt`, `--guest-watchdog`, and `--isolation snp`, which a microVM refuses |
| `DevicesConfig.virtio_consoles` | `--virtio-console listen=` or `connect=`, or, with an empty `socket_path`, `none` |
| `DevicesConfig.virtiofs_config` | `--mount`, or `--mount-aggregate` with a `--mount-child` per `aggregate_children` entry |
| `VirtioFs.denied_paths`, `allowed_paths`, and `writable_paths` | `--mount-deny`, `--mount-allow`, and `--mount-write` |
| `VirtioFs.owner` | `--mount-owner` |
| `DevicesConfig.microvm_sandbox_blocks` | `--microvm-sandbox-block` |
| `MicrovmNetwork.address` | `--net` |
| `MicrovmNetwork.profile` | `--network-profile` |
| `MicrovmNetwork.egress`, `ingress`, and `host_loopback` | `--network-egress`, `--network-ingress`, and `--host-loopback` |
| `MicrovmNetwork.egress_allow` and `egress_deny` | `--network-egress-allow` and `--network-egress-deny` |
| `MicrovmNetwork.proxy` | `--network-proxy` |
| `MicrovmNetwork.host_loopback_forwards` | `--host-loopback-forward` |
| `MicrovmNetwork.allow_hosts`, `block_hosts`, and `allow_endpoints` | `--allow-host`, `--block-host`, and `--allow-endpoint` |
| `MicrovmSnapshotConfig.destination_path` | `--snapshot-destination` |
| `MicrovmSnapshotConfig.memory_capacity_bytes` | `--memory-capacity` |
| `MicrovmSnapshotConfig.snapshot_tier` | `--snapshot-tier` |
| `MicrovmSnapshotConfig.snapshot_generation_id` | `--snapshot-generation-id` |
| `MicrovmSnapshotConfig.snapshot_scratch_restore_mode` | `--snapshot-scratch-restore-mode` |
| `MicrovmSnapshotConfig.restore_path` | `--restore-snapshot` |
| `MicrovmSnapshotConfig.restore_ready_path` | `--restore-ready-path` |
| `MicrovmSnapshotConfig.restore_processor_count` | `--restore-processors` |
| `MicrovmSnapshotConfig.restore_memory_bytes` | `--restore-memory` |
| `MicrovmSnapshotConfig.restore_gate_timeout_ms` | `--restore-gate-timeout-ms` |
| `MicrovmSnapshotConfig.restore_generation_id` | `--restore-generation-id` |
| `MicrovmSnapshotConfig.restore_absent_scratch` | `--restore-absent-scratch` |
| `MicrovmSnapshotConfig.restore_network_policy` | `--restore-network-policy` |
| `MicrovmSnapshotConfig.restore_activation_record_path` | `--restore-activation-record` |

A string field takes its option's syntax: `MicrovmNetwork.address` is
`IPv4/PREFIX`, such as `10.0.0.2/24`, each egress rule is
`CIDR[:tcp[:PORT[-PORT]]|:udp[:PORT[-PORT]]|:icmp]`, `proxy` and each
`allow_endpoints` entry `IPv4:PORT`, and each `allow_hosts` or `block_hosts`
entry `IPv4[/PREFIX]`. An enumeration's unspecified value, a zero number, an
empty string, and a false flag leave the option out, so that its default
applies. A storage generation is the 16 bytes of the option's 32 hexadecimal
characters. A sandbox block's `role` is its option's `ROLE`, and its
`backend` and `read_only` its `DISK` and `,ro`: the distro layer must be
read-only and scratch writable, and a microVM that captures or restores a
snapshot requires an unformatted raw `FileDisk` without direct I/O. An
aggregate `VirtioFs` leaves `root_path` empty and `read_write` false, since
the aggregate's root is read-only, and names its children's access modes and
kinds instead; with `--mount-aggregate`, every access-policy path is absolute.
The protocol adds its own rules on which fields a request combines: a restore
field requires `restore_path`, a capture field `destination_path`, the
`allow_hosts`, `block_hosts`, and `allow_endpoints` lists exclude one another,
and a microVM takes at most one `virtiofs_config` and one `virtio_consoles`
entry, no other `DevicesConfig` device, and only serial port 0, portb, which
receives the guest's output and is never read.

The fork's fields of upstream's messages follow upstream's protobuf field
numbers, so that bindings generated from this fork's `vmservice.proto` also
encode upstream's requests: `VMConfig.machine_profile` and `cpu_profile` take
fields 23 and 24, `MemoryConfig.backing_file_path` field 10,
`DevicesConfig.microvm_sandbox_blocks` and `microvm_network` fields 7 and 8,
and `VirtioFs.guest_mount_target` to `aggregate_children` fields 4 to 10.
Standard-machine requests reject these fields, and microVM attachments reject
`VirtioFs.read_only=true` before opening device resources; use `read_write`
to select a microVM attachment's access mode.

`VMConfig.MICROVM` is numeric value 1 and accepts exactly 1, 2, 4, or 8
processors. Snapshots record microVM ABI value 1 and boot layout value 1,
which identifies the Linux-direct MP-table layout, and restore refuses a
snapshot of any other value. A snapshot captured over the management RPC
restores from the command line, and the reverse, as long as the restore
repeats the settings that the command line's restore requires.

## microVM snapshots

`CreateVMRequest.microvm_snapshot` exposes the microVM capture and
restore flow over both transports:

* `destination_path` configures guest-requested capture. Supply a microVM
  configuration, including `DirectBoot` kernel/initrd files, memory, and the
  processor count. The path must not exist. Before quiescing, the source
  delivers the serial port 0 (portb) output the guest wrote before its
  request, so the endpoint receives it before the source terminates. The
  capture has no deadline, from gating host input through the commit; the
  client bounds it by ending the server.
  `memory_capacity_bytes` optionally reserves an immutable, 128-MiB-aligned
  RAM capacity while keeping the configured base memory as the exact
  `memory.bin` payload and initial Linux direct e820 map. Without
  `MemoryConfig.backing_file_path`, the server backs guest RAM with a file
  that it creates next to the destination and moves into the snapshot; with
  one, the capture copies that file, which it leaves in place.
  `snapshot_tier` is required exactly for a microVM with sandbox blocks: a
  `platform` capture records the blocks unbound and its scratch fresh, and
  its kernel command line, `kernel_cmdline` included, may hold only the
  tokens that all its clones share, so it leaves the filesystem slot dormant;
  the `workload-start` and `instance-checkpoint` tiers bind the blocks to
  `snapshot_generation_id` and publish the paired scratch, whose restore-time
  materialization `snapshot_scratch_restore_mode` records.
* `restore_path` selects manifest-authoritative restore. `config` may be
  absent, or may contain only the matching microVM profile, an optional exact
  processor-count assertion, a `cpu_profile` that the restore accepts (see
  below), serial port 0 host attachment, the guest power actions, and the
  restore-time devices that the command line's restore takes: the
  `virtio_consoles` and `virtiofs_config` attachments, the sandbox blocks, and
  the network policy, without an address, which the restore takes from the
  snapshot. Guest-visible boot, memory, processor topology, other device,
  NUMA, and PCIe fields are rejected. A saved listener is reconstructed from
  the manifest; a saved client requires the matching path configuration. A
  restore attaches the read-only layers again, and a scratch device, or
  `restore_absent_scratch`, only for a platform snapshot, whose scratch is
  fresh; the paired scratch of a later tier comes from the snapshot, and its
  restore requires `restore_generation_id`. A networked restore repeats the
  snapshot's network policy, which `restore_network_policy` replaces for the
  NIC template of a platform snapshot or of a snapshot without a tier.
* Every restore delivers the restore packet with fresh entropy, and every
  microVM process receives a fresh, non-serialized 16-byte generation ID
  through the fixed portb selector. On restore, the ID is the first 16 bytes
  of the restore packet's entropy.
* `restore_processor_count` requests restore-time activation of the contiguous
  VP prefix `0..count-1`. Zero requests no activation. A nonzero value is
  valid only for a snapshot that advertises processor activation, implies the
  post-restore gate, and must satisfy the snapshot's boot-online and immutable
  capacity bounds. `ProcessorConfig.processor_count`, when present, remains an
  exact capacity assertion. On MSHV, a nonzero value instantiates and binds
  only that VP prefix, as for `--restore-processors`; MSHV restores without a
  target, and all KVM and WHP restores, instantiate the full VP capacity.
* `restore_memory_bytes` selects a 128-MiB-aligned total RAM target between the
  snapshot base and immutable capacity. Zero selects the base. Expansion uses
  fresh private zeroed backing, implies the post-restore repair gate, and is
  rejected for snapshots without a RAM capacity contract. A nonzero value sets
  the memory-target flag of the restore packet, with the expansion
  ranges (none at the base size); zero leaves the flag clear.
* `restore_gate_timeout_ms` bounds gated guest repair. Zero selects the
  60-second default; a nonzero value is valid only with `restore_path`. The
  restore of a snapshot with a tier, a restore with a processor or memory
  target, and a restore that replaces the network policy of a snapshot without
  a tier gate host input until the guest acknowledges the restore.
* `restore_ready_path` names an existing Unix domain socket on Linux or a
  `//./pipe/...` named pipe on Windows. `ResumeVM` writes and flushes exactly
  `OPENVMM_RESTORE_READY_V1\n` after all fatal restore startup work completes.
  For a gated restore, this occurs after guest repair succeeds and host input
  is re-enabled, while the restored vCPU remains stopped. Signaling failure makes `ResumeVM` fail and tears down the
  managed VM. The peer must accept and read concurrently with `ResumeVM`;
  Windows flush completion waits until the complete frame has been consumed.

On cold boot, `DevicesConfig.virtio_consoles` may hold one entry, which
configures a microVM Unix socket or named-pipe endpoint, or, with an empty
`socket_path` and `connect` false, the console with no endpoint, which
discards the guest's output, as `--virtio-console none` does. A console takes
no TCP endpoint, so the server never opens one, and a restore refuses a
snapshot whose console recorded one. Listener mode recreates the path on
restore, and a restore may attach a listener at a fresh path to a console that
has no endpoint, which then keeps discarding the guest's output while no
client is connected.
Client mode is required and uses a five-second connection timeout
before vCPUs start. The device uses MMIO `0xd0002000`, IRQ 18, and selects
`hvc1`; its canonical path and policy become the stable restore attachment.

`DevicesConfig.virtiofs_config` may bind one HostFs attachment to the fixed
microVM slot. Set `tag` to `microvm` and `guest_mount_target` to an absolute
Linux path, and supply either `root_path`, one host directory, or
`aggregate_children`, several host directories or regular files.
`read_write=false` selects the default read-only policy of a single
directory, and the access-policy paths and `owner` apply as the command line's
options do. Restoring an active attachment requires the exact same canonical
roots, tag, guest target, access modes, policy paths, and owner, and the live
roots must retain their saved identities. A snapshot captured with the slot
dormant may omit the attachment or supply a new one, and only a platform
snapshot an aggregate; after `ResumeVM`, the guest explicitly mounts tag
`microvm`. The fixed device uses MMIO `0xd0001000`, IRQ 17, one request queue,
and no DAX window.

Capture and restore paths are mutually exclusive. A successful capture halts
the managed source VM at the committed boundary and terminates the OpenVMM
source process; clients observe the transport closing. Restore creates a private
copy-on-write RAM view and leaves the VM paused until `ResumeVM`.
It completes the restoration of every device first, so invalid device-private
state, or a saved virtio-fs object that OpenVMM cannot reopen, fails the
restore rather than `ResumeVM`.
The readiness endpoint is a process-local orchestration attachment and is not
part of saved state. Each successful restore publishes one event; validation,
attachment, or worker-start failure publishes none. `ResumeVM` waits for the
devices to start their queues before any vCPU runs: a queue that fails to start
fails it, and the restore then publishes no event, as
[Snapshots](../../../user_guide/openvmm/snapshots.md#restoring-a-snapshot)
describes.

`VMConfig.cpu_profile` selects the microVM's CPU profile as
[`--cpu-profile`](cli.md) does, with the same values and checks: `auto`, the
default, which an empty value also selects, for the built-in profile of the
host CPU's generation; `host` for a development profile derived from this
host; or a built-in profile ID. A cold boot fails as the command line's does
for a profile that the host cannot boot, such as with
`E_PROFILE_UNSUPPORTED` where the backend cannot present the profile that
`auto` selected. A restore uses the snapshot's profile and accepts only
`auto`, the snapshot's profile ID, or, for a snapshot of a host profile,
`host`; any other value fails with `E_PROFILE_UNKNOWN`. A standard VM rejects
a nonempty `cpu_profile`.

The API has the same KVM/MSHV/WHP backend, sandbox block, artifact integrity,
and security restrictions documented under [`--snapshot-destination`]. The
management API cannot restore a snapshot with a control console, which needs
an authenticated broker attachment that it cannot provide.

[`vmservice.proto`]: https://github.com/microsoft/openvmm/blob/main/openvmm/openvmm_ttrpc_vmservice/src/vmservice.proto
[`--snapshot-destination`]: ./cli.md
