// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenVMM-specific allowlist of villain tests that are **expected to SKIP** in
//! the current configuration, and the reason why.
//!
//! # Why skips are failures by default
//!
//! Each villain test boots the "kitchen-sink" VM (every virtio device we
//! support attached) and, if its target device is absent or a precondition is
//! unmet, the guest prints `[SKIP] <name>` and moves on. Because the
//! kitchen-sink VM is *supposed* to have every device attached, a skip almost
//! always means one of:
//!
//! 1. A device we meant to attach but didn't — a harness/config bug (this is
//!    exactly how a `#[cfg]` mistake once silently dropped the vsock device).
//! 2. A device or feature OpenVMM genuinely doesn't implement yet — a real
//!    coverage gap worth tracking.
//! 3. A transport/arch not applicable to the current phase-1 config.
//!
//! All three are things we want *reviewed*, not swallowed. So a `SKIP` is a
//! **failure** (see [`crate::villain::evaluate`]) unless the test is listed
//! here. This mirrors [`crate::known_failures`] and keeps the set of
//! not-actually-tested cases explicit and auditable rather than hidden behind a
//! green dashboard.
//!
//! # Run-and-assert (not `#[ignore]`)
//!
//! Unlike [`crate::known_failures`] (which marks tests `#[ignore]` so they never
//! boot — necessary because those wedge the host), expected-skip tests are
//! cheap: the device is absent, so the guest skips immediately and the VM
//! powers off. We therefore **run them and assert they still skip**. If an entry
//! here produces any *other* verdict — e.g. OpenVMM grew the device and it now
//! `PASS`es, or it now `FAIL`s — the test fails with an actionable message so
//! the stale entry gets pruned (or moved to [`crate::known_failures`]). This
//! catches drift in both directions instead of letting an allowlist entry rot.
//!
//! The list is seeded from the first full-suite CI run: every unexpected skip
//! surfaces as a failure to triage, and genuinely-expected skips are then added
//! here with a reason.

/// A villain test that is expected to SKIP in the current OpenVMM configuration.
pub struct KnownSkip {
    /// The villain test name (matches `tests.tsv` column 1 / `vv.test=<name>`).
    pub name: &'static str,
    /// Human-readable reason the test skips, ideally referencing the missing
    /// device / unimplemented feature (and a tracking issue where one exists),
    /// e.g. `"virtio-scsi (0x1048) not attached to the kitchen-sink VM"`.
    pub reason: &'static str,
}

/// The known-skip allowlist. Keep sorted by name.
///
/// Empty until seeded from the first full-suite run: with an empty list every
/// skip is a failure, so the initial run surfaces the complete set of
/// not-exercised tests for triage.
pub const KNOWN_SKIPS: &[KnownSkip] = &[];

/// Look up the known-skip entry for `name`, if present.
pub fn lookup(name: &str) -> Option<&'static KnownSkip> {
    KNOWN_SKIPS.iter().find(|e| e.name == name)
}
