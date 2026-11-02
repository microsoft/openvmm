# OpenVMM Repository

OpenVMM is a modular, cross-platform Virtual Machine Monitor (VMM) written in Rust.
This repository is home to both OpenVMM and OpenHCL (a paravisor).
Documentation lives in `Guide/` and is published at https://openvmm.dev.

## Build & Setup

Restore required dependencies before building for the first time:
```bash
cargo xflowey restore-packages
```

The project supports cross-compilation for `x86_64` and `aarch64`.
OpenHCL can only be built from Linux (WSL2 counts).
For cross-compilation from WSL2 to Windows, see
`Guide/src/dev_guide/getting_started/cross_compile.md` and source
`. ./build_support/setup_windows_cross.sh`.

## Protocol Buffers Enums

For `.proto` changes (including gRPC/ttrpc), read and follow the
[Protocol Buffers enum conventions](instructions/protobuf.instructions.md).

## Git Commit Rules

- **Never amend commits that have already been pushed without express
  permission from the user.** Make new commits instead. PRs are
  squash-merged, so a clean history is unnecessary.
- Rebasing onto `main` to resolve conflicts is fine, but do not use
  `git commit --amend`, `git rebase -i`, or `git push --force` to clean
  up history on already-pushed commits.

## Pre-Commit Checklist (MANDATORY)

**You MUST run these commands before every `git commit` in this repo.
Do NOT commit without completing all three steps.**

1. `cargo clippy --all-targets -p <package-name>` — for each modified package.
2. `cargo doc --no-deps -p <package-name>` — for each modified package.
3. `cargo xtask fmt --fix` — fix formatting, headers, naming conventions.
   Run this **last** because fixing clippy/doc issues may introduce
   formatting changes that need to be cleaned up.

If `cargo xtask fmt --fix` still fails after auto-fixes, fix the remaining
reported issues manually and re-run until it succeeds. Do not run individual
`--pass` commands afterward.

## Trust Boundaries & Safety

Both OpenVMM and OpenHCL process data from untrusted sources. Code must
**never panic** on untrusted input.

- **OpenVMM does not trust the guest** — code must not panic on any guest input
- **OpenHCL does not trust the root** — code must not panic on any root input
- **OpenHCL does not trust the VTL0 guest** — the attack surface is subtle and needs human review

**Error handling across trust boundaries:**
- Use `thiserror` for typed error enums at library/API boundaries and
  protocol-facing code
- Use `anyhow` with `.context("...")` for application-level plumbing and
  context propagation
- Never `.unwrap()` or `.expect()` on data that crosses a trust boundary
- For protocol/hardware enums where unknown values must round-trip without
  panicking, use the `open_enum!` macro instead of a normal Rust `enum`
- Rate-limit trace events that can be triggered repeatedly by guest
  interactions — use `tracelimit::warn_ratelimited!` (or `error_ratelimited!`,
  `info_ratelimited!`) instead of bare `tracing::warn!` etc.

**Other safety rules:**
1. Avoid `unsafe` code
2. Avoid taking new external dependencies, especially those that significantly increase binary size
3. Several OpenHCL crates (e.g., `minimal_rt`, `openhcl_boot`, `sidecar`,
   `host_fdt_parser`) must support `no_std` builds. In particular,
   `minimal_rt` and `host_fdt_parser` are unconditionally `no_std`, and
   `openhcl_boot` and `sidecar` use `cfg_attr(minimal_rt, no_std, no_main)`.
   Do not introduce `std`-only dependencies or APIs into code that is
   compiled for the `minimal_rt` configuration in these crates
4. Prefer `assert!` over `debug_assert!` for internal invariants — the
   performance cost is negligible in nearly all code, and catching invariant
   violations in release builds is more valuable. The project follows a
   "fail fast" philosophy: crash immediately on broken invariants rather
   than letting the process continue in an undefined state where bugs are
   harder to diagnose. (This does not apply to untrusted input — use error
   handling at trust boundaries, not assertions.)

## Testing

Run tests with cargo-nextest using the `agent` profile, which suppresses
output for passing tests and only shows slow/failing tests:
```bash
cargo nextest run --profile agent -p <package-name>
```

**Do NOT pipe test output to `grep`, `tail`, or other filters.** The `agent`
profile already minimizes output. Piping hides failures and makes hangs
invisible.

For VMM test validation during development, use `cargo xflowey vmm-tests-run`:
```bash
cargo xflowey vmm-tests-run --filter "test(my_test_name)"
```
Do not pass `--dir` in most cases. When cross-compiling for Windows from WSL2,
`--dir` on the Windows filesystem (e.g., `--dir /mnt/d/vmm_tests`) is required
only when the selected tests use disk images that need a Windows filesystem
(Hyper-V tests, or VHDX / dynamic VHD1 images); `vmm-tests-run` detects this and
errors with a clear message when it's needed. For native host tests, and for
Windows-from-WSL2 tests that stream disks or use fixed VHD1 / VMGS / ISO images,
it is unnecessary.

This automatically discovers artifacts, builds dependencies, and runs tests.
See `Guide/src/dev_guide/tests/vmm.md` for details.

