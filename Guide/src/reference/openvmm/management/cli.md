# CLI

```admonish danger title="Disclaimer"
The following list is not exhaustive, and may be out of date.

The most up to date reference is always the [code itself](https://openvmm.dev/rustdoc/linux/openvmm_entry/struct.Options.html),
as well as the generated CLI help (via `cargo run -- --help`).
```

* `--processors <COUNT>`: The number of processors. Defaults to 1.
* `--memory <SPEC>`: Configure guest RAM. Defaults to `size=1G`.
  `SPEC` can be a size-only shorthand, such as `--memory 4G`, or a
  comma-separated key/value list:

  ```bash
  --memory size=4G,shared=on,prefetch=off
  ```

  The keys below select the guest RAM **memory backing**. For an explanation
  of shared vs. private memory, prefetch, huge pages, and file-backed RAM —
  and how to choose between them — see
  [Memory Backing](../../architecture/openvmm/memory-backing.md).

  Supported keys:
  * `size=<SIZE>` - guest RAM size. Sizes accept `K`, `M`, `G`, and
    `T` suffixes, optionally followed by `B`.
  * `shared[=on|off]` - use shared file-backed guest RAM. The default is
    `on`; `off` uses private anonymous memory.
  * `prefetch[=on|off]` - pre-populate guest RAM mappings up front.
    Only has an effect under WHP; a no-op on KVM/mshv.
  * `thp[=on|off]` - mark guest RAM (shared or private) as Transparent Huge
    Page eligible. Linux-only, best-effort, and on by default; pass `thp=off` to
    opt out.
  * `hugepages[=on|off]` - allocate guest RAM from explicit large/huge pages
    (Linux hugetlb pages or a Windows `SEC_LARGE_PAGES` section). Requires
    shared memory.
  * `hugepage_size=<SIZE>` - request a specific large-page size, such
    as `2MB` or `1GB`. Requires `hugepages=on`; defaults to 2 MB. On
    Windows only 2 MB is supported.
  * `file=<PATH>` - use an existing file as the guest RAM backing file.
    This is used by snapshots.

  Examples:

  ```bash
  --memory 4G
  --memory size=64GB,hugepages=on,hugepage_size=2MB
  --memory size=4G,file=path/to/memory.bin
  --memory size=4G,thp=off
  ```
* `--hv`: Exposes Hyper-V enlightenments. VMBus is enabled by default
  when `--hv` is active; pass `--no-vmbus` to suppress VMBus while keeping
  enlightenments.
* `--no-vmbus`: Disables the VMBus server and all VMBus devices, even when
  `--hv` or `--uefi` is active. The guest boots using only standard PCIe
  devices and virtio transports. Incompatible with `--disk`, `--pcat`,
  `--vtl2`, and VMBus serial options.
* `--hypervisor <SPEC>`: Select a specific hypervisor backend, optionally with
  backend-specific parameters. The format is `name` or `name:key=val,key,...`.
  Available backends: `whp` (Windows), `kvm` (Linux), `mshv` (Linux,
  `x86_64` guests only), `hvf` (macOS). When omitted, OpenVMM
  auto-detects the best available backend.

  WHP accepts the following parameters (x86_64 guests only):
  * `user_mode_apic` — use the user-mode APIC emulator instead of WHP's
    in-hypervisor APIC
  * `no_enlightenments` — disable in-hypervisor Hyper-V enlightenment support

  Examples:
  ```bash
  --hypervisor whp
  --hypervisor whp:user_mode_apic
  --hypervisor whp:user_mode_apic,no_enlightenments
  --hypervisor kvm
  ```
* `--nested-virt`: Expose hardware virtualization (VMX/SVM) to the guest so it
  can run its own hypervisor (Hyper-V, KVM, etc.). Only supported on `x86_64`,
  and only by backends that support nested virtualization (currently WHP and
  KVM); requesting it with a backend that does not support it fails early. The
  host must expose virtualization extensions to the VM running OpenVMM. When
  enabled, a guest may detect nested virtualization and turn on features such
  as Virtual Secure Mode (VSM), which can hurt performance and interfere with
  VMBus devices; nested virt cannot currently be combined with `--hv`/VMBus or
  `--hypervisor whp:user_mode_apic`.
