# Reproducible Build Investigation

## Summary

Two CI builds of `openhcl-cvm.bin` (x64-cvm) produce different IGVM
files. One build uses the flowey node
(`build_openhcl_igvm_from_recipe_nix`) directly, the other uses
`cargo xflowey build-reproducible x64-cvm --release` via the
`test_reproducible_build` job. Both are intended to produce
byte-identical output.

## Root Cause

**The `openvmm_hcl` binary differs between the two builds.** Everything
else is identical or a downstream consequence of that binary difference.

### What matches

| Component | Status |
|---|---|
| `openhcl_boot` | Identical |
| `openhcl_boot.dbg` | Identical |
| `openhcl.bin.map` | Identical |
| Kernel (`underhill-kernel_*.bin`) | Identical |
| Boot shim (`underhill-boot-shim_*.bin`) | Identical |
| Kernel modules (`.ko` files in initrd) | Identical |
| IGVM structure (`regions.txt` layout) | Identical |
| All measured/private page variants (`_1`) | Identical (except `shim-params_1` — 5 bytes, and `loader-imported-regions_1` — 48 bytes) |

### What differs

| Component | Difference |
|---|---|
| `openvmm_hcl` | 1,533,141 bytes differ out of 15,536,928 (9.9%) — same size, different code |
| `openvmm_hcl.dbg` | Size differs by 48 bytes (184,218,880 vs 184,218,832) |
| `underhill-initrd_1.cpio.gz` | 6.3M bytes differ — caused by different `openvmm_hcl` inside |
| Shared page variants (`_0`) | Most differ, filled with ~99.5-99.8% non-zero random-looking data |

### Binary analysis of `openvmm_hcl`

Both are ELF 64-bit x86-64 shared objects, same total size. The
differences are spread across code and data segments:

| ELF Segment | Size | Differing Bytes | % |
|---|---|---|---|
| LOAD[0] (headers) | 22 KB | 0 | 0% |
| LOAD[1] (.text/.rodata) | 11.4 MB | 1,372,198 | 11.5% |
| LOAD[2] (.data) | 2.1 MB | 160,943 | 7.3% |
| LOAD[3] | 1.3 MB | 0 | 0% |
| DYNAMIC | 432 B | 0 | 0% |

11.5% of the text segment differing rules out simple metadata/path
embedding — this is a codegen-level difference.

### Shared page "garbage" data

Many IGVM regions appear in two variants: a `shared=true` copy and a
`shared=false` (measured) copy at different GPAs. The measured copies
are identical across builds and contain sparse, properly-zeroed data.
The shared copies are filled with random-looking bytes (~99.7%
non-zero) and differ between builds. Examples:

- `underhill-gdt_0` (shared): 4088/4096 bytes non-zero, differs
- `underhill-gdt_1` (measured): 8/4096 bytes non-zero, identical
- `underhill-device-tree.dtb` (shared): no valid DTB magic, all garbage

This suggests the shared page buffers are allocated from uninitialized
memory during IGVM generation. This is a secondary reproducibility
issue — even if `openvmm_hcl` were identical, these pages would still
differ.

## Investigation of initrd contents

The initrd archives contain identical file listings (29 entries) and
all extracted files are byte-identical — including kernel modules. The
archive-level difference is entirely caused by the `bin/openvmm_hcl`
entry having different content. The gzip headers are identical
(mtime=0, same flags), confirming the compression itself is
deterministic.

## Build pipeline comparison

Both CI paths ultimately call the same flowey node
(`build_openhcl_igvm_from_recipe_nix`) with the same parameters:

- `arch = X86_64`
- `kernel_kind = Cvm`
- `profile = OpenvmmHclShip`
- `custom_target = x86_64-unknown-linux-musl`

Both wrap cargo commands in `nix-shell --pure --run`. The
`test_reproducible_build` job compiles `flowey_hvlite` in debug mode
first, then runs `pipeline run build-reproducible --release`, which
internally creates a new pipeline with the NixShell command wrapper.

## Root cause: nondeterministic crate ordering in fat LTO

The binary difference is caused by **function reordering** in the
`.text` section, not different codegen. The two builds produce
byte-identical function bodies placed at different addresses.

### Evidence

