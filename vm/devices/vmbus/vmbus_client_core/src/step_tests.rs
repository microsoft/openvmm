// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

extern crate std;

use super::*;
use alloc::vec::Vec;

/// Scripted [`ActionSink`] used by the unit tests: records every
/// emitted [`Action`] into a `Vec` so assertions can inspect the
/// exact sequence.
#[derive(Default)]
struct Recording {
    actions: Vec<Action>,
}

impl ActionSink for Recording {
    fn emit(&mut self, action: Action) {
        self.actions.push(action);
    }
}

fn make_config() -> Config {
    Config {
        sint: vmbus_core::VMBUS_SINT,
        vtl: 0,
        supported_versions: &[Version::Copper],
        supported_feature_flags: FeatureFlags::new(),
    }
}

#[test]
fn new_starts_disconnected_and_not_running() {
    let core = ClientCore::new(make_config());
    assert!(matches!(core.phase(), ClientPhase::Disconnected));
    assert!(!core.running());
    assert!(!core.host_busy());
    assert!(core.channels().is_empty());
}

#[test]
fn start_toggles_running() {
    let mut core = ClientCore::new(make_config());
    let mut sink = Recording::default();
    core.step(Event::Start, &mut sink);
    assert!(core.running());
    assert!(sink.actions.is_empty());
}

#[test]
fn stop_clears_running() {
    let mut core = ClientCore::new(make_config());
    let mut sink = Recording::default();
    core.step(Event::Start, &mut sink);
    core.step(Event::Stop, &mut sink);
    assert!(!core.running());
}

#[test]
fn host_busy_records_backpressure() {
    let mut core = ClientCore::new(make_config());
    let mut sink = Recording::default();
    core.step(Event::HostBusy { busy: true }, &mut sink);
    assert!(core.host_busy());
    core.step(Event::HostBusy { busy: false }, &mut sink);
    assert!(!core.host_busy());
}

#[test]
fn reset_returns_to_initial_state_but_preserves_running() {
    let mut core = ClientCore::new(make_config());
    let mut sink = Recording::default();
    core.step(Event::Start, &mut sink);
    core.step(Event::HostBusy { busy: true }, &mut sink);
    core.step(Event::Reset, &mut sink);
    assert!(matches!(core.phase(), ClientPhase::Disconnected));
    assert!(!core.host_busy());
    // Reset deliberately preserves running (see doc comment on
    // Event::Reset handling in `step`).
    assert!(core.running());
}

#[test]
fn host_message_with_truncated_header_does_not_panic() {
    // Regression guard: any parse failure on host bytes must be
    // swallowed, not panic. VMBus is a trust boundary.
    let mut core = ClientCore::new(make_config());
    let mut sink = Recording::default();
    core.step(Event::HostMessage(&[0u8; 2]), &mut sink);
    core.step(Event::HostMessage(&[]), &mut sink);
    // Any garbage.
    core.step(Event::HostMessage(&[0xff; 64]), &mut sink);
    assert!(sink.actions.is_empty());
    assert!(matches!(core.phase(), ClientPhase::Disconnected));
    assert_eq!(core.malformed_host_message_count(), 3);
}

// -- Phase 4b-ii: Connect + version-response ladder --------------

/// Multi-version fixture so the fallthrough ladder can be
/// exercised. Ordered lowest → highest, matching the workspace
/// convention that `.last()` is the preferred (highest) version.
fn make_multi_version_config() -> Config {
    Config {
        sint: vmbus_core::VMBUS_SINT,
        vtl: 0,
        supported_versions: &[Version::Iron, Version::Copper],
        supported_feature_flags: FeatureFlags::new().with_modify_connection(true),
    }
}

fn connect_params() -> ConnectParams {
    ConnectParams {
        target_message_vp: 0,
        monitor_page: None,
        client_id: Guid::ZERO,
    }
}

/// Wrap a wire-typed message in the `MessageHeader` framing the
/// core's `dispatch_host_message` expects.
fn make_host_message<T>(msg: &T) -> Vec<u8>
where
    T: zerocopy::IntoBytes
        + zerocopy::Immutable
        + zerocopy::KnownLayout
        + vmbus_core::protocol::VmbusMessage,
{
    vmbus_core::OutgoingMessage::new(msg).data().to_vec()
}

/// Extract wire bytes from a captured `Action::PostMessage`.
fn expect_post(action: &Action) -> &[u8] {
    match action {
        Action::PostMessage(bytes) => bytes.as_slice(),
        other => panic!("expected PostMessage, got {other:?}"),
    }
}

#[test]
fn connect_posts_initiate_contact_and_transitions_to_connecting() {
    let mut core = ClientCore::new(make_config());
    let mut sink = Recording::default();
    core.step(
        Event::Connect {
            request_id: RequestId(7),
            params: connect_params(),
        },
        &mut sink,
    );
    // Exactly one PostMessage; no Complete yet.
    assert_eq!(sink.actions.len(), 1);
    let bytes = expect_post(&sink.actions[0]);
    // The wire message must at least contain the message header
    // and one InitiateContact2 struct.
    assert!(bytes.len() >= 8);
    // Phase transitioned; RequestId is tracked as outstanding.
    assert!(matches!(
        core.phase(),
        ClientPhase::Connecting {
            version: Version::Copper,
            request_id: RequestId(7),
            ..
        }
    ));
}

#[test]
fn connect_from_non_disconnected_phase_completes_with_invalid_state() {
    let mut core = ClientCore::new(make_config());
    let mut sink = Recording::default();
    // First connect: legitimate.
    core.step(
        Event::Connect {
            request_id: RequestId(1),
            params: connect_params(),
        },
        &mut sink,
    );
    sink.actions.clear();
    // Second connect while still Connecting: rejected.
    core.step(
        Event::Connect {
            request_id: RequestId(2),
            params: connect_params(),
        },
        &mut sink,
    );
    assert_eq!(sink.actions.len(), 1);
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id: RequestId(2),
            result: CompletionResult::Connect(Err(ConnectError::InvalidState)),
        }
    ));
}

#[test]
fn successful_version_response_transitions_to_connected() {
    let mut core = ClientCore::new(make_multi_version_config());
    let mut sink = Recording::default();
    core.step(
        Event::Connect {
            request_id: RequestId(1),
            params: connect_params(),
        },
        &mut sink,
    );
    sink.actions.clear();

    // Fake a successful VersionResponse2 from the host.
    let response = vmbus_core::protocol::VersionResponse2 {
        version_response: vmbus_core::protocol::VersionResponse {
            version_supported: 1,
            connection_state: vmbus_core::protocol::ConnectionState::SUCCESSFUL,
            padding: 0,
            selected_version_or_connection_id: 1,
        },
        // Modify-connection is in Config; the host offers all
        // Copper flags. Intersection selects modify_connection.
        supported_features: FeatureFlags::new().with_modify_connection(true).into(),
    };
    let wire = make_host_message(&response);
    core.step(Event::HostMessage(&wire), &mut sink);

    // Exactly one Complete, no further posts (phase 4b-ii scope).
    assert_eq!(sink.actions.len(), 1);
    let Action::Complete {
        request_id: RequestId(1),
        result: CompletionResult::Connect(Ok(ConnectionSuccess { version })),
    } = sink.actions[0]
    else {
        panic!("unexpected: {:?}", sink.actions[0]);
    };
    assert_eq!(version.version, Version::Copper);
    assert!(version.feature_flags.modify_connection());
    assert!(matches!(core.phase(), ClientPhase::Connected { .. }));
}

#[test]
fn feature_flags_are_intersected_with_config() {
    // Config only supports modify_connection; host offers every
    // Copper flag. Result must be exactly modify_connection.
    let mut core = ClientCore::new(make_multi_version_config());
    let mut sink = Recording::default();
    core.step(
        Event::Connect {
            request_id: RequestId(1),
            params: connect_params(),
        },
        &mut sink,
    );
    sink.actions.clear();
    let response = vmbus_core::protocol::VersionResponse2 {
        version_response: vmbus_core::protocol::VersionResponse {
            version_supported: 1,
            connection_state: vmbus_core::protocol::ConnectionState::SUCCESSFUL,
            padding: 0,
            selected_version_or_connection_id: 1,
        },
        supported_features: 0xffff_ffff,
    };
    let wire = make_host_message(&response);
    core.step(Event::HostMessage(&wire), &mut sink);
    let ClientPhase::Connected { version } = *core.phase() else {
        panic!();
    };
    assert!(version.feature_flags.modify_connection());
    // Anything not in the config must be cleared.
    assert!(!version.feature_flags.guest_specified_signal_parameters());
}

#[test]
fn host_reject_walks_ladder_and_reposts() {
    let mut core = ClientCore::new(make_multi_version_config());
    let mut sink = Recording::default();
    core.step(
        Event::Connect {
            request_id: RequestId(1),
            params: connect_params(),
        },
        &mut sink as &mut dyn ActionSink,
    );
    // Initial PostMessage for Copper.
    assert_eq!(sink.actions.len(), 1);
    sink.actions.clear();

    // Host says version not supported (v_supported = 0).
    let response = vmbus_core::protocol::VersionResponse2 {
        version_response: vmbus_core::protocol::VersionResponse {
            version_supported: 0,
            connection_state: vmbus_core::protocol::ConnectionState::SUCCESSFUL,
            padding: 0,
            selected_version_or_connection_id: 0,
        },
        supported_features: 0,
    };
    let wire = make_host_message(&response);
    core.step(Event::HostMessage(&wire), &mut sink);

    // Ladder walks to Iron: one more PostMessage, no Complete.
    assert_eq!(sink.actions.len(), 1);
    let _ = expect_post(&sink.actions[0]);
    assert!(matches!(
        core.phase(),
        ClientPhase::Connecting {
            version: Version::Iron,
            ..
        }
    ));
}

#[test]
fn ladder_bottoms_out_with_version_not_supported() {
    let mut core = ClientCore::new(make_config()); // Only Copper.
    let mut sink = Recording::default();
    core.step(
        Event::Connect {
            request_id: RequestId(3),
            params: connect_params(),
        },
        &mut sink,
    );
    sink.actions.clear();

    // Host rejects the only version.
    let response = vmbus_core::protocol::VersionResponse2 {
        version_response: vmbus_core::protocol::VersionResponse {
            version_supported: 0,
            connection_state: vmbus_core::protocol::ConnectionState::SUCCESSFUL,
            padding: 0,
            selected_version_or_connection_id: 0,
        },
        supported_features: 0,
    };
    let wire = make_host_message(&response);
    core.step(Event::HostMessage(&wire), &mut sink);

    assert_eq!(sink.actions.len(), 1);
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id: RequestId(3),
            result: CompletionResult::Connect(Err(ConnectError::VersionNotSupported)),
        }
    ));
    assert!(matches!(core.phase(), ClientPhase::Disconnected));
}

