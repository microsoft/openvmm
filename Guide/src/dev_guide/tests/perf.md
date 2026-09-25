# Performance Tests (burette)

`burette` is a standalone binary that runs performance benchmarks
against OpenVMM using the `petri` test framework. It measures boot
time, memory overhead, and concurrent VM scaling behavior, producing
JSON reports that can be compared across builds.

Use Burette for developer-controlled experiments, report comparison, and
self-contained remote runs.

## Prerequisites

- Linux host with `/dev/kvm` access (or Windows with Hyper-V)
- Built `openvmm`, `pipette`, and test kernel/initrd artifacts

Build everything:

```bash
cargo build --release \
  -p burette -p openvmm -p pipette
```

Burette resolves guest kernels, initrds, tools, OpenVMM, and Pipette through
Petri's known artifact paths. If a selected benchmark reports a missing
artifact, use `cargo xflowey vmm-tests-run --build-only` with a related test to
populate the test-content directory, or use `burette package` from a machine
where all artifacts resolve.

## Architecture

Each benchmark controls an OpenVMM process through Petri:

```text
burette
  |- resolve host and guest artifacts
  |- launch OpenVMM
  |    `- boot Linux test guest
  |- connect to Pipette in the guest
  |- run or observe the workload
  |- collect host and guest measurements
  `- stop the VM and write JSON results
```

Some tests rebuild the VM for every sample, while throughput tests can retain a
warm VM and run repeated guest workloads. Read the benchmark description when
comparing first-run and steady-state results.

## Running Tests

### Boot time

Measures launch-to-pipette-connect time using Linux direct boot:

```bash
burette run --test boot-time -o report.json
```

Available profiles control the VM configuration:

| Profile | Description |
|---|---|
| `standard` | Full device set, serial agent, shared memory |
| `quiet-serial` | Like standard but suppresses kernel console |
| `minimal` | Pipette-as-init, minimal devices, shared memory |
| `minimal-private` | Same as minimal but with private memory (fastest) |

```bash
# Use a specific profile
burette run --test boot-time --profile minimal -o report.json

# Custom iteration count and guest RAM
burette run --test boot-time --iterations 20 --mem-mb 1024
```

### Memory overhead

Boots a single VM and measures host-side memory consumption for the
openvmm process tree:

```bash
burette run --test memory -o memory.json
```
On Windows, the available process accounting differs and VMM overhead follows
the private-memory measurement.

### Network throughput

Measures TCP throughput (Gbps) and UDP packet rate (pps) using iperf3
with a linux_direct VM, an erofs tool image, and Consomme networking:

```bash
burette run --test network -o network.json

# Test with virtio-net instead of VMBus
burette run --test network --nic virtio-net -o network.json
```

The report records TCP throughput and UDP packet-rate metrics. Keep the NIC,
network backend, host CPU placement, and MTU constant between compared runs.

Compare shared vs. private memory overhead:

```bash
burette run --test memory --profile minimal -o shared.json
burette run --test memory --profile minimal-private -o private.json
burette compare shared.json private.json
```

### Scale boot

Launches N VMs concurrently to measure boot time under contention
and per-VM memory overhead. Default sweep: N = 1, 2, 4, 8, 16, 32,
64.

```bash
# Full geometric sweep (auto-stops at 90% host memory)
burette run --test scale-boot --mem-mb 256 -o scale.json

# Single data point
burette run --test scale-boot --vms 16 --mem-mb 256

# Custom sweep
burette run --test scale-boot --vms 1,2,4,8 --max-vms 32
```

Per-N metrics include `scale_{N}_mean_boot_ms`,
`scale_{N}_p99_boot_ms`, `scale_{N}_last_ready_ms`,
`scale_{N}_per_vm_memory_mib`, and others.

### Disk I/O

Measures block I/O throughput (MiB/s) and IOPS using fio in a linux_direct
VM with an erofs tool image and a data disk. Supports virtio-blk and storvsc
(synthetic SCSI) backends:

```bash
# Virtio-blk with RAM-backed disk (measures virtio overhead)
burette run --test disk-io -o disk.json

# Storvsc backend
burette run --test disk-io --disk-backend storvsc -o disk.json

# File-backed disk for realistic host I/O latency
burette run --test disk-io --data-disk /tmp/test.raw --data-disk-size-gib 8
```

Reported metrics per backend:

