// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The echo requests that the relay has sent, and the replies that answer them.
//!
//! The host's ICMP sockets differ. A raw socket sends the message as it is,
//! delivers whole IPv4 packets, and delivers every ICMP message that reaches
//! the host, whichever process it is for. An unprivileged datagram socket
//! delivers only the echo replies to its own requests, but it takes the echo
//! identifier from its own binding, which replaces the guest's, and on Linux
//! it delivers the ICMP message alone.
//!
//! So that the guest's identifier survives the datagram socket, and only the
//! replies to the guest's requests leave the raw one, each request takes a
//! sequence number of its own on the host, which its reply returns, and a
//! raw socket's requests also take an identifier of the flow's own. The reply
//! then names the request, whose identifier and sequence number the relay
//! restores for the guest. The flow's identifier and first sequence number are
//! random, so that the requests of other flows and processes that share the raw
//! socket's view of the host are unlikely to match a request of this flow.

use smoltcp::wire::Icmpv4Message;
use smoltcp::wire::Icmpv4Packet;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::Ipv4Packet;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::Duration;
use std::time::Instant;

/// How long the relay waits for the reply to an echo request before it forgets
/// the request.
pub(super) const ECHO_TIMEOUT: Duration = Duration::from_secs(30);

/// The most echo requests that a flow awaits replies to at once. A request
/// beyond them makes the relay forget the oldest.
pub(super) const MAX_PENDING_ECHOES: usize = 64;

/// The hop limit of a reply that its socket does not report.
const DEFAULT_HOP_LIMIT: u8 = 64;

/// What a host ICMP socket is, and delivers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Delivery {
    /// A raw socket, which delivers the whole IPv4 packet, with the identifier
    /// that the request carried, and every ICMP message that reaches the host.
    Raw,
    /// An unprivileged datagram socket that delivers the whole IPv4 packet, as
    /// the BSD sockets do, whose binding may replace the identifier.
    Packet,
    /// An unprivileged datagram socket that delivers the ICMP message alone, as
    /// Linux's does, whose binding replaces the identifier.
    Message,
}

impl Delivery {
    /// What the sockets that the relay opens on this host are. Only Linux's
    /// datagram sockets deliver the message alone.
    pub(super) const HOST: Self = if cfg!(windows) {
        Self::Raw
    } else if cfg!(any(target_os = "linux", target_os = "android")) {
        Self::Message
    } else {
        Self::Packet
    };
}

/// An echo request that the relay has sent and has no reply to yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PendingEcho {
    /// The identifier that the guest gave the request.
    pub(super) guest_ident: u16,
    /// The sequence number that the guest gave the request.
    pub(super) guest_seq: u16,
    /// The host that the request went to, which the reply must come from.
    pub(super) destination: Ipv4Addr,
    /// When the relay sent the request.
    sent: Instant,
}

/// The echo requests of one flow that have no reply yet, by the sequence
/// number that they carry on the host.
#[derive(Debug)]
pub(super) struct PendingEchoes {
    /// The identifier that the flow's requests carry through a raw socket.
    ident: u16,
    echoes: HashMap<u16, PendingEcho>,
    next_seq: u16,
}

impl PendingEchoes {
    /// Returns an empty table whose requests carry `ident` through a raw
    /// socket, and whose first sequence number is `first_seq`.
    pub(super) fn new(ident: u16, first_seq: u16) -> Self {
        Self {
            ident,
            echoes: HashMap::new(),
            next_seq: first_seq,
        }
    }

    /// Returns an empty table with a random identifier and first sequence
    /// number, as a new flow takes.
    pub(super) fn random() -> Self {
        let mut bytes = [0; 4];
        getrandom::fill(&mut bytes).expect("prng failure");
        Self::new(
            u16::from_ne_bytes([bytes[0], bytes[1]]),
            u16::from_ne_bytes([bytes[2], bytes[3]]),
        )
    }

    /// Returns the number of requests that await a reply.
    pub(super) fn len(&self) -> usize {
        self.echoes.len()
    }

    /// Records the echo request in `message`, which goes to `destination`, and
    /// rewrites the message to carry the numbers that its reply returns: a
    /// sequence number of its own, and for a raw socket the flow's identifier.
    /// Returns the sequence number. `message` must be a whole ICMP echo
    /// request.
    pub(super) fn relay_request(
        &mut self,
        delivery: Delivery,
        message: &mut [u8],
        destination: Ipv4Addr,
        now: Instant,
    ) -> u16 {
        let mut request = Icmpv4Packet::new_unchecked(message);
        let host_seq = self.insert(
            request.echo_ident(),
            request.echo_seq_no(),
            destination,
            now,
        );
        request.set_echo_seq_no(host_seq);
        if delivery == Delivery::Raw {
            request.set_echo_ident(self.ident);
        }
        request.fill_checksum();
        host_seq
    }

