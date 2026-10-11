// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Egress policy enforcement of the Consomme endpoint.
//!
//! The endpoint accepts a run-scoped [`EgressPolicy`] while no queue is
//! active. The queue's transmit path assembles each guest frame from guest
//! memory and drops the frames that the policy denies before they reach the
//! Consomme stack. A reply candidate that the rules deny passes only when the
//! stack carries its flow, and the rules evaluate traffic to a virtual address
//! at which the stack presents a host-local peer where the stack routes it.
//! The endpoint also consumes the guest's listener announcements when it
//! publishes them, to the host's private networks or on host loopback alone.

use crate::ClientNoPool;
use crate::ConsommeEndpoint;
use crate::ConsommeQueue;
use anyhow::Context as _;
use consomme::ChecksumState;
use net_backend::BufferAccess;
use net_backend::TxSegmentType;
use net_backend_resources::egress::EgressPolicy;
use net_backend_resources::ingress::IngressTransport;

impl ConsommeEndpoint {
    /// Implements `set_egress_policy`.
    pub(crate) fn install_egress_policy(&mut self, policy: EgressPolicy) -> anyhow::Result<()> {
        self.endpoint_state
            .lock()
            .as_mut()
            .context("cannot configure Consomme egress policy while a queue is active")?
            .egress_policy = Some(policy);
        Ok(())
    }
}

impl ConsommeQueue {
    /// Applies the listener announcement that `frame` carries, if the
    /// endpoint publishes the guest's listeners, and returns whether the frame
    /// was one.
    fn publish_announcement(&mut self, frame: &[u8]) -> bool {
        let Some(endpoint) = self.endpoint_state.as_mut() else {
            return false;
        };
        let Some(publisher) = endpoint.publisher.as_mut() else {
            return false;
        };
        let params = endpoint.consomme.params_mut();
        let Some(payload) = net_backend_resources::ingress::listener_announcement_payload(
            frame,
            frame.len(),
            params.client_mac.0,
            params.client_ip,
            params.gateway_ip,
        ) else {
            return false;
        };
        publisher.apply(
            &frame[payload],
            &mut endpoint.consomme.access(&mut ClientNoPool {
                driver: &self.driver,
            }),
        );
        true
    }

    /// Sends the queued guest frames that the egress policy allows to the
    /// Consomme stack and completes all of them.
    pub(crate) fn process_tx(&mut self, pool: &mut dyn BufferAccess) {
        while let Some(head) = self.state.tx_avail.front() {
            let TxSegmentType::Head(metadata) = &head.ty else {
                unreachable!()
            };
            let tx_id = metadata.id;
            let checksum = ChecksumState {
                ipv4: metadata.flags.offload_ip_header_checksum(),
                tcp: metadata.flags.offload_tcp_checksum(),
                udp: metadata.flags.offload_udp_checksum(),
                tso: metadata
                    .flags
                    .offload_tcp_segmentation()
                    .then_some(metadata.max_segment_size),
                gso: metadata
                    .flags
                    .offload_udp_segmentation()
                    .then_some(metadata.max_segment_size),
            };
            let segment_count = metadata.segment_count as usize;
            let packet_len = metadata.len as usize;

            let mut buffer = std::mem::take(&mut self.state.tx_scratch);
            buffer.clear();
            buffer.resize(packet_len, 0);
            let guest_memory = pool.guest_memory();
            let mut offset = 0usize;
            for segment in self.state.tx_avail.drain(..segment_count) {
                let Some(end) = offset.checked_add(segment.len as usize) else {
                    tracing::error!("network TX segment length overflow");
                    break;
                };
                let Some(destination) = buffer.get_mut(offset..end) else {
                    tracing::error!(packet_len, end, "network TX segments exceed packet length");
                    break;
                };
                if let Err(error) = guest_memory.read_at(segment.gpa, destination) {
                    tracing::error!(
                        error = &error as &dyn std::error::Error,
                        "network TX guest-memory read failure"
                    );
                }
                offset = end;
            }

            let complete = offset == packet_len;
            let announced = complete && self.publish_announcement(&buffer);
            let policy_result =
                (complete && !announced)
                    .then(|| {
                        self.endpoint_state.as_ref().and_then(|state| {
                            let consomme = &state.consomme;
                            state.egress_policy.as_ref().map(|policy| {
                                policy.authorize_frame_with_flows(
                                    &buffer,
                                    buffer.len(),
                                    &mut |candidate| match candidate.transport {
                                        IngressTransport::Tcp => consomme
                                            .carries_tcp_flow(candidate.guest, candidate.peer),
                                        IngressTransport::Udp => consomme
                                            .carries_udp_flow(candidate.guest, candidate.peer),
                                    },
                                    &|address| consomme.virtual_address_destination(address),
                                )
                            })
                        })
                    })
                    .flatten();
            if announced {
                // The endpoint consumed the announcement.
            } else if policy_result.is_some_and(|result| result.is_err()) {
                self.stats.tx_dropped.increment();
            } else if complete
                && let Err(error) =
                    self.with_consomme(pool, |consomme| consomme.send(&buffer, &checksum))
            {
                tracing::debug!(
                    error = &error as &dyn std::error::Error,
                    "tx packet ignored"
                );
                match error {
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
            self.state.tx_scratch = buffer;
            self.state.tx_ready.push_back(tx_id);
        }
    }
}
