// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzzer for the netvsp save/restore code path.
//!
//! The `save()` / `restore()` implementation processes data from the
//! host/root — a trust boundary in OpenHCL — and includes complex state-
//! machine reconstruction for VF state, pending TX completions, RSS config,
//! and GPADL remapping.
//!
//! ## Two complementary modes
//!
//! - **`ArbitraryRestore`**: construct a completely arbitrary
//!   [`saved_state::SavedState`], encode it, and call `restore()` without any
//!   prior live negotiation.  Exercises all error branches with structurally
//!   valid but semantically impossible states (unknown GPADL IDs, unsupported
//!   versions, invalid VF state, oversized channel lists, …).
//!
//! - **`SnapshotMutate`**: negotiate to the Ready state, take a live snapshot
//!   via `save()`, parse the snapshot into a `SavedState`, apply fuzzer-driven
//!   *structural* mutations (which keep GPADL IDs valid so `restore_state()`
//!   proceeds past the GPADL-lookup checks), then call `restore()`.  Exercises
//!   the deeper state-machine reconstruction logic where most of the complex
//!   code lives.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

mod fuzz_helpers;

use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use fuzz_helpers::DATA_PAGES;
use fuzz_helpers::FuzzGuestOsId;
use fuzz_helpers::PageLayout;
use fuzz_helpers::RingFullError;
use fuzz_helpers::build_rndis_oid_query;
use fuzz_helpers::build_rndis_oid_set;
use fuzz_helpers::build_structured_rndis_packet;
use fuzz_helpers::drain_queue_async;
use fuzz_helpers::negotiate_to_ready;
use fuzz_helpers::nic_setup::FuzzNicConfig;
use fuzz_helpers::nic_setup::FuzzNicSetup;
use fuzz_helpers::nic_setup::NicSetupHandle;
use fuzz_helpers::nic_setup::create_nic_with_channel;
use fuzz_helpers::send_inband_nvsp;
use fuzz_helpers::send_rndis_gpadirect;
use fuzz_helpers::send_rndis_via_direct_path;
use guestmem::GuestMemory;
use netvsp::protocol;
use netvsp::rndisprot;
use netvsp::saved_state;
use vmbus_async::queue::Queue;
use vmbus_channel::gpadl::GpadlId;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmcore::save_restore::SavedStateBlob;
use xtask_fuzz::fuzz_eprintln;
use xtask_fuzz::fuzz_target;

const LAYOUT: PageLayout = PageLayout {
    send_buf_pages: 1,
    data_pages: DATA_PAGES,
};

/// A fuzzer-driven structural mutation applied to a live `ReadyPrimary`
/// snapshot. Each mutation keeps the recv/send GPADL IDs intact so that
/// `restore_state()` proceeds past the GPADL-lookup checks and into deeper
/// state-machine reconstruction code — exactly the code that runs in OpenHCL
/// when restoring from a host-supplied blob.
#[derive(Arbitrary, Debug)]
enum SnapshotMutation {
    /// Replace all channel entries with fuzzer-supplied ones. Tests pending TX
    /// completion draining and in-use RX buffer registration on restore.
    SetChannels(Vec<Option<saved_state::Channel>>),
    /// Overwrite the guest VF state. Tests the VF state-machine
    /// reconstruction path including `DataPathSwitchPending`.
    SetGuestVfState(saved_state::GuestVfState),
    /// Overwrite the RNDIS state.
    SetRndisState(saved_state::RndisState),
    /// Overwrite the RSS state. Tests indirection-table parsing.
    SetRssState(Option<saved_state::RssState>),
    /// Overwrite the pending control messages. Tests the control-message
    /// replay path on restore.
    SetControlMessages(Vec<saved_state::IncomingControlMessage>),
    /// Overwrite the NDIS version.
    SetNdisVersion(saved_state::NdisVersion),
    /// Overwrite the NDIS config (MTU, capabilities).
    SetNdisConfig(saved_state::NdisConfig),
    /// Overwrite the offload config.
    SetOffloadConfig(saved_state::OffloadConfig),
    /// Toggle `pending_offload_change`.
    SetPendingOffloadChange(bool),
    /// Toggle `guest_link_down`.
    SetGuestLinkDown(bool),
    /// Set the pending link action.
    SetPendingLinkAction(Option<bool>),
    /// Set the packet filter.
    SetPacketFilter(Option<u32>),
    /// Replace the entire `ReadyPrimary`. The recv/send GPADL IDs are patched
    /// with the real values afterwards so that GPADL lookup succeeds.
    ReplaceReady(Box<saved_state::ReadyPrimary>),
    /// Overwrite the protocol version. Tests version-mismatch handling on
    /// restore (e.g. V2 state restored with V5 version, or completely
    /// invalid version numbers).
    SetVersion(u32),
    /// Toggle `tx_spread_sent`.
    SetTxSpreadSent(bool),
    /// Overwrite the receive buffer sub_allocation_size. Tests the
    /// sub-allocation size validation path on restore.
    SetReceiveBufferSubAllocationSize(u32),
    /// Overwrite the send buffer with an arbitrary (or None) value. Tests
    /// the send-buffer-optional path on restore.
    SetSendBuffer(Option<saved_state::SendBuffer>),
}

