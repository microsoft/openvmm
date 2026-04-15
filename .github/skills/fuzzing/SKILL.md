---
name: fuzzing
description: "Run, optimize, and debug OpenVMM fuzzers. Covers cargo-fuzz targets, crash reproduction, lldb debugging, code coverage analysis, entropy optimization, and multi-target parallel campaigns."
---

# OpenVMM Fuzzer Guide

## Prerequisites

- **Linux only** — libfuzzer-sys doesn't support Windows.
- **Nightly toolchain**: `rustup toolchain install nightly`
- **cargo-fuzz**: `cargo install cargo-fuzz`
- **lldb** (for debugging): `sudo apt-get install -y lldb`
- **aarch64 RUSTFLAGS**: On aarch64, set `RUSTFLAGS="-Ctarget-feature=+lse,+neon"` or builds fail with atomics errors. Not needed on x86_64.

## Fuzz targets

Fuzz targets live in `<crate>/fuzz/` directories alongside the crate they test.
Each has a `Cargo.toml` with `cargo-fuzz = true` metadata and a `fuzz_<name>.rs`
binary. Find all targets:

```bash
cargo xtask fuzz list
```

## Running a fuzzer

```bash
# Continuous fuzzing (finds new crashes)
cargo +nightly xtask fuzz run fuzz_ide

# Reproduce a specific crash artifact
cargo +nightly xtask fuzz run fuzz_ide path/to/crash-artifact
```

The xtask wrapper sets `XTASK_FUZZ_REPRO=1` automatically when an artifact
path is provided, which enables `init_tracing_if_repro()` in the fuzz target.

## Build without running

```bash
cargo +nightly xtask fuzz build fuzz_ide
```

The binary lands at `target/<triple>/release/fuzz_ide` (e.g.,
`target/aarch64-unknown-linux-gnu/release/fuzz_ide` or
`target/x86_64-unknown-linux-gnu/release/fuzz_ide`).

## Reproducing a crash (direct binary)

Running the binary directly is faster for iteration than going through xtask:

```bash
# Without tracing (fast, quiet)
./target/<triple>/release/fuzz_ide path/to/crash-artifact

# With tracing (verbose — shows device state at each poll)
XTASK_FUZZ_REPRO=1 ./target/<triple>/release/fuzz_ide path/to/crash-artifact

# With backtrace
RUST_BACKTRACE=full ./target/<triple>/release/fuzz_ide path/to/crash-artifact
```

## Debugging with lldb

### Key lessons

1. **Use batch mode with a command file** — lldb's interactive prompt doesn't
   work well from automated terminals. Write commands to a file and use
   `lldb -b -s <file>`.
2. **Set `auto-confirm true`** — prevents lldb from blocking on "Do you really
   want to quit?" prompts.
3. **Breakpoint on `panic_fmt`** — `rust_begin_unwind` is often mangled and
   won't resolve. Use `-r panic_fmt` instead.
4. **Drop `XTASK_FUZZ_REPRO`** when using lldb — the tracing output from
   repeated polls produces megabytes of spam that buries the debugger output.
5. **Release builds have limited variable info** — `frame variable` may show
   nothing. For richer inspection, build with `--dev` (see below).

### Command file template

Create a file (e.g., `scratch/lldb_fuzz.cmd`):

```
settings set auto-confirm true
breakpoint set -r panic_fmt
run
bt 20
frame select 1
source list
frame select 2
source list
frame variable
quit
```

### Run it

```bash
lldb -b -s scratch/lldb_fuzz.cmd \
  -- ./target/<triple>/release/fuzz_ide \
  path/to/crash-artifact 2>&1 | tail -80
```

The `| tail -80` trims libfuzzer startup noise. Adjust as needed.

### Using rust-lldb

`rust-lldb` adds Rust pretty-printers for `Vec`, `String`, etc. It requires
lldb to be installed. Usage is the same but replace `lldb` with `rust-lldb`:

```bash
rust-lldb -b -s scratch/lldb_fuzz.cmd \
  -- ./target/<triple>/release/fuzz_ide \
  path/to/crash-artifact 2>&1 | tail -80
```

### Debug builds for variable inspection

Release builds have limited `frame variable` output. For full variable
inspection, build with `--dev`:

```bash
cargo +nightly xtask fuzz build fuzz_ide -- --dev
```

The debug binary lands at `target/<triple>/debug/fuzz_ide`. It's slower but
lldb can inspect all locals and struct fields.

### Navigating Rust wrapper types in lldb

Reaching the actual data through Arc, Mutex, UnsafeCell etc. requires
knowing the field path. Use `type lookup` to discover struct layouts:

