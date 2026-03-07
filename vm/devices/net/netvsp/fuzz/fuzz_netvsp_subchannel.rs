// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzzer for NetVSP subchannel handling and multi-queue code paths.
//!
//! This fuzzer exercises the subchannel allocation and multi-queue paths end-
//! to-end using [`MultiChannelMockVmbus`]. When the guest sends a successful
//! `MESSAGE5_TYPE_SUB_CHANNEL` request, `restart_queues()` runs, calls
//! `enable_subchannels()`, and [`SubchannelOpener::open_pending`] completes
//! the ring handshake for each new subchannel. This unlocks code paths that
//! are unreachable with a single-channel mock:
//!
//! - `Nic::open()` subchannel branch (coordinator stop, `WorkerState::Ready`
//!   construction, "all subchannels opened" check)
//! - `restart_queues()` multi-queue loop (per-queue `QueueConfig`, RX buffer
//!   partitioning via `RxBufferRanges::new`)
//! - `RxBufferRange::send_if_remote()` cross-queue RX buffer routing
//! - `Coordinator::process()` subworker start loop (`workers[1..].start()`)
//! - RSS-driven queue distribution and indirection table application
//!
//! ## NVSP protocol messages tested
//!
//! - `MESSAGE5_TYPE_SUB_CHANNEL` — subchannel allocation
//! - `MESSAGE5_TYPE_SEND_INDIRECTION_TABLE` — indirection table updates
//! - `MESSAGE4_TYPE_SWITCH_DATA_PATH` — data path switching
//! - `MESSAGE1_TYPE_SEND_RNDIS_PACKET` — data via primary and subchannel rings

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

mod fuzz_helpers;

use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use fuzz_helpers::DATA_PAGES;
use fuzz_helpers::PageLayout;
use fuzz_helpers::build_rss_oid_set;
use fuzz_helpers::build_structured_rndis_packet;
use fuzz_helpers::drain_queue;
use fuzz_helpers::drain_queue_async;
use fuzz_helpers::negotiate_to_ready;
use fuzz_helpers::nic_setup::FuzzNicConfig;
use fuzz_helpers::nic_setup::SubchannelOpener;
use fuzz_helpers::nvsp_payload;
use fuzz_helpers::rndis_initialize;
use fuzz_helpers::run_fuzz_loop_with_config;
use fuzz_helpers::send_inband_nvsp;
use fuzz_helpers::send_rndis_gpadirect;
use fuzz_helpers::send_rndis_via_direct_path;
use fuzz_helpers::try_read_one_completion;
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

/// Maximum subchannels the multi-channel mock will support. Four gives enough
/// coverage for all partitioning logic without inflating memory or runtime.
const MAX_FUZZ_SUBCHANNELS: u16 = 4;

