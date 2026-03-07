// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzzer for the NetVSP RX (host-to-guest receive) path.
//!
//! ## NVSP protocol messages tested
//!
//! Indirectly exercised (host-to-guest responses):
//! - `MESSAGE1_TYPE_SEND_RNDIS_PACKET` — host-side RX packet delivery via
//!   receive buffer (triggered by loopback reflecting TX packets)
//! - `MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE` — TX completion from host
//!
//! ## RNDIS protocol messages tested
//!
//! - `MESSAGE_TYPE_PACKET_MSG` — structured RNDIS data packets via GpaDirect
//!   (fuzzed `Packet` fields, PPI chains, frame data)
//! - `MESSAGE_TYPE_PACKET_MSG` — zero-length, oversized, and burst packets
//! - `MESSAGE_TYPE_PACKET_MSG` — packets with valid Ethernet II frames
//! - `MESSAGE_TYPE_PACKET_MSG` — page-boundary edge-case frames that stress
//!   cross-page write logic in `write_at()` / `write_header()`
//! - `MESSAGE_TYPE_PACKET_MSG` — MTU-sized frames (1514 bytes)
//! - Arbitrary RNDIS control messages interleaved with RX traffic
//!
//! Indirectly exercised (host-to-guest, in receive buffer):
//! - `MESSAGE_TYPE_INDICATE_STATUS_MSG` — status indications
//! - `MESSAGE_TYPE_PACKET_MSG` — loopback-reflected RX packets

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

mod fuzz_helpers;

use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use fuzz_helpers::DATA_PAGES;
use fuzz_helpers::PageLayout;
use fuzz_helpers::StructuredRndisPacketMessage;
use fuzz_helpers::build_structured_rndis_packet;
use fuzz_helpers::drain_queue;
use fuzz_helpers::drain_queue_async;
use fuzz_helpers::endpoint::FuzzEndpoint;
use fuzz_helpers::endpoint::FuzzEndpointConfig;
use fuzz_helpers::endpoint::FuzzRxMetadata;
use fuzz_helpers::negotiate_to_ready_full;
use fuzz_helpers::nic_setup::FuzzNicConfig;
use fuzz_helpers::page_boundary_frame_size;
use fuzz_helpers::pick_version_pair;
use fuzz_helpers::rndis_initialize;
use fuzz_helpers::rndis_set_packet_filter;
use fuzz_helpers::run_fuzz_loop_with_config;
use fuzz_helpers::send_rndis_via_direct_path;
use fuzz_helpers::serialize_structured_rndis_packet_message;
use fuzz_helpers::try_read_one_completion;
use guestmem::GuestMemory;
use netvsp::protocol;
use vmbus_async::queue::Queue;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_ring::PAGE_SIZE;
use xtask_fuzz::fuzz_eprintln;
use xtask_fuzz::fuzz_target;

const LAYOUT: PageLayout = PageLayout {
    send_buf_pages: 1,
    data_pages: DATA_PAGES,
};

