// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Static IPv4 and IPv6 addressing and host access overrides of Consomme
//! resources.

use super::ResolveConsommeError;
use consomme::ConsommeParams;
use net_backend_resources::consomme::ConsommeHandle;

/// Applies the exact static IPv4 identity, which is mutually exclusive with a
/// CIDR, the exact static IPv6 identity that extends it, and the host access
/// overrides of `resource` to `state`.
pub(super) fn configure(
    state: &mut ConsommeParams,
    resource: &ConsommeHandle,
) -> Result<(), ResolveConsommeError> {
    if resource.cidr.is_some() && resource.static_ipv4.is_some() {
        return Err(ResolveConsommeError::ConflictingIpv4Configuration);
    }
    if resource.static_ipv6.is_some() && resource.static_ipv4.is_none() {
        return Err(ResolveConsommeError::StaticIpv6WithoutStaticIpv4);
    }
    if let Some(config) = &resource.static_ipv4 {
        state
            .set_static_ipv4(
                config.guest_ipv4,
                config.prefix_length,
                config.gateway_ipv4,
                config.gateway_mac.to_bytes(),
            )
            .map_err(ResolveConsommeError::InvalidStaticIpv4)?;
    }
    if let Some(config) = &resource.static_ipv6 {
        state
            .set_static_ipv6(config.guest_ipv6, config.prefix_length, config.gateway_ipv6)
            .map_err(ResolveConsommeError::InvalidStaticIpv6)?;
    }
    if let Some(allow) = resource.allow_host_local_access {
        state.allow_host_local_access = allow;
    }
    if let Some(map) = resource.map_gateway_to_host_loopback {
        state.map_gateway_to_host_loopback = map;
    }
    state.gateway_loopback_proxy_port = resource.gateway_loopback_proxy_port;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use net_backend_resources::consomme::static_ipv4::StaticIpv4Config;
    use net_backend_resources::consomme::static_ipv6::StaticIpv6Config;
    use net_backend_resources::mac_address::MacAddress;
    use std::net::Ipv4Addr;
    use std::net::Ipv6Addr;

    fn handle(static_ipv4: bool) -> ConsommeHandle {
        ConsommeHandle {
            cidr: None,
            static_ipv4: static_ipv4.then(|| StaticIpv4Config {
                guest_ipv4: Ipv4Addr::new(10, 0, 0, 2),
                prefix_length: 24,
                gateway_ipv4: Ipv4Addr::new(10, 0, 0, 1),
                gateway_mac: MacAddress::new([0x52, 0x54, 0, 0, 0, 1]),
            }),
            static_ipv6: Some(StaticIpv6Config {
                guest_ipv6: "fd00::a00:2".parse().unwrap(),
                prefix_length: 120,
                gateway_ipv6: "fd00::a00:1".parse().unwrap(),
            }),
            ports: Vec::new(),
            recv: None,
            allow_host_local_access: Some(false),
            map_gateway_to_host_loopback: Some(false),
            gateway_loopback_proxy_port: None,
        }
    }

    #[test]
    fn static_ipv6_extends_static_ipv4() {
        let mut state = ConsommeParams::new().unwrap();
        configure(&mut state, &handle(true)).unwrap();
        assert_eq!(
            state.gateway_ipv6,
            Some("fd00::a00:1".parse::<Ipv6Addr>().unwrap())
        );
        assert_eq!(state.gateway_mac_ipv6.0, [0x52, 0x54, 0, 0, 0, 1]);
        // The host access overrides apply after both identities.
        assert!(!state.map_gateway_to_host_loopback);

        let mut state = ConsommeParams::new().unwrap();
        assert!(matches!(
            configure(&mut state, &handle(false)),
            Err(ResolveConsommeError::StaticIpv6WithoutStaticIpv4)
        ));
    }
}
