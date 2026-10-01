// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A TAP interface based endpoint.

#![cfg(target_os = "linux")]
#![expect(missing_docs)]

pub mod resolver;
pub mod tap;

use async_trait::async_trait;
use futures::io::AsyncRead;
use inspect::InspectMut;
use linux_net_bindings::gen_if_tun;
use net_backend::BufferAccess;
use net_backend::ETHERNET_HEADER_LEN;
use net_backend::ETHERNET_VLAN_HEADER_LEN;
use net_backend::Endpoint;
use net_backend::L4Protocol;
use net_backend::Queue;
use net_backend::QueueConfig;
use net_backend::RssConfig;
use net_backend::RxChecksumOffload;
use net_backend::RxChecksumState;
use net_backend::RxGso;
use net_backend::RxGsoProtocol;
use net_backend::RxId;
use net_backend::RxMetadata;
use net_backend::TxError;
use net_backend::TxId;
use net_backend::TxMetadata;
use net_backend::TxOffloadSupport;
use net_backend::TxSegment;
use net_backend::linearize;
use net_backend::next_packet;
use pal_async::driver::Driver;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::io::ErrorKind;
use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

const MAX_TAP_FRAME_LEN: usize =
    gen_if_tun::ETH_MAX_MTU as usize + ETHERNET_VLAN_HEADER_LEN as usize;

// TODO: These virtio net header types duplicate definitions in virtio_net.
// Consider extracting a shared `virtio_net_header` crate if more consumers
// appear (e.g., vhost-user).
mod vnet_hdr {
    use bitfield_struct::bitfield;
    use zerocopy::FromBytes;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    /// Flags in the virtio network header.
    #[bitfield(u8)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct VirtioNetHdrFlags {
        pub needs_csum: bool,
        pub data_valid: bool,
        #[bits(6)]
        _reserved: u8,
    }

    /// GSO type bitfield in the virtio network header.
    #[bitfield(u8)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct VirtioNetHdrGso {
        #[bits(3)]
        pub protocol: VirtioNetHdrGsoProtocol,
        #[bits(4)]
        _reserved: u8,
        pub ecn: bool,
    }

    open_enum::open_enum! {
        /// GSO protocol in the virtio network header.
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
        pub enum VirtioNetHdrGsoProtocol: u8 {
            NONE = 0,
            TCPV4 = 1,
            UDP = 3,
            TCPV6 = 4,
            UDP_L4 = 5,
        }
    }

    impl VirtioNetHdrGsoProtocol {
        const fn from_bits(bits: u8) -> Self {
            Self(bits)
        }

        const fn into_bits(self) -> u8 {
            self.0
        }
    }

    /// The virtio network header prepended to packets when `IFF_VNET_HDR` is set.
    /// This is the 12-byte v1 format (without hash fields).
    #[repr(C)]
    #[derive(Debug, Default, Clone, Copy, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct VirtioNetHdr {
        pub flags: VirtioNetHdrFlags,
        pub gso_type: VirtioNetHdrGso,
        pub hdr_len: u16,
        pub gso_size: u16,
        pub csum_start: u16,
        pub csum_offset: u16,
        pub num_buffers: u16,
    }
}
pub use vnet_hdr::*;

/// An endpoint based on a TAP interface.
pub struct TapEndpoint {
    tap: Arc<Mutex<Option<tap::Tap>>>,
}

impl TapEndpoint {
    pub fn new(tap: tap::Tap) -> Result<Self, tap::Error> {
        Ok(Self {
            tap: Arc::new(Mutex::new(Some(tap))),
        })
    }
}

impl InspectMut for TapEndpoint {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond();
    }
}

#[async_trait]
impl Endpoint for TapEndpoint {
    fn endpoint_type(&self) -> &'static str {
        "tap"
    }

    async fn get_queues(
        &mut self,
        mut config: Vec<QueueConfig>,
        _rss: Option<&RssConfig<'_>>,
        queues: &mut Vec<Box<dyn Queue>>,
    ) -> anyhow::Result<()> {
        assert_eq!(config.len(), 1);
        let config = config.drain(..).next().unwrap();
        let mut offloads = 0;
        if config.rx_offload_support.checksum {
            offloads |= gen_if_tun::TUN_F_CSUM;
        }
        if config.rx_offload_support.tcpv4_gso {
            offloads |= gen_if_tun::TUN_F_TSO4;
        }
        if config.rx_offload_support.tcpv6_gso {
            offloads |= gen_if_tun::TUN_F_TSO6;
        }
        self.tap
            .lock()
            .as_ref()
            .expect("queue is already in use")
            .set_offloads(offloads)?;

        queues.push(Box::new(TapQueue::new(
            config.driver.as_ref(),
            self.tap.clone(),
            config.rx_offload_support,
        )?));
        Ok(())
    }

    async fn stop(&mut self) {
        assert!(self.tap.lock().is_some(), "queue has not been dropped");
    }

    fn is_ordered(&self) -> bool {
        true
    }

    fn tx_offload_support(&self) -> TxOffloadSupport {
        TxOffloadSupport {
            // TAP does not support IPv4 header checksum offload, but netvsp
            // (NDIS/TAP) guests require it for LSOv4. It's relatively cheap for
            // us to compute in software, so report it. Virtio-net won't use it.
            ipv4_header: true,
            tcp: true,
            udp: true,
            tso: true,
            uso: true,
        }
    }
}

