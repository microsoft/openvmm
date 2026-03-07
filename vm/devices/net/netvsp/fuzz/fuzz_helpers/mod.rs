// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Not every fuzz binary uses every helper, so suppress dead-code warnings
// for the entire module.
#![allow(dead_code)]

//! Shared helpers for netvsp fuzz targets.
//!
//! ## Error types
//! [`RingFullError`]
//! ## Constants & page layout
//! [`PageLayout`], [`DATA_PAGES`]
//! ## Low-level VMBus I/O
//! [`write_packet`]
//! ## NVSP helpers
//! [`nvsp_payload`], [`send_inband_nvsp`],
//! [`send_completion_packet`], [`send_tx_rndis_completion`],
//! [`VF_ASSOCIATION_TRANSACTION_ID`],
//! [`SWITCH_DATA_PATH_TRANSACTION_ID`]
//! ## RNDIS helpers
//! [`build_rndis_message`], [`build_rndis_oid_query`],
//! [`build_rndis_oid_set`], [`build_rss_oid_set`],
//! [`build_rndis_config_parameter`],
//! [`KNOWN_CONFIG_PARAM_NAMES`],
//! [`send_rndis_gpadirect`],
//! [`send_rndis_control`], [`send_rndis_via_direct_path`],
//! [`send_rndis_via_send_buffer`],
//! [`rndis_set_packet_filter`]
//! ## NVSP protocol negotiation
//! [`negotiate_to_ready`],
//! [`negotiate_to_ready_with_capabilities`],
//! [`negotiate_to_ready_full`], [`pick_version_pair`],
//! [`rndis_initialize`]
//! ## Structured fuzz types
//! [`StructuredRndisMessage`],
//! [`StructuredRndisPacketMessage`],
//! [`serialize_structured_rndis_packet_message`],
//! [`build_concatenated_rndis_messages`]
//! ## Structured PPI types
//! [`StructuredPpiEntry`],
//! [`serialize_ppi_chain`], [`build_lso_ppi_entry`],
//! [`build_checksum_ppi_entry`]
//! ## Send path helpers
//! [`build_structured_rndis_packet`],
//! [`page_boundary_frame_size`]
//! ## Arbitrary generators
//! [`arbitrary_outgoing_packet_type`],
//! [`arbitrary_send_receive_buffer_message`],
//! [`arbitrary_send_send_buffer_message`],
//! [`arbitrary_valid_ethernet_frame`]
//! ## Guest OS identity
//! [`FuzzGuestOsId`]
//! ## Fuzz loop boilerplate
//! [`FuzzNicSetup`], [`yield_to_executor`],
//! [`try_read_one_completion`], [`drain_queue`],
//! [`drain_queue_async`], [`run_fuzz_loop`],
//! [`run_fuzz_loop_with_config`]

use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use guestmem::GuestMemory;
use guestmem::MemoryRead;
use guestmem::ranges::PagedRange;
use hvdef::hypercall::HvGuestOsId;
use netvsp::protocol;
use netvsp::rndisprot;
use std::future::poll_fn;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use vmbus_async::queue::IncomingPacket;
use vmbus_async::queue::OutgoingPacket;
use vmbus_async::queue::Queue;
use vmbus_async::queue::TryWriteError;
use vmbus_channel::gpadl::GpadlId;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_ring::OutgoingPacketType;
use vmbus_ring::PAGE_SIZE;
use xtask_fuzz::fuzz_eprintln;
use zerocopy::IntoBytes;

// ---------------------------------------------------------------------------
// Child modules
// ---------------------------------------------------------------------------
pub mod endpoint;
pub mod nic_setup;
pub mod vf;
mod vmbus;

// ===========================================================================
// Error types
// ===========================================================================

/// The outgoing (fuzz→worker) ring buffer is full.
///
/// This is a non-fatal condition: the NIC worker is alive but applying
/// backpressure because it cannot send completions back to the guest
/// (the guest hasn't drained the host→guest ring). Treated as a clean
/// stop signal in the fuzz loop, identical to exhausted `Unstructured`
/// data.
#[derive(Debug)]
pub struct RingFullError;

impl std::fmt::Display for RingFullError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("outgoing ring full (backpressure)")
    }
}

impl std::error::Error for RingFullError {}

// ===========================================================================
// Constants & page layout
// ===========================================================================

/// Number of pages used for the VMBus ring buffer.
const RING_PAGES: usize = 4;
/// Number of pages used for the receive buffer GPADL.
const RECV_BUF_PAGES: usize = 9;
/// Number of extra pages for writing fuzzed RNDIS data via GpaDirect.
pub const DATA_PAGES: usize = 4;
/// Section size used when writing RNDIS data into the send buffer GPADL.
const SEND_BUFFER_SECTION_SIZE_BYTES: usize = 6144;

/// Describes the guest memory page layout for a fuzz target.
///
/// The layout is: ring pages | receive buffer | send buffer | data pages.
/// `data_pages` may be zero for fuzz targets that don't send GpaDirect data.
pub struct PageLayout {
    /// Number of pages for the send buffer GPADL.
    pub send_buf_pages: usize,
    /// Number of extra pages for fuzzed RNDIS data (0 if unused).
    pub data_pages: usize,
}

impl PageLayout {
    /// Total guest memory pages needed.
    pub const fn total_pages(&self) -> usize {
        RING_PAGES + RECV_BUF_PAGES + self.send_buf_pages + self.data_pages
    }

    /// Page offset where fuzzed RNDIS data starts (after ring + recv + send).
    pub const fn data_page_start(&self) -> usize {
        RING_PAGES + RECV_BUF_PAGES + self.send_buf_pages
    }
}

// ===========================================================================
// Low-level VMBus I/O
// ===========================================================================

/// Write one outgoing packet and increment transaction ID.
///
/// Uses [`try_write`] to avoid blocking when the ring is full (which
/// happens when the NIC worker has terminated).  With deterministic
/// draining the ring should always have space when the worker is alive.
pub async fn write_packet(
    queue: &mut Queue<GpadlRingMem>,
    tid: &mut u64,
    packet_type: OutgoingPacketType<'_>,
    payload: &[&[u8]],
) -> anyhow::Result<()> {
    let (_, mut writer) = queue.split();
    match writer.try_write(&OutgoingPacket {
        transaction_id: *tid,
        packet_type,
        payload,
    }) {
        Ok(()) => {
            *tid += 1;
            Ok(())
        }
        Err(TryWriteError::Full(_)) => {
            anyhow::bail!(RingFullError);
        }
        Err(TryWriteError::Queue(err)) => Err(err.into()),
    }
}

// ===========================================================================
// NVSP helpers
// ===========================================================================