    /// Records an echo request to `destination` that the guest numbered
    /// `guest_ident` and `guest_seq`, and returns the sequence number that the
    /// request carries on the host.
    pub(super) fn insert(
        &mut self,
        guest_ident: u16,
        guest_seq: u16,
        destination: Ipv4Addr,
        now: Instant,
    ) -> u16 {
        self.echoes
            .retain(|_, echo| now.saturating_duration_since(echo.sent) <= ECHO_TIMEOUT);
        if self.echoes.len() >= MAX_PENDING_ECHOES
            && let Some(oldest) = self
                .echoes
                .iter()
                .min_by_key(|(_, echo)| echo.sent)
                .map(|(&seq, _)| seq)
        {
            self.echoes.remove(&oldest);
        }
        let seq = loop {
            let seq = self.next_seq;
            self.next_seq = seq.wrapping_add(1);
            if !self.echoes.contains_key(&seq) {
                break seq;
            }
        };
        self.echoes.insert(
            seq,
            PendingEcho {
                guest_ident,
                guest_seq,
                destination,
                sent: now,
            },
        );
        seq
    }

    /// Forgets the request that carries `host_seq`, which never left.
    pub(super) fn forget(&mut self, host_seq: u16) {
        self.echoes.remove(&host_seq);
    }

    /// Takes the request that a reply answers, if it answers one: the reply
    /// carries the sequence number `host_seq`, comes from the request's
    /// destination `source`, and arrives in time. A raw socket also delivers
    /// the identifier that it sent, which must be the flow's.
    fn take(
        &mut self,
        delivery: Delivery,
        host_seq: u16,
        host_ident: u16,
        source: Ipv4Addr,
        now: Instant,
    ) -> Option<PendingEcho> {
        let echo = self.echoes.get(&host_seq)?;
        if echo.destination != source
            || now.saturating_duration_since(echo.sent) > ECHO_TIMEOUT
            || (delivery == Delivery::Raw && host_ident != self.ident)
        {
            return None;
        }
        self.echoes.remove(&host_seq)
    }

    /// Turns what a host socket delivered in `buffer[..received]` into the ICMP
    /// message that the guest receives, if it is the echo reply to a request of
    /// this flow, and leaves the message at the start of `buffer`. `peer` is
    /// the address that the socket reports for the datagram.
    pub(super) fn relay_reply(
        &mut self,
        delivery: Delivery,
        buffer: &mut [u8],
        received: usize,
        peer: Option<Ipv4Addr>,
        now: Instant,
    ) -> Option<Reply> {
        let (source, hop_limit, len) = split_delivery(delivery, buffer, received, peer)?;
        let mut message = Icmpv4Packet::new_checked(buffer.get_mut(..len)?).ok()?;
        if message.msg_type() != Icmpv4Message::EchoReply || message.msg_code() != 0 {
            return None;
        }
        let echo = self.take(
            delivery,
            message.echo_seq_no(),
            message.echo_ident(),
            source,
            now,
        )?;
        message.set_echo_ident(echo.guest_ident);
        message.set_echo_seq_no(echo.guest_seq);
        message.fill_checksum();
        Some(Reply {
            source,
            hop_limit,
            len,
        })
    }
}

/// An echo reply for the guest, whose ICMP message is at the start of the
/// buffer that [`PendingEchoes::relay_reply`] took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Reply {
    /// The host that answered.
    pub(super) source: Ipv4Addr,
    /// The hop limit of the reply.
    pub(super) hop_limit: u8,
    /// The length of the ICMP message.
    pub(super) len: usize,
}

/// Moves the ICMP message that a socket delivered in `buffer[..received]` to
/// the start of `buffer`, and returns the address that sent it, its hop limit,
/// and its length. A socket that delivers the message alone delivers it from
/// `peer`; the others deliver the IPv4 packet, whose header gives the sender and
/// hop limit and which must carry an ICMP message.
pub(super) fn split_delivery(
    delivery: Delivery,
    buffer: &mut [u8],
    received: usize,
    peer: Option<Ipv4Addr>,
) -> Option<(Ipv4Addr, u8, usize)> {
    match delivery {
        Delivery::Message => Some((peer?, DEFAULT_HOP_LIMIT, received)),
        Delivery::Raw | Delivery::Packet => {
            let packet = Ipv4Packet::new_checked(buffer.get(..received)?).ok()?;
            let header_len = usize::from(packet.header_len());
            let total_len = usize::from(packet.total_len());
            if packet.version() != 4
                || packet.next_header() != IpProtocol::Icmp
                || total_len < header_len
            {
                return None;
            }
            let (source, hop_limit) = (packet.src_addr(), packet.hop_limit());
            buffer.copy_within(header_len..total_len, 0);
            Some((source, hop_limit, total_len - header_len))
        }
    }
}
