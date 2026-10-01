# Timekeeping

This page explains the clocks and timers used by OpenVMM and OpenHCL, how
they relate to guest-visible time, and what happens when execution stops.

There is no single "VM clock". A guest can read a processor counter, read a
hypervisor reference counter, ask firmware for the date, and receive a
timesync message. These interfaces have different origins, owners, and
pause behavior. A timer also has state beyond the clock it reads: its
deadline, periodic phase, and any interrupt or message awaiting delivery.

## Clock domains

| Domain | Representation | Purpose |
| --- | --- | --- |
| Host monotonic time | `pal_async::timer::Instant`, nanoseconds | Scheduling host tasks and implementing elapsed-time clocks. Not a date. |
| Host system time | `SystemTime` or `jiff::Timestamp` | Calendar time, subject to host clock adjustments. |
| Software VM time | `vmcore::vmtime::VmTime`, 100 ns ticks | Pausable timeline used by software device emulation. |
| Partition reference time | `ReferenceTimeSource`, 100 ns ticks | Hyper-V reference-counter reads, synthetic timer deadlines, and correlation with UTC. Its source depends on the backend. |
| Architectural counters | x86 TSC; Arm physical/virtual counters | Guest instruction-visible counters, with architecture-specific frequencies and virtualization offsets. |
| Device wall-clock time | `LocalClockTime`, milliseconds since the Unix epoch | Reprogrammable calendar time for RTC and firmware services. |
| Guest OS time | Maintained by the guest | The OS derives monotonic and wall-clock time from available counters and synchronization services. |

Equal units do not imply equal clocks. In particular, `VmTime` and
partition reference time both use 100 ns ticks, but generally have
different origins. TSC ticks are not nanoseconds or CPU instruction cycles;
you need the counter's frequency to convert them to a duration.

Here, "host" means the OS running the relevant userspace component. For
hosted OpenVMM that is the host OS; for OpenHCL userspace it is the Linux
environment in VTL2, not the root partition.

```text
Host monotonic clock
  +-- host task deadlines (PolledTimer)
  +-- software VmTime, with a running/stopped mapping
        +-- emulated device counters and timer deadlines
        +-- Hyper-V reference time in some emulated configurations

Architectural counter / hypervisor clock
  +-- guest TSC or Arm counter
  +-- partition reference time in native and counter-backed configurations

System time / platform-provided UTC
  +-- LocalClock --> RTC or UEFI calendar time
  +-- UTC + partition-reference sample --> timesync message --> guest OS
```

The branches describe possible sources, not universal wiring. Clock
offsets and scales cannot be inferred from the counters' numeric values.

## Software VM time

[`VmTimeKeeper`][vmtime] owns a software timeline. Hosted OpenVMM creates it
stopped at zero. While running, its value is the VM time at the last start
plus elapsed host monotonic time. Stopping captures a fixed value;
starting again establishes a new host-time anchor without adding the
stopped interval. Reset sets it to zero, and restore installs a saved VM
time rather than an old host timestamp.
This measures elapsed time, not scheduled VP runtime: it advances while
guest CPUs are idle or descheduled unless the keeper is stopped.

The underlying [`pal_async` clock][host-time] uses `CLOCK_MONOTONIC` on
Unix and `QueryUnbiasedInterruptTimePrecise` on Windows. A VM pause,
host-system sleep, and guest CPU idle are different events: VM pause is
handled by the keeper, while host sleep behavior follows the underlying
OS clock. Do not use these timestamps as UTC.

Devices obtain a `VmTimeAccess` from a `VmTimeSource`. Each accessor can
read time and register a deadline; the backing task translates VM
deadlines into host timer deadlines while running. This lets a virtual
device wait without advancing its guest-visible timeline during a pause.
An overdue deadline still needs the consumer to run and process it:
deadline expiration is not the same as guest interrupt delivery.

`VmTimeSourceBuilder` provides synchronized cross-process access over
mesh, but requires a shared OS monotonic clock. It cannot synchronize
different machines or an OpenVMM host with OpenHCL. Stop reconciles the
secondary keepers to a common value without moving accessors backward.

The keeper saves the timeline value; devices save their own timer state.
Monotonicity does not extend across reset or restore of an older snapshot.

## Partition reference time and processor counters

[`ReferenceTimeSource`][reference-time] is a read interface, not a clock
controller. A read returns a reference count and may also return a UTC
timestamp captured at the same instant, allowing the two values to be
correlated. The reference count is not itself UTC. A missing UTC value means
that the source cannot cheaply provide the correlated timestamp.

