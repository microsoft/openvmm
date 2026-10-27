// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

extern crate std;

use super::*;
use alloc::vec::Vec;

#[derive(Default)]
struct Recording {
    actions: Vec<Action>,
}

impl ActionSink for Recording {
    fn emit(&mut self, action: Action) {
        self.actions.push(action);
    }
}

fn make_host_message<T>(msg: &T) -> Vec<u8>
where
    T: zerocopy::IntoBytes
        + zerocopy::Immutable
        + zerocopy::KnownLayout
        + vmbus_core::protocol::VmbusMessage,
{
    vmbus_core::OutgoingMessage::new(msg).data().to_vec()
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

fn count<F>(actions: &[Action], f: F) -> usize
where
    F: Fn(&Action) -> bool,
{
    actions.iter().filter(|a| f(a)).count()
}

/// Full connect → request-offers → open two channels → close +
/// release both → unload cycle. Drives every state transition
/// exactly once and checks the final state is
/// `ClientPhase::Disconnected` with no live channels.
#[test]
fn full_lifecycle_connect_offers_open_close_unload() {
    let config = Config {
        sint: vmbus_core::VMBUS_SINT,
        vtl: 0,
        supported_versions: &[Version::Copper],
        supported_feature_flags: FeatureFlags::new()
            .with_guest_specified_signal_parameters(true)
            .with_channel_interrupt_redirection(true)
            .with_modify_connection(true),
    };
    let mut core = ClientCore::new(config.clone());
    let mut sink = Recording::default();

    // 1. Connect
    let connect_rid = RequestId(1);
    core.step(
        Event::Connect {
            request_id: connect_rid,
            params: ConnectParams {
                target_message_vp: 0,
                monitor_page: None,
                client_id: Guid::ZERO,
            },
        },
        &mut sink,
    );
    assert_eq!(
        count(&sink.actions, |a| matches!(a, Action::PostMessage(_))),
        1
    );
    sink.actions.clear();

    // 2. VersionResponse ok
    let response = vmbus_core::protocol::VersionResponse2 {
        version_response: vmbus_core::protocol::VersionResponse {
            version_supported: 1,
            connection_state: vmbus_core::protocol::ConnectionState::SUCCESSFUL,
            padding: 0,
            selected_version_or_connection_id: 1,
        },
        supported_features: config.supported_feature_flags.into(),
    };
    core.step(Event::HostMessage(&make_host_message(&response)), &mut sink);
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id,
            result: CompletionResult::Connect(Ok(_)),
        } if *request_id == connect_rid
    ));
    assert!(matches!(core.phase(), ClientPhase::Connected { .. }));
    sink.actions.clear();

    // 3. RequestOffers
    let offers_rid = RequestId(2);
    core.step(
        Event::RequestOffers {
            request_id: offers_rid,
        },
        &mut sink,
    );
    assert!(matches!(core.phase(), ClientPhase::RequestingOffers { .. }));
    sink.actions.clear();

    // 4. Two offers
    for channel_id in [10, 11] {
        core.step(
            Event::HostMessage(&make_host_message(&make_offer(channel_id))),
            &mut sink,
        );
    }
    assert_eq!(
        count(&sink.actions, |a| matches!(a, Action::OfferReceived(_))),
        2
    );
    sink.actions.clear();

    // 5. AllOffersDelivered
    core.step(
        Event::HostMessage(&make_host_message(
            &vmbus_core::protocol::AllOffersDelivered {},
        )),
        &mut sink,
    );
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            result: CompletionResult::RequestOffers(Ok(())),
            ..
        }
    ));
    assert!(matches!(core.phase(), ClientPhase::Connected { .. }));
    sink.actions.clear();

    // 6. Open both channels
    for (i, channel_id) in [10u32, 11u32].iter().enumerate() {
        let rid = RequestId(100 + i as u64);
        core.step(
            Event::OpenChannel {
                request_id: rid,
                channel_id: vmbus_core::protocol::ChannelId(*channel_id),
                open: OpenChannelParams {
                    target_vp: None,
                    ring_offset: 0,
                    ring_gpadl_id: vmbus_core::protocol::GpadlId(*channel_id + 1),
                    event_flag: *channel_id as u16,
                    connection_id: *channel_id,
                    redirected_event_flag: None,
                    user_data: vmbus_core::protocol::UserDefinedData::default(),
                },
            },
            &mut sink,
        );
    }
    sink.actions.clear();

    // 7. OpenResult ok for both
    for channel_id in [10, 11] {
        let result = vmbus_core::protocol::OpenResult {
            channel_id: vmbus_core::protocol::ChannelId(channel_id),
            open_id: 0,
            status: vmbus_core::protocol::STATUS_SUCCESS as u32,
        };
        core.step(Event::HostMessage(&make_host_message(&result)), &mut sink);
    }
    // Each open produces 3 actions (ConnectionIdAssigned, Opened, Complete).
    assert_eq!(sink.actions.len(), 6);
    for channel_id in [10, 11] {
        let entry = &core.channels()[&vmbus_core::protocol::ChannelId(channel_id)];
        assert!(matches!(entry.phase, ChannelPhase::Opened { .. }));
    }
    sink.actions.clear();

    // 8. Close and release both
    for channel_id in [10, 11] {
        core.step(
            Event::CloseChannel {
                channel_id: vmbus_core::protocol::ChannelId(channel_id),
            },
            &mut sink,
        );
    }
    for channel_id in [10, 11] {
        core.step(
            Event::ReleaseChannel {
                channel_id: vmbus_core::protocol::ChannelId(channel_id),
            },
            &mut sink,
        );
    }
    sink.actions.clear();

    // 9. Host rescinds both (as it does on unload).
    for channel_id in [10, 11] {
        core.step(
            Event::HostMessage(&make_host_message(
                &vmbus_core::protocol::RescindChannelOffer {
                    channel_id: vmbus_core::protocol::ChannelId(channel_id),
                },
            )),
            &mut sink,
        );
    }
    // Since channels are client-released and now revoked, each
    // rescind should trigger RelIdReleased + entry removal
    // + OfferRescinded.
    assert_eq!(core.channels().len(), 0);
    sink.actions.clear();

    // 10. Unload
    let unload_rid = RequestId(999);
    core.step(
        Event::Unload {
            request_id: unload_rid,
        },
        &mut sink,
    );
    assert!(matches!(core.phase(), ClientPhase::Disconnecting { .. }));
    sink.actions.clear();

    // 11. UnloadComplete
    core.step(
        Event::HostMessage(&make_host_message(&vmbus_core::protocol::UnloadComplete {})),
        &mut sink,
    );
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            request_id,
            result: CompletionResult::Unload,
        } if *request_id == unload_rid
    ));
    assert!(matches!(core.phase(), ClientPhase::Disconnected));
}

