// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The set of virtio device IDs the "kitchen-sink" VM attaches, used to decide
//! which villain tests are relevant to run.
//!
//! # Unsupported-device tests are ignored; unexpected skips are failures
//!
//! Each villain test targets one virtio device. The "kitchen-sink" VM attaches
//! every device we support ([`SUPPORTED_DEVICE_IDS`]); a test for a device we
//! *don't* attach could only self-`[SKIP]`, so booting a VM for each such test
//! is wasted work. Those tests ([`skip_expected`] is true) are marked
//! `#[ignore]` at trial-construction time, exactly like [`crate::known_failures`]:
//! they never boot a VM and report as *ignored*, not as false passes.
//!
//! For a test whose device we *do* attach, a `[SKIP]` is suspicious — it means a
//! precondition we expected to hold didn't, or a device we meant to attach
//! silently wasn't (this is how a `#[cfg]` mistake once dropped the vsock
//! device). So [`crate::villain::evaluate`] treats a `SKIP` as a **failure**,
//! always. Force-running the ignored tests with `--run-ignored` therefore
//! reports their absent-device skips as failures too — which is correct: you
//! asked to actually run them and the device isn't there.
//!
//! # Device IDs rather than a per-test list
//!
//! Villain has ~1400 tests but only a handful of device classes. Rather than
//! enumerate every test to ignore, we enumerate the device IDs we *do* attach;
//! any test whose target device ID is not in that set is ignored. This is far
//! less to maintain and is derived directly from what the VM attaches.
//!
//! Device-agnostic tests (device ID `0x0000`, e.g. transport-level PCI checks)
//! run regardless of which devices are present, so they are never ignored.

/// The virtio device IDs attached by [`crate::run`]'s `attach_kitchen_sink`.
///
/// Keep in sync with the devices attached there. IDs follow the virtio spec
/// convention `0x1040 + virtio_device_type`.
pub const SUPPORTED_DEVICE_IDS: &[u16] = &[
    0x1041, // network
    0x1042, // block
    0x1043, // console
    0x1044, // entropy (rng)
    0x1053, // vsock (socket)
    0x105a, // fs (virtio-fs)
    0x105b, // pmem
];

/// Whether a villain test targeting `device_id` should be ignored because the
/// kitchen-sink VM does not attach that device (so the test could only `[SKIP]`).
///
/// Device-agnostic tests (`device_id == 0`) are never ignored.
pub fn skip_expected(device_id: u16) -> bool {
    device_id != 0 && !SUPPORTED_DEVICE_IDS.contains(&device_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn attached_devices_are_not_expected_to_skip() {
        for &id in SUPPORTED_DEVICE_IDS {
            assert!(!skip_expected(id), "0x{id:04x} is attached");
        }
    }

    #[test]
    fn absent_device_is_expected_to_skip() {
        // 0x1063 is present in tests.tsv but not attached by the kitchen sink.
        assert!(skip_expected(0x1063));
    }

    #[test]
    fn device_agnostic_tests_never_expected_to_skip() {
        assert!(!skip_expected(0x0000));
    }
}
