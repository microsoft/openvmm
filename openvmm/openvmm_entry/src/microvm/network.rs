// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM static network identity and egress policy.

use crate::Options;
use crate::VmResources;
use crate::cli_args;
use crate::cli_args::EndpointConfigCli;
use anyhow::Context;
use net_backend_resources::consomme::static_ipv4::StaticIpv4Config;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::NetEndpointHandleKind;

const MICROVM_NETWORK_STABLE_ID: &str = "net:microvm0";

#[derive(Clone)]
pub(super) struct EffectiveMicrovmNetwork {
    pub(super) config: openvmm_defs::microvm::MicrovmNetworkConfig,
    pub(super) policy: net_backend_resources::egress::EgressPolicy,
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
    let prefix_length =
        u8::try_from(saved.prefix_length).context("snapshot network prefix does not fit in u8")?;
    let config = format!(
        "{}/{}",
        std::net::Ipv4Addr::from(saved.guest_ipv4),
        prefix_length
    )
    .parse::<openvmm_defs::microvm::MicrovmNetworkConfig>()
    .context("snapshot static network identity is invalid")?;
    anyhow::ensure!(
        saved.gateway_ipv4 == u32::from(config.derived_gateway_ipv4)
            && saved.guest_mac == config.guest_mac.to_bytes()
            && saved.gateway_mac == config.gateway_mac.to_bytes(),
        "snapshot static network identity is not canonical"
    );
    Ok(config)
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
    let policy = opt.microvm_egress_policy(&config)?;
    openvmm_helpers::snapshot::microvm::validate_microvm_network_policy(saved, &policy)?;
    let attachment = microvm_network_attachment();
    anyhow::ensure!(
        Some(&attachment) == saved_attachment,
        "restore-time network endpoint does not match the snapshot attachment"
    );
    Ok(Some(EffectiveMicrovmNetwork {
        config,
        policy,
        attachment,
    }))
}

pub(super) fn microvm_network_endpoint(
    network: &openvmm_defs::microvm::MicrovmNetworkConfig,
    policy: &net_backend_resources::egress::EgressPolicy,
    loopback_forwards: &[cli_args::microvm::MicrovmLoopbackForwardCli],
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
        ports,
        recv: None,
        allow_host_local_access: Some(allow_host_loopback),
        map_gateway_to_host_loopback: Some(allow_host_loopback),
        gateway_loopback_proxy_port: policy.proxy_endpoint().map(|endpoint| endpoint.port()),
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
        let network: openvmm_defs::microvm::MicrovmNetworkConfig = "10.0.0.2/24".parse().unwrap();
        let policy = net_backend_resources::egress::EgressPolicy::bind(
            network.guest_ipv4,
            network.prefix_length,
            network.guest_mac,
            network.derived_gateway_ipv4,
            net_backend_resources::egress::EgressPolicyMode::TcpEndpoints(vec![
                "10.0.0.9:8443".parse().unwrap(),
                "192.0.2.7:443".parse().unwrap(),
                "10.0.0.9:443".parse().unwrap(),
            ]),
        )
        .unwrap();
        let source_hypervisor = if cfg!(windows) { "whp" } else { "kvm" };
        let irq = openvmm_defs::microvm::microvm_virtio_net_irq(Some(source_hypervisor)).unwrap();
        let mut command_line = build_microvm_command_line(&[], false).unwrap();
        openvmm_defs::microvm::append_microvm_virtio_discovery(
            &mut command_line,
            Some((&network, irq, policy.allows_gateway_dns())),
            false,
            None,
            false,
            false,
            &[],
        )
        .unwrap();
        openvmm_helpers::snapshot::microvm::microvm_machine_contract(
            source_hypervisor,
            openvmm_helpers::snapshot::microvm::MICROVM_BOOT_LAYOUT_VERSION,
            command_line,
            Some((&network, &policy, microvm_network_attachment())),
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
            std::time::SystemTime::now().into(),
            1_000_000_000,
            Some(1_000_000_000),
            vec![1, 2, 3],
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
}