/// Apply a single [`SnapshotMutation`] to the `ReadyPrimary` inside `state`.
/// The `recv_gpadl` and `send_gpadl` values are the real IDs from the fuzz
/// setup; they are used to fix up GPADL references after mutations that
/// replace the struct wholesale.
fn apply_snapshot_mutation(
    state: &mut saved_state::SavedState,
    mutation: SnapshotMutation,
    recv_gpadl: GpadlId,
    send_gpadl: GpadlId,
) {
    let ready = match state.open.as_mut().and_then(|o| {
        if let saved_state::Primary::Ready(r) = &mut o.primary {
            Some(r)
        } else {
            None
        }
    }) {
        Some(r) => r,
        None => return,
    };

    match mutation {
        SnapshotMutation::SetChannels(ch) => ready.channels = ch,
        SnapshotMutation::SetGuestVfState(s) => ready.guest_vf_state = s,
        SnapshotMutation::SetRndisState(s) => ready.rndis_state = s,
        SnapshotMutation::SetRssState(s) => ready.rss_state = s,
        SnapshotMutation::SetControlMessages(m) => ready.control_messages = m,
        SnapshotMutation::SetNdisVersion(v) => ready.ndis_version = v,
        SnapshotMutation::SetNdisConfig(c) => ready.ndis_config = c,
        SnapshotMutation::SetOffloadConfig(o) => ready.offload_config = o,
        SnapshotMutation::SetPendingOffloadChange(v) => ready.pending_offload_change = v,
        SnapshotMutation::SetGuestLinkDown(v) => ready.guest_link_down = v,
        SnapshotMutation::SetPendingLinkAction(v) => ready.pending_link_action = v,
        SnapshotMutation::SetPacketFilter(v) => ready.packet_filter = v,
        SnapshotMutation::ReplaceReady(mut new_ready) => {
            // Preserve real GPADL IDs so restore_state can look them up.
            new_ready.receive_buffer.gpadl_id = recv_gpadl;
            if let Some(sb) = &mut new_ready.send_buffer {
                sb.gpadl_id = send_gpadl;
            }
            *ready = *new_ready;
        }
        SnapshotMutation::SetVersion(v) => ready.version = v,
        SnapshotMutation::SetTxSpreadSent(v) => ready.tx_spread_sent = v,
        SnapshotMutation::SetReceiveBufferSubAllocationSize(v) => {
            ready.receive_buffer.sub_allocation_size = v;
        }
        SnapshotMutation::SetSendBuffer(sb) => {
            // Patch GPADL ID if present so GPADL lookup can succeed.
            let mut sb = sb;
            if let Some(ref mut s) = sb {
                s.gpadl_id = send_gpadl;
            }
            ready.send_buffer = sb;
        }
    }
}

/// Actions that can be taken after a successful restore + start to validate
/// that the restored state machine handles real traffic correctly.
#[derive(Arbitrary, Debug)]
enum PostRestoreAction {
    /// Send an RNDIS data packet to exercise the TX path with restored state.
    SendRndisPacket {
        ppi_bytes: Vec<u8>,
        frame_data: Vec<u8>,
    },
    /// Send an RNDIS OID query to validate restored OID handling state.
    SendOidQuery { oid: u32 },
    /// Send an RNDIS OID set to validate restored offload/config state.
    SendOidSet { oid: u32, value: Vec<u8> },
    /// Send an NVSP control message post-restore.
    SendControlMessage { message_type: u32, payload: Vec<u8> },
    /// Drain the queue to process any pending completions.
    DrainQueue,
}

