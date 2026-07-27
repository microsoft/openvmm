// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Enumeration of virtio-villain tests and parsing of their serial verdicts.

use anyhow::Context as _;
use std::path::Path;

/// A single villain test, as described by one row of `tests.tsv`.
///
/// `tests.tsv` is emitted by `init --list-tsv` (see villain `bin/init.c`) with
/// one tab-separated row per test and no header:
///
/// ```text
/// name  desc  version  spec_section  device_id  flags  required_features  min_queues
/// ```
#[derive(Debug, Clone)]
pub struct VillainTest {
    /// Test identifier, passed to the guest via `vv.test=<name>` and matched
    /// against the serial verdict marker.
    pub name: String,
    /// Human-readable description.
    pub desc: String,
    /// virtio device id the test targets (e.g. `0x0002` for block).
    pub device_id: u16,
}

/// Parse `tests.tsv` into the list of villain tests.
pub fn parse_tsv(path: &Path) -> anyhow::Result<Vec<VillainTest>> {
    let text = fs_err::read_to_string(path)?;
    let mut tests = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let mut cols = line.split('\t');
        let name = cols
            .next()
            .filter(|s| !s.is_empty())
            .with_context(|| format!("{}:{}: missing test name", path.display(), lineno + 1))?;
        // Columns: name, desc, version, spec_section, device_id, flags,
        // required_features, min_queues. Parse strictly so a corrupt or
        // format-changed tests.tsv fails loudly rather than silently producing
        // bogus values.
        let desc = cols.next().with_context(|| {
            format!(
                "{}:{}: missing description column",
                path.display(),
                lineno + 1
            )
        })?;
        let _version = cols.next();
        let _spec_section = cols.next();
        let device_id = cols.next().with_context(|| {
            format!(
                "{}:{}: missing device_id column",
                path.display(),
                lineno + 1
            )
        })?;
        let device_id = device_id
            .strip_prefix("0x")
            .and_then(|h| u16::from_str_radix(h, 16).ok())
            .with_context(|| {
                format!(
                    "{}:{}: invalid device_id {:?} (expected 0x-prefixed u16 hex)",
                    path.display(),
                    lineno + 1,
                    device_id,
                )
            })?;

        tests.push(VillainTest {
            name: name.to_string(),
            desc: desc.to_string(),
            device_id,
        });
    }
    anyhow::ensure!(!tests.is_empty(), "no tests found in {}", path.display());
    Ok(tests)
}

/// The verdict the guest emits on the serial console for a test.
///
/// The guest prints `[<TAG>] <name>` (villain `bin/init.c`), where `<TAG>` is
/// one of these values. The device-health re-probe that distinguishes REJECT
/// from WEDGED is computed by the guest itself; the host only reads the marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Device accepted the (valid) input and behaved correctly.
    Pass,
    /// Device correctly rejected the malformed input and stayed alive.
    Reject,
    /// Device absent or precondition unmet; the test did not run.
    Skip,
    /// Device accepted malformed input / misbehaved.
    Fail,
    /// Device stopped responding after the malformed input.
    Wedged,
    /// Known-failing test that failed as expected (guest-side xfail).
    Xfail,
    /// Known-failing test that unexpectedly passed (guest-side xfail).
    Xpass,
}

impl Verdict {
    fn from_tag(tag: &str) -> Option<Self> {
        Some(match tag {
            "PASS" => Verdict::Pass,
            "REJECT" => Verdict::Reject,
            "SKIP" => Verdict::Skip,
            "FAIL" => Verdict::Fail,
            "WEDGED" => Verdict::Wedged,
            "XFAIL" => Verdict::Xfail,
            "XPASS" => Verdict::Xpass,
            _ => return None,
        })
    }

    /// Whether this verdict is a "good" outcome (the device model behaved
    /// correctly or it is a guest-side xfail/xpass — none of which indicate an
    /// OpenVMM device-model bug).
    ///
    /// Note `Verdict::Skip` is deliberately **not** good: on the kitchen-sink VM
    /// a skip means the device was absent or a precondition was unmet, so the
    /// test exercised nothing. That is always a failure; tests for devices we do
    /// not attach are `#[ignore]`d up front (see [`crate::supported_devices`]).
    pub fn is_good(self) -> bool {
        matches!(
            self,
            Verdict::Pass | Verdict::Reject | Verdict::Xfail | Verdict::Xpass
        )
    }
}

/// The result of scanning a serial log for a single test's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictScan {
    /// The test's own `[TAG] <name>` marker was found.
    Found(Verdict),
    /// No villain markers appeared at all (guest never got far enough).
    NoMarkers,
    /// Other villain markers were present, but not this test's line
    /// (device wedged mid-test and never printed a verdict).
    MarkerMissing,
}

/// Scan a captured serial console log for `name`'s verdict marker.
///
/// Villain prints one `[<TAG>] <name>` line per test. A test may share the log
/// with unrelated boot output; we match the exact test name.
pub fn scan_verdict(log: &str, name: &str) -> VerdictScan {
    let mut saw_any_marker = false;
    for line in log.lines() {
        let line = line.trim();
        // Markers look like "[PASS] blk.split.bad_desc". Find the closing
        // bracket and split into tag + remainder.
        let Some(rest) = line.strip_prefix('[') else {
            continue;
        };
        let Some((tag, remainder)) = rest.split_once(']') else {
            continue;
        };
        let Some(verdict) = Verdict::from_tag(tag) else {
            continue;
        };
        saw_any_marker = true;
        // The name is the first whitespace-delimited token after the tag.
        // Some markers carry a trailing reason, e.g.
        // "[SKIP] D0001 (no device 0x1063)", so match only the first token
        // rather than the whole remainder. Villain test names never contain
        // whitespace (see `tests.tsv`).
        if remainder.split_whitespace().next() == Some(name) {
            return VerdictScan::Found(verdict);
        }
    }
    if saw_any_marker {
        VerdictScan::MarkerMissing
    } else {
        VerdictScan::NoMarkers
    }
}

