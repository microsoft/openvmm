---
name: vmm-tests
description: "Run VMM tests locally with cargo xflowey vmm-tests-run. Load when running or debugging VMM tests, or when you need to understand the petri test framework, artifact handling, or cross-compilation for VMM tests."
---

# Running VMM Tests

VMM tests boot full virtual machines and validate behavior. They live in
`vmm_tests/vmm_tests/tests/tests/` and use the `petri` test framework.

**Always use `cargo xflowey vmm-tests-run`** — never raw `cargo nextest run -p
vmm_tests`. The xflowey command handles artifact discovery, dependency
building, and test execution automatically.

## Quick Start

```bash
# Run a specific test
cargo xflowey vmm-tests-run --filter "test(my_test_name)"

# Run all tests matching a prefix
cargo xflowey vmm-tests-run --filter "test(/^boot_/)"

# Run all tests (rarely needed locally)
cargo xflowey vmm-tests-run --filter "all()"
```

Do not pass `--dir` — it defaults to `target/vmm_tests` and is only required
when cross-compiling for Windows from WSL2, where the output directory must be
on the Windows filesystem (e.g., `--dir /mnt/d/vmm_tests`).

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
filesystem (e.g., `/mnt/d/...`). `--dir` is required in this case.
Cross-compilation setup is required first — see
`Guide/src/dev_guide/getting_started/cross_compile.md`.

## Artifact Handling (Lazy Fetch)

By default, disk images (VHDs/ISOs) are streamed on demand via HTTP with local
SQLite caching. This avoids multi-GB upfront downloads.

- `--no-lazy-fetch` — download all images upfront instead of streaming
- Lazy fetch is automatically disabled for Hyper-V tests (they need local files)
- `--skip-vhd-prompt` — skip interactive VHD download prompts (useful for
  automation)

## Viewing Logs

Test output (petri logs, guest serial, etc.) is shown by default on failure.
For full OpenVMM tracing, set the `OPENVMM_LOG` environment variable:

```bash
OPENVMM_LOG=trace cargo xflowey vmm-tests-run --filter "test(foo)"
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

## Common Pitfalls

- **Don't use `cargo nextest run -p vmm_tests` directly** — artifacts won't
  be present and tests will fail with missing-artifact errors.
- **Windows output dir from WSL** — must be on `/mnt/c/` or `/mnt/d/`, not
  in the WSL filesystem.
- **Hyper-V tests** — require Hyper-V Administrators group membership and
  disable lazy fetch automatically.
- **CI failures** — use the `openvmm-ci-investigation` skill to diagnose
  failing VMM tests in CI, not this workflow.
