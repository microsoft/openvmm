# Snapshot Format

This page documents the on-disk format used by OpenVMM snapshots, intended
for developers working on the save/restore subsystem.

## Directory layout

A snapshot is stored as a directory containing three required files and, for
a mounted microVM scratch capture, one paired scratch image:

```text
snapshot-dir/
├── manifest.bin   # Protobuf-encoded SnapshotManifest
├── state.bin      # Protobuf-encoded device saved state
├── memory.bin     # Exact automatic RAM file or independent supplied-RAM clone
└── scratch.img    # Optional paired microVM writable scratch image
```

## Manifest format

The manifest is a protobuf message defined as
[`SnapshotManifest`](https://openvmm.dev/rustdoc/linux/openvmm_helpers/snapshot/struct.SnapshotManifest.html)
in `openvmm/openvmm_helpers/src/snapshot.rs`, encoded using the `mesh`
crate's protobuf encoding.

Every snapshot uses manifest version 2 with format magic
`OPENVMM_SNAPSHOT_V2\0`. Restore rejects any other version or magic with
`E_SNAPSHOT_VERSION`, which asks for a recapture. It decodes the version,
field 1, and then the magic, field 10, each alone before the rest of the
manifest, since earlier formats give some fields other types: version 6, the
manifest of earlier releases, keeps its machine contract in field 11, and the
first version 2, which has no magic, keeps artifact digests in fields 9 and 10.
A microVM snapshot's machine contract carries the NVX time ABI records (the
time contract and the CPU profile record), and a host-driven standard-machine
snapshot has no machine contract. Tiered microVM snapshots record capture
tier, clone/resume policy, and consumed configuration sections. Field 18 holds
the snapshot ID that SaveVM and the command line's `save-snapshot` generate,
which RestoreVM can require; a microVM capture records the zero ID, which
stands for none.

The format is a local machine-state contract, not an authenticated container.
Every snapshot receives the same regular-file, no-follow/no-reparse, bounded
decoding, exact-length, inventory, and machine-contract validation, but the
on-disk format does not authenticate same-length payload changes. A
`workload-start` or `instance-checkpoint` snapshot binds its sandbox blocks,
the read-only layers and the paired `scratch.img`, which guest RAM and a
mounted writable filesystem must restore as one exact pair, to one
`generation` identity: a caller-authenticated immutable storage generation,
not a byte digest. Neither capture nor restore reads a block to identify it:
OpenVMM validates role, size, geometry, and the shared generation, and a
restore requires the generation that the snapshot recorded. Restore rejects
the `sha256` block identity of earlier releases, whose snapshots must be
recaptured. Export or transport layers must provide broader integrity and
authentication outside this format.

The optional microVM network contract stores the SHA-256 digest of its bound
egress policy in encoding version 1, which covers the static IPv4 and IPv6
identities, guest MAC, policy rules, canonical ARP next-hop set, host-loopback
action and proxy endpoint, and ingress action. A platform snapshot, or a
snapshot without a tier, whose digest is that of the complete deny-all policy
bound to its identity is a NIC template, whose restore may install another
policy with `--restore-network-policy`, and the format records nothing else for
it.

## Device state (`state.bin`)

The device state contains every device's saved state, collected via the
`SaveRestore` trait and encoded as a `mesh` protobuf message. The
[Save State](contrib/save-state.md) compatibility rules (mesh tag stability,
default values, forward/backward compatibility) apply.

## Memory (`memory.bin`)

For automatically allocated microVM RAM, `memory.bin` is a hard link to the
exact OpenVMM-owned backing handle. State and manifest are written and flushed
first; OpenVMM then flushes the stopped guest's shared RAM mappings and exact
backing handle, creates the link last, and verifies the linked handle's file
identity and EOF before the staging directory is renamed into place. Linux
prefers `linkat(AT_EMPTY_PATH)` and can use a `/proc/self/fd` link with a
device/inode proof. Windows uses handle-relative `FileLinkInformation` and
verifies `FILE_ID_INFO`. Filesystems that cannot create the link fall back to
an independent sparse-aware copy.

User-supplied RAM always uses the independent-copy path, never a hard link.
Clone support is used when available, with allocated-range or zero-scan copying
as a fallback on Linux; Windows uses a dense copy. Both paths use an
already-open handle rather than reopening its pathname, so replacing the source
path cannot substitute different bytes.
Successful guest-requested capture is terminal for the source. If publication
fails, OpenVMM removes the private staging directory, its manifest first, and
allows rollback only once it has proved the removal of any staged manifest, so
that restore cannot open what remains, and, once it has tried to create the
automatic link, of the complete directory. On Linux, proving a removal
includes flushing its directory. Uncertain cleanup makes the source terminate
instead of resume.

