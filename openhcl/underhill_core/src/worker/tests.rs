// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::resolve_vpci_relay;
use product_policy::AllowedPciDevice;
use test_with_tracing::test;
use virt::IsolationType;

#[test]
fn vpci_relay_default() {
    let any = [AllowedPciDevice::default()];
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
            resolve_vpci_relay(None, isolation, vmbus_relay, mmio_hypercalls, window, &any)
                .unwrap(),
            expected,
            "{isolation:?} vmbus_relay={vmbus_relay} mmio_hypercalls={mmio_hypercalls} window={window}"
        );
    }
}

#[test]
fn vpci_relay_explicit() {
    let any = [AllowedPciDevice::default()];
    assert!(!resolve_vpci_relay(Some(false), IsolationType::Snp, true, true, true, &any).unwrap());
    assert!(resolve_vpci_relay(Some(true), IsolationType::None, true, false, true, &any).unwrap());
    assert!(resolve_vpci_relay(Some(true), IsolationType::Vbs, true, true, true, &any).unwrap());
    // Combinations where relayed devices can't work are rejected.
    assert!(resolve_vpci_relay(Some(true), IsolationType::Vbs, true, false, true, &any).is_err());
    assert!(resolve_vpci_relay(Some(true), IsolationType::Snp, false, true, true, &any).is_err());
    assert!(resolve_vpci_relay(Some(true), IsolationType::Snp, true, true, false, &any).is_err());
}

/// With an empty allowlist the relay could only reject offers, so it doesn't
/// run by default, and explicitly enabling it is an error. Non-isolated guests
/// don't filter devices, so they don't need an allowlist.
#[test]
fn vpci_relay_needs_an_allowed_device() {
    for isolation in [IsolationType::Snp, IsolationType::Tdx, IsolationType::Vbs] {
        assert!(!resolve_vpci_relay(None, isolation, true, true, true, &[]).unwrap());
        assert!(resolve_vpci_relay(Some(true), isolation, true, true, true, &[]).is_err());
    }
    assert!(resolve_vpci_relay(Some(true), IsolationType::None, true, false, true, &[]).unwrap());
}
