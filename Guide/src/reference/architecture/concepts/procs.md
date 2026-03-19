# Processors in the VMM

## VP index, CPU number, and APIC ID

Much code in the OpenVMM repo rely on a numeric identifier for a virtual
processor (VP). This is a VMM-specific VP index, which is the hypervisor-level
identifier assigned to each virtual processor, starting at 0. Three identifiers
are often confused:

| Identifier | What it is | Numbering |
|-----------|-----------|-----------|
| **VP index** | Hypervisor-assigned processor number | 0, 1, 2, ... contiguous |
| **Linux CPU number** | The kernel's `cpu` in OpenHCL | Currently equals VP index (see below) |
| **APIC ID** (x86) | Hardware interrupt target | May differ — depends on topology |
| **MPIDR** (aarch64) | ARM processor affinity register | Not the VP index — topology-dependent |

Each platform has its own architectural way of describing cpus, with x86 APIC
IDs and MPIDR on AArch64. Note that these values cannot be assumed to map
directly to VP index, as the physical or virtual topology of a system determines
the values for these architectural identifiers.

These can be different even than the **VTL0 guest's** perspective. The guest may
have its own CPU numbering (which may or may not match the VP index). Guests are
required to translate the guest VP number to a hypervisor VP number, which is
then passed to the VMM. For example, The VMBus protocol allows guest drivers to
specify a VP index for a channel.

```text
  VTL0 guest sees:          Host / VTL2 sees:
  ┌──────────────┐          ┌──────────────┐
  │ CPU 0 ───────┼────────► │ VP index 0   │
  │ CPU 1 ───────┼────────► │ VP index 1   │
  │ CPU 2 ───────┼────────► │ VP index 2   │
  │   ...        │          │   ...        │
  └──────────────┘          └──────────────┘
  Guest CPU N maps to       VP index N = Linux
  VP index N (typical)      CPU N (OpenHCL today)
```

In OpenHCL today, the VMM assumes that its view of the VP index is the same as
the CPU number in the OpenHCL Linux Kernel. This is a simplifying assumption,
not an architectural guarantee. This works because OpenHCL's boot shim validates
that device-tree CPU ordering matches VP index ordering. This mapping is not
guaranteed in a general purpose guest. The boot shim also controls the CPU
online sequence to maintain the mapping.

The APIC ID is a separate concept. On x86, the APIC ID may not match the VP
index, especially with complex topologies (multiple sockets, SMT). The
hypervisor provides a [`GetVpIndexFromApicId`
hypercall](https://learn.microsoft.com/en-us/virtualization/hyper-v-on-windows/tlfs/hypercalls/hvcallgetvpindexfromapicid)
for translation. On aarch64, the device tree `reg` property for each CPU is the
MPIDR, which is also not the VP index.
