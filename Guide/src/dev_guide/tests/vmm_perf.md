# VMM.Perf Runner

`vmm_perf` uses Microsoft's [VirtualClient][] workload runner to execute
packaged performance profiles against an OpenVMM build and collect their
metrics and diagnostic files.

[VirtualClient]: https://github.com/microsoft/VirtualClient

## When to use VMM.Perf

Use VMM.Perf for the standardized `fio`, `iperf3`, and boot-time workloads
assembled by the performance pipeline. It is suitable for repeatable local or
CI runs where the runtime archive and profile definitions must remain pinned.

## Run through Flowey

The preferred command is:

```bash
cargo xflowey vmm-perf
```

Flowey builds OpenVMM and `vmm_perf`, restores UEFI firmware, downloads the
pinned runtime archive, chooses a native target, and supplies all low-level
paths.

Run one profile:

```bash
cargo xflowey vmm-perf --profile fio
```

The pipeline selects a GNU Linux x64 target on Linux and Windows x64 on
Windows. A static Linux runner can be selected with `--target linux-x64-musl`
when required by the host environment.

## VM sizing

The default configuration requests 16 virtual processors and 64 GiB of guest
memory. The runner validates requested CPU and memory against host capacity and
fails before starting a workload when the host is too small.

Select smaller or additional shapes through Flowey:

```bash
cargo xflowey vmm-perf \
  --profile fio \
  --vmm-perf-vmsizes 'CpuCount=2,MemoryMB=4096' \
  --vmm-perf-vmsizes 'CpuCount=8,MemoryMB=16384'
```

Each shape becomes a separate named configuration. Running multiple profiles
multiplies the number of VirtualClient executions by the number of shapes.

## Direct runner interface

Direct invocation is useful when a pipeline has already produced all inputs:

```bash
vmm_perf \
  --openvmm path/to/openvmm \
  --firmware path/to/MSVM.fd \
  --runtime-archive path/to/vmm-perf-runtime.tar.gz \
  --output-dir path/to/results \
  --profile fio
```

The required inputs are:

- An OpenVMM executable for the current host.
- Matching UEFI firmware.
- A supported VirtualClient runtime archive.
- A destination for retained diagnostics.

`--temp-dir` changes the scratch root. `--vm-sizes-json` accepts an array of
named or unnamed configuration objects, while `--parameters-json` applies
additional scalar VirtualClient parameters to every selected configuration.

For example:

```bash
vmm_perf <REQUIRED_PATH_OPTIONS> \
  --vm-sizes-json \
  '[{"name":"small","parameters":{"CpuCount":2,"MemoryMB":4096}}]'
```

Use raw parameter overrides only when you understand the selected VirtualClient
profile. They can change the workload as well as the VM shape.

## Execution lifecycle

For each profile and configuration, the runner:

1. Validates input files and host capacity.
2. Extracts or reuses a runtime cache adjacent to the archive.
3. Verifies that the platform-specific profile exists in that runtime.
4. Creates isolated temporary, work, and output directories.
5. Constructs a VirtualClient command with OpenVMM, firmware, shape, and
   metadata parameters.
6. Executes one VirtualClient iteration.
7. Restores file ownership where required and gathers diagnostics.

The runner continues through remaining profile/configuration pairs after an
individual failure, then returns one combined failure summary.

## Output layout

Flowey defaults to `target/vmm_perf`, with scratch files under `t` and retained
results under `results`. Each profile/configuration output can contain:

- `results/` for workload `metrics.csv` files.
- `openvmm-logs/` for collected VMM logs.
- `virtual-client/logs/` for `console.log`, `metrics.csv`, `vc.metrics`,
  `vc.traces`, and split metric files.
- `virtual-client/runtime/` for publishable files emitted by the runtime.

A successful VirtualClient process that does not produce `vc.metrics` is
treated as a failed run.

## Troubleshooting

- A capacity error should be fixed by selecting a smaller VM shape, not by
  bypassing the check.
- A missing profile usually means the runtime archive does not match the
  runner's OS, architecture, or expected package version.
- An archive error can indicate an unsupported extension or multiple runtime
  roots inside the archive.
- Read `virtual-client/logs/console.log` for workload startup failures and the
  collected OpenVMM logs for VM failures.
- On Linux, ownership restoration errors can follow a workload that created
  root-owned output; retain the first workload error as the primary signal.
