# CLI

```admonish note title="CLI compatibility and reference"
The CLI is not a stable compatibility interface and may change between
releases. This page summarizes OpenVMM's command-line options. The generated
`openvmm --help` output is authoritative for the binary being run, and the
[`Options` rustdoc](https://openvmm.dev/rustdoc/linux/openvmm_entry/struct.Options.html)
describes the source definitions.
```

* `--version`, `-V`: Print the OpenVMM build identity and exit. `-V` prints
  the concise identity. `--version` also prints the upstream product version,
  full Git revision when available, and build target. An ordinary checkout
  reports `MAJOR.MINOR.PATCH+g<SHORT_REVISION>`. This includes an exact
  checkout of an `openvmm-vMAJOR.MINOR.PATCH` release tag. A checkout detected
  with tracked changes appends `.dirty`; staged changes refresh this reliably,
  while an unstaged-only transition may remain cached until another
  build-script input changes. A Git-free source tree reports
  `MAJOR.MINOR.PATCH`. On Windows, the executable's `VERSIONINFO` uses the
  product version as `MAJOR.MINOR.PATCH.0`.
* `--processors <COUNT>`: The number of processors. Defaults to 1.
* `--machine <PROFILE>`: Select the guest-visible machine contract. The
  default is `standard`. `microvm` selects the ACPI-free x86-64 Linux direct
  microVM, which
  runs on KVM, MSHV, or WHP with exactly 1, 2, 4, or 8 vCPUs. On
  Linux, auto-detection prefers MSHV when `/dev/mshv` is available and falls
  back to KVM:

  ```bash
  openvmm --machine microvm --hypervisor kvm \
    --kernel vmlinux --initrd initramfs.cpio.gz
  openvmm --machine microvm --hypervisor mshv \
    --kernel vmlinux --initrd initramfs.cpio.gz
  openvmm --machine microvm --hypervisor whp \
    --kernel vmlinux --initrd initramfs.cpio.gz
  openvmm --machine microvm --processors 8 --hypervisor whp \
    --kernel vmlinux --initrd initramfs.cpio.gz
  ```

  The kernel must be an uncompressed ELF64 image. The profile owns the base command line
  (`earlycon=xe9 console=hvc0 reboot=t panic=-1`) and switches the primary
  console to `hvc1` when `--virtio-console` is present. It appends
  `nr_cpus=<capacity>` from the validated processor topology. It reserves a 1-GiB
  MMIO gap from 3 to 4 GiB and exposes only PIC/IOAPIC, PIT, a CMOS RTC
  anchored to UTC,
  the microVM portb console, lifecycle ports, and the optional fixed virtio
  devices described below. User arguments cannot override `earlycon=`,
  `console=`, `virtio_mmio.device=`, `virtnet_*=`, or `virtfs_*=`.

  One virtio-fs slot is exposed at MMIO `0xd0001000`, IRQ 6 and remains
  dormant when `--mount` is omitted; `--mount` binds HostFs to it, and a
  second `--mount` adds a second slot at MMIO `0xd0008000`, IRQ 13;
  one optional `--virtio-console <BACKEND>` is exposed at MMIO `0xd0002000`,
  IRQ 7 as the boot/log console (`hvc1`); and `--microvm-sandbox-block`
  exposes fixed distro, runtime, custom, and scratch slots starting at MMIO
  `0xd0003000`. The profile also reserves MMIO `0xd0007000`, IRQ 3 for the
  dedicated control console (`hvc2`), selected by the host-owned
  `nvx_control_tty=hvc2` token. The reserved slot and resource identity do not
  expose a live control endpoint. Both consoles
  remain virtio-console devices from the guest's perspective. Ordinary
  `--virtio-blk` is rejected. All use split rings. Firmware, ACPI, SMBIOS, PCI,
  VMBus, UARTs, graphics, isolation, nested virtualization, and other devices
  are rejected. Linux discovers contiguous APIC IDs and the IOAPIC from Intel
  MP 1.4 tables at `0x0` and `0x400`; `boot_params` is at `0x2000`, the command
  line starts at `0x20000`, and no ACPI or SMBIOS data is exposed. Host-driven
  save/restore, pulse-save/restore, and worker restart remain unavailable.

  `microvm` may also expose a dedicated control virtio-console at MMIO
  `0xd0007000`, IRQ 3. It requires the boot virtio-console, preserves
  `console=hvc1`, and publishes `nvx_control_tty=hvc2`. The profile fixes
  boot-before-control discovery order and rejects user overrides that could
  change it.

  `microvm` uses one socket and one die,
  with one core per vCPU, no SMT, xAPIC mode, and contiguous APIC IDs from 0.
  Guest-requested snapshot capture and new-process restore are available for
  blockless and fixed-block machines on Linux/KVM, Linux/MSHV, and Windows/WHP.

  ```admonish warning title="microVM migration"
  The canonical `microvm` spelling now selects the contract formerly exposed
  as `microvm-v2`; the `microvm-v2` selector and the former ABI-v1 behavior are
  removed. Snapshot ABI and boot layout remain numeric value 2.
  ```