* `--uefi`: Boot using `mu_msvm` UEFI
* `--uefi-firmware <FILE>`: Path to the UEFI firmware file (`MSVM.fd`). When `--uefi` is specified, this option is required only if you do not set the environment variable `OPENVMM_UEFI_FIRMWARE` (or the architecture-specific variants `X86_64_OPENVMM_UEFI_FIRMWARE`, or `AARCH64_OPENVMM_UEFI_FIRMWARE`). If omitted, the default is read from `OPENVMM_UEFI_FIRMWARE` first, then falls back to the architecture-specific variables.
* `--pcat`: Boot using the Microsoft Hyper-V PCAT BIOS
* `--vmbus-scsi id=<name>[,sub_channels=<N>][,vtl2]`: Creates a
  named VMBus SCSI controller. Use with `--disk ...,on=<name>` to
  attach disks.
* `--disk file:<DISK>,on=<name>`: Attaches a disk to the named
  controller. The `DISK` argument can be:
  * A flat binary disk image
  * A VHD file with an extension of .vhd (Windows host only)
  * A VHDX file with an extension of .vhdx (Windows host only)

  On Linux, raw files and block devices use the `disk_blockdevice` backend
  (io_uring-based async I/O) by default. Append `;direct` to the path to
  bypass the OS page cache, e.g. `--disk file:/dev/sdb;direct,on=scsi0`.
* `--numa <PARAMS>`: Configure a guest NUMA node (repeatable, one per
  node). Mutually exclusive with `--memory`. Each `--numa` specifies one
  guest NUMA node with its own memory backing and optional VP assignment.

  Supported keys (in addition to all `--memory` keys except `file`):
  * `host_numa_node=<N>` - bind memory allocation to host NUMA node N
  * `vps=<LIST>` - explicit VP indices for this node. Uses bracket syntax
    with comma-separated indices and dash ranges: `vps=[0,1]`,
    `vps=[0-3]`, `vps=[0,1,4-5]`. When omitted, VPs are assigned by
    round-robin sockets across nodes. An empty list, `vps=[]`, declares a
    CPU-less node (e.g. a generic-initiator target); unlike a non-empty
    list, it may be combined with nodes that omit `vps`.

  Examples:

  ```bash
  --numa size=2G --numa size=2G
  --numa size=2G,host_numa_node=0 --numa size=2G,host_numa_node=1
  --numa size=2G,hugepages=on,vps=[0,1] --numa size=2G,vps=[2,3]
  --numa size=2G,vps=[0-3] --numa size=2G,vps=[4-7]
  ```

  See [NUMA Topology](../../architecture/openvmm/numa.md) for details.

* `--numa-distance <SRC:DST:DIST>`: Specify inter-node NUMA distance
  (repeatable). `SRC` and `DST` are 0-based node indices, `DIST` is
  10–255 (10 = local, 255 = unreachable). Each direction must be specified
  explicitly.

  ```bash
  --numa-distance 0:1:30 --numa-distance 1:0:30
  ```

* `--private-memory`, `--prefetch`, `--thp`, and
  `--memory-backing-file <PATH>`: Deprecated aliases for `--memory`
  parameters. Prefer `shared=off`, `prefetch=on`, `thp=on`, and
  `file=<PATH>`.
* `--pidfile <PATH>`: Write the process ID to the specified file on startup,
  and remove it on clean exit. If the process is killed with `SIGKILL` or
  crashes, the pidfile is not removed — consumers should verify the PID is
  still alive. No file locking is performed; concurrent launches with the same
  pidfile path will overwrite each other. Not written for short-lived utility
  modes such as `--write-saved-state-proto`.
