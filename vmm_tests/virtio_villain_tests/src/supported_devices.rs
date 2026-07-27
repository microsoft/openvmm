// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Decides which villain tests the "kitchen-sink" VM should `#[ignore]` because
//! they can only `[SKIP]` on our configuration — and, crucially, *never* ignores
//! a test that currently runs and passes.
//!
//! # Why ignore, and why never over-ignore
//!
//! Each villain test either exercises OpenVMM (PASS/FAIL/WEDGED/…) or self-skips
//! because a precondition it needs isn't met on this VM. On the kitchen-sink VM
//! [`crate::villain::evaluate`] treats a `[SKIP]` as a **failure**: a skip means
//! the test exercised nothing, which is how a `#[cfg]` mistake once silently
//! dropped the vsock device. To keep the gate meaningful we therefore *ignore*
//! (don't run) the tests we can predict will skip.
//!
//! The hard rule is **never ignore a test that currently passes**. If we ignored
//! a passing test and someone later removed the capability it exercises, the test
//! would silently start skipping and no one would notice — exactly the regression
//! this suite exists to catch. So [`expected_skip`] is built and validated to
//! ignore only tests that genuinely skip today.
//!
//! # How the decision is made: metadata by default, exceptions where it's wrong
//!
//! The default rule is a **metadata pre-check**. Villain's `tests.tsv` records,
//! per test, the virtio feature bits (`required_features`) and virtqueue count
//! (`min_queues`) it needs. A test is expected to skip when it targets a device
//! we attach but asks for a feature we don't offer or more queues than we expose
//! (see [`DeviceCaps`]). This is derived directly from the device models, so it
//! tracks reality as the models change.
//!
//! Villain's metadata is imperfect, so a small, explicit set of exceptions
//! overrides the default rule. Every exception is a documented deviation with a
//! `TODO` to follow up on *why* villain's metadata didn't match:
//!
//! * [`FORCE_RUN`] — the pre-check predicts a skip but the test actually **passes**
//!   (its body only exercises the feature when negotiated, and passes vacuously
//!   otherwise). These must keep running so a real regression surfaces.
//! * [`FORCE_IGNORE`] — the pre-check predicts the test can run but it self-skips
//!   at runtime because villain's `tests.tsv` *understates* the precondition
//!   (e.g. a multiqueue test registered with `min_queues = 0`).
//!
//! Two structural rules round it out: tests for a device we don't attach can only
//! skip, and PCI-transport tests registered with `device_id 0` can never resolve
//! a device in villain's harness (a villain bug — see [`expected_skip`]).
//!
//! Known *product* failures (real device-model bugs) are handled separately in
//! [`crate::known_failures`]; both are OR'd together at trial construction.

/// A virtio feature bit, as a mask.
const fn feature_bit(n: u32) -> u64 {
    1u64 << n
}

/// Transport-common feature bits OpenVMM's virtio transport offers for **every**
/// device, on top of whatever the device model itself advertises. The transport
/// unconditionally turns these on (see
/// `vm/devices/virtio/virtio/src/transport/core.rs`, `with_version_1(true)` and
/// `with_access_platform(true)`), so they must be included when deciding whether
/// a required feature is offered.
const COMMON_FEATURES: u64 = feature_bit(32) // VIRTIO_F_VERSION_1
    | feature_bit(33); // VIRTIO_F_ACCESS_PLATFORM

/// The virtio capabilities the kitchen-sink VM exposes for one device.
pub struct DeviceCaps {
    /// Device id (`0x1040 + virtio_device_type`).
    pub device_id: u16,
    /// Device-specific feature bits the model advertises via its `traits()`
    /// (the transport-common bits in [`COMMON_FEATURES`] are added by
    /// [`DeviceCaps::offered_features`], not stored here).
    device_features: u64,
    /// Number of virtqueues the device exposes.
    pub num_queues: u16,
}

impl DeviceCaps {
    /// The full set of feature bits negotiable on this device, i.e. the device's
    /// own bits plus the transport-common [`COMMON_FEATURES`].
    pub fn offered_features(&self) -> u64 {
        self.device_features | COMMON_FEATURES
    }
}

