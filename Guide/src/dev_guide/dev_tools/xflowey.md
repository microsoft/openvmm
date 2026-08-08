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

Results are written under `test_results/vmm_perf__<profile>/`. The GitHub CI
pipeline runs these profiles only in the `x64-linux-amd-kvm` VMM-test job.

## `xflowey` vs `xtask`

In a nutshell:

- `cargo xtask`: implements novel, standalone tools/utilities
- `cargo xflowey`: orchestrates invoking a sequence of tools/utilities, without
  doing any non-trivial data processing itself