* `--nic`: Exposes a NIC using the Consomme user-mode NAT.
* `--gfx`: Enable a graphical console over VNC (see below)
* `--vnc-port <PORT>`: VNC server port (default: 5900)
* `--vnc-listen <ADDRESS>`: VNC server bind address (default: `127.0.0.1`).
  Use `0.0.0.0` for all IPv4 interfaces, or `::` for dual-stack IPv4+IPv6.
* `--vnc-max-clients <COUNT>`: Maximum concurrent VNC clients (default: 16).
  Each client uses ~8MB for framebuffer buffers.
* `--vnc-evict-oldest`: When the client limit is reached, disconnect the oldest
  client instead of rejecting the new connection. Useful for admin takeover.
* `--virtio-9p`: Expose a virtio 9p file system. Uses the format `tag,root_path`, e.g. `myfs,C:\\`.
  The file system can be mounted in a Linux guest using `mount -t 9p  -o trans=virtio tag /mnt/point`.
  You can specify this argument multiple times to create multiple file systems.
* `--virtio-fs`: Expose a virtio-fs file system. The format is the same as `--virtio-9p`. The
  file system can be mounted in a Linux guest using `mount -t virtiofs tag /mnt/point`.
  You can specify this argument multiple times to create multiple file systems.
* `--virtio-rng`: Add a virtio entropy (RNG) device, exposing `/dev/hwrng` in the Linux guest.
  The guest kernel must have `CONFIG_HW_RANDOM_VIRTIO` enabled.
* `--virtio-rng-bus <BUS>`: Select the bus for the virtio-rng device (`auto`, `mmio`, `pci`, `vpci`).
  Defaults to `auto`.
* `--vhost-user <SOCKET_PATH>,type=<TYPE>[,tag=<NAME>][,num_queues=<N>][,queue_size=<N>][,pcie_port=<PORT>]`: Attach a
  vhost-user device backed by an external process over a Unix socket (Linux
  only). The backend process must already be listening on `SOCKET_PATH`.
  Supported `type` values: `blk`, `fs`. For `type=fs`, `tag=<NAME>` is required
  and specifies the mount tag exposed to the guest (max 36 bytes).
  `num_queues` and `queue_size` control the queue layout (defaults: blk
  num_queues=1/queue_size=128, fs num_queues=1/queue_size=1024).
  Alternatively, use `device_id=<N>` instead of `type=` to specify the numeric
  virtio device ID directly, with `queue_sizes=[N,N,N]` for per-queue sizes.
  Examples:
  ```sh
  --vhost-user /tmp/vhost-blk.sock,type=blk
  --vhost-user /tmp/vhost-blk.sock,type=blk,num_queues=4,queue_size=512
  --vhost-user /tmp/vhost-blk.sock,type=blk,pcie_port=rp0
  --vhost-user /tmp/virtiofsd.sock,type=fs,tag=myfs
  --vhost-user /tmp/virtiofsd.sock,type=fs,tag=myfs,num_queues=2,queue_size=1024
  --vhost-user /tmp/vhost.sock,device_id=26,queue_sizes=[256,256]
  ```

Serial devices can be configured to appear as different devices inside the guest:

* `--com1/com2 <BACKEND>`: Configure a COM port serial device.
* `--com1 debugger-mode:<BACKEND>`: Prefix any COM port binding with
  `debugger-mode:` to run that port in debugger mode for WinDbg kernel
  debugging over serial (KD), e.g. `--com1 debugger-mode:listen=<PATH>` or
  `--com1 debugger-mode:listen=tcp:<IP>:<PORT>`. In this mode OpenVMM keeps that
  port's backend drained and may drop bytes instead of applying backpressure, so
  the KD transport does not deadlock across guest resets or reboots; KD recovers
  dropped bytes with its own retransmission. Debugger mode is chosen
  independently per COM port, so one port can talk to WinDbg while another
  behaves normally.
* `--virtio-console <BACKEND>`: Expose a virtio console device (appears as
  `/dev/hvc0` inside the guest).