/// Multi-version ladder: highest version fails, second version
/// succeeds. Verifies the ladder posts once per rung and only
/// completes with success on the winning rung.
#[test]
fn version_ladder_multi_step() {
    let config = Config {
        sint: vmbus_core::VMBUS_SINT,
        vtl: 0,
        supported_versions: &[Version::Iron, Version::Copper],
        supported_feature_flags: FeatureFlags::new(),
    };
    let mut core = ClientCore::new(config);
    let mut sink = Recording::default();

    core.step(
        Event::Connect {
            request_id: RequestId(1),
            params: ConnectParams {
                target_message_vp: 0,
                monitor_page: None,
                client_id: Guid::ZERO,
            },
        },
        &mut sink,
    );
    // Initial post for Copper.
    assert_eq!(
        count(&sink.actions, |a| matches!(a, Action::PostMessage(_))),
        1
    );
    sink.actions.clear();

    // Host rejects Copper.
    let reject = vmbus_core::protocol::VersionResponse2 {
        version_response: vmbus_core::protocol::VersionResponse {
            version_supported: 0,
            connection_state: vmbus_core::protocol::ConnectionState::SUCCESSFUL,
            padding: 0,
            selected_version_or_connection_id: 0,
        },
        supported_features: 0,
    };
    core.step(Event::HostMessage(&make_host_message(&reject)), &mut sink);
    // Ladder posted Iron.
    assert_eq!(
        count(&sink.actions, |a| matches!(a, Action::PostMessage(_))),
        1
    );
    // No Connect completion yet.
    assert_eq!(
        count(&sink.actions, |a| matches!(a, Action::Complete { .. })),
        0
    );
    sink.actions.clear();

    // Host accepts Iron.
    let accept = vmbus_core::protocol::VersionResponse2 {
        version_response: vmbus_core::protocol::VersionResponse {
            version_supported: 1,
            connection_state: vmbus_core::protocol::ConnectionState::SUCCESSFUL,
            padding: 0,
            selected_version_or_connection_id: 1,
        },
        supported_features: 0,
    };
    core.step(Event::HostMessage(&make_host_message(&accept)), &mut sink);
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            result: CompletionResult::Connect(Ok(_)),
            ..
        }
    ));
    let ClientPhase::Connected { version } = *core.phase() else {
        panic!()
    };
    assert_eq!(version.version, Version::Iron);
}

