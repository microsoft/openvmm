# IGVM Overview

The Isolated Guest Virtual Machine (IGVM) format describes the initial state of an isolated virtual machine. OpenHCL ships as an IGVM so the host can load the package, place each component at the requested physical address in VTL2 memory, and pass configuration into the paravisor.

> **Note:** VTL2 (Virtual Trust Level 2) is the privilege level where OpenHCL code runs, isolated from the VTL0 guest. For background, see the [OpenHCL architecture overview](./openhcl.md#vtls).

## Package Contents

An OpenHCL IGVM bundles the artifacts that must be present when VTL2 starts:

- **Boot shim** (`openhcl_boot`) – first instruction stream executed in VTL2
- **Linux kernel** – minimal VTL2 operating system used by the paravisor
- **Sidecar kernel** (x86_64) – lightweight kernel used to scale to large CPU counts
- **Initial ramdisk (initrd)** – root filesystem containing `underhill_init`, `openvmm_hcl`, and shared dependencies; both executables point at the same binary and choose their persona via the invoked name
- **Memory layout directives** – target addresses for each image
- **Configuration parameters** – CPU topology, devices, logging options, and other boot-time settings

## Tooling and Builds

The host VMM builds the IGVM artifact using OpenHCL's build scripts; see [Building OpenHCL](../../dev_guide/getting_started/build_openhcl.md) for the end-to-end flow. The file format itself, validation tooling, and examples live in the [IGVM repository](https://github.com/microsoft/igvm).