```
(lldb) type lookup closeable_mutex::CloseableMutex<ide::IdeDevice>
```

Common wrapper-type traversal patterns:

| Rust type | lldb field path |
|-----------|----------------|
| `Arc<T>` | `var.ptr.pointer->data` (through `ArcInner`) |
| `CloseableMutex<T>` | `.value.value` (UnsafeCell inside Mutex) |
| `UnsafeCell<T>` | `.value` |
| `Option<T>` | `.$variants$.$variant$.value.__0` (check `$discr$` — 0 = None for most types) |
| Rust enum | `.$variants$.$variant$N.value.__0` (N = variant index) |

Full path example for IDE device state through `Arc<CloseableMutex<IdeDevice>>`:

```
(lldb) expr ide_device.ptr.pointer->data.value.value.channels[0].state
(lldb) expr ide_device.ptr.pointer->data.value.value.channels[0].enlightened_write
(lldb) expr ide_device.ptr.pointer->data.value.value.channels[0].bus_master_state
```

For drive registers inside `Option<DiskDrive>` containing an enum variant:

```
(lldb) expr ide_device.ptr.pointer->data.value.value.channels[0].drives[0].$variants$.$variant$.value.__0.$variants$.$variant$.value.__0.state.regs
```

### Setting breakpoint ignore counts

To skip the first N hits of a breakpoint (useful when a function is called
hundreds of times during polling):

```
(lldb) breakpoint set -f lib.rs -l 1253
(lldb) breakpoint modify -i 500 1
(lldb) run
```

This skips the first 500 hits, then stops on hit 501.

### Reading Rust bitfield status registers

Status register values appear as `(__0 = 'X')` where `'X'` is the char at
that byte value. Decode manually: `0x58` = bits 3,4,6 set = `drq`, `dsc`,
`drdy`.

## Minimizing crash inputs

```bash
cargo +nightly xtask fuzz tmin fuzz_ide path/to/crash-artifact
```

## Corpus management

```bash
# Minimize the corpus (remove redundant inputs)
cargo +nightly xtask fuzz cmin fuzz_ide
```

Corpus files live in `<crate>/fuzz/corpus/<target>/`.
Crash artifacts land in `<crate>/fuzz/artifacts/<target>/`.

## Fuzzer optimization principles

### Entropy efficiency
Every byte of fuzzer input should drive meaningful behavior. Common wastes:
- **Arbitrary setup parameters that don't affect coverage** — e.g., guest memory size, buffer counts. Use fixed values.
- **`u.ratio()` calls for branching** — consumes entropy for each decision. Use `Arbitrary` enums instead (1 byte per decision vs 8+ for ratio).
- **Redundant `arbitrary_data()` calls** — generate composite values with one call instead of field-by-field. E.g., a Guid from `[u8; 16]` instead of 11 separate calls.

### Simplification
- **Merge duplicated functions** that only differ in a small parameter. Use an enum variant to select behavior.
- **Remove dead protocol negotiation** — if a device rejects all interesting operations without init, always negotiate. Raw packet paths can still exercise init state machines through mutation.
- **Remove wrapper functions** that add no value over calling the underlying API directly.

### Coverage gaps
- **0-item/empty configurations** — fuzzers that always attach 1+ disks/devices miss the empty-controller code paths. Use `0..=N` ranges.
- **Hardcoded setup parameters** can hide code paths. E.g., fixed namespace IDs mean only one NVMe identify-namespace path is tested. Make setup params fuzzer-driven where they affect behavior.
- **TODOs mark legitimate gaps** — don't remove TODOs unless you've verified the marked code path is either (a) implemented or (b) genuinely doesn't affect coverage.

### Performance
- **Smaller backing stores** — RAM disks used for fuzzing don't need to be large. 32KB is sufficient for most device fuzzers (was 4MB).
- **Fixed guest memory** — varying guest memory size wastes entropy without affecting device logic.
- **Always negotiate protocols** — skipping negotiation wastes 100% of remaining fuzz actions on a single error branch.
- **`-fork=N`** for parallel fuzzing across CPUs. Allocate cores proportionally to fuzzer complexity.

## Multi-target parallel fuzzing

Run multiple fuzzers across available CPUs for extended campaigns:

```bash
# 6-hour campaign across 112 cores, 7 fuzzers
# Each runs with -fork=N, survives crashes/timeouts/OOMs
nohup bash -c 'RUSTFLAGS="-Ctarget-feature=+lse,+neon" \
  cargo +nightly xtask fuzz run fuzz_storvsp -- -- \
  -fork=20 -max_total_time=21600 \
  -ignore_crashes=1 -ignore_timeouts=1 -ignore_ooms=1 \
  -print_final_stats=1' > /tmp/fuzz_storvsp.log 2>&1 &
```