Restore likewise opens the snapshot directory once and resolves its artifacts
relative to that handle. Windows uses read-only handles with `FILE_SHARE_READ`
only, rejects reparse points, compares `FILE_ID_INFO` and EOF before and after
creating the private COW section, and keeps the directory and artifact guards
in the VM worker until teardown. Linux opens each artifact with `O_NOFOLLOW`
and `O_NONBLOCK`, so that a FIFO without a writer, for which a blocking open
would wait, reaches the check of the opened file's type and is refused like
any other file that is not a regular file, and then makes a regular file's
descriptor blocking again. It keeps the exact directory and regular-file
descriptors, and before handoff rejects a change of the device, inode, length,
or modification time of the opened manifest, state, or memory file, so
renaming or replacing the original path cannot substitute another generation.
A change of their links, mode, or owner alone passes, which lets a restore
open a snapshot before the capture's source, as it tears down, removes its RAM
file's temporary name, the other link of an automatic `memory.bin`.
Linux file descriptors do not provide mandatory write exclusion; deployments
that need authenticated or write-proof local artifacts must add a stronger mode
such as a lease, fs-verity, or a verified artifact broker.

## Scratch (`scratch.img`)

The microVM block contract records every fixed role, access mode, geometry,
identity kind, identity, and paired-scratch restore mode. A paired capture
copies the exact opened writable scratch handle into the same staging
directory after device queues drain and publishes it atomically with VM state.
Every bound block carries the same nonzero caller-authenticated storage
generation, and publication and restore check `scratch.img` structurally and
by exact size, without reading it to verify its content. A platform capture
binds no block, so its read-only layers are `unbound`.

Paired scratch defaults to `private-copy`, which preserves existing behavior
and may use a reflink or sparse copy. `copy-on-write` requires a filesystem
reflink and is restricted to reusable clone snapshots. `direct-claimed`
attaches the paired file only for a single-use resume snapshot after its
durable claim; the claimed artifact is then continuation storage rather than
an available snapshot. A missing restore-mode field retains `private-copy`
behavior.

A pre-mount capture instead records the `fresh` scratch policy and geometry,
contains no `scratch.img`, and requires a new scratch file of that geometry on
restore, unless `--restore-absent-scratch` declares the scratch absent:
OpenVMM then attaches a writable scratch device of that geometry with no file,
which reads zeros and fails every write.

## Code references

- Manifest type and I/O: `openvmm/openvmm_helpers/src/snapshot.rs`
- Format validation, publication, restore-side access, file-system helpers,
  and the microVM machine contract: `openvmm/openvmm_helpers/src/snapshot/`
- Restore entry point: `prepare_snapshot_restore()` in
  `openvmm/openvmm_entry/src/snapshot_restore/prepare.rs`
- File-backed memory: `SharedMemoryFd` type alias in
  `openvmm/openvmm_defs/src/worker.rs`

## Device state architecture

Each VM component that participates in save/restore is registered as a
"state unit" with a unique string name via `StateUnits::add("name")`.
During save, every state unit receives a `StateRequest::Save`. Units that
have state return `Ok(Some(blob))`; units with no persistent state (e.g.
the input distributor) return `Ok(None)` and are omitted from `state.bin`.

The resulting `state.bin` contains a `Vec<SavedStateUnit>`, where each
entry pairs a unit name with its opaque protobuf-encoded state blob.

### Restore matching rules

During restore, `StateUnits::restore()` matches saved-state entries to
currently registered units **by name**:

| Scenario | Result |
|---|---|
| Names match exactly | State is dispatched to the unit |
| Saved entry has no matching unit | **Error** — `unknown unit name` |
| Unit exists with no saved entry | Unit is skipped (keeps default state) |
| Duplicate name in saved state | **Error** — `duplicate unit name` |

This means removing a device between save and restore will fail, but
adding a new device is allowed (it initialises to its power-on defaults).

### Unit naming conventions

- **Chipset devices** — registered via `arc_mutex_device("name")` in
  `vmotherboard`, e.g. `"pit"`, `"rtc"`, `"uefi"`, `"ide"`.