For Hyper-V-compatible guests, the reference counter is a partition-wide
elapsed-time interface. On x86, guests can read `HV_X64_MSR_TIME_REF_COUNT`.
On Arm, the corresponding synthetic register is `HvRegisterTimeRefCount`,
accessed through a hypercall when the backend supports that interface.
The reference-TSC page provides an alternative that avoids an intercept:

```text
reference_time = high64(virtual_tsc * scale) + offset
```

The guest must follow the [sequence-number protocol][tlfs-timers]. A zero
sequence requires fallback to the reference counter: advertising the
facility does not mean that the page is usable at every instant.

The shared `hv1_emulator` accepts an explicit reference-time source. It
publishes a usable reference-TSC page only when configured for its
TSC-backed, zero-offset model; otherwise it leaves the sequence invalid.
A `VmTime`-backed reference counter therefore does not automatically
provide a direct TSC fast path.

Architectural counters have their own state. Changing reference time
does not inherently change x86 `RDTSC`, an Arm counter offset, or a timer
deadline expressed in counter ticks. Similarly, stopping a VP thread or
leaving a hypervisor run call does not by itself freeze those counters.

## Timer implementations

Choose the clock by the guest-visible contract, not by whichever timer
API is easiest to call.

| Timer or counter | Implementation and clock |
| --- | --- |
| PIT | `chipset::pit` advances counting elements from elapsed `VmTime` and schedules wakeups for interrupt generation. |
| ACPI PM timer | Software reads scale `VmTime` to 3.579545 MHz. With PM timer assist, the hypervisor handles reads using its own time source instead. |
| CMOS RTC | Calendar registers use `LocalClock`; alarm, periodic, and update interrupt scheduling uses `VmTime`. |
| UEFI watchdog | `watchdog_core` receives a `VmTimeAccess`, so its deadline belongs to the software VM timeline. |
| Software local APIC | `virt_support_apic` counts down using `VmTime`; backend-native APIC emulation instead has backend-owned timer state. |
| Hyper-V synthetic timers | Deadlines use partition reference time. Native implementations belong to the hypervisor; `hv1_emulator::synic` evaluates them in software. |
| Arm architectural timers | Compare architectural counter values with programmed deadlines; backend-specific code handles virtualization and interrupt delivery. |
| Host work timers | `PolledTimer` waits on host monotonic time. It has no implicit association with VM pause. |

Synthetic timers can be one-shot or periodic. In the shared emulator, a
one-shot count is an absolute reference-time deadline; a periodic count
is an interval. Expiration can request a direct interrupt or post a
SynIC message. Masking, a busy message slot, and VP scheduling can delay
delivery after the deadline.

This distinction matters during save/restore. Restoring a clock value
alone does not restore a periodic timer's phase, retract an already
pending interrupt, or preserve an undelivered message. Conversely,
blindly reloading a timer can discard events that were already pending.
Backend state interfaces and device saved state must be considered
together.

## Calendar time and synchronization

### RTC and UEFI

[`LocalClock`][local-clock] is an instance-local, reprogrammable calendar
clock. Setting it changes that instance, not the host's system clock.
Unlike `VmTime`, its intended behavior is to account for elapsed
wall-clock time while the VM is paused.

Hosted RTC devices use `SystemTimeClock`, which returns host
`SystemTime` plus an offset. Guest writes change that offset. Host
calendar-clock adjustments, including backward jumps, remain visible.
The default UEFI time source is also a `SystemTimeClock`; independently
created clock instances do not share guest-written offsets.

RTC dates can advance during a pause while `VmTime`-based interrupt
machinery is stopped. Periodic interrupt rates do not define wall time.

On Arm, the UEFI time service exposes the supplied local clock through
`GetTime`/`SetTime`. It retains timezone and daylight fields for readback
but does not apply them as timezone conversions. Its saved state stores
those fields; persistence of the backing clock is the platform's
responsibility. Similarly, CMOS register saved state is not a serialized
`LocalClock` implementation.

### Hyper-V timesync

The `hyperv_ic` timesync device sends a UTC sample paired with partition
reference time. The pair allows the guest to account for elapsed
reference time between sampling and consuming the message. This is
separate from setting the RTC or changing the partition clock.

