# Memory Layout

OpenVMM has to decide where every byte of guest physical address space goes:
RAM, MMIO windows for emulated and PCIe devices, paravisor private memory, and
architectural ranges like the LAPIC or GIC. This page describes how those
decisions are made.

```admonish warning title="Compatibility surface"
Guest physical addresses are part of the VM's compatibility contract. Guests
remember device and RAM locations across hibernation, and saved VM state
references them. Changing request order, placement class, or alignment can
move guest addresses and break resume on existing VMs.

Treat layout policy changes like VM ABI changes: a new default may be fine
for new VMs, but existing persisted configuration must continue to resolve
to the same guest physical addresses.
```

## Two pieces

Layout resolution is split into two pieces that you should think about
separately:

1. A **pure address-space allocator** in `vm_topology::layout`. It knows
   nothing about chipsets, firmware, VTLs, PCI, or the host. Callers describe
   what they need in terms of ranges, sizes, alignments, and a placement
   class, and the allocator returns deterministic guest physical addresses.
2. A **worker resolver** in `openvmm_core::worker::memory_layout`. This is
   where OpenVMM's policy lives: which platform ranges are pinned, what
   alignments NUMA nodes get, how PCIe ECAM is sized, and so on. The resolver
   describes the VM to the allocator, runs it, and builds the resulting
   [`MemoryLayout`](https://openvmm.dev/rustdoc/linux/vm_topology/memory/struct.MemoryLayout.html)
   that the rest of the VM worker uses to look up RAM, MMIO, PCI ECAM, and
   PCI MMIO ranges.

Keeping the allocator policy-free means its behavior can be exhaustively
tested in isolation, and the worker can be reasoned about as a list of
requests that fully describes the VM.

## The allocator

[`LayoutBuilder`](https://openvmm.dev/rustdoc/linux/vm_topology/layout/struct.LayoutBuilder.html)
accepts four kinds of input:

| Input | Purpose |
|---|---|
| `reserve(tag, range)` | Block allocation at this address but do not include it in the layout top. |
| `fixed(tag, range)` | A range whose address is already decided. Blocks allocation and counts as part of the layout. |
| `ram(tag, target, size, alignment)` | Ordinary guest RAM. The only request type that may be split across multiple extents. |
| `request(tag, target, size, alignment, placement)` | A single contiguous range, placed dynamically. The `placement` chooses one of three phases below. |

`reserve` and `fixed` differ only in how they affect the **layout top** —
the address one past the highest guest-visible byte. `fixed` ranges raise
it; `reserve` ranges do not. This matters because the layout top determines
where post-MMIO requests (such as paravisor private memory) start: a
reserved hole high up in the address space should not push them even
higher.

When `allocate()` runs, it processes requests in a fixed phase order. Each
phase pulls from whatever address space the earlier phases left free:

1. **Reserved ranges** are removed from the free space.
2. **Fixed ranges** are removed from the free space.
3. **`Placement::Mmio32`** requests are packed *top down* below 4 GiB, so
   RAM can start at GPA 0 and grow upward through the lowest free space.
4. **RAM** requests are placed *bottom up* from GPA 0, splitting around any
   holes left by the earlier phases. RAM is the only splittable kind.
5. **`Placement::Mmio64`** requests are packed *bottom up* starting at the
   end of RAM. This makes the layout top a function of requested topology
   rather than a precomputed high MMIO bucket size.
6. **`Placement::PostMmio`** requests are placed *after* everything else
   (excluding reserved ranges from the "everything else"). They are for
   ranges that should not affect the guest-visible top of memory.

Within `Mmio32` and `Mmio64`, requests are sorted by alignment (largest
first), then size (largest first), then caller order. This keeps large,
strictly-aligned device windows from being fragmented by small devices.
RAM and `PostMmio` use caller order verbatim: RAM order is the NUMA vnode
assignment, and `PostMmio` carries policy that should not be reordered by
alignment.

```admonish note
The allocator does not take host physical-address width as an input. The
layout is computed as a pure function of VM configuration; the worker
checks the resulting layout top against host capabilities after the fact.
This keeps guest physical addresses from shifting when the same VM moves
to a host with a different physical-address width.
```

## Worker policy

The worker resolver in
[`openvmm_core::worker::memory_layout`](https://github.com/microsoft/openvmm/blob/main/openvmm/openvmm_core/src/worker/memory_layout.rs)
issues requests in this order:

1. **Architectural reserved zone.** A `reserve` request for the
   per-architecture range containing LAPIC, IOAPIC, GIC, PL011, battery,
   TPM, and similar fixed-address platform devices.

    | Architecture | Range |
    |---|---|
    | x86_64 | `0xFE00_0000..0x1_0000_0000` |
    | aarch64 | `0xEF00_0000..0x1_0000_0000` |

2. **Chipset low MMIO** (`Mmio32`) — the VMOD/PCI0 `_CRS` low range for
   VMBus relay devices and PIIX4 PCI BARs. 2 MB alignment.
3. **Chipset high MMIO** (`Mmio64`) — the corresponding high range. 2 MB
   alignment.
4. **PCIe root complex ranges**, one per root complex:
    - **ECAM** (`Mmio32`). The size is derived from the bus window as
      `(end_bus - start_bus + 1) * 1 MB` (32 devices × 8 functions ×
      4 KiB per config space).
    - **Low MMIO** (`Mmio32`), 2 MB aligned. A caller can pin this to a
      fixed range instead of supplying a size, for assigned-device, IOMMU,
      and physical-topology passthrough.
    - **High MMIO** (`Mmio64`), 1 GB aligned. A caller can pin this to a
      fixed range as well. Per-BAR alignment would guarantee the entire
      window is usable for one large BAR, but burns address space on
      hosts with tight physical-address widths.
5. **Virtio-mmio slots** (`Mmio32`) — one contiguous region sized
   `slot_count * 4 KiB`, when any slots are configured.
6. **RAM**, in vnode order. The first request becomes vnode 0, the second
   vnode 1, and so on. Alignment depends on request size:

    | RAM request size | Alignment |
    |---|---|
    | < 1 GB | 2 MB |
    | ≥ 1 GB | 1 GB |

    Sub-GB nodes use 2 MB so small NUMA nodes do not waste a full GB of
    address space.
7. **VTL2 chipset MMIO** (`PostMmio`) — VTL2's own VMBus / chipset MMIO
   region, when VTL2 is configured. Placed after VTL0 so enabling VTL2
   does not move any VTL0 address.
8. **VTL2 private memory** (`PostMmio`) — when the IGVM file requests
   layout-mode VTL2 memory, the worker takes only its size and alignment
   from the IGVM relocation header. The IGVM file's relocation min/max
   bounds are not fed in as constraints here; they are validated later by
   the IGVM loader against the selected base. Treating them as constraints
   here would over-constrain layout and could put holes in VTL0 just to
   accommodate an IGVM file we will reject anyway.

After `allocate()` succeeds, the worker collects the resolved ranges into
the `MemoryLayout`'s MMIO, PCI ECAM, and PCI MMIO gap vectors, then checks
`MemoryLayout::end_of_layout()` against the host's physical-address width.

## RAM splitting

RAM is the only splittable request, and the splitter has one rule worth
calling out: the alignment passed in is also the **split granularity**.
When a single free range cannot hold the entire request, the part placed
in that range is rounded down to the request alignment before continuing.

The practical effect is that 1 GB-aligned RAM stays in 1 GB-aligned
chunks. A small fixed hole just above the 1 GB boundary will not cause a
"nearly 1 GB" RAM extent to be placed in the interrupted range; instead,
RAM resumes at the next 1 GB boundary.

## Examples

These examples use compact synthetic configurations. Each one is covered
by tests in `vm_topology::layout` or `openvmm_core::worker::memory_layout`.

### A fixed MMIO range splits RAM

4 GB of RAM with a 1 GB fixed MMIO range from 1 GB to 2 GB:

| Input | Range |
|---|---|
| RAM request | 4 GB |
| Fixed MMIO | `0x4000_0000..0x8000_0000` |

| Output | Range |
|---|---|
| RAM | `0x0000_0000..0x4000_0000` |
| MMIO | `0x4000_0000..0x8000_0000` |
| RAM | `0x8000_0000..0x1_4000_0000` |

Total RAM is still 4 GB — the fixed range is occupied address space, not
RAM.

### GB-aligned RAM stays GB-aligned

2 GB of RAM with a tiny fixed hole just above the 1 GB boundary should
not produce a sub-GB RAM fragment:

| Input | Range |
|---|---|
| RAM request | 2 GB, 1 GB alignment |
| Fixed MMIO | `0x4010_0000..0x4020_0000` |

| Output | Range |
|---|---|
| RAM | `0x0000_0000..0x4000_0000` |
| Fixed MMIO | `0x4010_0000..0x4020_0000` |
| RAM | `0x8000_0000..0xC000_0000` |

The splitter places one full 1 GB chunk, refuses to use the interrupted
sub-GB fragment, and resumes at the next 1 GB boundary.

### Small NUMA nodes use 2 MB alignment

Two 512 MB NUMA nodes:

| Input | Size |
|---|---|
| vnode 0 RAM | 512 MB |
| vnode 1 RAM | 512 MB |

| Output | Range |
|---|---|
| vnode 0 RAM | `0x0000_0000..0x2000_0000` |
| vnode 1 RAM | `0x2000_0000..0x4000_0000` |

With 1 GB alignment each node would burn a full GB of address space.
Request order is the vnode assignment, so swapping the requests swaps the
NUMA layout.

### VTL2 does not move VTL0

Starting from 2 GB of VTL0 RAM and a fixed 1 GB MMIO hole:

| VTL0 output | Range |
|---|---|
| RAM | `0x0000_0000..0x4000_0000` |
| MMIO | `0x4000_0000..0x8000_0000` |
| RAM | `0x8000_0000..0xC000_0000` |

Adding a 2 MB VTL2 private-memory request leaves the VTL0 layout
identical and places VTL2 after the VTL0-visible top:

| Private output | Range |
|---|---|
| VTL2 | `0xC000_0000..0xC020_0000` |

`MemoryLayout::end_of_layout()` reports the VTL0-visible top.
`MemoryLayout::vtl2_range()` reports the VTL2 range separately.

### Reserved holes do not raise the layout top

A reserved range blocks allocation but is not a guest-visible resource,
so it does not push later post-MMIO ranges higher:

| Input | Range |
|---|---|
| RAM request | 2 GB |
| Reserved hole | `0xFD_0000_0000..0xFD_4000_0000` |
| Post-MMIO request | 1 MB |

| Output | Range |
|---|---|
| RAM | `0x0000_0000..0x8000_0000` |
| Post-MMIO | `0x8000_0000..0x8010_0000` |

Trailing reserved ranges are omitted from the returned allocation list,
but a reserved range that sits between real allocations is reported so
callers can see the full occupied map.

## When to update this page

Update this page when any of these change:

- the allocator's phase order or any phase's placement direction
- the semantics of `reserve`, `fixed`, `ram`, or `request`
- the architectural reserved zones or their per-architecture addresses
- the worker's RAM alignment policy
- PCIe ECAM sizing or per-BAR alignment policy
- VTL2 chipset MMIO or VTL2 private-memory placement
- the host physical-address validation step
- `MemoryLayout::end_of_layout()` or `MemoryLayout::vtl2_range()` semantics
