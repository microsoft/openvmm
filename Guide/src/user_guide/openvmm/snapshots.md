# Snapshots

OpenVMM supports saving and restoring VM snapshots, allowing you to capture
the complete state of a running VM and resume it later.

## Overview

A snapshot captures three required pieces of state and may pair writable
microVM scratch:

- **Guest RAM** — the full contents of guest memory
- **Device state** — the saved state of all emulated devices
- **Manifest** — metadata describing the snapshot (architecture, memory size,
  VP count, page size, etc.)
- **Scratch** — the exact microVM writable image when capture occurs after mount

These are stored as three required files and one optional paired file:

| File            | Contents                                    |
|-----------------|---------------------------------------------|
| `manifest.bin`  | Protobuf-encoded snapshot metadata          |
| `state.bin`     | Serialized device state                     |
| `memory.bin`    | Memory backing file                         |
| `scratch.img`   | Paired microVM scratch, when declared       |

## Prerequisites

Host-driven snapshots require **file-backed guest memory**. Pass `file=<PATH>`
in the `--memory` option when launching a standard VM. A microVM launched with
`--snapshot-destination` automatically creates temporary file-backed RAM in
the destination's parent directory when no backing file was supplied. Capture
with sandbox blocks additionally requires
`--snapshot-tier platform|workload-start|instance-checkpoint`. Platform and
workload-start snapshots are reusable clones; instance checkpoints are
single-use resumes.

```admonish warning
Automatically allocated microVM RAM and the snapshot destination are on the
same filesystem so OpenVMM can promote the exact RAM file by hard link. A
filesystem without hard-link support falls back to copying. Explicit
user-supplied memory is always copied into a uniquely named sibling staging
directory. OpenVMM atomically renames the completed directory into place.
```

## Saving a snapshot

Start a VM with file-backed memory:

```bash
cargo run -- \
  --uefi \
  --vmbus-scsi id=scsi0 \
  --disk memdiff:file:path/to/disk.vhdx,on=scsi0 \
  --memory size=4096M,file=path/to/memory.bin
```

Once the VM is running, open the interactive console and issue a save command,
specifying the output directory:

```text
save-snapshot path/to/snapshot-dir
```

OpenVMM writes and flushes `manifest.bin`, `state.bin`, and `memory.bin` in a
sibling staging directory. Host-driven saves and user-supplied microVM backing
use an independent memory copy. Automatic microVM backing uses its exact RAM
file when the filesystem supports hard links. The destination must not already
exist. Publishing the completed directory is the commit point.

On Windows, large mapped-RAM flushes use up to eight concurrent, disjoint,
page-aligned ranges to reduce sensitivity to fragmented dirty-page writeback.
Small ranges are flushed inline. Capture waits for every range, including
when a flush fails; an error prevents publication. The subsequent file and
directory durability barriers are unchanged.
Worker startup has a cost on fast storage; this policy targets writeback
stalls rather than guaranteeing lower capture latency on every host.

The Windows-only `sparse_mmap` test `profile_fragmented_file_flush` reproduces
the fragmented writeback workload with alternating dirty 4-KiB pages in a
128-MiB file. It compares serial and bounded-parallel flushing, including the
subsequent file sync, and verifies the persisted bytes. Run it on an otherwise
idle host:

```text
cargo nextest run --profile agent --release -p sparse_mmap --run-ignored only -E "test(profile_fragmented_file_flush)" --success-output immediate
```

Timing output is diagnostic, not a portable assertion or proof that an
external storage-throttling condition has been eliminated.

```admonish warning
After a host-driven save, the VM remains **paused**. Guest-requested microVM
capture instead terminates the source process after publication commits.
If automatic-RAM publication fails after creating its staging link, OpenVMM
removes the complete staging directory before resuming; if cleanup cannot be
proved, it terminates the source instead.
```

While a guest-requested snapshot boundary is held, VM-worker management RPCs
that can change VM state are rejected rather than queued. This includes memory
writes, pause/resume, reset, interrupt injection, hotplug, and state dumps.
Snapshot lifecycle RPCs and memory reads remain available. Mutating RPCs with
an error result report that the boundary is active; pause, clear-halt, and NMI
requests report a reply-channel error because their result types cannot carry
an application error. Retry a rejected operation after the boundary is
released; ordinary paused VMs are not subject to this restriction.

## Restoring a snapshot

To restore, pass the snapshot directory with `--restore-snapshot`:

