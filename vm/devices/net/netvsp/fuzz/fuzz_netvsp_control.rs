// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzzer for NVSP control messages processed by the NetVSP implementation.
//!
//! This fuzzer exercises the NVSP protocol state machine by sending arbitrary
//! sequences of NVSP control messages through a VMBus channel to a NetVSP
//! instance. It performs protocol negotiation first (version init, NDIS
//! config, NDIS version, receive/send buffer setup), then runs fuzz actions.
//!
//! ## NVSP protocol messages tested
//!
//! Setup (via `negotiate_to_ready_full`):
//! - `MESSAGE_TYPE_INIT` — version negotiation (fuzz-selected pair from V2–V61)
//! - `MESSAGE2_TYPE_SEND_NDIS_CONFIG` — MTU and capabilities
//! - `MESSAGE1_TYPE_SEND_NDIS_VERSION` — NDIS version (6.30)
//! - `MESSAGE1_TYPE_SEND_RECEIVE_BUFFER` — receive buffer GPADL registration
//! - `MESSAGE1_TYPE_SEND_SEND_BUFFER` — send buffer GPADL registration
//!
//! Fuzz actions (NVSP control):
//! - `MESSAGE_TYPE_INIT` — arbitrary version values
//! - `MESSAGE2_TYPE_SEND_NDIS_CONFIG` — arbitrary MTU and capabilities
//! - `MESSAGE1_TYPE_SEND_NDIS_VERSION` — arbitrary NDIS version numbers
//! - `MESSAGE1_TYPE_SEND_RECEIVE_BUFFER` — arbitrary GPADL handle and ID
//! - `MESSAGE1_TYPE_SEND_SEND_BUFFER` — arbitrary GPADL handle and ID
//! - `MESSAGE1_TYPE_REVOKE_RECEIVE_BUFFER` — revoke receive buffer
//! - `MESSAGE1_TYPE_REVOKE_SEND_BUFFER` — revoke send buffer
//! - `MESSAGE4_TYPE_SWITCH_DATA_PATH` — data path switching
//! - `MESSAGE5_TYPE_SUB_CHANNEL` — subchannel allocation requests
//! - `MESSAGE5_TYPE_OID_QUERY_EX` — NVSP-level OID queries
//! - Arbitrary raw NVSP message types and payloads
//! - Arbitrary raw VMBus packet types
//!
//! Fuzz actions (RNDIS via GpaDirect):
//! - `MESSAGE_TYPE_INITIALIZE_MSG` — RNDIS initialize with fuzzed version/transfer size
//! - `MESSAGE_TYPE_KEEPALIVE_MSG` — RNDIS keepalive messages
//! - `MESSAGE_TYPE_RESET_MSG` — RNDIS reset messages
//! - Arbitrary RNDIS data and control payloads via GpaDirect
//!
//! Fuzz actions (completions and send buffer):
//! - `MESSAGE4_TYPE_SEND_VF_ASSOCIATION` completion (TID `0x8000000000000000`)
//! - `MESSAGE4_TYPE_SWITCH_DATA_PATH` completion (TID `0x8000000000000001`)
//! - Completion packets with arbitrary transaction IDs
//! - `MESSAGE1_TYPE_SEND_RNDIS_PACKET` with adversarial send buffer section indices
//! - Flood control messages to exercise the `TooManyControlMessages` error path

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

mod fuzz_helpers;

use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use fuzz_helpers::DATA_PAGES;
use fuzz_helpers::PageLayout;
use fuzz_helpers::SWITCH_DATA_PATH_TRANSACTION_ID;
use fuzz_helpers::VF_ASSOCIATION_TRANSACTION_ID;
use fuzz_helpers::build_rndis_message;
use fuzz_helpers::drain_queue_async;
use fuzz_helpers::negotiate_to_ready_full;
use fuzz_helpers::pick_version_pair;
use fuzz_helpers::run_fuzz_loop;
use fuzz_helpers::send_completion_packet;
use fuzz_helpers::send_inband_nvsp;
use fuzz_helpers::send_rndis_gpadirect;
use fuzz_helpers::send_rndis_via_direct_path;
use fuzz_helpers::try_read_one_completion;
use fuzz_helpers::write_packet;
use guestmem::GuestMemory;
use netvsp::protocol;
use netvsp::rndisprot;
use vmbus_async::queue::Queue;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_ring::OutgoingPacketType;
use xtask_fuzz::fuzz_eprintln;
use xtask_fuzz::fuzz_target;
use zerocopy::IntoBytes;

const LAYOUT: PageLayout = PageLayout {
    send_buf_pages: 1,
    data_pages: DATA_PAGES,
};