/// Per-device capabilities exposed by [`crate::run`]'s `attach_kitchen_sink`.
///
/// Keep in sync with the device models. The `device_features` masks below are
/// the device-specific bits each model advertises from its `traits()`
/// implementation (ring features included, transport-common bits excluded):
///
/// * net     `vm/devices/net/virtio_net/src/lib.rs` (`traits`)
/// * block   `vm/devices/virtio/virtio_blk/src/lib.rs` (`traits`)
/// * console `vm/devices/virtio/virtio_console/src/lib.rs` (`traits`)
/// * rng     `vm/devices/virtio/virtio_rng/src/lib.rs` (`traits`)
/// * vsock   `vm/devices/virtio/virtio_vsock/src/lib.rs` (`traits`)
/// * fs      `vm/devices/virtio/virtiofs/src/virtio.rs` (`traits`)
/// * pmem    `vm/devices/virtio/virtio_pmem/src/lib.rs` (`traits`)
pub const DEVICE_CAPS: &[DeviceCaps] = &[
    DeviceCaps {
        device_id: 0x1041, // network
        device_features: 0x0100_000C_3001_1823,
        num_queues: 2,
    },
    DeviceCaps {
        device_id: 0x1042, // block
        device_features: 0x0000_0004_3000_2644,
        num_queues: 1,
    },
    DeviceCaps {
        device_id: 0x1043, // console
        device_features: 0x0000_0004_3000_0001,
        num_queues: 2,
    },
    DeviceCaps {
        device_id: 0x1044, // entropy (rng)
        device_features: 0x0000_0004_3000_0000,
        num_queues: 1,
    },
    DeviceCaps {
        device_id: 0x1053, // vsock (socket)
        device_features: 0x0000_0000_0000_0005,
        num_queues: 3,
    },
    DeviceCaps {
        device_id: 0x105a, // fs (virtio-fs)
        device_features: 0x0000_0004_3000_0000,
        num_queues: 3,
    },
    DeviceCaps {
        device_id: 0x105b, // pmem
        device_features: 0x0000_0004_3000_0000,
        num_queues: 1,
    },
];

/// The capabilities for `device_id`, if the kitchen-sink VM attaches it.
pub fn device_caps(device_id: u16) -> Option<&'static DeviceCaps> {
    DEVICE_CAPS.iter().find(|c| c.device_id == device_id)
}

/// A villain test whose metadata pre-check verdict we deliberately override,
/// with the reason and a follow-up to reconcile villain's metadata.
pub struct Exception {
    /// The villain test name (`tests.tsv` column 1 / `vv.test=<name>`).
    pub name: &'static str,
    /// Why the pre-check is wrong for this test, and what to follow up on.
    pub reason: &'static str,
}

/// Tests the metadata pre-check predicts will skip, but which actually **pass**:
/// their body only exercises the declared feature when it is negotiated and
/// passes vacuously otherwise. They must keep running so a genuine regression
/// (the test starting to fail) is caught rather than hidden behind `#[ignore]`.
pub const FORCE_RUN: &[Exception] = &[Exception {
    name: "N0161",
    reason: "tests.tsv declares VIRTIO_NET_F_RSC_EXT (bit 61), which OpenVMM's \
             net device does not offer, so the pre-check would ignore it. But \
             the body only validates a coalescing report *if* the device sets \
             RSC_INFO; with RSC_EXT un-negotiated it never does and the test \
             passes. Keep running it. TODO(villain): mark RSC_EXT optional in \
             tests.tsv so the metadata matches the test's actual behavior.",
}];

