# OpenVMM

[![Build Status](https://github.com/microsoft/openvmm/actions/workflows/openvmm-ci.yaml/badge.svg?branch=main)](https://github.com/microsoft/openvmm/actions/workflows/openvmm-ci.yaml)

OpenVMM is a modular, cross-platform, general-purpose Virtual Machine Monitor (VMM), written in Rust. 

OpenVMM supports a variety of host operating systems, architectures, and virtualization backends. Similar 
to other general-purpose VMMs (such as Hyper-V, QEMU, VirtualBox), OpenVMM is able to host a wide variety 
of both modern and legacy guest operating systems on-top of its flexible virtual hardware platform.

OpenVMM can be used as a traditional host VMM, where a VMM runs in a privileged host/root partition and 
provides virtualization services to a unprivileged guest partition. However, one particularly notable 
use-case of OpenVMM is in OpenHCL, which is OpenVMM as a paravisor. The OpenHCL "paravisor" model enables 
the VMM to provide virtualization services from within the guest partition itself. Paravisors are quite 
exciting, as they enable a wide variety of useful and novel virtualization scenarios.

## Getting Started

For info on how to run, build, and use OpenVMM, check out the [The OpenVMM Guide][].

The guide is published out of this repo via [Markdown files](Guide/src/SUMMARY.md).
Please keep them up-to-date.

[The OpenVMM Guide]: https://aka.ms/openvmmguide

## Contributing

This project welcomes contributions and suggestions.  Most contributions require you to agree to a
Contributor License Agreement (CLA) declaring that you have the right to, and actually do, grant us
the rights to use your contribution. For details, visit https://cla.opensource.microsoft.com.

When you submit a pull request, a CLA bot will automatically determine whether you need to provide
a CLA and decorate the PR appropriately (e.g., status check, comment). Simply follow the instructions
provided by the bot. You will only need to do this once across all repos using our CLA.

This project has adopted the [Microsoft Open Source Code of Conduct](https://opensource.microsoft.com/codeofconduct/).
For more information see the [Code of Conduct FAQ](https://opensource.microsoft.com/codeofconduct/faq/) or
contact [opencode@microsoft.com](mailto:opencode@microsoft.com) with any additional questions or comments.

## Trademarks

This project may contain trademarks or logos for projects, products, or services. Authorized use of Microsoft
trademarks or logos is subject to and must follow
[Microsoft's Trademark & Brand Guidelines](https://www.microsoft.com/en-us/legal/intellectualproperty/trademarks/usage/general).
Use of Microsoft trademarks or logos in modified versions of this project must not cause confusion or imply Microsoft sponsorship.
Any use of third-party trademarks or logos are subject to those third-party's policies.