The `BACKEND` argument is the same for all serial devices:

  * `none`: Serial output is dropped.
  * `console`: Serial input is read and output is written to the console.
  * `stderr`: Serial output is written to stderr.
  * `listen=PATH`: A named pipe (on Windows) or Unix socket (on Linux) is set
      up to listen on the given path. Serial input and output is relayed to this
      pipe/socket.
  * `listen=tcp:IP:PORT`: As with `listen=PATH`, but listen for TCP
      connections on the given IP address and port. Typically IP will be
      127.0.0.1, to restrict connections to the current host.

## Guest power events

By default OpenVMM keeps running when the guest powers itself off, hibernates,
or triple-faults: the virtual processors stop, but the VMM process stays up so
you can inspect the VM or restart it from the
[interactive console](./interactive_console.md). A guest-requested reset reboots
the VM in place, as does a guest watchdog timeout when `--guest-watchdog` is
enabled.

Four flags override what happens on each guest power event, so a supervising
process can treat the OpenVMM process lifetime as the VM lifetime. Each takes a
`reset` (reboot in place), `halt` (stop the processors but keep the VMM process,
as above), or `exit` (exit the VMM process) action. The `exit` action may carry a
status code as `exit:<code>` (0-255); a bare `exit` uses 0:

* `--guest-reset-action <reset|halt|exit[:<code>]>` (default `reset`): the guest requested
  a reset.
* `--guest-shutdown-action <reset|halt|exit[:<code>]>` (default `halt`): the guest powered
  off or hibernated.
* `--guest-crash-action <reset|halt|exit[:<code>]>` (default `halt`): the guest
  triple-faulted. The fault registers are written to the trace log.
* `--guest-watchdog-action <reset|halt|exit[:<code>]>` (default `reset`): the guest
  watchdog timer expired without being petted (requires `--guest-watchdog`).

A bare `exit` exits with status 0; `exit:<code>` exits with that code instead, so
a supervisor can tell the exit reasons apart.

`--disable-frontpage`: when booting UEFI, power the VM off instead of showing the
firmware frontpage (the menu shown when there is no bootable device). Combined
with `--guest-shutdown-action exit`, a guest with no boot device exits the VMM.
Requires `--uefi`.

## PCIe Device Support

OpenVMM can emulate a PCI Express topology using `--pcie-root-complex` and
`--pcie-root-port`. Devices that support the `pcie_port=` option can be
attached to a root port to appear as PCIe devices in the guest.

### Setting up a PCIe topology

```sh
# Create a root complex and root port
--pcie-root-complex rc0 --pcie-root-port rc0:rp0
```

`--pcie-root-complex` accepts optional comma-separated options after the root
complex name:

```sh
--pcie-root-complex rc0,segment=0,start_bus=0,end_bus=255
```

- `segment=<N>`: PCIe segment number for the root complex.
- `start_bus=<N>` and `end_bus=<N>`: inclusive bus range assigned to that
  root complex.
- `low_mmio=<SIZE>` and `high_mmio=<SIZE>`: low/high MMIO window sizes.
- `low_mmio_base=<ADDR>` and `high_mmio_base=<ADDR>`: pin the low/high
  MMIO window to a fixed base address instead of letting the VM topology
  allocate it dynamically. Used with `preserve_bars` for P2P DMA.
- `preserve_bars`: treat non-zero BAR values found during PCI probing as
  pinned addresses (GPA = HPA). Required for peer-to-peer DMA between
  VFIO passthrough devices without ATS.
- `hdm=<SIZE>`: CXL HDM decoder MMIO window size (CFMWS window). Default
  is `1G`.
- `hdm_window_restrictions=<MASK>`: CFMWS window restrictions bitmask
  (`u16`, decimal or `0x`-prefixed hex). Default is `0x1`
  (`DEVICE_COHERENT`, bit 0 set).
  Defined bits:
  0: device coherent
  1: host-only coherent
  2: volatile
  3: persistent
  4: fixed device configuration
  5: BI
  Bits 15:6 are reserved and rejected.