/// Tests the metadata pre-check predicts can run, but which self-`[SKIP]` at
/// runtime because villain's `tests.tsv` understates the precondition they check
/// for. Ignored with the specific reason; each carries a `TODO(villain)` to fix
/// the upstream metadata so it can be dropped from this hand-list.
pub const FORCE_IGNORE: &[Exception] = &[
    // --- Multiqueue: self-SKIP unless the device exposes >= 2 (or 3) virtqueues,
    // but tests.tsv records min_queues = 0. OpenVMM's block exposes 1 request
    // queue and net exposes 2 (no control/RSS queues). TODO(villain): record the
    // real min_queues for these.
    Exception {
        name: "B0044",
        reason: "self-SKIPs unless block exposes >= 2 virtqueues (nq < 2); \
                 tests.tsv min_queues = 0 understates it. TODO(villain): set \
                 min_queues.",
    },
    Exception {
        name: "B0054",
        reason: "self-SKIPs unless block exposes >= 2 virtqueues (nq < 2); \
                 tests.tsv min_queues = 0 understates it. TODO(villain): set \
                 min_queues.",
    },
    Exception {
        name: "B0060",
        reason: "self-SKIPs unless block exposes >= 2 virtqueues (nq < 2); \
                 tests.tsv min_queues = 0 understates it. TODO(villain): set \
                 min_queues.",
    },
    Exception {
        name: "B0080",
        reason: "self-SKIPs unless block exposes >= 2 virtqueues (nq < 2); \
                 tests.tsv min_queues = 0 understates it. TODO(villain): set \
                 min_queues.",
    },
    Exception {
        name: "B0114",
        reason: "self-SKIPs unless block exposes >= 2 virtqueues (nq < 2); \
                 tests.tsv min_queues = 0 understates it. TODO(villain): set \
                 min_queues.",
    },
    Exception {
        name: "B0131",
        reason: "self-SKIPs unless block exposes >= 2 virtqueues (nq < 2); \
                 tests.tsv min_queues = 0 understates it. TODO(villain): set \
                 min_queues.",
    },
    Exception {
        name: "T0081",
        reason: "block transport test self-SKIPs unless >= 2 virtqueues \
                 (nq < 2); tests.tsv min_queues = 0 understates it. \
                 TODO(villain): set min_queues.",
    },
    Exception {
        name: "T0083",
        reason: "block transport test self-SKIPs unless >= 2 virtqueues \
                 (nq < 2); tests.tsv min_queues = 0 understates it. \
                 TODO(villain): set min_queues.",
    },
    Exception {
        name: "N0051",
        reason: "self-SKIPs without VIRTIO_NET_F_HASH_REPORT or VIRTIO_NET_F_RSS \
                 and >= 2 queues; OpenVMM net offers neither feature. \
                 TODO(villain): record required_features/min_queues.",
    },
    Exception {
        name: "N0064",
        reason: "self-SKIPs without VIRTIO_NET_F_RSS and >= 3 queues; OpenVMM \
                 net offers neither. TODO(villain): record \
                 required_features/min_queues.",
    },
    Exception {
        name: "N0065",
        reason: "self-SKIPs without VIRTIO_NET_F_RSS and >= 3 queues; OpenVMM \
                 net offers neither. TODO(villain): record \
                 required_features/min_queues.",
    },
    // --- Hot-add / resize / config-change: self-SKIP when the device reports no
    // newly added slot or unchanged capacity. OpenVMM does not hot-add or resize
    // virtio devices at runtime. TODO(villain): gate these behind a capability
    // flag in tests.tsv.
    Exception {
        name: "B0200",
        reason: "hot-add test self-SKIPs when the device reports no new slot \
                 (new_slot == 0); OpenVMM does not hot-add virtio devices. \
                 TODO(villain): gate on a hot-plug capability.",
    },
    Exception {
        name: "B0201",
        reason: "hot-add test self-SKIPs when the device reports no new slot \
                 (new_slot == 0); OpenVMM does not hot-add virtio devices. \
                 TODO(villain): gate on a hot-plug capability.",
    },
    Exception {
        name: "B0202",
        reason: "block-resize test self-SKIPs when capacity is unchanged; \
                 OpenVMM's backing capacity is fixed. TODO(villain): gate on a \
                 resize capability.",
    },
    Exception {
        name: "E0200",
        reason: "hot-add test self-SKIPs when the device reports no new slot \
                 (new_slot == 0); OpenVMM does not hot-add virtio devices. \
                 TODO(villain): gate on a hot-plug capability.",
    },
    Exception {
        name: "N0200",
        reason: "hot-add test self-SKIPs when the device reports no new slot \
                 (new_slot == 0); OpenVMM does not hot-add virtio devices. \
                 TODO(villain): gate on a hot-plug capability.",
    },
    Exception {
        name: "V0200",
        reason: "hot-add test self-SKIPs when the device reports no new slot \
                 (new_slot == 0); OpenVMM does not hot-add virtio devices. \
                 TODO(villain): gate on a hot-plug capability.",
    },
    // --- Console MULTIPORT / EMERG_WRITE: self-SKIP without the console feature
    // they exercise. OpenVMM's console is single-port and offers neither.
    // TODO(villain): record required_features for these.
    Exception {
        name: "C0030",
        reason: "self-SKIPs without VIRTIO_CONSOLE_F_EMERG_WRITE, which OpenVMM's \
                 console does not offer. TODO(villain): record required_features.",
    },
    Exception {
        name: "C0032",
        reason: "self-SKIPs without VIRTIO_CONSOLE_F_MULTIPORT; OpenVMM's console \
                 is single-port. TODO(villain): record required_features.",
    },
    Exception {
        name: "C0033",
        reason: "self-SKIPs without VIRTIO_CONSOLE_F_MULTIPORT; OpenVMM's console \
                 is single-port. TODO(villain): record required_features.",
    },
    Exception {
        name: "C0034",
        reason: "self-SKIPs without VIRTIO_CONSOLE_F_MULTIPORT; OpenVMM's console \
                 is single-port. TODO(villain): record required_features.",
    },
    Exception {
        name: "C0038",
        reason: "self-SKIPs without VIRTIO_CONSOLE_F_MULTIPORT; OpenVMM's console \
                 is single-port. TODO(villain): record required_features.",
    },
    Exception {
        name: "C0039",
        reason: "self-SKIPs without VIRTIO_CONSOLE_F_MULTIPORT; OpenVMM's console \
                 is single-port. TODO(villain): record required_features.",
    },
    Exception {
        name: "C0040",
        reason: "self-SKIPs without VIRTIO_CONSOLE_F_MULTIPORT; OpenVMM's console \
                 is single-port. TODO(villain): record required_features.",
    },
    Exception {
        name: "C0041",
        reason: "self-SKIPs without VIRTIO_CONSOLE_F_MULTIPORT; OpenVMM's console \
                 is single-port. TODO(villain): record required_features.",
    },
    // --- VIRTIO_F_NOTIFICATION_DATA: self-SKIP without the feature, which OpenVMM
    // does not negotiate for any device. TODO(villain): record required_features.
    Exception {
        name: "F0036",
        reason: "self-SKIPs without VIRTIO_F_NOTIFICATION_DATA, which OpenVMM does \
                 not negotiate. TODO(villain): record required_features.",
    },
    Exception {
        name: "T0098",
        reason: "self-SKIPs without VIRTIO_F_NOTIFICATION_DATA, which OpenVMM does \
                 not negotiate. TODO(villain): record required_features.",
    },
    Exception {
        name: "T0101",
        reason: "self-SKIPs without VIRTIO_F_NOTIFICATION_DATA, which OpenVMM does \
                 not negotiate. TODO(villain): record required_features.",
    },
    Exception {
        name: "T0102",
        reason: "self-SKIPs without VIRTIO_F_NOTIFICATION_DATA, which OpenVMM does \
                 not negotiate. TODO(villain): record required_features.",
    },
    Exception {
        name: "T0103",
        reason: "self-SKIPs without VIRTIO_F_NOTIFICATION_DATA, which OpenVMM does \
                 not negotiate. TODO(villain): record required_features.",
    },
    // --- MMIO feature self-skips (previously hand-listed in known_failures as
    // M0031/M0032). tests.tsv records reqf = 0 for these MMIO tests, so the
    // pre-check can't catch them. TODO(villain): record required_features.
    Exception {
        name: "M0031",
        reason: "MMIO QueueNotify-with-notification-data test self-SKIPs without \
                 VIRTIO_F_NOTIFICATION_DATA, which OpenVMM does not negotiate. \
                 TODO(villain): record required_features (tsv reqf = 0).",
    },
    Exception {
        name: "M0032",
        reason: "MMIO config-change-interrupt test self-SKIPs when the device \
                 raises no config-change interrupt; OpenVMM's kitchen-sink \
                 devices don't. TODO(villain): gate on a config-change capability \
                 (tsv reqf = 0).",
    },
    // --- Net offloads / stats: self-SKIP without the specific offload/stats
    // feature, none of which OpenVMM's net device offers. TODO(villain): record
    // required_features.
    Exception {
        name: "N0123",
        reason: "self-SKIPs without VIRTIO_NET_F_HOST_ECN; OpenVMM net does not \
                 offer it. TODO(villain): record required_features.",
    },
    Exception {
        name: "N0124",
        reason: "self-SKIPs without VIRTIO_NET_F_HOST_UFO (deprecated); OpenVMM \
                 net does not offer it. TODO(villain): record required_features.",
    },
    Exception {
        name: "N0125",
        reason: "self-SKIPs without VIRTIO_NET_F_DEVICE_STATS; OpenVMM net does \
                 not offer it. TODO(villain): record required_features.",
    },
    Exception {
        name: "N0126",
        reason: "self-SKIPs without VIRTIO_NET_F_GUEST_HDRLEN; OpenVMM net does \
                 not offer it. TODO(villain): record required_features.",
    },
    Exception {
        name: "N0130",
        reason: "self-SKIPs when no vIOMMU device is present to translate under \
                 VIRTIO_F_ACCESS_PLATFORM; the kitchen-sink VM attaches no \
                 vIOMMU. TODO(villain): gate on a vIOMMU capability.",
    },
    // --- Virtio admin virtqueue: self-SKIP when the admin-queue registers /
    // feature are absent. OpenVMM does not implement the admin virtqueue.
    // TODO(villain): record required_features / an admin-vq capability.
    Exception {
        name: "PCI0076",
        reason: "self-SKIPs when the PCI common-config admin-queue registers are \
                 absent (common_length < 0x48); OpenVMM has no admin virtqueue. \
                 TODO(villain): gate on an admin-vq capability.",
    },
    Exception {
        name: "PCI0077",
        reason: "self-SKIPs when the admin virtqueue is absent (common_length < \
                 0x48 / admin_num == 0); OpenVMM has no admin virtqueue. \
                 TODO(villain): gate on an admin-vq capability.",
    },
    Exception {
        name: "PCI0078",
        reason: "self-SKIPs when the admin virtqueue is absent (common_length < \
                 0x48 / admin_num == 0); OpenVMM has no admin virtqueue. \
                 TODO(villain): gate on an admin-vq capability.",
    },
    Exception {
        name: "PCI0079",
        reason: "self-SKIPs when the admin virtqueue is absent (common_length < \
                 0x48 / admin_num == 0); OpenVMM has no admin virtqueue. \
                 TODO(villain): gate on an admin-vq capability.",
    },
    Exception {
        name: "PCI0117",
        reason: "self-SKIPs without VIRTIO_F_ADMIN_VQ; OpenVMM does not implement \
                 the admin virtqueue. TODO(villain): record required_features.",
    },
    // --- Legacy PCI I/O BAR: self-SKIP when the legacy I/O-port resource can't be
    // opened (fd < 0). OpenVMM's modern virtio-pci exposes no legacy I/O BAR.
    // TODO(villain): gate these on a legacy-transport capability.
    Exception {
        name: "PCI0026",
        reason: "self-SKIPs when the legacy PCI I/O-port resource can't be opened \
                 (fd < 0); OpenVMM's modern virtio-pci has no legacy I/O BAR. \
                 TODO(villain): gate on a legacy-transport capability.",
    },
    Exception {
        name: "PCI0042",
        reason: "self-SKIPs when the legacy PCI I/O-port resource can't be opened \
                 (fd < 0); OpenVMM's modern virtio-pci has no legacy I/O BAR. \
                 TODO(villain): gate on a legacy-transport capability.",
    },
    Exception {
        name: "PCI0043",
        reason: "self-SKIPs when the legacy PCI I/O-port resource can't be opened \
                 (fd < 0); OpenVMM's modern virtio-pci has no legacy I/O BAR. \
                 TODO(villain): gate on a legacy-transport capability.",
    },
    Exception {
        name: "PCI0044",
        reason: "self-SKIPs when the legacy PCI I/O-port resource can't be opened \
                 (fd < 0); OpenVMM's modern virtio-pci has no legacy I/O BAR. \
                 TODO(villain): gate on a legacy-transport capability.",
    },
    Exception {
        name: "PCI0045",
        reason: "self-SKIPs when the legacy PCI I/O-port resource can't be opened \
                 (fd < 0); OpenVMM's modern virtio-pci has no legacy I/O BAR. \
                 TODO(villain): gate on a legacy-transport capability.",
    },
    Exception {
        name: "PCI0046",
        reason: "self-SKIPs when the legacy PCI I/O-port resource can't be opened \
                 (fd < 0); OpenVMM's modern virtio-pci has no legacy I/O BAR. \
                 TODO(villain): gate on a legacy-transport capability.",
    },
    Exception {
        name: "PCI0047",
        reason: "self-SKIPs when the legacy PCI I/O-port resource can't be opened \
                 (fd < 0); OpenVMM's modern virtio-pci has no legacy I/O BAR. \
                 TODO(villain): gate on a legacy-transport capability.",
    },
    Exception {
        name: "PCI0048",
        reason: "self-SKIPs when the legacy PCI I/O-port resource can't be opened \
                 (fd < 0); OpenVMM's modern virtio-pci has no legacy I/O BAR. \
                 TODO(villain): gate on a legacy-transport capability.",
    },
    Exception {
        name: "PCI0049",
        reason: "self-SKIPs when the legacy PCI I/O-port resource can't be opened \
                 (fd < 0); OpenVMM's modern virtio-pci has no legacy I/O BAR. \
                 TODO(villain): gate on a legacy-transport capability.",
    },
    // --- Other PCI-transport preconditions OpenVMM's layout doesn't satisfy.
    Exception {
        name: "PCI0070",
        reason: "per-vector MSI-X mask test self-SKIPs when it can't find the \
                 MSI-X table entry / queue-0 vector binding it needs; OpenVMM's \
                 virtio-pci MSI-X layout doesn't satisfy the precondition. \
                 TODO(villain): investigate the MSI-X table discovery.",
    },
    Exception {
        name: "PCI0072",
        reason: "self-SKIPs when notify_off_multiplier reads 0; OpenVMM reports 0 \
                 (a single shared notify register). TODO(villain): allow a zero \
                 multiplier layout.",
    },
];

