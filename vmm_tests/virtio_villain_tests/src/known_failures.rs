// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenVMM-specific list of villain tests that OpenVMM does not currently pass
//! and are therefore marked *ignored* in the libtest-mimic harness.
//!
//! This list lives here (in the OpenVMM tree), not upstream in villain,
//! because it describes OpenVMM-specific outcomes. Entries fall into three
//! kinds, distinguished by their `reason`:
//! - **OpenVMM bugs** — a device-model defect, linked to a filed
//!   `microsoft/openvmm#NNNN` issue. Remove the entry when the bug is fixed so
//!   the test runs (and gates) in CI again.
//! - **Not an OpenVMM bug** — a villain test defect, a precondition the test
//!   fails to establish, or an assertion that is not spec-grounded. OpenVMM
//!   behaves correctly, but the test still reports failure, so it stays ignored.
//!   The `reason` starts with "Not an OpenVMM bug:" and explains why.
//! - **Accepted-by-design** — a behavioral difference OpenVMM will not change
//!   (e.g. RNG0004); the `reason` captures the full rationale inline.
//!
//! Ignored tests behave like libtest `#[ignore]` tests:
//! - They are **skipped by default**, so CI (which runs with the default
//!   `run-ignored` setting) does not run them. This keeps the gate green
//!   against known product bugs and, importantly, avoids paying the per-test
//!   timeout for cases that wedge OpenVMM (see below).
//! - They can still be run locally during fix development with
//!   `cargo nextest run -p virtio_villain_tests --run-ignored all`
//!   (or `--run-ignored ignored-only` to run *only* the known failures).
//!
//! Why ignore rather than invert (XFAIL)? Several of these are unrecoverable
//! host hangs: OpenVMM's virtio worker spins on a malformed descriptor chain
//! and the VM never powers off (and cannot even be torn down). Such a test can
//! only be ended by the external nextest timeout, so it can never reach an
//! in-harness "expected failure" inversion. Skipping is both correct and much
//! cheaper.

/// A villain test that OpenVMM does not currently pass, and that we therefore
/// skip by default (mark ignored).
pub struct KnownFailure {
    /// The villain test name (matches `tests.tsv` column 1 / `vv.test=<name>`).
    pub name: &'static str,
    /// Human-readable reason. For OpenVMM bugs, reference the tracking issue,
    /// e.g. `"… (microsoft/openvmm#4045)"`. For tests where OpenVMM behaves
    /// correctly, start with `"Not an OpenVMM bug:"` and explain why the test
    /// still fails.
    pub reason: &'static str,
}