/// Build an NVSP message payload (header + data), padded to 8-byte alignment.
pub fn nvsp_payload(message_type: u32, data: &[u8]) -> Vec<u8> {
    let header = protocol::MessageHeader { message_type };
    let mut buf = Vec::with_capacity(4 + data.len());
    buf.extend_from_slice(header.as_bytes());
    buf.extend_from_slice(data);
    while buf.len() % 8 != 0 {
        buf.push(0);
    }
    buf
}

/// Build an NVSP `Message1SendRndisPacket` payload with header.
///
/// Internal helper — callers outside this module should use
/// [`send_rndis_gpadirect`] or [`send_rndis_via_send_buffer`] instead.
fn nvsp_rndis_payload(channel_type: u32, section_index: u32, section_size: u32) -> Vec<u8> {
    nvsp_payload(
        protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET,
        protocol::Message1SendRndisPacket {
            channel_type,
            send_buffer_section_index: section_index,
            send_buffer_section_size: section_size,
        }
        .as_bytes(),
    )
}

/// Send one NVSP in-band message with configurable completion behavior.
pub async fn send_inband_nvsp(
    queue: &mut Queue<GpadlRingMem>,
    tid: &mut u64,
    message_type: u32,
    data: &[u8],
    with_completion: bool,
) -> anyhow::Result<()> {
    let payload = nvsp_payload(message_type, data);
    let packet_type = if with_completion {
        OutgoingPacketType::InBandWithCompletion
    } else {
        OutgoingPacketType::InBandNoCompletion
    };
    write_packet(queue, tid, packet_type, &[&payload]).await
}

/// Send a completion packet with a caller-supplied transaction id and payload.
pub async fn send_completion_packet(
    queue: &mut Queue<GpadlRingMem>,
    transaction_id: u64,
    payload: &[&[u8]],
) -> anyhow::Result<()> {
    let (_, mut writer) = queue.split();
    match writer.try_write(&OutgoingPacket {
        transaction_id,
        packet_type: OutgoingPacketType::Completion,
        payload,
    }) {
        Ok(()) => Ok(()),
        Err(TryWriteError::Full(_)) => {
            anyhow::bail!(RingFullError);
        }
        Err(TryWriteError::Queue(err)) => Err(err.into()),
    }
}

/// Transaction ID for VF association completion packets.
pub const VF_ASSOCIATION_TRANSACTION_ID: u64 = 0x8000000000000000;
/// Transaction ID for switch data-path completion packets.
pub const SWITCH_DATA_PATH_TRANSACTION_ID: u64 = 0x8000000000000001;

/// Send a TX RNDIS completion packet with the given transaction ID.
///
/// This wraps a `Completion` packet containing an
/// NVSP `MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE` payload.
pub async fn send_tx_rndis_completion(
    queue: &mut Queue<GpadlRingMem>,
    transaction_id: u64,
    completion: &protocol::Message1SendRndisPacketComplete,
) -> anyhow::Result<()> {
    let payload = nvsp_payload(
        protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE,
        completion.as_bytes(),
    );
    let (_, mut writer) = queue.split();
    match writer.try_write(&OutgoingPacket {
        transaction_id,
        packet_type: OutgoingPacketType::Completion,
        payload: &[&payload],
    }) {
        Ok(()) => Ok(()),
        Err(TryWriteError::Full(_)) => {
            anyhow::bail!(RingFullError);
        }
        Err(TryWriteError::Queue(err)) => Err(err.into()),
    }
}

// ===========================================================================
// RNDIS helpers
// ===========================================================================

/// Build a complete RNDIS message with header + body.
pub fn build_rndis_message(message_type: u32, body: &[u8]) -> Vec<u8> {
    let header = rndisprot::MessageHeader {
        message_type,
        message_length: (size_of::<rndisprot::MessageHeader>() + body.len()) as u32,
    };
    let mut buf = Vec::with_capacity(header.message_length as usize);
    buf.extend_from_slice(header.as_bytes());
    buf.extend_from_slice(body);
    buf
}

/// Build an RNDIS OID query message (MESSAGE_TYPE_QUERY_MSG).
pub fn build_rndis_oid_query(oid: rndisprot::Oid, extra_data: &[u8]) -> Vec<u8> {
    let request = rndisprot::QueryRequest {
        request_id: 1,
        oid,
        information_buffer_length: extra_data.len() as u32,
        information_buffer_offset: if extra_data.is_empty() {
            0
        } else {
            size_of::<rndisprot::QueryRequest>() as u32
        },
        device_vc_handle: 0,
    };
    let mut body = Vec::new();
    body.extend_from_slice(request.as_bytes());
    body.extend_from_slice(extra_data);
    build_rndis_message(rndisprot::MESSAGE_TYPE_QUERY_MSG, &body)
}

/// Build an RNDIS OID set message (MESSAGE_TYPE_SET_MSG).
pub fn build_rndis_oid_set(oid: rndisprot::Oid, payload: &[u8]) -> Vec<u8> {
    let request = rndisprot::SetRequest {
        request_id: 1,
        oid,
        information_buffer_length: payload.len() as u32,
        information_buffer_offset: size_of::<rndisprot::SetRequest>() as u32,
        device_vc_handle: 0,
    };
    let mut body = Vec::new();
    body.extend_from_slice(request.as_bytes());
    body.extend_from_slice(payload);
    build_rndis_message(rndisprot::MESSAGE_TYPE_SET_MSG, &body)
}

/// Build a well-formed RSS OID SET message (`OID_GEN_RECEIVE_SCALE_PARAMETERS`)
/// with a valid hash key and indirection table. Use `max_queues` to bound the
/// indirection table entries so they pass the `entry < max_queues` check.
pub fn build_rss_oid_set(
    hash_information: u32,
    indirection_entries: &[u32],
    max_queues: u32,
    flags: u16,
) -> Vec<u8> {
    let key = [0xDAu8; 40]; // 40-byte Toeplitz hash key
    let itable_len = indirection_entries.len();
    let itable_byte_len = itable_len * 4;

    // Build NdisReceiveScaleParameters with correct offsets.
    let params_size = size_of::<rndisprot::NdisReceiveScaleParameters>();
    let itable_offset = params_size as u32;
    let key_offset = (params_size + itable_byte_len) as u32;

    let header = rndisprot::NdisObjectHeader {
        object_type: rndisprot::NdisObjectType::RSS_PARAMETERS,
        revision: 2,
        size: rndisprot::NDIS_SIZEOF_RECEIVE_SCALE_PARAMETERS_REVISION_2 as u16,
    };

    let params = rndisprot::NdisReceiveScaleParameters {
        header,
        flags,
        base_cpu_number: 0,
        hash_information,
        indirection_table_size: itable_byte_len as u16,
        pad0: 0,
        indirection_table_offset: itable_offset,
        hash_secret_key_size: 40,
        pad1: 0,
        hash_secret_key_offset: key_offset,
        processor_masks_offset: 0,
        number_of_processor_masks: 0,
        processor_masks_entry_size: 0,
        default_processor_number: 0,
    };

    let mut payload = Vec::new();
    payload.extend_from_slice(params.as_bytes());
    for &entry in indirection_entries {
        // Clamp entries to valid range.
        payload.extend_from_slice((entry % max_queues.max(1)).as_bytes());
    }
    payload.extend_from_slice(&key);

    build_rndis_oid_set(rndisprot::Oid::OID_GEN_RECEIVE_SCALE_PARAMETERS, &payload)
}

