// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzzer for the NetVSP Virtual Function (VF / SR-IOV) state machine.
//!
//! This fuzzer exercises the VF state machine by using a
//! [`FuzzVirtualFunction`] that provides fuzzer-controlled VF ID values and
//! state change signals.
//!
//! The VF state machine has 12+ states with complex transitions, and this
//! fuzzer aims to exercise all reachable state transitions including error
//! paths and unexpected message orderings.
//!
//! ## NVSP protocol messages tested
//!
//! - `MESSAGE4_TYPE_SEND_VF_ASSOCIATION` — VF association completion (guest-to-host
//!   acknowledgement with transaction ID `0x8000000000000000`)
//! - `MESSAGE4_TYPE_SWITCH_DATA_PATH` — data path switch request
//!   (synthetic to VF) and completion (guest-to-host acknowledgement with
//!   transaction ID `0x8000000000000001`)
//! - Arbitrary raw NVSP message types and payloads
//!
//! Indirectly exercised (host-to-guest, triggered by VF state changes):
//! - `MESSAGE4_TYPE_SEND_VF_ASSOCIATION` — host notifying guest of VF
//!   availability
//! - `MESSAGE4_TYPE_SWITCH_DATA_PATH` — host requesting data path switch
//!
//! ## RNDIS protocol messages tested
//!
//! - `MESSAGE_TYPE_PACKET_MSG` — structured RNDIS data packets via GpaDirect
//!   during VF state transitions
//!
//! ## VF state machine transitions exercised
//!
//! - VF ID changes (available-to-unavailable and vice versa)
//! - `VirtualFunction::wait_for_state_change()` notifications

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

mod fuzz_helpers;

use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use fuzz_helpers::DATA_PAGES;
use fuzz_helpers::PageLayout;
use fuzz_helpers::SWITCH_DATA_PATH_TRANSACTION_ID;
use fuzz_helpers::VF_ASSOCIATION_TRANSACTION_ID;
use fuzz_helpers::build_structured_rndis_packet;
use fuzz_helpers::drain_queue;
use fuzz_helpers::drain_queue_async;
use fuzz_helpers::endpoint::FuzzEndpoint;
use fuzz_helpers::endpoint::FuzzEndpointConfig;
use fuzz_helpers::negotiate_to_ready_with_capabilities;
use fuzz_helpers::nic_setup::FuzzNicConfig;
use fuzz_helpers::nvsp_payload;
use fuzz_helpers::rndis_initialize;
use fuzz_helpers::run_fuzz_loop_with_config;
use fuzz_helpers::send_completion_packet;
use fuzz_helpers::send_inband_nvsp;
use fuzz_helpers::send_rndis_via_direct_path;
use fuzz_helpers::try_read_one_completion;
use fuzz_helpers::vf::FuzzVirtualFunction;
use guestmem::GuestMemory;
use mesh::rpc::RpcSend;
use netvsp::protocol;
use vmbus_async::queue::Queue;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use xtask_fuzz::fuzz_eprintln;
use xtask_fuzz::fuzz_target;
use zerocopy::IntoBytes;

const LAYOUT: PageLayout = PageLayout {
    send_buf_pages: 1,
    data_pages: DATA_PAGES,
};