/// Actions the fuzzer can take to exercise subchannel and multi-queue handling.
#[derive(Arbitrary, Debug)]
enum SubchannelAction {
    /// Request subchannel allocation with a specific count.
    RequestSubchannels {
        request: protocol::Message5SubchannelRequest,
    },
    /// Request 1–4 subchannels (within the mock's supported range).
    RequestFewSubchannels { count: u8 },
    /// Request an arbitrary number of subchannels.
    RequestManySubchannels { count: u16 },
    /// Send a raw subchannel message with arbitrary bytes.
    RawSubchannelMessage { payload: Vec<u8> },
    /// Send an indirection table update message.
    SendIndirectionTable {
        /// Fuzzed indirection table entries.
        entries: Vec<u32>,
    },
    /// Send an NVSP control message interleaved with subchannel operations.
    SendControlMessage {
        message_type: u32,
        payload: Vec<u8>,
        with_completion: bool,
    },
    /// Send a data path switch message (may interact with subchannel state).
    SendSwitchDataPath {
        msg: protocol::Message4SwitchDataPath,
    },
    /// Send an RNDIS data packet on the primary channel.
    SendRndisPacket {
        ppi_bytes: Vec<u8>,
        frame_data: Vec<u8>,
    },
    /// Drain the primary queue and open any subchannels the device has
    /// requested via `enable_subchannels` since the last call.
    ///
    /// This is the key action that makes multi-queue code reachable: after a
    /// successful `RequestSubchannels`, drain the queue (allowing the device to
    /// process the request and call `restart_queues`), then call this action
    /// to complete the ring handshake for each new subchannel.
    OpenPendingSubchannels,
    /// Read one completion/notification from a subchannel ring.
    ReadSubchannelCompletion {
        /// Which subchannel to read from (wrapped mod number of open ones).
        idx: u8,
    },
    /// Send an RNDIS data packet on a subchannel ring.
    SendDataToSubchannel {
        /// Which subchannel to send on (wrapped mod number of open ones).
        idx: u8,
        ppi_bytes: Vec<u8>,
        frame_data: Vec<u8>,
    },
    /// Read one completion/notification from the primary channel.
    ReadCompletion,
    /// Drain all pending packets from the primary queue.
    DrainQueue,
    /// Release a receive buffer ID on a subchannel that belongs to a different
    /// queue's range.  This exercises the `send_if_remote()` cross-queue
    /// routing path in `release_recv_buffers()`, which is otherwise
    /// unreachable because the loopback endpoint never creates cross-queue RX
    /// traffic.
    ///
    /// The `buffer_id` is the raw RX buffer sub-allocation index.  When sent
    /// as a completion `transaction_id` on a subchannel whose `RxBufferRange`
    /// does not own that ID, `send_if_remote()` redirects the buffer release
    /// to the correct queue via the cross-queue channel.
    ReleaseRemoteBufferId {
        /// Which subchannel to send the completion on (wrapped mod open).
        subchannel_idx: u8,
        /// Raw buffer ID to release.  Should be outside the target
        /// subchannel's `id_range` to trigger the remote-redirect path.
        buffer_id: u32,
    },
    /// Send an RSS OID SET (`OID_GEN_RECEIVE_SCALE_PARAMETERS`) with a valid
    /// hash key and indirection table via GpaDirect. This exercises the
    /// `oid_set_rss_parameters` code path and, combined with subchannel
    /// allocation, the RSS-driven queue distribution in `restart_queues()`.
    SetRssParameters {
        /// Hash information flags (hash function | hash type). Use Toeplitz
        /// combined with IPv4/TCP/UDP/IPv6 hash types.
        hash_information: u32,
        /// Indirection table entries. Clamped to `max_queues`.
        indirection_entries: Vec<u32>,
        /// RSS parameter flags (e.g. DISABLE_RSS).
        flags: u16,
    },
    /// Send an RSS DISABLE to clear RSS state, then re-enable. Tests the
    /// `NDIS_RSS_PARAM_FLAG_DISABLE_RSS` path.
    DisableRss,
    /// Change the RSS indirection table to redirect traffic to different
    /// queues than the current configuration.  When subchannels are open,
    /// this forces cross-queue buffer redistribution through
    /// `remote_buffer_id_recv` in the main loop, exercising the
    /// `send_if_remote()` path that is otherwise unreachable.
    ChangeRssIndirection {
        /// Seed for building a shuffled indirection table.  Each entry is
        /// forced to a different queue than the previous table so that the
        /// NIC must redistribute RX buffers.
        seed: u32,
    },
}