struct TapQueue {
    slot: Arc<Mutex<Option<tap::Tap>>>,
    tap: Option<tap::PolledTap>,
    inner: Inner,
    buffer: Box<[u8]>,
    rx_offload_support: net_backend::RxOffloadSupport,
    rx_packets: u64,
    rx_gso_packets: u64,
    rx_malformed_packets: u64,
    rx_oversized_packets: u64,
    rx_truncated_packets: u64,
    rx_max_packet_len: usize,
    rx_max_buffer_capacity: u32,
}

struct Inner {
    rx_free: VecDeque<RxId>,
    rx_ready: VecDeque<RxId>,
}

impl InspectMut for TapQueue {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond()
            .field("rx_packets", self.rx_packets)
            .field("rx_gso_packets", self.rx_gso_packets)
            .field("rx_malformed_packets", self.rx_malformed_packets)
            .field("rx_oversized_packets", self.rx_oversized_packets)
            .field("rx_truncated_packets", self.rx_truncated_packets)
            .field("rx_max_packet_len", self.rx_max_packet_len)
            .field("rx_max_buffer_capacity", self.rx_max_buffer_capacity);
    }
}

impl Drop for TapQueue {
    fn drop(&mut self) {
        if let Some(tap) = self.tap.take() {
            *self.slot.lock() = Some(tap.into_inner());
        }
    }
}

impl TapQueue {
    fn new(
        driver: &dyn Driver,
        slot: Arc<Mutex<Option<tap::Tap>>>,
        rx_offload_support: net_backend::RxOffloadSupport,
    ) -> anyhow::Result<Self> {
        let tap = slot.lock().take().expect("queue is already in use");
        let tap = tap.polled(driver)?;
        Ok(Self {
            slot,
            tap: Some(tap),
            inner: Inner {
                rx_free: VecDeque::new(),
                rx_ready: VecDeque::new(),
            },
            // One extra byte makes a full buffer an unambiguous indication that
            // the frame exceeded the largest legal Ethernet frame.
            buffer: vec![0; size_of::<VirtioNetHdr>() + MAX_TAP_FRAME_LEN + 1].into_boxed_slice(),
            rx_offload_support,
            rx_packets: 0,
            rx_gso_packets: 0,
            rx_malformed_packets: 0,
            rx_oversized_packets: 0,
            rx_truncated_packets: 0,
            rx_max_packet_len: 0,
            rx_max_buffer_capacity: 0,
        })
    }
}

