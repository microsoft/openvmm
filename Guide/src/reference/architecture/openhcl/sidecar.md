# Sidecar Kernel (x86_64)

On supported x86_64 OpenHCL images, the sidecar kernel runs selected processors
outside Linux in a small VTL2 dispatch runtime.

**Source code:**
[openhcl/sidecar](https://github.com/microsoft/openvmm/tree/main/openhcl/sidecar)
| **Docs:**
[sidecar rustdoc](https://openvmm.dev/rustdoc/linux/sidecar/index.html)

## Why Sidecar?

Booting Linux on every CPU is expensive for VMs with large processor counts.
Initializing per-CPU kernel state and scheduler threads consumes time and
memory.

The sidecar kernel solves this by:

- **Parallelism:** Many VPs start concurrently without contending on Linux
	kernel locks.
- **On-Demand Scaling:** A sidecar CPU can convert to Linux on demand. This is a
	one-way transition.

## How It Works

During boot, the shim determines which CPUs run Linux and which run sidecar.

- **Linux CPUs:** A small subset, often one per NUMA node, boots Linux to run
	services, drivers, and the control plane.
- **Sidecar CPUs:** The remaining CPUs boot into the lightweight sidecar kernel.

The shim passes this assignment to Linux through the command line and to
sidecar through configuration pages.

### The Sidecar Loop

The sidecar kernel executes a simple dispatch loop on each CPU:

1. **Halt:** The CPU waits with `mwait` or `hlt` for an interrupt or command.
2. **Command Processing:** It handles requests such as running VTL0 VP code.
3. **Conversion:** It can hot-plug into Linux when full kernel capabilities are
	required.

The "Run VP" command enters the lower-VTL guest processor loop. A control-page
request can interrupt an unbounded command and return the processor to the
dispatch loop. Other commands exchange VP state through the per-CPU command
page.

Conversion into Linux is one-way for that boot. Once Linux onlines the
processor, sidecar no longer schedules lower-VTL work on it.

### Communication

Communication between the Linux kernel, the host, and the sidecar CPUs occurs through:

- **Control Page:** Shared memory for kernel-to-sidecar communication (one per NUMA node).
- **Command Pages:** Per-CPU pages for VMM-to-sidecar commands.
- **IPIs (Inter-Processor Interrupts):** Used to wake up sidecar CPUs when work is available.

The control page has a fixed virtual address in each CPU's private page tables,
while all CPUs in a node map the same physical control page. Per-CPU page tables
otherwise expose only the memory that processor needs.

Status fields such as processor state and `needs_attention` coordinate command
submission and completion. Ordering and atomic transitions are part of the
kernel/sidecar ABI; neither side should treat the pages as an unstructured
mailbox.
