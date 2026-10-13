// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Private-network and host-loopback ingress of a static microVM link.
//!
//! The portable profile's NAT owns no listener for new inbound connections,
//! and the caller's ingress policy names no ports. With ingress allowed, from
//! the host's private networks or from host loopback alone, a guest agent
//! therefore announces the TCP and UDP ports on which the workload
//! listens. It sends each announcement as one UDP datagram from a privileged
//! source port to [`LISTENER_ANNOUNCEMENT_PORT`] on the IPv4 gateway, so a
//! workload without the privilege to bind such a port cannot forge one. The
//! endpoint consumes announcements instead of forwarding them, and publishes
//! the announced ports on the host: on every host address for private-network
//! ingress, and on host loopback alone otherwise.
//!
//! An announcement's payload is the magic `NVXL`, a version byte, a flags
//! byte, a big-endian entry count, and that many four-byte entries in strictly
//! increasing order of transport and port: the IP protocol number (6 or 17), a
//! family mask (1 for IPv4, 2 for IPv6, or 3 for both), and the big-endian
//! port. Flag bit 0 marks an announcement that omits ports beyond
//! [`MAX_ANNOUNCED_LISTENERS`].

use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::ops::Range;
use thiserror::Error;

/// Gateway UDP port that receives listener announcements: the first of the
/// experimental ports that RFC 4727 reserves.
pub const LISTENER_ANNOUNCEMENT_PORT: u16 = 1021;

/// First port that an unprivileged guest process can bind. Announcements come
/// only from lower ports.
pub const UNPRIVILEGED_PORT_START: u16 = 1024;

/// Largest number of listeners that one announcement carries.
pub const MAX_ANNOUNCED_LISTENERS: usize = 64;

const MAGIC: [u8; 4] = *b"NVXL";
const VERSION: u8 = 1;
const HEADER_LENGTH: usize = 8;
const ENTRY_LENGTH: usize = 4;
const FLAG_TRUNCATED: u8 = 1;
const FAMILY_IPV4: u8 = 1;
const FAMILY_IPV6: u8 = 2;
const IP_PROTOCOL_TCP: u8 = 6;
const IP_PROTOCOL_UDP: u8 = 17;

/// A transport whose listeners the guest announces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IngressTransport {
    /// TCP.
    Tcp,
    /// UDP.
    Udp,
}

impl IngressTransport {
    fn protocol_number(self) -> u8 {
        match self {
            Self::Tcp => IP_PROTOCOL_TCP,
            Self::Udp => IP_PROTOCOL_UDP,
        }
    }
}

/// One port on which the guest listens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GuestListener {
    /// The listener's transport.
    pub transport: IngressTransport,
    /// The guest port.
    pub port: u16,
    /// Whether the listener accepts IPv4.
    pub ipv4: bool,
    /// Whether the listener accepts IPv6.
    pub ipv6: bool,
}

/// The ports on which the guest listens, as one announcement reports them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListenerAnnouncement {
    /// The announced listeners, in strictly increasing order of transport and
    /// port.
    pub listeners: Vec<GuestListener>,
    /// Whether the guest listens on more ports than the announcement carries.
    pub truncated: bool,
}

/// Reason a listener announcement is invalid.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum InvalidListenerAnnouncement {
    /// The payload does not begin with the announcement header.
    #[error("the payload is not a listener announcement")]
    NotAnAnnouncement,
    /// The announcement uses an unsupported version.
    #[error("listener announcement version {0} is unsupported")]
    UnsupportedVersion(u8),
    /// The announcement sets unknown flags.
    #[error("listener announcement flags {0:#04x} are unsupported")]
    UnsupportedFlags(u8),
    /// The announcement carries more listeners than permitted.
    #[error("listener announcement carries {0} listeners, more than the limit")]
    TooManyListeners(usize),
    /// The payload length does not match the entry count.
    #[error("listener announcement length does not match its entry count")]
    LengthMismatch,
    /// An entry names an unsupported transport.
    #[error("listener announcement names IP protocol {0}")]
    UnsupportedTransport(u8),
    /// An entry names no address family or an unknown one.
    #[error("listener announcement names family mask {0:#04x}")]
    InvalidFamilies(u8),
    /// An entry names port zero.
    #[error("listener announcement names port 0")]
    ZeroPort,
    /// The entries are not in strictly increasing order.
    #[error("listener announcement entries are not in strictly increasing order")]
    NotCanonical,
}