#[test]
fn failed_connection_state_completes_with_failed_to_connect() {
    let mut core = ClientCore::new(make_config());
    let mut sink = Recording::default();
    core.step(
        Event::Connect {
            request_id: RequestId(9),
            params: connect_params(),
        },
        &mut sink,
    );
    sink.actions.clear();

    let response = vmbus_core::protocol::VersionResponse2 {
        version_response: vmbus_core::protocol::VersionResponse {
            version_supported: 1,
            connection_state: vmbus_core::protocol::ConnectionState::FAILED_UNKNOWN_FAILURE,
            padding: 0,
            selected_version_or_connection_id: 0,
        },
        supported_features: 0,
    };
    let wire = make_host_message(&response);
    core.step(Event::HostMessage(&wire), &mut sink);

    assert_eq!(sink.actions.len(), 1);
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id: RequestId(9),
            result: CompletionResult::Connect(Err(ConnectError::FailedToConnect(_))),
        }
    ));
    assert!(matches!(core.phase(), ClientPhase::Disconnected));
}

#[test]
fn stray_version_response_in_disconnected_phase_is_dropped() {
    // Regression guard: host wire input arriving in the wrong
    // phase must never panic or emit spurious actions.
    let mut core = ClientCore::new(make_config());
    let mut sink = Recording::default();
    let response = vmbus_core::protocol::VersionResponse2 {
        version_response: vmbus_core::protocol::VersionResponse {
            version_supported: 1,
            connection_state: vmbus_core::protocol::ConnectionState::SUCCESSFUL,
            padding: 0,
            selected_version_or_connection_id: 1,
        },
        supported_features: 0,
    };
    let wire = make_host_message(&response);
    core.step(Event::HostMessage(&wire), &mut sink);
    assert!(sink.actions.is_empty());
    assert!(matches!(core.phase(), ClientPhase::Disconnected));
}

// -- Phase 4b-iii-a: offer bookkeeping --------------------------

/// Advance `core` all the way to [`ClientPhase::Connected`] so a
/// per-channel test can start from a stable base.
fn connect_to_copper(core: &mut ClientCore, sink: &mut Recording) {
    core.step(
        Event::Connect {
            request_id: RequestId(1),
            params: connect_params(),
        },
        sink,
    );
    let response = vmbus_core::protocol::VersionResponse2 {
        version_response: vmbus_core::protocol::VersionResponse {
            version_supported: 1,
            connection_state: vmbus_core::protocol::ConnectionState::SUCCESSFUL,
            padding: 0,
            selected_version_or_connection_id: 1,
        },
        supported_features: 0,
    };
    let wire = make_host_message(&response);
    core.step(Event::HostMessage(&wire), sink);
    sink.actions.clear();
    assert!(matches!(core.phase(), ClientPhase::Connected { .. }));
}

fn make_offer(id: u32) -> vmbus_core::protocol::OfferChannel {
    vmbus_core::protocol::OfferChannel {
        interface_id: Guid::ZERO,
        instance_id: Guid::ZERO,
        rsvd: [0; 4],
        flags: vmbus_core::protocol::OfferFlags::new(),
        mmio_megabytes: 0,
        user_defined: vmbus_core::protocol::UserDefinedData::default(),
        subchannel_index: 0,
        mmio_megabytes_optional: 0,
        channel_id: vmbus_core::protocol::ChannelId(id),
        monitor_id: 0,
        monitor_allocated: 0,
        is_dedicated: 0,
        connection_id: 0,
    }
}

#[test]
fn request_offers_from_wrong_phase_is_rejected() {
    let mut core = ClientCore::new(make_config());
    let mut sink = Recording::default();
    core.step(
        Event::RequestOffers {
            request_id: RequestId(5),
        },
        &mut sink,
    );
    assert_eq!(sink.actions.len(), 1);
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id: RequestId(5),
            result: CompletionResult::RequestOffers(Err(ConnectError::InvalidState)),
        }
    ));
}

#[test]
fn request_offers_posts_and_transitions_to_requesting_offers() {
    let mut core = ClientCore::new(make_config());
    let mut sink = Recording::default();
    connect_to_copper(&mut core, &mut sink);
    core.step(
        Event::RequestOffers {
            request_id: RequestId(42),
        },
        &mut sink,
    );
    assert_eq!(sink.actions.len(), 1);
    let _ = expect_post(&sink.actions[0]);
    assert!(matches!(
        core.phase(),
        ClientPhase::RequestingOffers {
            request_id: RequestId(42),
            offer_count: 0,
            ..
        }
    ));
}

#[test]
fn offer_channel_is_recorded_and_forwarded() {
    let mut core = ClientCore::new(make_config());
    let mut sink = Recording::default();
    connect_to_copper(&mut core, &mut sink);
    core.step(
        Event::RequestOffers {
            request_id: RequestId(1),
        },
        &mut sink,
    );
    sink.actions.clear();

    let wire = make_host_message(&make_offer(7));
    core.step(Event::HostMessage(&wire), &mut sink);
    assert_eq!(sink.actions.len(), 1);
    let Action::OfferReceived(descriptor) = &sink.actions[0] else {
        panic!("unexpected: {:?}", sink.actions[0]);
    };
    assert_eq!(descriptor.offer.channel_id.0, 7);
    // Recorded in the channel map.
    let entry = core
        .channels()
        .get(&vmbus_core::protocol::ChannelId(7))
        .expect("channel entry");
    assert!(matches!(entry.phase, ChannelPhase::Offered));
    // Offer count bumped.
    assert!(matches!(
        core.phase(),
        ClientPhase::RequestingOffers { offer_count: 1, .. }
    ));
}

#[test]
fn duplicate_offer_is_dropped_without_panic() {
    let mut core = ClientCore::new(make_config());
    let mut sink = Recording::default();
    connect_to_copper(&mut core, &mut sink);
    core.step(
        Event::RequestOffers {
            request_id: RequestId(1),
        },
        &mut sink,
    );
    sink.actions.clear();
    let wire = make_host_message(&make_offer(9));
    core.step(Event::HostMessage(&wire), &mut sink);
    core.step(Event::HostMessage(&wire), &mut sink);
    // Only one OfferReceived — the second was suppressed.
    assert_eq!(sink.actions.len(), 1);
    assert!(matches!(sink.actions[0], Action::OfferReceived(_)));
}

#[test]
fn all_offers_delivered_completes_and_returns_to_connected() {
    let mut core = ClientCore::new(make_config());
    let mut sink = Recording::default();
    connect_to_copper(&mut core, &mut sink);
    core.step(
        Event::RequestOffers {
            request_id: RequestId(11),
        },
        &mut sink,
    );
    let wire_offer = make_host_message(&make_offer(1));
    core.step(Event::HostMessage(&wire_offer), &mut sink);
    sink.actions.clear();

    let wire = make_host_message(&vmbus_core::protocol::AllOffersDelivered {});
    core.step(Event::HostMessage(&wire), &mut sink);
    assert_eq!(sink.actions.len(), 1);
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id: RequestId(11),
            result: CompletionResult::RequestOffers(Ok(())),
        }
    ));
    assert!(matches!(core.phase(), ClientPhase::Connected { .. }));
}

#[test]
fn stray_all_offers_delivered_is_dropped() {
    let mut core = ClientCore::new(make_config());
    let mut sink = Recording::default();
    connect_to_copper(&mut core, &mut sink);
    let wire = make_host_message(&vmbus_core::protocol::AllOffersDelivered {});
    core.step(Event::HostMessage(&wire), &mut sink);
    assert!(sink.actions.is_empty());
    assert!(matches!(core.phase(), ClientPhase::Connected { .. }));
}

#[test]
fn rescind_marks_channel_revoked_and_forwards() {
    let mut core = ClientCore::new(make_config());
    let mut sink = Recording::default();
    connect_to_copper(&mut core, &mut sink);
    core.step(
        Event::RequestOffers {
            request_id: RequestId(1),
        },
        &mut sink,
    );
    let offer_wire = make_host_message(&make_offer(3));
    core.step(Event::HostMessage(&offer_wire), &mut sink);
    sink.actions.clear();

    let rescind_wire = make_host_message(&vmbus_core::protocol::RescindChannelOffer {
        channel_id: vmbus_core::protocol::ChannelId(3),
    });
    core.step(Event::HostMessage(&rescind_wire), &mut sink);
    assert_eq!(sink.actions.len(), 1);
    assert!(matches!(
        &sink.actions[0],
        Action::OfferRescinded {
            channel_id: vmbus_core::protocol::ChannelId(3),
        }
    ));
    let entry = core
        .channels()
        .get(&vmbus_core::protocol::ChannelId(3))
        .expect("channel entry survives rescind");
    assert!(matches!(entry.phase, ChannelPhase::Revoked));
}

#[test]
fn rescind_for_unknown_channel_is_dropped_without_panic() {
    let mut core = ClientCore::new(make_config());
    let mut sink = Recording::default();
    connect_to_copper(&mut core, &mut sink);
    let rescind_wire = make_host_message(&vmbus_core::protocol::RescindChannelOffer {
        channel_id: vmbus_core::protocol::ChannelId(999),
    });
    core.step(Event::HostMessage(&rescind_wire), &mut sink);
    assert!(sink.actions.is_empty());
}

#[test]
fn offer_outside_connected_phase_is_not_recorded() {
    let mut core = ClientCore::new(make_config());
    let mut sink = Recording::default();
    let offer_wire = make_host_message(&make_offer(4));

    core.step(Event::HostMessage(&offer_wire), &mut sink);
    assert!(sink.actions.is_empty());
    assert!(core.channels().is_empty());

    connect_to_copper(&mut core, &mut sink);
    core.step(Event::HostMessage(&offer_wire), &mut sink);
    assert!(matches!(
        sink.actions.as_slice(),
        [Action::OfferReceived(OfferDescriptor { offer, .. })]
            if offer.channel_id == vmbus_core::protocol::ChannelId(4)
    ));
}

// -- Phase 4b-iii-b: channel lifecycle --------------------------