/// Known RNDIS config parameter names that exercise the named match arms in
/// `oid_set_rndis_config_parameter`.
pub const KNOWN_CONFIG_PARAM_NAMES: &[&str] = &[
    "*IPChecksumOffloadIPv4",
    "*LsoV2IPv4",
    "*LsoV2IPv6",
    "*TCPChecksumOffloadIPv4",
    "*TCPChecksumOffloadIPv6",
    "*UDPChecksumOffloadIPv4",
    "*UDPChecksumOffloadIPv6",
];

/// Build a well-formed `OID_GEN_RNDIS_CONFIG_PARAMETER` SET message with a
/// properly UTF-16LE–encoded parameter name and value.
///
/// `param_name` is the ASCII parameter name (e.g. `"*IPChecksumOffloadIPv4"`).
/// `param_type` is the NDIS parameter type (STRING, INTEGER, etc.).
/// `value_bytes` is the raw value payload — for STRING, the caller should
/// provide UTF-16LE encoded bytes; for INTEGER, 4 bytes of a u32.
pub fn build_rndis_config_parameter(
    param_name: &str,
    param_type: rndisprot::NdisParameterType,
    value_bytes: &[u8],
) -> Vec<u8> {
    // Encode the name as UTF-16LE.
    let name_u16: Vec<u16> = param_name.encode_utf16().collect();
    let name_bytes_len = name_u16.len() * 2;

    let info_size = size_of::<rndisprot::RndisConfigParameterInfo>();
    let name_offset = info_size as u32;
    let value_offset = (info_size + name_bytes_len) as u32;

    let info = rndisprot::RndisConfigParameterInfo {
        name_offset,
        name_length: name_bytes_len as u32,
        parameter_type: param_type,
        value_offset,
        value_length: value_bytes.len() as u32,
    };

    let mut payload = Vec::new();
    payload.extend_from_slice(info.as_bytes());
    // Name as UTF-16LE bytes.
    for &code_unit in &name_u16 {
        payload.extend_from_slice(&code_unit.to_le_bytes());
    }
    // Value bytes.
    payload.extend_from_slice(value_bytes);

    build_rndis_oid_set(rndisprot::Oid::OID_GEN_RNDIS_CONFIG_PARAMETER, &payload)
}

/// Write raw bytes into guest memory at a given page-aligned offset.
/// Returns the number of bytes written, or None when no bytes can be written.
fn write_to_guest(
    mem: &GuestMemory,
    data: &[u8],
    page_start: usize,
    max_pages: usize,
) -> Option<usize> {
    let max_bytes = max_pages * PAGE_SIZE;
    let len = data.len().min(max_bytes);
    if len == 0 {
        return None;
    }
    let base_addr = (page_start * PAGE_SIZE) as u64;
    mem.write_at(base_addr, &data[..len]).ok()?;
    Some(len)
}

/// Send a GpaDirect packet referencing data previously written to guest
/// memory at `page_start`.
async fn send_gpadirect(
    queue: &mut Queue<GpadlRingMem>,
    page_start: usize,
    byte_len: usize,
    payload: &[u8],
    tid: &mut u64,
) -> Result<(), anyhow::Error> {
    let page_count = byte_len.div_ceil(PAGE_SIZE);
    let pages: Vec<u64> = (page_start..page_start + page_count)
        .map(|p| p as u64)
        .collect();
    let gpa_range = PagedRange::new(0, byte_len, pages.as_slice()).unwrap();
    write_packet(
        queue,
        tid,
        OutgoingPacketType::GpaDirect(&[gpa_range]),
        &[payload],
    )
    .await
}

/// Write RNDIS data to guest memory and send it via GpaDirect with an
/// NVSP `Message1SendRndisPacket` wrapper.
pub async fn send_rndis_gpadirect(
    queue: &mut Queue<GpadlRingMem>,
    mem: &GuestMemory,
    rndis_bytes: &[u8],
    channel_type: u32,
    data_page_start: usize,
    data_page_count: usize,
    tid: &mut u64,
) -> Result<(), anyhow::Error> {
    if let Some(byte_len) = write_to_guest(mem, rndis_bytes, data_page_start, data_page_count) {
        let nvsp = nvsp_rndis_payload(channel_type, 0xffffffff, 0);
        send_gpadirect(queue, data_page_start, byte_len, &nvsp, tid).await?;
    }
    Ok(())
}

/// Send an RNDIS control message via GpaDirect using the control channel.
pub async fn send_rndis_control(
    queue: &mut Queue<GpadlRingMem>,
    mem: &GuestMemory,
    rndis_bytes: &[u8],
    layout: &PageLayout,
    tid: &mut u64,
) -> Result<(), anyhow::Error> {
    send_rndis_gpadirect(
        queue,
        mem,
        rndis_bytes,
        protocol::CONTROL_CHANNEL_TYPE,
        layout.data_page_start(),
        layout.data_pages,
        tid,
    )
    .await
}

/// Send RNDIS data via GpaDirect using a given layout's data page region.
///
/// This is a convenience wrapper around [`send_rndis_gpadirect`] that
/// unpacks `layout.data_page_start()` and `layout.data_pages`.  Use this
/// when you have a [`PageLayout`] and an arbitrary channel type.  For the
/// common case of sending on the control channel, prefer
/// [`send_rndis_control`].
pub async fn send_rndis_via_direct_path(
    queue: &mut Queue<GpadlRingMem>,
    mem: &GuestMemory,
    rndis_bytes: &[u8],
    channel_type: u32,
    layout: &PageLayout,
    tid: &mut u64,
) -> Result<(), anyhow::Error> {
    send_rndis_gpadirect(
        queue,
        mem,
        rndis_bytes,
        channel_type,
        layout.data_page_start(),
        layout.data_pages,
        tid,
    )
    .await
}