```bash
cargo run -- \
  --uefi \
  --vmbus-scsi id=scsi0 \
  --disk memdiff:file:path/to/disk.vhdx,on=scsi0 \
  --memory size=4096M \
  --processors 4 \
  --restore-snapshot path/to/snapshot-dir
```

`--restore-snapshot` verifies and opens `memory.bin` from the snapshot
directory, so `file=...` should not be specified in `--memory` (the two options
are mutually exclusive). Guest writes use a private copy-on-write mapping and
do not modify the snapshot artifact.

MicroVM orchestrators can add `--restore-ready-path <PATH>`. OpenVMM connects
to an existing Unix domain socket on Linux or named pipe on Windows and writes
`OPENVMM_RESTORE_READY_V1\n` after restore validation, attachment resolution,
and state-unit startup. Ungated restores publish it before releasing a restored
vCPU. Gated microVM restores publish it after the guest acknowledges repair and
host input is re-enabled, while the restored vCPU remains stopped. The event
is single-use and is not serialized.
Failure to write and flush it stops the started units and fails restore without
releasing gated input. The peer must accept and read while resume is in
progress; on Windows, flush completion waits until the named-pipe peer consumes
the complete frame.

For a tiered microVM restore, OpenVMM starts device workers with network and
control input gated. The guest performs post-restore repair and writes the
existing snapshot port (`0x605`) to acknowledge completion. OpenVMM stops at
that exact post-write boundary, completes the deferred write while the vCPU is
stopped, and then releases device input before resuming the vCPU. The acknowledgement is bounded by
`--restore-gate-timeout-ms` (60000 milliseconds by default). Platform manifests
leave read-only layer identities unbound, while later tiers require exact image
identities. An instance-checkpoint restore attempt atomically creates
`resume.claim`; subsequent restores are rejected. The claim is committed after
artifact and configuration validation but before worker construction, so the
restore attempt remains consumed if later worker startup fails.

The no-ACPI microVM portb contract exposes a process-local 16-byte generation
ID. Status bit 5 advertises the feature; writing `0xa6` to status port `0xea`
and reading 16 bytes from data port `0xe9` returns the ID. It is repeatable
within one process and is never restored from snapshot state. A restore derives
the ID from the first 16 bytes of the fresh entropy packet, so the guest can
reject an unchanged clone identity, reseed Linux, refresh runtime identifiers,
and acknowledge the input gate without another PMIO transfer.

A microVM template may opt into restore-time vCPU activation by booting with an
explicit canonical `maxcpus=1`, `2`, `4`, or `8` value below or equal to its
configured VP capacity. The snapshot records that boot-online count while its
topology, APIC IDs, and saved VP inventory remain fixed at capacity.
`--restore-processors <COUNT>` requests the contiguous online prefix
`0..COUNT-1` and must satisfy `boot-online <= COUNT <= capacity`. The guest
onlines and verifies that prefix before acknowledging the restore gate.
On MSHV, an explicit target instantiates and binds only that VP prefix; saving
such a reduced-prefix runtime is unsupported. MSHV restores without an explicit
target, and all KVM and WHP restores, instantiate the full VP capacity.
Versioned MSHV CPU contracts do not expose `IA32_TSC_ADJUST` because snapshot
state cannot preserve that register independently of `IA32_TSC`; this prevents
host-side TSC correction from appearing as per-VP firmware adjustment skew.
KVM advances snapshot downtime through each VP's TSC offset rather than an
`IA32_TSC` write. KVM's synchronization heuristic can discard sub-second
counter writes, leaving `kvm-clock` ahead of the TSC and triggering Linux's
clocksource watchdog. Relative offset updates preserve per-VP synchronization
without adding host read/write latency. KVM snapshot resume requires
`KVM_VCPU_TSC_CTRL` / `KVM_VCPU_TSC_OFFSET` support; unavailable offset access
fails restore explicitly instead of falling back to imprecise counter writes.
All backends read back the adjusted TSC before resume and reject a discarded
or incomplete downtime adjustment.
After restoring counters and advancing snapshot time, MSHV and WHP freeze
partition time and align every VP's TSC to the BSP's advanced counter before
any VP runs. The first VP run thaws time. Setting counters while time is
running would introduce inter-VP skew from host scheduling delays, which can
make Linux reject the TSC clocksource during CPU activation.
For restored WHP partitions with multiple VPs, RDTSC, RDTSCP, and
`IA32_TSC` reads additionally use one partition-reference-time epoch anchored
to the advanced BSP counter. This clock has 100-nanosecond resolution and
keeps unmodified guest counters synchronized after time resumes; equal
frozen WHP register values alone do not guarantee equal live counters.
RDTSCP retains the guest's TSC_AUX value, and timestamp access restrictions
remain enforced. Explicit guest writes to `IA32_TSC`, or changes to
`IA32_TSC_ADJUST`, retain native WHP counter and deadline-timer semantics on
the affected VP. Reset restores the original intercept configuration.
Ordinary boots, single-VP partitions, and configurations exposing nested
virtualization to the guest retain their native counter path.
Snapshots without the explicit capture-time `maxcpus` opt-in, including legacy
snapshots, reject a restore target. This is not a post-readiness hotplug API and
cannot add VPs absent from the saved topology.