/// Config whose negotiated feature set enables both the flags
/// vmbus_client checks for redirection support. Used for the
/// per-channel open/close/modify tests.
fn make_redirect_config() -> Config {
    Config {
        sint: vmbus_core::VMBUS_SINT,
        vtl: 0,
        supported_versions: &[Version::Copper],
        supported_feature_flags: FeatureFlags::new()
            .with_guest_specified_signal_parameters(true)
            .with_channel_interrupt_redirection(true),
    }
}

fn connect_with_flags(core: &mut ClientCore, sink: &mut Recording, offered: FeatureFlags) {
    core.step(
        Event::Connect {
            request_id: RequestId(1),
            params: connect_params(),
        },
        sink,
    );
    let response = vmbus_core::protocol::VersionResponse2 {
        version_response: vmbus_core::protocol::VersionResponse {
            version_supported: 1,
            connection_state: vmbus_core::protocol::ConnectionState::SUCCESSFUL,
            padding: 0,
            selected_version_or_connection_id: 1,
        },
        supported_features: offered.into(),
    };
    let wire = make_host_message(&response);
    core.step(Event::HostMessage(&wire), sink);
    sink.actions.clear();
    assert!(matches!(core.phase(), ClientPhase::Connected { .. }));
}

/// Send RequestOffers, deliver one offer for `channel_id`, then
/// AllOffersDelivered so the core returns to Connected.
fn deliver_one_offer(core: &mut ClientCore, sink: &mut Recording, channel_id: u32) {
    core.step(
        Event::RequestOffers {
            request_id: RequestId(90 + channel_id as u64),
        },
        sink,
    );
    let offer_wire = make_host_message(&make_offer(channel_id));
    core.step(Event::HostMessage(&offer_wire), sink);
    let all_wire = make_host_message(&vmbus_core::protocol::AllOffersDelivered {});
    core.step(Event::HostMessage(&all_wire), sink);
    sink.actions.clear();
}

fn open_params_basic(channel_id: u32) -> OpenChannelParams {
    OpenChannelParams {
        target_vp: None,
        ring_offset: 0,
        ring_gpadl_id: vmbus_core::protocol::GpadlId(channel_id + 1),
        event_flag: channel_id as u16,
        connection_id: channel_id,
        redirected_event_flag: None,
        user_data: vmbus_core::protocol::UserDefinedData::default(),
    }
}

#[test]
fn open_channel_from_offered_posts_wire_and_transitions_opening() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    deliver_one_offer(&mut core, &mut sink, 5);

    core.step(
        Event::OpenChannel {
            request_id: RequestId(100),
            channel_id: vmbus_core::protocol::ChannelId(5),
            open: open_params_basic(5),
        },
        &mut sink,
    );
    assert_eq!(sink.actions.len(), 1);
    let _ = expect_post(&sink.actions[0]);
    let entry = &core.channels()[&vmbus_core::protocol::ChannelId(5)];
    assert!(matches!(
        entry.phase,
        ChannelPhase::Opening {
            request_id: RequestId(100),
            redirected_event_flag: None,
        }
    ));
    assert_eq!(entry.connection_id, 5);
}

#[test]
fn open_channel_on_unknown_channel_fails() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    core.step(
        Event::OpenChannel {
            request_id: RequestId(101),
            channel_id: vmbus_core::protocol::ChannelId(42),
            open: open_params_basic(42),
        },
        &mut sink,
    );
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id: RequestId(101),
            result: CompletionResult::OpenChannel(Err(OpenChannelError::InvalidState)),
        }
    ));
}

#[test]
fn open_channel_on_revoked_channel_fails_with_revoked_error() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    deliver_one_offer(&mut core, &mut sink, 5);
    let rescind_wire = make_host_message(&vmbus_core::protocol::RescindChannelOffer {
        channel_id: vmbus_core::protocol::ChannelId(5),
    });
    core.step(Event::HostMessage(&rescind_wire), &mut sink);
    sink.actions.clear();
    core.step(
        Event::OpenChannel {
            request_id: RequestId(103),
            channel_id: vmbus_core::protocol::ChannelId(5),
            open: open_params_basic(5),
        },
        &mut sink,
    );
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id: RequestId(103),
            result: CompletionResult::OpenChannel(Err(OpenChannelError::Revoked)),
        }
    ));
}

#[test]
fn open_with_redirection_but_no_flag_support_fails_and_frees_flag() {
    // Config accepts no redirection flags. But caller has
    // pre-allocated a redirected_event_flag — the request must
    // fail and the flag must come back through Action::FreeEventFlag.
    let mut core = ClientCore::new(make_config());
    let mut sink = Recording::default();
    connect_with_flags(&mut core, &mut sink, FeatureFlags::new());
    deliver_one_offer(&mut core, &mut sink, 6);
    let flag = core.allocate_event_flag().unwrap();
    let mut params = open_params_basic(6);
    params.redirected_event_flag = Some(flag);
    core.step(
        Event::OpenChannel {
            request_id: RequestId(104),
            channel_id: vmbus_core::protocol::ChannelId(6),
            open: params,
        },
        &mut sink,
    );
    // Expect FreeEventFlag then Complete(Err).
    assert!(matches!(sink.actions[0], Action::FreeEventFlag(f) if f == flag));
    assert!(matches!(
        &sink.actions[1],
        Action::Complete {
            request_id: RequestId(104),
            result: CompletionResult::OpenChannel(Err(
                OpenChannelError::UnsupportedInterruptFeature,
            )),
        }
    ));
}

#[test]
fn invalid_restore_channel_frees_reserved_event_flag() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );

    for channel_id in [42, 43] {
        if channel_id == 43 {
            deliver_one_offer(&mut core, &mut sink, channel_id);
        }
        let flag = core.allocate_event_flag().unwrap();
        core.step(
            Event::RestoreChannel {
                request_id: RequestId(150 + channel_id as u64),
                channel_id: vmbus_core::protocol::ChannelId(channel_id),
                params: RestoreChannelParams {
                    redirected_event_flag: Some(flag),
                    connection_id: channel_id,
                },
            },
            &mut sink,
        );
        assert!(matches!(sink.actions[0], Action::FreeEventFlag(f) if f == flag));
        assert!(matches!(
            sink.actions[1],
            Action::Complete {
                result: CompletionResult::OpenChannel(Err(OpenChannelError::InvalidState)),
                ..
            }
        ));
        assert_eq!(core.flag_allocator.used_count(), 0);
        sink.actions.clear();
    }
}

#[test]
fn rescind_waits_for_pending_modify_before_release() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_and_offer(&mut core, &mut sink, 18);
    core.step(
        Event::ModifyChannel {
            request_id: RequestId(1800),
            channel_id: vmbus_core::protocol::ChannelId(18),
            request: ModifyRequest::TargetVp { target_vp: 1 },
        },
        &mut sink,
    );
    sink.actions.clear();

    let rescind = make_host_message(&vmbus_core::protocol::RescindChannelOffer {
        channel_id: vmbus_core::protocol::ChannelId(18),
    });
    core.step(Event::HostMessage(&rescind), &mut sink);
    core.step(
        Event::ReleaseChannel {
            channel_id: vmbus_core::protocol::ChannelId(18),
        },
        &mut sink,
    );
    assert!(
        core.channels()
            .contains_key(&vmbus_core::protocol::ChannelId(18))
    );
    assert!(!sink.actions.iter().any(|action| matches!(
        action,
        Action::Complete {
            request_id: RequestId(1800),
            ..
        }
    )));
    sink.actions.clear();

    let response = make_host_message(&vmbus_core::protocol::ModifyChannelResponse {
        channel_id: vmbus_core::protocol::ChannelId(18),
        status: vmbus_core::protocol::STATUS_SUCCESS,
    });
    core.step(Event::HostMessage(&response), &mut sink);
    assert!(matches!(
        sink.actions[0],
        Action::Complete {
            request_id: RequestId(1800),
            result: CompletionResult::ModifyChannel(vmbus_core::protocol::STATUS_SUCCESS),
        }
    ));
    assert!(matches!(sink.actions[1], Action::PostMessage(_)));
    assert!(
        !core
            .channels()
            .contains_key(&vmbus_core::protocol::ChannelId(18))
    );
}

#[test]
fn open_result_success_transitions_opened_and_completes() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    deliver_one_offer(&mut core, &mut sink, 7);
    core.step(
        Event::OpenChannel {
            request_id: RequestId(200),
            channel_id: vmbus_core::protocol::ChannelId(7),
            open: open_params_basic(7),
        },
        &mut sink,
    );
    sink.actions.clear();

    let result = vmbus_core::protocol::OpenResult {
        channel_id: vmbus_core::protocol::ChannelId(7),
        open_id: 0,
        status: vmbus_core::protocol::STATUS_SUCCESS as u32,
    };
    let wire = make_host_message(&result);
    core.step(Event::HostMessage(&wire), &mut sink);

    // Expected: ConnectionIdAssigned, Opened, Complete Ok.
    assert_eq!(sink.actions.len(), 3);
    assert!(matches!(
        sink.actions[0],
        Action::ChannelObservable {
            event: ChannelObservable::ConnectionIdAssigned(7),
            ..
        }
    ));
    assert!(matches!(
        sink.actions[1],
        Action::ChannelObservable {
            event: ChannelObservable::Opened,
            ..
        }
    ));
    assert!(matches!(
        &sink.actions[2],
        Action::Complete {
            request_id: RequestId(200),
            result: CompletionResult::OpenChannel(Ok(_)),
        }
    ));
    let entry = &core.channels()[&vmbus_core::protocol::ChannelId(7)];
    assert!(matches!(entry.phase, ChannelPhase::Opened { .. }));
}

#[test]
fn open_result_failure_returns_to_offered_and_reports_status() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    deliver_one_offer(&mut core, &mut sink, 8);
    core.step(
        Event::OpenChannel {
            request_id: RequestId(201),
            channel_id: vmbus_core::protocol::ChannelId(8),
            open: open_params_basic(8),
        },
        &mut sink,
    );
    sink.actions.clear();

    // Non-zero status = failure.
    let result = vmbus_core::protocol::OpenResult {
        channel_id: vmbus_core::protocol::ChannelId(8),
        open_id: 0,
        status: 0xC0000001,
    };
    let wire = make_host_message(&result);
    core.step(Event::HostMessage(&wire), &mut sink);
    // Expect ConnectionIdCleared + Complete(Err(HostFailed(_))).
    assert!(matches!(
        sink.actions[0],
        Action::ChannelObservable {
            event: ChannelObservable::ConnectionIdCleared,
            ..
        }
    ));
    let Action::Complete {
        result: CompletionResult::OpenChannel(Err(OpenChannelError::HostFailed(status))),
        ..
    } = &sink.actions[1]
    else {
        panic!();
    };
    assert_eq!(*status as u32, 0xC0000001);
    // Channel is back in Offered so a retry is possible.
    let entry = &core.channels()[&vmbus_core::protocol::ChannelId(8)];
    assert!(matches!(entry.phase, ChannelPhase::Offered));
}

