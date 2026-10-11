// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Maps real host-local addresses to unique virtual addresses within the
//! consomme subnet. This is used to substitute addresses that would appear to be local (e.g. the
//! guest IP address) with virtual addresses, such that the guest should direct any responses back
//! through the interface the packet arrived, instead of going through any loopback logic.

use inspect::Inspect;
use std::collections::HashMap;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;

/// Bidirectional map between real host-local addresses and virtual addresses
/// within the consomme subnet.
#[derive(Debug, Clone)]
pub(crate) struct LocalAddrMap {
    /// Real host address → virtual subnet address.
    real_to_virtual: HashMap<IpAddr, IpAddr>,
    /// Virtual subnet address → real host address.
    virtual_to_real: HashMap<IpAddr, IpAddr>,
    /// Next available IPv4 host offset from the broadcast address, counting
    /// downward. Virtual addresses are allocated from the high end of the
    /// subnet to minimize collisions with real addresses which are typically
    /// assigned from the low end. Capped at /24 as there should only be a max of four
    /// addresses used (guest IP, gateway IP, virtual IP for guest IP and virtual IP for 127.0.0.1).
    next_ipv4_offset_from_end: u8,
    /// Next available IPv6 interface ID suffix for link-local virtual addresses.
    /// We allocate fe80::ff:fe00:NNNN addresses (using a range unlikely to
    /// collide with EUI-64 derived addresses).
    next_ipv6_id: u16,
    /// Next available IPv6 host offset from the end of a static IPv6 subnet,
    /// counting downward from 1 for its last address, which IPv6, unlike
    /// IPv4, does not reserve for broadcast.
    next_ipv6_offset_from_end: u8,
}

impl Inspect for LocalAddrMap {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        resp.field("mappings_count", self.real_to_virtual.len());
        resp.field("next_ipv4_offset_from_end", self.next_ipv4_offset_from_end);
        resp.field("next_ipv6_id", self.next_ipv6_id);
        resp.field("next_ipv6_offset_from_end", self.next_ipv6_offset_from_end);
    }
}

impl LocalAddrMap {
    pub fn new() -> Self {
        Self {
            real_to_virtual: HashMap::new(),
            virtual_to_real: HashMap::new(),
            next_ipv4_offset_from_end: 1,
            next_ipv6_id: 1,
            next_ipv6_offset_from_end: 1,
        }
    }

    /// Look up or allocate a virtual IPv4 address for the given real address.
    ///
    /// `subnet_base` is the network address (e.g. 10.0.0.0 for a 10.0.0.0/24).
    /// `net_mask` is the subnet mask used to verify candidates stay within the
    /// subnet. Returns `None` if the address pool is exhausted.
    pub fn get_or_allocate_v4(
        &mut self,
        real_addr: Ipv4Addr,
        subnet_base: Ipv4Addr,
        net_mask: Ipv4Addr,
        gateway_ip: Ipv4Addr,
        client_ip: Ipv4Addr,
    ) -> Option<Ipv4Addr> {
        let real_ip = IpAddr::V4(real_addr);
        if let Some(&virtual_ip) = self.real_to_virtual.get(&real_ip) {
            match virtual_ip {
                IpAddr::V4(v4) => return Some(v4),
                _ => unreachable!(),
            }
        }

        // Allocate from the high end of the subnet, counting downward.
        // This minimizes collisions with real addresses which are typically
        // assigned from the low end (gateway, client, DHCP pools, etc.).
        let base_u32 = u32::from(subnet_base);
        let mask_u32 = u32::from(net_mask);
        let host_bits = !mask_u32; // e.g. 0x000000FF for /24
        loop {
            if self.next_ipv4_offset_from_end == 0 {
                // Wrapped around — pool exhausted.
                tracelimit::warn_ratelimited!(
                    real_addr = %real_addr,
                    "Local IPv4 virtual address pool exhausted"
                );
                return None;
            }
            let offset_from_end = self.next_ipv4_offset_from_end;
            self.next_ipv4_offset_from_end =
                self.next_ipv4_offset_from_end.checked_add(1).unwrap_or(0);

            let host_part = host_bits - offset_from_end as u32;
            if host_part == 0 {
                // Reached the network address — pool exhausted.
                self.next_ipv4_offset_from_end = 0;
                tracelimit::warn_ratelimited!(
                    real_addr = %real_addr,
                    "Local IPv4 virtual address pool exhausted"
                );
                return None;
            }
            let candidate_u32 = base_u32 | host_part;
            let candidate = Ipv4Addr::from(candidate_u32);
            // Skip if this collides with the current gateway or client IP.
            if candidate == gateway_ip || candidate == client_ip {
                continue;
            }
            // Skip if already used as a virtual address for a different real address.
            if self.virtual_to_real.contains_key(&IpAddr::V4(candidate)) {
                continue;
            }

            self.real_to_virtual.insert(real_ip, IpAddr::V4(candidate));
            self.virtual_to_real.insert(IpAddr::V4(candidate), real_ip);
            return Some(candidate);
        }
    }

