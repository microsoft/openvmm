// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Canonical microVM IPv4 egress policy and packet validation.

use crate::mac_address::MacAddress;
use mesh::MeshPayload;
use std::net::Ipv4Addr;
use std::str::FromStr;
use thiserror::Error;

/// Current encoding version used for snapshot egress-policy digests.
pub const EGRESS_POLICY_ENCODING_VERSION: u32 = 3;
const PREVIOUS_EGRESS_POLICY_ENCODING_VERSION: u32 = 2;
const LEGACY_EGRESS_POLICY_ENCODING_VERSION: u32 = 1;
const ETHERNET_BROADCAST: [u8; 6] = [0xff; 6];
const ETHERNET_UNSPECIFIED: [u8; 6] = [0; 6];

/// A canonical IPv4 network prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, MeshPayload)]
pub struct Ipv4Cidr {
    network: Ipv4Addr,
    prefix_length: u8,
}

impl Ipv4Cidr {
    /// Returns whether `address` belongs to this prefix.
    pub fn contains(self, address: Ipv4Addr) -> bool {
        let mask = prefix_mask(self.prefix_length);
        u32::from(address) & mask == u32::from(self.network)
    }

    /// Returns the canonical network address.
    pub fn network(self) -> Ipv4Addr {
        self.network
    }

    /// Returns the prefix length.
    pub fn prefix_length(self) -> u8 {
        self.prefix_length
    }
}

/// Error returned when parsing an IPv4 address or CIDR policy rule.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseIpv4CidrError {
    /// The address portion is not IPv4.
    #[error("invalid IPv4 address '{0}'")]
    InvalidAddress(String),
    /// The prefix portion is not an integer.
    #[error("invalid IPv4 prefix '{0}'")]
    InvalidPrefix(String),
    /// The prefix is greater than 32.
    #[error("IPv4 prefix /{0} is outside the supported range /0 through /32")]
    PrefixOutOfRange(u8),
    /// The rule has more than one prefix separator.
    #[error("invalid IPv4 CIDR '{0}'")]
    InvalidFormat(String),
}

impl FromStr for Ipv4Cidr {
    type Err = ParseIpv4CidrError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (address, prefix) = match value.split_once('/') {
            Some((address, prefix)) if !prefix.contains('/') => (address, Some(prefix)),
            Some(_) => return Err(ParseIpv4CidrError::InvalidFormat(value.to_owned())),
            None => (value, None),
        };
        let address = address
            .parse::<Ipv4Addr>()
            .map_err(|_| ParseIpv4CidrError::InvalidAddress(address.to_owned()))?;
        let prefix_length = prefix
            .map(|prefix| {
                prefix
                    .parse::<u8>()
                    .map_err(|_| ParseIpv4CidrError::InvalidPrefix(prefix.to_owned()))
            })
            .transpose()?
            .unwrap_or(32);
        if prefix_length > 32 {
            return Err(ParseIpv4CidrError::PrefixOutOfRange(prefix_length));
        }
        let network = Ipv4Addr::from(u32::from(address) & prefix_mask(prefix_length));
        Ok(Self {
            network,
            prefix_length,
        })
    }
}

/// An exact IPv4 TCP destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, MeshPayload)]
pub struct TcpEndpoint {
    address: Ipv4Addr,
    port: u16,
}

impl TcpEndpoint {
    /// Returns the destination address.
    pub fn address(self) -> Ipv4Addr {
        self.address
    }

    /// Returns the destination TCP port.
    pub fn port(self) -> u16 {
        self.port
    }
}

/// Error returned when parsing an exact TCP endpoint policy rule.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseTcpEndpointError {
    /// The endpoint is missing its port separator.
    #[error("expected <IPv4>:<TCP-port>")]
    InvalidFormat,
    /// The address portion is not IPv4.
    #[error("invalid IPv4 address '{0}'")]
    InvalidAddress(String),
    /// The port portion is not a `u16`.
    #[error("invalid TCP port '{0}'")]
    InvalidPort(String),
    /// Port zero is not a connectable endpoint.
    #[error("TCP port must be nonzero")]
    ZeroPort,
}

impl FromStr for TcpEndpoint {
    type Err = ParseTcpEndpointError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (address, port) = value
            .rsplit_once(':')
            .ok_or(ParseTcpEndpointError::InvalidFormat)?;
        let address = address
            .parse::<Ipv4Addr>()
            .map_err(|_| ParseTcpEndpointError::InvalidAddress(address.to_owned()))?;
        let port = port
            .parse::<u16>()
            .map_err(|_| ParseTcpEndpointError::InvalidPort(port.to_owned()))?;
        if port == 0 {
            return Err(ParseTcpEndpointError::ZeroPort);
        }
        Ok(Self { address, port })
    }
}

/// Default action for a rule-based egress policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, MeshPayload)]
pub enum EgressAction {
    /// Permit traffic that matches no rule.
    Allow,
    /// Deny traffic that matches no rule.
    Deny,
}

/// Optional transport restriction for an egress rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, MeshPayload)]
pub enum EgressTransport {
    /// Match every IPv4 protocol and port.
    Any,
    /// Match one TCP destination port.
    Tcp,
    /// Match one UDP destination port.
    Udp,
}

/// A canonical IPv4 destination rule with an optional TCP or UDP port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, MeshPayload)]
pub struct EgressRule {
    destination: Ipv4Cidr,
    transport: EgressTransport,
    port: u16,
}

impl EgressRule {
    /// Returns the destination prefix.
    pub fn destination(self) -> Ipv4Cidr {
        self.destination
    }

    /// Returns the optional transport restriction.
    pub fn transport(self) -> EgressTransport {
        self.transport
    }

    /// Returns the restricted port, or zero for an address-only rule.
    pub fn port(self) -> u16 {
        self.port
    }

    fn matches(self, destination: Ipv4Addr, transport: Option<(EgressTransport, u16)>) -> bool {
        if !self.destination.contains(destination) {
            return false;
        }
        match self.transport {
            EgressTransport::Any => true,
            expected => {
                transport.is_some_and(|(actual, port)| actual == expected && port == self.port)
            }
        }
    }
}

/// Error returned when parsing an L3/L4 egress rule.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseEgressRuleError {
    /// The rule did not use the supported address or address/protocol/port form.
    #[error("expected <IPv4[/PREFIX]> or <IPv4[/PREFIX]>:<tcp|udp>:<PORT>")]
    InvalidFormat,
    /// The destination prefix is invalid.
    #[error(transparent)]
    InvalidDestination(#[from] ParseIpv4CidrError),
    /// The transport is not TCP or UDP.
    #[error("invalid egress transport '{0}'; expected tcp or udp")]
    InvalidTransport(String),
    /// The port is not a `u16`.
    #[error("invalid egress port '{0}'")]
    InvalidPort(String),
    /// Port zero is not a connectable destination.
    #[error("egress rule port must be nonzero")]
    ZeroPort,
}

impl FromStr for EgressRule {
    type Err = ParseEgressRuleError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let fields = value.split(':').collect::<Vec<_>>();
        let (destination, transport, port) = match fields.as_slice() {
            [destination] => (destination.parse()?, EgressTransport::Any, 0),
            [destination, protocol, port] => {
                let transport = match *protocol {
                    "tcp" => EgressTransport::Tcp,
                    "udp" => EgressTransport::Udp,
                    other => {
                        return Err(ParseEgressRuleError::InvalidTransport(other.to_owned()));
                    }
                };
                let port = port
                    .parse::<u16>()
                    .map_err(|_| ParseEgressRuleError::InvalidPort((*port).to_owned()))?;
                if port == 0 {
                    return Err(ParseEgressRuleError::ZeroPort);
                }
                (destination.parse()?, transport, port)
            }
            _ => return Err(ParseEgressRuleError::InvalidFormat),
        };
        Ok(Self {
            destination,
            transport,
            port,
        })
    }
}

