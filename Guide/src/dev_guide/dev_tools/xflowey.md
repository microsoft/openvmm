# cargo xflowey

To implement various developer workflows (both locally, as well as in CI), the
OpenVMM project relies on [`flowey`](./flowey/flowey.md): a custom, in-house Rust library/framework
for writing maintainable, cross-platform automation.

`cargo xflowey` is a cargo alias that makes it easy for developers to run
`flowey`-based pipelines locally.

Some particularly notable pipelines:

- `cargo xflowey build-igvm` - primarily dev-tool used to build OpenHCL IGVM files locally
- `cargo xflowey restore-packages` - restores external packages needed to compile and run OpenVMM / OpenHCL
- `cargo xflowey vmm-tests-run` - build and run VMM tests with automatic artifact discovery. Use `--filter "test(name)"` to run specific tests
- `cargo xflowey cca-tests` - build and run ARM64 CCA tests using software emulator

## VMM.Perf

Run all Linux x64 VMM.Perf profiles with:

```text
cargo xflowey vmm-tests-run --filter "binary(vmm_perf)"
```

To run one profile, add its test name:

```text
cargo xflowey vmm-tests-run --filter "binary(vmm_perf) & test(fio)"
```

Each profile runs the default VM matrix:

- 2 CPUs and 4096 MB
- 4 CPUs and 8192 MB
- 8 CPUs and 16384 MB

Replace that matrix locally with concise, repeatable parameter sets:

```text
cargo xflowey vmm-tests-run \
  --filter "binary(vmm_perf) & test(fio)" \
  --vmm-perf-vmsizes 'CpuCount=2,MemoryMB=4096' \
  --vmm-perf-vmsizes 'CpuCount=8,MemoryMB=16384'
```

Each `--vmm-perf-vmsizes` occurrence creates one run and accepts comma-separated
`KEY=VALUE` parameters. Runs with both `CpuCount` and `MemoryMB` use descriptive
output directories such as `cpu-2-memory-4096mb`. To keep the existing matrix
and apply an override to every run, use repeatable `--vmm-perf-parameter`
options:

```text
--vmm-perf-parameter 'WorkDir=/mnt/perf-work' \
--vmm-perf-parameter 'HypervisorBackend=mshv'
```

Parameters may include any scalar VirtualClient profile parameter. Explicit
values override harness-generated defaults.

Before launching VirtualClient, each run validates its resolved `CpuCount`
against the host's available logical processors and `MemoryMB` against Linux
`MemAvailable`. The resolved `WorkDir` filesystem must also have at least 30
GiB available. An oversized configuration fails independently while the
remaining configurations continue. By default, temporary run data uses
`std::env::temp_dir()`, which resolves to Flowey's test temp directory because
Flowey sets `TMPDIR`; an explicit `WorkDir` parameter overrides the profile's
work output location.

Results are isolated under
`test_results/vmm_perf__<profile>/<configuration>/`. All configurations run
even if one fails, and the profile reports an aggregate failure afterward. The
GitHub PR pipeline runs these profiles in the Linux x64 KVM and MSHV VMM-test
jobs; the harness selects the backend from `/dev/kvm` or `/dev/mshv`.

The Linux ARM64 runtime is available to artifact discovery and local tooling,
but performance CI requires a native ARM64 runner and ARM64-compatible
firmware, guest image, and profile inputs. The emulated ARM64 TCG job is not
used for performance results.

## `xflowey` vs `xtask`

In a nutshell:

- `cargo xtask`: implements novel, standalone tools/utilities
- `cargo xflowey`: orchestrates invoking a sequence of tools/utilities, without
  doing any non-trivial data processing itself
