# OpenHCL Processes and Components

This page describes the major software components and processes in the OpenHCL
paravisor environment.

## Boot Shim (`openhcl_boot`)

The boot shim is the first code that executes in VTL2. It performs early
hardware initialization and prepares the Linux environment.

**Source code:**
[openhcl/openhcl_boot](https://github.com/microsoft/openvmm/tree/main/openhcl/openhcl_boot)
| **Docs:**
[openhcl_boot rustdoc](https://openvmm.dev/rustdoc/linux/openhcl_boot/index.html)

**Key Responsibilities:**

- **Hardware Initialization:** Sets up CPU state, enables MMU, and configures initial page tables.
- **Configuration Parsing:** Combines measured parameters with permitted host
	data. Isolated VMs filter host options that could weaken guest guarantees.
- **Device Tree Construction:** Describes CPU topology, memory, and devices for
	Linux.
- **Sidecar Initialization:** Sets up control structures for the Sidecar kernel (x86_64 only).
- **Kernel Handoff:** Transfers control to the Linux kernel.

## Linux Kernel

OpenHCL runs on a minimal Linux kernel that provides memory management,
scheduling, process support, and device drivers.

**Key Responsibilities:**

- **Hardware Abstraction:** Manages CPU and memory resources.
- **Device Drivers:** Provides drivers for paravisor-specific hardware and standard devices.
- **Filesystem:** Mounts the initial ramdisk (initrd) as the root filesystem.
- **Process Management:** Launches the initial userspace process (`underhill_init`).

## Sidecar Kernel (x86_64)

On supported x86_64 systems, a lightweight sidecar kernel runs on selected
CPUs to reduce Linux boot cost and resource use.

For more details, see the [Sidecar Architecture](./sidecar.md) page.

**Source code:**
[openhcl/sidecar](https://github.com/microsoft/openvmm/tree/main/openhcl/sidecar)
| **Docs:**
[sidecar rustdoc](https://openvmm.dev/rustdoc/linux/sidecar/index.html)

**Key Responsibilities:**

- **Fast Boot:** Allows secondary CPUs to start without initializing the full
	Linux kernel.
- **Dispatch Loop:** Runs a minimal loop waiting for commands from the host or the main kernel.
- **On-Demand Conversion:** Can be converted to a full Linux CPU when required.

## Init Process (`underhill_init`)

`underhill_init` is the first Linux userspace process. It prepares the minimal
paravisor environment and replaces itself with `openvmm_hcl`.

**Source code:**
[openhcl/underhill_init](https://github.com/microsoft/openvmm/tree/main/openhcl/underhill_init)
| **Docs:**
[underhill_init rustdoc](https://openvmm.dev/rustdoc/linux/underhill_init/index.html)

**Key Responsibilities:**

- **System Setup:** Mounts necessary filesystems (e.g., `/proc`, `/sys`, `/dev`).
- **Environment Preparation:** Sets up the execution environment for the paravisor.
- **Process Launch:** `exec`s the main paravisor process (`openvmm_hcl`).

## Paravisor (`openvmm_hcl`)

`openvmm_hcl` is the central OpenHCL management process. It runs in Linux
userspace and orchestrates virtualization services.

For its startup, worker, trust, packaging, and diagnostics contracts, see the
dedicated [`openvmm_hcl`](./openvmm_hcl.md) page.

**Source code:**
[openhcl/openvmm_hcl](https://github.com/microsoft/openvmm/tree/main/openhcl/openvmm_hcl)
| **Docs:**
[openvmm_hcl rustdoc](https://openvmm.dev/rustdoc/linux/openvmm_hcl/index.html)

**Key Responsibilities:**

- **Policy & Management:** Manages the lifecycle of the VM and enforces security policies.
- **Host Communication:** Interfaces with the host VMM to receive commands and report status.
- **Servicing:** Orchestrates save and restore operations (VTL2 servicing).
- **Worker Management:** Spawns and manages the VM worker process.

## VM Worker (`underhill_vm`)

The VM worker process (`underhill_vm`) owns the VM's high-performance data path
and is spawned by `openvmm_hcl`.

**Source code:**
[openhcl/underhill_core](https://github.com/microsoft/openvmm/tree/main/openhcl/underhill_core)
| **Docs:**
[underhill_core rustdoc](https://openvmm.dev/rustdoc/linux/underhill_core/index.html)

**Key Responsibilities:**

- **VP Loop:** Runs the virtual processor loop, handling VM exits.
- **Device Emulation:** Coordinates in-process devices and isolated device
  workers.
- **I/O Processing:** Handles high-speed I/O operations.

## Diagnostics Server (`diag_server`)

The diagnostics server exposes development and monitoring operations for the
OpenHCL environment.

**Source code:**
[openhcl/diag_server](https://github.com/microsoft/openvmm/tree/main/openhcl/diag_server)
| **Docs:**
[diag_server rustdoc](https://openvmm.dev/rustdoc/linux/diag_server/index.html)

**Key Responsibilities:**

- **External Interface:** Listens on a VSOCK port for diagnostic connections.
- **Command Handling:** Processes diagnostic commands and queries.
- **Log Retrieval:** Provides access to system logs.

## Profiler Worker (`profiler_worker`)

The profiler worker is an optional, on-demand process used for performance
analysis when the selected build includes its required profiling components.

**Source code:**
[openhcl/profiler_worker](https://github.com/microsoft/openvmm/tree/main/openhcl/profiler_worker)
| **Docs:**
[profiler_worker rustdoc](https://openvmm.dev/rustdoc/linux/profiler_worker/index.html)

**Key Responsibilities:**

- **Performance Data Collection:** Collects profiling data (e.g., CPU usage, traces) when requested.
- **Isolation:** Runs in a separate process to minimize impact on the main workload.

## Device Worker Processes

OpenHCL can run chipset device emulators in separate processes through the
`chipset_device_worker` framework. This isolates device logic from the main VM
worker.

**Source code:**
[workers/chipset_device_worker](https://github.com/microsoft/openvmm/tree/main/workers/chipset_device_worker)
| **Docs:**
[chipset_device_worker rustdoc](https://openvmm.dev/rustdoc/linux/chipset_device_worker/index.html)

**Key Responsibilities:**

- **Device Isolation:** Runs selected emulators outside the main VM worker.
- **I/O Proxying:** Forwards MMIO, PIO, and PCI configuration operations.
- **Memory Access:** Proxies guest-memory access needed by the device.
- **State Management:** Handles device save/restore operations across process boundaries.

**Current Use Cases:**

- **TPM Emulation:** The virtual TPM can run in a separate worker to isolate
  cryptographic operations and state.

This architecture can be extended to isolate other chipset devices when their
security or reliability requirements justify a process boundary.