/// Run-scoped egress policy mode.
#[derive(Clone, Debug, PartialEq, Eq, MeshPayload)]
pub enum EgressPolicyMode {
    /// No packet filtering.
    AllowAll,
    /// Permit only destinations in these IPv4 prefixes.
    AllowList(Vec<Ipv4Cidr>),
    /// Permit IPv4 destinations except those in these prefixes.
    BlockList(Vec<Ipv4Cidr>),
    /// Permit only exact IPv4 TCP destinations.
    TcpEndpoints(Vec<TcpEndpoint>),
    /// Deny every guest-originated frame.
    DenyAll,
    /// Apply canonical allow and deny rules with deny precedence.
    Rules {
        /// Action for traffic that matches no rule.
        default_action: EgressAction,
        /// Rules that permit matching traffic.
        allow: Vec<EgressRule>,
        /// Rules that reject matching traffic before allow evaluation.
        deny: Vec<EgressRule>,
    },
}

/// Reason an endpoint address cannot be used by exact endpoint policy.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum InvalidEndpointAddress {
    /// The address is `0.0.0.0`.
    #[error("it is unspecified")]
    Unspecified,
    /// The address belongs to the reserved current-network block.
    #[error("it belongs to the reserved 0.0.0.0/8 current-network block")]
    CurrentNetwork,
    /// The address belongs to IPv4 loopback.
    #[error("it is loopback")]
    Loopback,
    /// The address is IPv4 link-local.
    #[error("it is link-local")]
    LinkLocal,
    /// The address is multicast.
    #[error("it is multicast")]
    Multicast,
    /// The address is the limited broadcast address.
    #[error("it is the limited broadcast address")]
    LimitedBroadcast,
    /// The address belongs to the reserved high-address block.
    #[error("it belongs to the reserved 240.0.0.0/4 block")]
    Reserved,
    /// The address is the guest itself.
    #[error("it matches the guest IPv4 address")]
    Guest,
    /// The address is the guest subnet's network address.
    #[error("it is the guest subnet network address")]
    SubnetNetwork,
    /// The address is the guest subnet's broadcast address.
    #[error("it is the guest subnet broadcast address")]
    SubnetBroadcast,
}

/// Error returned when binding an egress policy to a static network identity.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum InvalidEgressPolicy {
    /// The prefix cannot describe a microVM subnet.
    #[error("IPv4 prefix /{0} is outside the supported range /1 through /30")]
    PrefixOutOfRange(u8),
    /// The guest address is the subnet network address.
    #[error("guest IPv4 address {0} is the subnet network address")]
    GuestNetworkAddress(Ipv4Addr),
    /// The guest address is the subnet broadcast address.
    #[error("guest IPv4 address {0} is the subnet broadcast address")]
    GuestBroadcastAddress(Ipv4Addr),
    /// The guest address collides with the canonical gateway.
    #[error("guest IPv4 address {0} collides with the canonical gateway")]
    GuestGatewayCollision(Ipv4Addr),
    /// The gateway is not the first usable address in the guest subnet.
    #[error("gateway IPv4 address {actual} does not match canonical gateway {expected}")]
    GatewayMismatch {
        /// Supplied gateway address.
        actual: Ipv4Addr,
        /// Canonical first usable subnet address.
        expected: Ipv4Addr,
    },
    /// The guest MAC cannot identify a transmitting endpoint.
    #[error("guest MAC address {0} is not a nonzero unicast address")]
    InvalidGuestMac(MacAddress),
    /// The gateway cannot serve as an endpoint next hop.
    #[error("gateway IPv4 address {address} is not a valid endpoint next hop: {reason}")]
    InvalidGateway {
        /// Invalid gateway address.
        address: Ipv4Addr,
        /// Reason the address is invalid.
        reason: InvalidEndpointAddress,
    },
    /// An exact endpoint address is invalid.
    #[error("endpoint IPv4 address {address} is invalid: {reason}")]
    InvalidEndpoint {
        /// Invalid endpoint address.
        address: Ipv4Addr,
        /// Reason the address is invalid.
        reason: InvalidEndpointAddress,
    },
    /// A decoded policy did not preserve canonical rule order and uniqueness.
    #[error("egress policy rules are not canonical")]
    NonCanonicalRules,
    /// A decoded policy carried next hops that do not match its bound rules.
    #[error("egress policy next hops do not match the bound endpoint rules")]
    InvalidNextHops,
    /// A proxy exception did not use the guest-visible gateway address.
    #[error("network proxy address {actual} must match the guest gateway {expected}")]
    InvalidProxyAddress {
        /// Supplied proxy address.
        actual: Ipv4Addr,
        /// Required guest-visible address.
        expected: Ipv4Addr,
    },
}

/// Egress policy bound to one static microVM link.
#[derive(Clone, Debug, PartialEq, Eq, MeshPayload)]
pub struct EgressPolicy {
    guest_ipv4: Ipv4Addr,
    prefix_length: u8,
    guest_mac: MacAddress,
    gateway_ipv4: Ipv4Addr,
    next_hops: Vec<Ipv4Addr>,
    mode: EgressPolicyMode,
    host_loopback: EgressAction,
    proxy_endpoint: Option<TcpEndpoint>,
}

impl EgressPolicy {
    /// Binds a policy to a validated static identity and canonicalizes its rules.
    pub fn bind(
        guest_ipv4: Ipv4Addr,
        prefix_length: u8,
        guest_mac: MacAddress,
        gateway_ipv4: Ipv4Addr,
        mut mode: EgressPolicyMode,
    ) -> Result<Self, InvalidEgressPolicy> {
        if !(1..=30).contains(&prefix_length) {
            return Err(InvalidEgressPolicy::PrefixOutOfRange(prefix_length));
        }
        let mask = prefix_mask(prefix_length);
        let network = Ipv4Addr::from(u32::from(guest_ipv4) & mask);
        let broadcast = Ipv4Addr::from(u32::from(network) | !mask);
        if guest_ipv4 == network {
            return Err(InvalidEgressPolicy::GuestNetworkAddress(guest_ipv4));
        }
        if guest_ipv4 == broadcast {
            return Err(InvalidEgressPolicy::GuestBroadcastAddress(guest_ipv4));
        }
        let expected_gateway = Ipv4Addr::from(u32::from(network) + 1);
        if guest_ipv4 == expected_gateway {
            return Err(InvalidEgressPolicy::GuestGatewayCollision(guest_ipv4));
        }
        if gateway_ipv4 != expected_gateway {
            return Err(InvalidEgressPolicy::GatewayMismatch {
                actual: gateway_ipv4,
                expected: expected_gateway,
            });
        }
        let guest_mac_bytes = guest_mac.to_bytes();
        if guest_mac_bytes == ETHERNET_UNSPECIFIED || guest_mac_bytes[0] & 1 != 0 {
            return Err(InvalidEgressPolicy::InvalidGuestMac(guest_mac));
        }

        match &mut mode {
            EgressPolicyMode::AllowList(rules) | EgressPolicyMode::BlockList(rules) => {
                rules.sort_unstable();
                rules.dedup();
            }
            EgressPolicyMode::TcpEndpoints(endpoints) => {
                endpoints.sort_unstable();
                endpoints.dedup();
            }
            EgressPolicyMode::Rules { allow, deny, .. } => {
                allow.sort_unstable();
                allow.dedup();
                deny.sort_unstable();
                deny.dedup();
            }
            EgressPolicyMode::AllowAll | EgressPolicyMode::DenyAll => {}
        }

        let next_hops = match &mode {
            EgressPolicyMode::AllowAll | EgressPolicyMode::DenyAll => Vec::new(),
            EgressPolicyMode::AllowList(_) | EgressPolicyMode::BlockList(_) => {
                vec![gateway_ipv4]
            }
            EgressPolicyMode::TcpEndpoints(endpoints) => {
                if let Some(reason) =
                    invalid_special_endpoint_address(gateway_ipv4, guest_ipv4, network, broadcast)
                {
                    return Err(InvalidEgressPolicy::InvalidGateway {
                        address: gateway_ipv4,
                        reason,
                    });
                }
                let mut next_hops = Vec::with_capacity(endpoints.len());
                for endpoint in endpoints {
                    if let Some(reason) = invalid_special_endpoint_address(
                        endpoint.address,
                        guest_ipv4,
                        network,
                        broadcast,
                    ) {
                        return Err(InvalidEgressPolicy::InvalidEndpoint {
                            address: endpoint.address,
                            reason,
                        });
                    }
                    next_hops.push(if contains_address(network, mask, endpoint.address) {
                        endpoint.address
                    } else {
                        gateway_ipv4
                    });
                }
                next_hops.sort_unstable();
                next_hops.dedup();
                next_hops
            }
            EgressPolicyMode::Rules {
                default_action,
                allow,
                ..
            } => {
                if *default_action == EgressAction::Deny && allow.is_empty() {
                    Vec::new()
                } else {
                    vec![gateway_ipv4]
                }
            }
        };

        Ok(Self {
            guest_ipv4,
            prefix_length,
            guest_mac,
            gateway_ipv4,
            next_hops,
            mode,
            host_loopback: EgressAction::Allow,
            proxy_endpoint: None,
        })
    }

