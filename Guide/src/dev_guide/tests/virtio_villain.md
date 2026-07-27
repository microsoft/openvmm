# virtio-villain Tests

[virtio-villain] is a guest-side virtio protocol fault-injection / conformance
suite. It is a static musl `init` (PID 1) that walks the guest's virtio
transports itself, injects out-of-spec virtqueue inputs, and prints a verdict
marker per test on the serial console before powering the VM off. OpenVMM runs
it as a device-model conformance gate: each villain test exercises OpenVMM's
virtio device emulation from inside the guest and reports whether OpenVMM
handled the (often malformed) input correctly.

The suite lives in `vmm_tests/virtio_villain_tests`. Unlike the
[VMM Tests](./vmm.md), villain is *not* built around the `#[vmm_test]` macro —
it is a standalone runner that uses **petri as a library** (like `burette`) plus
**libtest-mimic** to expose one nextest case per villain test. It drives OpenVMM
over the PCI virtio transport and runs on Linux `x86_64` under KVM.

## How it works

Each nextest case boots a single **"kitchen-sink" OpenVMM VM** — a linux-direct
VM with every supported virtio device attached — passing `vv.test=<id>` on the
kernel command line. Villain runs that one test, prints a `[TAG] <id>` verdict
to the serial console, and powers off. The harness reads the verdict back from
petri's teed serial log and turns it into a pass/fail:

| villain verdict | outcome |
|-----------------|---------|
| `[PASS]` / `[XFAIL]`-style good verdicts | **pass** |
| `[FAIL]` and other bad verdicts | **fail** |
| `[SKIP]` | **fail** (see below) |
| no verdict marker (guest wedged / never booted) | **fail** |

The test manifest is villain's `tests.tsv` (one row per test: name,
description, spec section, target `device_id`, …). The guest kernel is the
existing OpenVMM linux-direct test `vmlinux`; villain's `initramfs.cpio.gz` and
`tests.tsv` come from the `openvmm-deps` release artifact (or, locally, from
`--villain-initramfs` / `--villain-tsv` or the `VILLAIN_INITRAMFS` /
`VILLAIN_TSV` env vars).

### `[SKIP]` is always a failure — fail fast

A `[SKIP]` means the guest didn't exercise anything: the target device was
absent, or a precondition wasn't met. On the kitchen-sink VM — which is supposed
to attach every device we support — a skip almost always means a device we
*meant* to attach silently wasn't (this is exactly how a `#[cfg]` mistake once
dropped the vsock device and hid a whole test block). Letting such a test report
"pass" would quietly mask a harness bug (against the repo's fail-fast
philosophy: crash on a broken invariant, never silently degrade). So the harness
treats **every `[SKIP]` as a failure**.

Tests for devices we deliberately *don't* attach are handled up front by
ignoring them (next section), so they never reach this rule during a normal run.

## Two ways a test is skipped: `#[ignore]`, never silent

There are exactly two reasons a villain test does not gate, and **both surface
as `ignored`** in nextest output — never as a false pass. Both are just
libtest-style `#[ignore]`: skipped by default, runnable with `--run-ignored`.

### 1. Unsupported device (`supported_devices.rs`)

Villain has ~1400 tests but only a handful of device classes. Rather than
enumerate every test to skip, we enumerate the device IDs the kitchen-sink VM
*does* attach, in `SUPPORTED_DEVICE_IDS` (IDs follow the virtio convention
`0x1040 + virtio_device_type`):

| device_id | device |
|-----------|--------|
| `0x1041` | network |
| `0x1042` | block |
| `0x1043` | console |
| `0x1044` | entropy (rng) |
| `0x1053` | vsock |
| `0x105a` | virtio-fs |
| `0x105b` | pmem |

Any test whose target device ID is not in that set is `#[ignore]`d at
trial-construction time, so it never boots a VM. Device-agnostic tests
(`device_id == 0x0000`, e.g. transport-level PCI checks) run regardless.