- **VMBus devices** — named `"{interface_name}:{instance_id}"`, e.g.
  `"StorageVsp:ba6163d9-..."`. The instance GUID makes each offer
  unique.
- **Infrastructure units** — `"vmtime"`, `"input"`, `"vmbus"`.

### Devices that do not support save/restore

Not all devices implement save/restore. Devices signal this in one of
two ways:

1. **`SaveError::NotSupported`** — the `save()` method returns this error.
   If any state unit does this, the entire save operation fails.
2. **`supports_save_restore() -> false`** (virtio) or
   `supports_save_restore() -> None` (VMBus) — transport-level check
   that causes the transport's `save()` to return
   `SaveError::NotSupported`.

Key unsupported categories:

- **PCIe** — `GenericPcieRootComplex`, `GenericPcieSwitch` return
  `SaveError::NotSupported`.
- **NVMe** — `NvmeController` returns `SaveError::NotSupported`.
- **Pass-through PCI** — `AssignedPciDevice`, `RelayedVpciDevice`.
- **VGA / GDMA** — marked `todo!()` (will panic on save).
- **Virtio devices** — the `VirtioDevice` trait defaults
  `supports_save_restore()` to `false`. `virtio-blk`, `virtio-console`,
  `virtio-pmem`, `virtio-rng`, and `virtio-rtc` override it to `true`.
  `virtio-net` enables it only for resources with an explicit static identity
  and feature contract, such as the microVM NIC; ordinary virtio-net resources
  remain disabled.
  The transport stores an opaque typed device-private payload in addition to
  common queue state. Devices with unsupported host-side session state, such
  as `virtio-9p`, leave save/restore disabled. `virtiofs` enables typed
  device-private state only for the constrained microVM HostFs profile;
  ordinary, aggregate, and SectionFs resources remain disabled.
- **Some VMBus devices** — `GuestCrashDevice`, `GuestEmulationDevice`,
  `VmbusSerialHost`, `Vmbfs` return `None` from
  `supports_save_restore()`.

## VP state elements

Each VP's saved state is a `virt.x86.VpSavedState`
(`vmm_core/virt/src/x86/vp.rs`) with one optional field per element, such as the
registers, the APIC, and `IA32_TSC_AUX`, each under a field number of its own. A
save holds an element only where the partition has it, as for an MSR that the
guest's CPUID enumerates, and a restore sets only the elements that the saved
state holds, before any VP runs.

An element that a later build adds needs no version, because the field is
optional and the rules of [Save State](contrib/save-state.md) apply to it:

- A saved state from a build that predates the element has no such field, so
  restoring it leaves the VP's state as a new VP has it, the reset value, as it
  did before the element.
- A build that predates the element ignores the field in a saved state from a
  later build, so the state is lost there too.

Restoring across such builds therefore never fails on the element, but it can
lose what the element preserves, silently. There is no version to check, and no
reader refuses a saved state for an element that it does not know. Never change
or reuse the number of an element's field. `IA32_TSC_DEADLINE`, in field 15, and
`IA32_SPEC_CTRL`, in field 16, are such elements. A guest whose CPUID enumerates
IBRS, STIBP, or SSBD keeps its speculation controls across a restore, but one
restored from a snapshot of an earlier build, or by an earlier build, starts
with them cleared, as every guest did before, until it next writes the MSR.

The elements of this kind after `IA32_TSC_DEADLINE`, each a guest-writable MSR
or group of MSRs that a VP holds and a fresh VP does not:

| Field | Element | State | A save holds it when |
| --- | --- | --- | --- |
| 16 | `spec_ctrl` | `IA32_SPEC_CTRL` | the guest's CPUID enumerates IBRS, STIBP, or SSBD |
| 17 | `umwait_control` | `IA32_UMWAIT_CONTROL` | the guest's CPUID enumerates WAITPKG |
| 18 | `debug_ctl` | `IA32_DEBUGCTL` | the backend has it (KVM) |
| 19 | `misc_msrs` | `IA32_MISC_ENABLE`, `MSR_MISC_FEATURES_ENABLES` (CPUID faulting), and `IA32_POWER_CTL` | the backend has them (KVM) |
| 20 | `machine_check` | `IA32_MCG_STATUS`, `IA32_MCG_CTL`, and the control, status, address, miscellaneous, and CMCI control registers of each machine-check bank | the backend has them (KVM) |

