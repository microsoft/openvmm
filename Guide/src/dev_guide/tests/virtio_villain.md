# virtio-villain Tests

[virtio-villain] is a guest-side virtio conformance and fault-injection suite. It
is a static musl `init` (PID 1) that walks the guest's virtio transports, injects
out-of-spec virtqueue inputs, and prints a verdict for each test on the serial
console before powering the VM off. OpenVMM uses it as a device-model conformance
gate: each test exercises OpenVMM's virtio emulation from inside the guest and
reports whether OpenVMM handled the input correctly.

The suite lives in `vmm_tests/virtio_villain_tests`. Unlike the
[VMM Tests](./vmm.md), it does not use the `#[vmm_test]` macro. It is a standalone
runner built on petri (used as a library, like `burette`) and libtest-mimic,
exposing one nextest case per villain test. It drives OpenVMM over the virtio PCI
and MMIO transports and runs on Linux (`x86_64` and `aarch64`) under KVM.

## How it works

Each nextest case boots one "kitchen-sink" OpenVMM VM: a linux-direct VM with
every supported virtio device attached, started with `vv.test=<id>` on the kernel
command line. Villain runs that test, prints a `[TAG] <id>` verdict to the serial
console, and powers off. The harness reads the verdict from petri's serial log and
maps it to a result:

| villain verdict | outcome |
|-----------------|---------|
| `[PASS]` and other good verdicts | pass |
| `[FAIL]` and other bad verdicts | fail |
| `[SKIP]` | fail (see below) |
| no verdict marker (guest wedged or never booted) | fail |

The test manifest is villain's `tests.tsv`, which has one row per test (name,
description, spec section, target `device_id`, and so on). The guest kernel is the
existing OpenVMM linux-direct test `vmlinux`. Villain's `initramfs.cpio.gz` and
`tests.tsv` come from the `openvmm-deps` release artifact, or locally from
`--villain-initramfs` / `--villain-tsv` (or the `VILLAIN_INITRAMFS` /
`VILLAIN_TSV` environment variables).

### Why a `[SKIP]` is a failure

A `[SKIP]` means the guest did not exercise anything, because the target device
was absent or a precondition was not met. On the kitchen-sink VM, which attaches
every device we support, that usually indicates a harness problem: a device we
intended to attach was not attached (for example, when a `#[cfg]` guard once
dropped the vsock device). Reporting such a case as a pass would hide the bug, so
the harness treats every `[SKIP]` as a failure.

Tests for devices we do not attach are ignored up front (see below), so they do
not reach this rule during a normal run.

## When a test is ignored

A villain test does not gate for one of two reasons, and both are reported as
`ignored` in nextest output rather than as a pass. Both use libtest's `#[ignore]`:
skipped by default, and runnable with `--run-ignored`.

### Unsupported device (`supported_devices.rs`)

Villain has roughly 1400 tests but only a handful of device classes. Rather than
list every test to skip, we list the devices the kitchen-sink VM attaches, in
`DEVICE_CAPS` (keyed by device ID, which follows the virtio convention `0x1040 +
virtio_device_type`):

| device_id | device |
|-----------|--------|
| `0x1041` | network |
| `0x1042` | block |
| `0x1043` | console |
| `0x1044` | entropy (rng) |
| `0x1053` | vsock |
| `0x105a` | virtio-fs |
| `0x105b` | pmem |

A test whose target device is not in this set is ignored when its trial is
constructed, so it never boots a VM. Device-agnostic MMIO tests (`device_id ==
0x0000`) run against whatever device villain's `virtio_mmio_find` locates. Their
PCI counterparts are ignored: villain's `virtio_pci_find(0)` looks for a literal
PCI device `0x0000`, which no virtio device is, so they can only `[SKIP]`.

This avoids booting a VM for roughly 270 tests covering devices OpenVMM does not
yet emulate (IOMMU, memory balloon, virtio-mem, watchdog, RTC). To enable a
device's tests, add an entry to `DEVICE_CAPS` and attach the device in
`attach_kitchen_sink` (`run.rs`); keep the two in sync.

Because these tests are ignored rather than run, `--run-ignored` will run them and
report their absent-device `[SKIP]` as a failure.

### Known OpenVMM failure (`known_failures.rs`)

Tests that OpenVMM currently fails are listed in `KNOWN_FAILURES` and ignored, so
the gate stays green while the underlying bugs are triaged separately. Each entry
records the test name and a reason, ideally referencing a tracking issue. When a
bug is fixed, remove its entry so the test gates again.

These are ignored rather than inverted (XFAIL) because several are unrecoverable
host hangs: OpenVMM's virtio worker spins on a malformed descriptor chain and the
VM never powers off. Such a test can only be ended by the nextest timeout, so an
in-harness expected-failure inversion is not possible.

## Running locally (flowey)

`cargo xflowey virtio-villain-run` builds OpenVMM, downloads the villain artifact,
stages the guest kernel, and runs the suite:

```bash
cargo xflowey virtio-villain-run
```

Use `--filter` with a [nextest filter](https://nexte.st/docs/filtersets/) to run
a subset:

```bash
cargo xflowey virtio-villain-run --filter "test(B0001)"
```

Use `--run-ignored` to also run the ignored tests, for example when developing a
fix for a known failure. Because this also un-ignores unsupported-device tests
(which then fail on an absent-device `[SKIP]`), pair it with a `--filter` that
targets the specific test:

```bash
cargo xflowey virtio-villain-run --run-ignored --filter "test(B0002)"
```

## Running locally (manual nextest)

If you already have the villain payload, invoke nextest directly and point it at
the manifest and initramfs:

```bash
VILLAIN_TSV=/path/to/tests.tsv \
VILLAIN_INITRAMFS=/path/to/initramfs.cpio.gz \
cargo nextest run -p virtio_villain_tests --filter-expr 'test(B0001)'
```

`--run-ignored ignored-only` runs only the ignored tests; `--run-ignored all`
runs both ignored and normal tests.

## In CI

The `x64-linux-kvm-virtio-villain` job runs the suite under KVM as a separate,
parallel per-PR job. The work is split across two machines so the build does not
need KVM and the test run does not need a full toolchain:

- The Linux build machine produces a `cargo nextest archive`
  (`x64-linux-virtio-villain-tests-archive`) along with the OpenVMM binary it
  drives. The archive does not run test binaries; enumeration happens at run time.
- The KVM test machine consumes that archive, injects `VILLAIN_TSV` and
  `VILLAIN_INITRAMFS`, and runs nextest. Its result logs
  (`x64-linux-kvm-virtio-villain-vmm-tests-logs`) match the `upload-petri-results`
  glob, so villain results appear alongside the other VMM-test results.

Villain boots linux-direct with its own initramfs, so its test environment sets
`stage_uefi_and_virtio_win = false` and skips the mu_msvm UEFI firmware and
Windows `virtio-win` driver staging used by the full VMM-tests environment.

[virtio-villain]: https://github.com/weltling/virtio-villain
