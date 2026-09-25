// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Transmit path of a TAP queue.
//!
//! `tx_avail` writes the guest packets to the TAP interface synchronously and
//! completes all of them, including the packets that the interface drops.

use crate::TapQueue;
use crate::build_vnet_hdr;
use crate::fixup_ipv4_header_checksum;
use crate::fixup_ipv6_payload_length;
use net_backend::BufferAccess;
use net_backend::TxError;
use net_backend::TxId;
use net_backend::TxSegment;
use net_backend::linearize;
use net_backend::next_packet;
use std::io::ErrorKind;
use std::io::Write;
use zerocopy::IntoBytes;

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
                let hdr = build_vnet_hdr(meta);
                let hdr_bytes = hdr.as_bytes();
                let mut packet = linearize(pool, segments)?;

                // Fix up the IPv4 header checksum when the frontend
                // requested IPv4 header checksum offload.
                //
                // The virtio vnet header has no mechanism for IPv4 header
                // checksum offload, so we compute it in software. This
                // also covers NDIS/netvsp LSO packets, where the guest
                // driver zeroes ip_check (NDIS convention); the kernel's
                // TAP GSO engine requires a valid checksum to segment
                // the packet correctly.
                // Same NDIS/LSO convention for IPv6: the guest zeroes the IPv6
                // payload-length field on segmentation-offload frames. IPv6 has
                // no header checksum (so the IPv4 fixup above never runs for it);
                // fix the length here so the kernel TAP GSO engine can segment.
                if meta.flags.offload_ip_header_checksum() && meta.flags.is_ipv4() {
                    fixup_ipv4_header_checksum(&mut packet, meta.l2_len as usize);
                }
                if meta.flags.offload_tcp_segmentation() && meta.flags.is_ipv6() {
                    fixup_ipv6_payload_length(&mut packet, meta.l2_len as usize);
                }

                let bufs = [
                    std::io::IoSlice::new(hdr_bytes),
                    std::io::IoSlice::new(&packet),
                ];
                match tap.write_vectored(&bufs) {
                    Ok(bytes_written) => {
                        assert_eq!(
                            bytes_written,
                            hdr_bytes.len() + packet.len(),
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