/// Write RNDIS data into the send buffer GPADL and send it via the send buffer
/// path. This consolidates the duplicated send-buffer logic from the synthetic
/// datapath and interop fuzz targets.
pub async fn send_rndis_via_send_buffer(
    queue: &mut Queue<GpadlRingMem>,
    mem: &GuestMemory,
    rndis_bytes: &[u8],
    nvsp_msg: &protocol::Message1SendRndisPacket,
    layout: &PageLayout,
    tid: &mut u64,
) -> Result<(), anyhow::Error> {
    let send_buf_page_start = RING_PAGES + RECV_BUF_PAGES;
    let send_buf_max = layout.send_buf_pages * PAGE_SIZE;
    let write_len = rndis_bytes.len().min(send_buf_max);
    if write_len > 0 {
        let base_addr = (send_buf_page_start * PAGE_SIZE) as u64;
        let offset = (nvsp_msg.send_buffer_section_index as usize)
            .wrapping_mul(SEND_BUFFER_SECTION_SIZE_BYTES)
            % send_buf_max;
        let _ = mem.write_at(base_addr + offset as u64, &rndis_bytes[..write_len]);
    }

    send_inband_nvsp(
        queue,
        tid,
        protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET,
        nvsp_msg.as_bytes(),
        true,
    )
    .await
}

/// Set `OID_GEN_CURRENT_PACKET_FILTER` to `NPROTO_PACKET_FILTER` via RNDIS
/// OID set so that RX packets are actually delivered instead of being
/// silently dropped in `process_endpoint_rx`.
pub async fn rndis_set_packet_filter(
    queue: &mut Queue<GpadlRingMem>,
    mem: &GuestMemory,
    layout: &PageLayout,
    tid: &mut u64,
) -> Result<(), anyhow::Error> {
    let filter_bytes = build_rndis_oid_set(
        rndisprot::Oid::OID_GEN_CURRENT_PACKET_FILTER,
        &rndisprot::NPROTO_PACKET_FILTER.to_le_bytes(),
    );
    send_rndis_gpadirect(
        queue,
        mem,
        &filter_bytes,
        protocol::CONTROL_CHANNEL_TYPE,
        layout.data_page_start(),
        layout.data_pages,
        tid,
    )
    .await?;
    drain_queue_async(queue).await;
    Ok(())
}

/// Compute a frame size that targets page-boundary edge cases in the RX
/// buffer `write_at()` / `write_header()` code paths.
///
/// The `variant` byte selects one of eight interesting sizes:
/// - 0–2: sizes around one `PAGE_SIZE` boundary
/// - 3–4: sizes around one page minus the 256-byte RX header
/// - 5–6: sizes around two pages minus the RX header
/// - 7+: pseudorandom spread across the full data-page range
pub fn page_boundary_frame_size(variant: u8) -> usize {
    const RX_HDR: usize = 256; // RX_HEADER_LEN from buffers.rs
    let size = match variant % 8 {
        0 => PAGE_SIZE - 1,
        1 => PAGE_SIZE,
        2 => PAGE_SIZE + 1,
        3 => PAGE_SIZE - RX_HDR,     // header fits perfectly in first page
        4 => PAGE_SIZE - RX_HDR + 1, // header + 1 byte crosses page
        5 => 2 * PAGE_SIZE - RX_HDR, // two full pages of data
        6 => 2 * PAGE_SIZE - RX_HDR + 1, // crosses into third page by 1 byte
        _ => (variant as usize * 127) % (DATA_PAGES * PAGE_SIZE), // spread across range
    };
    size.min(DATA_PAGES * PAGE_SIZE)
}

// ===========================================================================
// NVSP protocol negotiation
// ===========================================================================

/// Send an NVSP InBandWithCompletion message and read the completion,
/// validating that it is well-formed and successful.
async fn send_and_read_completion(
    queue: &mut Queue<GpadlRingMem>,
    tid: &mut u64,
    message_type: u32,
    data: &[u8],
) -> anyhow::Result<()> {
    send_inband_nvsp(queue, tid, message_type, data, true).await?;
    let (mut reader, _) = queue.split();
    let packet = reader.read().await?;
    match &*packet {
        IncomingPacket::Completion(completion) => {
            match message_type {
                protocol::MESSAGE_TYPE_INIT => {
                    let mut r = completion.reader();
                    let header: protocol::MessageHeader = r.read_plain()?;
                    anyhow::ensure!(
                        header.message_type == protocol::MESSAGE_TYPE_INIT_COMPLETE,
                        "unexpected init completion message type: {}",
                        header.message_type
                    );
                    let c: protocol::MessageInitComplete = r.read_plain()?;
                    anyhow::ensure!(
                        c.status == protocol::Status::SUCCESS,
                        "init completion status not SUCCESS: {:?}",
                        c.status
                    );
                }
                protocol::MESSAGE1_TYPE_SEND_RECEIVE_BUFFER => {
                    let mut r = completion.reader();
                    let header: protocol::MessageHeader = r.read_plain()?;
                    anyhow::ensure!(
                        header.message_type == protocol::MESSAGE1_TYPE_SEND_RECEIVE_BUFFER_COMPLETE,
                        "unexpected receive buffer completion message type: {}",
                        header.message_type
                    );
                    let c: protocol::Message1SendReceiveBufferComplete = r.read_plain()?;
                    anyhow::ensure!(
                        c.status == protocol::Status::SUCCESS,
                        "receive buffer completion status not SUCCESS: {:?}",
                        c.status
                    );
                }
                protocol::MESSAGE1_TYPE_SEND_SEND_BUFFER => {
                    let mut r = completion.reader();
                    let header: protocol::MessageHeader = r.read_plain()?;
                    anyhow::ensure!(
                        header.message_type == protocol::MESSAGE1_TYPE_SEND_SEND_BUFFER_COMPLETE,
                        "unexpected send buffer completion message type: {}",
                        header.message_type
                    );
                    let c: protocol::Message1SendSendBufferComplete = r.read_plain()?;
                    anyhow::ensure!(
                        c.status == protocol::Status::SUCCESS,
                        "send buffer completion status not SUCCESS: {:?}",
                        c.status
                    );
                }
                _ => {
                    // NDIS config and NDIS version completions have empty
                    // payloads — just verify we got a completion packet.
                }
            }
        }
        IncomingPacket::Data(_) => {
            anyhow::bail!("expected completion packet, got data packet");
        }
    }
    Ok(())
}

/// The set of NVSP protocol versions used by fuzz targets.
///
/// V1 is excluded because the post-init messages (NDIS config, receive/send
/// buffer, RNDIS, etc.) use V2+ message types.  Negotiating V1 causes the
/// device to reject those messages with `PacketError::UnknownType`, which
/// crashes the NIC worker and leaves the guest hanging.
const SUPPORTED_VERSIONS: &[protocol::Version] = &[
    protocol::Version::V2,
    protocol::Version::V4,
    protocol::Version::V5,
    protocol::Version::V6,
    protocol::Version::V61,
];