    /// Applies the host-loopback posture and optional exact proxy exception.
    pub fn with_host_loopback(
        mut self,
        host_loopback: EgressAction,
        proxy_endpoint: Option<TcpEndpoint>,
    ) -> Result<Self, InvalidEgressPolicy> {
        if let Some(proxy) = proxy_endpoint
            && proxy.address != self.gateway_ipv4
        {
            return Err(InvalidEgressPolicy::InvalidProxyAddress {
                actual: proxy.address,
                expected: self.gateway_ipv4,
            });
        }
        if proxy_endpoint.is_some() && !self.next_hops.contains(&self.gateway_ipv4) {
            self.next_hops.push(self.gateway_ipv4);
            self.next_hops.sort_unstable();
            self.next_hops.dedup();
        }
        self.host_loopback = host_loopback;
        self.proxy_endpoint = proxy_endpoint;
        Ok(self)
    }

    /// Revalidates canonical fields after a serialized policy crosses a boundary.
    pub fn validate(&self) -> Result<(), InvalidEgressPolicy> {
        let rebound = Self::bind(
            self.guest_ipv4,
            self.prefix_length,
            self.guest_mac,
            self.gateway_ipv4,
            self.mode.clone(),
        )?
        .with_host_loopback(self.host_loopback, self.proxy_endpoint)?;
        if rebound.mode != self.mode {
            return Err(InvalidEgressPolicy::NonCanonicalRules);
        }
        if rebound.next_hops != self.next_hops {
            return Err(InvalidEgressPolicy::InvalidNextHops);
        }
        Ok(())
    }

    /// Returns the canonical policy mode.
    pub fn mode(&self) -> &EgressPolicyMode {
        &self.mode
    }