impl Queue for TapQueue {
    fn poll_ready(&mut self, cx: &mut Context<'_>, pool: &mut dyn BufferAccess) -> Poll<()> {
        if !self.inner.rx_ready.is_empty() {
            return Poll::Ready(());
        }

        let tap = if let Some(tap) = self.tap.as_mut() {
            tap
        } else {
            return Poll::Pending;
        };

        while let Some(&rx) = self.inner.rx_free.front() {
            match Pin::new(&mut *tap).poll_read(cx, &mut self.buffer) {
                Poll::Ready(Ok(read_len)) => {
                    if read_len == self.buffer.len() {
                        self.rx_truncated_packets += 1;
                        tracelimit::warn_ratelimited!(
                            read_len,
                            max_frame_len = MAX_TAP_FRAME_LEN,
                            "dropping truncated TAP packet"
                        );
                        self.inner.rx_ready.push_back(rx);
                        self.inner.rx_free.pop_front();
                        continue;
                    }
                    if read_len < size_of::<VirtioNetHdr>() {
                        self.rx_malformed_packets += 1;
                        tracelimit::warn_ratelimited!(
                            read_len,
                            "dropping TAP packet shorter than vnet header"
                        );
                        self.inner.rx_ready.push_back(rx);
                        self.inner.rx_free.pop_front();
                        continue;
                    }
                    let Ok((hdr, _)) = VirtioNetHdr::read_from_prefix(&self.buffer[..read_len])
                    else {
                        self.rx_malformed_packets += 1;
                        tracelimit::warn_ratelimited!(
                            read_len,
                            "dropping TAP packet with unreadable vnet header"
                        );
                        self.inner.rx_ready.push_back(rx);
                        self.inner.rx_free.pop_front();
                        continue;
                    };
                    let frame_start = size_of::<VirtioNetHdr>();
                    let frame_len = read_len - size_of::<VirtioNetHdr>();
                    let rx_meta = match parse_vnet_hdr(&hdr, frame_len, self.rx_offload_support) {
                        Ok(metadata) => metadata,
                        Err(reason) => {
                            self.rx_malformed_packets += 1;
                            tracelimit::warn_ratelimited!(
                                ?reason,
                                frame_len,
                                "dropping TAP packet with invalid vnet metadata"
                            );
                            self.inner.rx_ready.push_back(rx);
                            self.inner.rx_free.pop_front();
                            continue;
                        }
                    };
                    let capacity = pool.capacity(rx);
                    self.rx_packets += 1;
                    self.rx_gso_packets += u64::from(rx_meta.gso.is_some());
                    if self.rx_gso_packets == 1 {
                        tracing::info!(
                            frame_len,
                            capacity,
                            ?rx_meta.gso,
                            ?rx_meta.checksum_offload,
                            "received first TAP GSO packet"
                        );
                    }
                    self.rx_max_packet_len = self.rx_max_packet_len.max(frame_len);
                    self.rx_max_buffer_capacity = self.rx_max_buffer_capacity.max(capacity);
                    if frame_len <= capacity as usize {
                        pool.write_packet(
                            rx,
                            &RxMetadata {
                                offset: 0,
                                len: frame_len,
                                ..rx_meta
                            },
                            &self.buffer[frame_start..read_len],
                        );
                    } else {
                        self.rx_oversized_packets += 1;
                        if self.rx_oversized_packets == 1 {
                            tracing::warn!(
                                frame_len,
                                capacity,
                                "dropping TAP packet larger than guest receive buffer"
                            );
                        }
                    }

                    self.inner.rx_ready.push_back(rx);
                    self.inner.rx_free.pop_front();
                }
                Poll::Ready(Err(err)) => {
                    tracing::warn!(error = &err as &dyn std::error::Error, "tap rx error");
                    break;
                }
                Poll::Pending => break,
            }
        }

        if !self.inner.rx_ready.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    fn rx_avail(&mut self, _pool: &mut dyn BufferAccess, done: &[RxId]) {
        self.inner.rx_free.extend(done);
    }

    fn rx_poll(
        &mut self,
        _pool: &mut dyn BufferAccess,
        packets: &mut [RxId],
    ) -> anyhow::Result<usize> {
        // Send to the guest any packets that might have been read during poll_ready().
        let n = std::cmp::min(self.inner.rx_ready.len(), packets.len());
        for (done, id) in packets[..n].iter_mut().zip(self.inner.rx_ready.drain(..n)) {
            *done = id;
        }
        Ok(n)
    }

    fn tx_avail(
        &mut self,
        pool: &mut dyn BufferAccess,
        mut segments: &[TxSegment],
    ) -> anyhow::Result<(bool, usize)> {
        let n = segments.len();
        // Synchronously send packets received from the guest to host's network.
        if let Some(tap) = self.tap.as_mut() {
            while !segments.is_empty() {
                let (meta, _segs, _rest) = next_packet(segments);
                let hdr = build_vnet_hdr(meta);
                let hdr_bytes = hdr.as_bytes();
                let mut packet = linearize(pool, &mut segments)?;

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

    fn tx_poll(
        &mut self,
        _pool: &mut dyn BufferAccess,
        _done: &mut [TxId],
    ) -> Result<usize, TxError> {
        // Packets are sent synchronously so there is no no need to check here if
        // sending has been completed.
        Ok(0)
    }
}

/// Compute and write the IPv4 header checksum in place.
///
/// The IPv4 header length is derived from the IHL field in the packet itself
/// rather than trusting guest-provided metadata (`l3_len`), since that value
/// crosses a trust boundary. The IHL value is clamped to 20..60 bytes (the
/// valid range per RFC 791) and bounded by the packet length.
///
/// The virtio net header has no way to request IPv4 header checksum offload,
/// and in bridged configurations the kernel does not recompute it. When
/// netvsp (Windows/NDIS guests) sets `offload_ip_header_checksum`, we must
/// compute it in software before handing the frame to TAP.
fn fixup_ipv4_header_checksum(packet: &mut [u8], l2_len: usize) {
    // Need at least the minimum IPv4 header to read IHL.
    if packet.len() < l2_len + 20 {
        return;
    }
    // Derive header length from the IHL field in the packet, not from
    // guest-provided metadata.
    let ihl_bytes = ((packet[l2_len] & 0x0f) as usize) * 4;
    if !(20..=60).contains(&ihl_bytes) {
        return;
    }
    if packet.len() < l2_len + ihl_bytes {
        return;
    }
    // fix IP bad-len 0
    let ip_total_len = u16::try_from(packet.len() - l2_len).unwrap_or(0);
    packet[l2_len + 2..l2_len + 4].copy_from_slice(&ip_total_len.to_be_bytes());
    let ip_hdr = &mut packet[l2_len..l2_len + ihl_bytes];
    // Zero the checksum field (bytes 10-11) before computing.
    ip_hdr[10] = 0;
    ip_hdr[11] = 0;
    // RFC 1071 ones-complement sum over the header.
    let mut sum: u32 = 0;
    for chunk in ip_hdr.chunks(2) {
        let word = if chunk.len() == 2 {
            u16::from_be_bytes([chunk[0], chunk[1]])
        } else {
            u16::from_be_bytes([chunk[0], 0])
        };
        sum += word as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    let checksum = !(sum as u16);
    let [hi, lo] = checksum.to_be_bytes();
    packet[l2_len + 10] = hi;
    packet[l2_len + 11] = lo;
}

/// Set the IPv6 payload-length field for segmentation-offload frames.
///
/// NDIS/netvsp LSO guests zero the IPv6 payload-length field, expecting the
/// offload engine to fill it (the same convention under which IPv4 guests zero
/// the total-length and header checksum -- see [`fixup_ipv4_header_checksum`]).
/// IPv6 has no header checksum, so there is nothing to piggyback on; set the
/// field directly. Without it the kernel TAP GSO engine sees a zero-length IPv6
/// datagram and drops the super-frame instead of segmenting it, collapsing TX.
fn fixup_ipv6_payload_length(packet: &mut [u8], l2_len: usize) {
    // IPv6 fixed header is 40 bytes; the payload-length field (bytes 4-5)
    // covers everything after it.
    const IPV6_HEADER_LEN: usize = 40;
    if packet.len() < l2_len + IPV6_HEADER_LEN {
        return;
    }
    if packet[l2_len] >> 4 != 6 {
        return;
    }
    let payload_len = u16::try_from(packet.len() - l2_len - IPV6_HEADER_LEN).unwrap_or(0);
    packet[l2_len + 4..l2_len + 6].copy_from_slice(&payload_len.to_be_bytes());
}

/// Build a `VirtioNetHdr` from transmit metadata for the TAP device.
///
/// The virtio net header uses fully general `csum_start` / `csum_offset` fields
/// that can describe any protocol, whereas [`TxMetadata`] uses protocol-specific
/// flags (`offload_tcp_checksum`, `offload_udp_checksum`). This function bridges
/// the two by computing `csum_start` from `l2_len + l3_len` and hardcoding
/// `csum_offset` to the known offset of the checksum field within each protocol
/// header (16 for TCP, 6 for UDP).
///
/// For TSO, `gso_type` is set based on the `is_ipv4`/`is_ipv6` flags, and
/// `NEEDS_CSUM` is always set since the kernel requires the checksum to be
/// partially computed when performing segmentation. For USO,
/// `gso_type` is set to `UDP_L4` and the UDP header length (8) is used.
///
/// If no offload flags are set, an all-zero header is returned, which tells the
/// TAP device that the packet requires no special handling.
fn build_vnet_hdr(meta: &TxMetadata) -> VirtioNetHdr {
    if meta.flags.offload_tcp_segmentation() {
        let protocol = if meta.flags.is_ipv4() {
            VirtioNetHdrGsoProtocol::TCPV4
        } else {
            VirtioNetHdrGsoProtocol::TCPV6
        };
        VirtioNetHdr {
            flags: VirtioNetHdrFlags::new().with_needs_csum(true),
            gso_type: VirtioNetHdrGso::new().with_protocol(protocol),
            hdr_len: meta.l2_len as u16 + meta.l3_len + meta.l4_len as u16,
            gso_size: meta.max_segment_size,
            csum_start: meta.l2_len as u16 + meta.l3_len,
            csum_offset: 16, // TCP checksum field offset
            num_buffers: 0,
        }
    } else if meta.flags.offload_udp_segmentation() {
        VirtioNetHdr {
            flags: VirtioNetHdrFlags::new().with_needs_csum(true),
            gso_type: VirtioNetHdrGso::new().with_protocol(VirtioNetHdrGsoProtocol::UDP_L4),
            hdr_len: meta.l2_len as u16 + meta.l3_len + 8, // 8 = UDP header length
            gso_size: meta.max_segment_size,
            csum_start: meta.l2_len as u16 + meta.l3_len,
            csum_offset: 6, // UDP checksum field offset
            num_buffers: 0,
        }
    } else if meta.flags.offload_tcp_checksum() {
        VirtioNetHdr {
            flags: VirtioNetHdrFlags::new().with_needs_csum(true),
            gso_type: VirtioNetHdrGso::new(),
            hdr_len: 0,
            gso_size: 0,
            csum_start: meta.l2_len as u16 + meta.l3_len,
            csum_offset: 16, // TCP checksum field offset
            num_buffers: 0,
        }
    } else if meta.flags.offload_udp_checksum() {
        VirtioNetHdr {
            flags: VirtioNetHdrFlags::new().with_needs_csum(true),
            gso_type: VirtioNetHdrGso::new(),
            hdr_len: 0,
            gso_size: 0,
            csum_start: meta.l2_len as u16 + meta.l3_len,
            csum_offset: 6, // UDP checksum field offset
            num_buffers: 0,
        }
    } else {
        VirtioNetHdr::default()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum InvalidRxMetadata {
    ReservedFlags,
    ConflictingChecksumFlags,
    UnsupportedGsoProtocol,
    UnsupportedChecksumOffload,
    UnsupportedGsoOffload,
    UnexpectedEcn,
    InvalidGsoHeaderLength,
    InvalidGsoSize,
    InvalidChecksumOffset,
}

/// Parse and validate a TAP vnet header against the guest-negotiated receive
/// capabilities. Enabling a TAP offload allows Linux to emit packet formats
/// that are only safe to forward when the frontend can represent them.
fn parse_vnet_hdr(
    hdr: &VirtioNetHdr,
    frame_len: usize,
    support: net_backend::RxOffloadSupport,
) -> Result<RxMetadata, InvalidRxMetadata> {
    if hdr.flags.into_bits() & !0x03 != 0 || hdr.gso_type.into_bits() & !0x87 != 0 {
        return Err(InvalidRxMetadata::ReservedFlags);
    }
    if hdr.flags.needs_csum() && hdr.flags.data_valid() {
        return Err(InvalidRxMetadata::ConflictingChecksumFlags);
    }

    let protocol = hdr.gso_type.protocol();
    let gso = match protocol {
        VirtioNetHdrGsoProtocol::NONE => {
            if hdr.gso_type.ecn() {
                return Err(InvalidRxMetadata::UnexpectedEcn);
            }
            if hdr.hdr_len != 0 || hdr.gso_size != 0 {
                return Err(InvalidRxMetadata::InvalidGsoSize);
            }
            None
        }
        VirtioNetHdrGsoProtocol::TCPV4 | VirtioNetHdrGsoProtocol::TCPV6 => {
            let (supported, rx_protocol, minimum_header_len) =
                if protocol == VirtioNetHdrGsoProtocol::TCPV4 {
                    (
                        support.tcpv4_gso,
                        RxGsoProtocol::TcpV4,
                        ETHERNET_HEADER_LEN as usize + 20 + 20,
                    )
                } else {
                    (
                        support.tcpv6_gso,
                        RxGsoProtocol::TcpV6,
                        ETHERNET_HEADER_LEN as usize + 40 + 20,
                    )
                };
            if !supported {
                return Err(InvalidRxMetadata::UnsupportedGsoOffload);
            }
            // VIRTIO_NET_F_GUEST_ECN is not advertised by this frontend.
            if hdr.gso_type.ecn() {
                return Err(InvalidRxMetadata::UnexpectedEcn);
            }
            if !hdr.flags.needs_csum() {
                return Err(InvalidRxMetadata::UnsupportedChecksumOffload);
            }
            let header_len = hdr.hdr_len as usize;
            if header_len < minimum_header_len || header_len > frame_len {
                return Err(InvalidRxMetadata::InvalidGsoHeaderLength);
            }
            let payload_len = frame_len - header_len;
            if hdr.gso_size == 0 || hdr.gso_size as usize > payload_len {
                return Err(InvalidRxMetadata::InvalidGsoSize);
            }
            Some(RxGso {
                protocol: rx_protocol,
                header_len: hdr.hdr_len,
                max_segment_size: hdr.gso_size,
                ecn: false,
            })
        }
        _ => return Err(InvalidRxMetadata::UnsupportedGsoProtocol),
    };

    let checksum_offload = if hdr.flags.needs_csum() {
        if !support.checksum {
            return Err(InvalidRxMetadata::UnsupportedChecksumOffload);
        }
        let checksum_end = usize::from(hdr.csum_start)
            .checked_add(usize::from(hdr.csum_offset))
            .and_then(|offset| offset.checked_add(size_of::<u16>()))
            .ok_or(InvalidRxMetadata::InvalidChecksumOffset)?;
        let checksum_limit = gso
            .map(|metadata| metadata.header_len as usize)
            .unwrap_or(frame_len);
        if checksum_end > checksum_limit {
            return Err(InvalidRxMetadata::InvalidChecksumOffset);
        }
        Some(RxChecksumOffload {
            start: hdr.csum_start,
            offset: hdr.csum_offset,
        })
    } else {
        if hdr.csum_start != 0 || hdr.csum_offset != 0 {
            return Err(InvalidRxMetadata::InvalidChecksumOffset);
        }
        None
    };

    let (ip_checksum, l4_checksum) = if hdr.flags.data_valid() {
        (RxChecksumState::Good, RxChecksumState::Good)
    } else {
        (RxChecksumState::Unknown, RxChecksumState::Unknown)
    };

    let l4_protocol = match hdr.gso_type.protocol() {
        VirtioNetHdrGsoProtocol::TCPV4 | VirtioNetHdrGsoProtocol::TCPV6 => L4Protocol::Tcp,
        VirtioNetHdrGsoProtocol::UDP | VirtioNetHdrGsoProtocol::UDP_L4 => L4Protocol::Udp,
        _ => L4Protocol::Unknown,
    };
    Ok(RxMetadata {
        offset: 0,
        len: 0,
        ip_checksum,
        l4_checksum,
        l4_protocol,
        checksum_offload,
        gso,
        vlan: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use net_backend::TxFlags;

    fn all_rx_offloads() -> net_backend::RxOffloadSupport {
        net_backend::RxOffloadSupport {
            checksum: true,
            tcpv4_gso: true,
            tcpv6_gso: true,
        }
    }

    #[test]
    fn vnet_hdr_from_tx_metadata_csum() {
        let meta = TxMetadata {
            flags: TxFlags::new()
                .with_offload_tcp_checksum(true)
                .with_is_ipv4(true),
            l2_len: 14,
            l3_len: 20,
            ..Default::default()
        };
        let hdr = build_vnet_hdr(&meta);
        assert!(hdr.flags.needs_csum());
        assert!(!hdr.flags.data_valid());
        assert_eq!(hdr.csum_start, 14 + 20);
        assert_eq!(hdr.csum_offset, 16);
        assert_eq!(hdr.gso_type.protocol(), VirtioNetHdrGsoProtocol::NONE);
        assert_eq!(hdr.gso_size, 0);
    }

    #[test]
    fn vnet_hdr_from_tx_metadata_tso() {
        let meta = TxMetadata {
            flags: TxFlags::new()
                .with_offload_tcp_segmentation(true)
                .with_offload_tcp_checksum(true)
                .with_is_ipv4(true),
            l2_len: 14,
            l3_len: 20,
            l4_len: 32,
            max_segment_size: 1460,
            ..Default::default()
        };
        let hdr = build_vnet_hdr(&meta);
        assert_eq!(hdr.gso_type.protocol(), VirtioNetHdrGsoProtocol::TCPV4);
        assert_eq!(hdr.gso_size, 1460);
        assert_eq!(hdr.hdr_len, 14 + 20 + 32);
        assert!(hdr.flags.needs_csum());
        assert!(!hdr.flags.data_valid());
        assert_eq!(hdr.csum_start, 14 + 20);
        assert_eq!(hdr.csum_offset, 16);
    }

    #[test]
    fn vnet_hdr_from_tx_metadata_none() {
        let meta = TxMetadata::default();
        let hdr = build_vnet_hdr(&meta);
        assert!(!hdr.flags.needs_csum());
        assert!(!hdr.flags.data_valid());
        assert_eq!(hdr.gso_type.protocol(), VirtioNetHdrGsoProtocol::NONE);
        assert_eq!(hdr.hdr_len, 0);
        assert_eq!(hdr.gso_size, 0);
        assert_eq!(hdr.csum_start, 0);
        assert_eq!(hdr.csum_offset, 0);
    }

    #[test]
    fn vnet_hdr_from_tx_metadata_udp_csum() {
        let meta = TxMetadata {
            flags: TxFlags::new()
                .with_offload_udp_checksum(true)
                .with_is_ipv4(true),
            l2_len: 14,
            l3_len: 20,
            ..Default::default()
        };
        let hdr = build_vnet_hdr(&meta);
        assert!(hdr.flags.needs_csum());
        assert_eq!(hdr.csum_start, 14 + 20);
        assert_eq!(hdr.csum_offset, 6);
        assert_eq!(hdr.gso_type.protocol(), VirtioNetHdrGsoProtocol::NONE);
    }

    #[test]
    fn rx_metadata_from_vnet_hdr_valid() {
        let hdr = VirtioNetHdr {
            flags: VirtioNetHdrFlags::new().with_data_valid(true),
            ..Default::default()
        };
        let meta = parse_vnet_hdr(&hdr, 1500, all_rx_offloads()).unwrap();
        assert_eq!(meta.ip_checksum, RxChecksumState::Good);
        assert_eq!(meta.l4_checksum, RxChecksumState::Good);
        assert_eq!(meta.l4_protocol, L4Protocol::Unknown);
    }

    #[test]
    fn rx_metadata_from_vnet_hdr_needs_csum_treated_as_unknown() {
        let hdr = VirtioNetHdr {
            flags: VirtioNetHdrFlags::new().with_needs_csum(true),
            gso_type: VirtioNetHdrGso::new().with_protocol(VirtioNetHdrGsoProtocol::TCPV6),
            hdr_len: 74,
            gso_size: 1440,
            csum_start: 54,
            csum_offset: 16,
            ..Default::default()
        };
        let meta = parse_vnet_hdr(&hdr, 4096, all_rx_offloads()).unwrap();
        assert_eq!(meta.ip_checksum, RxChecksumState::Unknown);
        assert_eq!(meta.l4_checksum, RxChecksumState::Unknown);
        assert_eq!(meta.l4_protocol, L4Protocol::Tcp);
        assert_eq!(
            meta.checksum_offload,
            Some(RxChecksumOffload {
                start: 54,
                offset: 16
            })
        );
        assert_eq!(
            meta.gso,
            Some(RxGso {
                protocol: RxGsoProtocol::TcpV6,
                header_len: 74,
                max_segment_size: 1440,
                ecn: false,
            })
        );
    }

    #[test]
    fn rx_metadata_from_vnet_hdr_none() {
        let hdr = VirtioNetHdr::default();
        let meta = parse_vnet_hdr(&hdr, 1500, all_rx_offloads()).unwrap();
        assert_eq!(meta.ip_checksum, RxChecksumState::Unknown);
        assert_eq!(meta.l4_checksum, RxChecksumState::Unknown);
        assert_eq!(meta.l4_protocol, L4Protocol::Unknown);
    }

    #[test]
    fn rx_metadata_rejects_unsupported_udp_gso() {
        let hdr = VirtioNetHdr {
            flags: VirtioNetHdrFlags::new().with_data_valid(true),
            gso_type: VirtioNetHdrGso::new().with_protocol(VirtioNetHdrGsoProtocol::UDP),
            ..Default::default()
        };
        assert_eq!(
            parse_vnet_hdr(&hdr, 1500, all_rx_offloads()).unwrap_err(),
            InvalidRxMetadata::UnsupportedGsoProtocol
        );
    }

    #[test]
    fn rx_metadata_rejects_unnegotiated_gso() {
        let hdr = VirtioNetHdr {
            flags: VirtioNetHdrFlags::new().with_needs_csum(true),
            gso_type: VirtioNetHdrGso::new().with_protocol(VirtioNetHdrGsoProtocol::TCPV4),
            hdr_len: 54,
            gso_size: 1460,
            csum_start: 34,
            csum_offset: 16,
            ..Default::default()
        };
        assert_eq!(
            parse_vnet_hdr(
                &hdr,
                4096,
                net_backend::RxOffloadSupport {
                    checksum: true,
                    ..Default::default()
                }
            )
            .unwrap_err(),
            InvalidRxMetadata::UnsupportedGsoOffload
        );
    }

    #[test]
    fn rx_metadata_rejects_ecn_without_negotiation() {
        let hdr = VirtioNetHdr {
            flags: VirtioNetHdrFlags::new().with_needs_csum(true),
            gso_type: VirtioNetHdrGso::new()
                .with_protocol(VirtioNetHdrGsoProtocol::TCPV4)
                .with_ecn(true),
            hdr_len: 54,
            gso_size: 1460,
            csum_start: 34,
            csum_offset: 16,
            ..Default::default()
        };
        assert_eq!(
            parse_vnet_hdr(&hdr, 4096, all_rx_offloads()).unwrap_err(),
            InvalidRxMetadata::UnexpectedEcn
        );
    }

    #[test]
    fn rx_metadata_rejects_invalid_checksum_offset() {
        let hdr = VirtioNetHdr {
            flags: VirtioNetHdrFlags::new().with_needs_csum(true),
            csum_start: 1490,
            csum_offset: 16,
            ..Default::default()
        };
        assert_eq!(
            parse_vnet_hdr(&hdr, 1500, all_rx_offloads()).unwrap_err(),
            InvalidRxMetadata::InvalidChecksumOffset
        );
    }

    #[test]
    fn rx_metadata_rejects_zero_gso_size() {
        let hdr = VirtioNetHdr {
            flags: VirtioNetHdrFlags::new().with_needs_csum(true),
            gso_type: VirtioNetHdrGso::new().with_protocol(VirtioNetHdrGsoProtocol::TCPV6),
            hdr_len: 74,
            gso_size: 0,
            csum_start: 54,
            csum_offset: 16,
            ..Default::default()
        };
        assert_eq!(
            parse_vnet_hdr(&hdr, 4096, all_rx_offloads()).unwrap_err(),
            InvalidRxMetadata::InvalidGsoSize
        );
    }

    #[test]
    fn vnet_hdr_from_tx_metadata_uso() {
        let meta = TxMetadata {
            flags: TxFlags::new()
                .with_offload_udp_segmentation(true)
                .with_offload_udp_checksum(true)
                .with_is_ipv4(true),
            l2_len: 14,
            l3_len: 20,
            max_segment_size: 1472,
            ..Default::default()
        };
        let hdr = build_vnet_hdr(&meta);
        assert_eq!(hdr.gso_type.protocol(), VirtioNetHdrGsoProtocol::UDP_L4);
        assert_eq!(hdr.gso_size, 1472);
        assert_eq!(hdr.hdr_len, 14 + 20 + 8);
        assert!(hdr.flags.needs_csum());
        assert_eq!(hdr.csum_start, 14 + 20);
        assert_eq!(hdr.csum_offset, 6);
    }

    #[test]
    fn ipv4_header_checksum_fixup() {
        // Ethernet (14) + IPv4 header (20) with zero checksum field.
        let mut packet = vec![
            // Ethernet header (14 bytes)
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00,
            // IPv4 header (20 bytes)
            0x45, 0x00, 0x00, 0x28, // version/IHL, DSCP, total length
            0x00, 0x01, 0x00, 0x00, // id, flags, fragment offset
            0x40, 0x06, 0x00, 0x00, // TTL=64, proto=TCP, checksum=0
            0x0a, 0x00, 0x00, 0x01, // src: 10.0.0.1
            0x0a, 0x00, 0x00, 0x02, // dst: 10.0.0.2
        ];
        fixup_ipv4_header_checksum(&mut packet, 14);
        let csum = u16::from_be_bytes([packet[24], packet[25]]);
        // Verify by summing all 16-bit words of the IP header;
        // the result (with checksum included) should fold to 0xffff.
        let mut sum: u32 = 0;
        for chunk in packet[14..34].chunks(2) {
            sum += u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        assert_eq!(sum as u16, 0xffff);
        assert_ne!(csum, 0, "checksum should be non-zero");
    }

    #[test]
    fn ipv4_lso_total_length_fixup() {
        // NDIS/netvsp LSO guests zero the IPv4 total-length field, expecting
        // the offload engine to fill it. The fixup must set it to the full
        // datagram length so the kernel TAP GSO engine can segment the frame.
        let mut packet = vec![
            // Ethernet header (14 bytes)
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00,
            // IPv4 header (20 bytes) with total-length field zeroed (LSO convention)
            0x45, 0x00, 0x00, 0x00, // version/IHL, DSCP, total length = 0
            0x00, 0x01, 0x00, 0x00, // id, flags, fragment offset
            0x40, 0x06, 0x00, 0x00, // TTL=64, proto=TCP, checksum=0
            0x0a, 0x00, 0x00, 0x01, // src: 10.0.0.1
            0x0a, 0x00, 0x00, 0x02, // dst: 10.0.0.2
        ];
        // Append a TCP header + payload so the datagram exceeds the IP header.
        packet.extend(std::iter::repeat_n(0u8, 40));
        let expected_total = (packet.len() - 14) as u16; // 20 (IP) + 40 = 60
        fixup_ipv4_header_checksum(&mut packet, 14);
        let total = u16::from_be_bytes([packet[16], packet[17]]);
        assert_eq!(
            total, expected_total,
            "IP total-length must be set to the datagram length"
        );
    }

    #[test]
    fn ipv6_lso_payload_length_fixup() {
        // NDIS/netvsp LSO guests zero the IPv6 payload-length field; the fixup
        // must set it to the length of everything after the 40-byte IPv6 header.
        let mut packet = vec![
            // Ethernet header (14 bytes), ethertype 0x86dd = IPv6
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x86, 0xdd,
            // IPv6 header (40 bytes) with payload-length field zeroed
            0x60, 0x00, 0x00, 0x00, // version=6 / traffic class / flow label
            0x00, 0x00, 0x06, 0x40, // payload length = 0, next-header = TCP, hop limit = 64
            0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, // src
            0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02, // dst
        ];
        // Append a TCP header + payload after the IPv6 header.
        packet.extend(std::iter::repeat_n(0u8, 40));
        let expected_payload = (packet.len() - 14 - 40) as u16; // 40 here
        fixup_ipv6_payload_length(&mut packet, 14);
        let payload = u16::from_be_bytes([packet[14 + 4], packet[14 + 5]]);
        assert_eq!(
            payload, expected_payload,
            "IPv6 payload-length must be set to the datagram payload length"
        );
    }

    #[test]
    fn ipv4_oversize_total_length_is_zero_and_checksum_consistent() {
        // A super-frame whose datagram length exceeds the 16-bit IPv4
        // total-length field gets a zero total-length, and the recomputed
        // header checksum is consistent with that defined value.
        let mut packet = vec![0u8; 14 + 20 + 70_000];
        packet[14] = 0x45; // IPv4, IHL = 5 (20-byte header)
        fixup_ipv4_header_checksum(&mut packet, 14);
        let total = u16::from_be_bytes([packet[16], packet[17]]);
        assert_eq!(total, 0, "oversize total-length must be zero");
        // The header (including the checksum field) must fold to 0xffff,
        // i.e. the checksum is consistent with the zeroed total-length.
        let mut sum: u32 = 0;
        for chunk in packet[14..34].chunks(2) {
            sum += u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        assert_eq!(sum as u16, 0xffff, "checksum inconsistent with header");
    }

    #[test]
    fn ipv6_oversize_payload_length_is_zero() {
        // A super-frame whose payload exceeds the 16-bit IPv6 payload-length
        // field gets a zero payload-length (jumbogram convention).
        let mut packet = vec![0u8; 14 + 40 + 70_000];
        packet[14] = 0x60; // IPv6, version = 6
        fixup_ipv6_payload_length(&mut packet, 14);
        let payload = u16::from_be_bytes([packet[18], packet[19]]);
        assert_eq!(payload, 0, "oversize payload-length must be zero");
    }
}