/// Two complementary fuzzing modes for `save()` / `restore()`.
#[derive(Arbitrary, Debug)]
enum FuzzMode {
    /// Entirely arbitrary `SavedState`: no prior negotiation, immediate
    /// `restore()`. Reaches all error-handling branches (unknown GPADL IDs,
    /// unsupported versions, absurd channel counts, …).
    ArbitraryRestore {
        state: saved_state::SavedState,
        /// When `true`, patch the GPADL IDs in `state` to the real values
        /// from the fuzz setup before calling `restore()`. This lets GPADL
        /// lookup succeed so `restore_state()` proceeds further into the
        /// `ReadyPrimary` (or `Init`) reconstruction code rather than
        /// returning early on a lookup failure.
        use_real_gpadl_ids: bool,
    },
    /// Live snapshot then field-level mutation before `restore()`. Exercises
    /// the deeper state-machine reconstruction logic with structurally valid
    /// protobuf that still contains semantically impossible state.
    SnapshotMutate {
        mutations: Vec<SnapshotMutation>,
        /// If `true` and `restore()` succeeds, call `start()` and drain the
        /// queue to exercise the restarted state machine.
        restart_after_restore: bool,
        /// Actions to run after a successful restore + start. Exercises the
        /// restored state machine with real NVSP/RNDIS traffic rather than
        /// just a passive drain.
        post_restore_actions: Vec<PostRestoreAction>,
    },
}

fuzz_target!(|input: &[u8]| {
    xtask_fuzz::init_tracing_if_repro();
    let mut u = Unstructured::new(input);

    // Fuzz-select the guest OS identity so this target also covers all
    // `can_use_ring_opt` branches.
    let mut config = FuzzNicConfig::default();
    if let Ok(fuzz_os) = u.arbitrary::<FuzzGuestOsId>() {
        config.get_guest_os_id = fuzz_os.to_hv_guest_os_id();
    }

    pal_async::DefaultPool::run_with(async |driver| {
        // Build the NIC internals but do NOT start the channel yet; the fuzz
        // mode may call restore() before start(), so we control sequencing.
        let (handle, setup) = match create_nic_with_channel(&driver, &LAYOUT, config).await {
            Ok(pair) => pair,
            Err(_) => return,
        };

        let mode = match u.arbitrary::<FuzzMode>() {
            Ok(m) => m,
            Err(_) => {
                handle.cleanup().await;
                return;
            }
        };

        let fuzz_result = mesh::CancelContext::new()
            .with_timeout(std::time::Duration::from_millis(500))
            .until_cancelled(run_mode(&mut u, handle, setup, mode))
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
    });
});

async fn run_mode(
    u: &mut Unstructured<'_>,
    handle: NicSetupHandle,
    setup: FuzzNicSetup,
    mode: FuzzMode,
) -> anyhow::Result<()> {
    let FuzzNicSetup {
        mut queue,
        mem,
        recv_buf_gpadl_id,
        send_buf_gpadl_id,
        ..
    } = setup;

    match mode {
        FuzzMode::ArbitraryRestore {
            mut state,
            use_real_gpadl_ids,
        } => {
            // Optionally patch GPADL IDs so the restore proceeds into deeper
            // branches rather than failing on GPADL lookup.
            if use_real_gpadl_ids {
                patch_state_gpadl_ids(&mut state, recv_buf_gpadl_id, send_buf_gpadl_id);
            }

            let blob = SavedStateBlob::new(state);

            // The channel is in "not started" state — restore() can be called
            // directly without stop() because the device task hasn't started.
            // Must not panic regardless of whether restore returns Ok or Err.
            let _ = handle.channel.restore(blob).await;
        }

        FuzzMode::SnapshotMutate {
            mutations,
            restart_after_restore,
            post_restore_actions,
        } => {
            // Advance to Ready so save() captures a meaningful blob.
            let mut tid = 1u64;
            handle.channel.start();
            negotiate_to_ready(&mut queue, &mut tid, recv_buf_gpadl_id, send_buf_gpadl_id).await?;

            // Initialize RNDIS to reach Operational state — this ensures the
            // saved state captures richer state (offload config, RNDIS
            // initialized flag, pending control messages, etc.) rather than
            // just the NVSP negotiation state.
            fuzz_helpers::rndis_initialize(
                &mut queue,
                &mem,
                LAYOUT.data_page_start(),
                LAYOUT.data_pages,
                &mut tid,
            )
            .await?;

            // Send a TX packet to exercise the data path before saving, so
            // the saved state may contain in-flight TX completion state.
            let rndis_buf = build_structured_rndis_packet(&[], &[0xAA; 64]);
            send_rndis_via_direct_path(
                &mut queue,
                &mem,
                &rndis_buf,
                protocol::DATA_CHANNEL_TYPE,
                &LAYOUT,
                &mut tid,
            )
            .await?;

            drain_queue_async(&mut queue).await;

            // Stop the channel before save/restore.
            handle.channel.stop().await;

            if let Some(blob) = handle.channel.save().await? {
                // Parse the snapshot, apply structural mutations, re-encode.
                if let Ok(mut state) = blob.parse::<saved_state::SavedState>() {
                    for mutation in mutations {
                        apply_snapshot_mutation(
                            &mut state,
                            mutation,
                            recv_buf_gpadl_id,
                            send_buf_gpadl_id,
                        );
                    }
                    let mutated_blob = SavedStateBlob::new(state);

                    // Restore from the mutated blob. Must not panic.
                    let restore_ok = handle.channel.restore(mutated_blob).await.is_ok();

                    if restore_ok && restart_after_restore {
                        handle.channel.start();
                        // Yield so the coordinator/worker tasks can restart.
                        fuzz_helpers::yield_to_executor(20).await;
                        // Brief drain to exercise the restarted state machine.
                        drain_queue_async(&mut queue).await;

                        // Run post-restore actions to validate that the
                        // restored state machine handles real NVSP/RNDIS
                        // traffic correctly, not just a passive drain.
                        for action in post_restore_actions {
                            execute_post_restore_action(&action, &mut queue, &mem, &mut tid)
                                .await?;
                        }

                        handle.channel.stop().await;
                    }
                }
            }
        }
    }

    handle.cleanup().await;

    // Suppress unused-variable warning when `u` has nothing left to consume.
    let _ = u;
    Ok(())
}

