// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM static network identity and egress policy.

use crate::Options;
use crate::VmResources;
use crate::cli_args;
use crate::cli_args::EndpointConfigCli;
use net_backend_resources::consomme::static_ipv4::StaticIpv4Config;
use net_backend_resources::consomme::static_ipv6::StaticIpv6Config;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::NetEndpointHandleKind;

const MICROVM_NETWORK_STABLE_ID: &str = "net:microvm0";

#[derive(Clone, Debug)]
pub(super) struct EffectiveMicrovmNetwork {
    pub(super) config: openvmm_defs::microvm::MicrovmNetworkConfig,
    /// The policy that the device installs before any vCPU runs.
    pub(super) policy: net_backend_resources::egress::EgressPolicy,
    /// The policy that the snapshot contract records: `policy`, except at a
    /// restore with `--restore-network-policy`, where `policy` replaces the
    /// template's complete deny-all, which the contract keeps.
    pub(super) contract_policy: net_backend_resources::egress::EgressPolicy,
    pub(super) attachment: openvmm_helpers::snapshot::microvm::SnapshotAttachment,
}

fn microvm_network_attachment() -> openvmm_helpers::snapshot::microvm::SnapshotAttachment {
    openvmm_helpers::snapshot::microvm::SnapshotAttachment {
        stable_id: MICROVM_NETWORK_STABLE_ID.to_owned(),
        kind: "virtio-net".to_owned(),
        required: false,
        reconnect_policy: "recreate-endpoint".to_owned(),
        identity_kind: "user-mode-nat".to_owned(),
        identity: b"consomme".to_vec(),
        length: 0,
        reconnect_timeout_ms: 0,
    }
}

fn microvm_network_from_snapshot(
    saved: &openvmm_helpers::snapshot::microvm::SnapshotMicrovmNetwork,
) -> anyhow::Result<openvmm_defs::microvm::MicrovmNetworkConfig> {
    openvmm_helpers::snapshot::microvm::microvm_network_config_from_snapshot(saved)
}

