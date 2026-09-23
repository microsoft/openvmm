# OpenHCL Processes and Components

This page identifies the major software components and long-running processes
in OpenHCL. See the [OpenHCL boot flow](./boot.md) for their startup order.

## Boot components

The host loads these components before the main paravisor process starts:

- [`openhcl_boot` source](https://github.com/microsoft/openvmm/tree/main/openhcl/openhcl_boot)
	([rustdoc](https://openvmm.dev/rustdoc/linux/openhcl_boot/index.html)):
	Initializes VTL2, validates its inputs, builds the Linux device tree, and
	transfers control to Linux.
- **Linux kernel:** Provides memory management, scheduling, process support,
	and the drivers used by the paravisor.
- [`sidecar` source](https://github.com/microsoft/openvmm/tree/main/openhcl/sidecar)
	([rustdoc](https://openvmm.dev/rustdoc/linux/sidecar/index.html)):
	Runs a small dispatch kernel on selected x86_64 processors that have not been
	brought online in Linux. See [Sidecar Kernel](./sidecar.md).
- [`underhill_init` source](https://github.com/microsoft/openvmm/tree/main/openhcl/underhill_init)
	([rustdoc](https://openvmm.dev/rustdoc/linux/underhill_init/index.html)):
	Runs as Linux PID 1, prepares the user-mode environment, and replaces itself
	with `openvmm_hcl`.

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