    /// Returns the stable manifest name of this policy mode.
    pub fn mode_name(&self) -> &'static str {
        match self.mode {
            EgressPolicyMode::AllowAll => "allow-all",
            EgressPolicyMode::DenyAll => "deny-all",
            EgressPolicyMode::AllowList(_) => "allow-list",
            EgressPolicyMode::BlockList(_) => "block-list",
            EgressPolicyMode::TcpEndpoints(_) => "endpoint",
            EgressPolicyMode::Rules { .. } => "rules",
        }
    }

    /// Returns the canonical next hops that ARP may resolve.
    pub fn next_hops(&self) -> &[Ipv4Addr] {
        &self.next_hops
    }

    /// Returns whether general host-loopback connectivity is permitted.
    pub fn host_loopback_action(&self) -> EgressAction {
        self.host_loopback
    }

    /// Returns the exact proxy endpoint exempted from host-loopback denial.
    pub fn proxy_endpoint(&self) -> Option<TcpEndpoint> {
        self.proxy_endpoint
    }

    /// Returns whether packet parsing and filtering are active.
    pub fn is_active(&self) -> bool {
        match &self.mode {
            EgressPolicyMode::AllowAll => false,
            EgressPolicyMode::Rules {
                default_action: EgressAction::Allow,
                deny,
                ..
            } if deny.is_empty() => false,
            _ => true,
        }
    }

    /// Returns whether the gateway DNS proxy is reachable under this policy.
    pub fn allows_gateway_dns(&self) -> bool {
        match &self.mode {
            EgressPolicyMode::AllowAll => true,
            EgressPolicyMode::DenyAll => false,
            EgressPolicyMode::AllowList(rules) => {
                rules.iter().any(|rule| rule.contains(self.gateway_ipv4))
            }
            EgressPolicyMode::BlockList(rules) => {
                !rules.iter().any(|rule| rule.contains(self.gateway_ipv4))
            }
            EgressPolicyMode::TcpEndpoints(_) => false,
            EgressPolicyMode::Rules { .. } => {
                self.rules_allow_destination(self.gateway_ipv4, Some((EgressTransport::Tcp, 53)))
                    || self.rules_allow_destination(
                        self.gateway_ipv4,
                        Some((EgressTransport::Udp, 53)),
                    )
            }
        }
    }

    /// Returns canonical rule counts without exposing destinations.
    pub fn rule_counts(&self) -> (usize, usize) {
        match &self.mode {
            EgressPolicyMode::Rules { allow, deny, .. } => (allow.len(), deny.len()),
            EgressPolicyMode::AllowList(rules) => (rules.len(), 0),
            EgressPolicyMode::BlockList(rules) => (0, rules.len()),
            EgressPolicyMode::TcpEndpoints(rules) => (rules.len(), 0),
            EgressPolicyMode::AllowAll | EgressPolicyMode::DenyAll => (0, 0),
        }
    }

    fn rules_allow_destination(
        &self,
        destination: Ipv4Addr,
        transport: Option<(EgressTransport, u16)>,
    ) -> bool {
        let EgressPolicyMode::Rules {
            default_action,
            allow,
            deny,
        } = &self.mode
        else {
            return false;
        };
        if deny.iter().any(|rule| rule.matches(destination, transport)) {
            return false;
        }
        if allow
            .iter()
            .any(|rule| rule.matches(destination, transport))
        {
            return true;
        }
        *default_action == EgressAction::Allow
    }

    fn rules_allow_arp(&self, target: Ipv4Addr) -> bool {
        let EgressPolicyMode::Rules {
            default_action,
            allow,
            deny,
        } = &self.mode
        else {
            return false;
        };
        if target == self.gateway_ipv4 {
            return *default_action == EgressAction::Allow || !allow.is_empty();
        }
        if deny
            .iter()
            .any(|rule| rule.transport == EgressTransport::Any && rule.destination.contains(target))
        {
            return false;
        }
        if allow.iter().any(|rule| rule.destination.contains(target)) {
            return true;
        }
        *default_action == EgressAction::Allow
    }

    fn rules_have_transport_restrictions(&self) -> bool {
        matches!(
            &self.mode,
            EgressPolicyMode::Rules { allow, deny, .. }
                if allow
                    .iter()
                    .chain(deny)
                    .any(|rule| rule.transport != EgressTransport::Any)
        )
    }

    /// Returns stable bytes suitable for a policy digest.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        self.canonical_bytes_v3()
    }

    /// Returns canonical bytes for a supported snapshot encoding version.
    pub fn canonical_bytes_for_version(&self, version: u32) -> Option<Vec<u8>> {
        match version {
            LEGACY_EGRESS_POLICY_ENCODING_VERSION => Some(self.canonical_bytes_v1()),
            PREVIOUS_EGRESS_POLICY_ENCODING_VERSION => Some(self.canonical_bytes_v2()),
            EGRESS_POLICY_ENCODING_VERSION => Some(self.canonical_bytes_v3()),
            _ => None,
        }
    }

    fn canonical_bytes_v1(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(LEGACY_EGRESS_POLICY_ENCODING_VERSION as u8);
        bytes.extend_from_slice(&self.guest_ipv4.octets());
        bytes.extend_from_slice(&self.gateway_ipv4.octets());
        append_policy_mode(&mut bytes, &self.mode);
        bytes
    }

    fn canonical_bytes_v2(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(EGRESS_POLICY_ENCODING_VERSION as u8);
        bytes.extend_from_slice(&self.guest_ipv4.octets());
        bytes.push(self.prefix_length);
        bytes.extend_from_slice(&self.guest_mac.to_bytes());
        bytes.extend_from_slice(&self.gateway_ipv4.octets());
        append_policy_mode(&mut bytes, &self.mode);
        bytes.push(0xff);
        bytes.extend_from_slice(&(self.next_hops.len() as u64).to_be_bytes());
        for next_hop in &self.next_hops {
            bytes.extend_from_slice(&next_hop.octets());
        }
        bytes
    }

    fn canonical_bytes_v3(&self) -> Vec<u8> {
        let mut bytes = self.canonical_bytes_v2();
        bytes[0] = EGRESS_POLICY_ENCODING_VERSION as u8;
        bytes.push(match self.host_loopback {
            EgressAction::Allow => 0,
            EgressAction::Deny => 1,
        });
        if let Some(proxy) = self.proxy_endpoint {
            bytes.push(1);
            bytes.extend_from_slice(&proxy.address.octets());
            bytes.extend_from_slice(&proxy.port.to_be_bytes());
        } else {
            bytes.push(0);
        }
        bytes
    }

    /// Validates and authorizes an Ethernet frame before backend submission.
    pub fn authorize_frame(
        &self,
        frame_prefix: &[u8],
        frame_length: usize,
    ) -> Result<(), EgressDenied> {
        if !self.is_active() {
            return Ok(());
        }
        if matches!(self.mode, EgressPolicyMode::DenyAll) && self.proxy_endpoint.is_none() {
            return Err(EgressDenied::AllTrafficDenied);
        }
        ensure_available(frame_prefix, frame_length, 14, "Ethernet header")?;
        if frame_prefix[6..12] != self.guest_mac.to_bytes() {
            return Err(EgressDenied::SourceMacDenied);
        }
        let mut ether_type = read_u16(frame_prefix, 12);
        let mut l3_offset = 14;
        let vlan = matches!(ether_type, 0x8100 | 0x88a8);
        if vlan {
            if matches!(self.mode, EgressPolicyMode::TcpEndpoints(_)) {
                return Err(EgressDenied::VlanDenied);
            }
            ensure_available(frame_prefix, frame_length, 18, "VLAN header")?;
            ether_type = read_u16(frame_prefix, 16);
            l3_offset = 18;
            if matches!(ether_type, 0x8100 | 0x88a8) {
                return Err(EgressDenied::Malformed("stacked VLAN header"));
            }
        }

        match ether_type {
            0x0806 => self.authorize_arp(frame_prefix, frame_length, l3_offset),
            0x0800 => self.authorize_ipv4(frame_prefix, frame_length, l3_offset),
            other => Err(EgressDenied::UnsupportedEtherType(other)),
        }
    }

    fn authorize_arp(
        &self,
        frame: &[u8],
        frame_length: usize,
        offset: usize,
    ) -> Result<(), EgressDenied> {
        ensure_available(frame, frame_length, offset + 28, "ARP packet")?;
        if read_u16(frame, offset) != 1
            || read_u16(frame, offset + 2) != 0x0800
            || frame[offset + 4] != 6
            || frame[offset + 5] != 4
            || read_u16(frame, offset + 6) != 1
        {
            return Err(EgressDenied::Malformed("ARP request"));
        }
        let ethernet_destination = &frame[..6];
        let sender_hardware = &frame[offset + 8..offset + 14];
        let target_hardware = &frame[offset + 18..offset + 24];
        let broadcast_request =
            ethernet_destination == ETHERNET_BROADCAST && target_hardware == ETHERNET_UNSPECIFIED;
        let unicast_refresh = ethernet_destination[0] & 1 == 0
            && ethernet_destination != ETHERNET_UNSPECIFIED
            && target_hardware == ethernet_destination;
        let sender = read_ipv4(frame, offset + 14);
        let target = read_ipv4(frame, offset + 24);
        if sender_hardware != self.guest_mac.to_bytes()
            || sender != self.guest_ipv4
            || !(match &self.mode {
                EgressPolicyMode::Rules { .. } => self.rules_allow_arp(target),
                _ => self.next_hops.contains(&target),
            })
            || (!broadcast_request && !unicast_refresh)
        {
            return Err(EgressDenied::ArpDenied);
        }
        Ok(())
    }

    fn authorize_ipv4(
        &self,
        frame: &[u8],
        frame_length: usize,
        offset: usize,
    ) -> Result<(), EgressDenied> {
        ensure_available(frame, frame_length, offset + 20, "IPv4 header")?;
        let version_ihl = frame[offset];
        let header_length = usize::from(version_ihl & 0x0f) * 4;
        if version_ihl >> 4 != 4 || header_length < 20 {
            return Err(EgressDenied::Malformed("IPv4 header"));
        }
        if header_length != 20 {
            return Err(EgressDenied::Ipv4OptionsDenied);
        }
        ensure_available(frame, frame_length, offset + header_length, "IPv4 options")?;
        let total_length = usize::from(read_u16(frame, offset + 2));
        if total_length < header_length
            || offset
                .checked_add(total_length)
                .is_none_or(|end| end > frame_length)
        {
            return Err(EgressDenied::Malformed("IPv4 total length"));
        }
        if !ipv4_checksum_is_valid(&frame[offset..offset + header_length]) {
            return Err(EgressDenied::Malformed("IPv4 checksum"));
        }
        if read_ipv4(frame, offset + 12) != self.guest_ipv4 {
            return Err(EgressDenied::SourceAddressDenied);
        }

        let destination = read_ipv4(frame, offset + 16);
        let fragments = read_u16(frame, offset + 6) & 0x3fff;
        if let Some(proxy) = self.proxy_endpoint
            && destination == proxy.address
            && fragments == 0
            && frame[offset + 9] == 6
        {
            validate_transport(
                frame,
                frame_length,
                offset,
                header_length,
                total_length,
                fragments,
            )?;
            if read_u16(frame, offset + header_length + 2) == proxy.port {
                return Ok(());
            }
        }
        match &self.mode {
            EgressPolicyMode::AllowAll => Ok(()),
            EgressPolicyMode::DenyAll => Err(EgressDenied::AllTrafficDenied),
            EgressPolicyMode::AllowList(rules) => {
                validate_transport(
                    frame,
                    frame_length,
                    offset,
                    header_length,
                    total_length,
                    fragments,
                )?;
                if rules.iter().any(|rule| rule.contains(destination)) {
                    Ok(())
                } else {
                    Err(EgressDenied::DestinationDenied)
                }
            }
            EgressPolicyMode::BlockList(rules) => {
                validate_transport(
                    frame,
                    frame_length,
                    offset,
                    header_length,
                    total_length,
                    fragments,
                )?;
                if rules.iter().any(|rule| rule.contains(destination)) {
                    Err(EgressDenied::DestinationDenied)
                } else {
                    Ok(())
                }
            }
            EgressPolicyMode::TcpEndpoints(endpoints) => {
                if fragments != 0 || frame[offset + 9] != 6 {
                    return Err(EgressDenied::EndpointDenied);
                }
                let tcp_offset = offset + header_length;
                ensure_available(frame, frame_length, tcp_offset + 20, "TCP header")?;
                if total_length < header_length + 20 {
                    return Err(EgressDenied::Malformed("TCP length"));
                }
                let tcp_header_length = usize::from(frame[tcp_offset + 12] >> 4) * 4;
                if tcp_header_length < 20 || total_length < header_length + tcp_header_length {
                    return Err(EgressDenied::Malformed("TCP header"));
                }
                let destination_port = read_u16(frame, tcp_offset + 2);
                if endpoints.iter().any(|endpoint| {
                    endpoint.address == destination && endpoint.port == destination_port
                }) {
                    Ok(())
                } else {
                    Err(EgressDenied::EndpointDenied)
                }
            }
            EgressPolicyMode::Rules { .. } => {
                let transport = rule_transport(
                    frame,
                    frame_length,
                    offset,
                    header_length,
                    total_length,
                    fragments,
                    self.rules_have_transport_restrictions(),
                )?;
                if self.rules_allow_destination(destination, transport) {
                    Ok(())
                } else {
                    Err(EgressDenied::DestinationDenied)
                }
            }
        }
    }
}