/// Pick a fuzzer-driven protocol version pair from the supported set.
///
/// Returns `(protocol_version, protocol_version2)` for `MessageInit`,
/// where `protocol_version` <= `protocol_version2`.
pub fn pick_version_pair(u: &mut Unstructured<'_>) -> arbitrary::Result<protocol::MessageInit> {
    let v1 = *u.choose(SUPPORTED_VERSIONS)?;
    let v2 = *u.choose(SUPPORTED_VERSIONS)?;
    let (lo, hi) = if (v1 as u32) <= (v2 as u32) {
        (v1, v2)
    } else {
        (v2, v1)
    };
    Ok(protocol::MessageInit {
        protocol_version: lo as u32,
        protocol_version2: hi as u32,
    })
}

/// Negotiate NVSP up to the Ready state with custom capabilities and
/// a caller-supplied version init message.
///
/// This is the most flexible negotiation helper. Use [`negotiate_to_ready`]
/// or [`negotiate_to_ready_with_capabilities`] for common cases.
pub async fn negotiate_to_ready_full(
    queue: &mut Queue<GpadlRingMem>,
    tid: &mut u64,
    recv_buf_gpadl: GpadlId,
    send_buf_gpadl: GpadlId,
    capabilities: protocol::NdisConfigCapabilities,
    version_init: protocol::MessageInit,
) -> anyhow::Result<()> {
    // Version init — use the caller-supplied version pair.
    send_and_read_completion(
        queue,
        tid,
        protocol::MESSAGE_TYPE_INIT,
        version_init.as_bytes(),
    )
    .await?;

    // NDIS config.
    let config = protocol::Message2SendNdisConfig {
        mtu: 1500,
        reserved: 0,
        capabilities,
    };
    send_and_read_completion(
        queue,
        tid,
        protocol::MESSAGE2_TYPE_SEND_NDIS_CONFIG,
        config.as_bytes(),
    )
    .await?;

    // NDIS version.
    let version = protocol::Message1SendNdisVersion {
        ndis_major_version: 6,
        ndis_minor_version: 30,
    };
    send_and_read_completion(
        queue,
        tid,
        protocol::MESSAGE1_TYPE_SEND_NDIS_VERSION,
        version.as_bytes(),
    )
    .await?;

    // Receive buffer.
    let msg = protocol::Message1SendReceiveBuffer {
        gpadl_handle: recv_buf_gpadl,
        id: 0,
        reserved: 0,
    };
    send_and_read_completion(
        queue,
        tid,
        protocol::MESSAGE1_TYPE_SEND_RECEIVE_BUFFER,
        msg.as_bytes(),
    )
    .await?;

    // Send buffer.
    let msg = protocol::Message1SendSendBuffer {
        gpadl_handle: send_buf_gpadl,
        id: 0,
        reserved: 0,
    };
    send_and_read_completion(
        queue,
        tid,
        protocol::MESSAGE1_TYPE_SEND_SEND_BUFFER,
        msg.as_bytes(),
    )
    .await?;

    // Yield to the executor so the coordinator task can process the
    // CoordinatorMessage::Restart sent by the Worker after initialization.
    // The coordinator needs multiple poll rounds to: receive the Restart,
    // stop the worker, transition it to Ready, call restart_queues (which
    // sets up QueueState with endpoint queues), and re-start the worker.
    // Without this yield, the single-threaded executor never polls the
    // coordinator, so the worker stays in WaitingForCoordinator and
    // main_loop is never entered.
    yield_to_executor(20).await;

    Ok(())
}

/// Negotiate NVSP up to the Ready state with custom NDIS capabilities.
///
/// Use this when the fuzz target needs specific NDIS capabilities (e.g.
/// `.with_sriov(true)` for VF testing).
pub async fn negotiate_to_ready_with_capabilities(
    queue: &mut Queue<GpadlRingMem>,
    tid: &mut u64,
    recv_buf_gpadl: GpadlId,
    send_buf_gpadl: GpadlId,
    capabilities: protocol::NdisConfigCapabilities,
) -> anyhow::Result<()> {
    negotiate_to_ready_full(
        queue,
        tid,
        recv_buf_gpadl,
        send_buf_gpadl,
        capabilities,
        protocol::MessageInit {
            protocol_version: protocol::Version::V5 as u32,
            protocol_version2: protocol::Version::V6 as u32,
        },
    )
    .await
}

/// Perform full protocol negotiation (version init, NDIS config, NDIS version,
/// receive buffer, send buffer) to reach the ready state.
pub async fn negotiate_to_ready(
    queue: &mut Queue<GpadlRingMem>,
    tid: &mut u64,
    recv_buf_gpadl: GpadlId,
    send_buf_gpadl: GpadlId,
) -> anyhow::Result<()> {
    negotiate_to_ready_full(
        queue,
        tid,
        recv_buf_gpadl,
        send_buf_gpadl,
        protocol::NdisConfigCapabilities::new(),
        protocol::MessageInit {
            protocol_version: protocol::Version::V5 as u32,
            protocol_version2: protocol::Version::V6 as u32,
        },
    )
    .await
}

/// Send RNDIS initialize to transition to RndisState::Operational.
pub async fn rndis_initialize(
    queue: &mut Queue<GpadlRingMem>,
    mem: &GuestMemory,
    data_page_start: usize,
    data_page_count: usize,
    tid: &mut u64,
) -> Result<(), anyhow::Error> {
    let init_request = rndisprot::InitializeRequest {
        request_id: 0,
        major_version: rndisprot::MAJOR_VERSION,
        minor_version: rndisprot::MINOR_VERSION,
        max_transfer_size: 0,
    };
    let rndis_bytes = build_rndis_message(
        rndisprot::MESSAGE_TYPE_INITIALIZE_MSG,
        init_request.as_bytes(),
    );
    send_rndis_gpadirect(
        queue,
        mem,
        &rndis_bytes,
        protocol::CONTROL_CHANNEL_TYPE,
        data_page_start,
        data_page_count,
        tid,
    )
    .await?;

    // Yield so the worker (now in main_loop after the coordinator restart
    // sequence) can process the RNDIS initialize from the ring buffer.
    yield_to_executor(10).await;

    // Read and discard the RNDIS initialize completion (and any other
    // interleaved messages) so the ring doesn't fill up.
    drain_queue_async(queue).await;
    Ok(())
}

// ===========================================================================
// Structured fuzz types
// ===========================================================================

/// Fuzzed RNDIS message with arbitrary header and payload.
#[derive(Arbitrary, Debug)]
pub struct StructuredRndisMessage {
    pub header: rndisprot::MessageHeader,
    pub payload: Vec<u8>,
}

/// Build one byte buffer containing multiple concatenated RNDIS messages.
pub fn build_concatenated_rndis_messages(messages: &[StructuredRndisMessage]) -> Vec<u8> {
    let mut rndis_buf = Vec::new();
    for message in messages {
        let mut header = message.header;
        header.message_length =
            (size_of::<rndisprot::MessageHeader>() + message.payload.len()) as u32;
        rndis_buf.extend_from_slice(header.as_bytes());
        rndis_buf.extend_from_slice(&message.payload);
    }
    rndis_buf
}