/// Full gpadl lifecycle over an open channel: establish (with a
/// large enough buffer to force a body message), then teardown.
#[test]
fn gpadl_full_lifecycle_over_open_channel() {
    let config = Config {
        sint: vmbus_core::VMBUS_SINT,
        vtl: 0,
        supported_versions: &[Version::Copper],
        supported_feature_flags: FeatureFlags::new().with_guest_specified_signal_parameters(true),
    };
    let mut core = ClientCore::new(config.clone());
    let mut sink = Recording::default();
    core.step(
        Event::Connect {
            request_id: RequestId(1),
            params: ConnectParams {
                target_message_vp: 0,
                monitor_page: None,
                client_id: Guid::ZERO,
            },
        },
        &mut sink,
    );
    let response = vmbus_core::protocol::VersionResponse2 {
        version_response: vmbus_core::protocol::VersionResponse {
            version_supported: 1,
            connection_state: vmbus_core::protocol::ConnectionState::SUCCESSFUL,
            padding: 0,
            selected_version_or_connection_id: 1,
        },
        supported_features: config.supported_feature_flags.into(),
    };
    core.step(Event::HostMessage(&make_host_message(&response)), &mut sink);
    core.step(
        Event::RequestOffers {
            request_id: RequestId(2),
        },
        &mut sink,
    );
    core.step(
        Event::HostMessage(&make_host_message(&make_offer(20))),
        &mut sink,
    );
    core.step(
        Event::HostMessage(&make_host_message(
            &vmbus_core::protocol::AllOffersDelivered {},
        )),
        &mut sink,
    );
    sink.actions.clear();

    // Establish a large GPADL (2 body messages worth).
    let max_hdr = vmbus_core::protocol::GpadlHeader::MAX_DATA_VALUES;
    let max_body = vmbus_core::protocol::GpadlBody::MAX_DATA_VALUES;
    let total = max_hdr + max_body + 2;
    let request = GpadlRequest {
        id: vmbus_core::protocol::GpadlId(77),
        count: 1,
        buf: (0..total).map(|i| i as u64 * 0x1000).collect(),
    };
    core.step(
        Event::EstablishGpadl {
            request_id: RequestId(300),
            channel_id: vmbus_core::protocol::ChannelId(20),
            gpadl_id: vmbus_core::protocol::GpadlId(77),
            request,
        },
        &mut sink,
    );
    // Header + 2 bodies.
    assert_eq!(
        count(&sink.actions, |a| matches!(a, Action::PostMessage(_))),
        3
    );
    sink.actions.clear();

    // GpadlCreated success.
    let created = vmbus_core::protocol::GpadlCreated {
        channel_id: vmbus_core::protocol::ChannelId(20),
        gpadl_id: vmbus_core::protocol::GpadlId(77),
        status: vmbus_core::protocol::STATUS_SUCCESS,
    };
    core.step(Event::HostMessage(&make_host_message(&created)), &mut sink);
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            result: CompletionResult::EstablishGpadl(Ok(())),
            ..
        }
    ));
    sink.actions.clear();

    // Teardown.
    core.step(
        Event::TeardownGpadl {
            request_id: RequestId(400),
            channel_id: vmbus_core::protocol::ChannelId(20),
            gpadl_id: vmbus_core::protocol::GpadlId(77),
        },
        &mut sink,
    );
    assert_eq!(
        count(&sink.actions, |a| matches!(a, Action::PostMessage(_))),
        1
    );
    sink.actions.clear();

    let torndown = vmbus_core::protocol::GpadlTorndown {
        gpadl_id: vmbus_core::protocol::GpadlId(77),
    };
    core.step(Event::HostMessage(&make_host_message(&torndown)), &mut sink);
    assert!(matches!(
        &sink.actions[0],
        Action::Complete {
            result: CompletionResult::TeardownGpadl,
            ..
        }
    ));
    assert!(
        core.channels()[&vmbus_core::protocol::ChannelId(20)]
            .gpadls
            .is_empty()
    );
}