- `fio_{backend}_seq_read_bw` / `fio_{backend}_seq_write_bw` — sequential bandwidth (MiB/s)
- `fio_{backend}_rand_read_bw` / `fio_{backend}_rand_write_bw` — random bandwidth (MiB/s)
- `fio_{backend}_rand_read_iops` / `fio_{backend}_rand_write_iops` — random IOPS

By default a RAM-backed disk is used to isolate virtio/storvsc overhead
without host filesystem noise. Pass `--data-disk` with a path on fast
storage (e.g., NVMe) for end-to-end latency measurements.

### virtio-fs

The virtio-fs benchmark boots a minimal Linux guest, mounts a host-exported
filesystem, and runs `fio` against a test file through the virtio-fs data path:

```bash
burette run --test virtio-fs -o virtio-fs.json
```

Use `--virtiofs-file-size-mib` to change the generated test-file size. Ensure
the host filesystem has enough free space and keep its storage medium constant
between runs.

## Test lifecycle and cleanup

Burette creates a Petri log source, resolves only the artifacts required by the
selected test, and then runs that benchmark's harness. VM teardown is part of
the measured test's cleanup path. A failed workload can therefore leave useful
OpenVMM and guest logs even when no JSON sample is recorded.

Use `--log-dir` to retain logs outside the default
`vmm_test_results/burette` location. On Linux, `--perf-dir` can retain host
performance traces for supported tests.

## Comparing Reports

```bash
burette compare baseline.json candidate.json
```

Prints a table of deltas and percentage changes for each metric.
Optionally write the comparison to JSON:

```bash
burette compare baseline.json candidate.json -o diff.json
```

The comparison displays absolute and percentage deltas. A positive percentage
is not universally better: higher throughput is desirable, while higher boot
latency or memory overhead is a regression. Interpret the metric's unit and
direction before drawing a conclusion.

## Remote Deployment

Package all binaries and artifacts into a self-contained tarball:

```bash
burette package -o burette_bundle.tar.gz
```

On the remote machine:

```bash
tar xzf burette_bundle.tar.gz
cd burette_bundle
VMM_TESTS_CONTENT_DIR=$PWD ./burette run -o report.json
```

The bundle includes `burette`, `openvmm`, `pipette`, the test kernel,
and initrd — no Rust toolchain or repo checkout needed.

The remote host still needs a supported virtualization interface and any host
facilities required by the selected network, storage, or filesystem backend.

## Running All Tests

Omit `--test` to run every test:

```bash
burette run -o full_report.json
```

## JSON Report Format

Reports are JSON files with git revision info, timestamps, and
per-metric statistics:

```json
{
  "git_revision": "abc123",
  "git_commit_date": "2026-03-18T00:00:00Z",
  "date": "2026-03-18T01:00:00Z",
  "results": [
    {
      "name": "boot_time_ms",
      "unit": "ms",
      "iterations": 10,
      "mean": 126.3,
      "std_dev": 1.5,
      "min": 124.4,
      "max": 128.1
    }
  ]
}
```

The report records source revision and time alongside per-metric summary
statistics. Preserve the complete report rather than extracting only the mean;
sample count, standard deviation, minimum, and maximum help distinguish a real
change from host noise.

## Measurement practice

For useful before/after comparisons:

1. Use the same host, power policy, kernel, firmware, OpenVMM features, and
   guest artifacts.
2. Stop unrelated CPU, disk, and network workloads.
3. Use release builds for representative performance.
4. Run enough iterations to expose variance.
5. Keep raw reports and logs for both revisions.
6. Repeat surprising results before attributing them to a code change.

The scale test deliberately applies host-memory limits. A truncated sweep is
not directly comparable with a sweep that reached larger VM counts.

## Troubleshooting

- A missing-artifact error names the Petri artifact that could not be resolved;
  build or restore that exact target rather than substituting a host binary.
- A launch-to-Pipette timeout usually means the guest failed to boot or the
  selected transport did not connect. Read the Petri and guest serial logs.
- Network failures can come from the helper process, host backend, guest tool
  image, or firewall. Verify a single warm-up run before collecting samples.
- File-backed disk and virtio-fs results are sensitive to host caching and free
  space. Use a dedicated fast filesystem when measuring end-to-end I/O.
- A report with large variance is not made reliable by `compare`; remove the
  source of noise or increase the iteration count.