Allocate cores based on fuzzer complexity:
- Heavy fuzzers (storvsp, ide, nvme, nvram): 20 cores each
- Medium fuzzers (cmos_rtc): 15 cores
- Light fuzzers (diagnostics, scsi_buffers): 7–10 cores

Monitor progress: `tail -1 /tmp/fuzz_*.log`

After the run, collect coverage on each fuzzer for gap analysis.

## Code coverage

### Prerequisites

Everything from the main prerequisites, plus:

- **llvm-tools**: `rustup +nightly component add llvm-tools`
- **lcov** (for HTML reports): `sudo apt-get install -y lcov`

### Collecting coverage

The xtask has built-in coverage support. It runs each corpus entry through
a coverage-instrumented build and merges the results:

```bash
# Collect coverage data only
cargo +nightly xtask fuzz coverage fuzz_ide

# Collect + generate HTML report (requires lcov + genhtml)
cargo +nightly xtask fuzz coverage fuzz_ide --with-html-report

# Skip rebuild, just regenerate report from existing profdata
cargo +nightly xtask fuzz coverage fuzz_ide --with-html-report --only-report
```

On aarch64, set `RUSTFLAGS="-Ctarget-feature=+lse,+neon"` as usual.

Coverage artifacts land in:

| Artifact | Path |
|----------|------|
| profdata | `<crate>/fuzz/coverage/<target>/coverage.profdata` |
| HTML report | `target/<triple>/coverage/<triple>/release/lcov_html_<target>/index.html` |

### Filtering coverage to relevant crates

The raw HTML report covers *all* compiled code including dependencies — the
overall percentage is meaningless (typically 4–6%). What matters is coverage
of the **target crate itself** and its immediate domain dependencies.

Use `llvm-cov report` with source path filtering:

```bash
# Find the llvm-cov binary from nightly toolchain
LLVM_COV=$(find $(rustc +nightly --print sysroot) -name "llvm-cov" -type f | head -1)

# Full per-file report, excluding third-party code
$LLVM_COV report \
  -instr-profile <crate>/fuzz/coverage/<target>/coverage.profdata \
  -object target/<triple>/coverage/<triple>/release/<target> \
  --ignore-filename-regex='(rustc|\.cargo|registry)' 2>&1 \
  | grep 'vm/devices/storage/ide/src/'
```

For lcov-based per-file analysis, export to lcov and extract per-file stats:

```bash
$LLVM_COV export \
  -instr-profile <crate>/fuzz/coverage/<target>/coverage.profdata \
  -object target/<triple>/coverage/<triple>/release/<target> \
  --ignore-filename-regex='(rustc|\.cargo|registry)' \
  -format=lcov > /tmp/coverage.lcov

# Per-file function/line summary from lcov
awk '/SF:.*ide\/src\/lib\.rs$/,/end_of_record/' /tmp/coverage.lcov \
  | grep -E '^(FNF|FNH|LF|LH):'
# FNF/FNH = functions found/hit, LF/LH = lines found/hit

# Find uncovered line numbers
awk '/SF:.*ide\/src\/lib\.rs$/,/end_of_record/' /tmp/coverage.lcov \
  | grep '^DA:' | grep ',0$' | cut -d: -f2 | cut -d, -f1
```

## Coverage scope rules

Each fuzzer has a **primary scope** (the crate it exists to test) and
**secondary scope** (dependencies it exercises incidentally). Coverage
improvements should focus on primary scope first.

### What coverage matters per fuzzer

| Fuzzer | Primary scope | Secondary scope | Out of scope |
|--------|--------------|-----------------|--------------|
| fuzz_ide | `ide/src/` | `scsidisk/src/scsidvd/`, `scsidisk/src/atapi_scsi.rs`, `disk_backend/`, `disklayer_ram/` | `pci_core/`, `vmcore/`, `guestmem/` (infrastructure) |
| fuzz_storvsp | `storvsp/src/` | `scsidisk/src/lib.rs`, `vmbus_ring/`, `vmbus_async/`, `disk_backend/` | `vmbus_channel/`, `vmbus_core/` (infrastructure) |
| fuzz_nvme_driver | `nvme_driver/src/` | `nvme/src/`, `nvme_spec/`, `user_driver/` | `page_pool_alloc/`, `vmcore/` |
| fuzz_firmware_uefi_nvram | `firmware_uefi/src/service/nvram/` | `uefi_nvram_specvars/` | `crypto`, `openssl` (infrastructure) |
| fuzz_firmware_uefi_diagnostics | `firmware_uefi/src/service/diagnostics/` | — | `guestmem/` |
| fuzz_chipset_cmos_rtc | `chipset/src/cmos_rtc/` | `chipset_device_fuzz/` | `pal_async/`, `vmcore/` |
| fuzz_scsi_buffers | `scsi_buffers/src/` | — | `guestmem/` |
| fuzz_chipset_battery | `chipset/src/battery/` | `chipset_device_fuzz/` | — |