- **Unit tests** — spread throughout crates in `#[cfg(test)]` blocks.
  Should be fast, isolated, and not require root/administrator access.
  Add `use test_with_tracing::test;` in test modules so that `tracing`
  is initialized and traces appear in test output.
- **VMM tests** — integration tests in `vmm_tests/` using the petri
  framework (requires additional setup).
- Mark tests requiring special setup with `#[ignore]`.
- Assess documentation impact using
  `.github/instructions/doc-code-sync.instructions.md`; a Guide edit is not
  always needed.
- **CI failures** — to investigate failing CI checks on a PR, load the
  `openvmm-ci-investigation` skill.

### Unit-test coverage: evidence, not a goal

When changing unit-testable Rust behavior or assessing its tests on Linux x64
or ARM64, use focused local coverage to check which production paths the
selected tests exercise. Setup, scope, and diagnostics are documented in
`Guide/src/dev_guide/tests/unit.md#local-coverage`.

```bash
cargo xflowey unit-test-coverage -p <package> --base <review-base> --install-missing-deps
python3 repo_support/unit_test_coverage.py query --changed
```

Choose affected packages explicitly and the base appropriate to the review.
For tasks that seek existing test gaps, collect those packages and use
`query --gaps`; its ranking measures missing-line count, not behavioral risk.
Use file/range queries to inspect covering tests and prioritize important
requirements, regressions, boundaries, and realistic error paths.

- **Test behavior, not execution counts.** Add meaningful assertions about
  observable results or invariants. A bug-fix regression should detect the
  original bug where feasible. Executing a line does not prove its behavior
  was checked; line coverage is not branch coverage.
- **Do not optimize for 100%.** There is no percentage target or coverage
  gate. Do not add tests that merely execute lines, weaken assertions, hide
  code through exclusions or configuration changes, or alter sound production
  code solely to improve the score. Do not expose internals, remove defensive
  checks, or add artificial production seams merely to manufacture coverage.
  Legitimate refactoring must have an independent design or correctness reason.
- **Accept and explain useful limits.** Leave justified gaps in defensive,
  platform-specific, or VM-dependent paths rather than creating unnatural
  tests. Stop when the relevant behavioral risks are adequately tested, not
  when the counters reach a target. Coverage supplements, not replaces,
  appropriate integration/VM tests, review, and reasoning.
- **Report the actual evidence.** State package/filter and target scope,
  collection completeness, test outcome, and remaining production gaps.
  Passing-attempt evidence is distinct from failed-test execution; unmeasured
  or unclassified code is not proven covered or uncovered. Do not narrow the
  selection merely to present a better score or claim a focused run covers
  other platforms or the workspace.
- **Keep results current.** Recollect after edits. `--allow-stale` is for
  labeled historical inspection, not validation of the current change.
  If collection is unavailable or unrepresentative, report that limitation
  and use suitable tests instead of claiming coverage.

## Rust Edition

This project uses the **Rust 2024 edition** (`edition = "2024"` in root
`Cargo.toml`).

## Common Pitfalls

- **`guest_arch` not `target_arch`**: For guest-architecture-specific code,
  use `cfg(guest_arch = "x86_64")` — **not** `cfg(target_arch = "x86_64")`.
  The VMM can run guests of a different architecture than the host. Using
  `target_arch` will fail CI.
- **Workspace dependencies**: All dependency versions are centralized in
  the root `Cargo.toml`. In crate `Cargo.toml` files, use
  `dep_name.workspace = true` — not inline version specifiers. Add new
  dependencies to `[workspace.dependencies]` in the root first.
- **Pipeline YAMLs are auto-generated**: Files under `ci-flowey/` (e.g.,
  `openvmm-pr.yaml`) are generated by the flowey framework — do not
  hand-edit them. Run `cargo xflowey regen` to regenerate.
- **flowey nodes**: Use `flowey::shell_cmd!` and `rt.sh` inside flowey
  nodes — not `xshell::cmd!` or `xshell::Shell::new`.

## Autonomous Agent Inner Loop

When running as a coding agent (GitHub Copilot coding agent or similar),
follow this validation loop **before pushing each commit**. This covers
the common early CI failures (including the fmt + clippy checks from job0)
locally, avoiding slow push-and-wait cycles.

1. **Identify modified packages.** For each file you changed, find the
   crate's `Cargo.toml` and note the package name.
2. **Check compilation:** `cargo check -p <package>` — fast type-check.
3. **Clippy:** `cargo clippy --all-targets -p <package>` — lint.
4. **Doc:** `cargo doc --no-deps -p <package>` — catch doc errors.
5. **Unit tests:** `cargo nextest run --profile agent -p <package>` — run the crate's
   tests. If nextest is not installed, use `cargo test -p <package>`.
6. **Formatting:** `cargo xtask fmt --fix` — run last, since earlier
   fixes may introduce formatting changes.

If any step fails, fix the issue and re-run from that step. Do not push
until all six steps pass.

**Cost notes:** Steps 2–5 are scoped to the modified package (`-p`),
so they are fast even in this large workspace. Step 6 runs workspace-wide
but is also fast. The full cycle typically takes under 2 minutes for
a single-crate change.
