# cargo xflowey

`cargo xflowey` runs OpenVMM's local Flowey pipelines for builds, dependency
restoration, test execution, and generated CI workflows.

## Pipeline overview

Important local pipelines include:

| Pipeline | Purpose |
| --- | --- |
| `restore-packages` | Download and unpack external build dependencies |
| `build-igvm` | Build a supported or customized OpenHCL IGVM image |
| `vmm-tests-run` | Discover artifacts, build them, and run selected VMM tests |
| `regen` | Regenerate checked-in CI definitions from Flowey pipelines |

List the current surface with:

```bash
cargo xflowey --help
```

## How a pipeline runs

Flowey separates planning from execution. A pipeline declares jobs and their
node dependencies, and requests typed variables or artifacts from those nodes.
The runtime resolves the resulting graph, schedules ready steps, and records
enough state to avoid repeating valid work.

Local runs commonly write to:

- `flowey-out` for requested artifacts and user-facing output.
- `flowey-persist` for cached node state and downloaded inputs.
- `target` for Cargo build products and test-specific scratch roots.

## Restoring packages

Restore external dependencies needed by common builds:

```bash
cargo xflowey restore-packages
```

This can include firmware, kernels, sysroots, protocol compilers, and other
versioned packages. The exact set is selected from the build configuration and
host platform.

Some pipelines support `--install-missing-deps`, allowing Flowey to install
reported host prerequisites. Without it, the failure should explain the manual
installation needed.

## Building OpenHCL

Build a standard x64 image:

```bash
cargo xflowey build-igvm x64
```

Select release output or another supported recipe:

```bash
cargo xflowey build-igvm x64-cvm --release
```

The pipeline builds and stages `openvmm_hcl`, the boot loader, sidecar, kernel,
initrd, manifest resources, and final IGVM. Output is placed below
`flowey-out/artifacts/build-igvm` by recipe and build profile.

```admonish warning
The local `build-igvm` CLI is a developer interface and does not promise stable
automation syntax. Checked-in automation should depend on Flowey nodes and
typed artifacts rather than shelling out to this local command.
```

## Running VMM tests

Run one test or group with a nextest filter expression:

```bash
cargo xflowey vmm-tests-run --filter "test(my_test)"
```

The pipeline first enumerates matching tests and their static artifact
requirements. It then selects builds and downloads, initializes a test-content
directory, performs required image preparation, and invokes nextest with the
resolved environment.

Use `--build-only` to produce the selected artifacts without executing tests.
Cross-compilation and Incubator are selected through `--target` and related
options described in the VMM-test guide.

## Regenerating CI

Flowey pipeline definitions are the source of truth for generated CI YAML.
After changing those definitions, regenerate them with:

```bash
cargo xflowey regen
```

Do not hand-edit generated files under `ci-flowey` or generated workflow paths.
The `verify-flowey` xtask pass checks whether regeneration would change them.

The same graph definitions used for local pipelines are adapted to CI backends.
CI adds pools, triggers, permissions, and published artifacts around the shared
nodes rather than maintaining an unrelated shell implementation.

## `xflowey` vs `xtask`

Use the ownership boundary:

- `cargo xtask`: implements novel, standalone tools/utilities
- `cargo xflowey`: orchestrates invoking a sequence of tools/utilities, without
  doing any non-trivial data processing itself

## Troubleshooting

- Read the first failed node's context rather than only the final pipeline
  summary; later requested artifacts often fail because of that dependency.
- Use the pipeline's `--help` to find supported verbosity and dependency
  installation options.
- A stale or corrupt cached download can be isolated by cleaning the specific
  state reported in `flowey-persist`; avoid deleting every package by default.
- When cross-compiling from WSL, source the Windows cross environment before
  invoking a Windows target.
- If generated CI verification fails, run `cargo xflowey regen` and review the
  generated diff rather than editing YAML manually.