#[test]
fn close_channel_frees_flag_posts_and_emits_observables() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    deliver_one_offer(&mut core, &mut sink, 9);

    let flag = core.allocate_event_flag().unwrap();
    let mut params = open_params_basic(9);
    params.redirected_event_flag = Some(flag);
    core.step(
        Event::OpenChannel {
            request_id: RequestId(300),
            channel_id: vmbus_core::protocol::ChannelId(9),
            open: params,
        },
        &mut sink,
    );
    // Simulate successful open.
    let result = vmbus_core::protocol::OpenResult {
        channel_id: vmbus_core::protocol::ChannelId(9),
        open_id: 0,
        status: vmbus_core::protocol::STATUS_SUCCESS as u32,
    };
    let wire = make_host_message(&result);
    core.step(Event::HostMessage(&wire), &mut sink);
    sink.actions.clear();

    // Now close.
    core.step(
        Event::CloseChannel {
            channel_id: vmbus_core::protocol::ChannelId(9),
        },
        &mut sink,
    );
    // Expected: FreeEventFlag, PostMessage, ConnectionIdCleared, Closed.
    assert!(matches!(sink.actions[0], Action::FreeEventFlag(f) if f == flag));
    let _ = expect_post(&sink.actions[1]);
    assert!(matches!(
        sink.actions[2],
        Action::ChannelObservable {
            event: ChannelObservable::ConnectionIdCleared,
            ..
        }
    ));
    assert!(matches!(
        sink.actions[3],
        Action::ChannelObservable {
            event: ChannelObservable::Closed,
            ..
        }
    ));
    // Channel returns to Offered.
    let entry = &core.channels()[&vmbus_core::protocol::ChannelId(9)];
    assert!(matches!(entry.phase, ChannelPhase::Offered));
}

#[test]
fn close_on_revoked_is_no_op() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    deliver_one_offer(&mut core, &mut sink, 10);
    let rescind_wire = make_host_message(&vmbus_core::protocol::RescindChannelOffer {
        channel_id: vmbus_core::protocol::ChannelId(10),
    });
    core.step(Event::HostMessage(&rescind_wire), &mut sink);
    sink.actions.clear();

    core.step(
        Event::CloseChannel {
            channel_id: vmbus_core::protocol::ChannelId(10),
        },
        &mut sink,
    );
    assert!(sink.actions.is_empty());
}

#[test]
fn rescind_while_opening_completes_pending_open_with_revoked() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    deliver_one_offer(&mut core, &mut sink, 11);
    let flag = core.allocate_event_flag().unwrap();
    let mut params = open_params_basic(11);
    params.redirected_event_flag = Some(flag);
    core.step(
        Event::OpenChannel {
            request_id: RequestId(400),
            channel_id: vmbus_core::protocol::ChannelId(11),
            open: params,
        },
        &mut sink,
    );
    sink.actions.clear();

    let rescind_wire = make_host_message(&vmbus_core::protocol::RescindChannelOffer {
        channel_id: vmbus_core::protocol::ChannelId(11),
    });
    core.step(Event::HostMessage(&rescind_wire), &mut sink);
    // Expected: FreeEventFlag, Complete(Err(Revoked)), OfferRescinded.
    assert!(
        sink.actions
            .iter()
            .any(|a| matches!(a, Action::FreeEventFlag(f) if *f == flag))
    );
    assert!(sink.actions.iter().any(|a| matches!(
        a,
        Action::Complete {
            request_id: RequestId(400),
            result: CompletionResult::OpenChannel(Err(OpenChannelError::Revoked)),
        }
    )));
    assert!(
        sink.actions
            .iter()
            .any(|a| matches!(a, Action::OfferRescinded { .. }))
    );
}

#[test]
fn release_after_rescind_posts_relidreleased_and_drops_entry() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    deliver_one_offer(&mut core, &mut sink, 12);
    let rescind_wire = make_host_message(&vmbus_core::protocol::RescindChannelOffer {
        channel_id: vmbus_core::protocol::ChannelId(12),
    });
    core.step(Event::HostMessage(&rescind_wire), &mut sink);
    sink.actions.clear();

    core.step(
        Event::ReleaseChannel {
            channel_id: vmbus_core::protocol::ChannelId(12),
        },
        &mut sink,
    );
    assert!(matches!(
        sink.actions.as_slice(),
        [
            Action::PostMessage(_),
            Action::ChannelRemoved {
                channel_id: vmbus_core::protocol::ChannelId(12),
            }
        ]
    ));
    assert!(
        !core
            .channels()
            .contains_key(&vmbus_core::protocol::ChannelId(12))
    );
}

#[test]
fn release_before_rescind_only_marks_flag() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    deliver_one_offer(&mut core, &mut sink, 13);
    core.step(
        Event::ReleaseChannel {
            channel_id: vmbus_core::protocol::ChannelId(13),
        },
        &mut sink,
    );
    // No wire message yet — waiting for host rescind.
    assert!(sink.actions.is_empty());
    // Channel is still there but marked released.
    assert!(
        core.channels()
            .get(&vmbus_core::protocol::ChannelId(13))
            .unwrap()
            .is_client_released
    );
}

#[test]
fn modify_channel_posts_and_response_completes() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    deliver_one_offer(&mut core, &mut sink, 14);
    core.step(
        Event::ModifyChannel {
            request_id: RequestId(500),
            channel_id: vmbus_core::protocol::ChannelId(14),
            request: ModifyRequest::TargetVp { target_vp: 3 },
        },
        &mut sink,
    );
    assert_eq!(sink.actions.len(), 1);
    let _ = expect_post(&sink.actions[0]);
    sink.actions.clear();

    let response = vmbus_core::protocol::ModifyChannelResponse {
        channel_id: vmbus_core::protocol::ChannelId(14),
        status: 0,
    };
    let wire = make_host_message(&response);
    core.step(Event::HostMessage(&wire), &mut sink);
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id: RequestId(500),
            result: CompletionResult::ModifyChannel(0),
        }
    ));
}

#[test]
fn restore_channel_promotes_restored_to_opened() {
    // Use the low-level fabricate-a-Restored-entry path since
    // the crate doesn't yet expose a save/restore front end.
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    deliver_one_offer(&mut core, &mut sink, 15);
    // Manually put the channel in Restored so we can drive
    // RestoreChannel through step().
    {
        let entry = core
            .channels
            .get_mut(&vmbus_core::protocol::ChannelId(15))
            .unwrap();
        entry.phase = ChannelPhase::Restored;
    }
    core.step(
        Event::RestoreChannel {
            request_id: RequestId(600),
            channel_id: vmbus_core::protocol::ChannelId(15),
            params: RestoreChannelParams {
                redirected_event_flag: None,
                connection_id: 42,
            },
        },
        &mut sink,
    );
    assert_eq!(sink.actions.len(), 3);
    assert!(matches!(
        sink.actions[0],
        Action::ChannelObservable {
            event: ChannelObservable::ConnectionIdAssigned(42),
            ..
        }
    ));
    assert!(matches!(
        &sink.actions[2],
        Action::Complete {
            request_id: RequestId(600),
            result: CompletionResult::OpenChannel(Ok(_)),
        }
    ));
}

// -- Phase 4b-iv: GPADL establish + teardown --------------------

/// Bring `core` to Connected with an offered channel `channel_id`.
fn connect_and_offer(core: &mut ClientCore, sink: &mut Recording, channel_id: u32) {
    connect_with_flags(core, sink, make_redirect_config().supported_feature_flags);
    deliver_one_offer(core, sink, channel_id);
}

/// Convenience: minimal 3-GPA gpadl (fits inline in GpadlHeader).
fn small_gpadl(id: u32) -> (vmbus_core::protocol::GpadlId, GpadlRequest) {
    (
        vmbus_core::protocol::GpadlId(id),
        GpadlRequest {
            id: vmbus_core::protocol::GpadlId(id),
            count: 1,
            buf: alloc::vec![0x1000, 0x2000, 0x3000],
        },
    )
}

#[test]
fn establish_gpadl_posts_header_and_records_offered() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_and_offer(&mut core, &mut sink, 20);
    let (gid, request) = small_gpadl(555);
    core.step(
        Event::EstablishGpadl {
            request_id: RequestId(700),
            channel_id: vmbus_core::protocol::ChannelId(20),
            gpadl_id: gid,
            request,
        },
        &mut sink,
    );
    // Small gpadl fits inline — exactly one PostMessage.
    assert_eq!(sink.actions.len(), 1);
    let _ = expect_post(&sink.actions[0]);
    let entry = &core.channels()[&vmbus_core::protocol::ChannelId(20)];
    assert!(matches!(
        entry.gpadls[&gid],
        GpadlPhase::Offered {
            request_id: RequestId(700),
            ..
        }
    ));
}

#[test]
fn establish_gpadl_on_unknown_channel_fails() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    let (gid, request) = small_gpadl(1);
    core.step(
        Event::EstablishGpadl {
            request_id: RequestId(701),
            channel_id: vmbus_core::protocol::ChannelId(99),
            gpadl_id: gid,
            request,
        },
        &mut sink,
    );
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id: RequestId(701),
            result: CompletionResult::EstablishGpadl(Err(EstablishGpadlError::UnknownChannel)),
        }
    ));
}

#[test]
fn establish_gpadl_duplicate_id_is_rejected() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_and_offer(&mut core, &mut sink, 21);
    let (gid, req1) = small_gpadl(1);
    core.step(
        Event::EstablishGpadl {
            request_id: RequestId(801),
            channel_id: vmbus_core::protocol::ChannelId(21),
            gpadl_id: gid,
            request: req1.clone(),
        },
        &mut sink,
    );
    sink.actions.clear();
    core.step(
        Event::EstablishGpadl {
            request_id: RequestId(802),
            channel_id: vmbus_core::protocol::ChannelId(21),
            gpadl_id: gid,
            request: req1,
        },
        &mut sink,
    );
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id: RequestId(802),
            result: CompletionResult::EstablishGpadl(Err(EstablishGpadlError::DuplicateId)),
        }
    ));
}

