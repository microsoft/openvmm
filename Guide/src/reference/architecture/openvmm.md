# OpenVMM Architecture

This section describes the architecture of OpenVMM when it runs as a hosted
VMM.

- [Memory Layout](./openvmm/memory-layout.md) describes the guest physical
  address space.
- [Memory Backing](./openvmm/memory-backing.md) explains how guest RAM is
  allocated and shared.
- [NUMA Topology](./openvmm/numa.md) covers guest NUMA configuration and host
  memory placement.
- [Device Tree](./openvmm/device-tree.md) describes the device trees that
  OpenVMM builds for IGVM files and ARM64 Linux direct boot.
- [Timekeeping](./timekeeping.md) explains clocks, timers, and their
  behavior across VM lifecycle changes.
- [mesh](./openvmm/mesh.md) describes OpenVMM's inter-process communication
  framework.
