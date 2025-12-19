# OpenHCL Processes and Components

This document describes the major software components and processes that make up the OpenHCL paravisor environment.

## Boot Shim (`openhcl_boot`)

The boot shim is the first code that executes in VTL2. It is responsible for early hardware initialization and preparing the environment for the Linux kernel.

**Source code:** [openhcl/openhcl_boot](https://github.com/microsoft/openvmm/tree/main/openhcl/openhcl_boot) | **Docs:** [openhcl_boot rustdoc](https://openvmm.dev/rustdoc/linux/openhcl_boot/index.html)

**Key Responsibilities:**

- **Hardware Initialization:** Sets up CPU state, enables MMU, and configures initial page tables.
- **Configuration Parsing:** Receives boot parameters from the host via IGVM.
- **Device Tree Construction:** Builds a device tree describing the hardware configuration (CPU topology, memory regions, devices).
- **Sidecar Initialization:** Sets up control structures for the Sidecar kernel (x86_64 only).
- **Kernel Handoff:** Transfers control to the Linux kernel.

## Linux Kernel

OpenHCL runs on top of a minimal, specialized Linux kernel. This kernel provides core operating system services such as memory management, scheduling, and device drivers.

**Key Responsibilities:**

- **Hardware Abstraction:** Manages CPU and memory resources.
- **Device Drivers:** Provides drivers for paravisor-specific hardware and standard devices.
- **Filesystem:** Mounts the initial ramdisk (initrd) as the root filesystem.
- **Process Management:** Launches the initial userspace process (`underhill_init`).

## Sidecar Kernel (x86_64)

On x86_64 systems, OpenHCL includes a "sidecar" kernel—a lightweight, bare-metal kernel that runs on a subset of CPUs to improve boot performance and reduce resource usage.

For more details, see the [Sidecar Architecture](./sidecar.md) page.

**Source code:** [openhcl/sidecar](https://github.com/microsoft/openvmm/tree/main/openhcl/sidecar) | **Docs:** [sidecar rustdoc](https://openvmm.dev/rustdoc/linux/sidecar/index.html)

**Key Responsibilities:**

- **Fast Boot:** Allows secondary CPUs (APs) to boot quickly without initializing the full Linux kernel.
- **Dispatch Loop:** Runs a minimal loop waiting for commands from the host or the main kernel.
- **On-Demand Conversion:** Can be converted to a full Linux CPU if required.

## Init Process (`underhill_init`)

`underhill_init` is the first userspace process (PID 1) started by the Linux kernel. It acts as the system service manager for the paravisor environment.

**Source code:** [openhcl/underhill_init](https://github.com/microsoft/openvmm/tree/main/openhcl/underhill_init) | **Docs:** [underhill_init rustdoc](https://openvmm.dev/rustdoc/linux/underhill_init/index.html)

**Key Responsibilities:**

- **System Setup:** Mounts necessary filesystems (e.g., `/proc`, `/sys`, `/dev`).
- **Environment Preparation:** Sets up the execution environment for the paravisor.
- **Process Launch:** `exec`s the main paravisor process (`openvmm_hcl`).

## Paravisor (`openvmm_hcl`)

`openvmm_hcl` is the central management process of the OpenHCL paravisor. It runs in userspace and orchestrates the virtualization services.

**Source code:** [openhcl/openvmm_hcl](https://github.com/microsoft/openvmm/tree/main/openhcl/openvmm_hcl) | **Docs:** [openvmm_hcl rustdoc](https://openvmm.dev/rustdoc/linux/openvmm_hcl/index.html)

**Key Responsibilities:**

- **Policy & Management:** Manages the lifecycle of the VM and enforces security policies.
- **Host Communication:** Interfaces with the host VMM to receive commands and report status.
- **Servicing:** Orchestrates save and restore operations (VTL2 servicing).
- **Worker Management:** Spawns and manages the VM worker process.

## VM Worker (`underhill_vm`)

The VM worker process (`underhill_vm`) is responsible for the high-performance data path of the virtual machine. It is spawned by `openvmm_hcl`.

**Source code:** [openhcl/underhill_core](https://github.com/microsoft/openvmm/tree/main/openhcl/underhill_core) | **Docs:** [underhill_core rustdoc](https://openvmm.dev/rustdoc/linux/underhill_core/index.html)

**Key Responsibilities:**

- **VP Loop:** Runs the virtual processor loop, handling VM exits.
- **Device Emulation:** Emulates devices for the guest VM.
- **I/O Processing:** Handles high-speed I/O operations.

## Diagnostics Server (`diag_server`)

The diagnostics server provides an interface for debugging and monitoring the OpenHCL environment.

**Source code:** [openhcl/diag_server](https://github.com/microsoft/openvmm/tree/main/openhcl/diag_server) | **Docs:** [diag_server rustdoc](https://openvmm.dev/rustdoc/linux/diag_server/index.html)

**Key Responsibilities:**

- **External Interface:** Listens on a VSOCK port for diagnostic connections.
- **Command Handling:** Processes diagnostic commands and queries.
- **Log Retrieval:** Provides access to system logs.

## Profiler Worker (`profiler_worker`)

The profiler worker is an on-demand process used for performance analysis.

**Source code:** [openhcl/profiler_worker](https://github.com/microsoft/openvmm/tree/main/openhcl/profiler_worker) | **Docs:** [profiler_worker rustdoc](https://openvmm.dev/rustdoc/linux/profiler_worker/index.html)

**Key Responsibilities:**

- **Performance Data Collection:** Collects profiling data (e.g., CPU usage, traces) when requested.
- **Isolation:** Runs in a separate process to minimize impact on the main workload.