```admonish warning
Versions 3 through 5 do not contain or validate embedded checksums for
`state.bin` or `memory.bin`. Restore still requires regular files, bounded
manifest and state decoding, exact artifact lengths, and a compatible machine
contract, but same-length payload changes are not detected. Paired
`scratch.img` does have an exact length and SHA-256 identity because it must
match captured guest filesystem state. Protect snapshot directories with host
access controls. Integrity or authentication for export and transport must be
supplied outside the default snapshot format.
```

```admonish note
The `--memory` and `--processors` values must match the values recorded in
the snapshot manifest. If they do not match, OpenVMM will report a
validation error and refuse to start.
```

## Device configuration on restore

For standard-machine snapshots, device flags must still be supplied on restore
and must reproduce the saved machine. For microVM snapshots, the manifest is
authoritative for RAM, topology, ABI, fixed devices, placement,
features, interrupts, and the effective Linux direct command line. Restore-time
guest-visible overrides are rejected.

The CPU contract records the effective CPUID/XSTATE surface and TSC frequency.
Restore recreates and validates that rate before any vCPU runs. KVM snapshots
likewise require the destination to reproduce their saved backend CPU and
clock contract.

For a microVM boot configured with `--snapshot-destination`, OpenVMM adds
the backend TSC frequency to the effective kernel command line so the captured
guest clock matches this contract. Ordinary boots that cannot publish a
snapshot retain the guest's normal TSC discovery path.

All cold microVM boots also receive `lapic_timer_hz=<Hz>` when the backend
reports its LAPIC clock frequency. The NVX kernel uses this authoritative rate
instead of verifying a counting LAPIC against scheduling-sensitive emulated
PIT interrupts. TSC-deadline timers are unchanged. The parameter is canonicalized
before device discovery and `--`; conflicting, duplicate, malformed, or
out-of-range values are rejected. Platform snapshot validation checks a saved parameter
against its APIC frequency contract, while snapshots without the parameter
remain supported.

Every snapshot records a complete state-unit inventory. Each emulated device
saves state under a unique name (for example `"pit"`, `"vmbus"`, or `"ide"`),
and restore requires the saved and current inventories to match exactly. A
microVM manifest additionally records and validates the exact device inventory
and order.

For a phase-3 virtio console, the manifest also records its stable attachment
ID, canonical endpoint identity, reconnect policy, requiredness, and timeout.
Native socket, pipe, terminal, and file handles are never serialized. Restore
recreates listeners, reconnects required clients, or requires an inherited
replacement before starting the partition. Accepted host input and a partial
guest transmit offset live in the device-private virtio payload, preserving
their order across a new-process restore. Host input is gated before the vCPU
snapshot boundary and resumed only if capture rolls back.

If establishing the snapshot boundary returns an error after vCPU stopping
begins, OpenVMM keeps host input gated and stops and tears down the VM instead
of attempting a live rollback. This applies to both capture and the
post-restore acknowledgement boundary. Teardown still depends on the affected
backends responding; it does not provide a bounded shutdown deadline.

For microVM virtio-fs, the manifest always records the fixed, guest-discoverable
slot. A dormant slot has no host attachment or filesystem policy and carries
explicit dormant device-private state. An active slot also records the stable
attachment ID, exact canonical host path, pinned root identity, guest mount
target, access mode, no-DAX queue policy, and `live-revalidate` restore mode.
Its device-private payload records FUSE negotiation, namespace IDs and aliases,
lookup counts, reopenable handles, bounded directory-entry snapshots and
cookies, and queue progress. Native file descriptors and Windows handles are
never serialized.

