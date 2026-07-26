// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenVMM-specific list of villain tests that are **known to fail** and are
//! therefore marked *ignored* in the libtest-mimic harness.
//!
//! This list lives here (in the OpenVMM tree), not upstream in villain,
//! because it describes OpenVMM device-model bugs, not villain bugs.
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
//! in CI again. Each entry should link to a filed OpenVMM issue.

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
/// The initial entries are the virtio-blk (device 0x1042) malformed-descriptor
/// cases confirmed to send OpenVMM's virtio worker into a non-terminating loop
/// (an unrecoverable, guest-triggerable host hang). Issues still need to be
/// filed; update the `reason` with the issue link once they are.
pub const KNOWN_FAILURES: &[KnownFailure] = &[
    KnownFailure {
        name: "B0002",
        reason: "virtio-blk sector*512+data_len 64-bit wrap wedges the device \
                 (microsoft/openvmm#TODO)",
    },
    KnownFailure {
        name: "P0003",
        reason: "virtio-blk packed descriptor list exceeding queue size wedges \
                 the device (microsoft/openvmm#TODO)",
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
];

/// Look up the known-failure entry for `name`, if present.
pub fn lookup(name: &str) -> Option<&'static KnownFailure> {
    KNOWN_FAILURES.iter().find(|e| e.name == name)
}