    /// Look up or allocate a virtual IPv6 link-local address for the given real
    /// address.
    ///
    /// Allocates from the `fe80::ff:fe00:NNNN:1` range which is distinct from
    /// EUI-64 derived addresses (those use `0xFFFE` in bytes 11-12 rather than
    /// `0x00FF:FE00` in bytes 10-13).
    pub fn get_or_allocate_v6(
        &mut self,
        real_addr: Ipv6Addr,
        gateway_ll: Ipv6Addr,
        client_ll: Option<Ipv6Addr>,
        client_routable: Option<Ipv6Addr>,
    ) -> Option<Ipv6Addr> {
        let real_ip = IpAddr::V6(real_addr);
        if let Some(&virtual_ip) = self.real_to_virtual.get(&real_ip) {
            match virtual_ip {
                IpAddr::V6(v6) => return Some(v6),
                _ => unreachable!(),
            }
        }

        loop {
            if self.next_ipv6_id == 0 {
                return None;
            }
            // Build fe80::ff:fe00:NNNN:1
            let id = self.next_ipv6_id;
            self.next_ipv6_id = self.next_ipv6_id.checked_add(1).unwrap_or(0);

            let octets: [u8; 16] = [
                0xfe,
                0x80,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0x00,
                0xff,
                0xfe,
                0x00,
                (id >> 8) as u8,
                id as u8,
                0x00,
                0x01,
            ];
            let candidate = Ipv6Addr::from_octets(octets);

            // Skip collisions with gateway or client addresses.
            if candidate == gateway_ll
                || client_ll.is_some_and(|c| c == candidate)
                || client_routable.is_some_and(|c| c == candidate)
            {
                continue;
            }
            if self.virtual_to_real.contains_key(&IpAddr::V6(candidate)) {
                continue;
            }

            self.real_to_virtual.insert(real_ip, IpAddr::V6(candidate));
            self.virtual_to_real.insert(IpAddr::V6(candidate), real_ip);
            return Some(candidate);
        }
    }

