# Introduction

OpenVMM is a modular, cross-platform Virtual Machine Monitor (VMM), written in
Rust.

Although it can function as a traditional VMM, OpenVMM's development is
currently focused on its role in the [OpenHCL paravisor][paravisor].

The project is open-source, MIT Licensed, and developed publicly at
[microsoft/openvmm](https://github.com/microsoft/openvmm) on GitHub.

## Cross-Platform

OpenVMM supports a variety of host operating systems, architectures, and
virtualization backends:

| Host OS             | Architecture  | Virtualization API                     |
| ------------------- | ------------- | -------------------------------------- |
| Linux ([paravisor]) | x64 / Aarch64 | MSHV (using [VSM] / [TDX] / [SEV-SNP]) |
| Windows             | x64 / Aarch64 | WHP (Windows Hypervisor Platform)      |
| Linux               | x64 / Aarch64 | KVM                                    |
|                     | x64           | MSHV (Microsoft Hypervisor)            |
| macOS               | Aarch64       | Hypervisor.framework                   |

## Running in the OpenHCL paravisor

OpenVMM is the VMM that runs in the [OpenHCL paravisor][paravisor].

Unlike in traditional virtualization, where a VMM runs in a privileged host/root
partition and provides virtualization services to a unprivileged guest
partition, the "paravisor" model enables a VMM to provide virtualization
services from _within_ the guest partition itself.

It can be considered a form of "virtual firmware", running at a higher privilege
level than the primary guest OS.

Paravisors are quite exciting, as they enable a wide variety of useful and novel
virtualization scenarios! For example: at Microsoft, OpenHCL plays a key role in
enabling several important Azure scenarios:

- Enabling existing workloads to seamlessly leverage [Azure Boost] (Azure's
  next-generation hardware accelerator), without requiring any modifications to
  the guest VM image.

- Enabling existing guest operating systems to run inside [Confidential VMs].

- Powering [Trusted Launch VMs] - VMs that support Secure Boot, and include a
  vTPM.

## Standalone VMM

OpenVMM can also run as a general-purpose VMM on a Windows, Linux, or macOS
host. At the moment, this is primarily a development vehicle: most of the same
code runs in OpenVMM on a host and OpenVMM in a paravisor, and it is often
easier to test it on a host.

We will continue to build and test OpenVMM in this configuration, but currently
we are not focused on the goal of supporting this for production workloads. It
is missing many of the features and interface stability that are required for
general-purpose use. We recommend you consider other Rust-based VMMs such as
[Cloud Hypervisor](https://github.com/cloud-hypervisor/cloud-hypervisor) for
such use cases.

## Relationship to other Rust-based VMMs

OpenVMM's core security principles are aligned with those of the Rust-based
Cloud Hypervisor, Firecracker, and crosvm projects, which is why we also chose
to write OpenVMM in Rust. However, OpenVMM's unique goal of running efficiently
in a paravisor environment made it difficult to leverage existing projects.
OpenVMM requires fine-grained control over thread and task scheduling in order
to avoid introducing jitter and other performance issues into guest VMs. It is
difficult to achieve these requirements with traditional, thread-based
designs.

Instead, OpenVMM uses Rust's `async` support throughout its codebase, decoupling
the policy details of _where_ code runs (which OS threads) from the mechanism of
_what_ runs (device-specific emulators). In a paravisor or resource-constrained
environment, OpenVMM can run with one thread per guest CPU and ensure that
device work is cooperatively scheduled along with the guest OS. In more
traditional virtualization host, OpenVMM can run with one thread per device to
use host CPUs to fully parallelize guest CPU and IO processing.

This approach has a significant impact on the design and implementation of the
codebase, and bringing this model to an existing VMM would be a major
undertaking. We came to the conclusion that a new project was the best way to
achieve this goal.

We are indebted to the Rust VMM community for their trailblazing work. Now that
the OpenVMM project is open source, we hope to find ways to collaborate on
shared code while maintaining the benefits of the OpenVMM architecture.

## Guest Compatibility

Similar to other general-purpose VMMs (such as Hyper-V, QEMU, VirtualBox),
OpenVMM is able to host a wide variety of both modern and legacy guest operating
systems on-top of its flexible virtual hardware platform.

- Modern operating systems can boot via UEFI, and interface with a wide
selection of paravirtualized devices for services like networking, storage, and
graphics.

- Legacy x86 operating systems can boot via BIOS, and are presented with a
PC-compatible emulated device platform which includes legacy hardware such as
IDE hard-disk/optical drives, floppy disk drives, and VGA graphics cards.

OpenVMM is regularly tested to ensure compatibility with popular operating
systems (such as Windows, Linux, and FreeBSD), and strives to maintain
reasonable compatibility with other, more niche/legacy operating systems as
well.

* * *

To learn more about different facets of the OpenVMM project, check out the
following links:

|                                                                               |                                           |
| ----------------------------------------------------------------------------- | ----------------------------------------- |
| [Getting Started: OpenVMM](./user_guide/openvmm.md)                           | Running OpenVMM as traditional host VMM   |
| [Getting Started: OpenHCL](./user_guide/openhcl.md)                           | Running OpenVMM as a paravisor (OpenHCL)  |
| [Developer Guide: Getting Started](./dev_guide/getting_started.md)            | Building OpenVMM / OpenHCL locally        |
| [[Github] microsoft/openvmm](https://github.com/microsoft/openvmm)            | Viewing / Downloading OpenVMM source code |
| [[Github] OpenVMM issue tracker](https://github.com/microsoft/openvmm/issues) | Reporting OpenVMM issues                  |

[paravisor]: ./user_guide/openhcl.md
[VSM]:
    https://learn.microsoft.com/en-us/virtualization/hyper-v-on-windows/tlfs/vsm
[Azure Boost]: https://learn.microsoft.com/en-us/azure/azure-boost/overview
[Confidential VMs]:
    https://azure.microsoft.com/en-us/solutions/confidential-compute
[Trusted Launch VMs]:
    https://learn.microsoft.com/en-us/azure/virtual-machines/trusted-launch
[TDX]:
    https://www.intel.com/content/www/us/en/developer/tools/trust-domain-extensions/overview.html
[SEV-SNP]:
    https://www.amd.com/content/dam/amd/en/documents/epyc-business-docs/white-papers/SEV-SNP-strengthening-vm-isolation-with-integrity-protection-and-more.pdf
