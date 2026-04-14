---
name: vmm-tests
description: "Run VMM tests locally with cargo xflowey vmm-tests-run. Load when writing, running, or debugging VMM tests, or when you need to understand the petri test framework, artifact handling, or cross-compilation for VMM tests."
---

# Running VMM Tests

VMM tests boot full virtual machines and validate behavior. They live in
`vmm_tests/vmm_tests/tests/` and use the `petri` test framework.

**Always use `cargo xflowey vmm-tests-run`** — never raw `cargo nextest run -p
vmm_tests`. The xflowey command handles artifact discovery, dependency
building, and test execution automatically.

## Quick Start

```bash
# Run a specific test
cargo xflowey vmm-tests-run --filter "test(my_test_name)" --dir /tmp/vmm-tests-run

# Run all tests matching a prefix
cargo xflowey vmm-tests-run --filter "test(/^boot_/)" --dir /tmp/vmm-tests-run

# Run all tests (rarely needed locally)
cargo xflowey vmm-tests-run --filter "all()" --dir /tmp/vmm-tests-run
```

The `--dir` flag is **required** and specifies where build artifacts go.

## Filter Syntax

Filters use [nextest filter expressions](https://nexte.st/docs/filtersets/):

| Expression | Matches |
|-----------|---------|
| `test(foo)` | Tests with `foo` in the name |
| `test(/^boot_/)` | Tests starting with `boot_` (regex) |
| `test(foo) & !test(hyperv)` | `foo` tests excluding Hyper-V variants |
| `all()` | Everything |

## Platform Targeting

By default, tests build for the current host. Use `--target` for
cross-compilation:

```bash
# Cross-compile and run Windows tests from WSL2
cargo xflowey vmm-tests-run --target windows-x64 --dir /mnt/d/vmm_tests
```

| Target | Description |
|--------|-------------|
| `windows-x64` | Windows x86_64 (Hyper-V / WHP) |
| `windows-aarch64` | Windows ARM64 (Hyper-V / WHP) |
| `linux-x64` | Linux x86_64 |

**Windows from WSL2**: The output directory **must** be on the Windows
filesystem (e.g., `/mnt/d/...`). Cross-compilation setup is required first —
see `Guide/src/dev_guide/getting_started/cross_compile.md`.

## Artifact Handling (Lazy Fetch)

By default, disk images (VHDs/ISOs) are streamed on demand via HTTP with local
SQLite caching. This avoids multi-GB upfront downloads.

- `--no-lazy-fetch` — download all images upfront instead of streaming
- Lazy fetch is automatically disabled for Hyper-V tests (they need local files)
- `--skip-vhd-prompt` — skip interactive VHD download prompts (useful for
  automation)

## Viewing Logs

```bash
# Show test output (petri logs, guest serial, etc.)
cargo xflowey vmm-tests-run --filter "test(foo)" --dir /tmp/vmm-tests-run -- --no-capture

# Full OpenVMM trace output
OPENVMM_LOG=trace cargo xflowey vmm-tests-run --filter "test(foo)" --dir /tmp/vmm-tests-run -- --no-capture
```

## Other Useful Flags

| Flag | Purpose |
|------|---------|
| `--release` | Release build (default: debug) |
| `--build-only` | Build without running |
| `--verbose` | Verbose cargo output |
| `--install-missing-deps` | Auto-install missing system dependencies |
| `--custom-uefi-firmware <PATH>` | Use a custom UEFI firmware (MSVM.fd) |
| `--custom-kernel <PATH>` | Use a custom kernel image |

Run `cargo xflowey vmm-tests-run --help` for the full option list.

## Writing VMM Tests

Tests use the `petri` framework with the `#[vmm_test]` macro for
parameterized test generation. Start by reading:

- Existing tests: `vmm_tests/vmm_tests/tests/tests/multiarch.rs`
- `petri` rustdoc: `cargo doc -p petri --open`

### Test weight annotations

Put these words in your test name to control resource allocation:

- `heavy` — test needs more resources (e.g., 16 VPs)
- `very_heavy` — even more resources (e.g., 32 VPs)

### Unstable tests

If a test isn't reliable enough to gate PRs, mark individual variants
or the whole test as `unstable`:

```rust,ignore
#[vmm_test(
    unstable_hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
)]
async fn my_test<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    // ...
}
```

Unstable tests run in CI but don't gate PRs. To promote to stable, remove
`unstable` from the macro — no CI config changes needed.

To treat unstable failures as errors locally:
`PETRI_REPORT_UNSTABLE_FAIL=1`

## Common Pitfalls

- **Don't use `cargo nextest run -p vmm_tests` directly** — artifacts won't
  be present and tests will fail with missing-artifact errors.
- **Windows output dir from WSL** — must be on `/mnt/c/` or `/mnt/d/`, not
  in the WSL filesystem.
- **Hyper-V tests** — require Hyper-V Administrators group membership and
  disable lazy fetch automatically.
- **CI failures** — use the `openvmm-ci-investigation` skill to diagnose
  failing VMM tests in CI, not this workflow.