/// Turn a scan result into a pass/fail outcome for one villain test.
///
/// Fast-fail rule: a `SKIP` means the device was absent or a precondition was
/// unmet, so the test exercised nothing — always a **failure** (an unexercised
/// test must not masquerade as a pass). Tests for devices the kitchen-sink VM
/// does not attach are `#[ignore]`d up front (see [`crate::supported_devices`])
/// so they never reach here in a normal run; force-running them with
/// `--run-ignored` correctly reports the absent-device skip as a failure.
///
/// Otherwise [`Verdict::is_good`] decides, and a missing/absent marker (the
/// guest wedged or never booted) is a failure.
pub fn evaluate(scan: VerdictScan) -> anyhow::Result<()> {
    match scan {
        VerdictScan::Found(Verdict::Skip) => anyhow::bail!(
            "SKIP: the device was absent or a precondition was unmet, so this test \
             exercised nothing. On the kitchen-sink VM a skip means a device we meant \
             to attach silently wasn't (a harness/config bug); if the device is \
             deliberately not attached, the test should be ignored via \
             supported_devices::SUPPORTED_DEVICE_IDS"
        ),
        VerdictScan::Found(v) => {
            if v.is_good() {
                Ok(())
            } else {
                anyhow::bail!("{v:?}")
            }
        }
        VerdictScan::MarkerMissing => {
            anyhow::bail!("WEDGED (no verdict marker for this test)")
        }
        VerdictScan::NoMarkers => {
            anyhow::bail!("FAIL (guest emitted no villain markers)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Use the tracing-aware `#[test]` so `tracing` output appears in test logs
    // (repo convention; see .github/copilot-instructions.md).
    use test_with_tracing::test;

    #[test]
    fn evaluate_skip_is_always_a_failure() {
        // SKIP means nothing was exercised. Unsupported-device tests are ignored
        // up front; anything that actually runs and skips is a failure.
        assert!(evaluate(VerdictScan::Found(Verdict::Skip)).is_err());
    }

    #[test]
    fn evaluate_good_and_bad_verdicts() {
        assert!(evaluate(VerdictScan::Found(Verdict::Pass)).is_ok());
        assert!(evaluate(VerdictScan::Found(Verdict::Reject)).is_ok());
        assert!(evaluate(VerdictScan::Found(Verdict::Xfail)).is_ok());
        assert!(evaluate(VerdictScan::Found(Verdict::Xpass)).is_ok());
        assert!(evaluate(VerdictScan::Found(Verdict::Fail)).is_err());
        assert!(evaluate(VerdictScan::Found(Verdict::Wedged)).is_err());
        assert!(evaluate(VerdictScan::MarkerMissing).is_err());
        assert!(evaluate(VerdictScan::NoMarkers).is_err());
    }

    #[test]
    fn scan_finds_exact_marker() {
        let log = "\
[vv] virtio-villain
[SKIP] blk.other
[PASS] blk.split.bad_desc
";
        assert_eq!(
            scan_verdict(log, "blk.split.bad_desc"),
            VerdictScan::Found(Verdict::Pass)
        );
        assert_eq!(
            scan_verdict(log, "blk.other"),
            VerdictScan::Found(Verdict::Skip)
        );
    }

    #[test]
    fn scan_marker_with_trailing_reason() {
        // Villain appends a reason to some SKIP markers; the name is still the
        // first token (bin/init.c: "[SKIP] %s (no device 0x%04x)").
        let log = "[vv] virtio-villain\n[SKIP] D0001 (no device 0x1063)\n";
        assert_eq!(
            scan_verdict(log, "D0001"),
            VerdictScan::Found(Verdict::Skip)
        );
        // A name that is a prefix of the marked test must not match.
        assert_eq!(scan_verdict(log, "D000"), VerdictScan::MarkerMissing);
    }

    #[test]
    fn scan_missing_vs_no_markers() {
        assert_eq!(scan_verdict("[FAIL] a\n", "b"), VerdictScan::MarkerMissing);
        assert_eq!(scan_verdict("booting...\n", "b"), VerdictScan::NoMarkers);
    }

    #[test]
    fn parse_tsv_row() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tests.tsv");
        std::fs::write(
            &p,
            "blk.split.bad_desc\tvalidates descriptor\t1.2\t2.6.5\t0x0002\t0\t0x0000000000000000\t1\n",
        )
        .unwrap();
        let tests = parse_tsv(&p).unwrap();
        assert_eq!(tests.len(), 1);
        assert_eq!(tests[0].name, "blk.split.bad_desc");
        assert_eq!(tests[0].device_id, 0x0002);
    }

    #[test]
    fn parse_tsv_rejects_bad_device_id() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tests.tsv");
        // device_id column present but not valid 0x-prefixed hex.
        std::fs::write(&p, "t\tdesc\t1.2\t2.6.5\tnope\t0\t0\t1\n").unwrap();
        let err = parse_tsv(&p).unwrap_err();
        assert!(
            err.to_string().contains("invalid device_id"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn parse_tsv_rejects_truncated_row() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tests.tsv");
        // Only name + desc; device_id column missing entirely.
        std::fs::write(&p, "t\tdesc\n").unwrap();
        let err = parse_tsv(&p).unwrap_err();
        assert!(
            err.to_string().contains("missing device_id"),
            "unexpected error: {err:#}"
        );
    }
}