/// The known-failure list. Keep sorted by name.
///
/// The residual failures from the first full-suite CI run were root-caused
/// against OpenVMM and villain source. The genuine device-model bugs were filed
/// as `microsoft/openvmm#4045-4049` (grouped by root cause); the remainder are
/// villain test defects / spec-permitted behavior that OpenVMM handles correctly
/// but the test still flags. Keep the `reason` accurate to which kind each is.
pub const KNOWN_FAILURES: &[KnownFailure] = &[
    KnownFailure {
        name: "B0002",
        reason: "virtio-blk out-of-range read near sector UINT64_MAX: unchecked \
                 LBA arithmetic wraps past the capacity check and returns \
                 success with zeros (microsoft/openvmm#4046)",
    },
    KnownFailure {
        name: "B0081",
        reason: "virtio-blk out-of-range write at sector UINT64_MAX: unchecked \
                 LBA arithmetic wraps past the capacity check; the write is \
                 accepted and stored (microsoft/openvmm#4046)",
    },
    KnownFailure {
        name: "B0082",
        reason: "Not an OpenVMM bug: villain test defect. The test advertises \
                 32 KiB of physically-contiguous GPA from 8 anonymous mmap pages \
                 but only translates the first page to a PFN; OpenVMM performs \
                 finite, correct I/O over the GPAs as programmed.",
    },
    KnownFailure {
        name: "B0091",
        reason: "virtio-blk out-of-range read at sector UINT64_MAX: unchecked \
                 LBA arithmetic wraps past the capacity check and returns \
                 success with zeros (microsoft/openvmm#4046)",
    },
    KnownFailure {
        name: "B0125",
        reason: "virtio-blk request descriptor-chain loop: OpenVMM detects the \
                 bad chain but re-parses the same head forever, wedging the \
                 worker unrecoverably (microsoft/openvmm#4045)",
    },
    KnownFailure {
        name: "B0139",
        reason: "Not an OpenVMM bug: stale/currently-conformant. The discard \
                 sector+num_sectors add is a latent overflow (same class as \
                 microsoft/openvmm#4046), but for this test's operands the \
                 request correctly returns S_IOERR.",
    },
    KnownFailure {
        name: "E0028",
        reason: "virtio-pmem reports region size 0: the device maps a region but \
                 neither offers VIRTIO_PMEM_F_SHMEM_REGION nor populates the \
                 start/size config fields (microsoft/openvmm#4048)",
    },
    KnownFailure {
        name: "E0032",
        reason: "virtio-pmem reports start/capacity 0: the device maps a region \
                 but neither offers VIRTIO_PMEM_F_SHMEM_REGION nor populates the \
                 start/size config fields (microsoft/openvmm#4048)",
    },
    KnownFailure {
        name: "M0024",
        reason: "Not an OpenVMM bug: villain precondition defect. The test reads \
                 QueueReset without negotiating VIRTIO_F_RING_RESET (which \
                 OpenVMM does not offer); the register is only operative once \
                 the feature is negotiated (spec 4.2.2.2).",
    },
    KnownFailure {
        name: "M0030",
        reason: "Not an OpenVMM bug: unsubstantiated/stale. OpenVMM validates \
                 the wrapped QueueDesc range before any ring access and fails \
                 the queue-enable cleanly (state reset); no guest-kill \
                 reproduces on the current code.",
    },
    KnownFailure {
        name: "P0003",
        reason: "virtio-blk packed descriptor chain exceeding queue size: \
                 detected but re-parsed forever, wedging the worker \
                 (microsoft/openvmm#4045)",
    },
    KnownFailure {
        name: "PCI0102",
        reason: "Not an OpenVMM bug: villain assertion contradicts spec. The \
                 test requires subsystem vendor 0x1AF4, but spec 4.1.2.1 permits \
                 subsystem IDs to reflect the environment; OpenVMM reports the \
                 Microsoft subsystem vendor 0x1414.",
    },
    KnownFailure {
        name: "PCI0114",
        reason: "virtio-pci raises a spurious config-change interrupt (ISR bit \
                 1) at DRIVER_OK, so ISR reads 0x02 before any I/O \
                 (microsoft/openvmm#4049)",
    },
    KnownFailure {
        name: "RNG0004",
        reason: "virtio-rng non-atomic writable-descriptor handling. OpenVMM \
                 streams entropy into an over-long writable descriptor (len past \
                 end of RAM) and writes the backed portion of guest RAM (vring + \
                 init) before erroring on the first unbacked page, killing the \
                 guest before it emits its verdict. ACCEPTED BY DESIGN, not a \
                 bug: this write-through behavior matches physical/vDPA hardware \
                 (which faults per-access via the IOMMU after a partial write); \
                 the software VMMs QEMU (map-first) and Cloud Hypervisor \
                 (check_range) instead pre-validate the whole range and write \
                 nothing. Not a host hang, not a spec violation (spec 2.7.5 puts \
                 buffer validity on the driver), and no host-memory-safety issue. \
                 OpenVMM is intentionally left as-is. Represents the villain \
                 `*_huge_len_past_ram` (\"crosses end of RAM\") family",
    },
    KnownFailure {
        name: "S0048",
        reason: "Not an OpenVMM bug: villain test-ordering defect. It writes \
                 queue_size after the harness has already enabled the queue and \
                 set DRIVER_OK; OpenVMM correctly ignores post-enable writes \
                 (spec 4.1.4.3.2 requires configuring before enabling).",
    },
    KnownFailure {
        name: "T0001",
        reason: "virtio-blk self-looping descriptor chain: detected but \
                 re-parsed forever, wedging the worker unrecoverably \
                 (microsoft/openvmm#4045)",
    },
    KnownFailure {
        name: "T0002",
        reason: "virtio-blk descriptor chain exceeding queue size: detected but \
                 re-parsed forever, wedging the worker unrecoverably \
                 (microsoft/openvmm#4045)",
    },
    KnownFailure {
        name: "T0003",
        reason: "virtio-blk out-of-bounds descriptor `next` index: detected but \
                 re-parsed forever, wedging the worker unrecoverably \
                 (microsoft/openvmm#4045)",
    },
    KnownFailure {
        name: "T0008",
        reason: "virtio-blk descriptor addr+len 64-bit wrap: unchecked GPA-range \
                 arithmetic panics in checked builds (release returns S_IOERR) \
                 (microsoft/openvmm#4047)",
    },
    KnownFailure {
        name: "T0022",
        reason: "Not an OpenVMM bug: spec-permitted. Rejecting a driver-reoffered \
                 in-flight head is a driver MUST-NOT (spec 2.7.6), not a device \
                 requirement; OpenVMM re-parses each dequeue into fresh buffers \
                 with no host memory-safety issue. Optional hardening only.",
    },
    KnownFailure {
        name: "T0025",
        reason: "virtio-blk out-of-bounds available-ring entry: triggers the \
                 wedging worker busy-loop (microsoft/openvmm#4045). Note: this \
                 villain test always reports PASS, so it cannot detect a \
                 regression of that bug.",
    },
    KnownFailure {
        name: "T0054",
        reason: "virtio-blk descriptor chain length == queue_size + 1: detected \
                 but re-parsed forever, wedging the worker \
                 (microsoft/openvmm#4045)",
    },
    KnownFailure {
        name: "T0073",
        reason: "Not an OpenVMM bug: villain test defect. It hardcodes a \
                 top-of-RAM address and points a writable descriptor at an \
                 unowned page instead of discovering the real RAM top; OpenVMM's \
                 I/O is finite and in-range.",
    },
    KnownFailure {
        name: "T0082",
        reason: "virtio-blk descriptor with addr/len/flags/next all UINT_MAX: \
                 the invalid indirect/OOB access is detected but re-parsed \
                 forever, wedging the worker (microsoft/openvmm#4045)",
    },
    KnownFailure {
        name: "T0084",
        reason: "virtio-blk available ring full of out-of-bounds descriptor \
                 indices: triggers the wedging worker busy-loop \
                 (microsoft/openvmm#4045)",
    },
    KnownFailure {
        name: "Z0014",
        reason: "Not an OpenVMM bug: villain test defect. OpenVMM advertises no \
                 VIRTIO_BLK_F_ZONED and correctly returns S_UNSUPP; the test \
                 never negotiates ZONED and waits for only 1 of 64 completions.",
    },
];

/// Look up the known-failure entry for `name`, if present.
pub fn lookup(name: &str) -> Option<&'static KnownFailure> {
    KNOWN_FAILURES.iter().find(|e| e.name == name)
}