Restoring an active slot requires a fresh
`--mount <GUEST_TARGET,HOST_PATH[,ro|rw]>` attachment with the same canonical
host path, target, and mode. OpenVMM independently validates the root and every
saved object identity before starting a vCPU. A dormant-slot snapshot may
restore without an attachment or bind a new one. For a new attachment, the
resumed guest explicitly mounts tag `microvm`; the cold-boot mount hook does not
run again.

```admonish warning
The host directory is external live state, not snapshot content. Host
mutations after capture can change restored reads or make restore fail. A
read-write restore also changes the shared host directory, and restoring the
same VM snapshot again does not roll those changes back.
```

The rules are:

| Scenario | Result |
|---|---|
| Device set matches exactly | Restore succeeds |
| Dormant microVM virtio-fs slot becomes attached | **Restore succeeds** — the only additive transition |
| Snapshot contains a device not in current config | **Restore fails** — unknown unit name |
| Current config has a device not in snapshot | **Restore fails** — inventory mismatch |

In practice this means:

- You must pass the **same device flags** on restore as you did on save.
  Removing a device that was present at save time will cause restore to
  fail.
- Adding a new device that was not present at save time fails inventory
  validation rather than starting an unenumerated device in its default state.

```admonish warning
Inventory errors identify the saved and current state-unit lists. Compare the
restore configuration with the capture configuration when restoring a standard
machine.
```

## Device save/restore support

Not all devices support save/restore. If a VM includes a device that does
not support saving, the `save-snapshot` command will fail with
`SaveError::NotSupported`.

The following table summarises support for the device types relevant to
OpenVMM snapshots:

| Device | Bus | Save/Restore |
|---|---|---|
| PIT, PIC, I/O APIC, DMA | Chipset (ISA) | Yes |
| CMOS RTC, Power Management | Chipset (ISA) | Yes |
| i8042 (PS/2 keyboard/mouse) | Chipset (ISA) | Yes |
| Serial 16550 | Chipset (ISA) / PCI | Yes |
| UEFI firmware | Chipset (MMIO) | Yes |
| Framebuffer | Chipset (MMIO) | Yes |
| TPM | Chipset (MMIO) | Yes |
| IDE controller | PCI | Yes |
| PIIX4 bridges, bus, PM, RTC | PCI | Yes |
| Generic PCI bus | PCI | Yes |
| StorVsp (SCSI) | VMBus | Yes |
| NetVsp (NIC) | VMBus | Yes |
| Shutdown / Timesync / KVP ICs | VMBus | Yes |
| VMBus Keyboard / Mouse / Video | VMBus | Yes |
| Guest Emulation Log | VMBus | Yes |
| virtio-blk | Virtio (PCI/MMIO) | Yes |
| virtio-net | Virtio (PCI/MMIO) | microVM only |
| virtio-pmem | Virtio (PCI/MMIO) | Yes |
| virtio-rng | Virtio (PCI/MMIO) | Yes |
| virtio-console | Virtio (PCI/MMIO) | Yes |
| NVMe | PCI | **No** |
| VGA | PCI | **No** (`todo!()`) |
| GDMA (MANA network) | PCI | **No** (`todo!()`) |
| PCIe root complex / switch | PCIe | **No** |
| Assigned PCI (pass-through) | PCI | **No** |
| Relayed vPCI | PCI | **No** |
| PCAT BIOS firmware | Chipset (ISA) | **No** (see limitations) |
| virtio-fs | Virtio (MMIO) | microVM HostFs only |
| virtio-9p | Virtio (PCI/MMIO) | **No** |
| Guest Crash Device | VMBus | **No** |
| Guest Emulation Device (GED) | VMBus | **No** |
| VMBus serial (host) | VMBus | **No** |
| Vmbfs | VMBus | **No** |

```admonish tip
If you are unsure whether your VM configuration supports snapshots, try
issuing `save-snapshot` to a scratch directory. The save will fail
immediately with a clear error if any active device does not support it.
```

## Limitations

- Snapshots are **not portable** across architectures (e.g., you cannot
  restore an x86_64 snapshot on aarch64)
- Restores use private copy-on-write RAM, so a clone-policy snapshot can be
  restored repeatedly without copying it or modifying `memory.bin`.
  Instance-checkpoint snapshots permit one restore attempt.
- VMs using VPCI or PCIe devices do not currently support save/restore
- OpenHCL-based VMs do not currently support this snapshot mechanism
- VMs using PCAT firmware do not support save/restore
- Standard-machine restore still requires matching `--memory` and
  `--processors`. MicroVM restore reads them authoritatively from the manifest
  and rejects overrides. Persisted microVM ABI and boot-layout value 2 are
  supported.