pub(super) fn effective_microvm_network(
    opt: &Options,
    restore: Option<&openvmm_helpers::snapshot::microvm::SnapshotMachineContract>,
) -> anyhow::Result<Option<EffectiveMicrovmNetwork>> {
    let requested = match opt.net.as_slice() {
        [] => None,
        [network] => match &network.endpoint {
            EndpointConfigCli::Microvm(config) => Some(config.clone()),
            _ => anyhow::bail!("microVM --net was not validated as a static IPv4 identity"),
        },
        _ => anyhow::bail!("microVM permits at most one virtio-net device"),
    };
    let Some(restore) = restore else {
        return requested
            .map(|config| {
                let policy = opt.microvm_egress_policy(&config)?;
                Ok(EffectiveMicrovmNetwork {
                    config,
                    contract_policy: policy.clone(),
                    policy,
                    attachment: microvm_network_attachment(),
                })
            })
            .transpose();
    };
    anyhow::ensure!(
        requested.is_none(),
        "restore takes microVM network addressing from saved state"
    );

    let has_device = restore
        .devices
        .iter()
        .any(|device| device.stable_id == MICROVM_NETWORK_STABLE_ID);
    let saved_attachment = restore
        .attachments
        .iter()
        .find(|attachment| attachment.stable_id == MICROVM_NETWORK_STABLE_ID);
    anyhow::ensure!(
        has_device == restore.microvm_network.is_some() && has_device == saved_attachment.is_some(),
        "snapshot microVM network device, identity, and attachment inventories disagree"
    );
    let Some(saved) = restore.microvm_network.as_ref() else {
        anyhow::ensure!(
            !opt.microvm.restore_network_policy,
            "--restore-network-policy requires a snapshot with a NIC"
        );
        anyhow::ensure!(
            opt.microvm.network_profile.is_none()
                && opt.microvm.net_tap.is_none()
                && opt.microvm.network_egress.is_none()
                && opt.microvm.network_ingress.is_none()
                && opt.microvm.allow_host.is_empty()
                && opt.microvm.block_host.is_empty()
                && opt.microvm.allow_endpoint.is_empty()
                && opt.microvm.network_egress_allow.is_empty()
                && opt.microvm.network_egress_deny.is_empty()
                && opt.microvm.host_loopback.is_none()
                && opt.microvm.network_proxy.is_none()
                && opt.microvm.host_loopback_forward.is_empty(),
            "restore-time network resources cannot be added to a snapshot without a NIC"
        );
        return Ok(None);
    };
    anyhow::ensure!(
        saved.profile == openvmm_defs::microvm::MicrovmNetworkProfile::Portable.as_str(),
        "snapshot microVM network profile is unsupported"
    );
    anyhow::ensure!(
        opt.microvm.network_profile == Some(cli_args::microvm::MicrovmNetworkProfileCli::Portable),
        "networked microVM snapshot restore requires --network-profile portable"
    );
    let config = microvm_network_from_snapshot(saved)?;
    // The restore binds its policy to the captured identity, which a template
    // keeps whatever policy replaces its own.
    let policy = opt.microvm_egress_policy(&config)?;
    let contract_policy = if opt.microvm.restore_network_policy {
        openvmm_helpers::snapshot::microvm::validate_microvm_network_replacement(saved, &policy)?
    } else {
        openvmm_helpers::snapshot::microvm::validate_microvm_network_policy(saved, &policy)?;
        policy.clone()
    };
    let attachment = microvm_network_attachment();
    anyhow::ensure!(
        Some(&attachment) == saved_attachment,
        "restore-time network endpoint does not match the snapshot attachment"
    );
    Ok(Some(EffectiveMicrovmNetwork {
        config,
        policy,
        contract_policy,
        attachment,
    }))
}

/// Returns the ingress for which the guest announces its listeners under
/// `policy`: private-network ingress, whose listeners also admit the host with
/// explicit host-loopback access, or else host-loopback ingress alone.
pub(super) fn microvm_ingress(
    policy: &net_backend_resources::egress::EgressPolicy,
) -> Option<openvmm_defs::microvm::MicrovmIngress> {
    use net_backend_resources::egress::EgressAction;
    use openvmm_defs::microvm::MicrovmIngress;
    if policy.ingress_action() == EgressAction::Allow {
        Some(MicrovmIngress::PrivateNetworks)
    } else if policy.host_loopback_ingress_action() == EgressAction::Allow {
        Some(MicrovmIngress::HostLoopback)
    } else {
        None
    }
}

