// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A TAP interface based endpoint.

#![cfg(target_os = "linux")]
#![expect(missing_docs)]

pub mod resolver;
pub mod tap;
mod tx;

use async_trait::async_trait;
use futures::io::AsyncRead;
use inspect::InspectMut;
use linux_net_bindings::gen_if_tun;
use net_backend::BufferAccess;
use net_backend::ETHERNET_VLAN_HEADER_LEN;
use net_backend::Endpoint;
use net_backend::L4Protocol;
use net_backend::Queue;
use net_backend::QueueConfig;
use net_backend::RssConfig;
use net_backend::RxChecksumState;
use net_backend::RxGsoProtocol;
use net_backend::RxId;
use net_backend::RxMetadata;
use net_backend::RxOffload;
use net_backend::RxOffloadSupport;
use net_backend::TxError;
use net_backend::TxId;
use net_backend::TxMetadata;
use net_backend::TxOffloadSupport;
use net_backend::TxSegment;
use pal_async::driver::Driver;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use zerocopy::FromBytes;

const MAX_TAP_FRAME_LEN: usize =
    gen_if_tun::ETH_MAX_MTU as usize + ETHERNET_VLAN_HEADER_LEN as usize;
// Linux TAP contractually returns a complete frame within this bound. Size the
// read buffer to that contract rather than adding overflow detection.
const TAP_READ_BUFFER_LEN: usize = size_of::<VirtioNetHdr>() + MAX_TAP_FRAME_LEN;
const MAX_RX_PACKETS_PER_POLL: usize = 64;

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
    enabled_rx_offloads: Option<RxOffloadSupport>,
    stale_offload_recovery: Arc<Mutex<StaleOffloadRecovery>>,
}

