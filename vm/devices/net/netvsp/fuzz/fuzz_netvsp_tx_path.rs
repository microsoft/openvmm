// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzzer for the NVSP TX path (RNDIS packet messages).
//!
//! This fuzzer exercises the guest-to-host transmit path by sending arbitrary
//! RNDIS data packets through a VMBus channel to a connected NetVSP instance.
//! It performs protocol negotiation first, then sends fuzzed
//! RNDIS packet messages via GpaDirect, including:
//!
//! - Malformed RNDIS headers (bad message_type, message_length)
//! - Malformed rndisprot::Packet fields (data_offset, data_length, PPI)
//! - Arbitrary per-packet info (PPI) chains (checksum, LSO, unknown types)
//! - Structured PPI chains with valid `PerPacketInfo` headers and fuzzed
//!   offload payloads (`TxTcpIpChecksumInfo`, `TcpLsoInfo`)
//! - LSO edge cases: MSS of 0, 1, max (0xFFFFF), tcp_header_offset < 14
//! - Checksum edge cases: all flag combinations, tcp_header_offset < 14
//! - Multiple concatenated RNDIS packets in one VMBus message
//! - Send buffer path with arbitrary section indices
//! - TX completion messages with arbitrary transaction IDs
//!
//! ## NVSP protocol messages tested
//!
//! - `MESSAGE1_TYPE_SEND_RNDIS_PACKET` — RNDIS packet via send buffer path
//! - `MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE` — TX completion with arbitrary
//!   transaction ID
//!
//! ## RNDIS protocol messages tested
//!
//! - `MESSAGE_TYPE_PACKET_MSG` — structured RNDIS data packets

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

mod fuzz_helpers;

use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use fuzz_helpers::DATA_PAGES;
use fuzz_helpers::PageLayout;
use fuzz_helpers::StructuredPpiEntry;
use fuzz_helpers::StructuredRndisMessage;
use fuzz_helpers::StructuredRndisPacketMessage;
use fuzz_helpers::build_checksum_ppi_entry;
use fuzz_helpers::build_concatenated_rndis_messages;
use fuzz_helpers::build_lso_ppi_entry;
use fuzz_helpers::build_rndis_message;
use fuzz_helpers::build_structured_rndis_packet;
use fuzz_helpers::drain_queue_async;
use fuzz_helpers::endpoint::FuzzEndpointConfig;
use fuzz_helpers::negotiate_to_ready;
use fuzz_helpers::nic_setup::FuzzNicConfig;
use fuzz_helpers::rndis_initialize;
use fuzz_helpers::run_fuzz_loop_with_config;
use fuzz_helpers::send_inband_nvsp;
use fuzz_helpers::send_rndis_gpadirect;
use fuzz_helpers::send_rndis_via_direct_path;
use fuzz_helpers::send_rndis_via_send_buffer;
use fuzz_helpers::send_tx_rndis_completion;
use fuzz_helpers::serialize_ppi_chain;
use fuzz_helpers::serialize_structured_rndis_packet_message;
use fuzz_helpers::try_read_one_completion;
use guestmem::GuestMemory;
use netvsp::protocol;
use netvsp::rndisprot;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use vmbus_async::queue::Queue;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use xtask_fuzz::fuzz_eprintln;
use xtask_fuzz::fuzz_target;
use zerocopy::IntoBytes;

const LAYOUT: PageLayout = PageLayout {
    send_buf_pages: 4,
    data_pages: DATA_PAGES,
};

