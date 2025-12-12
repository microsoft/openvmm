# OpenHCL Boot Process

This document describes how OpenHCL boots, from initial load through to the running usermode paravisor process.

**Prerequisites:**

- [Getting Started: OpenHCL](../../user_guide/openhcl.md) - Introduction to OpenHCL and what it is
- [OpenHCL Architecture](./openhcl.md) - High-level architectural overview
- [Building OpenHCL](../../dev_guide/getting_started/build_openhcl.md) - How to build IGVM files

* * *

## Overview

OpenHCL is a paravisor (VTL2 hypervisor component) that runs alongside a guest operating system to provide security isolation and device emulation. The boot process involves several stages, starting from a packaged image loaded by the host VMM, through early initialization and kernel boot, to the final usermode paravisor process.

For more background on what OpenHCL is and the paravisor model, see the [OpenHCL user guide](../../user_guide/openhcl.md).

## IGVM File Format

OpenHCL is packaged as an **IGVM (Isolated Guest Virtual Machine) file**, a platform-agnostic format for describing the initial state of an isolated virtual machine. When the host loads OpenHCL, it parses the IGVM file and places each component at its designated physical address in VTL2 memory.

> **Note:** VTL2 (Virtual Trust Level 2) is the privilege level at which OpenHCL runs, providing isolation from the VTL0 guest OS. For more on VTLs, see the [OpenHCL Architecture documentation](./openhcl.md#vtls).

The IGVM file contains:

- **Boot shim** (`openhcl_boot`) - First code to execute in VTL2
- **Linux kernel** - The VTL2 operating system
- **Sidecar kernel** (x86_64 only) - Lightweight kernel for scaling to large CPU counts
- **Initial ramdisk (initrd)** - Root filesystem containing `openvmm_hcl` and dependencies
- **Memory layout directives** - Where to place each component
- **Configuration parameters** - Initial settings and topology information

For information on building IGVM files, see [Building OpenHCL](../../dev_guide/getting_started/build_openhcl.md).

## Boot Sequence

### Stage 1: openhcl_boot (Boot Shim)

The boot shim is the first code that executes in VTL2. It performs early initialization before transferring control to the Linux kernel:

**Source code:** [openhcl/openhcl_boot](https://github.com/microsoft/openvmm/tree/main/openhcl/openhcl_boot) | **Docs:** [openhcl_boot rustdoc](https://openvmm.dev/rustdoc/linux/openhcl_boot/index.html)

1. **Hardware initialization** - Sets up CPU state, enables MMU, configures initial page tables
2. **Configuration parsing** - Receives boot parameters from the host via IGVM
3. **Device tree construction** - Builds a device tree describing the hardware configuration (CPU topology, memory regions, devices)
4. **Sidecar initialization** (x86_64 only) - Sets up sidecar control/command pages so sidecar CPUs can start (see Sidecar Kernel section below)
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
- **Init process** - Starts the usermode init system

OpenHCL uses a minimal kernel configuration optimized for hosting the paravisor. See the [OpenHCL Architecture](./openhcl.md#openhcl-linux) documentation for more details.

The kernel exposes configuration to usermode through standard Linux interfaces:
- `/proc/device-tree` - Device tree accessible as a filesystem
- `/proc/cmdline` - Kernel command line parameters (can be configured via IGVM manifest, see also [logging](../openvmm/logging.md))
- `/sys` - Hardware topology and configuration
- Special device nodes - Paravisor-specific communication channels

### Stage 3: openvmm_hcl (Usermode Paravisor)

The final stage is the `openvmm_hcl` usermode process, which implements the core paravisor functionality:

**Source code:** [openhcl/openvmm_hcl](https://github.com/microsoft/openvmm/tree/main/openhcl/openvmm_hcl) | **Docs:** [openvmm_hcl rustdoc](https://openvmm.dev/rustdoc/linux/openvmm_hcl/index.html)

- **Configuration discovery** - Reads topology and settings from `/proc/device-tree` and kernel interfaces
- **Device emulation** - Intercepts and emulates guest device accesses
- **VTL0 management** - Monitors and controls the lower-privilege guest OS
- **Host communication** - Interfaces with the host VMM
- **Security enforcement** - Applies isolation policies at the paravisor boundary

## Sidecar Kernel (x86_64)

On x86_64, OpenHCL includes a **sidecar kernel** - a minimal, lightweight kernel that runs alongside the main Linux kernel to enable fast boot times for VMs with large CPU counts.

### Why Sidecar?

Booting all CPUs into Linux is expensive for large VMs. The sidecar kernel solves this by:
- Running a minimal dispatch loop on most CPUs instead of full Linux
- Allowing CPUs to be dynamically converted between sidecar and Linux as needed
- Parallelizing CPU startup so many VPs can be brought up concurrently

### How It Works

During boot, the configuration and control pages determine which CPUs run Linux and which run the sidecar kernel:
- **Linux CPUs** - A subset designated in the control/configuration data boot into the full Linux kernel
- **Sidecar CPUs** - Remaining CPUs boot into the lightweight sidecar kernel

The sidecar kernel:
- Runs independently on each CPU with minimal memory footprint
- Executes a simple dispatch loop, halting until needed
- Handles VP (virtual processor) run commands from the host VMM
- Can be converted to a Linux CPU on demand if more complex processing is required

Communication occurs through:
- **Control page** - Shared memory for kernel-to-sidecar communication (one per node)
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

OpenHCL supports VM save/restore (checkpointing):

**Usermode (`openvmm_hcl`)** orchestrates save/restore:
- Serializes device state and paravisor configuration
- Coordinates with the kernel through special interfaces
- Persists state that must survive across restarts

**Kernel state** is mostly ephemeral:
- Most kernel state is reconstructed fresh on restore
- Architecture-specific CPU state is saved by the hypervisor

**On restore:**
1. Host reloads the IGVM file with updated parameters
2. Boot shim reinitializes with the restored configuration
3. Kernel boots fresh with the same topology from the device tree
4. `openvmm_hcl` loads saved state and reconstructs device/paravisor state

The topology is regenerated on each boot from the host configuration, ensuring consistency between the host's view and OpenHCL's view.
