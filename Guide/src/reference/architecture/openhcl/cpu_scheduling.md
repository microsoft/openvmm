# CPU Scheduling

OpenHCL runs a cooperative async executor on each VP thread. This page explains
how VP threads split time between guest execution and device work, what happens
when things block, and how the [sidecar kernel](sidecar.md) changes the picture.

```admonish tip
This document uses "lower VTL" and "VTL0" to refer to what are similar things.
VTLs increase in privilege as the number gets higher. VTL2 is a higher privliege
level than VTL1, which is yet higher than VTL0.

Engineers focused on IO often think about the "guest" as "VTL0", since that's the
VTL that issues IO to storage and networking devices. When this document discusses
entering VTL0, though, it's more precise to say that control returns to any VTL that
is less privileged than VTL2. It might be VTL0 or it might be VTL1.
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
[threadpool](https://github.com/microsoft/openvmm/blob/main/openhcl/underhill_threadpool/src/lib.rs).
Each thread is CPU-affinitized — thread N is pinned to Linux CPU N (which today
equals VP index N).

```text
  VP 0 thread (CPU 0)    VP 1 thread (CPU 1)    VP 2 thread (CPU 2)
  ┌──────────────────┐   ┌──────────────────┐   ┌──────────────────┐
  │ cooperative      │   │ cooperative      │   │ cooperative      │
  │ async executor   │   │ async executor   │   │ async executor   │
  │                  │   │                  │   │                  │
  │ • device workers │   │ • device workers │   │ • device workers │
  │ • VMBus relay    │   │ • VMBus relay    │   │ • VMBus relay    │
  │ • when idle:     │   │ • when idle:     │   │ • when idle:     │
  │ enter lower VTL  │   │ enter lower VTL  │   │ enter lower VTL  │
  └──────────────────┘   └──────────────────┘   └──────────────────┘
```

The code calls VTL0 execution the thread's "idle task" — meaning the VP thread
enters VTL0 when there is no pending VTL2 async work. The thread itself is not
idle (the physical CPU is running guest code), but the VTL2 executor has nothing
to do.

Alongside the VP threads, the worker process runs a few additional threads:

- **GET worker** — on a dedicated thread because it issues blocking syscalls
  that would stall the VP executor. When a GET message arrives, it is processed
  on this dedicated thread, not on a VP thread. Results are dispatched back to
  VP threads via async channels.
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

All tasks with `target_vp = N` and `run_on_target = false` run on an abitrary
VP's thread. IOs issued by the task will use the target VP's io-uring. When the
IO completes, the task will be woken up on the target VP. It will likely run
there, even if `run_on_target` is false.

If no target VP is set, then the task will use the current VP's io-uring,
wherever the task executes.

## Blocking scenarios

Because the executor is cooperative and single-threaded per VP, several
situations can stall all tasks on a VP.

```admonish note
In OpenVMM (not OpenHCL), device workers and VP execution run
on separate threads, so there is no Guest VP blocking problem.
```

### VTL0 guest execution

When there are no pending VTL2 tasks, the VP thread enters a lower VTL via an
ioctl (`hcl_return_to_lower_vtl`). The thread is in the kernel until a VM exit
returns control to VTL2.

**IO completions still wake VTL2.** OpenHCL registers the io_uring fd with the
HCL kernel module via `set_poll_file`. When an io_uring completion fires (e.g.,
a disk I/O completes via `disk_blockdevice`), the kernel cancels the VM run,
returning the thread to VTL2. This applies to any async work that completes
through io_uring — not just disk I/O.

For device interrupts that don't go through io_uring (e.g., the physical NVMe
driver in `disk_nvme` receives interrupts via an eventfd), the eventfd is
registered with io_uring as a poll operation, so it also triggers the cancel
path.

If the VTL0 guest traps into the hypervisor (e.g., for a hypercall or MMIO
access that the hypervisor handles on behalf of the root), the VP is in the
hypervisor — not in VTL0 usermode — and the io_uring cancel mechanism does not
apply. The VP remains in the hypervisor until the intercept completes.

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
example, an MMIO write that traps to the hypervisor — the VP can be stopped in
the hypervisor while the root processes the intercept. Both VTL2 and VTL0 are
stalled on that VP.

This is not a software-level problem in OpenHCL — it's an artifact of the
hypervisor/root architecture. The VP physically cannot execute until the root
completes the intercept.

### VTL2 blocking VTL0

The reverse of VTL0 blocking: while the VP thread is running VTL2 tasks, VTL0
cannot execute on that VP. A long burst of VTL2 device work (e.g., processing a
large batch of StorVSP completions) delays guest execution.

## Timeline

A VP thread's execution over time:

```text
     RUNNING          STALLED          RUNNING        BLOCKED
  ┌──────────────┬───────────────┬──────────────┬──────────────┐
  │▓▓▓▓▓▓▓▓▓▓▓▓▓▓│░░░░░░░░░░░░░░░│▓▓▓▓▓▓▓▓▓▓▓▓▓▓│██████████████│
  │  VTL2 tasks  │  VTL0 guest   │  VTL2 tasks  │   kernel     │
  │              │               │              │   syscall    │
  │  storvsp,    │  ALL VTL2     │  storvsp,    │  ALL VTL2    │
  │  netvsp,     │  tasks wait   │  netvsp,     │  tasks wait  │
  │  relay       │               │  relay       │              │
  └──────────────┴───────────────┴──────────────┴──────────────┘
  ▓ = VTL2 work active    ░ = VTL0 running    █ = kernel blocked
```

Each segment is mutually exclusive — only one of VTL2 tasks, VTL0 guest, or
kernel work can run at any instant on a given VP thread.

## No work stealing

The OpenHCL threadpool does not implement work stealing. Targeted tasks always
run on their target VP's thread. For example: If VP 2's thread is blocked in
VTL0, a StorVSP worker targeted at VP 2 cannot be picked up by VP 3's thread.

Untargeted tasks (those without `run_on_target`) run on the thread that wakes
them — which is not the same as stealing.

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
   in the repo currently spawn helper threads — instead, subsystems that need
   blocking (GET, VMGS) run on their own dedicated threads outside the VP
   threadpool.

2. **Sidecar VPs run remotely first.** Beware doing work on a targeted VP early
   in boot. If a device worker is targeted at a sidecar VP, it initially runs on
   the base CPU, not the target CPU. This can cause contention, but more
   importantly: work that must occur on certain VP will cause that VP to exit
   the sidecar and enter Linux.

3. **Use `TaskControl` for worker lifecycle.** Device workers should implement
   [`AsyncRun`](https://openvmm.dev/rustdoc/linux/task_control/trait.AsyncRun.html)
   and be managed via
   [`TaskControl`](https://openvmm.dev/rustdoc/linux/task_control/struct.TaskControl.html),
   which provides start/stop/inspect integration. (This doesn't really apply to
   the CPU scheduling model, but is general good advice for writing device
   backends).