It uses correlated UTC from `ReferenceTimeSource` when available;
otherwise it samples local system time immediately after reference time.
It waits for ring-buffer space before sampling to reduce delivery delay.

The current implementation negotiates timesync version 4, sends an
initial synchronization message, and then sends samples on a five-second
host-timer cadence while the channel runs. On snapshot restore it
renegotiates and sends a fresh synchronization message. Whether and how
the guest OS adjusts its time is a guest policy decision.

After resume, elapsed time can exclude the pause while RTC reads and
timesync report the current date. Resynchronizing calendar time does not
mean a frozen elapsed-time clock was restored incorrectly.

## Stop, resume, and saved state

The optional [`PartitionTimeControl`][time-control] interface controls
backend time separately from `VmTimeKeeper`. Supporting backends create
partitions frozen. The lifecycle contract requires stopped VPs for both
operations, makes repeated freeze/thaw calls harmless, and treats an
unexpected transition failure as fatal inside the backend.

For hosted OpenVMM, the relevant full-stop ordering is:

```text
Stop:   stop all VPs --> freeze supported backend time
                    --> stop dependent device work --> stop VmTime
Start:  start VmTime --> start dependent device work
                    --> thaw supported backend time --> allow VPs to run
```

The state-unit dependency graph establishes this ordering; it is not an
atomic freeze of every clock. Backend time and software device time can
have different stop/start anchors.

| Operation | Timekeeping consequence |
| --- | --- |
| Full VM stop | Stops software VM time and requests backend freeze where supported. Does not freeze host clocks or calendar time. |
| Temporary VP stop or debugger halt | Does not itself stop `VmTime` or freeze backend time. Timers can become due while VPs cannot execute. |
| Full VM start | Thaws backend time even if a debugger halt or outstanding temporary-stop guard still prevents VP execution. |
| Reset | Resets software VM time to zero. Supporting backends remain frozen while partition and VP state are reset. |
| Snapshot restore | Restores stopped software time and the state supported by each backend/device. A time-state setter must not implicitly thaw a supporting backend. |
| VTL scrub | May reset and freeze a backing VTL clock. The partition unit thaws it before restarting VPs; this is not a freeze of every VTL. |

The x86 `virt` model separates reference time/page, TSC, APIC, and TSC deadline.
Deadline state requires guest/backend support and restores after APIC/TSC:
zero disarms; absent state is not restored. Native MSHV/WHP and KVM support
it, unlike software APICs. Reference time is independent of Hyper-V on KVM.
Lifecycle freeze is separate; `can_freeze_time` gates exact state comparison.

Arm state coverage differs: the generic `virt` partition-state schema is
empty and its VP schema does not currently include architectural
counter/timer state. A native freeze API is not, by itself, evidence of
complete portable snapshot support.

## Hosted backend configurations

These are current implementations, not uniform `virt` guarantees.
Guest-visible Hyper-V facilities require the relevant enlightenments.

| Backend/configuration | Reference-time source | Backend freeze on full stop |
| --- | --- | --- |
| MSHV, x86 or Arm | Hyper-V partition `ReferenceTime` property | Yes, through `TimeFreeze`. |
| WHP, offloaded Hyper-V | Native WHP reference time | Yes, separately for each backing partition. |
| WHP, emulated Hyper-V on x86 | Software `VmTime` | Yes for native WHP time; `VmTime` stops separately. |
| KVM, non-isolated x86 with stable TSC and offset access | KVM clock, converted from ns to 100 ns | Yes, through software clock rebasing and LAPIC timer staging. |
| KVM, other x86 configurations | KVM clock, converted from ns to 100 ns | No. |
| KVM, Arm | No Hyper-V reference-time source exposed | No; architectural timers remain KVM-owned. |
| HVF, Arm | Software `VmTime` | No architectural-counter freeze; software time still stops. |

Only running KVM x86 returns correlated UTC, when `KVM_GET_CLOCK` supplies
`KVM_CLOCK_REALTIME`. Frozen KVM, MSHV, WHP, and HVF sources do not.

### MSHV and WHP

`virt_mshv` uses native Hyper-V time and interrupt-controller facilities.
It reads reference time through a partition property rather than a VP
register, avoiding a query that would need to wait for a running VP.
Creation freezes the partition before guest initialization; runtime
freeze/thaw updates `TimeFreeze`. Guest reference-TSC-page exposure is
capability-dependent and is currently disabled for MSHV SNP partitions.
That restriction is separate from freezing the partition.