The DWARF compilation unit list reveals exactly **2 ordering swaps**
between builds — both involving crates with multiple versions:

| Position | Build A (local-reproducible) | Build B (node-based) |
|---|---|---|
| 11–12 | `base64-0.22.1`, `base64-0.13.1` | `base64-0.13.1`, `base64-0.22.1` |
| 134–135 | `nix-0.31.2`, `nix-0.30.1` | `nix-0.30.1`, `nix-0.31.2` |

These swapped compilation units cascade through LLVM's fat LTO pass:
the merged LLVM IR module has functions in a different order, producing
a different function layout in a contiguous ~1.4 MB block (0x47000–
0x1a7000) where ~95% of bytes differ. The remaining sparse differences
are jump tables, vtables, and function pointers in `.rodata` and
`.data` that reference the reordered functions.

### Why the ordering differs

When cargo invokes rustc for the final LTO link, it passes all
dependency `.rlib` files. For crates with the **same package name but
different versions** (e.g., `base64` 0.13.1 and 0.22.1), the ordering
appears to depend on a nondeterministic factor — likely hash map
iteration order in cargo's dependency resolution or filesystem readdir
order in the target directory. The two CI jobs run on separate machines
with separate target directories, producing different orderings for
these same-name crate pairs.

All 297 compilation units are identical between builds; only these 2
pairs are swapped. The `.build_info` sections, CGU hashes, compiler
version (rustc 1.94.0), and `comp_dir` are all identical.

### Diff distribution

```
.text (0x47000–0x1a7000):  ~95% of bytes differ (function reordering)
.text (rest):              <1% sparse diffs (relocated call targets)
.rodata:                   11,897 bytes (jump tables + reordered strings)
.data:                     160,943 bytes (function pointers/vtables)
```

## Hypotheses ruled out

1. ~~**RUSTFLAGS / trim-paths**~~ — both paths get identical
   environment from `nix-shell --pure` with `shell.nix`. No embedded
   paths found in either binary.

2. ~~**Incremental compilation**~~ — both paths set
   `CARGO_INCREMENTAL=0`. The profile directories are separate
   (`target/debug/` vs `target/x86_64-unknown-linux-musl/underhill-ship/`).

3. ~~**`strip_debug_info` inconsistency**~~ — both paths use
   `no_split_dbg_info: true` and strip with
   `reproducible_without_debuglink: true` (Nix platform).

4. ~~**Environment variable leakage**~~ — both paths wrap cargo
   commands in `nix-shell --pure --run`. The DWARF metadata is
   identical.

5. ~~**Different codegen**~~ — function bodies are byte-identical
   (verified by comparing disassembly of the same function at
   different addresses). The only differences are relocated
   address operands.

## Duplicate crate version details

| Crate | Old version | Pulled in by | New version | Pulled in by |
|---|---|---|---|---|
| `base64` | 0.13.1 | `pbjson 0.5.1` | 0.22.1 | many crates |
| `nix` | 0.30.1 | 12 in-tree crates | 0.31.2 | `elfcore 2.0.1` only |

## Possible fixes

1. **Eliminate duplicate crate versions** (recommended short-term) —
   upgrade `pbjson` to drop `base64 0.13.1`, and unify on one `nix`
   version (likely upgrade the 12 in-tree crates to 0.31.x, or pin
   `elfcore` to 0.30.1). This sidesteps the ordering issue entirely.

2. **Sort `.rlib` inputs to rustc** (long-term) — ensure cargo passes
   dependency `.rlib` files in a deterministic order (alphabetical by
   full package-id including version). This would need a cargo patch
   or upstream fix. See https://github.com/rust-lang/cargo/issues/
   for tracking.

3. **LLVM sort-by-name** — pass `-sort-section=name` to the linker
   or use LLVM flags to enforce deterministic function ordering
   regardless of input module order. This is a workaround, not a fix.

## Secondary issue: uninitialized shared page buffers

Separate from the binary reproducibility issue, many IGVM shared page
regions (`_0` variants) are filled with uninitialized memory (~99.7%
non-zero, random-looking data). Even if `openvmm_hcl` were identical,
these pages would still differ. The IGVM generation should
zero-initialize page buffers.