* `--net <IPv4/PREFIX>`: With `--machine microvm`, attach one virtio-net NIC
  at MMIO `0xd0000000`. KVM and MSHV use IRQ 10; WHP uses IRQ 5. Prefixes
  `/1` through `/30` are accepted. The first usable subnet address becomes
  the gateway; network, broadcast, and gateway addresses cannot be assigned
  to the guest. Guest and gateway MAC addresses are derived as
  `52:54:00:<second>:<third>:<fourth>` from their IPv4 addresses. Networking
  requires the only supported capability profile, `--network-profile portable`;
  omitting it rejects the command before OpenVMM opens host resources.

  ```bash
  openvmm --machine microvm --hypervisor whp \
    --kernel vmlinux --initrd initramfs.cpio.gz \
    --net 10.0.0.2/24 --network-profile portable
  ```

  `portable` uses an in-process Consomme endpoint on Linux/KVM, Linux/MSHV,
  and Windows/WHP. It needs no TAP, root access, driver, or host network
  configuration. `--net-tap` is incompatible and is rejected before any
  endpoint or host resource is created. The gateway provides DNS over UDP and
  TCP, ICMP echo, and outbound TCP/UDP through ordinary host sockets. Consomme
  rejects IPv4 fragments deterministically; policy filtering remains before
  host socket creation. Its per-connection TCP buffers start at 16 KiB and
  are bounded at 4 MiB; UDP bindings expire after five minutes; and at most
  256 DNS requests are pending at once. At most 128 TCP, 256 UDP, and 16 ICMP
  guest flows are active at once; excess flows are deterministically rejected
  before a host socket is created.

  `--allow-host <IPv4[/PREFIX]>`, `--block-host <IPv4[/PREFIX]>`, and
  `--allow-endpoint <IPv4:TCP-PORT>` are repeatable, mutually exclusive
  egress modes. Filtering runs before host socket creation. Active policy
  fails closed for malformed packets, non-IPv4 traffic, and IPv4 options.
  Exact endpoint mode also rejects UDP, ICMP, VLAN, fragments, and every TCP
  destination not listed. Endpoint addresses must be usable unicast identities;
  unspecified, current-network, loopback, link-local, multicast, reserved,
  guest-self, subnet-network, and subnet-broadcast addresses are rejected before
  host resources are opened. For each endpoint, ARP may resolve the endpoint
  itself when it is on-link, or the gateway otherwise. Duplicate endpoint
  addresses share one canonical next hop. This layer-2 permission does not relax
  the independent destination, TCP, or port check. No implicit DNS exception is
  added.

  `--network-egress <allow|deny>` and `--network-ingress <allow|deny>` map
  directional network default actions onto the portable profile. Egress defaults
  to `allow`; ingress defaults to `deny`, preserving the profile's existing
  behavior when neither option is present. Egress `deny` without allow rules
  blocks every guest-originated frame before Consomme opens a host socket.
  Responses belonging to a guest-initiated flow remain permitted when ingress
  is denied; unsolicited connections toward the guest remain unavailable.

  The portable profile cannot truthfully provide unrestricted inbound
  connectivity, so `--network-ingress allow` is rejected before VM resources
  are opened. Egress `allow` may accompany `--block-host`; egress `deny` may
  accompany `--allow-host` or `--allow-endpoint`. Contradictory default and
  rule combinations are rejected at the same validation boundary.

  `--network-egress-allow <RULE>` and `--network-egress-deny <RULE>` provide
  the generic L3/L4 policy form. A rule is either `IPv4[/PREFIX]` or
  `IPv4[/PREFIX]:tcp:PORT` / `IPv4[/PREFIX]:udp:PORT`. These flags require an
  explicit `--network-egress` default, accept at most 256 rules in each list,
  and cannot be mixed with the legacy `--allow-host`, `--block-host`, or
  `--allow-endpoint` forms. Deny rules are evaluated before allow rules.
  Address-only rules apply to every IPv4 protocol; port-specific policies
  reject fragmented IPv4 traffic because later fragments do not carry a
  verifiable transport header. Parsing, canonicalization, and contradictory
  option checks complete before VM resources are opened.

  `--host-loopback <allow|deny>` controls host-local access. The portable
  profile cannot provide generic bidirectional host-loopback connectivity:
  explicit `allow` without `--host-loopback-forward` is rejected before VM
  resources are opened. With deliberate forwards, `allow` preserves
  guest-to-host access subject to egress policy and publishes only the named
  host-to-guest ports. Omitting the option preserves the existing
  guest-gateway-to-host-loopback mapping without publishing guest ports.
  `deny` blocks general gateway and host-local destinations and rejects every
  `--host-loopback-forward`.

  `--network-proxy <IPv4:TCP-PORT>` preserves one exact proxy endpoint when
  host loopback is denied. The guest-visible address must equal the derived
  gateway, and only that TCP port is translated to host loopback. UDP on the
  same port is not exempt, even with ordinary egress allowed. The exception is
  included in the snapshot policy digest.

  `--host-loopback-forward <tcp|udp:HOST-PORT:GUEST-PORT>` binds one localhost
  port and forwards it into the guest. It requires explicit
  `--host-loopback allow`; duplicate bindings and more than 64 forwards are
  rejected before resources are opened. Live forwards are process-local
  attachments and are rejected for snapshot capture or restore.
  Explicit forwarding is port publishing, not support for a generic
  bidirectional host-loopback allow policy.

  Networked snapshots record the `portable` profile, drain accepted TX and
  endpoint-ready RX at the capture boundary, rewind unused guest RX
  descriptors, and recreate a fresh Consomme endpoint generation on restore.
  Restore of a networked snapshot requires `--network-profile portable` and
  the same active egress policy rules. The policy digest binds the saved static
  identity and its derived ARP next hops, which are reconstructed before vCPUs
  start. Native sockets and NAT flow tables are not serialized. The capture
  protocol does not retain pre-capture endpoint completions; restored guest
  software must establish new host-side flows.
