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
- `cargo xflowey vmm-perf` - build and run the standalone VMM.Perf profiles
- `cargo xflowey cca-tests` - build and run ARM64 CCA tests using software emulator

## VMM.Perf

Run all Linux x64 VMM.Perf profiles with:

```bash
cargo xflowey vmm-perf
```

By default, scratch files are created under `target/vmm_perf/temp` and retained
results are written to `target/vmm_perf/results`. Use `--dir` to select a
different root directory with the same `temp` and `results` layout.

To run one profile:

```bash
cargo xflowey vmm-perf --profile fio
```

To run explicit VM sizes:

```bash
cargo xflowey vmm-perf \
  --profile fio \
  --vmm-perf-vmsizes 'CpuCount=2,MemoryMB=4096' \
  --vmm-perf-vmsizes 'CpuCount=8,MemoryMB=16384'
```

Use `--target linux-x64-musl` when the host requires a statically linked
runner, such as the MSHV Azure Linux pool. The Windows x64 runner code remains
compile-validated, but the local xflowey command stays Linux-only until a
Windows VMM.Perf runtime package is available.

## `xflowey` vs `xtask`

In a nutshell:

- `cargo xtask`: implements novel, standalone tools/utilities
- `cargo xflowey`: orchestrates invoking a sequence of tools/utilities, without
  doing any non-trivial data processing itself