A partition has an element only where the guest can have the state and the
backend can access it, which the partition's capabilities record. The guest's
CPUID decides the first for `spec_ctrl` and `umwait_control`; no CPUID bit
decides it for the other elements, whose MSRs every CPU has. For the KVM
backend:

- It can access an MSR only if the host's `KVM_GET_MSR_INDEX_LIST` lists it, so
  a host without the MSR, such as one without WAITPKG, holds no element for it
  instead of failing every save.
- It can access none of them for an SNP guest, whose registers live in an
  encrypted VMSA.
- `IA32_DEBUGCTL` is the exception to the list. Every KVM virtualizes it, and
  `KVM_GET_MSRS` and `KVM_SET_MSRS` reach it on every kernel that the time ABI
  supports, Linux 5.16 and later: the vendor code fails a write only for a bit
  that KVM does not support (VMX, which drops BTF and LBR instead) or that is
  above bit 5 (SVM, which ignores the write without LBR virtualization). So 0
  always restores, and so does what a guest holds, unless a newer kernel gave it
  a bit, as Linux 6.17 gave it `RTM_DEBUG`, and the restore into an older kernel
  fails. A kernel lists the MSR only since commit `3700f0788da6` ("KVM: SVM: Add
  missing save/restore handling of LBR MSRs"), in Linux 7.1 and the stable
  kernels that took it, such as 6.18.57, which found that KVM enumerated neither
  `IA32_DEBUGCTL` nor the LBR MSRs for userspace to save. Following the list
  would make a snapshot restore only on such a kernel, though almost every guest
  holds 0.
- KVM does not list the registers of the machine-check banks, so `machine_check`
  follows `IA32_MCG_STATUS`, and the number of banks is the one that the VP's
  `IA32_MCG_CAP` reports.

The MSHV, WHP, and OpenHCL backends do not have elements 17 through 20, and
clear their capabilities.

A saved state restores only into a partition that has every element that it
holds. A destination whose backend lacks one, such as a host whose KVM does not
list `IA32_UMWAIT_CONTROL`, cannot give the guest the value that it saved, and
`restore_all` does not drop the element: it checks every element of the saved
state against the partition before it sets any of them, and refuses the restore
with an error, in the phase `restore check`, that names the first element that
the partition lacks. The refusal changes none of that VP's state, and the
restore of the VM state checks its elements the same way. A snapshot reaches it
only when it was edited or when the destination's backend cannot access an MSR
that the capturing host's could, since a restore requires the backend and the
CPU generation of the capture.

A restore of the machine-check state sets the banks that the saved state lists
and puts the VP's other banks back as `bind` left them: with every error enabled
on a host that advertises CMCI, and disabled otherwise. It refuses a saved state
that lists more banks than the VP has before it writes anything. The restore
also fails for a state that holds a value that KVM refuses, such as a bank
control other than 0 or all ones (bit 0 and bit 10 may be clear), which no guest
writes, after it has written the registers that precede it.

Some guest-writable state has no element:

- `IA32_BNDCFGS` belongs to MPX, which only the Skylake-SP CPU profile
  enumerates. Since Linux 5.6, the kernel does not enable the MPX state
  components in the host's XCR0, which KVM needs to expose MPX, so that CPU
  profile cannot run on a KVM host that the time ABI supports, which needs
  Linux 5.16.
- `MSR_VM_HSAVE_PA` belongs to SVM, which no CPU profile enumerates. A guest
  without SVM cannot run VMRUN, so nothing uses the MSR.
- AMD's legacy performance counters, `0xc0010000` through `0xc0010007`, hold
  state only on an AMD host whose KVM virtual PMU is on. A time ABI guest has no
  PMU, so the KVM backend raises #GP for them instead (see [Time and CPU
  compatibility](../user_guide/openvmm/snapshots.md#time-and-cpu-compatibility)),
  and a snapshot of another guest does not hold their state.

A snapshot from an earlier build holds none of elements 17 through 20, so a
restore leaves those MSRs as `bind` set them, as it did before:
`IA32_MISC_ENABLE` at the Microsoft hypervisor's value, the machine-check banks
as a new VP has them, and the other MSRs at 0. An earlier build ignores the
elements of a later snapshot in the same way.

### Fields of an element

An element can gain an optional field under the same rules. The activity
element (`virt.x86.Activity`, field 2) holds a VP's MP state, its pending and
blocked NMIs, its interrupt shadow, and the event and interruption that it was
about to take, and it has two optional fields: the count of pending NMIs and the
kind of the interrupt shadow. KVM can hold two NMIs pending, when a second NMI
reaches a VP before it has run, which the Boolean `nmi_pending` cannot say, so
the activity records the count in its optional field 7, `nmi_pending_count`,
when it is 2:

- A saved state from a build that predates the field has none, and nor has a
  state that holds at most one pending NMI, so restoring it sets the one
  pending NMI that `nmi_pending` gives, as before. Only a KVM VP that holds two
  pending NMIs saves the field, and only on Linux 6.4 and later, whose
  `KVM_GET_VCPU_EVENTS` reports the count: an earlier kernel reports whether an
  NMI is pending, so a save on it holds one and loses a second NMI, as before.
- A build that predates the field ignores it in a saved state from a later
  build, so it restores two pending NMIs as one: the guest loses an NMI,
  silently.

There is no version to check, and no reader refuses the field. Never change or
reuse the number of a field.

A restore never drops a count that it reads. A backend that holds at most one
pending NMI refuses the activity, and KVM does for a count that it would cut
down to what it holds: anything but 2, or 2 with NMIs blocked or an NMI being
injected. Neither changes any state before it refuses, and only a damaged or
crafted snapshot reaches either, since a snapshot restores only on the backend
that captured it.

An exception needs no field. KVM reports one that it has queued but not injected
yet as pending and injected both, because OpenVMM does not enable
`KVM_CAP_EXCEPTION_PAYLOAD`, and restores an exception only as injected, so a
save records it as the activity's interruption alone and holds no event for it:

- A saved state from a build that predates this holds the exception as both the
  event and the interruption, and restoring it injects the exception once, as
  before.
- A build that predates this restores a later state's interruption as the
  injected exception, as it restored the pair.

A restore refuses any other pending exception, which KVM would drop, before it
changes the activity. Only a damaged or crafted snapshot holds one.

KVM on an Intel CPU can hold an interrupt shadow of either kind, which the
Boolean `interrupt_shadow` cannot say: the shadow that a `MOV SS` or `POP SS`
leaves also blocks NMIs and suppresses certain debug exceptions at the next
instruction boundary, and the one that an `STI` leaves, with `IF` clear, does
not. A restore of a shadow that is given no kind sets a `MOV SS` shadow, which
errs on the safe side, and a restore of no shadow sets none, so the activity
records the kind in its optional field 8, `interrupt_shadow_kind`, when the
shadow is an `STI` shadow:

- A saved state from a build that predates the field has none, and nor has a
  state whose shadow is a `MOV SS` shadow, or that has no shadow, so restoring
  it sets a `MOV SS` shadow if it has a shadow, and none if it has none, as
  before, and a save encodes every such state byte for byte as an earlier build
  did. Only KVM on an Intel CPU saves the field, for an `STI` shadow. An AMD CPU
  has one kind of shadow, and the interrupt state register of MSHV and WHP has
  one bit for both, so they save none.
- A build that predates the field ignores it in a saved state from a later
  build, so it restores an `STI` shadow as a `MOV SS` shadow, as before: an NMI
  that the `STI` shadow would let through waits for one more instruction, and an
  instruction breakpoint at that instruction, which a `MOV SS` shadow suppresses
  on an Intel CPU and an `STI` shadow does not, is lost.

A restore never drops a kind that it reads, but into AMD's one shadow. A backend
that cannot hold the kind, MSHV, WHP, or OpenHCL's, refuses the activity, and
KVM refuses a kind without a shadow. None changes any state before it refuses.
KVM on an AMD CPU sets its one shadow for the kind, which reads back with none.
Only a damaged or crafted snapshot reaches any of them: a snapshot restores only
on the backend and the CPU generation that captured it, and an AMD capture
records no kind.

## Extending the format

When adding new fields to `SnapshotManifest`, use the next available mesh
tag number other than 18, 19, and 20: upstream OpenVMM assigns field 18 to
the `snapshot_id` that its SaveVM API records, and reserves fields 19 and 20
for `compatibility_bundle_digest` and `memory_artifacts`.
The protobuf encoding is forward-compatible: older readers will
ignore unknown fields. However, removing or reordering existing fields is a
breaking change. See [Save State](contrib/save-state.md) for the full set of
compatibility rules.

```admonish warning
Changing the mesh tag numbers of existing fields will break compatibility
with previously saved snapshots.
```