/// Actions the fuzzer can take on the data path.
#[derive(Arbitrary, Debug)]
enum DataPathAction {
    /// Send a single RNDIS packet message via GpaDirect with fuzzed content.
    SendRndisPacket {
        /// Structured RNDIS packet message.
        rndis: StructuredRndisPacketMessage,
        /// NVSP RNDIS packet metadata.
        nvsp_msg: protocol::Message1SendRndisPacket,
    },
    /// Send a structured RNDIS packet with fuzzed PPI and frame data via GpaDirect.
    SendStructuredRndisPacket {
        /// Fuzzed per-packet-info bytes (PPI chain).
        ppi_bytes: Vec<u8>,
        /// Fuzzed ethernet frame data.
        frame_data: Vec<u8>,
        /// NVSP RNDIS packet metadata.
        nvsp_msg: protocol::Message1SendRndisPacket,
    },
    /// Send a structured RNDIS packet with fuzzed PPI and a mostly valid
    /// Ethernet frame to drive backend RX parsing paths.
    SendStructuredValidEthernetFrame {
        /// Fuzzed per-packet-info bytes (PPI chain).
        ppi_bytes: Vec<u8>,
        /// Mostly valid Ethernet II frame.
        #[arbitrary(with = fuzz_helpers::arbitrary_valid_ethernet_frame)]
        frame_data: Vec<u8>,
        /// NVSP RNDIS packet metadata.
        nvsp_msg: protocol::Message1SendRndisPacket,
    },
    /// Send a structured RNDIS packet with a PPI chain
    /// containing properly formatted checksum and/or LSO entries.
    SendWithStructuredPpi {
        /// Structured PPI entries to serialize into the PPI region.
        ppi_entries: Vec<StructuredPpiEntry>,
        /// Fuzzed ethernet frame data.
        frame_data: Vec<u8>,
        /// NVSP RNDIS packet metadata.
        nvsp_msg: protocol::Message1SendRndisPacket,
    },
    /// Send a packet with a specific LSO PPI entry. Fuzzed fields cover
    /// edge cases: MSS of 0, 1, max (0xFFFFF), tcp_header_offset < 14
    /// (triggers `InvalidTcpHeaderOffset`), and large offsets.
    SendLsoPacket {
        /// MSS value (bits 0-19 of TcpLsoInfo). Fuzzed u32 covers 0, 1,
        /// 0xFFFFF and all values in between.
        mss: u32,
        /// TCP header offset. Values < 14 trigger an error path.
        tcp_header_offset: u16,
        /// Whether this is IPv6 (true) or IPv4 (false).
        is_ipv6: bool,
        /// Fuzzed ethernet frame data.
        frame_data: Vec<u8>,
    },
    /// Send a packet with a specific checksum PPI entry to exercise
    /// all flag combinations: IPv4/IPv6, TCP/UDP checksum, IP header
    /// checksum, and various tcp_header_offset values (including < 14
    /// to test the IHL-parsing fallback path).
    SendChecksumEdgeCase {
        /// Raw `TxTcpIpChecksumInfo` bits.
        checksum_info: u32,
        /// Fuzzed ethernet frame data.
        frame_data: Vec<u8>,
    },
    /// Send multiple concatenated RNDIS packets in one GpaDirect message.
    SendMultipleRndisPackets {
        /// Each entry is one structured RNDIS message.
        messages: Vec<StructuredRndisMessage>,
    },
    /// Send RNDIS data via the send buffer path (section index != 0xFFFFFFFF).
    SendViaSendBuffer {
        /// Structured RNDIS packet message to place in the send buffer.
        rndis: StructuredRndisPacketMessage,
        /// NVSP RNDIS packet metadata.
        nvsp_msg: protocol::Message1SendRndisPacket,
    },
    /// Send a TX completion with an arbitrary transaction ID (fuzz
    /// release_recv_buffers / completion handling).
    SendTxCompletion {
        transaction_id: u64,
        completion: protocol::Message1SendRndisPacketComplete,
    },
    /// Send an RNDIS control message (INITIALIZE, QUERY, SET, etc.).
    SendRndisControl {
        /// RNDIS message header.
        header: rndisprot::MessageHeader,
        /// Payload after the RNDIS MessageHeader.
        payload: Vec<u8>,
    },
    /// Drain completions from the host.
    ReadCompletion,
    /// Send a raw MESSAGE1_TYPE_SEND_RNDIS_PACKET with adversarial
    /// send_buffer_section_index and send_buffer_section_size values to
    /// exercise the section-index arithmetic and `try_subrange()` validation.
    /// Does NOT use the `send_rndis_via_send_buffer` helper (which clamps
    /// the write), so the device-side validation must handle the raw values.
    SendRawSendBufferPacket {
        /// Adversarial section index (e.g. u32::MAX, 0, section_count+1).
        send_buffer_section_index: u32,
        /// Adversarial section size.
        send_buffer_section_size: u32,
        /// Channel type (data or control).
        channel_type: u32,
    },
    /// Inject a `TxError::TryRestart` on the next `tx_poll`, then send a
    /// packet to trigger `process_endpoint_tx`.  This exercises the
    /// `TryRestart` error handling path that completes pending TXs and
    /// signals `CoordinatorMessage::Restart`.
    InjectTxRestart {
        ppi_bytes: Vec<u8>,
        frame_data: Vec<u8>,
    },
    /// Inject a `TxError::Fatal` on the next `tx_poll`, then send a packet
    /// to trigger `process_endpoint_tx`.  This exercises the `Fatal` error
    /// path that propagates `WorkerError::Endpoint`.
    InjectTxFatal {
        ppi_bytes: Vec<u8>,
        frame_data: Vec<u8>,
    },
}

