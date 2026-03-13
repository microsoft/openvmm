# IGVM Reproducibility Investigation

## The Diff Tool

`igvmfilegen diff` decomposes two IGVM files into their logical parts (using the
`.bin.map` file to name regions by component) and runs
[diffoscope](https://diffoscope.org/) on the extracted directory trees.

### Prerequisites

```
pip install diffoscope
```

### Usage

```bash
cargo run -p igvmfilegen -- diff \
  --left <left.bin> \
  --right <right.bin> \
  --left-map <left.bin.map> \
  --right-map <right.bin.map> \
  [--keep-extracted] \
  [-- <diffoscope args...>]
```

- `--left` / `--right`: The two IGVM files to compare.
- `--left-map` / `--right-map`: The `.bin.map` files produced alongside each
  IGVM file. Used to split page data into named components
  (`underhill-kernel`, `underhill-initrd`, etc.) so diffoscope can detect their
  file formats and recurse into them.
- `--keep-extracted`: Don't delete temp dirs after diffoscope exits. Prints
  their paths to stderr for manual inspection.
- Trailing args after `--` are forwarded to diffoscope (e.g. `--html report.html`,
  `--text -`, `--max-text-report-size 0`).

### Smoke test (identical files should produce no diff)

```bash
cargo run -p igvmfilegen -- diff \
  --left out/openhcl.bin --right out/openhcl.bin \
  --left-map out/openhcl.bin.map --right-map out/openhcl.bin.map
# Expected output: "No differences found."
```

### Using with local builds

```bash
cargo xflowey build-igvm x64
cargo xflowey build-igvm x64-cvm

cargo run -p igvmfilegen -- diff \
  --left flowey-out/artifacts/build-igvm/debug/x64/openhcl-x64.bin \
  --right flowey-out/artifacts/build-igvm/debug/x64-cvm/openhcl-x64-cvm.bin \
  --left-map flowey-out/artifacts/build-igvm/debug/x64/openhcl-x64.bin.map \
  --right-map flowey-out/artifacts/build-igvm/debug/x64-cvm/openhcl-x64-cvm.bin.map \
  --keep-extracted
```

### Using with CI artifacts

To investigate a failing `verify reproducible openhcl` job (e.g. run 22971331253):

```bash
# Download both builds and their extras (which contain .map files)
gh run download <run-id> --repo microsoft/openvmm \
  --name x64-cvm-reproducible-openhcl-igvm --dir /tmp/build-a
gh run download <run-id> --repo microsoft/openvmm \
  --name x64-cvm-local-reproducible-openhcl-igvm --dir /tmp/build-b
gh run download <run-id> --repo microsoft/openvmm \
  --name x64-cvm-reproducible-openhcl-igvm-extras --dir /tmp/extras-a
gh run download <run-id> --repo microsoft/openvmm \
  --name x64-cvm-local-reproducible-openhcl-igvm-extras --dir /tmp/extras-b

# Run the diff
cargo run -p igvmfilegen -- diff \
  --left /tmp/build-a/openhcl-cvm.bin \
  --right /tmp/build-b/openhcl-cvm.bin \
  --left-map /tmp/extras-a/openhcl-cvm/openhcl.bin.map \
  --right-map /tmp/extras-b/openhcl-cvm/openhcl.bin.map \
  --keep-extracted
```

### Extracted directory structure

Each IGVM file is extracted into:

```
<tempdir>/
  headers/
    platforms.txt              # Debug-formatted platform headers
    initializations.txt        # Debug-formatted initialization headers
  regions/
    underhill-kernel_0.bin     # Named by component from the .map file
    underhill-initrd.bin       # Detected as gzip by diffoscope
    underhill-boot-shim_0.bin  # Detected as ELF by diffoscope
    sidecar-kernel_0.bin       # Detected as ELF by diffoscope
    ...
  regions.txt                  # Text index: GPA range, page count, flags per region
  vp_context/
    snp_vp0.bin                # SNP VMSA as raw binary
    x64_vbs_Vtl2_vp0.txt      # VBS register list as formatted text
    ...
  parameter_areas/
    area_0000.bin              # ParameterArea initial_data by index
  metadata.txt                 # All non-PageData, non-VP-context directives
```

Pages at the same GPA with different compatibility masks (SNP/TDX/VBS) are
deduplicated since the data is identical.

## Findings from CI run 22971331253

Investigation of the failing `verify reproducible openhcl [x64-cvm-linux-nix]`
job.

### Summary

Only **2 regions** differ in the IGVM file:

| Region | Cause |
|--------|-------|
| `underhill-initrd` | Contains `openvmm_hcl` which has 24 non-deterministic bytes |
| `loader-imported-regions` | A hash/measurement that changes because the initrd changed |

### Root cause: 24 bytes in `openvmm_hcl`

The machine code (`.text`) is **byte-identical** between the two builds. The
entire non-determinism comes from two ELF metadata sections:

| Section | Bytes | What it is |
|---------|-------|------------|
| `.note.gnu.build-id` | 20 | Hash of the final binary content |
| `.gnu_debuglink` | 4 | CRC32 of the separate `.dbg` file |

### Causal chain

```
.debug_info / .debug_str are non-deterministic (~47K bytes differ in .dbg)
  -> .dbg file content differs
    -> .gnu_debuglink CRC32 differs (4 bytes in stripped binary)
      -> binary content differs
        -> .note.gnu.build-id differs (20 bytes, computed over binary content)
          -> initrd differs (contains the binary)
            -> loader-imported-regions measurement differs
              -> IGVM file differs
```

### The stripping step

The post-build stripping in `flowey_lib_hvlite/src/run_split_debug_info.rs` runs:

```
objcopy --only-keep-debug <bin> <output>.dbg
objcopy --strip-all --keep-section=.build_info --add-gnu-debuglink=<bin> <output>
```

`--strip-all` removes debug sections but `--add-gnu-debuglink` re-introduces
the non-deterministic CRC. `--strip-all` also does not remove
`.note.gnu.build-id`.

This stripping runs for **both debug and release builds**, so switching to
release mode alone does not fix the problem.

## Next steps

### Option A: Remove `--add-gnu-debuglink` from the IGVM binary

In `run_split_debug_info.rs`, stop adding the debuglink to the binary that goes
into the IGVM. Without the non-deterministic CRC, the rest of the binary is
identical, so the build-id would also become deterministic.

The `.dbg` file is still produced and shipped in the `-extras` artifact.
Debugging would require manually pointing gdb at it instead of automatic
discovery, which is acceptable since OpenHCL debugging already requires manual
setup.

This is the most surgical fix.

### Option B: Strip both sections

Add `--remove-section=.note.gnu.build-id --remove-section=.gnu_debuglink` to
the objcopy command. This is more aggressive but guarantees reproducibility
regardless of what happens in debug info generation.

### Option C: Fix debug info non-determinism at the source

Investigate why `.debug_info` and `.debug_str` differ between builds. This is
likely caused by:

- Non-deterministic hash seeds in rustc/LLVM
- Temporary file paths leaking into DWARF
- Compilation unit ordering differences

This is the "correct" fix but likely requires upstream rustc/LLVM changes and
is harder to investigate and land.

### Recommended path

Start with **Option A** (remove `--add-gnu-debuglink`) as it's a one-line
change that makes the IGVM binary deterministic without losing any shipped
artifacts. If the build-id alone still causes issues (it shouldn't, since
removing the CRC makes the binary content deterministic), fall back to
**Option B**.

Separately, **Option C** can be investigated as a long-term improvement to make
the `.dbg` files themselves reproducible.