#[test]
fn establish_gpadl_large_buffer_produces_body_messages() {
    // A buffer larger than GpadlHeader::MAX_DATA_VALUES forces
    // one or more GpadlBody follow-ups.
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_and_offer(&mut core, &mut sink, 22);
    let max = vmbus_core::protocol::GpadlHeader::MAX_DATA_VALUES;
    let big = (0..max + 3).map(|i| i as u64).collect::<Vec<u64>>();
    let request = GpadlRequest {
        id: vmbus_core::protocol::GpadlId(9),
        count: 1,
        buf: big,
    };
    core.step(
        Event::EstablishGpadl {
            request_id: RequestId(900),
            channel_id: vmbus_core::protocol::ChannelId(22),
            gpadl_id: vmbus_core::protocol::GpadlId(9),
            request,
        },
        &mut sink,
    );
    // 1 header + 1 body for the remaining 3 values.
    assert_eq!(sink.actions.len(), 2);
    for action in &sink.actions {
        let _ = expect_post(action);
    }
}

#[test]
fn establish_oversized_gpadl_is_rejected_without_state_change() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_and_offer(&mut core, &mut sink, 23);
    let gpadl_id = vmbus_core::protocol::GpadlId(10);
    let request = GpadlRequest {
        id: gpadl_id,
        count: 1,
        buf: alloc::vec![0; usize::from(u16::MAX) / size_of::<u64>() + 1],
    };
    core.step(
        Event::EstablishGpadl {
            request_id: RequestId(901),
            channel_id: vmbus_core::protocol::ChannelId(23),
            gpadl_id,
            request,
        },
        &mut sink,
    );

    assert!(matches!(
        sink.actions.as_slice(),
        [Action::Complete {
            request_id: RequestId(901),
            result: CompletionResult::EstablishGpadl(Err(EstablishGpadlError::RequestTooLarge)),
        }]
    ));
    assert!(
        core.channels()[&vmbus_core::protocol::ChannelId(23)]
            .gpadls
            .is_empty()
    );
    assert!(!core.outstanding.contains_key(&RequestId(901)));
}

#[test]
fn gpadl_created_success_transitions_to_created() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_and_offer(&mut core, &mut sink, 30);
    let (gid, request) = small_gpadl(7);
    core.step(
        Event::EstablishGpadl {
            request_id: RequestId(1000),
            channel_id: vmbus_core::protocol::ChannelId(30),
            gpadl_id: gid,
            request,
        },
        &mut sink,
    );
    sink.actions.clear();

    let created = vmbus_core::protocol::GpadlCreated {
        channel_id: vmbus_core::protocol::ChannelId(30),
        gpadl_id: gid,
        status: vmbus_core::protocol::STATUS_SUCCESS,
    };
    let wire = make_host_message(&created);
    core.step(Event::HostMessage(&wire), &mut sink);
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id: RequestId(1000),
            result: CompletionResult::EstablishGpadl(Ok(())),
        }
    ));
    let entry = &core.channels()[&vmbus_core::protocol::ChannelId(30)];
    assert!(matches!(entry.gpadls[&gid], GpadlPhase::Created));
}

#[test]
fn gpadl_created_failure_removes_gpadl_entry() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_and_offer(&mut core, &mut sink, 31);
    let (gid, request) = small_gpadl(8);
    core.step(
        Event::EstablishGpadl {
            request_id: RequestId(1100),
            channel_id: vmbus_core::protocol::ChannelId(31),
            gpadl_id: gid,
            request,
        },
        &mut sink,
    );
    sink.actions.clear();
    let created = vmbus_core::protocol::GpadlCreated {
        channel_id: vmbus_core::protocol::ChannelId(31),
        gpadl_id: gid,
        status: -1,
    };
    let wire = make_host_message(&created);
    core.step(Event::HostMessage(&wire), &mut sink);
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id: RequestId(1100),
            result: CompletionResult::EstablishGpadl(Err(EstablishGpadlError::HostRejected(-1))),
        }
    ));
    assert!(
        !core.channels()[&vmbus_core::protocol::ChannelId(31)]
            .gpadls
            .contains_key(&gid)
    );
}

#[test]
fn teardown_waits_for_in_flight_gpadl_creation() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_and_offer(&mut core, &mut sink, 41);
    let (gpadl_id, request) = small_gpadl(15);
    core.step(
        Event::EstablishGpadl {
            request_id: RequestId(1110),
            channel_id: vmbus_core::protocol::ChannelId(41),
            gpadl_id,
            request,
        },
        &mut sink,
    );
    sink.actions.clear();

    core.step(
        Event::TeardownGpadl {
            request_id: RequestId(1111),
            channel_id: vmbus_core::protocol::ChannelId(41),
            gpadl_id,
        },
        &mut sink,
    );
    assert!(sink.actions.is_empty());

    core.step(
        Event::HostMessage(&make_host_message(&vmbus_core::protocol::GpadlCreated {
            channel_id: vmbus_core::protocol::ChannelId(41),
            gpadl_id,
            status: vmbus_core::protocol::STATUS_SUCCESS,
        })),
        &mut sink,
    );
    assert!(matches!(
        sink.actions.as_slice(),
        [
            Action::Complete {
                request_id: RequestId(1110),
                result: CompletionResult::EstablishGpadl(Ok(())),
            },
            Action::PostMessage(_),
        ]
    ));
    assert!(matches!(
        core.channels()[&vmbus_core::protocol::ChannelId(41)].gpadls[&gpadl_id],
        GpadlPhase::TearingDown { .. }
    ));
    sink.actions.clear();

    core.step(
        Event::HostMessage(&make_host_message(&vmbus_core::protocol::GpadlTorndown {
            gpadl_id,
        })),
        &mut sink,
    );
    assert!(matches!(
        sink.actions.as_slice(),
        [Action::Complete {
            request_id: RequestId(1111),
            result: CompletionResult::TeardownGpadl,
        }]
    ));
}

#[test]
fn failed_gpadl_creation_completes_queued_teardown() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_and_offer(&mut core, &mut sink, 42);
    let (gpadl_id, request) = small_gpadl(16);
    core.step(
        Event::EstablishGpadl {
            request_id: RequestId(1120),
            channel_id: vmbus_core::protocol::ChannelId(42),
            gpadl_id,
            request,
        },
        &mut sink,
    );
    sink.actions.clear();

    core.step(
        Event::TeardownGpadl {
            request_id: RequestId(1121),
            channel_id: vmbus_core::protocol::ChannelId(42),
            gpadl_id,
        },
        &mut sink,
    );
    core.step(
        Event::HostMessage(&make_host_message(&vmbus_core::protocol::GpadlCreated {
            channel_id: vmbus_core::protocol::ChannelId(42),
            gpadl_id,
            status: -1,
        })),
        &mut sink,
    );

    assert!(matches!(
        sink.actions.as_slice(),
        [
            Action::Complete {
                request_id: RequestId(1120),
                result: CompletionResult::EstablishGpadl(Err(EstablishGpadlError::HostRejected(
                    -1
                ))),
            },
            Action::Complete {
                request_id: RequestId(1121),
                result: CompletionResult::TeardownGpadl,
            },
        ]
    ));
    assert!(
        !core.channels()[&vmbus_core::protocol::ChannelId(42)]
            .gpadls
            .contains_key(&gpadl_id)
    );
    assert!(!core.outstanding.contains_key(&RequestId(1121)));
}

#[test]
fn successful_gpadl_creation_retries_revoked_channel_release() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_and_offer(&mut core, &mut sink, 31);
    let (gpadl_id, request) = small_gpadl(9);
    core.step(
        Event::EstablishGpadl {
            request_id: RequestId(1150),
            channel_id: vmbus_core::protocol::ChannelId(31),
            gpadl_id,
            request,
        },
        &mut sink,
    );
    let rescind = make_host_message(&vmbus_core::protocol::RescindChannelOffer {
        channel_id: vmbus_core::protocol::ChannelId(31),
    });
    core.step(Event::HostMessage(&rescind), &mut sink);
    core.step(
        Event::ReleaseChannel {
            channel_id: vmbus_core::protocol::ChannelId(31),
        },
        &mut sink,
    );
    sink.actions.clear();

    let created = make_host_message(&vmbus_core::protocol::GpadlCreated {
        channel_id: vmbus_core::protocol::ChannelId(31),
        gpadl_id,
        status: vmbus_core::protocol::STATUS_SUCCESS,
    });
    core.step(Event::HostMessage(&created), &mut sink);

    assert!(matches!(
        sink.actions[0],
        Action::Complete {
            request_id: RequestId(1150),
            result: CompletionResult::EstablishGpadl(Ok(())),
        }
    ));
    assert!(matches!(sink.actions[1], Action::PostMessage(_)));
    assert!(
        !core
            .channels()
            .contains_key(&vmbus_core::protocol::ChannelId(31))
    );
}

#[test]
fn teardown_gpadl_created_posts_and_completes_on_torndown() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_and_offer(&mut core, &mut sink, 32);
    let (gid, request) = small_gpadl(10);
    core.step(
        Event::EstablishGpadl {
            request_id: RequestId(1200),
            channel_id: vmbus_core::protocol::ChannelId(32),
            gpadl_id: gid,
            request,
        },
        &mut sink,
    );
    let created = vmbus_core::protocol::GpadlCreated {
        channel_id: vmbus_core::protocol::ChannelId(32),
        gpadl_id: gid,
        status: vmbus_core::protocol::STATUS_SUCCESS,
    };
    let wire = make_host_message(&created);
    core.step(Event::HostMessage(&wire), &mut sink);
    sink.actions.clear();

    core.step(
        Event::TeardownGpadl {
            request_id: RequestId(1201),
            channel_id: vmbus_core::protocol::ChannelId(32),
            gpadl_id: gid,
        },
        &mut sink,
    );
    // GpadlTeardown wire message.
    assert_eq!(sink.actions.len(), 1);
    let _ = expect_post(&sink.actions[0]);
    sink.actions.clear();

    let torndown = vmbus_core::protocol::GpadlTorndown { gpadl_id: gid };
    let wire = make_host_message(&torndown);
    core.step(Event::HostMessage(&wire), &mut sink);
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id: RequestId(1201),
            result: CompletionResult::TeardownGpadl,
        }
    ));
    // Gpadl entry is removed after teardown.
    assert!(
        !core.channels()[&vmbus_core::protocol::ChannelId(32)]
            .gpadls
            .contains_key(&gid)
    );
}