/// Execute one fuzz action on the data path.
async fn execute_next_action(
    input: &mut Unstructured<'_>,
    queue: &mut Queue<GpadlRingMem>,
    mem: &GuestMemory,
    next_transaction_id: &mut u64,
    tx_error_mode: &Option<Arc<AtomicU8>>,
) -> Result<(), anyhow::Error> {
    let action = input.arbitrary::<DataPathAction>()?;
    fuzz_eprintln!("action: {action:?}");
    let tid = next_transaction_id;
    match action {
        DataPathAction::SendRndisPacket {
            mut rndis,
            nvsp_msg,
        } => {
            let rndis_bytes = serialize_structured_rndis_packet_message(&mut rndis);
            send_rndis_via_direct_path(
                queue,
                mem,
                &rndis_bytes,
                nvsp_msg.channel_type,
                &LAYOUT,
                tid,
            )
            .await?;
        }
        DataPathAction::SendStructuredRndisPacket {
            ppi_bytes,
            frame_data,
            nvsp_msg,
        } => {
            let rndis_buf = build_structured_rndis_packet(&ppi_bytes, &frame_data);
            send_rndis_via_direct_path(queue, mem, &rndis_buf, nvsp_msg.channel_type, &LAYOUT, tid)
                .await?;
        }
        DataPathAction::SendStructuredValidEthernetFrame {
            ppi_bytes,
            frame_data,
            nvsp_msg,
        } => {
            let rndis_buf = build_structured_rndis_packet(&ppi_bytes, &frame_data);
            send_rndis_via_direct_path(queue, mem, &rndis_buf, nvsp_msg.channel_type, &LAYOUT, tid)
                .await?;
        }
        DataPathAction::SendWithStructuredPpi {
            ppi_entries,
            frame_data,
            nvsp_msg,
        } => {
            let ppi_bytes = serialize_ppi_chain(&ppi_entries);
            let rndis_buf = build_structured_rndis_packet(&ppi_bytes, &frame_data);
            send_rndis_via_direct_path(queue, mem, &rndis_buf, nvsp_msg.channel_type, &LAYOUT, tid)
                .await?;
        }
        DataPathAction::SendLsoPacket {
            mss,
            tcp_header_offset,
            is_ipv6,
            frame_data,
        } => {
            let ppi_bytes = build_lso_ppi_entry(mss, tcp_header_offset, is_ipv6);
            let rndis_buf = build_structured_rndis_packet(&ppi_bytes, &frame_data);
            send_rndis_via_direct_path(
                queue,
                mem,
                &rndis_buf,
                protocol::DATA_CHANNEL_TYPE,
                &LAYOUT,
                tid,
            )
            .await?;
        }
        DataPathAction::SendChecksumEdgeCase {
            checksum_info,
            frame_data,
        } => {
            let ppi_bytes = build_checksum_ppi_entry(checksum_info);
            let rndis_buf = build_structured_rndis_packet(&ppi_bytes, &frame_data);
            send_rndis_via_direct_path(
                queue,
                mem,
                &rndis_buf,
                protocol::DATA_CHANNEL_TYPE,
                &LAYOUT,
                tid,
            )
            .await?;
        }
        DataPathAction::SendMultipleRndisPackets { messages } => {
            let rndis_buf = build_concatenated_rndis_messages(&messages);

            if !rndis_buf.is_empty() {
                send_rndis_via_direct_path(
                    queue,
                    mem,
                    &rndis_buf,
                    protocol::DATA_CHANNEL_TYPE,
                    &LAYOUT,
                    tid,
                )
                .await?;
            }
        }
        DataPathAction::SendViaSendBuffer {
            mut rndis,
            nvsp_msg,
        } => {
            let rndis_bytes = serialize_structured_rndis_packet_message(&mut rndis);
            send_rndis_via_send_buffer(queue, mem, &rndis_bytes, &nvsp_msg, &LAYOUT, tid).await?;
        }
        DataPathAction::SendTxCompletion {
            transaction_id,
            completion,
        } => {
            send_tx_rndis_completion(queue, transaction_id, &completion).await?;
        }
        DataPathAction::SendRndisControl { header, payload } => {
            // Send an RNDIS control message with arbitrary message type
            // and payload via GpaDirect.
            let rndis_buf = build_rndis_message(header.message_type, &payload);
            send_rndis_gpadirect(
                queue,
                mem,
                &rndis_buf,
                protocol::CONTROL_CHANNEL_TYPE,
                LAYOUT.data_page_start(),
                LAYOUT.data_pages,
                tid,
            )
            .await?;
        }
        DataPathAction::ReadCompletion => {
            let _ = try_read_one_completion(queue);
        }
        DataPathAction::SendRawSendBufferPacket {
            send_buffer_section_index,
            send_buffer_section_size,
            channel_type,
        } => {
            // Send a raw RNDIS packet message with adversarial section
            // index/size. This bypasses the send_rndis_via_send_buffer
            // helper's clamping to exercise the device-side validation of
            // the section index × 6144 multiplication and try_subrange().
            let msg = protocol::Message1SendRndisPacket {
                channel_type,
                send_buffer_section_index,
                send_buffer_section_size,
            };
            send_inband_nvsp(
                queue,
                tid,
                protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET,
                msg.as_bytes(),
                true,
            )
            .await?;
        }
        DataPathAction::InjectTxRestart {
            ppi_bytes,
            frame_data,
        } => {
            if let Some(mode) = tx_error_mode {
                // Arm the TryRestart error for the next tx_poll.
                mode.store(1, Ordering::Relaxed);
                // Send a packet to trigger tx_avail → tx_poll.
                let rndis_buf = build_structured_rndis_packet(&ppi_bytes, &frame_data);
                let _ = send_rndis_via_direct_path(
                    queue,
                    mem,
                    &rndis_buf,
                    protocol::DATA_CHANNEL_TYPE,
                    &LAYOUT,
                    tid,
                )
                .await;
                // Yield so the worker can process the error and restart.
                fuzz_helpers::yield_to_executor(20).await;
                // Drain to let the coordinator finish restarting.
                drain_queue_async(queue).await;
            }
        }
        DataPathAction::InjectTxFatal {
            ppi_bytes,
            frame_data,
        } => {
            if let Some(mode) = tx_error_mode {
                // Arm the Fatal error for the next tx_poll.
                mode.store(2, Ordering::Relaxed);
                // Send a packet to trigger tx_avail → tx_poll.
                let rndis_buf = build_structured_rndis_packet(&ppi_bytes, &frame_data);
                let _ = send_rndis_via_direct_path(
                    queue,
                    mem,
                    &rndis_buf,
                    protocol::DATA_CHANNEL_TYPE,
                    &LAYOUT,
                    tid,
                )
                .await;
                // Yield so the worker can process the fatal error.
                fuzz_helpers::yield_to_executor(10).await;
            }
        }
    }
    Ok(())
}