/// Actions the fuzzer can take to exercise the RX path.
#[derive(Arbitrary, Debug)]
enum RxAction {
    /// Send a TX packet via GpaDirect. The loopback endpoint will reflect
    /// it back as an RX packet, exercising the full TX-to-loopback-to-RX path.
    SendTxPacketForLoopback {
        /// Fuzzed per-packet-info bytes (PPI chain).
        ppi_bytes: Vec<u8>,
        /// Fuzzed ethernet frame data.
        frame_data: Vec<u8>,
    },
    /// Send a TX packet with a mostly valid Ethernet frame for loopback.
    SendValidFrameForLoopback {
        /// Fuzzed per-packet-info bytes.
        ppi_bytes: Vec<u8>,
        /// Mostly valid Ethernet II frame.
        #[arbitrary(with = fuzz_helpers::arbitrary_valid_ethernet_frame)]
        frame_data: Vec<u8>,
    },
    /// Send multiple TX packets in rapid succession to stress RX buffer
    /// allocation under load.
    BurstTxForLoopback {
        /// Multiple frames to send back-to-back.
        frames: Vec<Vec<u8>>,
    },
    /// Send a zero-length frame to test edge cases in the RX path.
    SendEmptyFrame,
    /// Send a very large frame (> MTU) to test oversized packet handling.
    SendOversizedFrame {
        /// Size of the frame (clamped to data page capacity).
        size: u16,
    },
    /// Send a structured RNDIS packet with fuzzed fields via GpaDirect.
    SendRndisPacket {
        rndis: StructuredRndisPacketMessage,
        nvsp_msg: protocol::Message1SendRndisPacket,
    },
    /// Send an RNDIS control message while RX is active to test
    /// interleaving of control and data paths.
    SendRndisControl {
        /// Raw payload after the RNDIS MessageHeader.
        payload: Vec<u8>,
        /// RNDIS message type.
        message_type: u32,
    },
    /// Read one completion/notification from the host.
    ReadCompletion,
    /// Drain all pending packets from the queue.
    DrainQueue,
    /// Send a frame whose size is chosen to hit page-boundary edge cases
    /// in `write_at()` and `write_header()`. This exercises the cross-page
    /// write logic in `buffers.rs` with sizes near 4096-byte boundaries.
    SendPageBoundaryFrame {
        /// 0 = PAGE_SIZE - 1, 1 = PAGE_SIZE, 2 = PAGE_SIZE + 1,
        /// 3 = PAGE_SIZE - RX_HEADER_LEN, 4 = 2 * PAGE_SIZE - RX_HEADER_LEN,
        /// 5+ = arbitrary small offset from PAGE_SIZE.
        variant: u8,
    },
    /// Send a frame of exactly MTU size to exercise the capacity boundary.
    SendMtuSizedFrame,
}

/// Execute one RX fuzz action.
async fn execute_next_action(
    input: &mut Unstructured<'_>,
    queue: &mut Queue<GpadlRingMem>,
    mem: &GuestMemory,
    next_transaction_id: &mut u64,
) -> Result<(), anyhow::Error> {
    let action = input.arbitrary::<RxAction>()?;
    fuzz_eprintln!("action: {action:?}");
    match action {
        RxAction::SendTxPacketForLoopback {
            ppi_bytes,
            frame_data,
        }
        | RxAction::SendValidFrameForLoopback {
            ppi_bytes,
            frame_data,
        } => {
            // Both variants produce the same packet — the difference is in
            // `#[arbitrary(with = ...)]` on the enum definition, which controls
            // how `frame_data` is generated (fully random vs. valid Ethernet).
            let rndis_buf = build_structured_rndis_packet(&ppi_bytes, &frame_data);
            send_rndis_via_direct_path(
                queue,
                mem,
                &rndis_buf,
                protocol::DATA_CHANNEL_TYPE,
                &LAYOUT,
                next_transaction_id,
            )
            .await?;
        }
        RxAction::BurstTxForLoopback { frames } => {
            for frame_data in &frames {
                let rndis_buf = build_structured_rndis_packet(&[], frame_data);
                send_rndis_via_direct_path(
                    queue,
                    mem,
                    &rndis_buf,
                    protocol::DATA_CHANNEL_TYPE,
                    &LAYOUT,
                    next_transaction_id,
                )
                .await?;
            }
        }
        RxAction::SendEmptyFrame => {
            let rndis_buf = build_structured_rndis_packet(&[], &[]);
            send_rndis_via_direct_path(
                queue,
                mem,
                &rndis_buf,
                protocol::DATA_CHANNEL_TYPE,
                &LAYOUT,
                next_transaction_id,
            )
            .await?;
        }
        RxAction::SendOversizedFrame { size } => {
            // Create a frame larger than a typical MTU.
            let actual_size = (size as usize).clamp(1500, DATA_PAGES * PAGE_SIZE);
            let frame_data = vec![0xAB; actual_size];
            let rndis_buf = build_structured_rndis_packet(&[], &frame_data);
            send_rndis_via_direct_path(
                queue,
                mem,
                &rndis_buf,
                protocol::DATA_CHANNEL_TYPE,
                &LAYOUT,
                next_transaction_id,
            )
            .await?;
        }
        RxAction::SendRndisPacket {
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
                next_transaction_id,
            )
            .await?;
        }
        RxAction::SendRndisControl {
            payload,
            message_type,
        } => {
            let rndis_buf = fuzz_helpers::build_rndis_message(message_type, &payload);
            fuzz_helpers::send_rndis_control(queue, mem, &rndis_buf, &LAYOUT, next_transaction_id)
                .await?;
        }
        RxAction::ReadCompletion => {
            let _ = try_read_one_completion(queue);
        }
        RxAction::DrainQueue => {
            drain_queue(queue);
        }
        RxAction::SendPageBoundaryFrame { variant } => {
            let frame_data = vec![0xBB; page_boundary_frame_size(variant)];
            let rndis_buf = build_structured_rndis_packet(&[], &frame_data);
            send_rndis_via_direct_path(
                queue,
                mem,
                &rndis_buf,
                protocol::DATA_CHANNEL_TYPE,
                &LAYOUT,
                next_transaction_id,
            )
            .await?;
        }
        RxAction::SendMtuSizedFrame => {
            // Send a frame of exactly 1514 bytes (standard Ethernet MTU +
            // header), the most common real-world frame size, to ensure the
            // hot path is well-covered.
            let frame_data = vec![0xDD; 1514];
            let rndis_buf = build_structured_rndis_packet(&[], &frame_data);
            send_rndis_via_direct_path(
                queue,
                mem,
                &rndis_buf,
                protocol::DATA_CHANNEL_TYPE,
                &LAYOUT,
                next_transaction_id,
            )
            .await?;
        }
    }
    Ok(())
}