// ---- Fuzz actions ----

/// Actions the fuzzer can take after (optional) protocol negotiation.
#[derive(Arbitrary, Debug)]
enum ControlAction {
    /// Send an arbitrary packet payload with a fuzzed packet type.
    SendRawPacket {
        #[arbitrary(with = fuzz_helpers::arbitrary_outgoing_packet_type)]
        packet_type: OutgoingPacketType<'static>,
        payload: Vec<u8>,
    },
    /// Send a raw NVSP message with arbitrary type and payload.
    SendRawInBand {
        message_type: u32,
        payload: Vec<u8>,
        with_completion: bool,
    },
    /// Send a structured Init message with arbitrary version values.
    SendInit { init: protocol::MessageInit },
    /// Send an NDIS version message with arbitrary version numbers.
    SendNdisVersion {
        version: protocol::Message1SendNdisVersion,
    },
    /// Send NDIS config with arbitrary MTU and capabilities.
    SendNdisConfig {
        config: protocol::Message2SendNdisConfig,
    },
    /// Send a receive buffer message.
    SendReceiveBuffer {
        #[arbitrary(with = fuzz_helpers::arbitrary_send_receive_buffer_message)]
        msg: protocol::Message1SendReceiveBuffer,
    },
    /// Send a send buffer message.
    SendSendBuffer {
        #[arbitrary(with = fuzz_helpers::arbitrary_send_send_buffer_message)]
        msg: protocol::Message1SendSendBuffer,
    },
    /// Send a revoke receive buffer message.
    RevokeReceiveBuffer {
        msg: protocol::Message1RevokeReceiveBuffer,
    },
    /// Send a revoke send buffer message.
    RevokeSendBuffer {
        msg: protocol::Message1RevokeSendBuffer,
    },
    /// Send a switch data path message.
    SwitchDataPath {
        msg: protocol::Message4SwitchDataPath,
    },
    /// Send a subchannel request.
    SubChannelRequest {
        request: protocol::Message5SubchannelRequest,
    },
    /// Send an OID query.
    OidQueryEx { msg: protocol::Message5OidQueryEx },
    /// Read a completion/response from the host.
    ReadCompletion,
    /// Send a VF association completion (TID 0x8000000000000000).
    SendVfAssociationCompletion,
    /// Send a switch data path completion (TID 0x8000000000000001).
    SendSwitchDataPathCompletion,
    /// Send a completion packet with an arbitrary transaction ID and payload.
    SendRawCompletion { tid: u64, payload: Vec<u8> },
    /// Send an arbitrary RNDIS packet via GpaDirect on the data channel.
    SendRndisPacketDirect { payload: Vec<u8> },
    /// Send an arbitrary RNDIS control message via GpaDirect on the control channel.
    SendRndisControlDirect { payload: Vec<u8> },
    /// Send an RNDIS INITIALIZE message with a fully fuzzed InitializeRequest.
    /// This exercises arbitrary version numbers and max_transfer_size values.
    SendRndisInitialize {
        request: rndisprot::InitializeRequest,
    },
    /// Send an RNDIS keepalive message to exercise keepalive handling.
    SendRndisKeepalive { request_id: u32 },
    /// Send an RNDIS RESET message to exercise the reset handling path.
    SendRndisReset { reserved: u32 },
    /// Send a raw MESSAGE1_TYPE_SEND_RNDIS_PACKET with adversarial
    /// send_buffer_section_index and send_buffer_section_size values.
    SendRawSendBufferPacket {
        send_buffer_section_index: u32,
        send_buffer_section_size: u32,
        channel_type: u32,
    },
    /// Flood the NIC with many RNDIS control messages without draining the
    /// completion queue. This exercises the `TooManyControlMessages` error
    /// path, which triggers when the queued control message bytes exceed
    /// 100 KB.
    FloodControlMessages {
        // How many messages to send in the burst (clamped to 15–40).
        count: u8,
    },
}