/// Execute one subchannel fuzz action.
#[allow(clippy::too_many_arguments)]
async fn execute_next_action(
    input: &mut Unstructured<'_>,
    queue: &mut Queue<GpadlRingMem>,
    mem: &GuestMemory,
    next_transaction_id: &mut u64,
    opener: &mut SubchannelOpener,
    open_queues: &mut Vec<Queue<GpadlRingMem>>,
) -> Result<(), anyhow::Error> {
    let action = input.arbitrary::<SubchannelAction>()?;
    fuzz_eprintln!("action: {action:?}");
    match action {
        SubchannelAction::RequestSubchannels { request } => {
            send_inband_nvsp(
                queue,
                next_transaction_id,
                protocol::MESSAGE5_TYPE_SUB_CHANNEL,
                request.as_bytes(),
                true,
            )
            .await?;
        }
        SubchannelAction::RequestFewSubchannels { count } => {
            let request = protocol::Message5SubchannelRequest {
                operation: protocol::SubchannelOperation::ALLOCATE,
                num_sub_channels: ((count % MAX_FUZZ_SUBCHANNELS as u8) + 1) as u32,
            };
            send_inband_nvsp(
                queue,
                next_transaction_id,
                protocol::MESSAGE5_TYPE_SUB_CHANNEL,
                request.as_bytes(),
                true,
            )
            .await?;
        }
        SubchannelAction::RequestManySubchannels { count } => {
            let request = protocol::Message5SubchannelRequest {
                operation: protocol::SubchannelOperation::ALLOCATE,
                num_sub_channels: count as u32,
            };
            send_inband_nvsp(
                queue,
                next_transaction_id,
                protocol::MESSAGE5_TYPE_SUB_CHANNEL,
                request.as_bytes(),
                true,
            )
            .await?;
        }
        SubchannelAction::RawSubchannelMessage { payload } => {
            send_inband_nvsp(
                queue,
                next_transaction_id,
                protocol::MESSAGE5_TYPE_SUB_CHANNEL,
                &payload,
                true,
            )
            .await?;
        }
        SubchannelAction::SendIndirectionTable { entries } => {
            let header = protocol::Message5SendIndirectionTable {
                table_entry_count: entries.len() as u32,
                table_offset: size_of::<protocol::Message5SendIndirectionTable>() as u32,
            };
            let mut payload = Vec::new();
            payload.extend_from_slice(header.as_bytes());
            for entry in &entries {
                payload.extend_from_slice(entry.as_bytes());
            }
            send_inband_nvsp(
                queue,
                next_transaction_id,
                protocol::MESSAGE5_TYPE_SEND_INDIRECTION_TABLE,
                &payload,
                true,
            )
            .await?;
        }
        SubchannelAction::SendControlMessage {
            message_type,
            payload,
            with_completion,
        } => {
            send_inband_nvsp(
                queue,
                next_transaction_id,
                message_type,
                &payload,
                with_completion,
            )
            .await?;
        }
        SubchannelAction::SendSwitchDataPath { msg } => {
            send_inband_nvsp(
                queue,
                next_transaction_id,
                protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH,
                msg.as_bytes(),
                true,
            )
            .await?;
        }
        SubchannelAction::SendRndisPacket {
            ppi_bytes,
            frame_data,
        } => {
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
        SubchannelAction::OpenPendingSubchannels => {
            // Drain the primary queue first so the device can process any
            // outstanding subchannel requests and call enable_subchannels.
            drain_queue(queue);
            let new_queues = opener.open_pending().await;
            open_queues.extend(new_queues);
        }
        SubchannelAction::ReadSubchannelCompletion { idx } => {
            if !open_queues.is_empty() {
                let i = idx as usize % open_queues.len();
                let q = &mut open_queues[i];
                let _ = try_read_one_completion(q);
            }
        }
        SubchannelAction::SendDataToSubchannel {
            idx,
            ppi_bytes,
            frame_data,
        } => {
            if !open_queues.is_empty() {
                let i = idx as usize % open_queues.len();
                let q = &mut open_queues[i];
                let rndis_buf = build_structured_rndis_packet(&ppi_bytes, &frame_data);
                // Reuse the primary layout's data pages for RNDIS content;
                // the subchannel ring sends GpaDirect references into the same
                // guest memory as the primary channel.
                send_rndis_via_direct_path(
                    q,
                    mem,
                    &rndis_buf,
                    protocol::DATA_CHANNEL_TYPE,
                    &LAYOUT,
                    next_transaction_id,
                )
                .await?;
            }
        }
        SubchannelAction::ReadCompletion => {
            let _ = try_read_one_completion(queue);
        }
        SubchannelAction::DrainQueue => {
            drain_queue(queue);
        }
        SubchannelAction::ReleaseRemoteBufferId {
            subchannel_idx,
            buffer_id,
        } => {
            if !open_queues.is_empty() {
                let i = subchannel_idx as usize % open_queues.len();
                let q = &mut open_queues[i];
                // Build an NVSP MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE
                // completion and send it with `transaction_id` set to the
                // target buffer_id. Because the subchannel's RxBufferRange
                // doesn't own this buffer_id, `send_if_remote()` will redirect
                // the buffer release to the owning queue.
                let completion = protocol::Message1SendRndisPacketComplete {
                    status: protocol::Status::SUCCESS,
                };
                let payload = nvsp_payload(
                    protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET_COMPLETE,
                    completion.as_bytes(),
                );
                let (_, mut writer) = q.split();
                writer
                    .write(vmbus_async::queue::OutgoingPacket {
                        transaction_id: buffer_id as u64,
                        packet_type: OutgoingPacketType::Completion,
                        payload: &[&payload],
                    })
                    .await?;
            }
        }
        SubchannelAction::SetRssParameters {
            hash_information,
            indirection_entries,
            flags,
        } => {
            // Max queues = primary + open subchannels.
            let max_queues = 1 + open_queues.len() as u32;
            // Ensure at least 4 indirection entries (RSS requires non-empty).
            let entries: Vec<u32> = if indirection_entries.is_empty() {
                vec![0; 4]
            } else {
                indirection_entries
            };
            let rndis_buf = build_rss_oid_set(hash_information, &entries, max_queues, flags);
            send_rndis_gpadirect(
                queue,
                mem,
                &rndis_buf,
                protocol::CONTROL_CHANNEL_TYPE,
                LAYOUT.data_page_start(),
                LAYOUT.data_pages,
                next_transaction_id,
            )
            .await?;
        }
        SubchannelAction::DisableRss => {
            // Send RSS params with DISABLE flag set.
            let rndis_buf = build_rss_oid_set(
                0, // hash_information (function mask = 0 also triggers disable)
                &[0; 4],
                1,
                rndisprot::NDIS_RSS_PARAM_FLAG_DISABLE_RSS,
            );
            send_rndis_gpadirect(
                queue,
                mem,
                &rndis_buf,
                protocol::CONTROL_CHANNEL_TYPE,
                LAYOUT.data_page_start(),
                LAYOUT.data_pages,
                next_transaction_id,
            )
            .await?;
        }
        SubchannelAction::ChangeRssIndirection { seed } => {
            // Only meaningful when subchannels are open.
            if !open_queues.is_empty() {
                let max_queues = 1 + open_queues.len() as u32;
                // Build an indirection table that maps each entry to a
                // different queue than a simple round-robin.  Use the seed
                // to deterministically permute the mapping so that
                // different fuzz inputs produce different redistribution
                // patterns making cross-queue buffer returns likely.
                let table_size = 16u32; // standard indirection table size
                let entries: Vec<u32> = (0..table_size)
                    .map(|i| {
                        // XOR each index with the seed, then wrap to a
                        // valid queue.  This ensures non-trivial shuffling.
                        (i ^ seed) % max_queues
                    })
                    .collect();

                // Use Toeplitz + IPv4/TCP hash type to enable RSS.
                let hash_info: u32 = 0x0000_0001 // NDIS_HASH_FUNCTION_TOEPLITZ
                    | 0x0000_0100; // NDIS_HASH_IPV4
                let rndis_buf = build_rss_oid_set(hash_info, &entries, max_queues, 0);
                send_rndis_gpadirect(
                    queue,
                    mem,
                    &rndis_buf,
                    protocol::CONTROL_CHANNEL_TYPE,
                    LAYOUT.data_page_start(),
                    LAYOUT.data_pages,
                    next_transaction_id,
                )
                .await?;

                // Yield and drain to let the NIC process the RSS change and
                // redistribute buffers across queues.
                fuzz_helpers::yield_to_executor(10).await;
                drain_queue(queue);
            }
        }
    }
    Ok(())
}

fuzz_target!(|input: &[u8]| {
    run_fuzz_loop_with_config(
        input,
        &LAYOUT,
        FuzzNicConfig {
            max_subchannels: MAX_FUZZ_SUBCHANNELS,
            ..FuzzNicConfig::default()
        },
        |fuzzer_input, setup| {
            Box::pin(async move {
                let mut queue = setup.queue;
                let mem = setup.mem;
                let mut next_transaction_id = 1u64;
                let mut opener = setup
                    .subchannel_opener
                    .expect("SubchannelOpener must be present when max_subchannels > 0");
                let mut open_queues: Vec<Queue<GpadlRingMem>> = Vec::new();

                // Always negotiate to the ready state — subchannel requests are
                // only valid after negotiation to version >= 5.
                negotiate_to_ready(
                    &mut queue,
                    &mut next_transaction_id,
                    setup.recv_buf_gpadl_id,
                    setup.send_buf_gpadl_id,
                )
                .await?;

                // 90% of the time, also initialize RNDIS so that data-path
                // actions on subchannels are meaningful.
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

                // Run subchannel-focused fuzz actions until input is exhausted.
                while !fuzzer_input.is_empty() {
                    execute_next_action(
                        fuzzer_input,
                        &mut queue,
                        &mem,
                        &mut next_transaction_id,
                        &mut opener,
                        &mut open_queues,
                    )
                    .await?;
                    drain_queue_async(&mut queue).await;
                }
                Ok(())
            })
        },
    );
});
