// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::resolve_vpci_relay;
use test_with_tracing::test;
use virt::IsolationType;

#[test]
fn vpci_relay_default() {
    // (isolation, VMBus relay, MMIO hypercalls, MMIO window, enabled)
    let cases = [
        (IsolationType::None, true, true, true, false),
        (IsolationType::Snp, true, true, true, true),
        (IsolationType::Snp, false, true, true, false),
        (IsolationType::Vbs, true, true, true, true),
        (IsolationType::Vbs, true, false, true, false),
        (IsolationType::Vbs, false, true, true, false),
        // Without an MMIO window the relay is skipped instead of failing
        // startup.
        (IsolationType::Vbs, true, true, false, false),
        (IsolationType::Snp, true, true, false, false),
    ];
    for (isolation, vmbus_relay, mmio_hypercalls, window, expected) in cases {
        assert_eq!(
            resolve_vpci_relay(None, isolation, vmbus_relay, mmio_hypercalls, window).unwrap(),
            expected,
            "{isolation:?} vmbus_relay={vmbus_relay} mmio_hypercalls={mmio_hypercalls} window={window}"
        );
    }
}

#[test]
fn vpci_relay_explicitly_enabled() {
    assert!(resolve_vpci_relay(Some(true), IsolationType::None, true, false, true).unwrap());
    assert!(resolve_vpci_relay(Some(true), IsolationType::Vbs, true, true, true).unwrap());
    // Combinations where relayed devices can't work are rejected.
    assert!(resolve_vpci_relay(Some(true), IsolationType::Vbs, true, false, true).is_err());
    assert!(resolve_vpci_relay(Some(true), IsolationType::Snp, false, true, true).is_err());
    assert!(resolve_vpci_relay(Some(true), IsolationType::Snp, true, true, false).is_err());
}

#[test]
fn vpci_relay_explicitly_disabled() {
    assert!(!resolve_vpci_relay(Some(false), IsolationType::Snp, true, true, true).unwrap());
}
