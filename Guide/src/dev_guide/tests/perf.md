# Performance Tests (burette)

`burette` is a standalone binary that runs performance benchmarks
against OpenVMM using the `petri` test framework. It measures boot
time, memory overhead, and concurrent VM scaling behavior, producing
JSON reports that can be compared across builds.

## Prerequisites

- Linux host with `/dev/kvm` access (or Windows with Hyper-V)
- Built `openvmm`, `pipette`, and test kernel/initrd artifacts

Build everything:

```bash
cargo build --release \
  -p burette -p openvmm -p pipette
```

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

### Network throughput

Measures TCP throughput (Gbps) and UDP packet rate (pps) using iperf3
with an Alpine VM and Consomme networking:

```bash
burette run --test network -o network.json

# Test with virtio-net instead of VMBus
burette run --test network --nic virtio-net -o network.json
```

Reported metrics:

- `memory_rss_kib` — total RSS across process tree
- `memory_private_kib` — private (non-shared) pages
- `memory_vmm_overhead_kib` — VMM runtime overhead excluding guest
  RAM (Linux only; on Windows this equals `private_kib`)
- `memory_pss_kib` — proportional set size (Linux only)
- `memory_process_count` — processes in the tree

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

## Comparing Reports

```bash
burette compare baseline.json candidate.json
```

Prints a table of deltas and percentage changes for each metric.
Optionally write the comparison to JSON:

```bash
burette compare baseline.json candidate.json -o diff.json
```

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
