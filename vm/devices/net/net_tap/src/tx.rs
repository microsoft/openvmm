// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Transmit path of a TAP queue.
//!
//! `tx_avail` writes the guest packets to the TAP interface synchronously and
//! completes all of them, including the packets that the interface drops.

use crate::TapQueue;
use crate::VirtioNetHdr;
use crate::build_vnet_hdr;
use crate::fixup_gso_pseudo_header;
use crate::fixup_ipv4_header_checksum;
use crate::fixup_ipv6_payload_length;
use net_backend::BufferAccess;
use net_backend::TxError;
use net_backend::TxId;
use net_backend::TxSegment;
use net_backend::VlanMetadata;
use net_backend::linearize;
use net_backend::next_packet;
use std::io::ErrorKind;
use std::io::Write;
use zerocopy::IntoBytes;

/// The destination and source MAC addresses, which an 802.1Q tag follows.
const ETHERNET_ADDRESSES_LEN: usize = 12;
const ETH_P_8021Q: u16 = 0x8100;
const VLAN_TAG_LEN: u16 = 4;

impl TapQueue {
    /// Implements `tx_avail`: writes the packets of `segments`.
    pub(crate) fn transmit(
        &mut self,
        pool: &mut dyn BufferAccess,
        segments: &mut &[TxSegment],
    ) -> anyhow::Result<(bool, usize)> {
        let n = segments.len();
        // Synchronously send packets received from the guest to host's network.
        if let Some(tap) = self.tap.as_mut() {
            while !segments.is_empty() {
                let (meta, _segs, _rest) = next_packet(segments);
                let mut hdr = build_vnet_hdr(meta);
                let mut packet = linearize(pool, segments)?;

                // Fix up the IPv4 header checksum when the frontend
                // requested IPv4 header checksum offload.
                //
                // The virtio vnet header has no mechanism for IPv4 header
                // checksum offload, so we compute it in software. This
                // also covers NDIS/netvsp LSO packets, where the guest
                // driver zeroes ip_check (NDIS convention). The host
                // validates the IPv4 header when the frame enters its IP
                // stack or a br_netfilter bridge, before segmentation, and
                // drops the frame if the checksum is wrong.
                // Same NDIS/LSO convention for IPv6: the guest zeroes the IPv6
                // payload-length field on segmentation-offload frames. IPv6 has
                // no header checksum (so the IPv4 fixup above never runs for it);
                // fix the length here, or the host's IPv6 input validation
                // truncates the frame to its header before segmentation.
                if meta.flags.offload_ip_header_checksum() && meta.flags.is_ipv4() {
                    fixup_ipv4_header_checksum(&mut packet, meta.l2_len as usize);
                }
                if (meta.flags.offload_tcp_segmentation() || meta.flags.offload_udp_segmentation())
                    && meta.flags.is_ipv6()
                {
                    fixup_ipv6_payload_length(&mut packet, meta.l2_len as usize);
                }

                // Convert the LSO length-less pseudo-header seed to the convention the
                // kernel's GSO path expects (pseudo-header with the full L4 length) so
                // segmentation produces correct per-segment checksums.
                if meta.flags.offload_tcp_segmentation() || meta.flags.offload_udp_segmentation() {
                    fixup_gso_pseudo_header(&mut packet, meta.l2_len as usize);
                }

                // A vnet header has no field for the tag, so the frame carries it. The
                // fixups above index the untagged frame, so this comes after them. A frame
                // too short to hold an Ethernet header stays as it is for the kernel to refuse.
                let tag = meta
                    .vlan
                    .filter(|_| packet.len() >= net_backend::ETHERNET_HEADER_LEN as usize)
                    .map(wire_vlan_tag);
                if tag.is_some() {
                    shift_vnet_hdr_for_tag(&mut hdr);
                }
                let (macs, after_macs) = packet.split_at(if tag.is_some() {
                    ETHERNET_ADDRESSES_LEN
                } else {
                    0
                });
                let hdr_bytes = hdr.as_bytes();
                let bufs = [
                    std::io::IoSlice::new(hdr_bytes),
                    std::io::IoSlice::new(macs),
                    std::io::IoSlice::new(tag.as_ref().map_or(&[], |tag| tag.as_slice())),
                    std::io::IoSlice::new(after_macs),
                ];
                match tap.write_vectored(&bufs) {
                    Ok(bytes_written) => {
                        assert_eq!(
                            bytes_written,
                            hdr_bytes.len() + packet.len() + tag.map_or(0, |tag| tag.len()),
                            "TAP should never partial write"
                        );
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        // dropped packet: buffer is full

                        // TODO: return partial transmit here. This relies on
                        // remembering this condition and polling for POLLOUT in
                        // poll_ready().
                    }
                    Err(err) if err.raw_os_error() == Some(libc::EIO) => {
                        // dropped packet: interface is not up
                    }
                    Err(err) => {
                        tracing::warn!(
                            error = &err as &dyn std::error::Error,
                            "write to TAP interface failed"
                        );
                    }
                }
            }
        }
        let completed_synchronously = true;
        Ok((completed_synchronously, n))
    }

    /// Implements `tx_poll`.
    pub(crate) fn poll_tx_done(&mut self, _done: &mut [TxId]) -> Result<usize, TxError> {
        // Packets are sent synchronously so there is no no need to check here if
        // sending has been completed.
        Ok(0)
    }
}

/// An 802.1Q tag as it sits in a frame: TPID, then TCI, both big-endian.
///
/// [`VlanMetadata`] holds the priority in its low bits and the wire in its high bits, so its raw
/// bits are not the TCI.
fn wire_vlan_tag(vlan: VlanMetadata) -> [u8; 4] {
    let tci = (u16::from(vlan.priority()) << 13)
        | (u16::from(vlan.drop_eligible_indicator()) << 12)
        | vlan.vlan_id();
    let [tpid_hi, tpid_lo] = ETH_P_8021Q.to_be_bytes();
    let [tci_hi, tci_lo] = tci.to_be_bytes();
    [tpid_hi, tpid_lo, tci_hi, tci_lo]
}

/// Moves a vnet header's offsets past a tag inserted after the MAC addresses. `gso_size` counts
/// payload, so it stays.
fn shift_vnet_hdr_for_tag(hdr: &mut VirtioNetHdr) {
    if hdr.flags.needs_csum() {
        hdr.csum_start = hdr.csum_start.saturating_add(VLAN_TAG_LEN);
    }
    if hdr.hdr_len != 0 {
        hdr.hdr_len = hdr.hdr_len.saturating_add(VLAN_TAG_LEN);
    }
}