`virt_whp` offloads Hyper-V facilities when offload is enabled, the
required WHP features are available, and a user-mode APIC is not selected.
Otherwise, its x86 software path uses `hv1_emulator` with a
`VmTime`-backed reference counter and software synthetic timers. The
user-mode APIC also uses `VmTime`. Arm does not support the user-mode-APIC
or disabled-offload options; its virtual timer PPI is configured in WHP's
GIC parameters.

WHP uses [`WHvSuspendPartitionTime`][whp-suspend] and explicit resume,
not the implicit resume performed by entering a VP. With VTL2 emulation,
VTL0 and VTL2 have distinct WHP backing partitions and frozen-state
bookkeeping. A full stop freezes both. Scrubbing VTL2 freezes only its
backing clock and, on x86, preserves its reference count around native
reset. This simulation excludes the time spent in that reset; it is not
an exact model of servicing downtime.

The WHP x86 partition-state accessors still have TODOs for locally
emulated Hyper-V state: they access native WHP state even in that
configuration. Do not infer full emulated-enlightenment snapshot
coverage from the presence of the freeze interface.

### KVM

Non-isolated x86 supports software freeze without requiring a stable TSC.
Clocks start cached at zero. With stable clock correlation and
`KVM_VCPU_TSC_OFFSET`, capture uses a common host-TSC anchor and thaw rebases
reference time and TSC together, preserving per-VP differences. This
optimization assumes identity TSC scaling, not different-frequency migration.
Otherwise, freeze captures each VP's TSC through KVM and thaw reinstalls the
cached values. Explicit offset writes remain preferred: the fallback samples
the current guest TSC and adjusts its offset by the difference to the saved
value, without requiring a known frequency or a userspace host-TSC anchor.
Capture and restoration are sequential, so this path has per-VP sampling skew.
On older kernels without offset access, restore first writes a distant
temporary TSC with VPs stopped and LAPIC timers disarmed, then the saved value.
This avoids KVM's legacy nearby-write synchronization heuristic retaining a
short pause. A saved zero is restored as one tick because a zero MSR write
unconditionally requests synchronization. Frozen state still reads as zero.
SEV-isolated partitions do not expose software freeze.

`--hypervisor kvm:force_tsc_fallback` forces per-VP capture/restore for testing
on stable hosts; it does not make the host TSC unstable. The option is carried
by the KVM hypervisor resource handle. Backend contract tests register this
configuration separately as `kvm_tsc_fallback`.
Reference time is saved/reset without Hyper-V too, rounded up to 100 ns for
snapshots. `KVM_SET_CLOCK` flags zero exclude UTC downtime.

Freeze caches deadlines privately and disarms LAPIC timers. Thaw merges
countdowns into live APIC state, preserving IRQs, then restores the deadline
MSR. Injection is serialized and irqfds briefly detached. UAPI limitations:

- Clock/timer capture is not atomic; countdown skew delays expiration.
- Zero remaining countdown cannot distinguish pending from delivered
  one-shots. Rearming nonzero initial count makes it immediately due,
  preferring a possible duplicate over a lost wakeup.
- Periodic pending ticks can coalesce; zero count restarts a full period.
- TSC-deadline mode is distinguishable: KVM clears the deadline on delivery.
- Ordinary pause leaves synthetic timers' private state intact; KVM rechecks
  deadlines after clock updates. Snapshot config/count writes wait for thaw,
  but cannot restore private periodic phase or undelivered messages.
- Older snapshots lack LAPIC TSC deadlines and unenlightened reference time.

Thus `can_freeze_time` remains false despite lifecycle freeze support.

On Arm, KVM owns counters/timers and the interrupt controller. OpenVMM sets
the topology's virtual timer PPI; KVM's required physical PPI is not exposed.
There is no Hyper-V time/SynIC support or software pause compensation.

### HVF

On macOS/Arm, HVF's synthetic reference counter and software SynIC timers
use `VmTime`. Architectural counters use a different basis:
`CNTPCT_EL0` reflects the host counter and `CNTVCT_EL0` reflects
`mach_absolute_time()` minus HVF's virtual-timer offset. The backend does
not adjust that offset to remove VM pause duration.

For a VP waiting after WFI, the backend converts the remaining
architectural virtual-timer interval into a `VmTime` wakeup. It then
re-enters HVF, whose timer-activation exit raises the emulated GIC PPI.
Using `VmTime` to arrange this wakeup does not make the architectural
counter itself a `VmTime` clock.

