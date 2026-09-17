# CPU Scheduling

OpenHCL runs a cooperative async executor on each VP thread. This page explains
how VP threads split time between guest execution and device work, what happens
when things block, and how the [sidecar kernel](sidecar.md) changes the picture.

```admonish tip
This document uses "lower VTL" and "VTL0" to refer to what are similar
things. VTLs increase in privilege as the number gets higher. VTL2 is a
higher privilege level than VTL1, which is yet higher than VTL0.

Engineers focused on IO often think about the "guest" as "VTL0", since
that's the VTL that issues IO to storage and networking devices. When
this document discusses entering VTL0, though, it's more precise to say
that control returns to any VTL that is less privileged than VTL2. It
might be VTL0 or it might be VTL1.
```

## Scope

This page covers the **VM worker process** — the main OpenHCL process that runs
device emulation and VP dispatch. OpenHCL also runs other processes (see
[Processes and Components](processes.md)), but the cooperative executor model
described here applies specifically to the worker process and its per-VP
threadpool.

For background on Rust async executors, see the [Asynchronous Programming in
Rust](https://rust-lang.github.io/async-book/) book (especially this [Under the
Hood](https://rust-lang.github.io/async-book/02_execution/01_chapter.html)
section).

## Thread model

OpenHCL's worker process runs one thread per VP in its
[threadpool](https://openvmm.dev/rustdoc/linux/underhill_threadpool/index.html).
Each thread is CPU-affinitized — thread N is pinned to Linux CPU N,
which maps 1:1 to VP index N in current configurations.

```text
  VP 0 thread (CPU 0)    VP 1 thread (CPU 1)    VP 2 thread (CPU 2)
  ┌──────────────────┐   ┌──────────────────┐   ┌──────────────────┐
  │ cooperative      │   │ cooperative      │   │ cooperative      │
  │ async executor   │   │ async executor   │   │ async executor   │
  │                  │   │                  │   │                  │
  │ • async workers  │   │ • async workers  │   │ • async workers  │
  │   e.g. devices   │   │   e.g. devices   │   │   e.g. devices   │
  │ • when idle:     │   │ • when idle:     │   │ • when idle:     │
  │ enter lower VTL  │   │ enter lower VTL  │   │ enter lower VTL  │
  └──────────────────┘   └──────────────────┘   └──────────────────┘
```

The code calls VTL0 execution the thread's "idle task" — meaning the VP thread
enters VTL0 when there is no pending VTL2 async work. The thread itself is not
idle (the physical CPU is running guest code), but the VTL2 executor has nothing
to do.

Alongside the VP threads, the worker process runs a few additional threads:

- **GET (Guest Emulation Transport) worker** — on a dedicated thread because
  it issues blocking syscalls that would stall the VP executor. When a GET
  message arrives, it is processed on this dedicated thread, not on a VP
  thread. Results are dispatched back to VP threads via async channels.
- **Tracing thread** — log collection.
- **CPU-online helper threads** — temporary, used when bringing sidecar VPs into
  Linux.

This list may not be exhaustive at the point that you're reading these docs, but
the point remains: _most_ work happens on the same thread as the lower VTL VPs
and work that occurs on their behalf in OpenHCL.

## Cooperative scheduling

Each VP thread runs an async executor that multiplexes all tasks targeted at
that VP. Tasks only yield to each other at `.await` points. If you're not
familiar with the Rust execution model, it may be tempting to think that the
system will time slice execution or blocking requests with other async tasks
running on the same VP thread. It won't.

### What runs on a VP thread

All tasks with `target_vp = N` and `run_on_target = true` run on VP N's thread,
once the target VP is ready (i.e., the CPU is online and affinity is set).

All tasks with `target_vp = N` and `run_on_target = false` can be spawned on
any VP's thread. However, `target_vp` still matters for IO affinity: IOs
issued by the task use VP N's io-uring. When the IO completes, the task is
woken on VP N and will likely continue running there. In practice, these
tasks gravitate toward their target VP despite not requiring it for
scheduling. This keeps per-VP I/O flows localized: submission, completion,
and follow-up processing tend to happen on the same VP.

All tasks without a specific `target_vp` will fall into the thread pool's
untargeted path. These could run on any arbitrary VP. IO operations from
untargeted tasks currently use VP 0's io-uring. This is a compatibility
default and may change — see the
[backend implementation](https://github.com/microsoft/openvmm/blob/main/openhcl/underhill_core/src/threadpool_vm_task_backend.rs)
for details.

## Blocking scenarios

Because the executor is cooperative and single-threaded per VP, several
situations can stall all tasks on a VP.

```admonish note
In OpenVMM (not OpenHCL), device workers and VP execution run
on separate threads. Code authors *should not* issue a blocking wait inside
an async task. This is a general Rust rule, that emphatically applies in
OpenVMM and OpenHCL.
```

### VTL0 guest execution

When there are no VTL2 tasks that can make forward progress, the VP thread
enters a lower VTL via an ioctl (`hcl_return_to_lower_vtl`). The thread is
in the kernel until a VM exit returns control to VTL2.

**IO completions still wake VTL2.** OpenHCL registers the io_uring fd with
the HCL kernel module via `set_poll_file`. When an io_uring completion fires
(e.g., a disk I/O completes via `disk_blockdevice`), the kernel cancels the
VM run, returning the thread to VTL2. This applies to any async work that
completes through io_uring — not just disk I/O.

For device interrupts that don't go through io_uring (e.g., the physical
NVMe driver in `disk_nvme` receives interrupts via an eventfd), the eventfd
is registered with io_uring as a poll operation, so it also triggers the
cancel path.

If the VTL0 guest traps into the hypervisor (e.g., for a hypercall or MMIO
(Memory-Mapped I/O) access that the hypervisor handles on behalf of the
root), the VP is in the hypervisor — not in VTL0 usermode — and the
io_uring cancel mechanism does not apply. The VP remains in the hypervisor
until the intercept completes. I/O completions may still occur but do not
preempt the intercept.

### Kernel syscall blocking

If a device worker issues a blocking syscall (e.g., a disk backend falls back to
synchronous I/O), the thread is in the kernel. No `.await` yield is possible
because the thread itself is blocked. VTL0 cannot execute either.

New device backends should use the built-in io_uring primitives in OpenHCL to
create workers for those blocking tasks. If that is impossible, that device will
need to spawn a new thread. This should be rare, and you should discuss your
rationale with the community before implementing something that way.

### Hypervisor intercepts

When VTL2 triggers an operation that requires root partition handling — for
example, an MMIO (Memory-Mapped I/O) write that traps to the hypervisor — the
VP can be stopped in the hypervisor while the root processes the intercept. Both
VTL2 and VTL0 are stalled on that VP.

This is not a software-level problem in OpenHCL — it's an artifact of the
hypervisor/root architecture. The VP physically cannot execute until the root
completes the intercept.

### VTL2 blocking VTL0

The reverse of VTL0 blocking: while the VP thread is running VTL2 tasks, VTL0
cannot execute on that VP. A long burst of VTL2 device work (e.g., processing a
large batch of StorVSP completions) delays guest execution. During large
batches, ensure the code contains `.await` points so other VTL2 tasks (and
ultimately VTL0 execution) can make progress; there is no automatic preemption.

## Timeline

A VP thread's execution over time:

```text
     RUNNING          STALLED          RUNNING        BLOCKED
  ┌──────────────┬─────────────────┬──────────────┬──────────────┐
  │▓▓▓▓▓▓▓▓▓▓▓▓▓▓│░░░░░░░░░░░░░░░░░│▓▓▓▓▓▓▓▓▓▓▓▓▓▓│██████████████│
  │  VTL2 tasks  │  VTL0 guest     │  VTL2 tasks  │   kernel     │
  │              │                 │              │   syscall    │
  │  storvsp,    │  all VTL2       │  storvsp,    │  ALL VTL2    │
  │  netvsp,     │  futures pending│  netvsp,     │  tasks wait  │
  │  relay       │  (exit → wake)  │  relay       │              │
  └──────────────┴─────────────────┴──────────────┴──────────────┘
  ▓ = VTL2 work active    ░ = VTL0 running    █ = kernel blocked
```

Each segment is mutually exclusive — only one of VTL2 tasks, VTL0 guest, or
kernel work can run at any instant on a given VP thread.

## No work stealing

The OpenHCL threadpool does not implement work stealing. Targeted tasks always
run on their target VP's thread. For example: If VP 2's thread is blocked in
VTL0, a StorVSP worker targeted at VP 2 cannot be picked up by VP 3's thread.

Tasks without a specific `target_vp` run on the thread that wakes them — which
is not the same as work stealing. Similarly, tasks with `run_on_target = false`
may run on a thread other than their target VP's, but their IOs are still
directed to the target VP's io-uring.

## Sidecar changes

On x64 non-isolated VMs, the [sidecar kernel](sidecar.md) splits VP execution
from device work. Most VPs run in the sidecar — a minimal kernel that handles
VTL0 entry/exit without Linux. Only a few CPUs (typically one per NUMA node)
boot into Linux. This is to amortize the CPU startup cost until it becomes
necessary.

Device workers run only on the CPUs that are onlined in the OpenHCL Linux
kernel. Device workers that are CPU agnostic can run in this more limited set of
Linux CPUs.

When a sidecar VP hits an intercept that requires VTL2 processing (the first
handled VM exit), the sidecar CPU is hot-plugged into Linux. From that point,
the VP's device workers can run on its own thread instead.

## Impact on device design

When writing device backends, keep these rules in mind:

1. **Never block synchronously** in a device worker on a VP thread. Use async
   I/O (io_uring) or spawn a helper thread for blocking work. No VMBus devices
   used in OpenHCL currently spawn helper threads. Instead, subsystems that need
   blocking (GET, VMGS) run on their own dedicated threads outside the VP
   threadpool.

2. **Sidecar VPs run remotely first.** Beware doing work on a targeted VP early
   in boot. If a device worker is targeted at a sidecar VP, it initially runs on
   the base CPU, not the target CPU. This can cause contention, but more
   importantly: work that must occur on certain VP will cause that VP to exit
   the sidecar and enter Linux. This hot-plug has non-trivial latency; sidecar
   exists specifically to defer this cost until the VP actually needs VTL2
   processing.

3. **Use `TaskControl` for worker lifecycle.** Device workers should implement
   [`AsyncRun`](https://openvmm.dev/rustdoc/linux/task_control/trait.AsyncRun.html)
   and be managed via
   [`TaskControl`](https://openvmm.dev/rustdoc/linux/task_control/struct.TaskControl.html),
   which provides start/stop/inspect integration. (This doesn't really apply to
   the CPU scheduling model, but is general good advice for writing device
   backends).
