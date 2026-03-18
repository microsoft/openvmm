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

## Git Commit Rules

- **Never amend commits that have already been pushed.** Make new commits
  instead. PRs are squash-merged, so a clean history is unnecessary.
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

Run tests with cargo-nextest in the specific packages you are modifying:
```bash
cargo nextest run -p <package-name>
```

- **Unit tests** — spread throughout crates in `#[cfg(test)]` blocks.
  Should be fast, isolated, and not require root/administrator access.
  Add `use test_with_tracing::test;` in test modules so that `tracing`
  is initialized and traces appear in test output.
- **VMM tests** — integration tests in `vmm_tests/` using the petri
  framework (requires additional setup).
- Mark tests requiring special setup with `#[ignore]`.
- Update `Guide/` docs when adding features or changing behavior
  (see `.github/instructions/doc-code-sync.instructions.md` for the mapping)
- **CI failures** — to investigate failing CI checks on a PR, load the
  `openvmm-ci-investigation` skill.

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