impl ListenerAnnouncement {
    /// Parses and validates an announcement's UDP payload.
    pub fn parse(payload: &[u8]) -> Result<Self, InvalidListenerAnnouncement> {
        let header = payload
            .get(..HEADER_LENGTH)
            .ok_or(InvalidListenerAnnouncement::NotAnAnnouncement)?;
        if header[..4] != MAGIC {
            return Err(InvalidListenerAnnouncement::NotAnAnnouncement);
        }
        if header[4] != VERSION {
            return Err(InvalidListenerAnnouncement::UnsupportedVersion(header[4]));
        }
        let flags = header[5];
        if flags & !FLAG_TRUNCATED != 0 {
            return Err(InvalidListenerAnnouncement::UnsupportedFlags(flags));
        }
        let count = usize::from(u16::from_be_bytes([header[6], header[7]]));
        if count > MAX_ANNOUNCED_LISTENERS {
            return Err(InvalidListenerAnnouncement::TooManyListeners(count));
        }
        let entries = &payload[HEADER_LENGTH..];
        if entries.len() != count * ENTRY_LENGTH {
            return Err(InvalidListenerAnnouncement::LengthMismatch);
        }
        let mut listeners = Vec::with_capacity(count);
        for entry in entries.chunks_exact(ENTRY_LENGTH) {
            let transport = match entry[0] {
                IP_PROTOCOL_TCP => IngressTransport::Tcp,
                IP_PROTOCOL_UDP => IngressTransport::Udp,
                other => return Err(InvalidListenerAnnouncement::UnsupportedTransport(other)),
            };
            let families = entry[1];
            if families == 0 || families & !(FAMILY_IPV4 | FAMILY_IPV6) != 0 {
                return Err(InvalidListenerAnnouncement::InvalidFamilies(families));
            }
            let port = u16::from_be_bytes([entry[2], entry[3]]);
            if port == 0 {
                return Err(InvalidListenerAnnouncement::ZeroPort);
            }
            let listener = GuestListener {
                transport,
                port,
                ipv4: families & FAMILY_IPV4 != 0,
                ipv6: families & FAMILY_IPV6 != 0,
            };
            if listeners.last().is_some_and(|previous: &GuestListener| {
                (previous.transport, previous.port) >= (transport, port)
            }) {
                return Err(InvalidListenerAnnouncement::NotCanonical);
            }
            listeners.push(listener);
        }
        Ok(Self {
            listeners,
            truncated: flags & FLAG_TRUNCATED != 0,
        })
    }

    /// Encodes the announcement's UDP payload, which [`Self::parse`] accepts
    /// when the listeners are canonical.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER_LENGTH + self.listeners.len() * ENTRY_LENGTH);
        bytes.extend_from_slice(&MAGIC);
        bytes.push(VERSION);
        bytes.push(if self.truncated { FLAG_TRUNCATED } else { 0 });
        bytes.extend_from_slice(&(self.listeners.len() as u16).to_be_bytes());
        for listener in &self.listeners {
            bytes.push(listener.transport.protocol_number());
            bytes.push(
                if listener.ipv4 { FAMILY_IPV4 } else { 0 }
                    | if listener.ipv6 { FAMILY_IPV6 } else { 0 },
            );
            bytes.extend_from_slice(&listener.port.to_be_bytes());
        }
        bytes
    }
}

