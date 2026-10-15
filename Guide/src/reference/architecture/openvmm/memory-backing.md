# Memory Backing

Guest RAM is just a range of guest physical addresses (see
[Memory Layout](./memory-layout.md)); OpenVMM still has to decide what *host*
memory sits behind those addresses and how it is mapped. That decision — the
**memory backing** — affects startup time, runtime performance, which features
are available (snapshots, DMA passthrough, VTL2), and how much physical memory
the VM commits up front.

This page explains the backing modes and the tradeoffs between them. The
specific command-line syntax lives in the
[CLI reference](../../openvmm/management/cli.md); the `--memory` and `--numa`
options select a backing per VM (or per NUMA node) using the keys described
below. The builder API is
[`RamBackingRequest`](https://openvmm.dev/rustdoc/linux/membacking/struct.RamBackingRequest.html)
in the `membacking` crate.

## Shared vs. private memory

The most fundamental choice is whether guest RAM is **shared** or **private**.
The distinction is whether the host memory can be shared with other processes:
shared memory lives in an OS memory object (with a file descriptor or handle)
that can be mapped into more than one process, while private memory belongs to
the OpenVMM process alone and cannot be handed out. It is selected with
`shared=on|off` and defaults to `on`.

**Shared** memory is backed by such an object — a `memfd` on Linux, or a
pagefile-backed section on Windows. Because there is a real, shareable backing
object behind every guest page, shared memory is required for the features that
need to hand that object to something else:

- **Snapshots** — the backing file is what gets saved and restored (see
  [Snapshots](../../../user_guide/openvmm/snapshots.md)).
- **VTL2 / OpenHCL** and other consumers that mmap guest RAM out of process.

**Private** memory is ordinary anonymous memory (`MAP_ANONYMOUS` on Linux,
`VirtualAlloc` on Windows) with no backing object to share. It is lighter
weight but cannot be handed to another process. It can still be mapped into
in-process DMA targets by host virtual address, so assigned-device and IOMMU
DMA do not inherently require shared memory. Private memory is also
incompatible with x86 PCAT/legacy RAM splitting and with reusing an existing
backing.

```admonish tip
Use `shared=on` when you need a feature that requires a shareable backing
object — such as snapshots or a paravisor — and `shared=off` (private)
otherwise, for the lighter-weight anonymous backing.
```

## Prefetch

`prefetch=on|off` asks OpenVMM to commit guest RAM and program it into the
hypervisor's second-stage page tables (the SLAT) up front, instead of faulting
each page in lazily on first guest access.

This is a tradeoff. Prefetching forces the whole guest RAM range to be
allocated in advance and inserted into the SLAT before the guest runs, which
raises initial memory use and lengthens startup. In exchange, the guest does
not take a fault/exit the first time it touches each page. With prefetch off
(the default), startup is fast and only the memory the guest actually touches
is committed, but each first touch costs a fault.

Prefetch applies to both **shared** (file-backed) and **private** (anonymous)
guest RAM.

One thing limits where it has any effect: only the **WHP** (Windows) backend
implements it. On **KVM** and **mshv**, `prefetch=on` is a no-op — those
backends do not pre-populate their second-stage page tables or pre-fault the
host mapping, so guest RAM is always faulted in on demand.

## Huge pages

There are two independent mechanisms for backing guest RAM with pages larger
than 4 KiB: opportunistic Transparent Huge Pages and explicit huge-page
backing. They are quite different — one is an always-on, best-effort hint, the
other an opt-in, guaranteed reservation — so it is worth keeping them straight.

### Transparent Huge Pages (`thp=on`)

Transparent Huge Pages are a **Linux** feature that is **on by default**.
OpenVMM marks guest RAM as THP-eligible (via `madvise` with `MADV_HUGEPAGE`),
inviting the kernel to *opportunistically* back it with 2 MB pages. It is
best-effort: the kernel promotes pages when it can and silently falls back to
4 KB when it cannot, so nothing is pinned or guaranteed. Because it is only an
advisory hint, it applies to **both shared and private** guest RAM; pass
`thp=off` to opt out.

- Applies to both shared and private memory.
- Linux only; a no-op on other hosts.
- On by default; suppressed automatically for explicit `hugepages=on`
  backings, which are already huge.

### Explicit huge pages (`hugepages=on`)

`hugepages=on` requests **explicit, guaranteed** large-page backing from a
reserved pool. Unlike THP this is not best-effort — the allocation either gets
large pages or fails.

- Requires **shared** memory (`shared=on`); incompatible with private memory,
  file-backed memory, and x86 PCAT/legacy RAM splitting.
- Guest RAM size and each RAM range must be a multiple of the huge-page size.
- `hugepage_size=<SIZE>` overrides the default of 2 MB.

**On Linux**, this uses `hugetlb`-backed memory. The pages come from the
kernel's pre-reserved hugetlb pool, so that pool must be large enough for the
whole guest; otherwise allocation fails with a message telling you to grow the
pool or shrink the VM.

**On Windows**, this uses a large-page (`SEC_LARGE_PAGES`) section. Several
Windows-specific rules apply:

- The process must hold the **"Lock pages in memory"**
  (`SeLockMemoryPrivilege`) privilege.
- The whole guest RAM is committed and pinned up front. Allocation *fails*
  (rather than falling back to 4 KB) if enough contiguous physical memory is
  not available — so request it at startup.
- `hugepages=on` implies `prefetch=on`: Windows only installs 2 MB SLAT
  entries when the SLAT is populated in large batches, so the RAM is
  pre-populated up front to make the large-page backing actually yield 2 MB
  SLAT mappings.
- Only the 2 MB large-page size is supported (matching
  `GetLargePageMinimum()`); other sizes are rejected.

````admonish note title="Granting \"Lock pages in memory\" on Windows"
Grant `SeLockMemoryPrivilege` to the current user from an **elevated**
PowerShell prompt:

```powershell
.\scripts\grant-privilege.ps1
```

Sign out and back in for it to take effect, then verify with `whoami /priv`.
````

## File-backed RAM (`file=<PATH>`)

`file=<PATH>` backs guest RAM with an existing file rather than an anonymous
`memfd`. This is the mechanism behind [Snapshots](../../../user_guide/openvmm/snapshots.md):
the backing file persists guest memory to disk so it can be saved and
restored. It is a form of shared memory and cannot be combined with private
memory or explicit huge pages.

## Choosing a backing

| Goal | Backing |
|---|---|
| Default, general use | `shared=on` (file-backed shared memory) |
| Smallest footprint, no snapshots/out-of-process sharing | `shared=off` (private) |
| Snapshots, VTL2, out-of-process memory consumers | `shared=on` |
| In-process assigned-device/IOMMU DMA | Either backing mode |
| Save/restore to a specific file | `file=<PATH>` |
| Opportunistic 2 MB pages, Linux | on by default (`thp=off` to disable) |
| Guaranteed large pages, best TLB behavior | `hugepages=on` |
| Avoid first-touch faults (WHP only) | add `prefetch=on` |

## Compatibility summary

| Option | Requires | Platform | Notes |
|---|---|---|---|
| `shared=off` (private) | — | all | No snapshots/out-of-process sharing; not with PCAT legacy RAM |
| `prefetch=on` | — | WHP only | Commits + populates SLAT up front; no-op on KVM/mshv |
| `thp=on|off` | — | Linux | On by default; best-effort 2 MB pages; suppressed under `hugepages=on` |
| `hugepages=on` | `shared=on` | Linux, Windows | Guaranteed; size/range must be huge-page aligned |
| `hugepage_size=<SIZE>` | `hugepages=on` | Linux (any), Windows (2 MB only) | Default 2 MB |
| `file=<PATH>` | `shared=on` | all | Persistent backing file for snapshots |