#[test]
fn teardown_coalesces_multiple_racing_requests() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_and_offer(&mut core, &mut sink, 33);
    let (gid, request) = small_gpadl(11);
    core.step(
        Event::EstablishGpadl {
            request_id: RequestId(1300),
            channel_id: vmbus_core::protocol::ChannelId(33),
            gpadl_id: gid,
            request,
        },
        &mut sink,
    );
    let created = vmbus_core::protocol::GpadlCreated {
        channel_id: vmbus_core::protocol::ChannelId(33),
        gpadl_id: gid,
        status: vmbus_core::protocol::STATUS_SUCCESS,
    };
    let wire = make_host_message(&created);
    core.step(Event::HostMessage(&wire), &mut sink);
    sink.actions.clear();

    // Two callers race on teardown for the same gpadl.
    core.step(
        Event::TeardownGpadl {
            request_id: RequestId(1301),
            channel_id: vmbus_core::protocol::ChannelId(33),
            gpadl_id: gid,
        },
        &mut sink,
    );
    core.step(
        Event::TeardownGpadl {
            request_id: RequestId(1302),
            channel_id: vmbus_core::protocol::ChannelId(33),
            gpadl_id: gid,
        },
        &mut sink,
    );
    // Only one wire message; second request is queued.
    assert_eq!(
        sink.actions
            .iter()
            .filter(|a| matches!(a, Action::PostMessage(_)))
            .count(),
        1
    );
    sink.actions.clear();

    let torndown = vmbus_core::protocol::GpadlTorndown { gpadl_id: gid };
    let wire = make_host_message(&torndown);
    core.step(Event::HostMessage(&wire), &mut sink);
    // Both request ids get completed.
    let completed: Vec<_> = sink
        .actions
        .iter()
        .filter_map(|a| match a {
            Action::Complete {
                request_id,
                result: CompletionResult::TeardownGpadl,
            } => Some(*request_id),
            _ => None,
        })
        .collect();
    assert_eq!(completed, [RequestId(1301), RequestId(1302)]);
}

#[test]
fn gpadl_torndown_uses_the_in_flight_channel_owner() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_and_offer(&mut core, &mut sink, 36);
    deliver_one_offer(&mut core, &mut sink, 37);
    let gpadl_id = vmbus_core::protocol::GpadlId(12);

    for (channel_id, request_id) in [(36, 1360), (37, 1370)] {
        core.step(
            Event::EstablishGpadl {
                request_id: RequestId(request_id),
                channel_id: vmbus_core::protocol::ChannelId(channel_id),
                gpadl_id,
                request: GpadlRequest {
                    id: gpadl_id,
                    count: 1,
                    buf: alloc::vec![0x1000],
                },
            },
            &mut sink,
        );
        let created = make_host_message(&vmbus_core::protocol::GpadlCreated {
            channel_id: vmbus_core::protocol::ChannelId(channel_id),
            gpadl_id,
            status: vmbus_core::protocol::STATUS_SUCCESS,
        });
        core.step(Event::HostMessage(&created), &mut sink);
    }
    sink.actions.clear();

    core.step(
        Event::TeardownGpadl {
            request_id: RequestId(1371),
            channel_id: vmbus_core::protocol::ChannelId(37),
            gpadl_id,
        },
        &mut sink,
    );
    assert!(matches!(
        core.channels()[&vmbus_core::protocol::ChannelId(36)].gpadls[&gpadl_id],
        GpadlPhase::Created
    ));
    core.step(
        Event::TeardownGpadl {
            request_id: RequestId(1361),
            channel_id: vmbus_core::protocol::ChannelId(36),
            gpadl_id,
        },
        &mut sink,
    );
    assert_eq!(
        sink.actions
            .iter()
            .filter(|action| matches!(action, Action::PostMessage(_)))
            .count(),
        1
    );
    sink.actions.clear();

    let torndown = make_host_message(&vmbus_core::protocol::GpadlTorndown { gpadl_id });
    core.step(Event::HostMessage(&torndown), &mut sink);

    assert!(matches!(
        core.channels()[&vmbus_core::protocol::ChannelId(36)].gpadls[&gpadl_id],
        GpadlPhase::TearingDown { .. }
    ));
    assert!(
        !core.channels()[&vmbus_core::protocol::ChannelId(37)]
            .gpadls
            .contains_key(&gpadl_id)
    );
    assert!(sink.actions.iter().any(|action| matches!(
        action,
        Action::Complete {
            request_id: RequestId(1371),
            result: CompletionResult::TeardownGpadl,
        }
    )));
    assert!(
        sink.actions
            .iter()
            .any(|action| matches!(action, Action::PostMessage(_)))
    );
    sink.actions.clear();

    core.step(Event::HostMessage(&torndown), &mut sink);
    assert!(sink.actions.iter().any(|action| matches!(
        action,
        Action::Complete {
            request_id: RequestId(1361),
            result: CompletionResult::TeardownGpadl,
        }
    )));
    assert!(
        !core.channels()[&vmbus_core::protocol::ChannelId(36)]
            .gpadls
            .contains_key(&gpadl_id)
    );
}

#[test]
fn save_restore_preserves_duplicate_gpadl_teardown_order() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_and_offer(&mut core, &mut sink, 38);
    deliver_one_offer(&mut core, &mut sink, 39);
    let gpadl_id = vmbus_core::protocol::GpadlId(13);

    for (channel_id, request_id) in [(38, 1380), (39, 1390)] {
        core.step(
            Event::EstablishGpadl {
                request_id: RequestId(request_id),
                channel_id: vmbus_core::protocol::ChannelId(channel_id),
                gpadl_id,
                request: GpadlRequest {
                    id: gpadl_id,
                    count: 1,
                    buf: alloc::vec![0x1000],
                },
            },
            &mut sink,
        );
        core.step(
            Event::HostMessage(&make_host_message(&vmbus_core::protocol::GpadlCreated {
                channel_id: vmbus_core::protocol::ChannelId(channel_id),
                gpadl_id,
                status: vmbus_core::protocol::STATUS_SUCCESS,
            })),
            &mut sink,
        );
    }

    core.step(
        Event::TeardownGpadl {
            request_id: RequestId(1391),
            channel_id: vmbus_core::protocol::ChannelId(39),
            gpadl_id,
        },
        &mut sink,
    );
    core.step(
        Event::TeardownGpadl {
            request_id: RequestId(1381),
            channel_id: vmbus_core::protocol::ChannelId(38),
            gpadl_id,
        },
        &mut sink,
    );

    let saved = core.save();
    let mut restored = ClientCore::new(make_redirect_config());
    restored.restore(saved).unwrap();
    assert_eq!(
        restored.teardown_gpadls.get(&gpadl_id),
        Some(&vmbus_core::protocol::ChannelId(39))
    );

    sink.actions.clear();
    restored.step(
        Event::HostMessage(&make_host_message(&vmbus_core::protocol::GpadlTorndown {
            gpadl_id,
        })),
        &mut sink,
    );
    assert_eq!(
        restored.teardown_gpadls.get(&gpadl_id),
        Some(&vmbus_core::protocol::ChannelId(38))
    );
    assert!(
        sink.actions
            .iter()
            .any(|action| matches!(action, Action::PostMessage(_)))
    );
}

#[test]
fn restore_rejects_queued_gpadl_teardown_without_owner() {
    let config = make_redirect_config();
    let version = VersionInfo {
        version: Version::Copper,
        feature_flags: config.supported_feature_flags,
    };
    let mut core = ClientCore::new(config);
    let gpadl_id = vmbus_core::protocol::GpadlId(14);

    let error = core
        .restore(SavedState {
            version: Some(version),
            channels: alloc::vec![SavedChannel {
                offer: make_offer(40),
                phase: SavedChannelPhase::Opened,
                gpadls: alloc::vec![SavedGpadl {
                    id: gpadl_id,
                    phase: SavedGpadlPhase::TearingDown { queued: true },
                }],
            }],
            released_channel_ids: Vec::new(),
            modify_connection_pending: false,
            hvsock_requests: Vec::new(),
        })
        .unwrap_err();

    assert_eq!(error, RestoreError::QueuedGpadlWithoutOwner(gpadl_id.0));
}

#[test]
fn teardown_unknown_gpadl_is_no_op() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_and_offer(&mut core, &mut sink, 34);
    core.step(
        Event::TeardownGpadl {
            request_id: RequestId(1400),
            channel_id: vmbus_core::protocol::ChannelId(34),
            gpadl_id: vmbus_core::protocol::GpadlId(999),
        },
        &mut sink,
    );
    // Completes without wire post.
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id: RequestId(1400),
            result: CompletionResult::TeardownGpadl,
        }
    ));
}

#[test]
fn establish_gpadl_on_revoked_channel_is_allowed_to_finish() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_and_offer(&mut core, &mut sink, 35);
    let rescind_wire = make_host_message(&vmbus_core::protocol::RescindChannelOffer {
        channel_id: vmbus_core::protocol::ChannelId(35),
    });
    core.step(Event::HostMessage(&rescind_wire), &mut sink);
    sink.actions.clear();
    let (gid, request) = small_gpadl(50);
    core.step(
        Event::EstablishGpadl {
            request_id: RequestId(1500),
            channel_id: vmbus_core::protocol::ChannelId(35),
            gpadl_id: gid,
            request,
        },
        &mut sink,
    );
    assert!(matches!(&sink.actions[0], Action::PostMessage(_)));

    sink.actions.clear();
    let created = make_host_message(&vmbus_core::protocol::GpadlCreated {
        channel_id: vmbus_core::protocol::ChannelId(35),
        gpadl_id: gid,
        status: vmbus_core::protocol::STATUS_UNSUCCESSFUL,
    });
    core.step(Event::HostMessage(&created), &mut sink);
    assert!(matches!(
        sink.actions.last().unwrap(),
        Action::Complete {
            request_id: RequestId(1500),
            result: CompletionResult::EstablishGpadl(Err(EstablishGpadlError::HostRejected(
                vmbus_core::protocol::STATUS_UNSUCCESSFUL
            ))),
        }
    ));
}

// -- Phase 4b-v: hvsock + unload + modify-connection + pause ----

#[test]
fn unload_from_connected_posts_and_transitions_to_disconnecting() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    core.step(
        Event::Unload {
            request_id: RequestId(2000),
        },
        &mut sink,
    );
    assert_eq!(sink.actions.len(), 1);
    let _ = expect_post(&sink.actions[0]);
    assert!(matches!(
        core.phase(),
        ClientPhase::Disconnecting {
            request_id: RequestId(2000),
            ..
        }
    ));
}

