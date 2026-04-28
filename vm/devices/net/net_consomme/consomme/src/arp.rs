// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Access;
use super::Client;
use super::DropReason;
use crate::ChecksumState;
use crate::MIN_MTU;
use crate::is_same_ipv4_subnet;
use smoltcp::wire::ArpOperation;
use smoltcp::wire::ArpPacket;
use smoltcp::wire::ArpRepr;
use smoltcp::wire::EthernetFrame;
use smoltcp::wire::EthernetProtocol;
use smoltcp::wire::EthernetRepr;

impl<T: Client> Access<'_, T> {
    pub(crate) fn handle_arp(
        &mut self,
        frame: &EthernetRepr,
        payload: &[u8],
    ) -> Result<(), DropReason> {
        let arp = ArpRepr::parse(&ArpPacket::new_unchecked(payload))?;
        let ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Request,
            source_hardware_addr,
            source_protocol_addr,
            target_protocol_addr,
            ..
        } = arp
        else {
            return Err(DropReason::UnsupportedArp);
        };

        // Don't respond for the client IP or remote addresses.
        if self.inner.state.params.client_ip == target_protocol_addr
            || !is_same_ipv4_subnet(
                self.inner.state.params.gateway_ip,
                target_protocol_addr,
                self.inner.state.params.net_mask,
            )
        {
            tracing::debug!(?target_protocol_addr, gateway = %self.inner.state.params.gateway_ip, subnet_mask = %self.inner.state.params.net_mask, "Ignoring ARP request");
            return Ok(());
        }

        // For any addresses in the guest subnet, provide the gateway MAC address. This is the
        // standard mechanism to indicate all traffic flows through the gateway, even local subnet
        // traffic.
        let mac_address = self.inner.state.params.gateway_mac;
        let e_repr = EthernetRepr {
            src_addr: mac_address,
            dst_addr: frame.src_addr,
            ethertype: EthernetProtocol::Arp,
        };
        let arp_repr = ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Reply,
            source_hardware_addr: mac_address,
            source_protocol_addr: target_protocol_addr,
            target_hardware_addr: source_hardware_addr,
            target_protocol_addr: source_protocol_addr,
        };
        let mut buffer = [0; MIN_MTU];
        let mut response = EthernetFrame::new_unchecked(&mut buffer);
        e_repr.emit(&mut response);
        let mut arp_response = ArpPacket::new_unchecked(response.payload_mut());
        arp_repr.emit(&mut arp_response);
        let len = e_repr.buffer_len() + arp_repr.buffer_len();
        self.client.recv(&buffer[..len], &ChecksumState::NONE);
        Ok(())
    }
}
