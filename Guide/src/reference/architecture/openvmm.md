# OpenVMM Architecture

This section describes the architecture of OpenVMM when it runs as a hosted
VMM.

- [Memory Layout](./openvmm/memory-layout.md) describes the guest physical
  address space.
- [Memory Backing](./openvmm/memory-backing.md) explains how guest RAM is
  allocated and shared.
- [NUMA Topology](./openvmm/numa.md) covers guest NUMA configuration and host
  memory placement.
- [mesh](./openvmm/mesh.md) describes OpenVMM's inter-process communication
  framework.

## Partition time

The optional
[`virt::PartitionTimeControl`](https://openvmm.dev/rustdoc/virt/trait.PartitionTimeControl.html)
interface controls backend partition time independently of VP execution.
Supporting backends create partitions with time frozen. A full VM stop stops
all VPs before freezing partition time, so save, restore, and reset operate
without time advancing. Resume thaws time before starting VPs. Setting a saved
clock value does not itself freeze or thaw time.

Temporary VP stops, including debugger halts and firmware servicing, do not
freeze time. A VTL scrub can reset and freeze that VTL's clock; the partition
unit thaws it before restarting VPs. The lifecycle interface is infallible:
backends treat unexpected time-control failures as fatal, rather than allowing
execution or state capture with an unknown clock state. Initial partition
creation can still report setup errors.

MSHV and WHP implement this interface. WHP controls both its VTL0 and optional
VTL2 backing partitions. KVM and HVF do not yet expose this interface. OpenHCL
does not use it to freeze the surrounding partition's clock.

Software device time is managed separately by `VmTimeKeeper`. The partition
state unit depends on the software clock, so VPs and backend time stop before
the software clock stops, and resume after it starts. This ordering does not
provide an atomic snapshot of all hardware and software clocks.