fuzz_target!(|input: &[u8]| {
    // Create a FuzzEndpoint with TX error injection enabled.
    let (endpoint, handles) = fuzz_helpers::endpoint::FuzzEndpoint::new(FuzzEndpointConfig {
        enable_tx_error_injection: true,
        enable_async_tx: true,
        ..FuzzEndpointConfig::default()
    });
    let tx_error_mode = handles.tx_error_mode;

    run_fuzz_loop_with_config(
        input,
        &LAYOUT,
        FuzzNicConfig {
            endpoint: Box::new(endpoint),
            ..FuzzNicConfig::default()
        },
        |fuzzer_input, setup| {
            Box::pin(async move {
                let mut queue = setup.queue;
                let mem = setup.mem;
                let mut next_transaction_id = 1u64;

                // Always negotiate to the ready state first — data path messages are
                // only processed once the NIC is fully initialized.
                negotiate_to_ready(
                    &mut queue,
                    &mut next_transaction_id,
                    setup.recv_buf_gpadl_id,
                    setup.send_buf_gpadl_id,
                )
                .await?;

                // 90% of the time, initialize RNDIS to reach Operational state.
                // The remaining 10% tests behavior when RNDIS packets arrive
                // before the initialize handshake.
                if fuzzer_input.ratio(9, 10)? {
                    rndis_initialize(
                        &mut queue,
                        &mem,
                        LAYOUT.data_page_start(),
                        LAYOUT.data_pages,
                        &mut next_transaction_id,
                    )
                    .await?;
                }

                // Run data path actions until input is exhausted.
                while !fuzzer_input.is_empty() {
                    execute_next_action(
                        fuzzer_input,
                        &mut queue,
                        &mem,
                        &mut next_transaction_id,
                        &tx_error_mode,
                    )
                    .await?;
                    drain_queue_async(&mut queue).await;
                }
                Ok(())
            })
        },
    );
});
