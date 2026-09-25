// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Egress policy tests: frames that the policy denies complete without
//! reaching the endpoint, and the endpoint checks the bytes it transmits.

use super::TestHarness;
use net_backend_resources::egress::EgressPolicy;
use net_backend_resources::egress::EgressPolicyMode;
use net_backend_resources::mac_address::MacAddress;
use pal_async::DefaultDriver;
use pal_async::async_test;
use test_with_tracing::test;

#[async_test]
async fn denied_tx_completes_without_backend_ownership(driver: DefaultDriver) {
    let policy = EgressPolicy::bind(
        std::net::Ipv4Addr::new(10, 0, 0, 2),
        24,
        MacAddress::new([0x52, 0x54, 0, 0, 0, 2]),
        std::net::Ipv4Addr::new(10, 0, 0, 1),
        EgressPolicyMode::TcpEndpoints(vec!["192.0.2.7:443".parse().unwrap()]),
    )
    .unwrap();
    let mut harness = TestHarness::new_with_egress_policy(&driver, Some(policy));
    let handle = harness.enable_and_get_handle().await;

    harness.post_tx_and_signal(0, 64);
    assert_eq!(harness.wait_for_used().await, (0, 0));
    assert!(handle.take_tx_avail_log().is_empty());
}

fn policy_tcp_frame(destination_port: u16) -> Vec<u8> {
    let mut frame = vec![0u8; 14 + 20 + 20];
    frame[6..12].copy_from_slice(&[0x52, 0x54, 0, 0, 0, 2]);
    frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    let ip = &mut frame[14..];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&40u16.to_be_bytes());
    ip[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
    ip[8] = 64;
    ip[9] = 6;
    ip[12..16].copy_from_slice(&[10, 0, 0, 2]);
    ip[16..20].copy_from_slice(&[192, 0, 2, 7]);
    ip[20..22].copy_from_slice(&12345u16.to_be_bytes());
    ip[22..24].copy_from_slice(&destination_port.to_be_bytes());
    ip[32] = 5 << 4;
    let mut sum = ip[..20].chunks_exact(2).fold(0u32, |sum, word| {
        sum + u32::from(u16::from_be_bytes([word[0], word[1]]))
    });
    while sum > u32::from(u16::MAX) {
        sum = (sum & u32::from(u16::MAX)) + (sum >> 16);
    }
    ip[10..12].copy_from_slice(&(!(sum as u16)).to_be_bytes());
    frame
}

fn policy_arp_frame(target: std::net::Ipv4Addr) -> Vec<u8> {
    let guest_mac = [0x52, 0x54, 0, 0, 0, 2];
    let mut frame = vec![0u8; 42];
    frame[..6].fill(0xff);
    frame[6..12].copy_from_slice(&guest_mac);
    frame[12..14].copy_from_slice(&0x0806u16.to_be_bytes());
    frame[14..16].copy_from_slice(&1u16.to_be_bytes());
    frame[16..18].copy_from_slice(&0x0800u16.to_be_bytes());
    frame[18] = 6;
    frame[19] = 4;
    frame[20..22].copy_from_slice(&1u16.to_be_bytes());
    frame[22..28].copy_from_slice(&guest_mac);
    frame[28..32].copy_from_slice(&[10, 0, 0, 2]);
    frame[38..42].copy_from_slice(&target.octets());
    frame
}

fn endpoint_policy() -> EgressPolicy {
    EgressPolicy::bind(
        std::net::Ipv4Addr::new(10, 0, 0, 2),
        24,
        MacAddress::new([0x52, 0x54, 0, 0, 0, 2]),
        std::net::Ipv4Addr::new(10, 0, 0, 1),
        EgressPolicyMode::TcpEndpoints(vec![
            "10.0.0.9:443".parse().unwrap(),
            "192.0.2.7:443".parse().unwrap(),
        ]),
    )
    .unwrap()
}

#[async_test]
async fn endpoint_policy_forwards_only_bound_arp_targets(driver: DefaultDriver) {
    let mut allowed_harness = TestHarness::new_with_egress_policy(&driver, Some(endpoint_policy()));
    let allowed_handle = allowed_harness.enable_and_get_handle().await;
    allowed_harness
        .post_tx_frame_and_signal(0, &policy_arp_frame(std::net::Ipv4Addr::new(10, 0, 0, 9)));
    assert_eq!(allowed_harness.wait_for_used().await, (0, 0));
    assert!(!allowed_handle.take_tx_avail_log().is_empty());

    let mut denied_harness = TestHarness::new_with_egress_policy(&driver, Some(endpoint_policy()));
    let denied_handle = denied_harness.enable_and_get_handle().await;
    denied_harness
        .post_tx_frame_and_signal(0, &policy_arp_frame(std::net::Ipv4Addr::new(10, 0, 0, 10)));
    assert_eq!(denied_harness.wait_for_used().await, (0, 0));
    assert!(denied_handle.take_tx_avail_log().is_empty());
}

#[async_test]
async fn endpoint_policy_checks_transmitted_bytes_after_guest_mutation(driver: DefaultDriver) {
    let policy = EgressPolicy::bind(
        std::net::Ipv4Addr::new(10, 0, 0, 2),
        24,
        MacAddress::new([0x52, 0x54, 0, 0, 0, 2]),
        std::net::Ipv4Addr::new(10, 0, 0, 1),
        EgressPolicyMode::TcpEndpoints(vec!["192.0.2.7:443".parse().unwrap()]),
    )
    .unwrap();
    let mut harness = TestHarness::new_with_egress_policy(&driver, Some(policy));
    let handle = harness.enable_and_get_handle().await;
    handle.replace_next_tx(policy_tcp_frame(80));

    harness.post_tx_frame_and_signal(0, &policy_tcp_frame(443));
    assert_eq!(harness.wait_for_used().await, (0, 0));
    assert!(handle.take_tx_avail_log().is_empty());
}

#[async_test]
async fn endpoint_policy_rechecks_mutated_arp_target_at_backend(driver: DefaultDriver) {
    let mut harness = TestHarness::new_with_egress_policy(&driver, Some(endpoint_policy()));
    let handle = harness.enable_and_get_handle().await;
    handle.replace_next_tx(policy_arp_frame(std::net::Ipv4Addr::new(10, 0, 0, 10)));

    harness.post_tx_frame_and_signal(0, &policy_arp_frame(std::net::Ipv4Addr::new(10, 0, 0, 9)));
    assert_eq!(harness.wait_for_used().await, (0, 0));
    assert!(handle.take_tx_avail_log().is_empty());
}
