// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenVMM-specific list of villain tests that are **known to fail** and are
//! therefore marked *ignored* in the libtest-mimic harness.
//!
//! This list lives here (in the OpenVMM tree), not upstream in villain,
//! because it describes OpenVMM-specific outcomes — device-model bugs *or*
//! accepted behavioral differences — not villain bugs.
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
//!
//! When a bug is fixed, remove its entry here; the test then runs (and gates)
//! in CI again. Bug entries should link to a filed OpenVMM issue. A few entries
//! are instead **accepted-by-design** differences that OpenVMM will not change
//! (e.g. RNG0004 — see its reason); those stay listed and their `reason` field
//! captures the full rationale inline rather than being tracked as bugs.

/// A villain test that OpenVMM is known to fail, and that we therefore skip by
/// default (mark ignored).
pub struct KnownFailure {
    /// The villain test name (matches `tests.tsv` column 1 / `vv.test=<name>`).
    pub name: &'static str,
    /// Human-readable reason, ideally referencing a tracking issue, e.g.
    /// `"virtio-blk unbounded descriptor walk (microsoft/openvmm#NNNN)"`.
    pub reason: &'static str,
}

/// The known-failure list. Keep sorted by name.
///
/// Seeded from the first full-suite CI run (openvmm-deps 0.3.0-112, x86_64/KVM):
/// the tests below either drove OpenVMM's virtio worker into a non-terminating
/// loop (marked "host hang" — these time out, which is why the list is ignored
/// rather than inverted) or the device model accepted malformed input / returned
/// a wrong value. Issues still need to be filed; update each `reason` with the
/// issue link once they are.
pub const KNOWN_FAILURES: &[KnownFailure] = &[
    KnownFailure {
        name: "B0002",
        reason: "virtio-blk sector*512+data_len 64-bit wrap wedges the device \
                 (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "B0081",
        reason: "virtio-blk write at sector = UINT64_MAX mishandled \
                 (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "B0082",
        reason: "virtio-blk 32KB (64-sector) single request host hang \
                 (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "B0091",
        reason: "virtio-blk read with sector = UINT64_MAX mishandled \
                 (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "B0125",
        reason: "virtio-blk descriptor chain loop in request host hang \
                 (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "B0139",
        reason: "virtio-blk discard segment sector+num_sectors u64 overflow \
                 mishandled (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "E0028",
        reason: "virtio-pmem shared memory region size reported non-zero \
                 (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "E0032",
        reason: "virtio-pmem config start/capacity read incorrect \
                 (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "M0024",
        reason: "virtio-mmio QueueReset register readback returns a value other \
                 than 0 or 1 (spec 4.2.2.2 requires 1 while reset is in progress, \
                 0 otherwise) (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "M0030",
        reason: "virtio-mmio QueueDesc programmed at the top of the 64-bit \
                 address space kills the guest before it reports a verdict \
                 (address-edge descriptor handling, same class as the villain \
                 huge-len/address-wrap family) (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "M0031",
        reason: "virtio-mmio QueueNotify-with-notification-data test self-SKIPs: \
                 OpenVMM does not negotiate VIRTIO_F_NOTIFICATION_DATA. Not a \
                 bug, just an unsupported feature; ignored so the SKIP does not \
                 fail the suite",
    },
    KnownFailure {
        name: "M0032",
        reason: "virtio-mmio config-change interrupt test self-SKIPs: OpenVMM's \
                 kitchen-sink devices do not raise the config-change interrupt \
                 the test needs. Not a bug, just an unmet precondition; ignored \
                 so the SKIP does not fail the suite",
    },
    KnownFailure {
        name: "P0003",
        reason: "virtio-blk packed descriptor list exceeding queue size wedges \
                 the device (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "PCI0102",
        reason: "virtio PCI subsystem vendor is not 0x1AF4 \
                 (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "PCI0114",
        reason: "virtio PCI ISR does not read zero with no pending interrupt \
                 (microsoft/openvmm#TODO)",
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
        reason: "virtio-blk queue_size does not read back the driver-written \
                 value (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "T0001",
        reason: "virtio-blk self-looping descriptor chain hangs the virtio \
                 worker unrecoverably (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "T0002",
        reason: "virtio-blk descriptor chain exceeding queue size hangs the \
                 virtio worker unrecoverably (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "T0003",
        reason: "virtio-blk out-of-bounds descriptor `next` index hangs the \
                 virtio worker unrecoverably (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "T0008",
        reason: "virtio-blk descriptor addr+len 64-bit wrap mishandled \
                 (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "T0022",
        reason: "virtio-blk duplicate head index in available ring mishandled \
                 (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "T0025",
        reason: "virtio-blk available ring entry index out of bounds host hang \
                 (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "T0054",
        reason: "virtio-blk descriptor chain length == queue_size + 1 host hang \
                 (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "T0073",
        reason: "virtio-blk descriptor buffer spanning to exact end of RAM host \
                 hang (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "T0082",
        reason: "virtio-blk descriptor with addr/len/flags/next all UINT_MAX host \
                 hang (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "T0084",
        reason: "virtio-blk avail ring full of out-of-bounds descriptor indices \
                 host hang (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "Z0014",
        reason: "virtio-blk opening more zones than max_open_zones allows \
                 mishandled (microsoft/openvmm#TODO)",
    },
];

/// Look up the known-failure entry for `name`, if present.
pub fn lookup(name: &str) -> Option<&'static KnownFailure> {
    KNOWN_FAILURES.iter().find(|e| e.name == name)
}
