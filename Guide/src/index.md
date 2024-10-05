# Introduction

OpenVMM is a virtual machine monitor (VMM) written in Rust for Windows and Linux.
It was codenamed HvLite so you may see some references to this name in the
documentation and code. The HvLite name comes from the project being a
lightweight redesign of the Hyper-V VMM (virtualization stack), with the goal of
supporting most of the guest-visible features of Hyper-V. However, OpenVMM has a
few significant design departures from Hyper-V:

* Cross-platform: OpenVMM runs on Windows and Linux, and it can run on either the
  Microsoft hypervisor or on KVM.

* Decoupled: Unlike Hyper-V, OpenVMM is built in its own repo and can run on
  multiple versions of Windows. It runs as an individual process, without the
  presence of any control service/daemon. This improves development and
  deployment time and simplifies servicing.

* Public APIs: Wherever possible, OpenVMM depends on public Windows (and Linux)
  APIs. It uses the Windows Hypervisor Platform (WHP) interface to interact with
  the Microsoft hypervisor.

* User mode: OpenVMM implements as much as possible in user mode. This reduces
  the impact of successful guest attacks, improves developer productivity, and
  simplifies servicing.

An important use case of the OpenVMM VMM is
[OpenHCL](./user_guide/openhcl/index.md), an environment for providing
virtualization services from inside Virtual Trust Level 2 (VTL2) in a guest
virtual machine rather than in the privileged host/root partition.

This guide describes the architecture and provides developer guidelines for both
OpenVMM and OpenHCL. For instructions on setting up the OpenVMM repo locally,
please refer to the getting started guide for
[Windows](./user_guide/getting_started/windows.md) or
[WSL2](./user_guide/getting_started/linux.md).

Once you have the dependencies installed, please refer to the instructions for
building [OpenVMM](./user_guide/openvmm/build.md) or
[OpenHCL](./user_guide/openhcl/build.md), (and optionally, setup
your [development environment](./dev_guide/suggested_dev_env.md))

The latest version of this guide can be found at [here](https://aka.ms/openvmmguide).

The source code for OpenVMM can be found at [https://aka.ms/openvmm](https://aka.ms/openvmm).