/// Fuzzed RNDIS packet message with structured header, packet, and tail.
#[derive(Arbitrary, Debug)]
pub struct StructuredRndisPacketMessage {
    pub header: rndisprot::MessageHeader,
    pub packet: rndisprot::Packet,
    pub tail_bytes: Vec<u8>,
}

/// Serialize a [`StructuredRndisPacketMessage`] into a byte vector,
/// fixing up `message_length` to match the actual serialized size.
pub fn serialize_structured_rndis_packet_message(
    rndis: &mut StructuredRndisPacketMessage,
) -> Vec<u8> {
    rndis.header.message_length = (size_of::<rndisprot::MessageHeader>()
        + size_of::<rndisprot::Packet>()
        + rndis.tail_bytes.len()) as u32;
    let mut rndis_bytes = Vec::with_capacity(rndis.header.message_length as usize);
    rndis_bytes.extend_from_slice(rndis.header.as_bytes());
    rndis_bytes.extend_from_slice(rndis.packet.as_bytes());
    rndis_bytes.extend_from_slice(&rndis.tail_bytes);
    rndis_bytes
}

// ===========================================================================
// Structured PPI (Per-Packet Info) types for offload fuzzing
// ===========================================================================

/// A single PPI entry with structured content. This enables the fuzzer to
/// produce well-formed PPI chains that actually exercise the checksum and LSO
/// parsing paths in the worker, rather than relying on random bytes.
#[derive(Arbitrary, Debug)]
pub enum StructuredPpiEntry {
    /// A `PPI_TCP_IP_CHECKSUM` (type 0) entry with a fuzzed
    /// `TxTcpIpChecksumInfo` value (packed as a `u32`).
    ChecksumInfo {
        /// Raw `TxTcpIpChecksumInfo` bits — flags for IPv4/IPv6, TCP/UDP
        /// checksum, IP header checksum, and tcp_header_offset.
        info: u32,
    },
    /// A `PPI_LSO` (type 2) entry with a fuzzed `TcpLsoInfo` value
    /// (packed as a `u32`).
    LsoInfo {
        /// Raw `TcpLsoInfo` bits — MSS (bits 0-19), tcp_header_offset
        /// (bits 20-29), IPv4/IPv6 flag (bit 31).
        info: u32,
    },
    /// An unknown PPI type with arbitrary payload. This exercises the
    /// `_ => {}` fallthrough in the PPI parsing loop.
    Unknown {
        /// PPI type value (should differ from 0 and 2 for true unknown).
        typ: u32,
        /// Arbitrary payload data.
        data: Vec<u8>,
    },
    /// A fully fuzzed PPI entry with arbitrary `PerPacketInfo` header fields.
    /// This exercises error paths: zero size, offset > size, undersized
    /// payloads, etc.
    Malformed {
        /// Fuzzed PPI header (via the `Arbitrary` derive on `PerPacketInfo`).
        header: rndisprot::PerPacketInfo,
        /// Arbitrary trailing data.
        data: Vec<u8>,
    },
}

/// Serialize a slice of [`StructuredPpiEntry`] values into a contiguous PPI
/// byte buffer suitable for passing to [`build_structured_rndis_packet`].
pub fn serialize_ppi_chain(entries: &[StructuredPpiEntry]) -> Vec<u8> {
    let ppi_header_size = size_of::<rndisprot::PerPacketInfo>() as u32; // 12
    let mut buf = Vec::new();
    for entry in entries {
        match entry {
            StructuredPpiEntry::ChecksumInfo { info } => {
                let header = rndisprot::PerPacketInfo {
                    size: ppi_header_size + size_of::<u32>() as u32, // 16
                    typ: rndisprot::PPI_TCP_IP_CHECKSUM,
                    per_packet_information_offset: ppi_header_size,
                };
                buf.extend_from_slice(header.as_bytes());
                buf.extend_from_slice(info.as_bytes());
            }
            StructuredPpiEntry::LsoInfo { info } => {
                let header = rndisprot::PerPacketInfo {
                    size: ppi_header_size + size_of::<u32>() as u32, // 16
                    typ: rndisprot::PPI_LSO,
                    per_packet_information_offset: ppi_header_size,
                };
                buf.extend_from_slice(header.as_bytes());
                buf.extend_from_slice(info.as_bytes());
            }
            StructuredPpiEntry::Unknown { typ, data } => {
                let header = rndisprot::PerPacketInfo {
                    size: ppi_header_size + data.len() as u32,
                    typ: *typ,
                    per_packet_information_offset: ppi_header_size,
                };
                buf.extend_from_slice(header.as_bytes());
                buf.extend_from_slice(data);
            }
            StructuredPpiEntry::Malformed { header, data } => {
                buf.extend_from_slice(header.as_bytes());
                buf.extend_from_slice(data);
            }
        }
    }
    buf
}

/// Build a single LSO PPI entry as raw bytes. Constructs `TcpLsoInfo` from
/// the MSS, tcp_header_offset, and IPv4/IPv6 flag, then wraps it in a
/// well-formed `PerPacketInfo` header.
pub fn build_lso_ppi_entry(mss: u32, tcp_header_offset: u16, is_ipv6: bool) -> Vec<u8> {
    let ppi_header_size = size_of::<rndisprot::PerPacketInfo>() as u32;
    // TcpLsoInfo layout: bits 0-19 = MSS, bits 20-29 = tcp_header_offset,
    // bit 31 = 1 for IPv6 (0 for IPv4).
    let lso_bits = (mss & 0xfffff)
        | (((tcp_header_offset as u32) & 0x3ff) << 20)
        | (if is_ipv6 { 1u32 << 31 } else { 0 });
    let header = rndisprot::PerPacketInfo {
        size: ppi_header_size + size_of::<u32>() as u32,
        typ: rndisprot::PPI_LSO,
        per_packet_information_offset: ppi_header_size,
    };
    let mut buf = Vec::with_capacity(header.size as usize);
    buf.extend_from_slice(header.as_bytes());
    buf.extend_from_slice(lso_bits.as_bytes());
    buf
}

/// Build a single checksum PPI entry as raw bytes. Wraps the raw
/// `TxTcpIpChecksumInfo` bits in a well-formed `PerPacketInfo` header.
pub fn build_checksum_ppi_entry(checksum_info: u32) -> Vec<u8> {
    let ppi_header_size = size_of::<rndisprot::PerPacketInfo>() as u32;
    let header = rndisprot::PerPacketInfo {
        size: ppi_header_size + size_of::<u32>() as u32,
        typ: rndisprot::PPI_TCP_IP_CHECKSUM,
        per_packet_information_offset: ppi_header_size,
    };
    let mut buf = Vec::with_capacity(header.size as usize);
    buf.extend_from_slice(header.as_bytes());
    buf.extend_from_slice(checksum_info.as_bytes());
    buf
}

