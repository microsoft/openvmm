# NUMA Topology

OpenVMM supports virtual NUMA (vNUMA) topologies, allowing a VM to
expose multiple NUMA nodes to the guest. Each node has its own memory
backing, optional host NUMA binding, and VP (virtual processor)
assignment. The guest sees ACPI tables describing the topology, and
operating systems like Linux and Windows use this information for
NUMA-aware memory allocation and thread scheduling.

## How it works

Every VM has a NUMA topology — even a single-node VM (which is what
`--memory` creates). A multi-node topology is configured with the
`--numa` flag, one per node. Each node specifies:

- **Memory** — how much RAM and how it should be allocated (shared vs.
  private, hugepage settings, etc.)
- **VPs** — which virtual processors belong to this node
- **Host binding** — optionally, which host NUMA node to allocate
  memory from

### VP assignment

By default, VPs are distributed across nodes by round-robin over
sockets: `(vp_index / vps_per_socket) % num_nodes`. This matches
QEMU's default and works well for symmetric topologies.

For asymmetric cases — AMD sub-socket NUMA boundaries, ARM clusters,
or any layout where the socket-based formula is wrong — VPs can be
assigned explicitly per node using the `vps=` option.

Explicit VP lists must be disjoint (no VP in two nodes), complete
(every VP assigned to exactly one node), and in range.

### Inter-node distances

NUMA distances control the ACPI SLIT that the guest sees — an N×N
matrix describing the relative cost of accessing memory across nodes.
Distances range from 10 (local) to 255 (unreachable), and self-distance
must be 10.

When no distances are specified, OpenVMM uses defaults: 10 for local,
20 for cross-node. The SLIT is only generated when there is more than
one node or explicit distances are provided.

Each direction must be specified independently — distances are not
automatically made symmetric.

## Memory backing

Each NUMA node gets its own memory backing. This enables:

- **Per-node hugepage sizes** — node 0 can use 2 MB pages while
  node 1 uses 1 GB pages
- **Per-node allocation policies** — shared vs. private, prefetch vs.
  demand-paged, all independent per node
- **Clean host NUMA binding** — binding applies to the entire backing,
  not a sub-range

The memory layout allocator places each node's RAM in vnode order (see
[Memory Layout](./memory-layout.md)). Each node's RAM starts at or
above the highest address used by the previous node, so vnode ordering
equals address ordering.

## Host NUMA binding

Each node can optionally bind its memory allocation to a specific host
NUMA node. This is an *allocation* property — it controls where memory
comes from on the host, not how the guest topology maps to the host
topology.

Binding is strict: allocation fails if the requested host node is out
of memory. This matches the behavior of QEMU and Cloud Hypervisor.

VP thread affinity (pinning VP threads to specific host CPUs) is a
separate concern and is not part of the NUMA configuration.

## Guest-visible topology

How the guest sees the NUMA topology depends on the boot mode:

- **UEFI, PCAT, and Linux direct boot with ACPI** — ACPI tables: the
  **SRAT** maps each VP and memory range to a proximity domain (NUMA
  node), and the **SLIT** provides the inter-node distance matrix.
- **IGVM / OpenHCL** — devicetree: the host provides each CPU and
  memory node with a `numa-node-id` property (this is existing
  infrastructure, not new to the NUMA topology feature).
- **Linux direct boot with devicetree** — NUMA information is not yet
  included in the generated devicetree. Guests see a flat single-node
  topology regardless of the configured NUMA layout.

On Linux guests, the topology is visible via `numactl --hardware`.
On Windows guests, it appears in Task Manager and via the
`GetLogicalProcessorInformationEx` API.

## Device NUMA affinity

PCIe root complexes and VPCI devices can optionally be assigned to a
NUMA node so the guest OS sees correct device locality. When no node is
specified, the ACPI `_PXM` object is omitted and the VPCI NUMA flag is
not set — the guest treats the device as having no specific NUMA
affinity and uses its default (current-node) allocation policy.

For PCIe, each root complex can specify a `vnode` that is exposed via
the ACPI `_PXM` object on the host bridge. Linux reads this to populate
`/sys/bus/pci/devices/<BDF>/numa_node` for all devices under that root
complex. Use `node=N` on `--pcie-root-complex`:

```bash
# Root complex on NUMA node 1
openvmm --numa size=2G --numa size=2G \
        --pcie-root-complex rc0,node=1 \
        --pcie-root-port rc0:rp0 ...
```

For VPCI devices, the NUMA node can be set in the config (`vnode` field
on `VpciDeviceConfig`) and is reported to the guest via the VPCI
protocol's `BusRelations2` message. There is no CLI flag for VPCI
device affinity yet — it is set programmatically.

## CLI usage

The `--numa` flag replaces `--memory` for multi-node VMs. It is
repeatable — one `--numa` per node:

```bash
# Two nodes, 2 GB each, default VP assignment (round-robin)
openvmm --numa size=2G --numa size=2G ...

# Two nodes bound to host NUMA nodes
openvmm --numa size=2G,host_numa_node=0 \
        --numa size=2G,host_numa_node=1 ...

# Per-node hugepages with explicit VP assignment
openvmm --numa size=2G,hugepages=on,vps=[0,1] \
        --numa size=2G,vps=[2,3] ...

# Custom inter-node distances
openvmm --numa size=2G --numa size=2G \
        --numa-distance 0:1:30 --numa-distance 1:0:30 ...
```

`--numa` and `--memory` are mutually exclusive. All `--memory` options
except `file` (shared, prefetch, thp, hugepages, hugepage_size) are
available as per-node options in `--numa`.

See [CLI](../../openvmm/management/cli.md) for the full option
reference.

## Limitations

- **Snapshot/restore** with multi-node topologies is not yet supported.
- **Linux direct boot with devicetree** (aarch64) does not include
  NUMA information in the generated devicetree. The guest sees a flat
  single-node topology regardless of the configured NUMA layout.