fuzz_target!(|input: &[u8]| {
    // Parse a loopback metadata template from the front of the input so that
    // TX→RX loopback packets exercise varied checksum-flag branches in
    // `write_header()` (48 combinations of IP checksum × L4 protocol × L4
    // checksum state).
    let mut pre = Unstructured::new(input);
    let loopback_meta = pre.arbitrary::<FuzzRxMetadata>().unwrap_or_default();
    let remaining_start = input.len() - pre.len();
    let fuzz_input = &input[remaining_start..];

    // Use the FuzzEndpoint so that the loopback reflects TX-to-RX.
    let (mut endpoint, _handles) = FuzzEndpoint::new(FuzzEndpointConfig {
        enable_rx_injection: false,
        enable_action_injection: false,
        ..FuzzEndpointConfig::default()
    });
    endpoint.loopback_metadata = loopback_meta;
    let config = FuzzNicConfig {
        endpoint: Box::new(endpoint),
        virtual_function: None,
        ..FuzzNicConfig::default()
    };

    run_fuzz_loop_with_config(fuzz_input, &LAYOUT, config, |fuzzer_input, setup| {
        Box::pin(async move {
            let mut queue = setup.queue;
            let mem = setup.mem;
            let mut next_transaction_id = 1u64;

            // Pick a fuzzer-driven protocol version pair.
            let version_init = pick_version_pair(fuzzer_input)?;

            // Negotiate to the ready state first.
            negotiate_to_ready_full(
                &mut queue,
                &mut next_transaction_id,
                setup.recv_buf_gpadl_id,
                setup.send_buf_gpadl_id,
                protocol::NdisConfigCapabilities::new(),
                version_init,
            )
            .await?;

            // 90% of the time, initialize RNDIS to reach Operational state.
            if fuzzer_input.ratio(9, 10)? {
                rndis_initialize(
                    &mut queue,
                    &mem,
                    LAYOUT.data_page_start(),
                    LAYOUT.data_pages,
                    &mut next_transaction_id,
                )
                .await?;

                // Set the packet filter so RX packets are actually delivered
                // instead of being silently dropped in process_endpoint_rx.
                rndis_set_packet_filter(&mut queue, &mem, &LAYOUT, &mut next_transaction_id)
                    .await?;
            }

            // Run RX-focused fuzz actions until input is exhausted.
            while !fuzzer_input.is_empty() {
                execute_next_action(fuzzer_input, &mut queue, &mem, &mut next_transaction_id)
                    .await?;
                drain_queue_async(&mut queue).await;
            }
            Ok(())
        })
    });
});