#[test]
fn unload_while_requesting_offers_cancels_offer_request() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    core.step(
        Event::RequestOffers {
            request_id: RequestId(2001),
        },
        &mut sink,
    );
    sink.actions.clear();

    core.step(
        Event::Unload {
            request_id: RequestId(2002),
        },
        &mut sink,
    );

    assert!(matches!(
        sink.actions.as_slice(),
        [
            Action::Complete {
                request_id: RequestId(2001),
                result: CompletionResult::RequestOffers(Err(ConnectError::InvalidState)),
            },
            Action::PostMessage(_),
        ]
    ));
    assert!(matches!(
        core.phase(),
        ClientPhase::Disconnecting {
            request_id: RequestId(2002),
            ..
        }
    ));
    assert_eq!(core.pending_request_count(), 1);
}

#[test]
fn unload_from_disconnected_completes_synchronously() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    core.step(
        Event::Unload {
            request_id: RequestId(2001),
        },
        &mut sink,
    );
    assert_eq!(sink.actions.len(), 1);
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id: RequestId(2001),
            result: CompletionResult::Unload,
        }
    ));
}

#[test]
fn unload_complete_returns_to_disconnected() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    core.step(
        Event::Unload {
            request_id: RequestId(2100),
        },
        &mut sink,
    );
    sink.actions.clear();

    let wire = make_host_message(&vmbus_core::protocol::UnloadComplete {});
    core.step(Event::HostMessage(&wire), &mut sink);
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id: RequestId(2100),
            result: CompletionResult::Unload,
        }
    ));
    assert!(matches!(core.phase(), ClientPhase::Disconnected));
}

#[test]
fn modify_connection_without_feature_flag_fails() {
    // make_config() does not enable modify_connection.
    let mut core = ClientCore::new(make_config());
    let mut sink = Recording::default();
    connect_with_flags(&mut core, &mut sink, FeatureFlags::new());
    core.step(
        Event::ModifyConnection {
            request_id: RequestId(2200),
            monitor_page: MonitorPageGpas {
                parent_to_child: 0x1000,
                child_to_parent: 0x2000,
            },
        },
        &mut sink,
    );
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id: RequestId(2200),
            result: CompletionResult::ModifyConnection(_),
        }
    ));
}

#[test]
fn modify_connection_response_completes_pending() {
    // Config supports modify_connection.
    let mut core = ClientCore::new(make_multi_version_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        FeatureFlags::new().with_modify_connection(true),
    );
    core.step(
        Event::ModifyConnection {
            request_id: RequestId(2300),
            monitor_page: MonitorPageGpas {
                parent_to_child: 0x1000,
                child_to_parent: 0x2000,
            },
        },
        &mut sink,
    );
    let _ = expect_post(&sink.actions[0]);
    sink.actions.clear();

    let response = vmbus_core::protocol::ModifyConnectionResponse {
        connection_state: vmbus_core::protocol::ConnectionState::SUCCESSFUL,
    };
    let wire = make_host_message(&response);
    core.step(Event::HostMessage(&wire), &mut sink);
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id: RequestId(2300),
            result: CompletionResult::ModifyConnection(_),
        }
    ));
}

#[test]
fn duplicate_modify_connection_is_rejected() {
    let mut core = ClientCore::new(make_multi_version_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        FeatureFlags::new().with_modify_connection(true),
    );
    core.step(
        Event::ModifyConnection {
            request_id: RequestId(2400),
            monitor_page: MonitorPageGpas::default(),
        },
        &mut sink,
    );
    sink.actions.clear();
    core.step(
        Event::ModifyConnection {
            request_id: RequestId(2401),
            monitor_page: MonitorPageGpas::default(),
        },
        &mut sink,
    );
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id: RequestId(2401),
            result: CompletionResult::ModifyConnection(_),
        }
    ));
}

#[test]
fn modify_channel_before_iron_is_rejected_without_posting() {
    let mut core = ClientCore::new(Config {
        sint: vmbus_core::VMBUS_SINT,
        vtl: 0,
        supported_versions: &[Version::Win10Rs5],
        supported_feature_flags: FeatureFlags::new(),
    });
    let mut sink = Recording::default();
    connect_with_flags(&mut core, &mut sink, FeatureFlags::new());
    deliver_one_offer(&mut core, &mut sink, 24);

    core.step(
        Event::ModifyChannel {
            request_id: RequestId(2420),
            channel_id: vmbus_core::protocol::ChannelId(24),
            request: ModifyRequest::TargetVp { target_vp: 1 },
        },
        &mut sink,
    );

    assert!(matches!(
        sink.actions.as_slice(),
        [Action::Complete {
            request_id: RequestId(2420),
            result: CompletionResult::ModifyChannel(-1),
        }]
    ));
    assert_eq!(core.pending_request_count(), 0);
}

#[test]
fn hvsock_connect_posts_and_tracks_pending() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    let request = HvsockConnectRequest {
        service_id: Guid {
            data1: 0xdead,
            ..Guid::ZERO
        },
        endpoint_id: Guid {
            data1: 0xbeef,
            ..Guid::ZERO
        },
        silo_id: Guid::ZERO,
        hosted_silo_unaware: false,
    };
    core.step(
        Event::HvsockConnect {
            request_id: RequestId(2500),
            request,
        },
        &mut sink,
    );
    assert_eq!(sink.actions.len(), 1);
    let _ = expect_post(&sink.actions[0]);
}

#[test]
fn hvsock_connect_before_win10_rs5_is_rejected_without_posting() {
    let mut core = ClientCore::new(Config {
        sint: vmbus_core::VMBUS_SINT,
        vtl: 0,
        supported_versions: &[Version::Win10Rs4],
        supported_feature_flags: FeatureFlags::new(),
    });
    let mut sink = Recording::default();
    connect_with_flags(&mut core, &mut sink, FeatureFlags::new());

    core.step(
        Event::HvsockConnect {
            request_id: RequestId(2501),
            request: HvsockConnectRequest {
                service_id: Guid {
                    data1: 0xdead,
                    ..Guid::ZERO
                },
                endpoint_id: Guid {
                    data1: 0xbeef,
                    ..Guid::ZERO
                },
                silo_id: Guid::ZERO,
                hosted_silo_unaware: false,
            },
        },
        &mut sink,
    );

    assert!(matches!(
        sink.actions.as_slice(),
        [Action::Complete {
            request_id: RequestId(2501),
            result: CompletionResult::HvsockConnect(None),
        }]
    ));
    assert_eq!(core.pending_hvsock_request_count(), 0);
}

#[test]
fn prepare_save_completes_unsaveable_requests() {
    let mut core = ClientCore::new(make_multi_version_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        FeatureFlags::new().with_modify_connection(true),
    );
    core.step(
        Event::ModifyConnection {
            request_id: RequestId(2510),
            monitor_page: MonitorPageGpas::default(),
        },
        &mut sink,
    );
    core.step(
        Event::HvsockConnect {
            request_id: RequestId(2511),
            request: HvsockConnectRequest {
                service_id: Guid {
                    data1: 0xdead,
                    ..Guid::ZERO
                },
                endpoint_id: Guid {
                    data1: 0xbeef,
                    ..Guid::ZERO
                },
                silo_id: Guid::ZERO,
                hosted_silo_unaware: false,
            },
        },
        &mut sink,
    );

    sink.actions.clear();
    core.step(Event::PrepareSave, &mut sink);

    assert!(sink.actions.iter().any(|action| matches!(
        action,
        Action::Complete {
            request_id: RequestId(2510),
            result: CompletionResult::ModifyConnection(
                vmbus_core::protocol::ConnectionState::FAILED_UNKNOWN_FAILURE
            ),
        }
    )));
    assert!(sink.actions.iter().any(|action| matches!(
        action,
        Action::Complete {
            request_id: RequestId(2511),
            result: CompletionResult::HvsockConnect(None),
        }
    )));
    assert!(core.modify_connection_pending());
    assert_eq!(core.pending_request_count(), 0);
    assert_eq!(core.pending_hvsock_request_count(), 1);
    let _ = core.save();

    sink.actions.clear();
    core.step(Event::PrepareSave, &mut sink);
    assert!(sink.actions.is_empty());
    let _ = core.save();
}

#[test]
fn prepare_save_restore_keeps_modify_slot_until_late_response() {
    let mut core = ClientCore::new(make_multi_version_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        FeatureFlags::new().with_modify_connection(true),
    );
    core.step(
        Event::ModifyConnection {
            request_id: RequestId(2520),
            monitor_page: MonitorPageGpas::default(),
        },
        &mut sink,
    );
    core.step(Event::PrepareSave, &mut sink);
    let saved = core.save();
    core.restore(saved).unwrap();
    sink.actions.clear();

    core.step(
        Event::ModifyConnection {
            request_id: RequestId(2521),
            monitor_page: MonitorPageGpas::default(),
        },
        &mut sink,
    );
    assert!(matches!(
        sink.actions.as_slice(),
        [Action::Complete {
            request_id: RequestId(2521),
            result: CompletionResult::ModifyConnection(
                vmbus_core::protocol::ConnectionState::FAILED_UNKNOWN_FAILURE
            ),
        }]
    ));
    sink.actions.clear();

    let late = make_host_message(&vmbus_core::protocol::ModifyConnectionResponse {
        connection_state: vmbus_core::protocol::ConnectionState::FAILED_LOW_RESOURCES,
    });
    core.step(Event::HostMessage(&late), &mut sink);
    assert!(sink.actions.is_empty());
    assert!(!core.modify_connection_pending());
    assert_eq!(core.pending_request_count(), 0);

    core.step(
        Event::ModifyConnection {
            request_id: RequestId(2522),
            monitor_page: MonitorPageGpas::default(),
        },
        &mut sink,
    );
    assert!(matches!(sink.actions.as_slice(), [Action::PostMessage(_)]));
    sink.actions.clear();

    let response = make_host_message(&vmbus_core::protocol::ModifyConnectionResponse {
        connection_state: vmbus_core::protocol::ConnectionState::SUCCESSFUL,
    });
    core.step(Event::HostMessage(&response), &mut sink);
    assert!(matches!(
        sink.actions.as_slice(),
        [Action::Complete {
            request_id: RequestId(2522),
            result: CompletionResult::ModifyConnection(
                vmbus_core::protocol::ConnectionState::SUCCESSFUL
            ),
        }]
    ));
    assert!(!core.modify_connection_pending());
    assert_eq!(core.pending_request_count(), 0);
}