**Rules:**

1. **Primary scope is the goal.** Line coverage of the target crate is the
   metric to optimize. The fuzzer exists to find bugs in this code.

2. **Secondary scope matters when it's a trust boundary.** `scsidisk` parsing
   guest-supplied SCSI commands is a real attack surface — coverage there has
   value. But `disklayer_ram` just storing bytes is infrastructure.

3. **Out-of-scope code is noise.** `pci_core`, `vmcore`, `guestmem` get
   exercised incidentally. Don't optimize the fuzzer to improve their coverage
   — those crates deserve their own fuzzers if coverage matters.

4. **Save/restore and resolver code is structurally unreachable.** Functions
   like `save_restore.rs`, `resolver.rs` won't be hit by fuzzing because
   the fuzzer doesn't exercise serialization or resource resolution paths.
   Don't count them against coverage targets.

5. **Inspect (`InspectMut`) implementations are low-value.** These debug/
   diagnostics paths aren't part of the attack surface. Low coverage is
   acceptable.

## Coverage-guided fuzzer improvement process

### Step 1: Collect and filter

```bash
# Run coverage
cargo +nightly xtask fuzz coverage <target> --with-html-report

# Open the HTML report for visual inspection
# target/<triple>/coverage/<triple>/release/lcov_html_<target>/index.html
```

### Step 2: Identify gaps in primary scope

Look at the HTML report or extract uncovered lines:

```bash
awk '/SF:.*<crate>\/src\/lib\.rs$/,/end_of_record/' /tmp/coverage.lcov \
  | grep '^DA:' | grep ',0$' | cut -d: -f2 | cut -d, -f1
```

Read the source at those lines. Classify each uncovered region:

| Classification | Action |
|---------------|--------|
| **Reachable via fuzzer input** — the fuzzer *could* reach this code but hasn't found the right input sequence | Investigate why. Is the path gated by specific byte patterns the fuzzer struggles to generate? |
| **Structurally unreachable** — the fuzzer harness doesn't wire up the code path (e.g., save/restore, PCI config reads the harness skips) | Extend the harness: add new `FuzzAction` variants, expose more device interfaces to the fuzzer. |
| **Error-handling / defensive code** — `unreachable!()`, error paths for invalid hardware states | Low priority. These are worth testing but don't indicate a fuzzer deficiency. |
| **Inspect / debug only** — `InspectMut`, `Display`, `Debug` impls | Skip entirely. Not attack surface. |

### Step 3: Improve the fuzzer

Improvements fall into two categories, in priority order:

**A. Extend the fuzz harness**

If an entire code path is structurally unreachable, the harness needs changes.
Examples:

- fuzz_ide doesn't exercise PCI config space reads → add PCI config read/write
  to the `FuzzAction` enum in `chipset_device_fuzz`
- fuzz_storvsp doesn't test sub-channel operations → add corresponding VMBus
  channel actions

**B. Improve `Arbitrary` implementations**

If the fuzzer generates mostly one type of operation, check the `Arbitrary`
impl. Common issues:

- Uniform distribution across action types wastes entropy on less interesting
  paths. Weight toward rarer operations.
- Overly constrained generation (e.g., only valid SCSI commands) misses
  malformed-input bugs.
- Overly unconstrained generation (random bytes everywhere) wastes time on
  inputs that fail early validation and never reach deep code.

### Step 4: Measure improvement

After making changes:

1. Run the fuzzer for a fixed duration (e.g., 1 hour)
2. Minimize the corpus: `cargo +nightly xtask fuzz cmin <target>`
3. Collect coverage again
4. Compare line counts against prior coverage runs

Track coverage changes over time. Ratchet up: if a change reduces primary
scope coverage, investigate why.

### Step 5: Iterate

Coverage-guided improvement is iterative. Each cycle:

1. Collect → 2. Identify gaps → 3. Fix → 4. Fuzz → 5. Measure → repeat

Target **≥85% line coverage** in primary scope as a practical goal. The
remaining ~15% is typically error-handling code, debug impls, and paths
requiring specific hardware/timing conditions the fuzzer can't easily produce.