pub(super) fn microvm_network_endpoint(
    network: &openvmm_defs::microvm::MicrovmNetworkConfig,
    policy: &net_backend_resources::egress::EgressPolicy,
    loopback_forwards: &[cli_args::microvm::MicrovmLoopbackForwardCli],
    private_network_ingress: Option<net_backend_resources::consomme::PrivateNetworkIngress>,
    _resources: &mut VmResources,
) -> anyhow::Result<Resource<NetEndpointHandleKind>> {
    let ports = loopback_forwards
        .iter()
        .map(|forward| net_backend_resources::consomme::HostPortConfig {
            protocol: match forward.protocol {
                cli_args::microvm::MicrovmLoopbackForwardProtocol::Tcp => {
                    net_backend_resources::consomme::HostPortProtocol::Tcp
                }
                cli_args::microvm::MicrovmLoopbackForwardProtocol::Udp => {
                    net_backend_resources::consomme::HostPortProtocol::Udp
                }
            },
            host_address: Some(net_backend_resources::consomme::HostIpAddress::Ipv4(
                std::net::Ipv4Addr::LOCALHOST,
            )),
            host_port: net_backend_resources::consomme::HostPort::Fixed(forward.host_port),
            guest_port: forward.guest_port,
        })
        .collect();
    let allow_host_loopback =
        policy.host_loopback_action() == net_backend_resources::egress::EgressAction::Allow;
    Ok(net_backend_resources::consomme::ConsommeHandle {
        cidr: None,
        static_ipv4: Some(StaticIpv4Config {
            guest_ipv4: network.guest_ipv4,
            prefix_length: network.prefix_length,
            gateway_ipv4: network.derived_gateway_ipv4,
            gateway_mac: network.gateway_mac,
        }),
        static_ipv6: network.ipv6.map(|ipv6| StaticIpv6Config {
            guest_ipv6: ipv6.guest_ipv6,
            prefix_length: ipv6.prefix_length,
            gateway_ipv6: ipv6.derived_gateway_ipv6,
        }),
        ports,
        recv: None,
        allow_host_local_access: Some(allow_host_loopback),
        map_gateway_to_host_loopback: Some(allow_host_loopback),
        gateway_loopback_proxy_port: policy.proxy_endpoint().map(|endpoint| endpoint.port()),
        private_network_ingress,
        host_loopback_ingress: microvm_ingress(policy)
            == Some(openvmm_defs::microvm::MicrovmIngress::HostLoopback),
    }
    .into_resource())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::microvm::restore::align_legacy_network_policy_contract;
    use clap::Parser as _;
    use openvmm_defs::microvm::build_microvm_command_line;
    use test_with_tracing::test;

    pub(crate) fn network_contract() -> openvmm_helpers::snapshot::microvm::SnapshotMachineContract
    {
        network_contract_for(&"10.0.0.2/24".parse().unwrap())
    }

    fn network_contract_for(
        network: &openvmm_defs::microvm::MicrovmNetworkConfig,
    ) -> openvmm_helpers::snapshot::microvm::SnapshotMachineContract {
        let policy = network
            .bind_egress_policy(
                net_backend_resources::egress::EgressPolicyMode::TcpEndpoints(vec![
                    "10.0.0.9:8443".parse().unwrap(),
                    "192.0.2.7:443".parse().unwrap(),
                    "10.0.0.9:443".parse().unwrap(),
                ]),
            )
            .unwrap();
        contract_with_policy(network, &policy)
    }

    /// Returns the contract of a platform template whose NIC has the identity
    /// `network` and the complete deny-all policy.
    fn template_contract_for(
        network: &openvmm_defs::microvm::MicrovmNetworkConfig,
    ) -> openvmm_helpers::snapshot::microvm::SnapshotMachineContract {
        contract_with_policy(
            network,
            &openvmm_helpers::snapshot::microvm::microvm_network_template_policy(network).unwrap(),
        )
    }

    /// Returns the contract of a snapshot whose NIC has the identity `network`
    /// and was captured with `policy`.
    fn contract_with_policy(
        network: &openvmm_defs::microvm::MicrovmNetworkConfig,
        policy: &net_backend_resources::egress::EgressPolicy,
    ) -> openvmm_helpers::snapshot::microvm::SnapshotMachineContract {
        let source_hypervisor = if cfg!(windows) { "whp" } else { "kvm" };
        let irq = openvmm_defs::microvm::microvm_virtio_net_irq(Some(source_hypervisor)).unwrap();
        let mut command_line = build_microvm_command_line(&[], false).unwrap();
        openvmm_defs::microvm::append_microvm_virtio_discovery(
            &mut command_line,
            Some((network, irq, policy.gateway_dns_server(), None)),
            false,
            &[],
            false,
            false,
            &[],
        )
        .unwrap();
        openvmm_helpers::snapshot::microvm::microvm_machine_contract(
            source_hypervisor,
            openvmm_helpers::snapshot::microvm::MICROVM_BOOT_LAYOUT_VERSION,
            command_line,
            Some((network, policy, microvm_network_attachment())),
            false,
            None,
            None,
            None,
            Vec::new(),
            1,
            1024,
            None,
            [
                "partition",
                "vmtime",
                "pic",
                "ioapic",
                "pit",
                "rtc",
                "microvm-portb",
                "microvm-shutdown",
                "microvm-snapshot-request",
                "virtio-net-3489660928",
            ]
            .map(str::to_owned)
            .to_vec(),
            crate::microvm::restore::tests::test_time_contract(),
            crate::microvm::restore::tests::test_cpu_profile(),
        )
        .unwrap()
    }

    #[test]
    fn network_restore_rebinds_endpoint_next_hops_and_requires_policy() {
        let contract = network_contract();
        let options = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--network-profile",
            "portable",
            "--allow-endpoint",
            "192.0.2.7:443",
            "--allow-endpoint",
            "10.0.0.9:443",
            "--allow-endpoint",
            "10.0.0.9:8443",
        ])
        .unwrap();
        let restored = effective_microvm_network(&options, Some(&contract))
            .unwrap()
            .unwrap();
        assert_eq!(
            restored.config.guest_ipv4,
            std::net::Ipv4Addr::new(10, 0, 0, 2)
        );
        assert_eq!(
            restored.policy.next_hops(),
            &[
                std::net::Ipv4Addr::new(10, 0, 0, 1),
                std::net::Ipv4Addr::new(10, 0, 0, 9),
            ]
        );
        let restored_again = effective_microvm_network(&options, Some(&contract))
            .unwrap()
            .unwrap();
        assert_eq!(restored_again.policy, restored.policy);

        let missing_policy = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--network-profile",
            "portable",
        ])
        .unwrap();
        assert!(effective_microvm_network(&missing_policy, Some(&contract)).is_err());
    }

    #[test]
    fn network_restore_keeps_the_ipv6_identity_that_the_snapshot_records() {
        let options = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--network-profile",
            "portable",
            "--allow-endpoint",
            "192.0.2.7:443",
            "--allow-endpoint",
            "10.0.0.9:443",
            "--allow-endpoint",
            "10.0.0.9:8443",
        ])
        .unwrap();
        let dual_stack = effective_microvm_network(&options, Some(&network_contract()))
            .unwrap()
            .unwrap();
        let ipv6 = dual_stack.config.ipv6.unwrap();
        assert_eq!(
            ipv6.guest_ipv6,
            "fd00::a00:2".parse::<std::net::Ipv6Addr>().unwrap()
        );
        assert_eq!(
            dual_stack.policy.ipv6_link().map(|link| link.guest()),
            Some(ipv6.guest_ipv6)
        );

        // A snapshot whose network predates IPv6 restores without IPv6.
        let ipv4_only = "10.0.0.2/24"
            .parse::<openvmm_defs::microvm::MicrovmNetworkConfig>()
            .unwrap()
            .without_ipv6();
        let restored = effective_microvm_network(&options, Some(&network_contract_for(&ipv4_only)))
            .unwrap()
            .unwrap();
        assert_eq!(restored.config, ipv4_only);
        assert!(restored.policy.ipv6_link().is_none());
        assert_eq!(restored.policy.encoding_version(), 3);
    }

    #[test]
    fn legacy_network_policy_contract_alignment_preserves_validated_digest() {
        let mut saved = network_contract();
        let saved_network = saved.microvm_network.as_mut().unwrap();
        saved_network.egress_policy_encoding_version = 0;
        saved_network.egress_policy_sha256 = vec![0x5a; 32];
        let mut expected = network_contract();

        align_legacy_network_policy_contract(&saved, &mut expected);
        assert_eq!(saved.microvm_network, expected.microvm_network);
    }

    #[test]
    fn network_restore_rejects_attachment_identity_change() {
        let mut contract = network_contract();
        let attachment = contract
            .attachments
            .iter_mut()
            .find(|attachment| attachment.stable_id == MICROVM_NETWORK_STABLE_ID)
            .unwrap();
        attachment.identity.push(b'x');
        let options = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--network-profile",
            "portable",
            "--allow-endpoint",
            "192.0.2.7:443",
            "--allow-endpoint",
            "10.0.0.9:443",
            "--allow-endpoint",
            "10.0.0.9:8443",
        ])
        .unwrap();
        assert!(effective_microvm_network(&options, Some(&contract)).is_err());
    }

    #[test]
    fn network_restore_requires_portable_profile() {
        let contract = network_contract();
        let options = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--allow-host",
            "192.0.2.0/24",
        ])
        .unwrap();
        assert!(effective_microvm_network(&options, Some(&contract)).is_err());
    }

    #[cfg(guest_arch = "x86_64")]
    #[test]
    fn microvm_ingress_follows_the_ingress_and_host_loopback_actions() {
        use openvmm_defs::microvm::MicrovmIngress;

        let network: openvmm_defs::microvm::MicrovmNetworkConfig = "10.0.0.2/24".parse().unwrap();
        for (arguments, expected) in [
            (
                &["--network-ingress", "allow", "--host-loopback", "allow"][..],
                Some(MicrovmIngress::PrivateNetworks),
            ),
            (
                &["--network-ingress", "allow"][..],
                Some(MicrovmIngress::PrivateNetworks),
            ),
            (
                &["--host-loopback", "allow", "--network-egress", "deny"][..],
                Some(MicrovmIngress::HostLoopback),
            ),
            (&["--host-loopback", "deny"][..], None),
            (&[][..], None),
        ] {
            let options = Options::try_parse_from(
                [
                    "openvmm",
                    "--machine",
                    "microvm",
                    "--network-profile",
                    "portable",
                    "--net",
                    "10.0.0.2/24",
                ]
                .into_iter()
                .chain(arguments.iter().copied()),
            )
            .unwrap();
            options.validate_microvm_options().unwrap();
            let policy = options.microvm_egress_policy(&network).unwrap();
            assert_eq!(microvm_ingress(&policy), expected, "{arguments:?}");
        }
    }

    /// Parses the options of a networked restore with `arguments`.
    fn restore_options(arguments: &[&str]) -> Options {
        Options::try_parse_from(
            [
                "openvmm",
                "--machine",
                "microvm",
                "--restore-snapshot",
                "snapshot",
                "--network-profile",
                "portable",
            ]
            .into_iter()
            .chain(arguments.iter().copied()),
        )
        .unwrap()
    }

    #[cfg(guest_arch = "x86_64")]
    #[test]
    fn a_complete_deny_all_capture_records_a_template() {
        let network: openvmm_defs::microvm::MicrovmNetworkConfig = "10.0.0.2/24".parse().unwrap();
        let template =
            openvmm_helpers::snapshot::microvm::microvm_network_template_policy(&network).unwrap();
        let capture = |arguments: &[&str]| {
            let options = Options::try_parse_from(
                [
                    "openvmm",
                    "--machine",
                    "microvm",
                    "--net",
                    "10.0.0.2/24",
                    "--network-profile",
                    "portable",
                    "--snapshot-destination",
                    "snapshot",
                ]
                .into_iter()
                .chain(arguments.iter().copied()),
            )
            .unwrap();
            options.validate_microvm_options().unwrap();
            options.microvm_egress_policy(&network).unwrap()
        };
        assert_eq!(
            capture(&["--network-egress", "deny", "--host-loopback", "deny"]),
            template
        );
        // Deny-all egress alone leaves host loopback allowed.
        assert_ne!(capture(&["--network-egress", "deny"]), template);
    }

    #[cfg(guest_arch = "x86_64")]
    #[test]
    fn a_template_restore_installs_a_new_policy_and_keeps_the_identity() {
        let network: openvmm_defs::microvm::MicrovmNetworkConfig = "10.0.0.2/24".parse().unwrap();
        let contract = template_contract_for(&network);
        let template =
            openvmm_helpers::snapshot::microvm::microvm_network_template_policy(&network).unwrap();
        for arguments in [
            // A clone without connectivity keeps the template's policy.
            &["--network-egress", "deny", "--host-loopback", "deny"][..],
            &["--network-egress", "allow"][..],
            &[
                "--network-egress",
                "deny",
                "--network-egress-allow",
                "10.0.0.1/32:tcp:8080",
                "--network-egress-allow",
                "fd00::a00:1/128:tcp:8080",
                "--host-loopback",
                "deny",
            ][..],
            &[
                "--network-egress",
                "deny",
                "--allow-endpoint",
                "192.0.2.7:443",
            ][..],
            &[
                "--network-egress",
                "deny",
                "--host-loopback",
                "deny",
                "--network-proxy",
                "10.0.0.1:3128",
            ][..],
            &[
                "--network-egress",
                "deny",
                "--host-loopback",
                "allow",
                "--host-loopback-forward",
                "tcp:3000:8080",
                "--host-loopback-forward",
                "udp:3000:8080",
            ][..],
            &["--network-ingress", "allow"][..],
            &["--network-ingress", "allow", "--host-loopback", "allow"][..],
        ] {
            let options = restore_options(&[&["--restore-network-policy"][..], arguments].concat());
            options.validate_microvm_options().unwrap();
            let restored = effective_microvm_network(&options, Some(&contract))
                .unwrap()
                .unwrap();
            // The device installs the requested policy, bound to the
            // template's identity, which the contract checks with the
            // template's policy.
            assert_eq!(restored.config, network, "{arguments:?}");
            assert_eq!(
                restored.policy,
                options.microvm_egress_policy(&network).unwrap(),
                "{arguments:?}"
            );
            assert_eq!(restored.contract_policy, template, "{arguments:?}");
            assert_eq!(
                contract_with_policy(&restored.config, &restored.contract_policy).microvm_network,
                contract.microvm_network,
                "{arguments:?}"
            );
        }
        let deny_all = restore_options(&[
            "--restore-network-policy",
            "--network-egress",
            "deny",
            "--host-loopback",
            "deny",
        ]);
        let restored = effective_microvm_network(&deny_all, Some(&contract))
            .unwrap()
            .unwrap();
        assert_eq!(restored.policy, template);

        // Without the option, the template's restore keeps its policy.
        let kept = effective_microvm_network(
            &restore_options(&["--network-egress", "deny", "--host-loopback", "deny"]),
            Some(&contract),
        )
        .unwrap()
        .unwrap();
        assert_eq!(kept.policy, template);
        assert_eq!(kept.contract_policy, template);
        let error = effective_microvm_network(
            &restore_options(&["--network-egress", "allow"]),
            Some(&contract),
        )
        .unwrap_err();
        assert!(error.to_string().contains("does not match"), "{error:#}");
    }

    #[test]
    fn only_a_template_admits_a_new_policy() {
        let network: openvmm_defs::microvm::MicrovmNetworkConfig = "10.0.0.2/24".parse().unwrap();
        let replacement =
            restore_options(&["--restore-network-policy", "--network-egress", "allow"]);
        let deny_all_with_host_loopback = network
            .bind_egress_policy(net_backend_resources::egress::EgressPolicyMode::DenyAll)
            .unwrap();
        for contract in [
            network_contract(),
            contract_with_policy(&network, &deny_all_with_host_loopback),
        ] {
            let error = effective_microvm_network(&replacement, Some(&contract)).unwrap_err();
            assert!(
                error.to_string().contains("not a complete deny-all"),
                "{error:#}"
            );
        }

        // A template without a NIC has no policy to replace.
        let mut contract = template_contract_for(&network);
        contract.microvm_network = None;
        contract
            .devices
            .retain(|device| device.stable_id != MICROVM_NETWORK_STABLE_ID);
        contract
            .attachments
            .retain(|attachment| attachment.stable_id != MICROVM_NETWORK_STABLE_ID);
        let error = effective_microvm_network(&replacement, Some(&contract)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "--restore-network-policy requires a snapshot with a NIC"
        );
    }

    #[test]
    fn a_template_restore_binds_its_policy_to_the_captured_identity() {
        let network: openvmm_defs::microvm::MicrovmNetworkConfig = "10.0.0.2/24".parse().unwrap();
        let contract = template_contract_for(&network);
        for (arguments, expected) in [
            // The proxy and the endpoints must suit the captured identity, as
            // at a cold boot.
            (
                &[
                    "--network-egress",
                    "deny",
                    "--host-loopback",
                    "deny",
                    "--network-proxy",
                    "10.0.0.9:3128",
                ][..],
                "must match the guest gateway",
            ),
            (
                &[
                    "--network-egress",
                    "deny",
                    "--allow-endpoint",
                    "10.0.0.2:443",
                ][..],
                "matches the guest IPv4 address",
            ),
        ] {
            let options = restore_options(&[&["--restore-network-policy"][..], arguments].concat());
            let error = effective_microvm_network(&options, Some(&contract)).unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }
        // The restore cannot readdress the template's NIC.
        let readdressed = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--network-profile",
            "portable",
            "--restore-network-policy",
            "--net",
            "10.1.0.2/24",
        ])
        .unwrap();
        let error = effective_microvm_network(&readdressed, Some(&contract)).unwrap_err();
        assert!(error.to_string().contains("from saved state"), "{error:#}");

        // A template captured on a /30 cannot give its clones explicit
        // host-loopback access, which a cold boot on a /30 cannot have, but
        // takes every other policy.
        let slash_30: openvmm_defs::microvm::MicrovmNetworkConfig = "10.0.0.2/30".parse().unwrap();
        let contract = template_contract_for(&slash_30);
        for arguments in [
            &["--host-loopback", "allow"][..],
            &["--network-ingress", "allow", "--host-loopback", "allow"][..],
        ] {
            let options = restore_options(&[&["--restore-network-policy"][..], arguments].concat());
            let error = effective_microvm_network(&options, Some(&contract)).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("--host-loopback allow requires an IPv4 network of /29 or larger"),
                "{arguments:?}: {error:#}"
            );
        }
        for arguments in [
            &["--network-egress", "allow"][..],
            &["--network-ingress", "allow"][..],
            &["--network-egress", "deny", "--host-loopback", "deny"][..],
        ] {
            let options = restore_options(&[&["--restore-network-policy"][..], arguments].concat());
            let restored = effective_microvm_network(&options, Some(&contract))
                .unwrap()
                .unwrap();
            assert_eq!(restored.config, slash_30, "{arguments:?}");
        }

        // A template whose network predates IPv6 keeps no IPv6 identity.
        let ipv4_only = network.without_ipv6();
        let contract = template_contract_for(&ipv4_only);
        let ipv4_rules = restore_options(&[
            "--restore-network-policy",
            "--network-egress",
            "deny",
            "--network-egress-allow",
            "192.0.2.0/24:tcp:443",
        ]);
        let restored = effective_microvm_network(&ipv4_rules, Some(&contract))
            .unwrap()
            .unwrap();
        assert_eq!(restored.config, ipv4_only);
        assert!(restored.policy.ipv6_link().is_none());
        let ipv6_rules = restore_options(&[
            "--restore-network-policy",
            "--network-egress",
            "deny",
            "--network-egress-allow",
            "2001:db8::/32:tcp:443",
        ]);
        let error = effective_microvm_network(&ipv6_rules, Some(&contract)).unwrap_err();
        assert!(error.to_string().contains("no IPv6 identity"), "{error:#}");
    }
}