/// Actions the fuzzer can take to exercise VF state transitions.
#[derive(Arbitrary, Debug)]
enum VfAction {
    /// Update the VF ID (simulates VF becoming available or unavailable).
    ChangeVfId {
        /// New VF ID, or None to indicate VF is unavailable.
        id: Option<u32>,
    },
    /// Trigger a VF state change notification.
    TriggerStateChange,
    /// Send a VF association completion from the guest side.
    /// This acknowledges the host's `SEND_VF_ASSOCIATION` message.
    SendVfAssociationCompletion,
    /// Send a switch data path request from the guest (synthetic -to- VF or
    /// VF -to- synthetic).
    SendSwitchDataPath {
        /// Whether to switch to the VF data path (true) or synthetic (false).
        use_vf: bool,
    },
    /// Send a switch data path completion from the guest side.
    /// This acknowledges the host's `SWITCH_DATA_PATH` message.
    SendSwitchDataPathCompletion,
    /// Send a raw NVSP control message to interleave with VF operations.
    SendControlMessage {
        message_type: u32,
        payload: Vec<u8>,
        with_completion: bool,
    },
    /// Send an RNDIS data packet to interleave TX with VF state changes.
    SendRndisPacket {
        ppi_bytes: Vec<u8>,
        frame_data: Vec<u8>,
    },
    /// Read one completion/notification from the host.
    ReadCompletion,
    /// Drain all pending packets from the queue.
    DrainQueue,
    /// Schedule a VF ID change, then immediately trigger a state change. This
    /// tests the combined path where the coordinator processes a new VF ID and
    /// a state change notification in quick succession.
    ChangeIdAndTriggerStateChange { id: Option<u32> },
    /// Full VF lifecycle sequence: set ID → trigger state change → send VF
    /// association completion → switch data path → switch data path completion.
    /// Tests the complete VF bring-up flow in a single action.
    FullVfBringUp { vf_id: u32, use_vf: bool },
    /// Send a VF association completion with a raw/adversarial payload.
    SendRawVfAssociationCompletion { payload: Vec<u8> },
}