/// Execute one fuzz action by sending a message through the vmbus channel.
///
/// Returns `Ok(true)` if the fuzz loop should continue, or `Ok(false)` if
/// the NIC may have terminated (e.g. after a standalone buffer revocation)
/// and the fuzz loop should exit cleanly.
async fn execute_next_action(
    input: &mut Unstructured<'_>,
    queue: &mut Queue<GpadlRingMem>,
    mem: &GuestMemory,
    next_transaction_id: &mut u64,
) -> Result<bool, anyhow::Error> {
    let action = input.arbitrary::<ControlAction>()?;
    fuzz_eprintln!("action: {action:?}");
    match action {
        ControlAction::SendRawPacket {
            packet_type,
            payload,
        } => {
            write_packet(queue, next_transaction_id, packet_type, &[&payload]).await?;
        }
        ControlAction::SendRawInBand {
            message_type,
            payload: raw_payload,
            with_completion,
        } => {
            send_inband_nvsp(
                queue,
                next_transaction_id,
                message_type,
                &raw_payload,
                with_completion,
            )
            .await?;
        }
        ControlAction::SendInit { init } => {
            send_inband_nvsp(
                queue,
                next_transaction_id,
                protocol::MESSAGE_TYPE_INIT,
                init.as_bytes(),
                true,
            )
            .await?;
        }
        ControlAction::SendNdisVersion { version } => {
            send_inband_nvsp(
                queue,
                next_transaction_id,
                protocol::MESSAGE1_TYPE_SEND_NDIS_VERSION,
                version.as_bytes(),
                true,
            )
            .await?;
        }
        ControlAction::SendNdisConfig { config } => {
            send_inband_nvsp(
                queue,
                next_transaction_id,
                protocol::MESSAGE2_TYPE_SEND_NDIS_CONFIG,
                config.as_bytes(),
                true,
            )
            .await?;
        }
        ControlAction::SendReceiveBuffer { msg } => {
            send_inband_nvsp(
                queue,
                next_transaction_id,
                protocol::MESSAGE1_TYPE_SEND_RECEIVE_BUFFER,
                msg.as_bytes(),
                true,
            )
            .await?;
        }
        ControlAction::SendSendBuffer { msg } => {
            send_inband_nvsp(
                queue,
                next_transaction_id,
                protocol::MESSAGE1_TYPE_SEND_SEND_BUFFER,
                msg.as_bytes(),
                true,
            )
            .await?;
        }
        ControlAction::RevokeReceiveBuffer { msg } => {
            send_inband_nvsp(
                queue,
                next_transaction_id,
                protocol::MESSAGE1_TYPE_REVOKE_RECEIVE_BUFFER,
                msg.as_bytes(),
                true,
            )
            .await?;
            // A matching revoke (id == 0) terminates the NIC worker.
            // Drain any pending completions and signal the caller to stop
            // the fuzz loop so we don't hang writing to a dead ring.
            fuzz_helpers::drain_queue(queue);
            return Ok(false);
        }
        ControlAction::RevokeSendBuffer { msg } => {
            send_inband_nvsp(
                queue,
                next_transaction_id,
                protocol::MESSAGE1_TYPE_REVOKE_SEND_BUFFER,
                msg.as_bytes(),
                true,
            )
            .await?;
            // See RevokeReceiveBuffer comment above.
            fuzz_helpers::drain_queue(queue);
            return Ok(false);
        }
        ControlAction::SwitchDataPath { msg } => {
            send_inband_nvsp(
                queue,
                next_transaction_id,
                protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH,
                msg.as_bytes(),
                true,
            )
            .await?;
        }
        ControlAction::SubChannelRequest { request } => {
            send_inband_nvsp(
                queue,
                next_transaction_id,
                protocol::MESSAGE5_TYPE_SUB_CHANNEL,
                request.as_bytes(),
                true,
            )
            .await?;
        }
        ControlAction::OidQueryEx { msg } => {
            send_inband_nvsp(
                queue,
                next_transaction_id,
                protocol::MESSAGE5_TYPE_OID_QUERY_EX,
                msg.as_bytes(),
                true,
            )
            .await?;
        }
        ControlAction::ReadCompletion => {
            // Try to read a completion from the host side. This is important
            // for forward progress of various code paths.
            let _ = try_read_one_completion(queue);
        }
        ControlAction::SendVfAssociationCompletion => {
            send_completion_packet(queue, VF_ASSOCIATION_TRANSACTION_ID, &[]).await?;
        }
        ControlAction::SendSwitchDataPathCompletion => {
            send_completion_packet(queue, SWITCH_DATA_PATH_TRANSACTION_ID, &[]).await?;
        }
        ControlAction::SendRawCompletion { tid, payload } => {
            send_completion_packet(queue, tid, &[&payload]).await?;
        }
        ControlAction::SendRndisPacketDirect { payload } => {
            send_rndis_via_direct_path(
                queue,
                mem,
                &payload,
                protocol::DATA_CHANNEL_TYPE,
                &LAYOUT,
                next_transaction_id,
            )
            .await?;
        }
        ControlAction::SendRndisControlDirect { payload } => {
            send_rndis_via_direct_path(
                queue,
                mem,
                &payload,
                protocol::CONTROL_CHANNEL_TYPE,
                &LAYOUT,
                next_transaction_id,
            )
            .await?;
        }
        ControlAction::SendRndisInitialize { request } => {
            let rndis_bytes =
                build_rndis_message(rndisprot::MESSAGE_TYPE_INITIALIZE_MSG, request.as_bytes());
            send_rndis_gpadirect(
                queue,
                mem,
                &rndis_bytes,
                protocol::CONTROL_CHANNEL_TYPE,
                LAYOUT.data_page_start(),
                LAYOUT.data_pages,
                next_transaction_id,
            )
            .await?;
        }
        ControlAction::SendRndisKeepalive { request_id } => {
            let keepalive = rndisprot::KeepaliveRequest { request_id };
            let rndis_bytes =
                build_rndis_message(rndisprot::MESSAGE_TYPE_KEEPALIVE_MSG, keepalive.as_bytes());
            send_rndis_gpadirect(
                queue,
                mem,
                &rndis_bytes,
                protocol::CONTROL_CHANNEL_TYPE,
                LAYOUT.data_page_start(),
                LAYOUT.data_pages,
                next_transaction_id,
            )
            .await?;
        }
        ControlAction::SendRndisReset { reserved } => {
            let rndis_bytes =
                build_rndis_message(rndisprot::MESSAGE_TYPE_RESET_MSG, &reserved.to_le_bytes());
            send_rndis_gpadirect(
                queue,
                mem,
                &rndis_bytes,
                protocol::CONTROL_CHANNEL_TYPE,
                LAYOUT.data_page_start(),
                LAYOUT.data_pages,
                next_transaction_id,
            )
            .await?;
        }
        ControlAction::SendRawSendBufferPacket {
            send_buffer_section_index,
            send_buffer_section_size,
            channel_type,
        } => {
            let msg = protocol::Message1SendRndisPacket {
                channel_type,
                send_buffer_section_index,
                send_buffer_section_size,
            };
            send_inband_nvsp(
                queue,
                next_transaction_id,
                protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET,
                msg.as_bytes(),
                true,
            )
            .await?;
        }
        ControlAction::FloodControlMessages { count } => {
            // Clamp count to [15, 40] to send enough messages to approach
            // the 100KB control message queue threshold without timing out.
            // 15 × 8192 = 120KB, enough to trigger the limit.
            let n = 15 + (count as usize % 26); // 15..=40
            let big_payload = vec![0xABu8; 8192];
            let rndis_bytes = build_rndis_message(rndisprot::MESSAGE_TYPE_QUERY_MSG, &big_payload);
            // Yield every 3 messages: often enough to keep the ring from
            // filling up (the 16KB ring only holds a few GpaDirect packets),
            // but infrequent enough to avoid switching overhead and timeouts.
            for i in 0..n {
                let result = send_rndis_gpadirect(
                    queue,
                    mem,
                    &rndis_bytes,
                    protocol::CONTROL_CHANNEL_TYPE,
                    LAYOUT.data_page_start(),
                    LAYOUT.data_pages,
                    next_transaction_id,
                )
                .await;
                match result {
                    Ok(()) => {}
                    Err(e) if e.downcast_ref::<fuzz_helpers::RingFullError>().is_some() => break,
                    Err(e) => return Err(e),
                }
                if (i + 1) % 3 == 0 {
                    fuzz_helpers::yield_to_executor(1).await;
                }
            }
            // Final yield + drain to avoid interfering with subsequent actions.
            fuzz_helpers::yield_to_executor(1).await;
            drain_queue_async(queue).await;
        }
    }
    Ok(true)
}

fuzz_target!(|input: &[u8]| {
    run_fuzz_loop(input, &LAYOUT, |fuzzer_input, setup| {
        Box::pin(async move {
            let mut queue = setup.queue;
            let mem = setup.mem;
            let mut next_transaction_id = 1u64;

            // Pick a fuzzer-driven protocol version pair.
            let version_init = pick_version_pair(fuzzer_input)?;

            negotiate_to_ready_full(
                &mut queue,
                &mut next_transaction_id,
                setup.recv_buf_gpadl_id,
                setup.send_buf_gpadl_id,
                protocol::NdisConfigCapabilities::new(),
                version_init,
            )
            .await?;

            // Run fuzz actions until input is exhausted or the NIC terminates.
            while !fuzzer_input.is_empty() {
                let should_continue =
                    execute_next_action(fuzzer_input, &mut queue, &mem, &mut next_transaction_id)
                        .await?;
                if !should_continue {
                    break;
                }
                drain_queue_async(&mut queue).await;
            }
            Ok(())
        })
    });
});
