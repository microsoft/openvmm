// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM static network identity.

use crate::Options;
use crate::VmResources;
use crate::cli_args::EndpointConfigCli;
use net_backend_resources::consomme::static_ipv4::StaticIpv4Config;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::NetEndpointHandleKind;

#[derive(Clone)]
pub(super) struct EffectiveMicrovmNetwork {
    pub(super) config: openvmm_defs::microvm::MicrovmNetworkConfig,
}

pub(super) fn effective_microvm_network(
    opt: &Options,
) -> anyhow::Result<Option<EffectiveMicrovmNetwork>> {
    let requested = match opt.net.as_slice() {
        [] => None,
        [network] => match &network.endpoint {
            EndpointConfigCli::Microvm(config) => Some(config.clone()),
            _ => anyhow::bail!("microVM --net was not validated as a static IPv4 identity"),
        },
        _ => anyhow::bail!("microVM permits at most one virtio-net device"),
    };
    Ok(requested.map(|config| EffectiveMicrovmNetwork { config }))
}

pub(super) fn microvm_network_endpoint(
    network: &openvmm_defs::microvm::MicrovmNetworkConfig,
    _resources: &mut VmResources,
) -> anyhow::Result<Resource<NetEndpointHandleKind>> {
    Ok(net_backend_resources::consomme::ConsommeHandle {
        cidr: None,
        static_ipv4: Some(StaticIpv4Config {
            guest_ipv4: network.guest_ipv4,
            prefix_length: network.prefix_length,
            gateway_ipv4: network.derived_gateway_ipv4,
            gateway_mac: network.gateway_mac,
        }),
        ports: Vec::new(),
        recv: None,
        allow_host_local_access: None,
        map_gateway_to_host_loopback: None,
        gateway_loopback_proxy_port: None,
    }
    .into_resource())
}
