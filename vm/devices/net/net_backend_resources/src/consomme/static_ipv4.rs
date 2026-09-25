// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Exact static IPv4 identity of a Consomme endpoint.
//!
//! A [`StaticIpv4Config`] in a [`ConsommeHandle`](super::ConsommeHandle)
//! replaces the CIDR-derived guest and gateway addresses with an exact
//! identity.

use crate::mac_address::MacAddress;
use mesh::MeshPayload;

/// Exact static IPv4 identity used by the microVM network profile.
#[derive(Clone, Debug, MeshPayload)]
pub struct StaticIpv4Config {
    /// Guest IPv4 address.
    pub guest_ipv4: std::net::Ipv4Addr,
    /// IPv4 subnet prefix length.
    pub prefix_length: u8,
    /// Host gateway IPv4 address.
    pub gateway_ipv4: std::net::Ipv4Addr,
    /// Host gateway Ethernet address.
    pub gateway_mac: MacAddress,
}
