// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Exact static IPv6 identity of a dual-stack Consomme endpoint.
//!
//! A [`StaticIpv6Config`] in a [`ConsommeHandle`](super::ConsommeHandle)
//! adds an exact IPv6 identity to the endpoint's exact static IPv4 identity,
//! whose gateway Ethernet address it shares.

use mesh::MeshPayload;

/// Exact static IPv6 identity used by the dual-stack microVM network profile.
#[derive(Clone, Debug, MeshPayload)]
pub struct StaticIpv6Config {
    /// Guest IPv6 address.
    pub guest_ipv6: std::net::Ipv6Addr,
    /// IPv6 subnet prefix length.
    pub prefix_length: u8,
    /// Host gateway IPv6 address.
    pub gateway_ipv6: std::net::Ipv6Addr,
}