/// Look up an [`Exception`] by name in `list`.
fn find_exception(list: &'static [Exception], name: &str) -> Option<&'static Exception> {
    list.iter().find(|e| e.name == name)
}

/// Whether a villain test should be `#[ignore]`d because it can only `[SKIP]` on
/// the kitchen-sink VM, returning the reason if so. Returns `None` for tests we
/// expect to actually run (and must not hide behind `#[ignore]`).
///
/// The decision order is:
/// 1. [`FORCE_RUN`] override — never ignore a test we know passes.
/// 2. [`FORCE_IGNORE`] override — ignore tests whose runtime precondition
///    `tests.tsv` understates.
/// 3. Device-agnostic PCI tests (`device_id == 0`, non-MMIO): villain's
///    `virtio_pci_find(0)` looks for a literal PCI device `0x0000`, which no
///    virtio device is, so these always `[SKIP]` on every hypervisor. This is a
///    villain harness bug (MMIO tests use `virtio_mmio_find`, which finds any
///    device; PCI has no equivalent "any device" path). TODO(villain): give the
///    PCI runner an any-device path for `device_id 0` transport tests.
/// 4. A device we don't attach — the test can only skip.
/// 5. The metadata pre-check: a required feature we don't offer, or more queues
///    than we expose.
pub fn expected_skip(
    name: &str,
    device_id: u16,
    is_mmio: bool,
    required_features: u64,
    min_queues: u32,
) -> Option<&'static str> {
    // 1. Never ignore a test we know runs and passes.
    if find_exception(FORCE_RUN, name).is_some() {
        return None;
    }

    // 2. Ignore tests whose runtime precondition tests.tsv understates.
    if let Some(e) = find_exception(FORCE_IGNORE, name) {
        return Some(e.reason);
    }

    // 3. Non-MMIO transport tests registered with device_id 0 can never resolve a
    // device in villain's PCI runner, so they always skip.
    if device_id == 0 && !is_mmio {
        return Some(
            "villain harness bug: a PCI-transport test registered with device_id \
             0 is looked up via virtio_pci_find(0), which searches for a literal \
             PCI device 0x0000 that no virtio device is, so it always SKIPs on \
             every hypervisor. TODO(villain): add an any-device path to the PCI \
             runner (MMIO already has one via virtio_mmio_find).",
        );
    }

    // Device-agnostic tests (device_id 0) that reach here are MMIO transport
    // tests, which run against whatever device virtio_mmio_find locates; they are
    // not device-gated, so the pre-check below doesn't apply.
    if device_id == 0 {
        return None;
    }

    // 4. A device the kitchen-sink VM doesn't attach can only skip.
    let Some(caps) = device_caps(device_id) else {
        return Some("device is not attached by the kitchen-sink VM");
    };

    // 5. Metadata pre-check against what OpenVMM actually offers.
    if required_features & !caps.offered_features() != 0 {
        return Some(
            "requires a virtio feature OpenVMM's device model does not offer \
             (required_features has bits outside DeviceCaps::offered_features)",
        );
    }
    if min_queues > u32::from(caps.num_queues) {
        return Some(
            "requires more virtqueues than OpenVMM's device model exposes \
             (min_queues > DeviceCaps::num_queues)",
        );
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn attached_devices_have_caps_and_are_not_skipped() {
        for caps in DEVICE_CAPS {
            // A no-requirement test on an attached device must run.
            assert!(
                expected_skip("X0000", caps.device_id, false, 0, 0).is_none(),
                "0x{:04x} is attached",
                caps.device_id
            );
        }
    }

    #[test]
    fn common_features_are_always_offered() {
        // VERSION_1 (32) and ACCESS_PLATFORM (33) are offered for every device
        // even though the models don't list them (the transport adds them).
        for caps in DEVICE_CAPS {
            assert_eq!(caps.offered_features() & COMMON_FEATURES, COMMON_FEATURES);
        }
    }

    #[test]
    fn absent_device_is_expected_to_skip() {
        // 0x1063 is present in tests.tsv but not attached by the kitchen sink.
        assert!(expected_skip("D0001", 0x1063, false, 0, 0).is_some());
    }

    #[test]
    fn device_agnostic_mmio_tests_run() {
        assert!(expected_skip("M0001", 0x0000, true, 0, 0).is_none());
    }

    #[test]
    fn device_agnostic_pci_tests_are_skipped() {
        // device_id 0 + non-MMIO: villain's virtio_pci_find(0) never resolves.
        assert!(expected_skip("PCI0050", 0x0000, false, 0, 0).is_some());
    }

    #[test]
    fn missing_feature_is_expected_to_skip() {
        // Require a bit block doesn't offer (VIRTIO_NET_F_RSS-ish high bit).
        assert!(expected_skip("X", 0x1042, false, feature_bit(60), 0).is_some());
    }

    #[test]
    fn too_many_queues_is_expected_to_skip() {
        // Block exposes 1 queue; a test needing 2 must be ignored.
        assert!(expected_skip("X", 0x1042, false, 0, 2).is_some());
    }

    #[test]
    fn force_run_overrides_missing_feature() {
        // N0161 declares RSC_EXT (bit 61), which net doesn't offer, yet it must
        // still run (it passes vacuously).
        assert!(expected_skip("N0161", 0x1041, false, feature_bit(61), 0).is_none());
    }

    #[test]
    fn force_ignore_overrides_pre_check() {
        // B0044 declares min_queues = 0 but self-skips at runtime.
        assert!(expected_skip("B0044", 0x1042, false, 0, 0).is_some());
    }

    #[test]
    fn exception_names_are_unique_and_disjoint() {
        // A name must not appear twice, nor in both lists (contradictory).
        let mut all: Vec<&str> = FORCE_RUN
            .iter()
            .chain(FORCE_IGNORE)
            .map(|e| e.name)
            .collect();
        let count = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), count, "duplicate or overlapping exception names");
    }
}