// ===========================================================================
// Send path helpers
// ===========================================================================

/// Build a structured RNDIS packet (MessageHeader + Packet + PPI + frame data)
/// from component parts. This is used by both the synthetic datapath and
/// interop fuzz targets.
pub fn build_structured_rndis_packet(ppi_bytes: &[u8], frame_data: &[u8]) -> Vec<u8> {
    let ppi_len = ppi_bytes.len();
    let data_offset = (size_of::<rndisprot::Packet>() + ppi_len) as u32;
    let data_len = frame_data.len() as u32;
    let total_rndis_len = size_of::<rndisprot::MessageHeader>()
        + size_of::<rndisprot::Packet>()
        + ppi_len
        + frame_data.len();

    let rndis_header = rndisprot::MessageHeader {
        message_type: rndisprot::MESSAGE_TYPE_PACKET_MSG,
        message_length: total_rndis_len as u32,
    };
    let rndis_packet = rndisprot::Packet {
        data_offset,
        data_length: data_len,
        oob_data_offset: 0,
        oob_data_length: 0,
        num_oob_data_elements: 0,
        per_packet_info_offset: if ppi_len > 0 {
            size_of::<rndisprot::Packet>() as u32
        } else {
            0
        },
        per_packet_info_length: ppi_len as u32,
        vc_handle: 0,
        reserved: 0,
    };

    let mut buf = Vec::with_capacity(total_rndis_len);
    buf.extend_from_slice(rndis_header.as_bytes());
    buf.extend_from_slice(rndis_packet.as_bytes());
    buf.extend_from_slice(ppi_bytes);
    buf.extend_from_slice(frame_data);
    buf
}

// ===========================================================================
// Arbitrary generators
// ===========================================================================

/// Generate a random [`OutgoingPacketType`] for fuzz actions.
pub fn arbitrary_outgoing_packet_type(
    u: &mut Unstructured<'_>,
) -> arbitrary::Result<OutgoingPacketType<'static>> {
    Ok(match u.arbitrary::<u8>()? % 3 {
        0 => OutgoingPacketType::InBandNoCompletion,
        1 => OutgoingPacketType::InBandWithCompletion,
        _ => OutgoingPacketType::Completion,
    })
}

/// Generate a random [`protocol::Message1SendReceiveBuffer`] with an
/// arbitrary GPADL handle.
pub fn arbitrary_send_receive_buffer_message(
    u: &mut Unstructured<'_>,
) -> arbitrary::Result<protocol::Message1SendReceiveBuffer> {
    Ok(protocol::Message1SendReceiveBuffer {
        gpadl_handle: GpadlId(u.arbitrary::<u32>()?),
        id: u.arbitrary::<u16>()?,
        reserved: u.arbitrary::<u16>()?,
    })
}

/// Generate a random [`protocol::Message1SendSendBuffer`] with an
/// arbitrary GPADL handle.
pub fn arbitrary_send_send_buffer_message(
    u: &mut Unstructured<'_>,
) -> arbitrary::Result<protocol::Message1SendSendBuffer> {
    Ok(protocol::Message1SendSendBuffer {
        gpadl_handle: GpadlId(u.arbitrary::<u32>()?),
        id: u.arbitrary::<u16>()?,
        reserved: u.arbitrary::<u16>()?,
    })
}

/// Generate a mostly valid Ethernet II frame suitable for exercising
/// backend-driven RX parsing paths.
pub fn arbitrary_valid_ethernet_frame(u: &mut Unstructured<'_>) -> arbitrary::Result<Vec<u8>> {
    let mut frame = Vec::new();

    let dst: [u8; 6] = u.arbitrary()?;
    let src: [u8; 6] = u.arbitrary()?;
    frame.extend_from_slice(&dst);
    frame.extend_from_slice(&src);

    let ethertype = match u.int_in_range::<u8>(0..=4)? {
        0 => 0x0800u16,
        1 => 0x86DDu16,
        2 => 0x0806u16,
        3 => 0x8100u16,
        _ => 0x88A8u16,
    };
    frame.extend_from_slice(&ethertype.to_be_bytes());

    let payload_len = u.int_in_range::<usize>(46..=512)?;
    frame.extend_from_slice(u.bytes(payload_len)?);

    if frame.len() < 60 {
        frame.resize(60, 0);
    }

    Ok(frame)
}

// ===========================================================================
// FuzzGuestOsId
// ===========================================================================

/// Fuzz-selected guest OS identity for exercising all branches of
/// `can_use_ring_opt` and other guest-OS-dependent code paths.
///
/// The seven variants map 1-to-1 to the seven outcomes of `can_use_ring_opt`:
/// - `None`            → no OS ID reported → returns `false`
/// - `Proprietary`     → Windows / non-open-source → returns `true`
/// - `LinuxOld`        → Linux version < 199424 → returns `false`
/// - `LinuxNew`        → Linux version ≥ 199424 → returns `true`
/// - `FreeBsdOld`      → FreeBSD version < 1400097 → returns `false`
/// - `FreeBsdNew`      → FreeBSD version ≥ 1400097 → returns `true`
/// - `OtherOpenSource` → Xen / Illumos / etc. → returns `true`
#[derive(Clone, Copy, Debug, Arbitrary)]
pub enum FuzzGuestOsId {
    None,
    Proprietary,
    LinuxOld,
    LinuxNew,
    FreeBsdOld,
    FreeBsdNew,
    OtherOpenSource,
}