/// Returns the range of a listener announcement's UDP payload within a guest
/// Ethernet frame, or `None` when the frame is not an announcement.
///
/// An announcement is an untagged, unfragmented IPv4 UDP datagram without IP
/// options from `guest_ipv4` and `guest_mac`, with a valid IPv4 header
/// checksum, from a privileged port to [`LISTENER_ANNOUNCEMENT_PORT`] on
/// `gateway_ipv4`. Only the headers must be within `frame_prefix`, so a
/// prefix of the frame suffices to recognize it.
pub fn listener_announcement_payload(
    frame_prefix: &[u8],
    frame_length: usize,
    guest_mac: [u8; 6],
    guest_ipv4: Ipv4Addr,
    gateway_ipv4: Ipv4Addr,
) -> Option<Range<usize>> {
    const ETHERNET_LENGTH: usize = 14;
    const IPV4_LENGTH: usize = 20;
    const UDP_LENGTH: usize = 8;
    let headers = frame_prefix.get(..ETHERNET_LENGTH + IPV4_LENGTH + UDP_LENGTH)?;
    if frame_length < headers.len() || headers[6..12] != guest_mac || headers[12..14] != [8, 0] {
        return None;
    }
    let ip = &headers[ETHERNET_LENGTH..ETHERNET_LENGTH + IPV4_LENGTH];
    let total_length = usize::from(u16::from_be_bytes([ip[2], ip[3]]));
    let fragments = u16::from_be_bytes([ip[6], ip[7]]) & 0x3fff;
    if ip[0] != 0x45
        || fragments != 0
        || ip[9] != IP_PROTOCOL_UDP
        || ip[12..16] != guest_ipv4.octets()
        || ip[16..20] != gateway_ipv4.octets()
        || total_length < IPV4_LENGTH + UDP_LENGTH
        || ETHERNET_LENGTH + total_length > frame_length
        || !ipv4_header_checksum_is_valid(ip)
    {
        return None;
    }
    let udp = &headers[ETHERNET_LENGTH + IPV4_LENGTH..];
    let source_port = u16::from_be_bytes([udp[0], udp[1]]);
    let destination_port = u16::from_be_bytes([udp[2], udp[3]]);
    let udp_length = usize::from(u16::from_be_bytes([udp[4], udp[5]]));
    if source_port == 0
        || source_port >= UNPRIVILEGED_PORT_START
        || destination_port != LISTENER_ANNOUNCEMENT_PORT
        || udp_length < UDP_LENGTH
        || udp_length > total_length - IPV4_LENGTH
    {
        return None;
    }
    let payload = ETHERNET_LENGTH + IPV4_LENGTH + UDP_LENGTH;
    Some(payload..ETHERNET_LENGTH + IPV4_LENGTH + udp_length)
}

fn ipv4_header_checksum_is_valid(header: &[u8]) -> bool {
    let mut sum = header.chunks_exact(2).fold(0u32, |sum, word| {
        sum + u32::from(u16::from_be_bytes([word[0], word[1]]))
    });
    while sum > u32::from(u16::MAX) {
        sum = (sum & u32::from(u16::MAX)) + (sum >> 16);
    }
    sum == u32::from(u16::MAX)
}

