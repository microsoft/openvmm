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

To run explicit VM sizes:

```text
cargo xflowey vmm-tests-run \
  --filter "binary(vmm_perf) & test(fio)" \
  --vmm-perf-vmsizes 'CpuCount=2,MemoryMB=4096' \
  --vmm-perf-vmsizes 'CpuCount=8,MemoryMB=16384'
```

The Windows x64 implementation is compile-validated, including WHP host
validation and Windows profile/runtime paths, but its tests remain
unregistered until a Windows VMM.Perf runtime artifact is available.

## `xflowey` vs `xtask`

In a nutshell:

- `cargo xtask`: implements novel, standalone tools/utilities
- `cargo xflowey`: orchestrates invoking a sequence of tools/utilities, without
  doing any non-trivial data processing itself