impl FuzzGuestOsId {
    /// Convert to the `Option<HvGuestOsId>` expected by [`FuzzNicConfig`].
    pub fn to_hv_guest_os_id(self) -> Option<HvGuestOsId> {
        use hvdef::hypercall::HvGuestOsMicrosoft;
        use hvdef::hypercall::HvGuestOsMicrosoftIds;
        use hvdef::hypercall::HvGuestOsOpenSource;
        use hvdef::hypercall::HvGuestOsOpenSourceType;

        match self {
            FuzzGuestOsId::None => None,
            FuzzGuestOsId::Proprietary => {
                // Windows NT: is_open_source bit is 0.
                let os = HvGuestOsMicrosoft::new()
                    .with_os_id(HvGuestOsMicrosoftIds::WINDOWS_NT.0)
                    .with_vendor_id(1);
                Some(HvGuestOsId::from(u64::from(os)))
            }
            FuzzGuestOsId::LinuxOld => {
                // Linux 3.10.0 (version 199168) — below the 199424 threshold.
                let os = HvGuestOsOpenSource::new()
                    .with_is_open_source(true)
                    .with_os_type(HvGuestOsOpenSourceType::LINUX.0)
                    .with_version(199168);
                Some(HvGuestOsId::from(u64::from(os)))
            }
            FuzzGuestOsId::LinuxNew => {
                // Linux 3.11.0 (version 199424) — at the threshold.
                let os = HvGuestOsOpenSource::new()
                    .with_is_open_source(true)
                    .with_os_type(HvGuestOsOpenSourceType::LINUX.0)
                    .with_version(199424);
                Some(HvGuestOsId::from(u64::from(os)))
            }
            FuzzGuestOsId::FreeBsdOld => {
                // FreeBSD version 1400096 — just below the 1400097 threshold.
                let os = HvGuestOsOpenSource::new()
                    .with_is_open_source(true)
                    .with_os_type(HvGuestOsOpenSourceType::FREEBSD.0)
                    .with_version(1400096);
                Some(HvGuestOsId::from(u64::from(os)))
            }
            FuzzGuestOsId::FreeBsdNew => {
                // FreeBSD version 1400097 — at the threshold.
                let os = HvGuestOsOpenSource::new()
                    .with_is_open_source(true)
                    .with_os_type(HvGuestOsOpenSourceType::FREEBSD.0)
                    .with_version(1400097);
                Some(HvGuestOsId::from(u64::from(os)))
            }
            FuzzGuestOsId::OtherOpenSource => {
                // Xen — neither Linux nor FreeBSD, hits the `_ => true` arm.
                let os = HvGuestOsOpenSource::new()
                    .with_is_open_source(true)
                    .with_os_type(HvGuestOsOpenSourceType::XEN.0)
                    .with_version(1);
                Some(HvGuestOsId::from(u64::from(os)))
            }
        }
    }
}

// ===========================================================================
// Fuzz loop boilerplate
// ===========================================================================

/// Yield control to the async executor `n` times.
///
/// After NVSP negotiation the Worker sends `CoordinatorMessage::Restart` and
/// enters `WaitingForCoordinator`.  The coordinator needs several poll rounds
/// to: (1) receive the `Restart`, (2) stop the worker, (3) transition it to
/// `Ready`, (4) execute `restart_queues` (which sets up `QueueState`), and
/// (5) re-start the worker so it enters `main_loop`.  Yielding here gives
/// those tasks the CPU time they need in the single-threaded executor.
pub async fn yield_to_executor(n: usize) {
    for _ in 0..n {
        let mut yielded = false;
        poll_fn(|cx: &mut Context<'_>| {
            if !yielded {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
    }
}

/// Try reading a single packet from the queue, returning true iff it is a completion packet.
pub fn try_read_one_completion(queue: &mut Queue<GpadlRingMem>) -> bool {
    let (mut reader, _) = queue.split();
    match reader.try_read() {
        Ok(packet) => matches!(&*packet, IncomingPacket::Completion(_)),
        Err(_) => false,
    }
}

/// Drain all pending packets from the queue. Useful to avoid ring-full
/// deadlocks between fuzz actions.
pub fn drain_queue(queue: &mut Queue<GpadlRingMem>) {
    loop {
        let (mut reader, _) = queue.split();
        if reader.try_read().is_err() {
            break;
        }
    }
}

/// Drain all pending packets from the queue, yielding to the executor once
/// per packet so the NIC worker and coordinator tasks can make progress
/// in the single-threaded fuzz executor.
///
/// Use this between fuzz actions to prevent ring-full deadlocks: each
/// yield gives background tasks CPU time to process ring data and
/// produce new completions.
///
/// For a synchronous (non-yielding) drain, use [`drain_queue`] instead.
pub async fn drain_queue_async(queue: &mut Queue<GpadlRingMem>) {
    loop {
        let (mut reader, _) = queue.split();
        if reader.try_read().is_err() {
            break;
        }
        // Yield once per packet so background tasks can process.
        yield_to_executor(1).await;
    }
}

// ===========================================================================
// Fuzz loop entry points
// ===========================================================================

/// Run the standard fuzz loop boilerplate: set up a NIC, run a fuzz
/// callback with a timeout, and report the outcome.
///
/// This eliminates the repeated `do_fuzz` / `fuzz_target!` pattern
/// across fuzz targets.
pub fn run_fuzz_loop<F>(input: &[u8], layout: &PageLayout, fuzz_loop: F)
where
    F: for<'a> FnOnce(
        &'a mut Unstructured<'_>,
        nic_setup::FuzzNicSetup,
    )
        -> std::pin::Pin<Box<dyn Future<Output = Result<(), anyhow::Error>> + 'a>>,
{
    run_fuzz_loop_with_config(
        input,
        layout,
        nic_setup::FuzzNicConfig::default(),
        fuzz_loop,
    )
}

/// Run the standard fuzz loop boilerplate with a custom NIC configuration.
///
/// A [`FuzzGuestOsId`] is consumed from the `Unstructured` input to override
/// `config.get_guest_os_id`, so every target that flows through this function
/// automatically exercises all `can_use_ring_opt` branches.
pub fn run_fuzz_loop_with_config<F>(
    input: &[u8],
    layout: &PageLayout,
    mut config: nic_setup::FuzzNicConfig,
    fuzz_loop: F,
) where
    F: for<'a> FnOnce(
        &'a mut Unstructured<'_>,
        nic_setup::FuzzNicSetup,
    )
        -> std::pin::Pin<Box<dyn Future<Output = Result<(), anyhow::Error>> + 'a>>,
{
    xtask_fuzz::init_tracing_if_repro();

    let mut u = Unstructured::new(input);

    // Fuzz-select the guest OS identity so every target automatically covers
    // all `can_use_ring_opt` branches (None / proprietary / Linux old/new /
    // FreeBSD old/new / other open-source).
    if let Ok(fuzz_os) = u.arbitrary::<FuzzGuestOsId>() {
        config.get_guest_os_id = fuzz_os.to_hv_guest_os_id();
    }

    pal_async::DefaultPool::run_with(async |driver| {
        nic_setup::setup_fuzz_nic_with_config(&driver, layout, config, |setup| async {
            let fuzz_result = mesh::CancelContext::new()
                .with_timeout(Duration::from_millis(500))
                .until_cancelled(fuzz_loop(&mut u, setup))
                .await;

            match fuzz_result {
                Ok(Ok(())) => {
                    fuzz_eprintln!("fuzz: test case exhausted arbitrary data");
                }
                Ok(Err(e)) => {
                    if e.downcast_ref::<arbitrary::Error>().is_some() {
                        fuzz_eprintln!("fuzz: arbitrary data exhausted: {e:#}");
                    } else if e.downcast_ref::<RingFullError>().is_some() {
                        fuzz_eprintln!("fuzz: ring full (backpressure), stopping");
                    } else {
                        panic!("fuzz: action error: {e:#}");
                    }
                }
                Err(_) => {
                    panic!("fuzz: timed out after 500ms");
                }
            }
            Ok(())
        })
        .await
    })
    .expect("fuzz pool failed to run");
}