impl TapEndpoint {
    pub fn new(tap: tap::Tap) -> Result<Self, tap::Error> {
        // Reset state inherited from an earlier user of this persistent TAP
        // device. Packets already queued before this ioctl can still carry the
        // previous offload metadata, so the first queue starts with recovery
        // enabled until it observes the queue drain.
        tap.set_offloads(0)?;
        Ok(Self {
            tap: Arc::new(Mutex::new(Some(tap))),
            enabled_rx_offloads: None,
            stale_offload_recovery: Arc::new(Mutex::new(StaleOffloadRecovery::all())),
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
        let enabled_rx_offloads = config.enabled_rx_offloads;
        let mut offloads = 0;
        if enabled_rx_offloads.checksum {
            offloads |= gen_if_tun::TUN_F_CSUM;
        }
        if enabled_rx_offloads.tcpv4_gso {
            offloads |= gen_if_tun::TUN_F_TSO4;
        }
        if enabled_rx_offloads.tcpv6_gso {
            offloads |= gen_if_tun::TUN_F_TSO6;
        }
        self.tap
            .lock()
            .as_ref()
            .expect("queue is already in use")
            .set_offloads(offloads)?;

        self.stale_offload_recovery
            .lock()
            .merge(StaleOffloadRecovery::after_reconfiguration(
                self.enabled_rx_offloads,
                enabled_rx_offloads,
            ));
        self.enabled_rx_offloads = Some(enabled_rx_offloads);
        queues.push(Box::new(TapQueue::new(
            config.driver.as_ref(),
            self.tap.clone(),
            self.stale_offload_recovery.clone(),
            enabled_rx_offloads,
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
    stale_offload_recovery_slot: Arc<Mutex<StaleOffloadRecovery>>,
    tap: Option<tap::PolledTap>,
    inner: Inner,
    buffer: Box<[u8]>,
    enabled_rx_offloads: RxOffloadSupport,
    stale_offload_recovery: StaleOffloadRecovery,
    read_since_pending: bool,
}

struct Inner {
    rx_free: VecDeque<RxId>,
    rx_ready: VecDeque<RxId>,
}

impl Inner {
    fn complete_rx(&mut self, rx: RxId) {
        let oldest = self.rx_free.pop_front().expect("RX queue is empty");
        assert_eq!(oldest.0, rx.0);
        self.rx_ready.push_back(rx);
    }
}

impl InspectMut for TapQueue {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond();
    }
}

impl Drop for TapQueue {
    fn drop(&mut self) {
        self.stale_offload_recovery_slot
            .lock()
            .merge(self.stale_offload_recovery);
        if let Some(tap) = self.tap.take() {
            *self.slot.lock() = Some(tap.into_inner());
        }
    }
}

impl TapQueue {
    fn new(
        driver: &dyn Driver,
        slot: Arc<Mutex<Option<tap::Tap>>>,
        stale_offload_recovery_slot: Arc<Mutex<StaleOffloadRecovery>>,
        enabled_rx_offloads: RxOffloadSupport,
    ) -> anyhow::Result<Self> {
        let tap = slot.lock().take().expect("queue is already in use");
        let tap = tap.polled(driver)?;
        let stale_offload_recovery = std::mem::take(&mut *stale_offload_recovery_slot.lock());
        Ok(Self {
            slot,
            stale_offload_recovery_slot,
            tap: Some(tap),
            inner: Inner {
                rx_free: VecDeque::new(),
                rx_ready: VecDeque::new(),
            },
            buffer: vec![0; TAP_READ_BUFFER_LEN].into_boxed_slice(),
            enabled_rx_offloads,
            stale_offload_recovery,
            read_since_pending: false,
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

        let mut packets_processed = 0;
        while let Some(&rx) = self.inner.rx_free.front() {
            if packets_processed == MAX_RX_PACKETS_PER_POLL {
                cx.waker().wake_by_ref();
                break;
            }

            match Pin::new(&mut *tap).poll_read(cx, &mut self.buffer) {
                Poll::Ready(Ok(read_len)) => {
                    self.read_since_pending = true;
                    packets_processed += 1;
                    let (hdr, frame) = VirtioNetHdr::read_from_prefix(&self.buffer[..read_len])
                        .expect("TAP packet is missing the vnet header");
                    assert!(
                        !frame.is_empty(),
                        "TAP packet is missing the Ethernet frame"
                    );
                    let frame_len = frame.len();
                    assert_vnet_hdr_contract(&hdr, frame_len);
                    let Some(rx_meta) = parse_vnet_hdr_for_queue(
                        &hdr,
                        self.enabled_rx_offloads,
                        &self.stale_offload_recovery,
                    ) else {
                        continue;
                    };
                    let capacity = pool.capacity(rx);
                    if frame_len <= capacity as usize {
                        pool.write_packet(
                            rx,
                            &RxMetadata {
                                offset: 0,
                                len: frame_len,
                                ..rx_meta
                            },
                            frame,
                        );
                    } else {
                        continue;
                    }

                    self.inner.complete_rx(rx);
                }
                Poll::Ready(Err(err)) => {
                    tracing::warn!(error = &err as &dyn std::error::Error, "tap rx error");
                    break;
                }
                Poll::Pending => {
                    self.stale_offload_recovery
                        .queue_pending(&mut self.read_since_pending);
                    break;
                }
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
        self.transmit(pool, &mut segments)
    }

    fn tx_poll(
        &mut self,
        _pool: &mut dyn BufferAccess,
        done: &mut [TxId],
    ) -> Result<usize, TxError> {
        self.poll_tx_done(done)
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
/// field directly. Without it the host's IPv6 input validation (`ip6_rcv_core`,
/// and `br_validate_ipv6` on a br_netfilter bridge) trims the frame to its bare
/// header before segmentation, collapsing TX. Linux accepts a zero payload
/// length only on a TCP GSO frame, and only from 7.0.
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

/// Rewrite the L4 checksum field of a segmentation-offload (GSO) frame to the
/// pseudo-header checksum that includes the full L4 segment length.
///
/// NDIS/netvsp LSO hands us a length-less pseudo-header seed: the guest cannot know
/// the per-segment length, so it leaves it out. The kernel's GSO path expects the
/// opposite. The stack's own `__tcp_v4_send_check` writes `th->check =
/// ~tcp_v4_check(skb->len, ...)`, the pseudo-header computed over the full skb L4
/// length, and `tcp_gso_segment` (and the hardware TSO drivers) then adjust it down
/// per segment. Passing the length-less seed straight through leaves that adjustment
/// off by the segment length, so every emitted segment carries a wrong checksum and
/// the far end drops it. Recompute the seed here from the addresses and protocol in
/// the packet's IP header plus the L4 length taken from the frame size, so it does
/// not depend on the value the guest supplied; the data is untouched, the
/// kernel or NIC still completes the per-segment checksum.
///
/// For IPv6 the extension-header chain (hop-by-hop, routing, destination-options)
/// is walked to find the upper-layer header. If it ends in an Authentication/ESP
/// header or an unrecognized next-header the L4 offset cannot be determined and the
/// seed is left unchanged.
///
/// Fragments (IPv4 with MF or a fragment offset, or IPv6 with a Fragment header) are
/// also left unchanged: a non-first fragment has no L4 header at the computed offset,
/// and a fragment is never a valid segmentation frame.
fn fixup_gso_pseudo_header(packet: &mut [u8], l2_len: usize) {
    if packet.len() < l2_len + 1 {
        return;
    }
    let (l4_off, proto, mut sum) = match packet[l2_len] >> 4 {
        4 => {
            if packet.len() < l2_len + 20 {
                return;
            }
            let ihl = ((packet[l2_len] & 0x0f) as usize) * 4;
            if !(20..=60).contains(&ihl) || packet.len() < l2_len + ihl {
                return;
            }
            // MF flag or a non-zero fragment offset.
            let frag = u16::from_be_bytes([packet[l2_len + 6], packet[l2_len + 7]]);
            if frag & 0x3fff != 0 {
                return;
            }
            let proto = packet[l2_len + 9];
            let mut sum: u32 = 0;
            for chunk in packet[l2_len + 12..l2_len + 20].chunks_exact(2) {
                sum += u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
            }
            (l2_len + ihl, proto, sum)
        }
        6 => {
            const IPV6_HEADER_LEN: usize = 40;
            if packet.len() < l2_len + IPV6_HEADER_LEN {
                return;
            }
            // The 16-byte source and destination addresses (bytes 8..40) are in the
            // fixed header regardless of any extension headers.
            let mut sum: u32 = 0;
            for chunk in packet[l2_len + 8..l2_len + IPV6_HEADER_LEN].chunks_exact(2) {
                sum += u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
            }
            // Walk the extension-header chain to the upper-layer header. The base
            // header's Next Header may name a hop-by-hop, routing or
            // destination-options extension header rather than TCP/UDP directly.
            let mut proto = packet[l2_len + 6];
            let mut off = l2_len + IPV6_HEADER_LEN;
            loop {
                match proto {
                    6 | 17 => break,
                    // Length-prefixed extension headers: Hdr Ext Len counts 8-octet
                    // units beyond the first 8, and the first byte is the next header.
                    0 | 43 | 60 => {
                        if packet.len() < off + 2 {
                            return;
                        }
                        let ext_len = (packet[off + 1] as usize + 1) * 8;
                        if packet.len() < off + ext_len {
                            return;
                        }
                        proto = packet[off];
                        off += ext_len;
                    }
                    // Fragment/Authentication/ESP/no-next-header/unknown: the
                    // upper-layer header cannot be located, so leave the seed unchanged.
                    _ => return,
                }
            }
            (off, proto, sum)
        }
        _ => return,
    };
    let csum_field = match proto {
        6 => l4_off + 16,
        17 => l4_off + 6,
        _ => return,
    };
    if packet.len() < csum_field + 2 {
        return;
    }
    // Pseudo-header: src and dst addresses, protocol, and the L4 length (header plus
    // payload). The folded, non-complemented sum is the CHECKSUM_PARTIAL seed.
    let l4_len = packet.len() - l4_off;
    sum += proto as u32 + l4_len as u32;
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    packet[csum_field..csum_field + 2].copy_from_slice(&(sum as u16).to_be_bytes());
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
    UnsupportedGsoProtocol,
    UnsupportedChecksumOffload,
    UnsupportedGsoOffload(RxGsoProtocol),
    UnexpectedEcn,
}

#[derive(Debug, Copy, Clone, Default)]
struct StaleOffloadRecovery {
    inherited: bool,
    checksum: bool,
    tcpv4_gso: bool,
    tcpv6_gso: bool,
}

impl StaleOffloadRecovery {
    fn all() -> Self {
        Self {
            inherited: true,
            checksum: true,
            tcpv4_gso: true,
            tcpv6_gso: true,
        }
    }

    fn after_reconfiguration(
        previous: Option<RxOffloadSupport>,
        current: RxOffloadSupport,
    ) -> Self {
        let Some(previous) = previous else {
            return Self::default();
        };
        Self {
            inherited: false,
            checksum: previous.checksum && !current.checksum,
            tcpv4_gso: previous.tcpv4_gso && !current.tcpv4_gso,
            tcpv6_gso: previous.tcpv6_gso && !current.tcpv6_gso,
        }
    }

    fn permits(&self, reason: &InvalidRxMetadata) -> bool {
        match reason {
            InvalidRxMetadata::UnsupportedChecksumOffload => self.checksum,
            InvalidRxMetadata::UnsupportedGsoOffload(RxGsoProtocol::TcpV4) => self.tcpv4_gso,
            InvalidRxMetadata::UnsupportedGsoOffload(RxGsoProtocol::TcpV6) => self.tcpv6_gso,
            InvalidRxMetadata::UnsupportedGsoProtocol | InvalidRxMetadata::UnexpectedEcn => {
                self.inherited
            }
        }
    }

    fn merge(&mut self, other: Self) {
        self.inherited |= other.inherited;
        self.checksum |= other.checksum;
        self.tcpv4_gso |= other.tcpv4_gso;
        self.tcpv6_gso |= other.tcpv6_gso;
    }

    fn queue_pending(&mut self, read_since_pending: &mut bool) {
        if *read_since_pending {
            *self = Self::default();
            *read_since_pending = false;
        }
    }
}

fn parse_vnet_hdr_for_queue(
    hdr: &VirtioNetHdr,
    support: RxOffloadSupport,
    recovery: &StaleOffloadRecovery,
) -> Option<RxMetadata> {
    match parse_vnet_hdr(hdr, support) {
        Ok(metadata) => Some(metadata),
        Err(reason) if recovery.permits(&reason) => {
            tracelimit::warn_ratelimited!(
                ?reason,
                "dropping TAP packet queued before RX offload reconfiguration"
            );
            None
        }
        Err(reason) => {
            panic!("TAP returned invalid vnet metadata: {reason:?}");
        }
    }
}

fn assert_vnet_hdr_contract(hdr: &VirtioNetHdr, frame_len: usize) {
    assert!(
        !(hdr.flags.needs_csum() && hdr.flags.data_valid()),
        "TAP returned conflicting checksum flags"
    );

    if hdr.flags.needs_csum() {
        let checksum_end = usize::from(hdr.csum_start)
            .checked_add(usize::from(hdr.csum_offset))
            .and_then(|offset| offset.checked_add(size_of::<u16>()))
            .expect("TAP checksum metadata overflowed");
        assert!(
            checksum_end <= frame_len,
            "TAP checksum metadata exceeds the Ethernet frame"
        );
    }

    match hdr.gso_type.protocol() {
        VirtioNetHdrGsoProtocol::TCPV4 | VirtioNetHdrGsoProtocol::TCPV6 => {
            assert!(
                hdr.flags.needs_csum() || hdr.flags.data_valid(),
                "TAP returned TCP GSO without a checksum state"
            );
            assert_ne!(hdr.gso_size, 0, "TAP returned a zero GSO segment size");
            assert!(
                usize::from(hdr.hdr_len) <= frame_len,
                "TAP GSO header exceeds the Ethernet frame"
            );
            if hdr.flags.needs_csum() {
                assert_eq!(
                    hdr.csum_offset, 16,
                    "TAP returned an invalid TCP checksum offset"
                );
            }
        }
        _ => {}
    }
}

fn parse_vnet_hdr(
    hdr: &VirtioNetHdr,
    support: RxOffloadSupport,
) -> Result<RxMetadata, InvalidRxMetadata> {
    let protocol = hdr.gso_type.protocol();
    let offload = match protocol {
        VirtioNetHdrGsoProtocol::NONE => {
            if hdr.gso_type.ecn() {
                return Err(InvalidRxMetadata::UnexpectedEcn);
            }
            if hdr.flags.needs_csum() {
                if !support.checksum {
                    return Err(InvalidRxMetadata::UnsupportedChecksumOffload);
                }
                RxOffload::checksum(hdr.csum_start, hdr.csum_offset)
            } else {
                RxOffload::default()
            }
        }
        VirtioNetHdrGsoProtocol::TCPV4 | VirtioNetHdrGsoProtocol::TCPV6 => {
            // VIRTIO_NET_F_GUEST_ECN is not advertised by this frontend.
            if hdr.gso_type.ecn() {
                return Err(InvalidRxMetadata::UnexpectedEcn);
            }
            let (supported, protocol) = if protocol == VirtioNetHdrGsoProtocol::TCPV4 {
                (support.tcpv4_gso, RxGsoProtocol::TcpV4)
            } else {
                (support.tcpv6_gso, RxGsoProtocol::TcpV6)
            };
            if !supported {
                return Err(InvalidRxMetadata::UnsupportedGsoOffload(protocol));
            }
            if hdr.flags.needs_csum() && !support.checksum {
                return Err(InvalidRxMetadata::UnsupportedChecksumOffload);
            }
            RxOffload::tcp_gso(
                protocol,
                hdr.hdr_len,
                hdr.gso_size,
                hdr.flags.needs_csum().then_some(hdr.csum_start),
            )
        }
        _ => return Err(InvalidRxMetadata::UnsupportedGsoProtocol),
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
        offload,
        vlan: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use net_backend::TxFlags;

    fn all_rx_offloads() -> RxOffloadSupport {
        RxOffloadSupport {
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
        let meta = parse_vnet_hdr(&hdr, all_rx_offloads()).unwrap();
        assert_eq!(meta.ip_checksum, RxChecksumState::Good);
        assert_eq!(meta.l4_checksum, RxChecksumState::Good);
        assert_eq!(meta.l4_protocol, L4Protocol::Unknown);
        assert_eq!(meta.offload, RxOffload::default());
    }

    #[test]
    fn rx_metadata_from_vnet_hdr_checksum() {
        let hdr = VirtioNetHdr {
            flags: VirtioNetHdrFlags::new().with_needs_csum(true),
            csum_start: 34,
            csum_offset: 16,
            ..Default::default()
        };
        let meta = parse_vnet_hdr(&hdr, all_rx_offloads()).unwrap();
        assert_eq!(meta.offload, RxOffload::checksum(34, 16));
    }

    #[test]
    fn rx_metadata_from_vnet_hdr_tcp_gso() {
        let hdr = VirtioNetHdr {
            flags: VirtioNetHdrFlags::new().with_needs_csum(true),
            gso_type: VirtioNetHdrGso::new().with_protocol(VirtioNetHdrGsoProtocol::TCPV6),
            hdr_len: 74,
            gso_size: 1440,
            csum_start: 54,
            csum_offset: 16,
            ..Default::default()
        };
        let meta = parse_vnet_hdr(&hdr, all_rx_offloads()).unwrap();
        assert_eq!(meta.l4_protocol, L4Protocol::Tcp);
        assert_eq!(
            meta.offload,
            RxOffload::tcp_gso(RxGsoProtocol::TcpV6, 74, 1440, Some(54))
        );
    }

    #[test]
    fn vnet_hdr_contract_accepts_data_valid_tcp_gso() {
        let hdr = VirtioNetHdr {
            flags: VirtioNetHdrFlags::new().with_data_valid(true),
            gso_type: VirtioNetHdrGso::new().with_protocol(VirtioNetHdrGsoProtocol::TCPV4),
            hdr_len: 54,
            gso_size: 1460,
            ..Default::default()
        };
        assert_vnet_hdr_contract(&hdr, 1514);
    }

    #[test]
    fn vnet_hdr_contract_accepts_needs_csum_tcp_gso() {
        let hdr = VirtioNetHdr {
            flags: VirtioNetHdrFlags::new().with_needs_csum(true),
            gso_type: VirtioNetHdrGso::new().with_protocol(VirtioNetHdrGsoProtocol::TCPV6),
            hdr_len: 74,
            gso_size: 1440,
            csum_start: 54,
            csum_offset: 16,
            ..Default::default()
        };
        assert_vnet_hdr_contract(&hdr, 1514);
    }

    #[test]
    #[should_panic(expected = "TAP returned conflicting checksum flags")]
    fn vnet_hdr_contract_rejects_conflicting_tcp_gso_checksum_state() {
        let hdr = VirtioNetHdr {
            flags: VirtioNetHdrFlags::new()
                .with_needs_csum(true)
                .with_data_valid(true),
            gso_type: VirtioNetHdrGso::new().with_protocol(VirtioNetHdrGsoProtocol::TCPV4),
            hdr_len: 54,
            gso_size: 1460,
            csum_start: 34,
            csum_offset: 16,
            ..Default::default()
        };
        assert_vnet_hdr_contract(&hdr, 1514);
    }

    #[test]
    #[should_panic(expected = "TAP returned TCP GSO without a checksum state")]
    fn vnet_hdr_contract_rejects_missing_tcp_gso_checksum_state() {
        let hdr = VirtioNetHdr {
            gso_type: VirtioNetHdrGso::new().with_protocol(VirtioNetHdrGsoProtocol::TCPV4),
            hdr_len: 54,
            gso_size: 1460,
            ..Default::default()
        };
        assert_vnet_hdr_contract(&hdr, 1514);
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
                RxOffloadSupport {
                    checksum: true,
                    ..Default::default()
                }
            )
            .unwrap_err(),
            InvalidRxMetadata::UnsupportedGsoOffload(RxGsoProtocol::TcpV4)
        );
    }

    #[test]
    fn rx_metadata_rejects_unnegotiated_checksum() {
        let hdr = VirtioNetHdr {
            flags: VirtioNetHdrFlags::new().with_needs_csum(true),
            csum_start: 34,
            csum_offset: 16,
            ..Default::default()
        };
        assert_eq!(
            parse_vnet_hdr(&hdr, Default::default()).unwrap_err(),
            InvalidRxMetadata::UnsupportedChecksumOffload
        );
    }

    #[test]
    fn stale_offload_recovery_retains_oldest_buffer_until_current_packet() {
        let rx = RxId(7);
        let mut inner = Inner {
            rx_free: VecDeque::from([rx]),
            rx_ready: VecDeque::new(),
        };
        let mut recovery = StaleOffloadRecovery::after_reconfiguration(None, all_rx_offloads());
        assert!(!recovery.permits(&InvalidRxMetadata::UnsupportedChecksumOffload));

        recovery =
            StaleOffloadRecovery::after_reconfiguration(Some(all_rx_offloads()), all_rx_offloads());
        assert!(!recovery.permits(&InvalidRxMetadata::UnsupportedChecksumOffload));
        assert!(!recovery.permits(&InvalidRxMetadata::UnsupportedGsoOffload(
            RxGsoProtocol::TcpV4
        )));

        recovery = StaleOffloadRecovery::after_reconfiguration(
            Some(all_rx_offloads()),
            RxOffloadSupport {
                checksum: true,
                tcpv4_gso: false,
                tcpv6_gso: true,
            },
        );
        assert!(recovery.permits(&InvalidRxMetadata::UnsupportedGsoOffload(
            RxGsoProtocol::TcpV4
        )));
        assert!(!recovery.permits(&InvalidRxMetadata::UnsupportedGsoOffload(
            RxGsoProtocol::TcpV6
        )));
        assert!(!recovery.permits(&InvalidRxMetadata::UnsupportedChecksumOffload));
        assert!(!recovery.permits(&InvalidRxMetadata::UnexpectedEcn));

        let stale_hdr = VirtioNetHdr {
            flags: VirtioNetHdrFlags::new().with_needs_csum(true),
            gso_type: VirtioNetHdrGso::new().with_protocol(VirtioNetHdrGsoProtocol::TCPV4),
            hdr_len: 54,
            gso_size: 1460,
            csum_start: 34,
            csum_offset: 16,
            ..Default::default()
        };
        assert!(
            parse_vnet_hdr_for_queue(
                &stale_hdr,
                RxOffloadSupport {
                    checksum: true,
                    tcpv4_gso: false,
                    tcpv6_gso: true,
                },
                &recovery,
            )
            .is_none()
        );
        assert_eq!(inner.rx_free.front().map(|id| id.0), Some(rx.0));
        assert!(inner.rx_ready.is_empty());

        let mut read_since_pending = true;
        recovery.queue_pending(&mut read_since_pending);
        assert!(!read_since_pending);
        assert!(
            parse_vnet_hdr_for_queue(
                &VirtioNetHdr::default(),
                RxOffloadSupport {
                    checksum: true,
                    tcpv4_gso: false,
                    tcpv6_gso: true,
                },
                &recovery,
            )
            .is_some()
        );
        inner.complete_rx(rx);
        assert!(inner.rx_free.is_empty());
        assert_eq!(inner.rx_ready.front().map(|id| id.0), Some(rx.0));

        recovery = StaleOffloadRecovery::after_reconfiguration(
            Some(all_rx_offloads()),
            RxOffloadSupport {
                checksum: false,
                tcpv4_gso: true,
                tcpv6_gso: true,
            },
        );
        assert!(recovery.permits(&InvalidRxMetadata::UnsupportedChecksumOffload));
    }

    #[test]
    fn initial_pending_does_not_close_inherited_recovery() {
        let mut recovery = StaleOffloadRecovery::all();
        let mut read_since_pending = false;

        recovery.queue_pending(&mut read_since_pending);
        assert!(recovery.permits(&InvalidRxMetadata::UnsupportedChecksumOffload));
        assert!(recovery.permits(&InvalidRxMetadata::UnsupportedGsoOffload(
            RxGsoProtocol::TcpV4
        )));
        assert!(recovery.permits(&InvalidRxMetadata::UnexpectedEcn));
        assert!(recovery.permits(&InvalidRxMetadata::UnsupportedGsoProtocol));

        read_since_pending = true;
        recovery.queue_pending(&mut read_since_pending);
        assert!(!read_since_pending);
        assert!(!recovery.permits(&InvalidRxMetadata::UnsupportedChecksumOffload));
        assert!(!recovery.permits(&InvalidRxMetadata::UnsupportedGsoOffload(
            RxGsoProtocol::TcpV4
        )));
        assert!(!recovery.permits(&InvalidRxMetadata::UnexpectedEcn));
        assert!(!recovery.permits(&InvalidRxMetadata::UnsupportedGsoProtocol));
    }

    #[test]
    fn inherited_recovery_drops_ecn_and_udp_gso() {
        let recovery = StaleOffloadRecovery::all();
        let support = all_rx_offloads();
        let ecn = VirtioNetHdr {
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
        let udp_gso = VirtioNetHdr {
            flags: VirtioNetHdrFlags::new().with_needs_csum(true),
            gso_type: VirtioNetHdrGso::new().with_protocol(VirtioNetHdrGsoProtocol::UDP_L4),
            hdr_len: 42,
            gso_size: 1452,
            csum_start: 34,
            csum_offset: 6,
            ..Default::default()
        };

        assert!(parse_vnet_hdr_for_queue(&ecn, support, &recovery).is_none());
        assert!(parse_vnet_hdr_for_queue(&udp_gso, support, &recovery).is_none());
    }

    #[test]
    fn unfinished_recovery_survives_queue_recreation() {
        let all = all_rx_offloads();
        let none = RxOffloadSupport::default();
        let checksum_only = RxOffloadSupport {
            checksum: true,
            ..Default::default()
        };

        let mut recovery_slot = StaleOffloadRecovery::default();
        recovery_slot.merge(StaleOffloadRecovery::after_reconfiguration(Some(all), none));
        let first_queue_recovery = std::mem::take(&mut recovery_slot);

        // Simulate dropping the first queue before it reaches EAGAIN.
        recovery_slot.merge(first_queue_recovery);
        recovery_slot.merge(StaleOffloadRecovery::after_reconfiguration(
            Some(none),
            checksum_only,
        ));
        let second_queue_recovery = std::mem::take(&mut recovery_slot);

        assert!(
            second_queue_recovery.permits(&InvalidRxMetadata::UnsupportedGsoOffload(
                RxGsoProtocol::TcpV4
            ))
        );
        assert!(
            second_queue_recovery.permits(&InvalidRxMetadata::UnsupportedGsoOffload(
                RxGsoProtocol::TcpV6
            ))
        );
    }

    #[test]
    #[should_panic(expected = "TAP returned invalid vnet metadata")]
    fn steady_state_offload_mismatch_fails_fast() {
        let hdr = VirtioNetHdr {
            flags: VirtioNetHdrFlags::new().with_needs_csum(true),
            csum_start: 34,
            csum_offset: 16,
            ..Default::default()
        };
        parse_vnet_hdr_for_queue(&hdr, Default::default(), &StaleOffloadRecovery::default());
    }

    #[test]
    fn rx_metadata_rejects_ecn_before_unnegotiated_gso() {
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
            parse_vnet_hdr(&hdr, Default::default()).unwrap_err(),
            InvalidRxMetadata::UnexpectedEcn
        );
    }

    #[test]
    fn rx_metadata_rejects_unsupported_udp_gso() {
        let hdr = VirtioNetHdr {
            gso_type: VirtioNetHdrGso::new().with_protocol(VirtioNetHdrGsoProtocol::UDP),
            ..Default::default()
        };
        assert_eq!(
            parse_vnet_hdr(&hdr, all_rx_offloads()).unwrap_err(),
            InvalidRxMetadata::UnsupportedGsoProtocol
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
    fn gso_pseudo_header_seed_includes_length() {
        // 14 eth + 20 IPv4 (TCP) + 20 TCP + 100 payload; the seed must be rewritten to
        // the pseudo-header that includes the full L4 length (header plus payload).
        let mut pkt = vec![0u8; 14 + 20 + 20 + 100];
        pkt[14] = 0x45; // IPv4, ihl = 5
        pkt[14 + 9] = 6; // protocol = TCP
        pkt[14 + 12..14 + 16].copy_from_slice(&[192, 168, 168, 156]); // src
        pkt[14 + 16..14 + 20].copy_from_slice(&[192, 168, 168, 17]); // dst
        fixup_gso_pseudo_header(&mut pkt, 14);
        let seed = u16::from_be_bytes([pkt[50], pkt[51]]); // TCP checksum field
        let l4_len: u32 = 20 + 100;
        let mut want: u32 = 0xc0a8 + 0xa89c + 0xc0a8 + 0xa811 + 6 + l4_len;
        while want >> 16 != 0 {
            want = (want & 0xffff) + (want >> 16);
        }
        assert_eq!(seed, want as u16);
    }

    #[test]
    fn gso_pseudo_header_seed_includes_length_udp() {
        // 14 eth + 20 IPv4 (UDP) + 8 UDP + 100 payload; the UDP checksum field sits at
        // a different offset (6) and the L4 length covers the 8-byte UDP header.
        let mut pkt = vec![0u8; 14 + 20 + 8 + 100];
        pkt[14] = 0x45; // IPv4, ihl = 5
        pkt[14 + 9] = 17; // protocol = UDP
        pkt[14 + 12..14 + 16].copy_from_slice(&[192, 168, 168, 156]); // src
        pkt[14 + 16..14 + 20].copy_from_slice(&[192, 168, 168, 17]); // dst
        fixup_gso_pseudo_header(&mut pkt, 14);
        let seed = u16::from_be_bytes([pkt[40], pkt[41]]); // UDP checksum field (l4 + 6)
        let l4_len: u32 = 8 + 100;
        let mut want: u32 = 0xc0a8 + 0xa89c + 0xc0a8 + 0xa811 + 17 + l4_len;
        while want >> 16 != 0 {
            want = (want & 0xffff) + (want >> 16);
        }
        assert_eq!(seed, want as u16);
    }

    // Sum the 16-byte IPv6 src + dst (bytes 8..40 of the IPv6 header) plus protocol
    // and L4 length, folded to 16 bits, the way fixup_gso_pseudo_header does.
    fn ipv6_pseudo_seed(pkt: &[u8], l2: usize, proto: u32, l4_len: u32) -> u16 {
        let mut s: u32 = proto + l4_len;
        for c in pkt[l2 + 8..l2 + 40].chunks_exact(2) {
            s += u16::from_be_bytes([c[0], c[1]]) as u32;
        }
        while s >> 16 != 0 {
            s = (s & 0xffff) + (s >> 16);
        }
        s as u16
    }

    #[test]
    fn gso_pseudo_header_seed_ipv6_tcp() {
        // 14 eth + 40 IPv6 (TCP) + 20 TCP + 100 payload.
        let mut pkt = vec![0u8; 14 + 40 + 20 + 100];
        pkt[14] = 0x60; // IPv6
        pkt[14 + 6] = 6; // next header = TCP
        pkt[14 + 8..14 + 24]
            .copy_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        pkt[14 + 24..14 + 40]
            .copy_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        fixup_gso_pseudo_header(&mut pkt, 14);
        let l4_off = 14 + 40;
        let seed = u16::from_be_bytes([pkt[l4_off + 16], pkt[l4_off + 17]]); // TCP cksum field
        assert_eq!(seed, ipv6_pseudo_seed(&pkt, 14, 6, 20 + 100));
    }

    #[test]
    fn gso_pseudo_header_seed_ipv6_udp_with_extension_header() {
        // 14 eth + 40 IPv6 + 8 hop-by-hop ext header + 8 UDP + 100 payload. The base
        // header's Next Header points at the extension header, which in turn points at
        // UDP, so the fixup must walk past it to find the L4 checksum field.
        let mut pkt = vec![0u8; 14 + 40 + 8 + 8 + 100];
        pkt[14] = 0x60; // IPv6
        pkt[14 + 6] = 0; // next header = hop-by-hop options
        pkt[14 + 8..14 + 24]
            .copy_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        pkt[14 + 24..14 + 40]
            .copy_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let ext = 14 + 40;
        pkt[ext] = 17; // ext header next-header = UDP
        pkt[ext + 1] = 0; // Hdr Ext Len 0 -> 8 bytes total
        fixup_gso_pseudo_header(&mut pkt, 14);
        let l4_off = 14 + 40 + 8; // UDP starts after the 8-byte extension header
        let seed = u16::from_be_bytes([pkt[l4_off + 6], pkt[l4_off + 7]]); // UDP cksum field
        assert_eq!(seed, ipv6_pseudo_seed(&pkt, 14, 17, 8 + 100));
    }

    #[test]
    fn gso_pseudo_header_leaves_ipv4_fragments_unchanged() {
        // MF set (a first fragment), then a non-zero offset (a later fragment).
        for frag in [0x2000u16, 0x00b9] {
            let mut pkt = vec![0u8; 14 + 20 + 20 + 100];
            pkt[14] = 0x45; // IPv4, ihl = 5
            pkt[14 + 6..14 + 8].copy_from_slice(&frag.to_be_bytes());
            pkt[14 + 9] = 6; // protocol = TCP
            pkt[14 + 12..14 + 16].copy_from_slice(&[192, 168, 168, 156]); // src
            pkt[14 + 16..14 + 20].copy_from_slice(&[192, 168, 168, 17]); // dst
            let orig = pkt.clone();
            fixup_gso_pseudo_header(&mut pkt, 14);
            assert_eq!(pkt, orig, "fragment field {frag:#06x}");
        }
    }

    #[test]
    fn gso_pseudo_header_leaves_ipv6_fragments_unchanged() {
        // 14 eth + 40 IPv6 + 8 Fragment header + 8 UDP + 100 payload.
        let mut pkt = vec![0u8; 14 + 40 + 8 + 8 + 100];
        pkt[14] = 0x60; // IPv6
        pkt[14 + 6] = 44; // next header = Fragment
        pkt[14 + 8..14 + 24]
            .copy_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        pkt[14 + 24..14 + 40]
            .copy_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        pkt[14 + 40] = 17; // Fragment header next-header = UDP
        let orig = pkt.clone();
        fixup_gso_pseudo_header(&mut pkt, 14);
        assert_eq!(pkt, orig);
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
        // datagram length so the host's IP input validation accepts the frame.
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
