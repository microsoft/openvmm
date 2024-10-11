
# Running OpenVMM

This chapter provides a high-level overview of different ways to launch and
interact with OpenVMM.

* * *

To get started, ensure you have a copy of the OpenVMM executable and its runtime
dependencies, via one of the following options:

## Building OpenVMM Locally

Follow the instructions on: [Building OpenVMM](../../dev_guide/getting_started/build_openvmm.md).

## Pre-Built Binaries

If you would prefer to try OpenVMM without building it from scratch, you can
download pre-built copies of the binary from
[OpenVMM CI](https://github.com/microsoft/openvmm/actions/workflows/openvmm-ci.yaml).

Simply select a successful pipeline run (should have a Green checkbox), and
scroll down to select an appropriate `*-openvmm` artifact for your particular
architecture and operating system.

**On Windows:** You must also download a copy of `lxutil.dll` from
[`microsoft/openvmm-deps`](https://github.com/microsoft/openvmm-deps/releases/tag/Microsoft.WSL.LxUtil.10.0.26100.1-240331-1435.ge-release)
on GitHub, and ensure it is in the same directory as `openvmm.exe`.