/// A guest transmission that an ingress-allowing policy admits only as part of
/// a flow that the endpoint already carries, such as a reply to a connection
/// that a private-network client opened.
///
/// A TCP segment is a candidate unless it opens a connection: a SYN without
/// ACK is always subject to the egress rules. Every UDP datagram is a
/// candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplyCandidate {
    /// The transport of the flow.
    pub transport: IngressTransport,
    /// The guest's address and port, as the frame's source.
    pub guest: SocketAddr,
    /// The peer's address and port, as the frame's destination.
    pub peer: SocketAddr,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0, 0, 0, 2];
    const GUEST: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);
    const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);

    fn listener(transport: IngressTransport, port: u16, ipv4: bool, ipv6: bool) -> GuestListener {
        GuestListener {
            transport,
            port,
            ipv4,
            ipv6,
        }
    }

    pub(crate) fn announcement_frame(
        source_port: u16,
        destination: Ipv4Addr,
        destination_port: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut frame = vec![0u8; 42 + payload.len()];
        frame[..6].copy_from_slice(&[0xff; 6]);
        frame[6..12].copy_from_slice(&GUEST_MAC);
        frame[12..14].copy_from_slice(&[8, 0]);
        let total_length = (28 + payload.len()) as u16;
        let ip = &mut frame[14..34];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&total_length.to_be_bytes());
        ip[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
        ip[8] = 64;
        ip[9] = 17;
        ip[12..16].copy_from_slice(&GUEST.octets());
        ip[16..20].copy_from_slice(&destination.octets());
        let mut sum = ip.chunks_exact(2).fold(0u32, |sum, word| {
            sum + u32::from(u16::from_be_bytes([word[0], word[1]]))
        });
        while sum > 0xffff {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        ip[10..12].copy_from_slice(&(!(sum as u16)).to_be_bytes());
        let udp = &mut frame[34..42];
        udp[..2].copy_from_slice(&source_port.to_be_bytes());
        udp[2..4].copy_from_slice(&destination_port.to_be_bytes());
        udp[4..6].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        frame[42..].copy_from_slice(payload);
        frame
    }

    #[test]
    fn announcements_round_trip() {
        let announcement = ListenerAnnouncement {
            listeners: vec![
                listener(IngressTransport::Tcp, 22, true, false),
                listener(IngressTransport::Tcp, 8080, true, true),
                listener(IngressTransport::Udp, 53, false, true),
            ],
            truncated: true,
        };
        let bytes = announcement.to_bytes();
        assert_eq!(
            bytes,
            [
                b'N', b'V', b'X', b'L', 1, 1, 0, 3, 6, 1, 0, 22, 6, 3, 0x1f, 0x90, 17, 2, 0, 53
            ]
        );
        assert_eq!(ListenerAnnouncement::parse(&bytes).unwrap(), announcement);
        assert_eq!(
            ListenerAnnouncement::parse(&ListenerAnnouncement::default().to_bytes()).unwrap(),
            ListenerAnnouncement::default()
        );
    }

    #[test]
    fn malformed_announcements_are_rejected() {
        let valid = ListenerAnnouncement {
            listeners: vec![
                listener(IngressTransport::Tcp, 80, true, true),
                listener(IngressTransport::Udp, 80, true, true),
            ],
            truncated: false,
        }
        .to_bytes();
        let with = |offset: usize, value: u8| {
            let mut bytes = valid.clone();
            bytes[offset] = value;
            bytes
        };
        let too_many = {
            let mut bytes = valid[..8].to_vec();
            bytes[6..8].copy_from_slice(&65u16.to_be_bytes());
            bytes.extend(std::iter::repeat_n(0, 65 * 4));
            bytes
        };
        let unordered = {
            let mut bytes = valid.clone();
            bytes[8] = 17;
            bytes[12] = 6;
            bytes
        };
        for (bytes, expected) in [
            (&valid[..7], InvalidListenerAnnouncement::NotAnAnnouncement),
            (
                &with(0, b'X')[..],
                InvalidListenerAnnouncement::NotAnAnnouncement,
            ),
            (
                &with(4, 2)[..],
                InvalidListenerAnnouncement::UnsupportedVersion(2),
            ),
            (
                &with(5, 2)[..],
                InvalidListenerAnnouncement::UnsupportedFlags(2),
            ),
            (
                &too_many[..],
                InvalidListenerAnnouncement::TooManyListeners(65),
            ),
            (
                &valid[..valid.len() - 1],
                InvalidListenerAnnouncement::LengthMismatch,
            ),
            (
                &with(8, 1)[..],
                InvalidListenerAnnouncement::UnsupportedTransport(1),
            ),
            (
                &with(9, 0)[..],
                InvalidListenerAnnouncement::InvalidFamilies(0),
            ),
            (
                &with(9, 4)[..],
                InvalidListenerAnnouncement::InvalidFamilies(4),
            ),
            (&with(11, 0)[..], InvalidListenerAnnouncement::ZeroPort),
            // A duplicate entry.
            (&with(12, 6)[..], InvalidListenerAnnouncement::NotCanonical),
            (&unordered[..], InvalidListenerAnnouncement::NotCanonical),
        ] {
            assert_eq!(
                ListenerAnnouncement::parse(bytes),
                Err(expected),
                "{bytes:x?}"
            );
        }
    }

    #[test]
    fn announcement_frames_are_recognized_from_their_headers() {
        let payload = ListenerAnnouncement::default().to_bytes();
        let frame = announcement_frame(1023, GATEWAY, LISTENER_ANNOUNCEMENT_PORT, &payload);
        let recognize = |frame: &[u8], length: usize| {
            listener_announcement_payload(frame, length, GUEST_MAC, GUEST, GATEWAY)
        };
        assert_eq!(recognize(&frame, frame.len()), Some(42..42 + payload.len()));
        // The headers suffice.
        assert_eq!(
            recognize(&frame[..42], frame.len()),
            Some(42..42 + payload.len())
        );
        assert_eq!(recognize(&frame[..41], frame.len()), None);

        for frame in [
            announcement_frame(1024, GATEWAY, LISTENER_ANNOUNCEMENT_PORT, &payload),
            announcement_frame(0, GATEWAY, LISTENER_ANNOUNCEMENT_PORT, &payload),
            announcement_frame(1023, GATEWAY, LISTENER_ANNOUNCEMENT_PORT + 1, &payload),
            announcement_frame(
                1023,
                Ipv4Addr::new(10, 0, 0, 3),
                LISTENER_ANNOUNCEMENT_PORT,
                &payload,
            ),
        ] {
            assert_eq!(recognize(&frame, frame.len()), None);
        }

        let mut corrupt = frame.clone();
        corrupt[24] ^= 1;
        assert_eq!(recognize(&corrupt, corrupt.len()), None);
        let mut other_mac = frame.clone();
        other_mac[11] ^= 1;
        assert_eq!(recognize(&other_mac, other_mac.len()), None);
        // The IPv4 length must fit in the frame.
        assert_eq!(recognize(&frame, frame.len() - 1), None);
    }
}
