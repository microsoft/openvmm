# Sidecar Kernel (x86_64)

On x86_64, OpenHCL includes a **sidecar kernel** - a minimal, lightweight kernel that runs alongside the main Linux kernel.

**Source code:** [openhcl/sidecar](https://github.com/microsoft/openvmm/tree/main/openhcl/sidecar) | **Docs:** [sidecar rustdoc](https://openvmm.dev/rustdoc/linux/sidecar/index.html)

## Why Sidecar?

Booting a full Linux kernel on every CPU is expensive, especially for Virtual Machines with large CPU counts (e.g., hundreds of vCPUs). Initializing the kernel data structures, per-CPU memory, and scheduling threads for every core takes significant time and memory.

The sidecar kernel solves this by:

- **Fast Boot:** It runs a minimal dispatch loop on most CPUs instead of the full Linux kernel.
- **Resource Efficiency:** It has a tiny memory footprint compared to a full Linux CPU.
- **Parallelism:** It allows many VPs to be brought up concurrently without contention on Linux kernel locks.
- **On-Demand Scaling:** CPUs can be dynamically converted from running in the sidecar to full Linux on demand (this is a one-way transition).

## How It Works

During the boot process, the **Boot Shim** determines which CPUs will run the main Linux kernel and which will run the sidecar kernel.

- **Linux CPUs:** A small subset of CPUs (often just one or one per NUMA node) boot into the full Linux kernel to handle system services, device drivers, and the control plane.
- **Sidecar CPUs:** The remaining CPUs boot into the lightweight sidecar kernel.

The boot shim passes this assignment to the Linux kernel via command-line parameters and to the sidecar kernel via configuration pages in memory.

### The Sidecar Loop

The sidecar kernel executes a simple dispatch loop on each CPU:

1. **Halt:** The CPU halts (mwait/hlt) until it receives an interrupt or a command.
2. **Command Processing:** It handles simple commands from the host VMM, such as "Run VP" (execute the guest VTL0 code).
3. **Conversion:** If the CPU is needed for a task that requires full Linux capabilities (e.g., handling a complex I/O interrupt or running a userspace process), it can be commanded to "hot-plug" itself into the running Linux kernel.

### Communication

Communication between the Linux kernel, the host, and the sidecar CPUs occurs through:

- **Control Page:** Shared memory for kernel-to-sidecar communication (one per NUMA node).
- **Command Pages:** Per-CPU pages for VMM-to-sidecar commands.
- **IPIs (Inter-Processor Interrupts):** Used to wake up sidecar CPUs when work is available.