This keeps ~270 tests for devices we don't yet emulate (IOMMU, memory balloon,
virtio-mem, watchdog, RTC) from booting a VM only to `[SKIP]`. **To enable a
device's whole test block, add its ID to `SUPPORTED_DEVICE_IDS` and attach the
device in `run.rs`'s `attach_kitchen_sink`** — keep the two in sync.

Because unsupported-device tests are ignored (not run), force-running them with
`--run-ignored` will correctly *fail* their absent-device `[SKIP]`: you asked to
actually run them and the device isn't there.

### 2. Known OpenVMM bug (`known_failures.rs`)

Tests that OpenVMM currently *fails* are listed in `KNOWN_FAILURES` and
`#[ignore]`d so the gate stays green while the underlying product bugs are
triaged. Each entry has the villain test name and a reason (ideally a tracking
issue).

```admonish note
We `#[ignore]` known failures rather than invert them (XFAIL) because several
are **unrecoverable host hangs** — OpenVMM's virtio worker spins on a malformed
descriptor chain and the VM never powers off. Such a test can only be ended by
the external nextest timeout, so it can never reach an in-harness "expected
failure". Ignoring is both correct and much cheaper.
```

When a bug is fixed, remove its entry; the test runs (and gates) again. To work
on a fix, run the ignored tests explicitly (see below).

## Running locally (flowey)

The easiest path builds OpenVMM, downloads the villain artifact, stages the
guest kernel, and runs the suite in one command:

```bash
cargo xflowey virtio-villain-run
```

Run a subset with a [nextest filter](https://nexte.st/docs/filtersets/):

```bash
cargo xflowey virtio-villain-run --filter "test(B0001)"
cargo xflowey virtio-villain-run --filter "test(/^B00/)"
```

Run the known-failing (ignored) tests too — e.g. while developing a fix:

```bash
cargo xflowey virtio-villain-run --run-ignored --filter "test(B0002)"
```

```admonish warning
`--run-ignored` also un-ignores unsupported-device tests, which will then fail
with an absent-device `[SKIP]`. Combine it with a `--filter` that targets the
specific known-failure you're working on.
```

## Running locally (manual nextest)

You can invoke the nextest binary directly if you already have the villain
payload. Point it at the `tests.tsv` and `initramfs.cpio.gz`:

```bash
VILLAIN_TSV=/path/to/tests.tsv \
VILLAIN_INITRAMFS=/path/to/initramfs.cpio.gz \
cargo nextest run -p virtio_villain_tests --run-ignored ignored-only --filter-expr 'test(B0002)'
```

`--run-ignored ignored-only` runs *only* the ignored tests; `--run-ignored all`
runs both. Omit it to run just the gating tests.

## In CI

A separate, parallel per-PR job — **`x64-linux-kvm-virtio-villain`** — runs the
suite under KVM on a beefier Linux machine. To keep villain independent of the
main VMM-tests jobs and to run on KVM hardware, the work is split:

- The Linux **build** machine produces a `cargo nextest archive`
  (`x64-linux-virtio-villain-tests-archive`) alongside the OpenVMM binary it
  drives. The archive does not run any test binaries — test enumeration
  (`--list`) happens at run time in the consume job.
- The KVM **test** job consumes that archive, injects `VILLAIN_TSV` /
  `VILLAIN_INITRAMFS`, and runs nextest. Its petri result logs
  (`x64-linux-kvm-virtio-villain-vmm-tests-logs`) match the `upload-petri-results`
  glob, so villain results appear alongside the other VMM-test results.

Because villain boots linux-direct with its own initramfs, its test env skips
the mu_msvm UEFI firmware and Windows `virtio-win` driver staging that the full
VMM-tests env pulls in (`stage_uefi_and_virtio_win = false`).

[virtio-villain]: https://github.com/weltling/virtio-villain