    /// Look up or allocate a virtual IPv6 address for the given real address
    /// inside the static IPv6 subnet of `gateway` and `prefix_len`.
    ///
    /// Addresses are allocated downward from the subnet's last address, which
    /// IPv6 does not reserve for broadcast, and skip the gateway and the
    /// client, as for IPv4. A guest answers a peer there from its address in
    /// the subnet, whereas it would answer a link-local peer from its
    /// link-local address. Returns `None` if the address pool is exhausted.
    pub fn get_or_allocate_v6_in_subnet(
        &mut self,
        real_addr: Ipv6Addr,
        gateway: Ipv6Addr,
        prefix_len: u8,
        client: Ipv6Addr,
    ) -> Option<Ipv6Addr> {
        let real_ip = IpAddr::V6(real_addr);
        if let Some(&virtual_ip) = self.real_to_virtual.get(&real_ip) {
            match virtual_ip {
                IpAddr::V6(v6) => return Some(v6),
                _ => unreachable!(),
            }
        }

        let host_bits = u128::MAX.checked_shr(u32::from(prefix_len)).unwrap_or(0);
        let network = u128::from(gateway) & !host_bits;
        loop {
            if self.next_ipv6_offset_from_end == 0 {
                tracelimit::warn_ratelimited!(
                    real_addr = %real_addr,
                    "Local IPv6 virtual address pool exhausted"
                );
                return None;
            }
            let offset_from_end = self.next_ipv6_offset_from_end;
            self.next_ipv6_offset_from_end = offset_from_end.checked_add(1).unwrap_or(0);

            let Some(host_part) = host_bits
                .checked_sub(u128::from(offset_from_end - 1))
                .filter(|&h| h != 0)
            else {
                // Reached the network address — pool exhausted.
                self.next_ipv6_offset_from_end = 0;
                tracelimit::warn_ratelimited!(
                    real_addr = %real_addr,
                    "Local IPv6 virtual address pool exhausted"
                );
                return None;
            };
            let candidate = Ipv6Addr::from(network | host_part);
            if candidate == gateway || candidate == client {
                continue;
            }
            if self.virtual_to_real.contains_key(&IpAddr::V6(candidate)) {
                continue;
            }

            self.real_to_virtual.insert(real_ip, IpAddr::V6(candidate));
            self.virtual_to_real.insert(IpAddr::V6(candidate), real_ip);
            return Some(candidate);
        }
    }

    /// Reverse-lookup: given a virtual address the guest is sending to,
    /// return the real host address it maps to.
    pub fn resolve_virtual(&self, virtual_addr: &IpAddr) -> Option<IpAddr> {
        self.virtual_to_real.get(virtual_addr).copied()
    }