## OpenHCL: time inside a partition

OpenHCL runs in VTL2 inside a partition whose lifecycle is controlled
outside OpenHCL. Stopping lower-VTL execution or servicing OpenHCL is not
equivalent to freezing that surrounding partition. Its `VmPartition`
wrapper therefore leaves the backend time-control hooks as no-ops.

OpenHCL does have its own `VmTimeKeeper` for software device emulation.
It starts at zero, participates in state-unit stop/start, and has saved
state. Servicing also records partition reference time at stop and uses
it to report blackout duration on restart. That measurement is separate
from restoring the software VM-time value; it is not an instruction to
rewind the surrounding partition clock or add downtime to `VmTime`.

For non-hardware-isolated configurations, `virt_mshv_vtl` obtains native
reference time through HCL's VTL2 `TimeRefCount` register access. In x86
SNP and TDX configurations, it emulates lower-VTL Hyper-V facilities with
a reference source derived from `RDTSC` and a frequency-based scale.
This source is not `VmTime`. The reference-TSC page uses the same
zero-offset model. The frequency comes from the hypervisor; TDX also
validates it against hardware-advertised frequency information.

The emulated SynIC compares deadlines against that reference source.
To wait using software VM time, the implementation converts a *relative
interval*, `next_reference_time - current_reference_time`, into a
`VmTime` deadline instead of equating the clocks' origins. SNP also
reconciles kernel-handled STIMER0 writes with the userspace SynIC;
kernel expiry wakes VTL2, while the SynIC scan performs delivery. TDX
uses an L2 virtual-TSC execution deadline when supported, with a
`VmTime`-based fallback. The shared software APIC timer still uses
`VmTime`; hardware acceleration and interrupt delivery are distinct
from the reference-clock choice. The x86 CVM TSC model must not be
assumed to describe Arm hardware-isolated configurations.

OpenHCL's calendar clock is different again. `UnderhillLocalClock`
queries root-provided time over Guest Emulation Transport, caching
samples for up to one second, and adds a guest-programmable offset.
It persists the offset and saves it for servicing; without a saved
offset it uses the host-provided timezone offset. Thus RTC calendar
values need not represent UTC even though the internal type uses an
epoch-based numeric representation.

```admonish warning
Clock availability does not establish trust in UTC. In a confidential VM,
root-provided calendar time is an external input, not authenticated time.
A processor-derived reference counter is not a trusted calendar clock
either. These mechanisms do not by themselves provide rollback-resistant
timestamps or guarantees about host scheduling and timer delivery.
```

## Implementation map

Start with these repository paths:

- `vm/vmcore/src/{vmtime,reference_time}.rs`: timelines and accessors.
- `vmm_core/src/{partition_unit,vmtime_unit}.rs`: lifecycle operations;
  `openvmm/openvmm_core/src/worker/dispatch.rs`: hosted dependency wiring.
- `vm/hv1/hv1_emulator/src/{hv,synic}.rs`: reference pages and timers.
- `vmm_core/virt_{mshv,whp,kvm,hvf}/src/`: backend state and VP loops.
- `vm/devices/chipset/src/` (`pit`, `pm`, `cmos_rtc.rs`) and
  `support/local_clock/src/`: device timers and calendar clocks.
- `vm/devices/hyperv_ic/src/timesync.rs`: UTC/reference-time samples.
- `openhcl/virt_mshv_vtl/src/`: native and confidential-VM time sources;
  `openhcl/underhill_core/src/emuplat/local_clock.rs`: calendar time.

See also [snapshot behavior](../../user_guide/openvmm/snapshots.md) and
the [snapshot format](../../dev_guide/snapshot_format.md).

[vmtime]: https://openvmm.dev/rustdoc/vmcore/vmtime/index.html
[host-time]: https://openvmm.dev/rustdoc/pal_async/timer/index.html
[reference-time]: https://openvmm.dev/rustdoc/vmcore/reference_time/index.html
[local-clock]: https://openvmm.dev/rustdoc/local_clock/index.html
[time-control]: https://openvmm.dev/rustdoc/virt/trait.PartitionTimeControl.html
[tlfs-timers]: https://learn.microsoft.com/en-us/virtualization/hyper-v-on-windows/tlfs/timers
[whp-suspend]: https://learn.microsoft.com/en-us/virtualization/api/hypervisor-platform/funcs/whvsuspendpartitiontime
