# OpenHCL Architecture

**Prerequisites:**

- [Getting Started: OpenHCL](../../user_guide/openhcl.md).
- [Architecture: OpenVMM Architecture](./openvmm.md).

* * *

## Overview

OpenHCL is a paravisor execution environment that runs within the guest partition of a virtual machine. It provides virtualization services to the guest OS from within the guest itself, rather than from the host.

The following diagram offers a brief, high-level overview of the OpenHCL Architecture.

![OpenHCL High Level Overview](./_images/openhcl.png)

## Virtual Trust Levels (VTLs)

OpenHCL relies on [Virtual Trust Levels] (VTLs) to establish a security boundary between itself and the guest OS.

- **VTL2:** OpenHCL runs here[^sk]. It has higher privileges and is isolated from VTL0.
- **VTL0:** The Guest OS (e.g., Windows, Linux) runs here. It cannot access VTL2 memory or resources.

This isolation is enforced by the underlying hypervisor (Hyper-V) and can be backed by:

- Hardware [TEEs], like Intel [TDX] and AMD [SEV-SNP].
- Software-based constructs, like Hyper-V [VSM].

## Scenarios

OpenHCL enables several key scenarios by providing a trusted execution environment within the VM:

### Azure Boost

OpenHCL acts as a compatibility layer for Azure Boost. It translates legacy synthetic device interfaces (like VMBus networking and storage) used by the guest OS into the hardware-accelerated interfaces (proprietary [Microsoft Azure Network Adapter] (MANA) and NVMe) provided by the Azure Boost infrastructure. This allows unmodified guest images to take advantage of next-generation hardware.

The following diagram shows a high level overview of how synthetic networking is supported in OpenHCL over Microsoft Azure Network Adapter (MANA)

![OpenHCL Synthetic Networking](./_images/openhcl-synthetic-nw.png)

The following diagram shows a high level overview of how accelerated networking is supported in OpenHCL over MANA

![OpenHCL Accelerated Networking](./_images/openhcl-accelnet.png)

### Confidential Computing

In Confidential VMs (CVMs), the host is not trusted. OpenHCL runs inside the encrypted VM context (VTL2) and provides necessary services (like device emulation and TPM) that the untrusted host cannot securely provide.

### Trusted Launch

OpenHCL hosts a virtual TPM (vTPM) and enforces Secure Boot policies, ensuring the integrity of the guest boot process.

## Architecture Components

OpenHCL is built on top of a specialized Linux kernel and consists of several userspace processes that work together to provide these services.

For more details on the internal components and boot process, see:

- [Processes and Components](./openhcl/processes.md)
- [Boot Flow](./openhcl/boot.md)
- [Sidecar](./openhcl/sidecar.md)
- [IGVM Artifact](./openhcl/igvm.md)

[^sk]: Why not VTL1? Windows already uses VTL1 in order to host the [Secure Kernel].

[VSM]: https://learn.microsoft.com/en-us/virtualization/hyper-v-on-windows/tlfs/vsm
[Virtual Trust Levels]: https://learn.microsoft.com/en-us/virtualization/hyper-v-on-windows/tlfs/vsm
[TDX]: https://www.intel.com/content/www/us/en/developer/tools/trust-domain-extensions/overview.html
[SEV-SNP]: https://www.amd.com/content/dam/amd/en/documents/epyc-business-docs/white-papers/SEV-SNP-strengthening-vm-isolation-with-integrity-protection-and-more.pdf
[TEEs]: https://en.wikipedia.org/wiki/Trusted_execution_environment
[Secure Kernel]: https://www.microsoft.com/en-us/security/blog/2020/07/08/introducing-kernel-data-protection-a-new-platform-security-technology-for-preventing-data-corruption/
[Microsoft Azure Network Adapter]: https://learn.microsoft.com/en-us/azure/virtual-network/accelerated-networking-mana-overview
