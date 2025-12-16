# OpenHCL Boot Process

This document describes how OpenHCL boots, from initial load through to the running usermode paravisor process. For a primer on the IGVM package that delivers OpenHCL, see [IGVM Overview](./igvm.md).

## Boot Sequence

### Stage 1: openhcl_boot (Boot Shim)

The boot shim is the first code that executes in VTL2. It performs early initialization before transferring control to the Linux kernel:

**Source code:** [openhcl/openhcl_boot](https://github.com/microsoft/openvmm/tree/main/openhcl/openhcl_boot) | **Docs:** [openhcl_boot rustdoc](https://openvmm.dev/rustdoc/linux/openhcl_boot/index.html)

1. **Hardware initialization** - Sets up CPU state, enables MMU, configures initial page tables
2. **Configuration parsing** - Receives boot parameters from the host via IGVM
3. **Device tree construction** - (non-isolated only) Builds a device tree describing the hardware configuration (CPU topology, memory regions, devices)
4. **Sidecar initialization** (x86_64 only, non-isolated only) - Sets up sidecar control/command pages so sidecar CPUs can start (see Sidecar Kernel section below)
5. **Kernel handoff** - Transfers control to the Linux kernel entry point with device tree and command line

The boot shim receives configuration through:

- **IGVM parameters** - Structured data from the IGVM file
- **Architecture-specific boot protocol** - Device tree pointer (ARM64) or boot parameters structure (x86_64)

### Stage 2: Linux Kernel

The VTL2 Linux kernel provides core operating system services:

- **Device tree parsing** - Discovers CPU topology, memory layout, and hardware configuration
- **Memory management** - Sets up VTL2 virtual memory and page allocators
- **Device drivers** - Initializes paravisor-specific drivers and standard devices
- **Initrd mount** - Mounts the initial ramdisk as the root filesystem
- **Init process** - Launches `underhill_init`, the usermode launcher that prepares the environment and then execs the OpenHCL paravisor

OpenHCL uses a minimal kernel configuration optimized for hosting the paravisor. See the [OpenHCL Architecture](./openhcl.md#openhcl-linux) documentation for more details. This minimal kernel can be found in the [OHCL-Linux-Kernel](https://github.com/microsoft/OHCL-Linux-Kernel) repository.

The kernel exposes configuration to usermode through standard Linux interfaces:

- `/proc/device-tree` - Device tree accessible as a filesystem
- `/proc/cmdline` - Kernel command line parameters (can be configured via IGVM manifest, see also [logging](../openvmm/logging.md))
- `/sys` - Hardware topology and configuration
- Special device nodes - Paravisor-specific communication channels

The initrd carries a single multi-call binary (see [openhcl/underhill_entry](https://github.com/microsoft/openvmm/tree/main/openhcl/underhill_entry)) that selects its persona based on the name used to execute. Symlinks expose the names `underhill_init`, `openvmm_hcl`, and `underhill_vm` (plus servicing tools such as `underhill_dump`). This keeps every usermode phase—launcher, paravisor, and VM worker—on the same binary while still presenting distinct process names.

### Stage 3: openvmm_hcl (Usermode Paravisor)

The final stage is the `openvmm_hcl` usermode process, which implements the core paravisor functionality:

**Source code:** [openhcl/openvmm_hcl](https://github.com/microsoft/openvmm/tree/main/openhcl/openvmm_hcl) | **Docs:** [openvmm_hcl rustdoc](https://openvmm.dev/rustdoc/linux/openvmm_hcl/index.html)

- **Configuration discovery** - Reads topology and settings from `/proc/device-tree` and kernel interfaces
- **Device emulation** - Intercepts and emulates guest device accesses
- **VTL0 management** - Monitors and controls the lower-privilege guest OS
- **Host communication** - Interfaces with the host VMM
- **Security enforcement** - Applies isolation policies at the paravisor boundary

`openvmm_hcl` spawns the Underhill VM worker in [openhcl/underhill_core](https://github.com/microsoft/openvmm/tree/main/openhcl/underhill_core), which shows up in process listings as `underhill_vm`. The worker owns the VM partition loop (virtual processors, exits, device I/O) while `openvmm_hcl` retains the policy, servicing, and management plane, so both processes remain active for the lifetime of the VM.

## Sidecar Kernel (x86_64)

On x86_64, OpenHCL includes a **sidecar kernel** - a minimal, lightweight kernel that runs alongside the main Linux kernel to enable fast boot times for VMs with large CPU counts.

### Why Sidecar?

Booting all CPUs into Linux is expensive for large VMs. The sidecar kernel solves this by:

- Running a minimal dispatch loop on most CPUs instead of full Linux
- Allowing CPUs to be dynamically converted from running in the sidecar to Linux on demand (this is one-way)
- Parallelizing CPU startup so many VPs can be brought up concurrently

### How It Works

During boot, the configuration and control pages determine which CPUs run Linux and which run the sidecar kernel:

- **Linux CPUs** - A subset designated in the control/configuration data boot into the full Linux kernel
- **Sidecar CPUs** - Remaining CPUs boot into the lightweight sidecar kernel

The boot shim decides which CPUs start in the Linux kernel vs which start in the sidecar kernel. The boot shim passes this info to the Linux kernel via a command-line parameter, and to the sidecar kernel by storing config info in a well know physical address.

The sidecar kernel:

- Runs independently on each CPU with minimal memory footprint
- Executes a simple dispatch loop, halting until needed
- Handles VP (virtual processor) run commands from the host VMM
- Can be converted to a Linux CPU on demand if more complex processing is required. For example, if openhcl must process IO or interrupts on the CPU.

Communication occurs through:

- **Control page** - Shared memory for kernel-to-sidecar communication (one per numa node)
- **Command pages** - Per-CPU pages for VMM-to-sidecar commands
- **IPIs** - Interrupts to wake sidecar CPUs when work is available

**Source code:** [openhcl/sidecar](https://github.com/microsoft/openvmm/tree/main/openhcl/sidecar) | **Docs:** [sidecar rustdoc](https://openvmm.dev/rustdoc/linux/sidecar/index.html)

## Configuration Data Flow

Configuration and topology information flows through the boot stages:

1. **Host VMM** → Generates configuration based on VM settings
2. **IGVM file** → Embeds configuration in the package
3. **openhcl_boot** → Parses configuration, builds device tree
4. **Linux kernel** → Reads device tree, exposes via `/proc` and `/sys`
5. **openvmm_hcl** → Reads from kernel interfaces, configures paravisor

Key topology information includes:

- Number and layout of virtual processors (VPs)
- NUMA topology and memory node configuration
- Device configuration and MMIO regions
- Paravisor-specific settings

## Save and Restore

OpenHCL supports VM save/restore for non-isolated VMs. This is called "runtime reload" or "VTL2 servicing" or "OpenHCL servicing".

**Usermode (`openvmm_hcl`)** orchestrates save/restore:

- Serializes device state and paravisor configuration
- Coordinates with the kernel through special interfaces
- Persists state that must survive across restarts

**Kernel state** is ephemeral, the kernel state is reconstructed fresh on restore

**On restore:**

1. Host reloads the IGVM file with updated parameters
2. Boot shim reinitializes with the restored configuration
3. Kernel boots fresh with the same topology from the device tree
4. `openvmm_hcl` loads saved state and reconstructs device/paravisor state

The topology is regenerated on each boot from the host configuration, ensuring consistency between the host's view and OpenHCL's view.