- `node=<N>`: NUMA node affinity for this root complex. The guest sees
  this via the ACPI `_PXM` object. When omitted, no `_PXM` is emitted
  and the guest uses its default allocation policy.

### Root port and switch options

`--pcie-root-port` accepts optional comma-separated options after the port
name:

```sh
--pcie-root-port rc0:rp0,hotplug,acs=0x005f,cxl
```

- `addr=<dev>[.<fn>]`: places the root port at a fixed device/function on
  its bus. `dev` is 0-31 and the optional `fn` is 0-7 (both decimal or
  `0x`-prefixed hex). When omitted, the port is assigned the lowest
  available devfn. Ports are assigned in order, so an explicit `addr` that
  collides with an already-assigned port is an error.
- `hotplug`: enables hotplug support for that root port.
- `acs=<mask>`: sets the Access Control Services capability mask for the
  root port. The value can be decimal or hexadecimal. Default is `0x005f`.
  Use `acs=0` to disable ACS for a root port.
- `cxl`: marks the root port as CXL-capable.

`--pcie-switch` accepts optional comma-separated options as well:

```sh
--pcie-switch rp0:switch0,num_downstream_ports=4,acs=0x005f
```

- `num_downstream_ports=<N>`: number of downstream ports for the switch.
- `hotplug`: enables hotplug support on all downstream switch ports.
- `acs=<mask>`: ACS capability mask requested for downstream switch ports.
  The upstream switch port does not expose ACS. Default is `0x005f`.
  Use `acs=0` to disable ACS for switch downstream ports.

### Generic initiators

A generic initiator is a device that originates memory accesses but has no
CPUs of its own — for example a GPU or accelerator with its own coherent
memory. Declaring one emits an SRAT Generic Initiator Affinity structure that
tells the guest which NUMA node the device belongs to, so the guest can
account for access latency and online the device's memory on the right
proximity domain.

Use `--pcie-generic-initiator` to mark the device directly behind a PCIe port
as a generic initiator for a NUMA node:

```sh
# Create a CPU-less, memory-less NUMA node and a root port, then declare the
# device behind the root port as a generic initiator for that node.
--numa size=2G --numa size=0,vps=[] \
  --pcie-root-complex rc0 --pcie-root-port rc0:rp0 \
  --pcie-generic-initiator port=rp0,node=1
```

- Syntax: `port=<port_name>,node=<node>`.
- `port=<port_name>` may be a root port name or a switch downstream port name
  (e.g. `switch0-downstream-1`); it is resolved against the live topology.
- `node=<node>` is the NUMA node the device is a generic initiator for, and
  should typically be a CPU-less and memory-less node created via `--numa`.


### Attaching devices to PCIe

Several device types support the `pcie_port=<name>` option to attach to a
PCIe root port. The syntax varies slightly between device types:

**Disks** (comma-separated option): `--nvme-pci` + `--disk`, `--virtio-blk`

```sh
--virtio-blk file:/path/to/disk.raw,pcie_port=rp0
--nvme-pci id=nvme0,pcie_port=rp0 --disk file:/path/to/disk.raw,on=nvme0
```

**CXL test endpoint** (comma-separated option): `--cxl-test`

```sh
--cxl-test mem:1G,pcie_port=rp0
```

`--cxl-test` creates a CXL Type-3 test endpoint with one component-register
BAR.
The `mem:<len>` value sets the emulated HDM size and allocates backing memory.

**NICs** (colon-prefixed): `--net`, `--virtio-net`, `--mana`

```sh
--virtio-net pcie_port=rp0:tap:tap0  # TAP is Linux-only
--net pcie_port=rp0:consomme
--mana pcie_port=rp0:tap:tap0        # TAP is Linux-only
```

**Filesystems and other virtio devices** (colon-prefixed):
`--virtio-fs`, `--virtio-fs-shmem`, `--virtio-9p`, `--virtio-pmem`

```sh
--virtio-fs pcie_port=rp0:myfs,/path/to/share
--virtio-fs-shmem pcie_port=rp0:myfs,/path/to/share
--virtio-9p pcie_port=rp0:myfs,/path/to/share
--virtio-pmem pcie_port=rp0:/path/to/file
```