/// Reason an active egress policy rejected a frame.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum EgressDenied {
    /// The configured posture denies every guest-originated frame.
    #[error("all egress traffic is denied")]
    AllTrafficDenied,
    /// A required packet structure was truncated or internally inconsistent.
    #[error("malformed {0}")]
    Malformed(&'static str),
    /// The policy does not permit this layer-2 protocol.
    #[error("unsupported Ethernet type {0:#06x}")]
    UnsupportedEtherType(u16),
    /// Exact endpoint mode does not permit VLAN encapsulation.
    #[error("VLAN traffic is denied by exact endpoint policy")]
    VlanDenied,
    /// The Ethernet source does not match the bound guest identity.
    #[error("Ethernet source does not match the configured guest")]
    SourceMacDenied,
    /// ARP was not a canonical request for an authorized next hop.
    #[error("ARP request does not resolve an authorized next hop")]
    ArpDenied,
    /// The packet tried to spoof another source IPv4 address.
    #[error("IPv4 source does not match the configured guest")]
    SourceAddressDenied,
    /// IPv4 options are not supported because source routing can change the
    /// effective destination after policy evaluation.
    #[error("IPv4 options are denied by policy")]
    Ipv4OptionsDenied,
    /// An allow-list or block-list denied the destination.
    #[error("IPv4 destination is denied")]
    DestinationDenied,
    /// Exact endpoint mode denied the protocol, address, or TCP port.
    #[error("traffic does not match an allowed TCP endpoint")]
    EndpointDenied,
    /// Port-specific rules cannot safely authorize fragmented transport headers.
    #[error("IPv4 fragments are denied by port-specific egress policy")]
    FragmentDenied,
}

fn prefix_mask(prefix_length: u8) -> u32 {
    if prefix_length == 0 {
        0
    } else {
        u32::MAX << (32 - prefix_length)
    }
}

fn contains_address(network: Ipv4Addr, mask: u32, address: Ipv4Addr) -> bool {
    u32::from(address) & mask == u32::from(network)
}

fn invalid_special_endpoint_address(
    address: Ipv4Addr,
    guest: Ipv4Addr,
    network: Ipv4Addr,
    broadcast: Ipv4Addr,
) -> Option<InvalidEndpointAddress> {
    let first = address.octets()[0];
    if address.is_unspecified() {
        Some(InvalidEndpointAddress::Unspecified)
    } else if first == 0 {
        Some(InvalidEndpointAddress::CurrentNetwork)
    } else if address.is_loopback() {
        Some(InvalidEndpointAddress::Loopback)
    } else if address.is_link_local() {
        Some(InvalidEndpointAddress::LinkLocal)
    } else if address.is_multicast() {
        Some(InvalidEndpointAddress::Multicast)
    } else if address.is_broadcast() {
        Some(InvalidEndpointAddress::LimitedBroadcast)
    } else if first >= 240 {
        Some(InvalidEndpointAddress::Reserved)
    } else if address == guest {
        Some(InvalidEndpointAddress::Guest)
    } else if address == network {
        Some(InvalidEndpointAddress::SubnetNetwork)
    } else if address == broadcast {
        Some(InvalidEndpointAddress::SubnetBroadcast)
    } else {
        None
    }
}

fn append_policy_mode(bytes: &mut Vec<u8>, mode: &EgressPolicyMode) {
    match mode {
        EgressPolicyMode::AllowAll => bytes.push(0),
        EgressPolicyMode::DenyAll => bytes.push(4),
        EgressPolicyMode::AllowList(rules) => {
            bytes.push(1);
            append_cidrs(bytes, rules);
        }
        EgressPolicyMode::BlockList(rules) => {
            bytes.push(2);
            append_cidrs(bytes, rules);
        }
        EgressPolicyMode::TcpEndpoints(endpoints) => {
            bytes.push(3);
            let mut endpoints = endpoints.clone();
            endpoints.sort_unstable();
            endpoints.dedup();
            for endpoint in endpoints {
                bytes.extend_from_slice(&endpoint.address.octets());
                bytes.extend_from_slice(&endpoint.port.to_be_bytes());
            }
        }
        EgressPolicyMode::Rules {
            default_action,
            allow,
            deny,
        } => {
            bytes.push(5);
            bytes.push(match default_action {
                EgressAction::Allow => 0,
                EgressAction::Deny => 1,
            });
            append_rules(bytes, allow);
            append_rules(bytes, deny);
        }
    }
}

fn append_cidrs(bytes: &mut Vec<u8>, rules: &[Ipv4Cidr]) {
    let mut rules = rules.to_vec();
    rules.sort_unstable();
    rules.dedup();
    for rule in rules {
        bytes.extend_from_slice(&rule.network.octets());
        bytes.push(rule.prefix_length);
    }
}

fn append_rules(bytes: &mut Vec<u8>, rules: &[EgressRule]) {
    let mut rules = rules.to_vec();
    rules.sort_unstable();
    rules.dedup();
    bytes.extend_from_slice(&(rules.len() as u64).to_be_bytes());
    for rule in rules {
        bytes.extend_from_slice(&rule.destination.network.octets());
        bytes.push(rule.destination.prefix_length);
        bytes.push(match rule.transport {
            EgressTransport::Any => 0,
            EgressTransport::Tcp => 1,
            EgressTransport::Udp => 2,
        });
        bytes.extend_from_slice(&rule.port.to_be_bytes());
    }
}