#[test]
fn hvsock_offer_completes_pending_connect() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    let service = Guid {
        data1: 0xdead,
        ..Guid::ZERO
    };
    let endpoint = Guid {
        data1: 0xbeef,
        ..Guid::ZERO
    };
    core.step(
        Event::HvsockConnect {
            request_id: RequestId(2600),
            request: HvsockConnectRequest {
                service_id: service,
                endpoint_id: endpoint,
                silo_id: Guid::ZERO,
                hosted_silo_unaware: false,
            },
        },
        &mut sink,
    );
    sink.actions.clear();

    // Craft a matching offer.
    let mut offer = make_offer(60);
    offer.interface_id = service;
    offer.instance_id = endpoint;
    offer.flags = vmbus_core::protocol::OfferFlags::new().with_tlnpi_provider(true);
    // user_defined is already zeroed (is_for_guest_accept = 0).
    let wire = make_host_message(&offer);
    core.step(Event::HostMessage(&wire), &mut sink);
    // Exactly one Complete for the hvsock request; no
    // OfferReceived (because the offer was consumed by the
    // hvsock intercept).
    assert!(sink.actions.iter().any(|a| matches!(
        a,
        Action::Complete {
            request_id: RequestId(2600),
            result: CompletionResult::HvsockConnect(Some(_)),
        }
    )));
    assert!(
        !sink
            .actions
            .iter()
            .any(|a| matches!(a, Action::OfferReceived(_)))
    );
}

#[test]
fn tl_connect_result_failure_completes_with_none() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    let service = Guid {
        data1: 0xf00d,
        ..Guid::ZERO
    };
    let endpoint = Guid {
        data1: 0xbaad,
        ..Guid::ZERO
    };
    core.step(
        Event::HvsockConnect {
            request_id: RequestId(2700),
            request: HvsockConnectRequest {
                service_id: service,
                endpoint_id: endpoint,
                silo_id: Guid::ZERO,
                hosted_silo_unaware: false,
            },
        },
        &mut sink,
    );
    sink.actions.clear();

    let result = vmbus_core::protocol::TlConnectResult {
        endpoint_id: endpoint,
        service_id: service,
        status: -1,
    };
    let wire = make_host_message(&result);
    core.step(Event::HostMessage(&wire), &mut sink);
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id: RequestId(2700),
            result: CompletionResult::HvsockConnect(None),
        }
    ));
}

#[test]
fn duplicate_hvsock_connects_complete_one_at_a_time() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    let service = Guid {
        data1: 0xface,
        ..Guid::ZERO
    };
    let endpoint = Guid {
        data1: 0xcafe,
        ..Guid::ZERO
    };
    let request = HvsockConnectRequest {
        service_id: service,
        endpoint_id: endpoint,
        silo_id: Guid::ZERO,
        hosted_silo_unaware: false,
    };
    for request_id in [2800, 2801] {
        core.step(
            Event::HvsockConnect {
                request_id: RequestId(request_id),
                request,
            },
            &mut sink,
        );
    }
    sink.actions.clear();

    let mut offer = make_offer(61);
    offer.interface_id = service;
    offer.instance_id = endpoint;
    offer.flags = vmbus_core::protocol::OfferFlags::new().with_tlnpi_provider(true);
    core.step(Event::HostMessage(&make_host_message(&offer)), &mut sink);
    assert!(matches!(
        sink.actions.as_slice(),
        [Action::Complete {
            request_id: RequestId(2800),
            result: CompletionResult::HvsockConnect(Some(_)),
        }]
    ));
    sink.actions.clear();

    let result = vmbus_core::protocol::TlConnectResult {
        endpoint_id: endpoint,
        service_id: service,
        status: -1,
    };
    core.step(Event::HostMessage(&make_host_message(&result)), &mut sink);
    assert!(matches!(
        sink.actions.as_slice(),
        [Action::Complete {
            request_id: RequestId(2801),
            result: CompletionResult::HvsockConnect(None),
        }]
    ));
}

#[test]
fn disconnected_restore_discards_saved_channels() {
    let mut core = ClientCore::new(make_redirect_config());
    core.restore(SavedState {
        version: None,
        channels: alloc::vec![SavedChannel {
            offer: make_offer(70),
            phase: SavedChannelPhase::Offered,
            gpadls: alloc::vec![SavedGpadl {
                id: vmbus_core::protocol::GpadlId(1),
                phase: SavedGpadlPhase::Created,
            }],
        }],
        released_channel_ids: Vec::new(),
        modify_connection_pending: true,
        hvsock_requests: alloc::vec![SavedHvsockRequest {
            service_id: Guid::ZERO,
            endpoint_id: Guid::ZERO,
        }],
    })
    .unwrap();

    assert!(matches!(core.phase(), ClientPhase::Disconnected));
    assert!(core.channels().is_empty());
    assert!(!core.modify_connection_pending());
    assert_eq!(core.pending_hvsock_request_count(), 0);
}

#[test]
fn post_restore_replays_deferred_channel_releases() {
    let config = make_redirect_config();
    let version = VersionInfo {
        version: Version::Copper,
        feature_flags: config.supported_feature_flags,
    };
    let mut core = ClientCore::new(config);
    let channel_id = vmbus_core::protocol::ChannelId(71);
    core.restore(SavedState {
        version: Some(version),
        channels: Vec::new(),
        released_channel_ids: alloc::vec![channel_id],
        modify_connection_pending: false,
        hvsock_requests: Vec::new(),
    })
    .unwrap();

    let mut sink = Recording::default();
    core.post_restore(&mut sink as &mut dyn ActionSink);

    assert_eq!(
        expect_post(&sink.actions[0]),
        make_host_message(&vmbus_core::protocol::RelIdReleased { channel_id })
    );
    assert!(core.save().released_channel_ids.is_empty());
}

#[test]
fn cancelled_hvsock_responses_keep_fifo_across_restore() {
    for late_success in [false, true] {
        let config = make_redirect_config();
        let mut core = ClientCore::new(config.clone());
        let mut sink = Recording::default();
        connect_with_flags(&mut core, &mut sink, config.supported_feature_flags);
        let request = HvsockConnectRequest {
            service_id: Guid {
                data1: 1,
                ..Guid::ZERO
            },
            endpoint_id: Guid {
                data1: 2,
                ..Guid::ZERO
            },
            silo_id: Guid::ZERO,
            hosted_silo_unaware: false,
        };
        for request_id in [RequestId(1), RequestId(2)] {
            core.step(
                Event::HvsockConnect {
                    request_id,
                    request,
                },
                &mut sink,
            );
        }
        core.step(Event::PrepareSave, &mut sink);
        let saved = core.save();
        assert_eq!(saved.hvsock_requests.len(), 2);
        let mut restored = ClientCore::new(config);
        restored.restore(saved).unwrap();
        sink.actions.clear();

        // A new instance can reuse a local request ID, but not an old wire reply.
        restored.step(
            Event::HvsockConnect {
                request_id: RequestId(1),
                request,
            },
            &mut sink,
        );
        assert!(matches!(
            sink.actions.as_slice(),
            [Action::Complete {
                request_id: RequestId(1),
                result: CompletionResult::HvsockConnect(None),
            }]
        ));
        sink.actions.clear();
        for channel_id in [80, 81] {
            if late_success {
                let mut offer = make_offer(channel_id);
                offer.interface_id = request.service_id;
                offer.instance_id = request.endpoint_id;
                offer.flags = vmbus_core::protocol::OfferFlags::new().with_tlnpi_provider(true);
                restored.step(Event::HostMessage(&make_host_message(&offer)), &mut sink);
                assert!(matches!(sink.actions.as_slice(), [Action::PostMessage(_)]));
                assert_eq!(
                    expect_post(&sink.actions[0]),
                    make_host_message(&vmbus_core::protocol::RelIdReleased {
                        channel_id: offer.channel_id,
                    })
                );
                assert!(!restored.channels().contains_key(&offer.channel_id));
            } else {
                let response = vmbus_core::protocol::TlConnectResult {
                    service_id: request.service_id,
                    endpoint_id: request.endpoint_id,
                    status: -1,
                };
                restored.step(Event::HostMessage(&make_host_message(&response)), &mut sink);
                assert!(sink.actions.is_empty());
            }
            sink.actions.clear();
        }
        assert_eq!(restored.pending_hvsock_request_count(), 0);
        restored.step(
            Event::HvsockConnect {
                request_id: RequestId(1),
                request,
            },
            &mut sink,
        );
        assert!(matches!(sink.actions.as_slice(), [Action::PostMessage(_)]));
        sink.actions.clear();
        let mut offer = make_offer(82);
        offer.interface_id = request.service_id;
        offer.instance_id = request.endpoint_id;
        offer.flags = vmbus_core::protocol::OfferFlags::new().with_tlnpi_provider(true);
        restored.step(Event::HostMessage(&make_host_message(&offer)), &mut sink);
        assert!(matches!(
            sink.actions.as_slice(),
            [Action::Complete {
                request_id: RequestId(1),
                result: CompletionResult::HvsockConnect(Some(descriptor)),
            }] if descriptor.offer.channel_id == offer.channel_id
        ));
        assert_eq!(restored.pending_request_count(), 0);
        assert_eq!(restored.pending_hvsock_request_count(), 0);
    }
}

#[test]
fn reset_notifies_removed_channels() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_and_offer(&mut core, &mut sink, 80);
    deliver_one_offer(&mut core, &mut sink, 81);
    core.step(Event::Reset, &mut sink);
    assert!(matches!(
        sink.actions.as_slice(),
        [
            Action::ChannelRemoved {
                channel_id: vmbus_core::protocol::ChannelId(80)
            },
            Action::ChannelRemoved {
                channel_id: vmbus_core::protocol::ChannelId(81)
            }
        ]
    ));
    assert!(core.channels().is_empty());
}

#[test]
fn pause_and_resume_post_wire_messages() {
    let mut core = ClientCore::new(make_redirect_config());
    let mut sink = Recording::default();
    connect_with_flags(
        &mut core,
        &mut sink,
        make_redirect_config().supported_feature_flags,
    );
    core.step(Event::Pause, &mut sink);
    core.step(Event::Resume, &mut sink);
    assert_eq!(sink.actions.len(), 2);
    let _ = expect_post(&sink.actions[0]);
    let _ = expect_post(&sink.actions[1]);
}