/// Execute one VF fuzz action.
async fn execute_next_action(
    input: &mut Unstructured<'_>,
    queue: &mut Queue<GpadlRingMem>,
    mem: &GuestMemory,
    next_transaction_id: &mut u64,
    vf_handles: &fuzz_helpers::vf::FuzzVfHandles,
) -> Result<(), anyhow::Error> {
    let action = input.arbitrary::<VfAction>()?;
    fuzz_eprintln!("action: {action:?}");
    match action {
        VfAction::ChangeVfId { id } => {
            vf_handles.id_send.send(id);
        }
        VfAction::TriggerStateChange => {
            let pending = vf_handles.state_change_send.call(|rpc| rpc, ());
            // Usually wait for completion so the coordinator's RPC handling is
            // exercised; occasionally skip waiting to preserve event pressure.
            if input.ratio(4, 5)? {
                let _ = pending.await;
            }
        }
        VfAction::SendVfAssociationCompletion => {
            let payload = nvsp_payload(protocol::MESSAGE4_TYPE_SEND_VF_ASSOCIATION, &[]);
            send_completion_packet(queue, VF_ASSOCIATION_TRANSACTION_ID, &[&payload]).await?;
        }
        VfAction::SendSwitchDataPath { use_vf } => {
            let msg = protocol::Message4SwitchDataPath {
                active_data_path: if use_vf {
                    protocol::DataPath::VF.0
                } else {
                    protocol::DataPath::SYNTHETIC.0
                },
            };
            send_inband_nvsp(
                queue,
                next_transaction_id,
                protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH,
                msg.as_bytes(),
                true,
            )
            .await?;
        }
        VfAction::SendSwitchDataPathCompletion => {
            let payload = nvsp_payload(protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH, &[]);
            send_completion_packet(queue, SWITCH_DATA_PATH_TRANSACTION_ID, &[&payload]).await?;
        }
        VfAction::SendControlMessage {
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
        VfAction::SendRndisPacket {
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
        VfAction::ReadCompletion => {
            let _ = try_read_one_completion(queue);
        }
        VfAction::DrainQueue => {
            drain_queue(queue);
        }
        VfAction::ChangeIdAndTriggerStateChange { id } => {
            // Set ID first, then trigger state change — exercises the path
            // where wait_for_state_change drains pending ID updates.
            vf_handles.id_send.send(id);
            let pending = vf_handles.state_change_send.call(|rpc| rpc, ());
            let _ = pending.await;
        }
        VfAction::FullVfBringUp { vf_id, use_vf } => {
            // 1. Make VF available.
            vf_handles.id_send.send(Some(vf_id));
            let pending = vf_handles.state_change_send.call(|rpc| rpc, ());
            let _ = pending.await;
            drain_queue(queue);

            // 2. Send VF association completion.
            let payload = nvsp_payload(protocol::MESSAGE4_TYPE_SEND_VF_ASSOCIATION, &[]);
            send_completion_packet(queue, VF_ASSOCIATION_TRANSACTION_ID, &[&payload]).await?;
            drain_queue(queue);

            // 3. Switch data path.
            let msg = protocol::Message4SwitchDataPath {
                active_data_path: if use_vf {
                    protocol::DataPath::VF.0
                } else {
                    protocol::DataPath::SYNTHETIC.0
                },
            };
            send_inband_nvsp(
                queue,
                next_transaction_id,
                protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH,
                msg.as_bytes(),
                true,
            )
            .await?;
            drain_queue(queue);

            // 4. Send switch data path completion.
            let payload = nvsp_payload(protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH, &[]);
            send_completion_packet(queue, SWITCH_DATA_PATH_TRANSACTION_ID, &[&payload]).await?;
            drain_queue(queue);
        }
        VfAction::SendRawVfAssociationCompletion { payload } => {
            send_completion_packet(queue, VF_ASSOCIATION_TRANSACTION_ID, &[&payload]).await?;
        }
    }
    Ok(())
}

fuzz_target!(|input: &[u8]| {
    // Create a FuzzVirtualFunction with no initial VF ID — VF presence is
    // signaled later via ChangeVfId actions, after protocol negotiation.
    // Starting with None avoids the NIC trying to send VF_ASSOCIATION
    // during startup before the guest has negotiated.
    let (vf, vf_handles) = FuzzVirtualFunction::new(None);

    // Fuzz-select whether VF switch should fail, exercising both
    // DataPathSynthetic (failure) and DataPathSwitched (success) states.
    // Use the last byte of input to avoid consuming bytes from the main
    // Unstructured stream.
    let fail_vf = input.last().is_some_and(|b| b % 4 == 0);

    let (endpoint, _handles) = FuzzEndpoint::new(FuzzEndpointConfig {
        enable_rx_injection: false,
        enable_action_injection: false,
        fail_vf_switch: fail_vf,
        ..FuzzEndpointConfig::default()
    });

    let config = FuzzNicConfig {
        endpoint: Box::new(endpoint),
        virtual_function: Some(Box::new(vf)),
        ..FuzzNicConfig::default()
    };

    run_fuzz_loop_with_config(input, &LAYOUT, config, |fuzzer_input, setup| {
        Box::pin(async move {
            let mut queue = setup.queue;
            let mem = setup.mem;
            let mut next_transaction_id = 1u64;

            // Always negotiate to the ready state with SR-IOV enabled — VF
            // messages are only sent when the guest advertises sriov support.
            negotiate_to_ready_with_capabilities(
                &mut queue,
                &mut next_transaction_id,
                setup.recv_buf_gpadl_id,
                setup.send_buf_gpadl_id,
                protocol::NdisConfigCapabilities::new().with_sriov(true),
            )
            .await?;

            // 90% of the time, also initialize RNDIS.
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

            // Run VF-focused fuzz actions until input is exhausted.
            while !fuzzer_input.is_empty() {
                execute_next_action(
                    fuzzer_input,
                    &mut queue,
                    &mem,
                    &mut next_transaction_id,
                    &vf_handles,
                )
                .await?;
                drain_queue_async(&mut queue).await;
            }

            // Explicitly drop the VF handles so their sender channels close.
            // This allows FuzzVirtualFunction::wait_for_state_change() to
            // pend forever, which lets the NIC's coordinator observe the
            // stop signal from channel revoke instead of busy-looping.
            drop(vf_handles);

            Ok(())
        })
    });
});