For `--virtio-rng` and `--virtio-console`, use their separate PCIe port flags:

```sh
--virtio-rng --virtio-rng-pcie-port rp0
--virtio-console console --virtio-console-pcie-port rp0
```

**vhost-user devices** (comma-separated option, Linux only): `--vhost-user`

```sh
--vhost-user /tmp/vhost-blk.sock,type=blk,pcie_port=rp0
--vhost-user /tmp/virtiofsd.sock,type=fs,tag=myfs,pcie_port=rp0
```

**VFIO device assignment** (Linux only): `--vfio` (and optional `--iommu`)

```sh
# Legacy VFIO group/container path:
--vfio host=0000:01:00.0,port=rp0

# Modern VFIO cdev + iommufd path (Linux >= 6.6):
--iommu id=iommu0 --vfio host=0000:01:00.0,port=rp0,iommu=iommu0

# Pin BAR0 to its physical address for P2P DMA:
--vfio host=0000:01:00.0,port=rp0,bar0=pt
```

### SMMU (aarch64 only)

`--smmu` enables an emulated Arm SMMUv3 IOMMU for a named PCIe root
complex. The flag is repeatable — use one `--smmu` per root complex that
should have an SMMU. Devices behind a covered root complex get software
IOVA→GPA translation for DMA and MSI addresses.

The syntax is a comma-separated key/value list:

```sh
--smmu rc=<name>[,accel][,oas=auto|N]
```

- `rc=<name>` (required): the PCIe root complex this SMMU covers.
- `accel` (optional): enable hardware-accelerated (iommufd-nested)
  translation, delegating stage-1 walks to the host IOMMU. See the note
  below — this is not yet wired up and currently fails at startup.
- `oas=auto|N` (optional): the SMMU's output address size (OAS) in bits.
  `auto` (the default) advertises a fixed 48 bits, which covers typical
  configurations. Very large RAM or an explicitly pinned high MMIO/ECAM
  base can exceed this, requiring an explicit larger `oas=` (e.g. `oas=52`).
  A fixed `N` must be one of the SMMUv3-legal encodings: `32`, `36`, `40`,
  `42`, `44`, `48`, or `52`.

```sh
# Enable an emulated SMMU on root complex rc0
--smmu rc=rc0

# Multiple root complexes
--smmu rc=rc0 --smmu rc=rc1

# Pin the output address size to 48 bits
--smmu rc=rc0,oas=48
```

```admonish warning
`accel` is accepted by the parser but the acceleration backend is not yet
implemented: requesting it fails during SMMU setup rather than silently
falling back to emulated translation.

VFIO devices therefore cannot currently be placed behind an SMMU-covered
root complex, because host passthrough requires the (not-yet-available)
iommufd nested translation path.
```

### AMD IOMMU (x86_64 only)

`--amd-iommu <RC_NAME>` enables an emulated AMD-Vi IOMMU for the named
root complex. The flag is repeatable — use one `--amd-iommu` per root
complex that should have an IOMMU. Devices behind a covered root complex
get software IOVA→GPA translation for DMA and interrupt remapping.

```sh
# Enable AMD IOMMU on root complex rc0
--amd-iommu rc0
```

Mutually exclusive with `--intel-vtd` within the same VM (only one x86
IOMMU type can be active).

### Intel VT-d (x86_64 only)

`--intel-vtd <RC_NAME>` enables an emulated Intel VT-d IOMMU for the
named root complex. The flag is repeatable — use one `--intel-vtd` per
root complex that should have an IOMMU. The guest discovers VT-d units
via the ACPI DMAR table (not PCI config space).

```sh
# Enable Intel VT-d on root complex rc0
--intel-vtd rc0

# Multiple root complexes
--intel-vtd rc0 --intel-vtd rc1
```

Mutually exclusive with `--amd-iommu` within the same VM (only one x86
IOMMU type can be active).