    /// Remove all mappings (e.g., on network reconfiguration).
    pub fn clear(&mut self) {
        self.real_to_virtual.clear();
        self.virtual_to_real.clear();
        // Reset allocation counters.
        self.next_ipv4_offset_from_end = 1;
        self.next_ipv6_id = 1;
        self.next_ipv6_offset_from_end = 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allocate_v4() {
        let mut map = LocalAddrMap::new();
        let subnet_base = Ipv4Addr::new(10, 0, 0, 0);
        let net_mask = Ipv4Addr::new(255, 255, 255, 0);
        let gateway = Ipv4Addr::new(10, 0, 0, 1);
        let client = Ipv4Addr::new(10, 0, 0, 2);

        // First allocation should be .254 (high end of /24 subnet, skipping broadcast .255)
        let addr = map
            .get_or_allocate_v4(Ipv4Addr::LOCALHOST, subnet_base, net_mask, gateway, client)
            .unwrap();
        assert_eq!(addr, Ipv4Addr::new(10, 0, 0, 254));

        // Same real address should return the same virtual address.
        let addr2 = map
            .get_or_allocate_v4(Ipv4Addr::LOCALHOST, subnet_base, net_mask, gateway, client)
            .unwrap();
        assert_eq!(addr, addr2);

        // Different real address should get .253
        let addr3 = map
            .get_or_allocate_v4(
                Ipv4Addr::new(192, 168, 1, 5),
                subnet_base,
                net_mask,
                gateway,
                client,
            )
            .unwrap();
        assert_eq!(addr3, Ipv4Addr::new(10, 0, 0, 253));
    }

    #[test]
    fn test_reverse_lookup() {
        let mut map = LocalAddrMap::new();
        let subnet_base = Ipv4Addr::new(10, 0, 0, 0);
        let net_mask = Ipv4Addr::new(255, 255, 255, 0);
        let gateway = Ipv4Addr::new(10, 0, 0, 1);
        let client = Ipv4Addr::new(10, 0, 0, 2);

        let virtual_addr = map
            .get_or_allocate_v4(Ipv4Addr::LOCALHOST, subnet_base, net_mask, gateway, client)
            .unwrap();
        let resolved = map.resolve_virtual(&IpAddr::V4(virtual_addr));
        assert_eq!(resolved, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }

    #[test]
    fn test_skips_gateway_and_client() {
        let mut map = LocalAddrMap::new();
        // Set gateway=.254 and client=.253 to verify they are skipped.
        let subnet_base = Ipv4Addr::new(10, 0, 0, 0);
        let net_mask = Ipv4Addr::new(255, 255, 255, 0);
        let gateway = Ipv4Addr::new(10, 0, 0, 254);
        let client = Ipv4Addr::new(10, 0, 0, 253);

        let addr = map
            .get_or_allocate_v4(Ipv4Addr::LOCALHOST, subnet_base, net_mask, gateway, client)
            .unwrap();
        // Should skip .254 (gateway) and .253 (client), allocate .252
        assert_eq!(addr, Ipv4Addr::new(10, 0, 0, 252));
    }

    #[test]
    fn test_allocate_v6() {
        let mut map = LocalAddrMap::new();
        let gateway_ll = Ipv6Addr::new(0xfe80, 0, 0, 0, 0x5055, 0x0aff, 0xfe00, 0x0102);

        let addr = map
            .get_or_allocate_v6(Ipv6Addr::LOCALHOST, gateway_ll, None, None)
            .unwrap();
        // Should be fe80::00ff:fe00:0001:0001
        let expected = Ipv6Addr::new(0xfe80, 0, 0, 0, 0x00ff, 0xfe00, 0x0001, 0x0001);
        assert_eq!(addr, expected);
    }

    #[test]
    fn test_allocate_v6_in_subnet() {
        let mut map = LocalAddrMap::new();
        let gateway: Ipv6Addr = "fd00::a00:1".parse().unwrap();
        let client: Ipv6Addr = "fd00::a00:2".parse().unwrap();

        // The first address is the last of the /120: IPv6 has no broadcast.
        let addr = map
            .get_or_allocate_v6_in_subnet(Ipv6Addr::LOCALHOST, gateway, 120, client)
            .unwrap();
        assert_eq!(addr, "fd00::a00:ff".parse::<Ipv6Addr>().unwrap());
        assert_eq!(
            map.get_or_allocate_v6_in_subnet(Ipv6Addr::LOCALHOST, gateway, 120, client),
            Some(addr)
        );
        assert_eq!(
            map.resolve_virtual(&IpAddr::V6(addr)),
            Some(IpAddr::V6(Ipv6Addr::LOCALHOST))
        );
        let other: Ipv6Addr = "2001:db8::5".parse().unwrap();
        assert_eq!(
            map.get_or_allocate_v6_in_subnet(other, gateway, 120, client),
            Some("fd00::a00:fe".parse().unwrap())
        );

        // The gateway and the client are skipped, and the pool ends short of
        // the network address. The smallest static subnet, a /126, keeps its
        // last address for a host peer.
        for (prefix_len, client, expected) in [
            (
                125,
                "fd00::5",
                &["fd00::7", "fd00::6", "fd00::4", "fd00::3", "fd00::2"][..],
            ),
            (126, "fd00::2", &["fd00::3"][..]),
        ] {
            let mut map = LocalAddrMap::new();
            let gateway: Ipv6Addr = "fd00::1".parse().unwrap();
            let client: Ipv6Addr = client.parse().unwrap();
            let allocated = (0..8u16)
                .map_while(|index| {
                    map.get_or_allocate_v6_in_subnet(
                        Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, index),
                        gateway,
                        prefix_len,
                        client,
                    )
                })
                .collect::<Vec<_>>();
            let expected = expected
                .iter()
                .map(|address| address.parse::<Ipv6Addr>().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(allocated, expected, "/{prefix_len}");
        }
    }
}
