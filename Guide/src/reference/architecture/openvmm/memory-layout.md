# Memory Layout

OpenVMM computes guest physical address layouts by combining fixed platform
ranges, RAM requests, MMIO requests, and private implementation ranges through a
single deterministic allocator.

The memory layout is part of the VM compatibility contract. Guest operating
systems remember RAM and device addresses across hibernation, and saved VM state
contains device state tied to those addresses. For an existing VM, changing
request order, placement class, or alignment policy can move guest physical
addresses and break resume.

```admonish warning title="Compatibility surface"
Treat layout policy changes like VM ABI changes. A new default can be fine for
new VMs, but existing persisted VM configuration must continue to resolve to the
same guest physical addresses.
```

## Layers

Memory layout is split across three layers:

| Layer | Responsibility |
|---|---|
| `vm_topology::layout` | Pure address-space allocation. |
| `openvmm_core::worker::memory_layout` | Production VM policy and validation. |
| `vm_topology::memory::MemoryLayout` | Shared validation and query API. |

[`vm_topology::layout::LayoutBuilder`](https://openvmm.dev/rustdoc/linux/vm_topology/layout/struct.LayoutBuilder.html)
knows only about ranges, sizes, alignments, and placement classes. It does not
know about chipsets, firmware, VTLs, PCI, or host physical address width.
Callers express policy by adding fixed ranges, reserved ranges, RAM requests,
and dynamic MMIO requests.

The VM worker owns the production policy. It feeds existing chipset MMIO gaps
into the allocator as fixed occupied ranges, resolves PCIe root complex ECAM
from an optional fixed range or the root-complex bus window, resolves PCIe low
MMIO and high MMIO from typed intents, then asks the allocator to place RAM and
private implementation ranges. Future work moves more MMIO consumers from
precomputed gaps into typed dynamic requests.

`MemoryLayout` remains the object other worker code uses to query RAM, MMIO,
PCI ECAM, PCI MMIO, VTL2 memory, and the VTL0-visible layout top.

## Request Types

The allocator accepts these input forms:

| Input | Meaning |
|---|---|
| `reserve(tag, range)` | Blocks allocation, but does not raise layout top. |
| `fixed(tag, range)` | Already-known occupied range that is part of layout. |
| `ram(tag, target, size, alignment)` | Splittable ordinary RAM request. |
| `request(..., Placement::Mmio32)` | Single range below 4 GB, packed top down. |
| `request(..., Placement::Mmio64)` | Single range after RAM, packed bottom up. |
| `request(..., Placement::PostMmio)` | Single range after all VTL0 RAM and MMIO. |

`reserve` is for architectural holes that must block allocation but should not
make the VTL0 layout appear larger. A high reserved hole near the top of the
address space, for example, should not force VTL2 or high MMIO above that hole.

`fixed` is for ranges that have already been resolved by policy or existing
configuration. Fixed ranges block all dynamic allocation and are included in the
returned placed ranges.

## Allocation Order

The allocator is deterministic for the same request list. The phase order is:

1. Remove reserved ranges from free space.
2. Remove fixed ranges from free space.
3. Allocate 32-bit MMIO below 4 GB, top down.
4. Allocate ordinary RAM from GPA 0 upward, splitting around holes.
5. Allocate 64-bit MMIO from the end of RAM upward.
6. Allocate post-MMIO ranges after the VTL0-visible layout.

Within MMIO phases, requests are ordered by alignment, then size, then caller
order. RAM and post-MMIO requests use caller order because those orders carry
policy. RAM request order assigns NUMA vnode ownership. Post-MMIO request order
keeps private implementation ranges from being reordered by alignment.

## Worker Policy

The VM worker resolver applies the production policy in
`openvmm/openvmm_core/src/worker/memory_layout.rs`:

1. Validate total RAM size and optional per-vNUMA budgets.
2. Add existing chipset MMIO gaps as fixed ranges.
3. Add PCIe root complex ECAM and low MMIO requests as `Placement::Mmio32`.
   A root complex with no fixed ECAM range gets an ECAM size derived from its
   bus window.
4. Add PCIe root complex high MMIO requests as `Placement::Mmio64`.
5. Add RAM requests in vnode order.
6. Add optional IGVM VTL2 memory as `Placement::PostMmio`.
7. Allocate all ranges.
8. Build `MemoryLayout` from resolved RAM, chipset MMIO gaps, and resolved PCIe
   ranges.
9. Validate the VTL0-visible layout top against host physical address width.

Host physical address width is deliberately not an allocator input. The layout
is computed from VM configuration first, then checked against the host. That
keeps guest physical addresses from changing just because the VM runs on a host
with a different physical address width.

## RAM Alignment

Worker RAM requests use two alignment policies:

| RAM request size | Alignment |
|---|---|
| Less than 1 GB | 2 MB |
| At least 1 GB | 1 GB |

The alignment is also split granularity. If a RAM request cannot fit entirely in
the current free range, the allocator rounds the non-final chunk down to the
request alignment before continuing. That prevents a tiny fixed hole from
creating odd sub-GB RAM fragments in an otherwise GB-sized VM.

Sub-GB RAM requests use 2 MB alignment so small NUMA nodes do not waste a full
GB of guest physical address space.

## VTL2 Placement

IGVM files can request VTL2 memory using `Vtl2BaseAddressType::MemoryLayout`.
The worker derives only a size and alignment from the IGVM file. It does not
feed IGVM relocation min/max bounds into layout.

VTL2 memory is allocated as `Placement::PostMmio`, after all VTL0-visible RAM
and MMIO. Enabling VTL2 must not move VTL0 RAM or device ranges. The selected
VTL2 base is later validated by the IGVM loader against the file's relocation
records. Unsupported IGVM files fail there instead of reshaping the VTL0 layout.

## Examples

The examples below use compact synthetic ranges. They describe the same policy
that the unit tests cover in `openvmm_core::worker::memory_layout` and
`vm_topology::layout`.

### Fixed MMIO Splits RAM

A VM with 4 GB of RAM and a fixed MMIO hole from 1 GB to 2 GB gets RAM on both
sides of the hole.

| Input | Range |
|---|---|
| RAM request | 4 GB |
| Fixed MMIO | `0x4000_0000..0x8000_0000` |

| Output | Range |
|---|---|
| RAM | `0x0000_0000..0x4000_0000` |
| MMIO | `0x4000_0000..0x8000_0000` |
| RAM | `0x8000_0000..0x1_4000_0000` |

The total RAM is still 4 GB. The fixed range is occupied address space, not RAM.

### GB RAM Chunks Stay GB-Sized

A 2 GB RAM request with a small fixed hole just above 1 GB should not create a
nearly-1-GB chunk plus a tiny fragment.

| Input | Range |
|---|---|
| RAM request | 2 GB, 1 GB alignment |
| Fixed MMIO | `0x4010_0000..0x4020_0000` |

| Output | Range |
|---|---|
| RAM | `0x0000_0000..0x4000_0000` |
| Fixed MMIO | `0x4010_0000..0x4020_0000` |
| RAM | `0x8000_0000..0xC000_0000` |

The allocator uses the first full 1 GB chunk, skips the interrupted region, and
continues at the next 1 GB boundary.

### Small NUMA Nodes Use 2 MB Alignment

For two 512 MB NUMA nodes, using 1 GB alignment would waste address space and
make the layout harder to read. The worker uses 2 MB alignment for sub-GB RAM
requests.

| Input | Size |
|---|---|
| vnode 0 RAM | 512 MB |
| vnode 1 RAM | 512 MB |

| Output | Range |
|---|---|
| vnode 0 RAM | `0x0000_0000..0x2000_0000` |
| vnode 1 RAM | `0x2000_0000..0x4000_0000` |

The request order is the vnode assignment order, so changing it changes the NUMA
layout.

### VTL2 Does Not Move VTL0

Start with 2 GB of VTL0 RAM and a fixed MMIO hole from 1 GB to 2 GB.

| VTL0 output | Range |
|---|---|
| RAM | `0x0000_0000..0x4000_0000` |
| MMIO | `0x4000_0000..0x8000_0000` |
| RAM | `0x8000_0000..0xC000_0000` |

If the IGVM file asks for 2 MB of VTL2 memory, the VTL0 layout stays exactly the
same. VTL2 is placed separately after the VTL0-visible top.

| Private output | Range |
|---|---|
| VTL2 | `0xC000_0000..0xC020_0000` |

`MemoryLayout::end_of_layout()` reports the VTL0-visible top. VTL2 remains
available through `MemoryLayout::vtl2_range()`.

### Reserved High Holes Do Not Raise Layout Top

A reserved range blocks allocation, but it does not describe a guest-visible
resource. If a VM has 2 GB of RAM and a high reserved hole, post-MMIO memory can
still start immediately after the VTL0 layout.

| Input | Range |
|---|---|
| RAM request | 2 GB |
| Reserved hole | `0xFD_0000_0000..0xFD_4000_0000` |
| Post-MMIO request | 1 MB |

| Output | Range |
|---|---|
| RAM | `0x0000_0000..0x8000_0000` |
| Post-MMIO | `0x8000_0000..0x8010_0000` |

The reserved hole is not returned at the end of the sorted layout because it is
only a constraint. If a reserved range sits between returned allocations, it is
reported so callers can inspect the occupied map.

## Where To Update This Page

Update this page when changing any of these behaviors:

- placement phase order in `vm_topology::layout`
- `reserve`, `fixed`, `ram`, or `request` semantics
- worker RAM alignment policy
- VTL2 `MemoryLayout` placement
- host physical-address validation policy
- `MemoryLayout::end_of_layout()` or `MemoryLayout::vtl2_range()` semantics