* `--mount <GUEST_TARGET,HOST_PATH[,ro|rw]>`: With `--machine microvm`, attach
  one no-DAX HostFs device at MMIO `0xd0001000`, IRQ 6, with tag `microvm`.
  Repeat it once to attach a second device at MMIO `0xd0008000`, IRQ 13, with
  tag `microvm1`; each share has its own guest target, access mode, and
  denied paths. The default mode is read-only; `rw` must be explicit. The
  guest target must be an absolute, non-root Linux path without dot, parent,
  empty, whitespace, backslash, or `=` components. Guest targets and host
  directories of different shares must not equal or contain one another; on
  Linux, a bind mount or nested mount that reaches the files of another share
  counts as containing it.

  ```bash
  openvmm --machine microvm --hypervisor kvm \
    --kernel path/to/vmlinux --initrd path/to/initramfs.cpio.gz \
    --mount /mnt/share,path/to/share,ro
  openvmm --machine microvm --hypervisor kvm \
    --kernel path/to/vmlinux --initrd path/to/initramfs.cpio.gz \
    --mount /workspace,path/to/workspace,rw \
    --mount /opt/hostedtoolcache,path/to/toolcache,ro
  ```

  The device has one high-priority queue, one request queue, direct-I/O file
  behavior, zero entry and attribute cache lifetimes, and no shared-memory
  window. `--mount` conflicts with `--virtio-fs` and
  `--virtio-fs-shmem`; those standard-machine options cannot select the
  microVM filesystem profile. A read-write attachment accepts guest-created
  symbolic links, stores their targets exactly, and never follows them on the
  host; a read-only attachment rejects them with `EROFS`. On Windows, the
  links are WSL-style reparse points.

  `--mount-deny <HOST_PATH>` is repeatable and hides an existing file or
  directory inside an exported root. Paths are canonicalized to
  host-relative policy entries before resources are opened. With two shares,
  each path must be absolute and applies to the share whose root contains it.
  The complete root,
  paths outside the roots, duplicates, overlaps, symlink/reparse components,
  and nested-mount crossings are rejected. The virtio-fs server blocks the
  denied subtree and its root object identity, so `..`, a second mount of the
  same device, hard-link aliases, symlinks, junctions, and bind-mount aliases
  cannot re-expose it.

  `--mount-owner <vmm|caller>` selects the host identity of the guest's
  operations. `vmm`, the default, runs them all as OpenVMM. `caller` runs
  each one as its guest caller's UID and GID, without supplementary groups or
  capabilities, and squashes guest UID 0 and GID 0 to the owner of the export
  root, which must not be root. `caller` requires a Linux host. It also
  requires `CAP_SETUID` and `CAP_SETGID` unless every caller has OpenVMM's
  own UID and GID and OpenVMM has no other supplementary groups; an
  operation that cannot run as its caller fails with `EPERM`. The mode applies
  to every `--mount`.
  See [virtio-fs](../../devices/virtio/virtio-fs.md#host-identity-of-guest-operations).

  Filesystem snapshots contain guest-visible FUSE and queue state, not host
  directory contents or native handles. An active snapshot requires
  `--mount` again for every captured share, in capture order, with the exact
  canonical host path, guest target, access mode, denied-path set, and
  `--mount-owner` mode; the live roots and every saved object identity are
  also revalidated before vCPUs start. A snapshot captured without `--mount`
  may remain dormant or bind one new attachment to the first slot. The resumed
  guest must then explicitly run `mount -t virtiofs microvm <GUEST_TARGET>`
  because its cold-boot mount hook has already completed.
  See [virtio-fs](../../devices/virtio/virtio-fs.md).
* `--snapshot-destination <DIR>`: Publish a microVM snapshot when the guest
  writes to PMIO port `0x605`. The destination must not exist and its parent
  must already be a directory. OpenVMM automatically creates file-backed RAM
  in that parent when no memory backing file was supplied, delivers the portb
  console output that the guest wrote before its request, quiesces the VM,
  writes and flushes a sibling staging directory, and atomically renames it to
  `DIR`. After a successful commit, the source VM terminates without executing
  the instruction after the snapshot `out`.

  `--snapshot-quiesce-timeout-ms <MILLISECONDS>` sets the bounded quiesce
  timeout and defaults to 5000. The same bound applies to delivering portb
  output before quiescing; output still undelivered when it expires, or
  accepted while no portb peer is connected, is saved in the snapshot and
  written by the restored VM. A request with no configured destination is
  ignored and the guest continues. Capture requires 1, 2, 4, or 8 vCPUs,
  KVM, MSHV, or WHP, and shared file-backed RAM. Sandbox block media must
  be cached regular raw files with nonzero 512-byte-aligned geometry. An
  attached virtio console saves accepted but undelivered input and the offset
  of a partially forwarded guest transmit descriptor. An attached microVM
  virtio-net device saves its static identity, queue progress, drained packet
  ownership, endpoint generation, and policy requirement.
  The fixed microVM virtio-fs slot saves either an explicit dormant state or,
  when attached, its negotiated FUSE policy, namespace and handle identifiers,
  aliases, and directory cookies. The host tree remains external live state.

  `--memory-capacity <SIZE>` opts the snapshot into restore-time memory
  expansion. `SIZE` is an immutable 128-MiB-aligned upper bound, must be at
  least the base `--memory` size, and reserves the complete canonical GPA
  aperture without adding it to the initial Linux direct e820 RAM map or
  `memory.bin`.

  ```bash
  openvmm --machine microvm --hypervisor kvm --memory 128M \
    --kernel vmlinux --initrd initramfs.cpio.gz \
    --snapshot-destination snapshot
  ```

  `/sbin/nvx-snapshot` requests a paired capture by default. When scratch is
  mounted, it freezes the workload cgroup with a bounded wait, syncs, freezes
  the scratch filesystem, and asks OpenVMM to drain queues and atomically
  publish `scratch.img`. `/sbin/nvx-snapshot --fresh-scratch` is for a
  pre-mount boundary and records that restore must supply a fresh scratch.
* `--microvm-workload-identity <UID:GID>`: Add a fixed non-root numeric
  workload identity to the host-owned microVM command line. UID and GID zero
  are rejected. The guest workload supervisor must resolve both values in the
  workload root before launch and fail closed when either identity is
  unavailable. Workload requests cannot replace this identity. Snapshot
  restore takes the captured identity from the authoritative command line and
  rejects an override.
* `--microvm-lifecycle <one-shot|managed>`: Select the host-owned workload
  lifecycle written into the initial microVM command line. `one-shot` starts
  one workload and expects the guest to terminate the VM when it completes.
  `managed` keeps the guest supervisor resident for multiple sequential
  workload requests and requires a fixed workload identity plus a live,
  authenticated `--microvm-control-console`. Snapshot restore takes the
  captured lifecycle and rejects an override.
* `--microvm-report <PATH>`: Atomically create one bounded local JSON outcome
  report after the microVM controller and worker mesh finish teardown. The
  report contains a schema version, opaque instance ID, backend category,
  workload operation/category and numeric status, network-policy
  applied/rejected state with rule counts, and explicit resource-release
  booleans. Worker join failures produce a `teardown-failure` outcome and a
  nonzero process status.

  The destination must not exist and its parent must be a plain directory.
  Reports never contain commands, environment values, paths, network
  destinations, proxy details, workload output, credentials, or free-form
  errors. OpenVMM writes the file only to the requested local destination and
  does not upload it.
* `--restore-snapshot <DIR>`: Restore a microVM from a committed snapshot.
  The manifest supplies the authoritative RAM size, topology, ABI,
  fixed device inventory, effective kernel command line, source backend, time
  contract, and CPU profile. Kernel, initrd, command-line, ordinary
  `--memory`, processor, device, and topology overrides are not accepted;
  expansion-capable snapshots use only `--restore-memory`. Repeat the
  snapshot's exact `--processors` count; a mismatch is rejected before any VP
  starts. Restore requires the same backend kind as capture.

  When the snapshot contains a virtio console, its attachment policy comes
  from the manifest. OpenVMM recreates listeners, reconnects required clients,
  or requires an inherited replacement before creating the partition. Restore
  fails before any vCPU starts when a required attachment cannot be rebuilt.
  A listener peer may connect after restore; guest transmit descriptors remain
  pending while no peer is connected.

  When the snapshot contains active virtio-fs attachments, restore requires
  a fresh `--mount` for each, in capture order. Each argument must reproduce
  the manifest's exact canonical host path, guest target, and `ro`/`rw` mode
  while also supplying a live root with the same saved identity. A snapshot
  advertising the dormant slot may instead accept one new attachment;
  snapshots without that capability reject additive attachment.

  When the snapshot contains virtio-net, restore also requires
  `--network-profile portable`; the snapshot's profile and canonical egress
  policy must match the supplied portable configuration.

  For sandbox-block snapshots, restore repeats each read-only
  `--microvm-sandbox-block` argument. Its role, access, geometry, and SHA-256
  must match the manifest. A paired snapshot supplies scratch internally from
  a verified process-private copy of `scratch.img`; passing another scratch is
  rejected. A fresh-scratch snapshot instead requires a writable scratch
  argument with matching geometry.

  `--restore-memory <SIZE>` selects the total RAM for this launch. It requires
  an expansion-capable snapshot and a 128-MiB-aligned value from the exact
  captured base through the immutable capacity. Base RAM remains a private
  copy-on-write mapping of `memory.bin`; selected expansion ranges use fresh
  zeroed private backing. Expansion implies the post-restore repair gate.

  ```bash
  openvmm --machine microvm --hypervisor kvm \
    --restore-snapshot snapshot
  ```
* `--restore-ready-path <PATH>`: Connect to an existing Unix domain socket on
  Linux or a `//./pipe/...` named pipe on Windows and write exactly
  `OPENVMM_RESTORE_READY_V1\n` once all restored state, required attachments,
  and execution-owned workers are ready. Ungated restores flush the event
  before releasing the restored vCPU. Gated microVM restores flush it after the
  guest acknowledges post-restore repair and external input is re-enabled,
  while the restored vCPU remains stopped.
  It is valid only with `--restore-snapshot` and is process-local; it is not
  saved in the snapshot. A connection, write, or flush failure aborts startup
  and stops the VM. The peer must accept and read the event while startup is
  in progress; Windows flush completion waits for the named-pipe peer to
  consume the complete frame.
* `--restore-entropy`: Accepted for compatibility; it has no effect. Every
  microVM restore makes restore packet version 4 available on the private
  portb restore channel. The packet carries 64 bytes of fresh entropy, the
  online-VP target, the memory-expansion ranges, the downtime, and the TSC
  rate deviation; the guest must consume it and explicitly reseed its RNG.
  [Snapshots](../../../user_guide/openvmm/snapshots.md) documents its format,
  the portb status bits, and the time sample.
  Every microVM portb device also reports generation-ID support in status bit
  5. Writing `0xa6` to the status port and reading 16 bytes from the data port
  returns an opaque ID that is stable for that VM process and may be selected
  repeatedly. OpenVMM creates it before vCPU entry and does not serialize it.
  On restore, it is the first 16 bytes of the restore packet's fresh entropy,
  allowing the guest repair path to update clone identity without additional
  port I/O.
* `--restore-processors <COUNT>`: For an opt-in microVM snapshot, bring the
  contiguous VP prefix `0..COUNT-1` online before restore readiness. The
  snapshot's manifest VP count remains immutable capacity and must still match
  `--processors`. The target must be 1, 2, 4, or 8 and satisfy
  `boot-online <= target <= capacity`. This option sets the restore packet's
  online-VP target and implies the post-restore gate. Snapshots without
  activation metadata reject it. An
  explicit MSHV restore instantiates and binds only the requested prefix while
  validating the full saved VP inventory; that reduced-prefix process cannot
  be saved again. MSHV restores without this option, and KVM and WHP restores,
  instantiate the full VP capacity.
* `--restore-gate-timeout-ms <MILLISECONDS>`: Bound microVM guest repair and
  gate acknowledgement after restore. The default is 60000 milliseconds.
* `--cpu-profile <ID>`: Select the microVM's pinned CPU profile, `auto`
  (the default) for the profile of the host's CPU generation, or `host` to
  derive a development profile from this host's backend fingerprint for a
  host that no pinned profile serves. `auto` never selects a host profile. A
  host without a profile, or a profile of another generation, fails before the
  partition is created. On an Intel or AMD host, `auto`'s errors name
  `--cpu-profile host` when no pinned profile serves the host and when the
  backend does not support the pinned profile (`E_PROFILE_UNSUPPORTED`). A
  restore uses the snapshot's profile and rejects any
  other; `host` also names a host profile that the snapshot recorded. See
  [Time and CPU compatibility](../../../user_guide/openvmm/snapshots.md#time-and-cpu-compatibility).
* `--x-time-abi-verify`: Build the microVM partition and run the time ABI
  checks (CPU profile support, effective CPUID, rates) without starting the
  guest, print one `NVX-TIME-ABI-VERIFY:` line, and exit with status 0 or 1.
  Host qualification uses it; it cannot be combined with snapshot capture or
  restore.
* `--snapshot-tier <TIER>`: Required for snapshot capture with sandbox blocks. Choose
  `platform`, `workload-start`, or `instance-checkpoint`. The first two are
  reusable clone policies; instance checkpoints use single-use resume policy.

A committed snapshot contains `manifest.bin`, `state.bin`, `memory.bin`, and
optionally the manifest-declared `scratch.img`. Restore rejects unknown files,
symlinks, malformed or oversized data, length or scratch-digest mismatches, and
incompatible machine contracts before starting a vCPU. `memory.bin` uses a
private writable copy-on-write mapping and paired scratch is privately copied,
so clone-policy snapshots can be restored repeatedly without modifying
artifacts. Instance-checkpoint snapshots permit one restore attempt.

Versions 3 through 6 do not embed or validate checksums for `state.bin` or `memory.bin`;
legacy version 2 checksum fields are accepted without re-hashing their
payloads. This format does not detect same-length payload changes,
authenticate, or encrypt a snapshot. Treat all three artifacts as sensitive
guest state and protect the directory with host access controls.
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
* `--no-hv`: Boots AArch64 UEFI without exposing Hyper-V enlightenments.
  By default, UEFI exposes the enlightenments. This option requires
  `--no-vmbus`, is not supported for x86_64 UEFI, and conflicts with `--hv`,
  `--vtl2`, `--get`, and `--pcat`.
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
* `--cpu-fingerprint <PATH>`: Write a CPU fingerprint of the host for the
  backend selected by `--hypervisor` (or the auto-detected one) to `PATH`, or
  to standard output for `-`, and exit without starting a VM. Backend
  parameters are rejected.

  The fingerprint records the guest CPU surface that the backend supports on
  this host: every CPUID leaf and subleaf, the XSAVE features and layout, the
  feature MSRs, and the processor feature banks. It also records the time
  capabilities (TSC and LAPIC timer rates, invariant TSC, TSC-deadline,
  `TSC_ADJUST`, TSC offset control and scaling, time freezing, and MSR
  interception) and the host CPU, OS, and hypervisor identity. KVM reports its
  surface through `KVM_GET_SUPPORTED_CPUID` and its MSR-based features; MSHV
  and WHP report the CPUID of a transient probe partition that is destroyed
  before OpenVMM exits. No guest runs.

  The output is deterministic, sorted JSON (schema
  `openvmm-cpu-fingerprint/v1`). CPUID fields that depend on the executing
  processor or on the virtual processor state, such as APIC IDs and the
  enabled XSAVE size, are normalized. `surface_digest` is the SHA-256 of the
  guest CPU surface, so hosts that offer guests the same CPU share it, and
  `digest` covers the whole document.

  OpenVMM then checks the fingerprint against the pinned CPU profile of the
  host's generation (the profile that `--cpu-profile auto` would select) and
  prints one line to standard error, for example:

  ```text
  NVX-CPU-PROFILE: status=pass backend=kvm generation=icelake-sp profile=intel.icelake-sp.v1 profile_digest=sha256:… surface_digest=sha256:… host_invariant_tsc=yes
  ```

  If no pinned profile serves the host, or the backend does not support every
  feature, limit, XSAVE layout, and MSR value of the profile, the line has
  `status=fail`, `code`, and `detail`, and OpenVMM exits with status 1 and the
  same code (`E_PROFILE_HOST_UNKNOWN` or `E_PROFILE_UNSUPPORTED`). On MSHV and
  WHP, which present the hypervisor's own guest view at every CPUID entry that
  the profile does not list, a partition that reads non-zero at such an entry
  fails with `E_CPU_UNLISTED`, so that no such entry carries host data; the
  identity range `0x40000000` to `0x4fffffff` and the topology leaves `0xb`
  and `0x1f` are exempt. WHP reads VP 0 of a second probe partition
  configured from the profile, as a cold boot configures its partition, at
  the host's CPUID entries outside the profile; MSHV reads the fingerprint's
  probe partition, which enables every feature the backend offers. KVM
  installs the profile as the guest's whole CPUID table and answers every
  other entry from it (zero, or the architecture's out-of-range and topology
  results), so the check does not apply. The fingerprint is still written, so
  hosts of new generations can be fingerprinted to derive their profiles.

  ```bash
  openvmm --hypervisor kvm --cpu-fingerprint host.json
  openvmm --hypervisor whp --cpu-fingerprint -
  ```
* `--isolation <MODE>`: Enable a confidential or isolated VM mode.
  Supported modes include `vbs` and, for `x86_64` guests on KVM or MSHV,
  `snp`.

  SNP support is currently limited to Linux direct boot and is intended for
  bring-up. It supports either loader-based kernel/initrd boot or an SNP IGVM
  selected with `--igvm-personality linux-direct`. MSHV SNP can expose Hyper-V
  enlightenments with `--hv --no-vmbus`; VMBus devices remain unsupported.
  KVM SNP does not support Hyper-V enlightenments.
  The IGVM must use VTL0, no shared GPA boundary, and no relocation metadata.

  SNP does not support UEFI, VTL2, or hugetlb-backed memory. In addition to
  the minimal emulated chipset and serial console, optional devices are
  limited to virtio devices attached through PCIe.

  A minimal MSHV IGVM invocation is:

  ```bash
  openvmm --hypervisor mshv --isolation snp \
    --igvm path/to/snp-linux-direct.bin \
    --igvm-personality linux-direct --com1 console \
    --hv --no-vmbus -m 160MB -p 1
  ```
* `--snp-restricted-injection`: Enable restricted interrupt injection in the
  MSHV partition and loader-generated SNP VMSA. This is only supported on mshv
  today.
* `--hypervisor mshv:snp_disable_cpuid_offload=true`: Disable MSHV handling of
  SNP GHCB CPUID requests so they are forwarded to OpenVMM. The default is
  offloading enabled. This diagnostic parameter is meaningful only with
  `--isolation snp`.
* `--nested-virt`: Expose hardware virtualization (VMX/SVM) to the guest so it
  can run its own hypervisor (Hyper-V, KVM, etc.). Only supported on `x86_64`,
  and only by backends that support nested virtualization (currently WHP and
  KVM); requesting it with a backend that does not support it fails early. The
  host must expose virtualization extensions to the VM running OpenVMM. When
  enabled, a guest may detect nested virtualization and turn on features such
  as Virtual Secure Mode (VSM), which can hurt performance and interfere with
  VMBus devices; nested virt cannot currently be combined with `--hv`/VMBus or
  `--hypervisor whp:user_mode_apic`.
* `--uefi [OPTIONS]`: Boot using `mu_msvm` UEFI. Options are comma-separated:
  * `firmware=<FILE>`: Path to the UEFI firmware file (`MSVM.fd`). If omitted, the default is read from `OPENVMM_UEFI_FIRMWARE`, then from `X86_64_OPENVMM_UEFI_FIRMWARE` or `AARCH64_OPENVMM_UEFI_FIRMWARE`.
  * `debug`: Enable UEFI debugging on COM1.
  * `enable_memory_protections`: Enable UEFI memory protections.
  * `force_dma_bounce`: Force UEFI to bounce-buffer all DMA traffic.
  * `force_firmware_version`: Continue when a present version record is malformed or declares an incompatible interface version. A missing record only produces a warning.
  * `disable_frontpage`: Shut down instead of showing the UEFI front page.
  * `console=<default|com1|com2|none>`: Select the UEFI console.
  * `diagnostics=<default|info|full>`: Select the EFI diagnostics log level.
  * `default_boot_always_attempt`: Attempt the default boot path even if configured boot entries exist and fail.

  With `--igvm --vtl2`, `--uefi` configures the UEFI firmware that OpenHCL
  loads into VTL0. All options except `firmware` and
  `force_firmware_version` are supported in this mode. Those options apply
  only when OpenVMM loads an external firmware image and are rejected with
  `--igvm`. Explicit non-VTL2 IGVM personalities do not accept `--uefi`.

  The previous standalone UEFI options remain accepted but are deprecated.
* `--pcat`: Boot using the Microsoft Hyper-V PCAT BIOS
* `--igvm <FILE>`: Boot from an IGVM file.
* `--igvm-personality <uefi|linux-direct>`: Select the chipset and
  device shape for an IGVM boot without VTL2. This option is required with
  `--igvm` unless `--vtl2` is present; there is no default for non-VTL2
  boots. The personality does not select the isolation platform. Use
  `--isolation` separately when required by the IGVM.

  The `uefi` personality uses the Gen2 device shape, but firmware is loaded
  from the IGVM. It does not select the normal external-UEFI load path. The
  `linux-direct` personality enables Hyper-V enlightenments only when `--hv`
  is also specified. The UEFI personality requires Hyper-V enlightenments and
  fails explicitly on backend and isolation combinations that cannot provide
  them.

  With `--igvm --vtl2`, omit `--igvm-personality`. OpenVMM retains the
  existing HCL-host device shape and VBS-compatible IGVM behavior.
* `--tpm [VERSION]`: Add a vTPM device. Supported versions are `138` and
  `185`; a bare `--tpm` uses version `185`. The dotted forms `1.38` and `1.85`
  are also accepted.
* `--vmbus-scsi id=<name>[,sub_channels=<N>][,vtl2]`: Creates a
  named VMBus SCSI controller. Use with `--disk ...,on=<name>` to
  attach disks.
* `--disk file:<DISK>,on=<name>`: Attaches a disk to the named
  controller. The `DISK` argument can be:
  * A flat binary disk image
  * A VHD file with an extension of .vhd (Windows host only)
  * A VHDX file with an extension of .vhdx

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
* `--smbios <PARAMS>`: Override the SMBIOS (DMI) identity reported to the
  guest (repeatable), using `type=N,key=value[,key=value...]`.
  Type 0 supports `vendor`, `version`, `date`, and `release`; Type 1 supports
  `manufacturer`, `product`, `version`, `serial`, `uuid`, `sku`, and `family`.
  Use `uuid=random` to generate a per-VM system UUID.

  OpenVMM Linux direct boot supports both types. OpenHCL Linux direct and UEFI
  support Type 1 only. PCAT supports only Type 1 `serial` and `uuid`.
  Unsupported fields are rejected.

  ```bash
  --smbios type=1,manufacturer=Contoso,product="Virtual Machine"
  ```
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
* `--virtio-fs-bus <BUS>`: Select the bus for `--virtio-fs` and
  `--virtio-fs-shmem` devices. Accepted values are `auto`, `mmio`, `pci`,
  `pcie:PORT`, and `vpci`. Defaults to `auto`. A `pcie_port` prefix on either
  device option overrides this setting. Each PCIe port may be assigned to only
  one device, whether selected by `pcie:PORT` or a `pcie_port` prefix.
* `--virtio-rng`: Add a virtio entropy (RNG) device, exposing `/dev/hwrng` in the Linux guest.
  The guest kernel must have `CONFIG_HW_RANDOM_VIRTIO` enabled.
* `--virtio-rng-bus <BUS>`: Select the bus for the virtio-rng device. Accepted
  values are `auto`, `mmio`, `pci`, `pcie:PORT`, and `vpci`. Defaults to
  `auto`. `--virtio-rng-pcie-port` overrides this option.
* `--virtio-vsock-path <PATH>`: Add a virtio-vsock device using the OpenVMM
  hybrid Unix socket relay.
* `--virtio-vsock-bus <mmio|pci|pcie[:PORT]>`: Select the bus for a
  virtio-vsock device created by `--virtio-vsock-path` or
  `--virtio-vsock-vhost-cid`. When omitted, OpenVMM selects the bus
  automatically. The `pcie` value uses the root port named `vsock`. Use
  `pcie:PORT` to select another root port.
* `--virtio-vsock-vhost-cid <CID>`: Add a virtio-vsock device backed by the
  Linux kernel's `vhost_vsock` implementation. This makes the guest reachable
  from host applications through `AF_VSOCK` at `CID`, which must be between 3
  and 4294967294 (CIDs 0-2 are reserved for the hypervisor, loopback, and host,
  respectively, and u32::MAX is the ANY wildcard). This option requires
  `/dev/vhost-vsock`, the `vhost_vsock` kernel module, and shared file-backed
  guest RAM (the default memory backing). It uses identity-mapped DMA and
  does not support a non-identity virtual IOMMU. It conflicts with
  `--virtio-vsock-path`.
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
* `--virtio-console <BACKEND>`: Expose a virtio console device. It normally
  appears as `/dev/hvc0`. Under `--machine microvm`, it occupies fixed MMIO
  `0xd0002000`, IRQ 7, and is selected as `/dev/hvc1`; the raw portb path
  remains available as `hvc0` for early output and recovery.

  A microVM accepts these explicit attachment policies:

  * `listen=PATH` or `listen=tcp:IP:PORT`: save the canonical endpoint and
    recreate the optional listener on restore. Unix sockets must be beside the
    snapshot directory. Windows pipes use the
    `//./pipe/openvmm-microvm-<NAME>` namespace. TCP ports must be nonzero.
    TCP addresses must be loopback addresses. A restore caller can resupply
    `--virtio-console listen=<FRESH-ENDPOINT>` with `--restore-snapshot` to
    replace the saved listener identity while preserving the attachment
    contract. Omitting the option recreates the saved listener endpoint.
  * `connect=PATH` or `connect=tcp:IP:PORT`: require a client connection before
    vCPUs start. Cold boot and restore use a five-second timeout. The
    restore command must explicitly resupply the matching client attachment.
  * `console`: require the restore caller to supply the same inherited terminal
    attachment. Portb recovery output moves to stderr while the terminal is
    attached to `hvc1`.
  * `none`: keep the device present and explicitly discard guest TX while
    disconnected.

  A generic byte stream guarantees no replay up to OpenVMM's backend write
  boundary; it cannot prove that the remote application consumed bytes without
  its own acknowledgment protocol.

* `--microvm-control-console <BACKEND>`: With `--machine microvm`, expose a
  second independent single-port virtio console at fixed MMIO `0xd0007000`, IRQ
  3. `--virtio-console` is required on a fresh boot and remains the only kernel
  console. The control device normally appears as the profile-owned
  `nvx_control_tty=hvc2`.

  On Linux, the only live backend is `listen=PATH`, and PATH is always an
  AF_UNIX socket. Its parent must already be an owned, non-symlink directory
  with mode `0700`; OpenVMM exclusively binds the socket, sets and verifies
  mode `0600`, and verifies `SO_PEERCRED` before accepting the protocol
  attachment.

  On Windows, `listen=//./pipe/openvmm-microvm-<NAME>` creates one byte-mode
  named pipe with a protected DACL granting access only to LocalSystem and the
  OpenVMM process user. OpenVMM obtains the connecting process ID from the pipe,
  resolves its token user SID, and requires it to match the OpenVMM process
  user before capability authentication. TCP, client-connect, terminal, file,
  stdout/stderr, and inherited control backends are rejected on every platform.

  A live endpoint also requires the hidden launcher option
  `--microvm-control-auth-stdin`. The launcher must attach a prepared readable
  one-way pipe to standard input, containing exactly 32 random capability
  bytes, and close every writer before starting OpenVMM. OpenVMM safely
  duplicates stdin into an owned file, performs one bounded nonblocking read
  through EOF, and closes the duplicate. An all-zero capability is rejected.
  Standard input remains at EOF and is reserved for authentication: the stdin
  REPL is disabled, the boot console must use a socket or `none`, and portb
  recovery output goes to stderr. This option cannot be combined with the
  management RPC server, console relay, or `--paused`. Capability
  bytes must never appear in arguments, environment variables, endpoint names,
  logs, snapshots, or attachment identities. The first host record must prove
  that capability. Peer identity is checked first. Authentication must complete
  within `--microvm-control-auth-timeout-ms` (default 5000, range 1 to 60000).
  A stalled or failed authentication attempt closes the connection without a
  protocol Error record and without changing the broker epoch.

  The outer protocol uses byte-counted guest receive credits so host DATA is
  backpressured before the guest's bounded ingress storage is exhausted. See
  [Control-session Protocol](./control_session_protocol.md).

  `none` does not consume stdin and rejects `--microvm-control-auth-stdin`.
  OpenVMM generates an unreachable random capability so disconnected process
  tests remain supported.

  The numeric `--microvm-control-auth-handle` interface is not supported.
  Launchers must explicitly select the stdin contract; there is no fallback
  to an unauthenticated endpoint.

  Boot and control endpoints must be distinct. Snapshot capture records only
  the separate `console:microvm-control0` endpoint and broker-authenticated
  reconnect policy. It never records a capability or UID/SID. Restore validates
  the saved endpoint contract, requires a fresh launcher-provided capability
  for a live endpoint, and generates a fresh VMM instance ID; saved credentials
  and stale capabilities are never reused. Restore callers resupply
  `--microvm-control-console listen=<FRESH-ENDPOINT>` together with
  `--microvm-control-auth-stdin`; the endpoint may change, but the saved stable
  attachment ID, attachment kind, backend kind, reconnect policy, required
  flag, length, and timeout remain exact.

  Restore snapshots containing this device through the CLI. The OpenVMM
  management RPC does not expose control-console restore attachments and
  rejects these snapshots explicitly.

The `BACKEND` argument is the same for all serial devices:

  * `none`: Serial output is dropped.
  * `console`: Serial input is read and output is written to the console.
  * `stderr`: Serial output is written to stderr.
  * `listen=PATH`: A named pipe (on Windows) or Unix socket (on Linux) is set
      up to listen on the given path. Serial input and output is relayed to this
      pipe/socket.
  * `listen=tcp:IP:PORT`: As with `listen=PATH`, but listen for TCP
      connections on the given IP address and port. A microVM requires a
      loopback IP such as `127.0.0.1` or `::1`.
  * `connect=PATH`: Connect to an existing named pipe or Unix socket.
  * `connect=tcp:IP:PORT`: Connect to an existing TCP listener.

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

For microVMs, a process-status shutdown through port `0x604`, or a power event
configured to exit, drains previously accepted portb console output before the
process exits. This includes the host stdout/stderr relay, not just the device's
transmit buffer. Draining is bounded to five seconds; an output error or timeout
is reported as a failure rather than a successful exit with truncated output.
In RPC-server mode, a drain failure also fails pending `WaitVm` requests and
terminates the server with an error instead of leaving it uninitialized.
A guest-requested snapshot capture instead flushes accepted portb output to its
endpoint before quiescing, without closing it because the capture can still roll
back; see `--snapshot-destination`.

* `--crash-dump-path <PATH>`: when the guest triple-faults, write a
  WinDbg-compatible `.vmrs` dump of the VM's processor state and guest memory to
  `PATH` before the `--guest-crash-action` is applied (see
  [VM Memory Dumps](../../../user_guide/openvmm/vm_memory_dumps.md)). This is a
  host-side, whole-VM dump, distinct from `--openhcl-dump-path` (OpenHCL's
  in-guest crash dump device driven by the guest OS).

The `--uefi disable_frontpage` option powers the VM off instead of showing the
firmware frontpage when there is no bootable device. Combined with
`--guest-shutdown-action exit`, a guest with no boot device exits the VMM.

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

By default, PCIe ECAM is placed above 4 GiB to preserve low MMIO space for
device BARs. Use `--pcie-ecam-below-4gb` to place every PCI segment's ECAM in
32-bit MMIO instead. This compatibility workaround is intended for direct-boot
guest kernels that cannot discover high ECAM without firmware interfaces
available during a conventional boot. The flag defaults to off and requires
`--pcie-root-complex`.

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
- `pasid`: advertises support for TLP prefixing (such as for guest PASID
  behind a virtual IOMMU)

`--pcie-switch` accepts optional comma-separated options as well:

```sh
--pcie-switch rp0:switch0,num_downstream_ports=4,acs=0x005f
```

- `num_downstream_ports=<N>`: number of downstream ports for the switch.
- `hotplug`: enables hotplug support on all downstream switch ports.
- `acs=<mask>`: ACS capability mask requested for downstream switch ports.
  The upstream switch port does not expose ACS. Default is `0x005f`.
  Use `acs=0` to disable ACS for switch downstream ports.
- `pasid`: advertises support for TLP prefixing (such as for guest PASID
  behind a virtual IOMMU)

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
--vfio host=0000:01:00.0,port=rp0,bar0=host
```

### SMMU (aarch64 only)

`--smmu` enables an emulated Arm SMMUv3 IOMMU for a named PCIe root
complex. The flag is repeatable — use one `--smmu` per root complex that
should have an SMMU. Devices behind a covered root complex get IOVA→GPA
translation for DMA and MSI addresses. See
[Arm SMMUv3](../../emulated/iommu/smmuv3.md) for the device reference.

The syntax is a comma-separated key/value list:

```sh
--smmu rc=<name>[,accel][,oas=auto|N]
```

- `rc=<name>` (required): the PCIe root complex this SMMU covers.
- `accel` (optional): delegate stage-1 translation to the host IOMMU via
  iommufd nesting, so VFIO-assigned devices behind this root complex are
  translated in hardware. Requires ACPI, a nesting-capable host SMMUv3, and
  that the devices use the `--iommu` cdev path with a single shared context.
  Without it, assigning a VFIO device behind an SMMU is rejected.
- `oas=auto|N` (optional): the SMMU's output address size (OAS) in bits.
  `auto` (the default) starts at 48 bits, which covers typical configurations.
  Under `accel`, a device attached before VM start changes it to the physical
  SMMU's OAS. VM start freezes the advertised value, so later hotplug validates
  against it rather than changing it. Very large RAM or an explicitly pinned
  high MMIO/ECAM base can exceed 48 bits, requiring an explicit larger `oas=`
  (e.g. `oas=52`). A fixed `N` must be one of the SMMUv3-legal encodings: `32`,
  `36`, `40`, `42`, `44`, `48`, or `52`, and cannot exceed the physical
  SMMU's OAS under `accel`.

```sh
# Enable an emulated SMMU on root complex rc0
--smmu rc=rc0

# Multiple root complexes
--smmu rc=rc0 --smmu rc=rc1

# Pin the output address size to 48 bits
--smmu rc=rc0,oas=48

# Assign a VFIO device behind an accelerated SMMU
--smmu rc=rc0,accel --iommu id=iommu0 \
  --vfio host=0000:01:00.0,port=rp0,iommu=iommu0
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
