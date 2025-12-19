# OpenHCL Boot Flow

This document describes the sequence of events that occur when OpenHCL boots, from the initial loading of the IGVM package to the fully running paravisor environment.

## 1. IGVM Loading

The boot process begins when the host VMM loads the OpenHCL IGVM package into VTL2 memory.
The IGVM package contains the initial code and data required to start the paravisor, including the boot shim, kernel, and initial ramdisk.
The host places these components at specific physical addresses defined in the IGVM header.

## 2. Boot Shim Execution (`openhcl_boot`)

The host transfers control to the entry point of the **Boot Shim**.

1. **Hardware Init:** The shim initializes the CPU state and memory management unit (MMU).
2. **Config Parsing:** It parses the configuration parameters provided by the host via the IGVM.
3. **Device Tree:** It constructs a Device Tree that describes the hardware topology (CPUs, memory) to the Linux kernel.
4. **Sidecar Setup (x86_64):** If configured, it sets up the control structures for the Sidecar kernel and signals which CPUs should boot into the Sidecar.
5. **Kernel Handoff:** Finally, it jumps to the Linux kernel entry point, passing the Device Tree and command line arguments.

## 3. Linux Kernel Boot

The **Linux Kernel** takes over and initializes the operating system environment.

1. **Kernel Init:** The kernel initializes its subsystems (memory, scheduler, etc.).
2. **Driver Init:** It loads drivers for the paravisor hardware and standard devices.
3. **Root FS:** It mounts the initial ramdisk (initrd) as the root filesystem.
4. **User Space:** It spawns the first userspace process, `underhill_init` (PID 1).

## 4. Userspace Initialization (`underhill_init`)

`underhill_init` prepares the userspace environment.

1. **Filesystems:** It mounts essential pseudo-filesystems like `/proc`, `/sys`, and `/dev`.
2. **Environment:** It sets up environment variables and system limits.
3. **Exec:** It replaces itself with the main paravisor process, `/bin/openvmm_hcl`.

## 5. Paravisor Startup (`openvmm_hcl`)

The **Paravisor** process (`openvmm_hcl`) starts and initializes the virtualization services.

1. **Config Discovery:** It reads the system topology and configuration from `/proc/device-tree` and other kernel interfaces.
2. **Service Init:** It initializes internal services, such as the VTL0 management logic and host communication channels.
3. **Worker Spawn:** It spawns the **VM Worker** process (`underhill_vm`) to handle the high-performance VM partition loop.

## 6. VM Execution

At this point, the OpenHCL environment is fully established.
The `underhill_vm` process runs the VTL0 guest, handling exits and emulating devices, while `openvmm_hcl` manages the overall policy and communicates with the host.

## Sidecar Boot Flow (x86_64)

On x86_64 systems using the Sidecar kernel, the boot flow for Application Processors (APs) is different:

1. **Shim Decision:** The Boot Shim determines which CPUs will run Linux and which will run the Sidecar.
2. **Sidecar Entry:** "Sidecar CPUs" jump directly to the Sidecar kernel entry point instead of the Linux kernel.
3. **Dispatch Loop:** These CPUs enter a lightweight dispatch loop, waiting for commands.
4. **On-Demand:** If a Sidecar CPU is needed for a Linux task (e.g., handling an interrupt that requires a Linux driver), it can be "hot-plugged" into the running Linux kernel.

## Configuration Data Flow

Configuration flows through the system as follows:

1. **Host VMM** generates the configuration.
2. **IGVM** delivers the configuration to VTL2.
3. **Boot Shim** parses it and converts it to a Device Tree.
4. **Linux Kernel** exposes the Device Tree to userspace.
5. **Paravisor** reads the Device Tree to configure itself.
