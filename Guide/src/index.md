# Introduction

OpenVMM is a modular, cross-platform, general-purpose Virtual Machine Monitor
(VMM), written in Rust.

The project is open-source, MIT Licensed, and developed publicly at
[microsoft/openvmm](https://github.com/microsoft/openvmm) on GitHub.

**Cross-Platform**

OpenVMM supports a variety of host operating systems, architectures, and
virtualization backends:

| Host OS             | Architecture  | Virtualization API                     |
| ------------------- | ------------- | -------------------------------------- |
| Windows             | x64 / Aarch64 | WHP (Windows Hypervisor Platform)      |
| Linux               | x64           | KVM                                    |
|                     | x64           | MSHV (Microsoft Hypervisor)            |
| macOS               | Aarch64       | Hypervisor.framework                   |
| Linux ([paravisor]) | x64 / Aarch64 | MSHV (using [VBS] / [TDX] / [SEV-SNP]) |

**General Purpose**

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
systems (such as Windows, Linux, and FreeBSD), and strives to maintain reasonable
compatibility with other, more niche/legacy operating systems as well.

**Modular**

OpenVMM is designed from the ground up to support a wide variety of distinct
virtualization scenarios, each with their own unique needs and constraints.

Rather than relying on a "one size fits all" solution, the OpenVMM project
enables users to build specialized versions of OpenVMM with the precise set of
features required to power their particular scenario.

For example: A build of OpenVMM designed to run on a user's personal PC might
compile-in all available features, in order support a wide variety of
workloads, whereas a build of OpenVMM designed to run linux container
workloads might opt for a narrow set of enabled features, in order to minimize
resource consumption and VM-visible surface area.

* * *

One particularly notable use-case of OpenVMM is in
[**OpenHCL**](./user_guide/openhcl.md) (AKA, OpenVMM as a paravisor).

Unlike in the traditional virtualization model, where a VMM runs in a privileged
host/root partition and provides virtualization services to a unprivileged guest
partition, the "paravisor" model enables a VMM to provide virtualization
services from _within_ the guest partition itself.

This is exciting, as it enables a wide variety of useful and novel
virtualization scenarios.

For example: at Microsoft, OpenHCL plays a key role in enabling several
important Azure scenarios:

- Enabling existing workloads to seamlessly leverage [Azure Boost] (Azure's
  next-generation hardware accelerator), without requiring any modifications to
  the guest VM image.

- Enabling existing, un-enlightened guest operating systems to run inside
  software and hardware-backed [Confidential VMs].

- Powering [Trusted Launch VMs] - VMs that support Secure Boot, and include a
  vTPM.

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
[VBS]: https://learn.microsoft.com/en-us/windows-hardware/design/device-experiences/oem-vbs
[Azure Boost]: https://learn.microsoft.com/en-us/azure/azure-boost/overview
[Confidential VMs]: https://azure.microsoft.com/en-us/solutions/confidential-compute
[Trusted Launch VMs]: https://learn.microsoft.com/en-us/azure/virtual-machines/trusted-launch
[TDX]: https://www.intel.com/content/www/us/en/developer/tools/trust-domain-extensions/overview.html
[SEV-SNP]: https://www.amd.com/en/developer/sev.html
