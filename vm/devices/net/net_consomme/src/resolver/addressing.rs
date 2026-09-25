// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Static IPv4 addressing and host access overrides of Consomme resources.

use super::ResolveConsommeError;
use consomme::ConsommeParams;
use net_backend_resources::consomme::ConsommeHandle;

/// Applies the exact static IPv4 identity, which is mutually exclusive with a
/// CIDR, and the host access overrides of `resource` to `state`.
pub(super) fn configure(
    state: &mut ConsommeParams,
    resource: &ConsommeHandle,
) -> Result<(), ResolveConsommeError> {
    if resource.cidr.is_some() && resource.static_ipv4.is_some() {
        return Err(ResolveConsommeError::ConflictingIpv4Configuration);
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
    if let Some(allow) = resource.allow_host_local_access {
        state.allow_host_local_access = allow;
    }
    if let Some(map) = resource.map_gateway_to_host_loopback {
        state.map_gateway_to_host_loopback = map;
    }
    state.gateway_loopback_proxy_port = resource.gateway_loopback_proxy_port;
    Ok(())
}