fn ensure_available(
    prefix: &[u8],
    frame_length: usize,
    needed: usize,
    description: &'static str,
) -> Result<(), EgressDenied> {
    if needed > frame_length || needed > prefix.len() {
        Err(EgressDenied::Malformed(description))
    } else {
        Ok(())
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_ipv4(bytes: &[u8], offset: usize) -> Ipv4Addr {
    Ipv4Addr::new(
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    )
}

fn ipv4_checksum_is_valid(header: &[u8]) -> bool {
    let mut sum = header
        .chunks_exact(2)
        .fold(0u32, |sum, word| sum + u32::from(read_u16(word, 0)));
    while sum > u32::from(u16::MAX) {
        sum = (sum & u32::from(u16::MAX)) + (sum >> 16);
    }
    sum == u32::from(u16::MAX)
}

fn validate_transport(
    frame: &[u8],
    frame_length: usize,
    ip_offset: usize,
    ip_header_length: usize,
    ip_total_length: usize,
    fragments: u16,
) -> Result<(), EgressDenied> {
    if fragments != 0 {
        return Ok(());
    }
    let transport_offset = ip_offset + ip_header_length;
    let payload_length = ip_total_length - ip_header_length;
    match frame[ip_offset + 9] {
        6 => {
            ensure_available(frame, frame_length, transport_offset + 20, "TCP header")?;
            if payload_length < 20 {
                return Err(EgressDenied::Malformed("TCP length"));
            }
            let header_length = usize::from(frame[transport_offset + 12] >> 4) * 4;
            if header_length < 20 || header_length > payload_length {
                return Err(EgressDenied::Malformed("TCP header"));
            }
        }
        17 => {
            ensure_available(frame, frame_length, transport_offset + 8, "UDP header")?;
            let length = usize::from(read_u16(frame, transport_offset + 4));
            if length < 8 || length > payload_length {
                return Err(EgressDenied::Malformed("UDP length"));
            }
        }
        1 => {
            ensure_available(frame, frame_length, transport_offset + 8, "ICMP header")?;
            if payload_length < 8 {
                return Err(EgressDenied::Malformed("ICMP length"));
            }
        }
        _ => {}
    }
    Ok(())
}

fn rule_transport(
    frame: &[u8],
    frame_length: usize,
    ip_offset: usize,
    ip_header_length: usize,
    ip_total_length: usize,
    fragments: u16,
    has_transport_restrictions: bool,
) -> Result<Option<(EgressTransport, u16)>, EgressDenied> {
    validate_transport(
        frame,
        frame_length,
        ip_offset,
        ip_header_length,
        ip_total_length,
        fragments,
    )?;
    if fragments != 0 {
        return if has_transport_restrictions {
            Err(EgressDenied::FragmentDenied)
        } else {
            Ok(None)
        };
    }
    let transport_offset = ip_offset + ip_header_length;
    Ok(match frame[ip_offset + 9] {
        6 => Some((EgressTransport::Tcp, read_u16(frame, transport_offset + 2))),
        17 => Some((EgressTransport::Udp, read_u16(frame, transport_offset + 2))),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    const GUEST_IPV4: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);
    const PREFIX_LENGTH: u8 = 24;
    const GATEWAY_IPV4: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);
    const GUEST_MAC_BYTES: [u8; 6] = [0x52, 0x54, 0, 0, 0, 2];

    fn bind(mode: EgressPolicyMode) -> EgressPolicy {
        EgressPolicy::bind(
            GUEST_IPV4,
            PREFIX_LENGTH,
            MacAddress::new(GUEST_MAC_BYTES),
            GATEWAY_IPV4,
            mode,
        )
        .unwrap()
    }

    fn set_ipv4_checksum(frame: &mut [u8], ip_offset: usize) {
        frame[ip_offset + 10..ip_offset + 12].fill(0);
        let header_length = usize::from(frame[ip_offset] & 0x0f) * 4;
        let header = &frame[ip_offset..ip_offset + header_length];
        let mut sum = header
            .chunks_exact(2)
            .fold(0u32, |sum, word| sum + u32::from(read_u16(word, 0)));
        while sum > u32::from(u16::MAX) {
            sum = (sum & u32::from(u16::MAX)) + (sum >> 16);
        }
        frame[ip_offset + 10..ip_offset + 12].copy_from_slice(&(!(sum as u16)).to_be_bytes());
    }

    fn tcp_frame(destination: Ipv4Addr, destination_port: u16) -> Vec<u8> {
        let mut frame = vec![0u8; 14 + 20 + 20];
        frame[6..12].copy_from_slice(&GUEST_MAC_BYTES);
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        let ip = &mut frame[14..];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&40u16.to_be_bytes());
        ip[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
        ip[8] = 64;
        ip[9] = 6;
        ip[12..16].copy_from_slice(&Ipv4Addr::new(10, 0, 0, 2).octets());
        ip[16..20].copy_from_slice(&destination.octets());
        ip[20..22].copy_from_slice(&12345u16.to_be_bytes());
        ip[22..24].copy_from_slice(&destination_port.to_be_bytes());
        ip[32] = 5 << 4;
        set_ipv4_checksum(&mut frame, 14);
        frame
    }

    fn udp_frame(destination: Ipv4Addr, destination_port: u16) -> Vec<u8> {
        let mut frame = vec![0u8; 14 + 20 + 8];
        frame[6..12].copy_from_slice(&GUEST_MAC_BYTES);
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        let ip = &mut frame[14..];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&28u16.to_be_bytes());
        ip[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
        ip[8] = 64;
        ip[9] = 17;
        ip[12..16].copy_from_slice(&Ipv4Addr::new(10, 0, 0, 2).octets());
        ip[16..20].copy_from_slice(&destination.octets());
        ip[20..22].copy_from_slice(&12345u16.to_be_bytes());
        ip[22..24].copy_from_slice(&destination_port.to_be_bytes());
        ip[24..26].copy_from_slice(&8u16.to_be_bytes());
        set_ipv4_checksum(&mut frame, 14);
        frame
    }

    fn arp_request(
        target: Ipv4Addr,
        ethernet_destination: [u8; 6],
        target_hardware: [u8; 6],
    ) -> Vec<u8> {
        let mut frame = vec![0u8; 42];
        frame[..6].copy_from_slice(&ethernet_destination);
        frame[6..12].copy_from_slice(&GUEST_MAC_BYTES);
        frame[12..14].copy_from_slice(&0x0806u16.to_be_bytes());
        frame[14..16].copy_from_slice(&1u16.to_be_bytes());
        frame[16..18].copy_from_slice(&0x0800u16.to_be_bytes());
        frame[18] = 6;
        frame[19] = 4;
        frame[20..22].copy_from_slice(&1u16.to_be_bytes());
        frame[22..28].copy_from_slice(&GUEST_MAC_BYTES);
        frame[28..32].copy_from_slice(&GUEST_IPV4.octets());
        frame[32..38].copy_from_slice(&target_hardware);
        frame[38..42].copy_from_slice(&target.octets());
        frame
    }

    #[test]
    fn policy_rules_are_parsed_and_canonicalized() {
        let policy = bind(EgressPolicyMode::AllowList(vec![
            "192.168.1.9/24".parse().unwrap(),
            "10.0.0.1".parse().unwrap(),
            "192.168.1.0/24".parse().unwrap(),
        ]));
        let EgressPolicyMode::AllowList(rules) = policy.mode() else {
            panic!("unexpected policy mode")
        };
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[1].network(), Ipv4Addr::new(192, 168, 1, 0));
        assert_eq!(policy.next_hops(), &[GATEWAY_IPV4]);
        assert!(policy.allows_gateway_dns());
        assert!(policy.validate().is_ok());
        assert_ne!(
            policy
                .canonical_bytes_for_version(LEGACY_EGRESS_POLICY_ENCODING_VERSION)
                .unwrap(),
            policy.canonical_bytes()
        );
    }

    #[test]
    fn exact_endpoint_policy_filters_before_transmission() {
        let policy = bind(EgressPolicyMode::TcpEndpoints(vec![
            "192.0.2.7:443".parse().unwrap(),
        ]));
        let allowed = tcp_frame(Ipv4Addr::new(192, 0, 2, 7), 443);
        policy.authorize_frame(&allowed, allowed.len()).unwrap();

        let denied = tcp_frame(Ipv4Addr::new(192, 0, 2, 7), 80);
        assert_eq!(
            policy.authorize_frame(&denied, denied.len()),
            Err(EgressDenied::EndpointDenied)
        );
        let mut malformed = allowed.clone();
        malformed[24] ^= 1;
        assert_eq!(
            policy.authorize_frame(&malformed, malformed.len()),
            Err(EgressDenied::Malformed("IPv4 checksum"))
        );
        let mut spoofed_mac = allowed.clone();
        spoofed_mac[11] ^= 1;
        assert_eq!(
            policy.authorize_frame(&spoofed_mac, spoofed_mac.len()),
            Err(EgressDenied::SourceMacDenied)
        );

        let mut with_options = allowed.clone();
        with_options.splice(34..34, [1, 1, 1, 0]);
        with_options[14] = 0x46;
        with_options[16..18].copy_from_slice(&44u16.to_be_bytes());
        set_ipv4_checksum(&mut with_options, 14);
        assert_eq!(
            policy.authorize_frame(&with_options, with_options.len()),
            Err(EgressDenied::Ipv4OptionsDenied)
        );

        let udp = udp_frame(Ipv4Addr::new(192, 0, 2, 7), 443);
        assert_eq!(
            policy.authorize_frame(&udp, udp.len()),
            Err(EgressDenied::EndpointDenied)
        );
        let mut ipv6 = vec![0u8; 14 + 40];
        ipv6[6..12].copy_from_slice(&GUEST_MAC_BYTES);
        ipv6[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
        assert_eq!(
            policy.authorize_frame(&ipv6, ipv6.len()),
            Err(EgressDenied::UnsupportedEtherType(0x86dd))
        );
        let mut vlan = vec![0u8; allowed.len() + 4];
        vlan[..12].copy_from_slice(&allowed[..12]);
        vlan[12..14].copy_from_slice(&0x8100u16.to_be_bytes());
        vlan[16..18].copy_from_slice(&0x0800u16.to_be_bytes());
        vlan[18..].copy_from_slice(&allowed[14..]);
        assert_eq!(
            policy.authorize_frame(&vlan, vlan.len()),
            Err(EgressDenied::VlanDenied)
        );
    }

    #[test]
    fn endpoint_policy_resolves_only_required_next_hops_without_widening_ipv4() {
        let on_link = Ipv4Addr::new(10, 0, 0, 9);
        let off_subnet = Ipv4Addr::new(192, 0, 2, 7);
        let unrelated = Ipv4Addr::new(10, 0, 0, 10);
        let policy = bind(EgressPolicyMode::TcpEndpoints(vec![
            "10.0.0.9:8443".parse().unwrap(),
            "192.0.2.7:443".parse().unwrap(),
            "10.0.0.9:443".parse().unwrap(),
            "10.0.0.9:443".parse().unwrap(),
        ]));

        assert_eq!(policy.next_hops(), &[GATEWAY_IPV4, on_link]);
        for target in [GATEWAY_IPV4, on_link] {
            let request = arp_request(target, ETHERNET_BROADCAST, ETHERNET_UNSPECIFIED);
            policy
                .authorize_frame(&request, request.len())
                .expect("required next hop must be resolvable");
        }
        let unrelated_request = arp_request(unrelated, ETHERNET_BROADCAST, ETHERNET_UNSPECIFIED);
        assert_eq!(
            policy.authorize_frame(&unrelated_request, unrelated_request.len()),
            Err(EgressDenied::ArpDenied)
        );

        policy
            .authorize_frame(&tcp_frame(on_link, 443), 54)
            .unwrap();
        policy
            .authorize_frame(&tcp_frame(off_subnet, 443), 54)
            .unwrap();
        let wrong_port = tcp_frame(on_link, 80);
        assert_eq!(
            policy.authorize_frame(&wrong_port, wrong_port.len()),
            Err(EgressDenied::EndpointDenied)
        );
        let unrelated_ipv4 = tcp_frame(unrelated, 443);
        assert_eq!(
            policy.authorize_frame(&unrelated_ipv4, unrelated_ipv4.len()),
            Err(EgressDenied::EndpointDenied)
        );
    }

    #[test]
    fn endpoint_policy_rejects_invalid_destination_identities() {
        let invalid = [
            (Ipv4Addr::UNSPECIFIED, InvalidEndpointAddress::Unspecified),
            (
                Ipv4Addr::new(0, 1, 2, 3),
                InvalidEndpointAddress::CurrentNetwork,
            ),
            (
                Ipv4Addr::new(127, 0, 0, 1),
                InvalidEndpointAddress::Loopback,
            ),
            (
                Ipv4Addr::new(169, 254, 1, 1),
                InvalidEndpointAddress::LinkLocal,
            ),
            (
                Ipv4Addr::new(224, 0, 0, 1),
                InvalidEndpointAddress::Multicast,
            ),
            (
                Ipv4Addr::BROADCAST,
                InvalidEndpointAddress::LimitedBroadcast,
            ),
            (
                Ipv4Addr::new(240, 0, 0, 1),
                InvalidEndpointAddress::Reserved,
            ),
            (GUEST_IPV4, InvalidEndpointAddress::Guest),
            (
                Ipv4Addr::new(10, 0, 0, 0),
                InvalidEndpointAddress::SubnetNetwork,
            ),
            (
                Ipv4Addr::new(10, 0, 0, 255),
                InvalidEndpointAddress::SubnetBroadcast,
            ),
        ];

        for (address, reason) in invalid {
            let endpoint = format!("{address}:443").parse().unwrap();
            assert_eq!(
                EgressPolicy::bind(
                    GUEST_IPV4,
                    PREFIX_LENGTH,
                    MacAddress::new(GUEST_MAC_BYTES),
                    GATEWAY_IPV4,
                    EgressPolicyMode::TcpEndpoints(vec![endpoint]),
                ),
                Err(InvalidEgressPolicy::InvalidEndpoint { address, reason })
            );
        }
    }

    #[test]
    fn decoded_policy_must_preserve_canonical_rules_and_next_hops() {
        let mut noncanonical_rules = bind(EgressPolicyMode::TcpEndpoints(vec![
            "10.0.0.9:443".parse().unwrap(),
        ]));
        let EgressPolicyMode::TcpEndpoints(endpoints) = &mut noncanonical_rules.mode else {
            panic!("expected endpoint policy")
        };
        endpoints.push("10.0.0.9:443".parse().unwrap());
        assert_eq!(
            noncanonical_rules.validate(),
            Err(InvalidEgressPolicy::NonCanonicalRules)
        );

        let mut invalid_next_hops = bind(EgressPolicyMode::TcpEndpoints(vec![
            "10.0.0.9:443".parse().unwrap(),
        ]));
        invalid_next_hops
            .next_hops
            .push(Ipv4Addr::new(10, 0, 0, 10));
        assert_eq!(
            invalid_next_hops.validate(),
            Err(InvalidEgressPolicy::InvalidNextHops)
        );
    }

    #[test]
    fn arp_requires_canonical_guest_requests() {
        let on_link = Ipv4Addr::new(10, 0, 0, 9);
        let peer_mac = [0x52, 0x54, 0, 0, 0, 9];
        let policy = bind(EgressPolicyMode::TcpEndpoints(vec![
            "10.0.0.9:443".parse().unwrap(),
        ]));

        let broadcast = arp_request(on_link, ETHERNET_BROADCAST, ETHERNET_UNSPECIFIED);
        policy.authorize_frame(&broadcast, broadcast.len()).unwrap();
        let unicast_refresh = arp_request(on_link, peer_mac, peer_mac);
        policy
            .authorize_frame(&unicast_refresh, unicast_refresh.len())
            .unwrap();

        let mut ethernet_spoof = broadcast.clone();
        ethernet_spoof[11] ^= 1;
        assert_eq!(
            policy.authorize_frame(&ethernet_spoof, ethernet_spoof.len()),
            Err(EgressDenied::SourceMacDenied)
        );
        let mut sender_hardware_spoof = broadcast.clone();
        sender_hardware_spoof[27] ^= 1;
        assert_eq!(
            policy.authorize_frame(&sender_hardware_spoof, sender_hardware_spoof.len()),
            Err(EgressDenied::ArpDenied)
        );
        let mut sender_ip_spoof = broadcast.clone();
        sender_ip_spoof[31] ^= 1;
        assert_eq!(
            policy.authorize_frame(&sender_ip_spoof, sender_ip_spoof.len()),
            Err(EgressDenied::ArpDenied)
        );
        let mut noncanonical_target_hardware = broadcast.clone();
        noncanonical_target_hardware[37] = 1;
        assert_eq!(
            policy.authorize_frame(
                &noncanonical_target_hardware,
                noncanonical_target_hardware.len()
            ),
            Err(EgressDenied::ArpDenied)
        );
        let mut reply = broadcast.clone();
        reply[20..22].copy_from_slice(&2u16.to_be_bytes());
        assert_eq!(
            policy.authorize_frame(&reply, reply.len()),
            Err(EgressDenied::Malformed("ARP request"))
        );
        assert_eq!(
            policy.authorize_frame(&broadcast[..40], broadcast.len()),
            Err(EgressDenied::Malformed("ARP packet"))
        );
    }

    #[test]
    fn list_policies_filter_destinations_and_allow_only_gateway_arp() {
        let allowed_destination = Ipv4Addr::new(192, 0, 2, 7);
        let blocked_destination = Ipv4Addr::new(198, 51, 100, 9);
        let allow = bind(EgressPolicyMode::AllowList(vec![
            "192.0.2.0/24".parse().unwrap(),
        ]));
        let allowed = tcp_frame(allowed_destination, 80);
        allow.authorize_frame(&allowed, allowed.len()).unwrap();
        let denied = tcp_frame(blocked_destination, 80);
        assert_eq!(
            allow.authorize_frame(&denied, denied.len()),
            Err(EgressDenied::DestinationDenied)
        );

        let block = bind(EgressPolicyMode::BlockList(vec![
            "198.51.100.0/24".parse().unwrap(),
        ]));
        block.authorize_frame(&allowed, allowed.len()).unwrap();
        assert_eq!(
            block.authorize_frame(&denied, denied.len()),
            Err(EgressDenied::DestinationDenied)
        );

        let gateway_request = arp_request(GATEWAY_IPV4, ETHERNET_BROADCAST, ETHERNET_UNSPECIFIED);
        allow
            .authorize_frame(&gateway_request, gateway_request.len())
            .unwrap();
        let other_arp = arp_request(
            Ipv4Addr::new(10, 0, 0, 9),
            ETHERNET_BROADCAST,
            ETHERNET_UNSPECIFIED,
        );
        assert_eq!(
            allow.authorize_frame(&other_arp, other_arp.len()),
            Err(EgressDenied::ArpDenied)
        );
    }

    #[test]
    fn l3_l4_rules_parse_and_apply_deny_precedence() {
        assert_eq!(
            "192.0.2.9/24:tcp:443".parse::<EgressRule>().unwrap(),
            EgressRule {
                destination: "192.0.2.0/24".parse().unwrap(),
                transport: EgressTransport::Tcp,
                port: 443,
            }
        );
        assert_eq!(
            "198.51.100.0/24".parse::<EgressRule>().unwrap().transport(),
            EgressTransport::Any
        );
        for invalid in [
            "192.0.2.0/24:icmp:8",
            "192.0.2.0/24:tcp:0",
            "192.0.2.0/24:tcp",
            "192.0.2.0/24:tcp:443:extra",
        ] {
            assert!(invalid.parse::<EgressRule>().is_err(), "{invalid}");
        }

        let policy = bind(EgressPolicyMode::Rules {
            default_action: EgressAction::Deny,
            allow: vec![
                "192.0.2.0/24:tcp:443".parse().unwrap(),
                "192.0.2.0/24:udp:53".parse().unwrap(),
                "10.0.0.9".parse().unwrap(),
                "192.0.2.0/24:tcp:443".parse().unwrap(),
            ],
            deny: vec!["192.0.2.7:tcp:443".parse().unwrap()],
        });
        assert_eq!(policy.mode_name(), "rules");
        assert_eq!(policy.rule_counts(), (3, 1));
        assert_eq!(policy.next_hops(), &[GATEWAY_IPV4]);

        let allowed_tcp = tcp_frame(Ipv4Addr::new(192, 0, 2, 8), 443);
        policy
            .authorize_frame(&allowed_tcp, allowed_tcp.len())
            .unwrap();
        let denied_by_precedence = tcp_frame(Ipv4Addr::new(192, 0, 2, 7), 443);
        assert_eq!(
            policy.authorize_frame(&denied_by_precedence, denied_by_precedence.len()),
            Err(EgressDenied::DestinationDenied)
        );
        let wrong_port = tcp_frame(Ipv4Addr::new(192, 0, 2, 8), 80);
        assert_eq!(
            policy.authorize_frame(&wrong_port, wrong_port.len()),
            Err(EgressDenied::DestinationDenied)
        );
        let allowed_udp = udp_frame(Ipv4Addr::new(192, 0, 2, 7), 53);
        policy
            .authorize_frame(&allowed_udp, allowed_udp.len())
            .unwrap();

        let on_link = Ipv4Addr::new(10, 0, 0, 9);
        let on_link_arp = arp_request(on_link, ETHERNET_BROADCAST, ETHERNET_UNSPECIFIED);
        policy
            .authorize_frame(&on_link_arp, on_link_arp.len())
            .unwrap();

        let mut fragment = allowed_tcp;
        fragment[20..22].copy_from_slice(&0x2000u16.to_be_bytes());
        set_ipv4_checksum(&mut fragment, 14);
        assert_eq!(
            policy.authorize_frame(&fragment, fragment.len()),
            Err(EgressDenied::FragmentDenied)
        );
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn host_loopback_denial_preserves_only_the_exact_proxy() {
        let proxy: TcpEndpoint = "10.0.0.1:8443".parse().unwrap();
        let policy = bind(EgressPolicyMode::DenyAll)
            .with_host_loopback(EgressAction::Deny, Some(proxy))
            .unwrap();
        assert_eq!(policy.host_loopback_action(), EgressAction::Deny);
        assert_eq!(policy.proxy_endpoint(), Some(proxy));
        assert_eq!(policy.next_hops(), &[GATEWAY_IPV4]);

        let arp = arp_request(GATEWAY_IPV4, ETHERNET_BROADCAST, ETHERNET_UNSPECIFIED);
        policy.authorize_frame(&arp, arp.len()).unwrap();
        let allowed = tcp_frame(GATEWAY_IPV4, 8443);
        policy.authorize_frame(&allowed, allowed.len()).unwrap();
        let denied = tcp_frame(GATEWAY_IPV4, 8080);
        assert_eq!(
            policy.authorize_frame(&denied, denied.len()),
            Err(EgressDenied::AllTrafficDenied)
        );
        assert_ne!(
            policy
                .canonical_bytes_for_version(PREVIOUS_EGRESS_POLICY_ENCODING_VERSION)
                .unwrap(),
            policy.canonical_bytes()
        );
        assert!(policy.validate().is_ok());

        let invalid_proxy: TcpEndpoint = "192.0.2.1:8443".parse().unwrap();
        assert_eq!(
            bind(EgressPolicyMode::AllowAll)
                .with_host_loopback(EgressAction::Deny, Some(invalid_proxy)),
            Err(InvalidEgressPolicy::InvalidProxyAddress {
                actual: Ipv4Addr::new(192, 0, 2, 1),
                expected: GATEWAY_IPV4,
            })
        );
    }

    #[test]
    fn allow_all_does_not_parse_guest_frames() {
        let policy = bind(EgressPolicyMode::AllowAll);
        policy.authorize_frame(&[], 0).unwrap();
    }

    #[test]
    fn deny_all_rejects_every_guest_frame_before_parsing() {
        let policy = bind(EgressPolicyMode::DenyAll);
        assert!(policy.is_active());
        assert!(!policy.allows_gateway_dns());
        assert!(policy.next_hops().is_empty());

        for frame in [
            tcp_frame(Ipv4Addr::new(192, 0, 2, 7), 443),
            arp_request(GATEWAY_IPV4, ETHERNET_BROADCAST, ETHERNET_UNSPECIFIED),
            Vec::new(),
        ] {
            assert_eq!(
                policy.authorize_frame(&frame, frame.len()),
                Err(EgressDenied::AllTrafficDenied)
            );
        }
    }
}
