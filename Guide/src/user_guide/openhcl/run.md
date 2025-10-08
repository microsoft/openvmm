# Running OpenHCL

This chapter provides a high-level overview of different ways to launch and
interact with OpenHCL.

## OpenHCL Platforms

- [On Windows - Hyper-V](./run/hyperv.md)
- [On Windows - OpenVMM](./run/openvmm.md)
- [On Linux](./run/openvmm_linux.md)

## High-level Overview

In order to run OpenHCL, an existing host VMM must first load the OpenHCL
environment into a VM, much akin to existing virtual firmware layers, like UEFI,
or BIOS[^vtls].

OpenHCL is distributed as an [IGVM] file (Independent Guest Virtual Machine),
which encapsulates all the directives and data required to launch a particular
virtual machine configuration on any given virtualization stack.

At this time, the only VMMs which are able to load and host OpenHCL IGVM files
are Hyper-V, and OpenVMM.

## Obtaining a copy of OpenHCL

To get started, ensure you have a copy of an OpenHCL IGVM firmware image, via
one of the following options:

### Building OpenHCL Locally

Follow the instructions on: [Building OpenHCL](../../dev_guide/getting_started/build_openhcl.md).

Note: At this time, OpenHCL can only be built on Linux / WSL2.

### Pre-Built Binaries

If you would prefer to try OpenHCL without building it from scratch, you can
download pre-built copies of OpenHCL IGVM files from
[OpenVMM CI](https://github.com/microsoft/openvmm/actions/workflows/openvmm-ci.yaml).

Simply select a successful pipeline run (should have a Green checkbox), and
scroll down to select an appropriate `*-openhcl-igvm` artifact for your
particular architecture and operating system.

[IGVM]: https://github.com/microsoft/igvm

[^vtls]: Though, unlike UEFI / BIOS, OpenHCL is loaded into a distinct, higher
    privilege execution context within the VM, called
    [VTL2](../../reference/architecture/openhcl.md#vtls).
