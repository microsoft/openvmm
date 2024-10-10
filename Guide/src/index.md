# Introduction

OpenVMM is a modular, cross-platform, general-purpose Virtual Machine Monitor
(VMM), written in Rust. The project is open-source under the MIT License, and
developed openly on
[github.com/microsoft/openvmm](https://github.com/microsoft/openvmm).

* **Cross-Platform**

  OpenVMM supports on a variety of host operating systems, architectures, and
  virtualization backends:

  | Host OS             | Architecture  | Virtualization API                     |
  | ------------------- | ------------- | -------------------------------------- |
  | Windows             | x64 / Aarch64 | WHP (Windows Hypervisor Platform)      |
  | Linux               | x64           | KVM                                    |
  |                     | x64           | MSHV (Microsoft Hypervisor)            |
  | macOS               | Aarch64       | Hypervisor.framework                   |
  | Linux ([paravisor]) | x64 / Aarch64 | MSHV (using [VBS] / [TDX] / [SEV-SNP]) |

[paravisor]: ./user_guide/openhcl.md
[VBS]: https://learn.microsoft.com/en-us/windows-hardware/design/device-experiences/oem-vbs
[TDX]: https://www.intel.com/content/www/us/en/developer/tools/trust-domain-extensions/overview.html
[SEV-SNP]: https://www.amd.com/en/developer/sev.html

* **General Purpose**

  OpenVMM can host a wide variety of popular guest operating systems (such as
  Windows, Linux, and FreeBSD), with support for both modern and legacy versions
  of those operating systems.

  - Modern operating systems (which boot via UEFI, or Linux Direct boot) can
  interface with OpenVMM's wide selection of modern paravirtualized VirtIO and
  VMBus-based paravirtualized devices.

  - Legacy operating systems (which boot via legacy x86 BIOS) can interface with
  OpenVMM's various emulated devices, including legacy IDE hard-disk/optical
  hardware, floppy disk drives, and VGA graphics cards.

* **Modular**

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

  One particularly notable specialized build of OpenVMM is
  [**OpenHCL**](./user_guide/openhcl.md) (AKA, OpenVMM as a paravisor), which
  provides virtualization services from _inside_ a guest virtual machine, rather
  than in the privileged host/root partition.
