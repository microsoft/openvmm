# cargo xtask

`cargo xtask` runs standalone, repository-specific development utilities
implemented by the `xtask` binary.

## Invocation

Run commands from the repository root so task-relative paths resolve
consistently:

```bash
cargo xtask --help
```

## Command overview

| Command | Purpose |
| --- | --- |
| `clean` | Remove generated build-system, package, and test artifacts |
| `fmt` | Run repository formatting and structural validation passes |
| `fuzz` | Discover, build, execute, minimize, and inspect fuzz targets |
| `guest-test` | Create or download guest test images |
| `install-git-hooks` | Install repository-managed git hooks |
| `verify-size` | Compare ELF section sizes between two binaries |

Run `cargo xtask <COMMAND> --help` for the current command-specific interface.

## Cleaning generated state

The default clean removes directories that `cargo clean` does not own:

- `flowey-out`
- `flowey-persist`
- `.packages`
- `vmm_test_results`

Preview the removal first:

```bash
cargo xtask clean --dry-run
```

Add `--cargo` to run `cargo clean` and remove `target` as well.

```admonish warning
The default clean removes restored packages and test results in addition to
generated Flowey state. Use `--dry-run` when you only intend to reclaim Cargo
output or need to retain diagnostic artifacts.
```

## Repository formatting

`cargo xtask fmt` is the canonical repository formatter and validator. It runs
four pass families:

| Pass | Coverage |
| --- | --- |
| `rustfmt` | Rust formatting and generated formatting expectations |
| `lints` | Repository source, metadata, documentation, and policy lints |
| `verify-fuzzers` | Fuzz-target manifest and registration consistency |
| `verify-flowey` | Flowey pipeline and generated-definition consistency |

Run every pass and apply supported fixes:

```bash
cargo xtask fmt --fix
```

Run only selected passes against changed files:

```bash
cargo xtask fmt --only-diffed \
	--pass rustfmt --pass lints
```

Without `--fix`, independent passes run in parallel unless `--no-parallel` is
set. A failed pass prints the corresponding fixing command where applicable.

## Fuzzing commands

The `fuzz` command wraps `cargo-fuzz` with workspace target discovery and
OpenVMM-specific build configuration.

Start by listing registered targets:

```bash
cargo xtask fuzz list
```

Run a target or reproduce one artifact:

```bash
cargo xtask fuzz run fuzz_ide
cargo xtask fuzz run fuzz_ide path/to/crash-artifact
```

The fuzzing section documents prerequisites, corpora, minimization, and
coverage workflows in depth.

## Guest test images

`guest-test uefi` places a UEFI executable into a bootable raw image. For
example:

```bash
cargo xtask guest-test uefi \
	--bootx64 path/to/guest_test_uefi.efi
```

`guest-test download-image` obtains the repository's supported guest test
image. Use each subcommand's help for architecture and output options.

## Git hooks

Install the repository-managed hooks with:

```bash
cargo xtask install-git-hooks
```

The installed hook locates the built automation binary through
`target/xtask-path`, avoiding an unnecessary Cargo invocation when possible.
Hook behavior still comes from the current checkout, so rerun installation if
the hook setup changes.

Fix a hook-reported issue with the same `cargo xtask` command shown in its
output. Do not bypass a failing hook without understanding which required check
is being skipped.

## Binary size comparison

Compare section sizes of two ELF binaries:

```bash
cargo xtask verify-size \
	--original path/to/original \
	--new path/to/new
```

The command reports changed sections, net size difference, and total absolute
section difference. It fails when the total difference exceeds its current
50-KiB threshold. This is a coarse guard for unexpected layout changes, not a
replacement for scenario-specific image size limits.

## `xtask` versus `xflowey`

Use `xtask` for a tool that performs project-specific data processing or a
focused repository operation. Use `xflowey` to orchestrate a dependency graph
of builds, downloads, and existing tools across platforms.

Examples:

- Formatting Rust and validating fuzzer manifests belong in `xtask`.
- Building OpenHCL from many components and resolving VMM-test artifacts belong
	in `xflowey`.

## Troubleshooting

- Run from the repository root unless using `--custom-root`.
- If an automation change is not reflected, allow Cargo to rebuild the `light`
	profile instead of invoking a stale path directly.
- On Windows, locked files can prevent cleanup; stop VMs, debuggers, and helper
	processes before retrying.
- Use `--no-parallel` when interleaved formatter output obscures the failing
	pass.
- A missing `cargo-fuzz` or target toolchain is a fuzzing prerequisite error,
	not an xtask registration failure.

The command follows the general
[`cargo-xtask`](https://github.com/matklad/cargo-xtask) convention.
