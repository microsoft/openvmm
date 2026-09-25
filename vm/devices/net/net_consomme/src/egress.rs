// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Egress processing of the Consomme queue.
//!
//! The queue's transmit path assembles each guest frame from guest memory and
//! sends it to the Consomme stack.

use crate::ConsommeQueue;
use consomme::ChecksumState;
use net_backend::BufferAccess;
use net_backend::TxSegmentType;

impl ConsommeQueue {
    /// Sends the queued guest frames to the Consomme stack and completes all of
    /// them.
    pub(crate) fn process_tx(&mut self, pool: &mut dyn BufferAccess) {
        while let Some(head) = self.state.tx_avail.front() {
            let TxSegmentType::Head(meta) = &head.ty else {
                unreachable!()
            };
            let tx_id = meta.id;
            let checksum = ChecksumState {
                ipv4: meta.flags.offload_ip_header_checksum(),
                tcp: meta.flags.offload_tcp_checksum(),
                udp: meta.flags.offload_udp_checksum(),
                tso: meta
                    .flags
                    .offload_tcp_segmentation()
                    .then_some(meta.max_segment_size),
                gso: meta
                    .flags
                    .offload_udp_segmentation()
                    .then_some(meta.max_segment_size),
            };

            // Reuse the scratch buffer to avoid per-packet heap allocation.
            // TSO caps the assembled packet at 64 KiB; assert so a buggy
            // upstream caller can't permanently inflate the scratch buffer
            // (and thus the queue's steady-state memory) by feeding an
            // oversized `meta.len`.
            debug_assert!(
                meta.len as usize <= 64 * 1024,
                "tx packet len {} exceeds 64 KiB TSO bound",
                meta.len
            );
            let mut buf = std::mem::take(&mut self.state.tx_scratch);
            buf.clear();
            buf.resize(meta.len as usize, 0);
            let gm = pool.guest_memory();
            let mut offset = 0;
            for segment in self.state.tx_avail.drain(..meta.segment_count as usize) {
                let dest = &mut buf[offset..offset + segment.len as usize];
                if let Err(err) = gm.read_at(segment.gpa, dest) {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "memory write failure"
                    );
                }
                offset += segment.len as usize;
            }

            if let Err(err) = self.with_consomme(pool, |c| c.send(&buf, &checksum)) {
                tracing::debug!(error = &err as &dyn std::error::Error, "tx packet ignored");
                match err {
                    consomme::DropReason::SendBufferFull
                    | consomme::DropReason::DestinationNotAllowed
                    | consomme::DropReason::TcpConnectionLimit
                    | consomme::DropReason::UdpConnectionLimit
                    | consomme::DropReason::IcmpConnectionLimit => {
                        self.stats.tx_dropped.increment()
                    }
                    consomme::DropReason::UnsupportedEthertype(_)
                    | consomme::DropReason::UnsupportedIpProtocol(_)
                    | consomme::DropReason::UnsupportedIcmpv6(_)
                    | consomme::DropReason::UnsupportedDhcp(_)
                    | consomme::DropReason::UnsupportedArp
                    | consomme::DropReason::UnsupportedDhcpv6(_)
                    | consomme::DropReason::UnsupportedNdp(_) => self.stats.tx_unknown.increment(),
                    consomme::DropReason::Packet(_)
                    | consomme::DropReason::Ipv4Checksum
                    | consomme::DropReason::Io(_)
                    | consomme::DropReason::BadTcpState(_)
                    | consomme::DropReason::FragmentedPacket
                    | consomme::DropReason::IpLengthMismatch
                    | consomme::DropReason::MalformedPacket => self.stats.tx_errors.increment(),
                }
            }
            self.state.tx_scratch = buf;

            self.state.tx_ready.push_back(tx_id);
        }
    }
}