/// Execute a single post-restore action against the restarted NIC.
///
/// These actions validate that the restored state machine handles real
/// NVSP/RNDIS protocol messages correctly — exercising paths like TX
/// processing with restored offload config, OID handling with restored RSS
/// state, and control messages with restored VF/link state.
async fn execute_post_restore_action(
    action: &PostRestoreAction,
    queue: &mut Queue<GpadlRingMem>,
    mem: &GuestMemory,
    tid: &mut u64,
) -> anyhow::Result<()> {
    fuzz_eprintln!("action: {action:?}");
    match action {
        PostRestoreAction::SendRndisPacket {
            ppi_bytes,
            frame_data,
        } => {
            let rndis_buf = build_structured_rndis_packet(ppi_bytes, frame_data);
            send_rndis_via_direct_path(
                queue,
                mem,
                &rndis_buf,
                protocol::DATA_CHANNEL_TYPE,
                &LAYOUT,
                tid,
            )
            .await?;
            drain_queue_async(queue).await;
        }
        PostRestoreAction::SendOidQuery { oid } => {
            let rndis_buf = build_rndis_oid_query(rndisprot::Oid(*oid), &[]);
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
            drain_queue_async(queue).await;
        }
        PostRestoreAction::SendOidSet { oid, value } => {
            let rndis_buf = build_rndis_oid_set(rndisprot::Oid(*oid), value);
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
            drain_queue_async(queue).await;
        }
        PostRestoreAction::SendControlMessage {
            message_type,
            payload,
        } => {
            send_inband_nvsp(queue, tid, *message_type, payload, true).await?;
            drain_queue_async(queue).await;
        }
        PostRestoreAction::DrainQueue => {
            drain_queue_async(queue).await;
        }
    }
    Ok(())
}

/// Patch all GPADL ID fields in a `SavedState` to use the real recv/send
/// buffer GPADL IDs from the fuzz setup.  This is needed for
/// `ArbitraryRestore` mode when the fuzzer wants `restore_state()` to succeed
/// at GPADL lookup and proceed into deeper state-machine reconstruction.
fn patch_state_gpadl_ids(state: &mut saved_state::SavedState, recv: GpadlId, send: GpadlId) {
    let primary = match state.open.as_mut().map(|o| &mut o.primary) {
        Some(p) => p,
        None => return,
    };
    match primary {
        saved_state::Primary::Ready(r) => {
            r.receive_buffer.gpadl_id = recv;
            if let Some(sb) = &mut r.send_buffer {
                sb.gpadl_id = send;
            }
        }
        saved_state::Primary::Init(i) => {
            if let Some(rb) = &mut i.receive_buffer {
                rb.gpadl_id = recv;
            }
            if let Some(sb) = &mut i.send_buffer {
                sb.gpadl_id = send;
            }
        }
        saved_state::Primary::Version => {}
    }
}
