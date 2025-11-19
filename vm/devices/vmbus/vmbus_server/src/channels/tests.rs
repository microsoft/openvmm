// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::MESSAGE_CONNECTION_ID;

use super::*;
use crate::channels::ConnectionState;
use guid::Guid;
use protocol::VmbusMessage;
use std::collections::VecDeque;
use std::sync::mpsc;
use test_with_tracing::test;
use vmbus_core::protocol::TargetInfo;
use zerocopy::FromBytes;

#[test]
fn test_version_negotiation_not_supported() {
    let mut env = TestEnv::new();

    test_initiate_contact(&mut env, TestVersion::Unsupported(0xffffffff), 0);
}

#[test]
fn test_version_negotiation_success() {
    let mut env = TestEnv::new();

    test_initiate_contact(
        &mut env,
        TestVersion::Supported {
            version: Version::Win10,
            expected_features: 0,
        },
        0,
    );
}

#[test]
fn test_version_negotiation_multiclient_sint() {
    let mut env = TestEnv::new();

    let target_info = TargetInfo::new()
        .with_sint(3)
        .with_vtl(0)
        .with_feature_flags(FeatureFlags::new().into());

    env.c()
        .handle_synic_message(in_msg_ex(
            protocol::MessageType::INITIATE_CONTACT,
            protocol::InitiateContact {
                version_requested: Version::Win10Rs3_1 as u32,
                target_message_vp: 0,
                interrupt_page_or_target_info: target_info.into(),
                parent_to_child_monitor_page_gpa: 0,
                child_to_parent_monitor_page_gpa: 0,
            },
            true,
            false,
        ))
        .unwrap();

    // No action is taken when a different SINT is requested, since it's not supported. An
    // unsupported message is sent to the requested SINT.
    assert!(env.notifier.modify_requests.is_empty());
    assert!(matches!(env.server.state, ConnectionState::Disconnected));
    env.notifier.check_message_with_target(
        OutgoingMessage::new(&protocol::VersionResponse {
            version_supported: 0,
            connection_state: protocol::ConnectionState::SUCCESSFUL,
            padding: 0,
            selected_version_or_connection_id: 0,
        }),
        MessageTarget::Custom(ConnectionTarget { vp: 0, sint: 3 }),
    );

    // SINT is ignored if the multiclient port is not used.
    test_initiate_contact(
        &mut env,
        TestVersion::Supported {
            version: Version::Win10Rs3_1,
            expected_features: 0,
        },
        target_info.into(),
    );
}

#[test]
fn test_version_negotiation_multiclient_vtl() {
    let mut env = TestEnv::new();

    let target_info = TargetInfo::new()
        .with_sint(VMBUS_SINT)
        .with_vtl(2)
        .with_feature_flags(FeatureFlags::new().into());

    env.c()
        .handle_synic_message(in_msg_ex(
            protocol::MessageType::INITIATE_CONTACT,
            protocol::InitiateContact {
                version_requested: Version::Win10Rs4 as u32,
                target_message_vp: 0,
                interrupt_page_or_target_info: target_info.into(),
                parent_to_child_monitor_page_gpa: 0,
                child_to_parent_monitor_page_gpa: 0,
            },
            true,
            false,
        ))
        .unwrap();

    let action = env.notifier.forward_request.take().unwrap();
    assert!(matches!(action, InitiateContactRequest { .. }));

    // The VTL contact message was forwarded but no action was taken by this server.
    assert!(env.notifier.messages.is_empty());
    assert!(matches!(env.server.state, ConnectionState::Disconnected));

    // VTL is ignored if the multiclient port is not used.
    test_initiate_contact(
        &mut env,
        TestVersion::Supported {
            version: Version::Win10Rs4,
            expected_features: 0,
        },
        target_info.into(),
    );

    assert!(env.notifier.forward_request.is_none());
}

#[test]
fn test_version_negotiation_feature_flags() {
    let mut env = TestEnv::new();

    // Test with no feature flags.
    let mut target_info = TargetInfo::new()
        .with_sint(VMBUS_SINT)
        .with_vtl(0)
        .with_feature_flags(FeatureFlags::new().into());
    test_initiate_contact(
        &mut env,
        TestVersion::Supported {
            version: Version::Copper,
            expected_features: 0,
        },
        target_info.into(),
    );

    env.c().handle_unload();
    env.complete_reset();
    env.notifier.messages.clear();
    // Request supported feature flags.
    target_info.set_feature_flags(
        FeatureFlags::new()
            .with_guest_specified_signal_parameters(true)
            .into(),
    );
    test_initiate_contact(
        &mut env,
        TestVersion::Supported {
            version: Version::Copper,
            expected_features: FeatureFlags::new()
                .with_guest_specified_signal_parameters(true)
                .into(),
        },
        target_info.into(),
    );

    env.c().handle_unload();
    env.complete_reset();
    env.notifier.messages.clear();
    // Request unsupported feature flags. This will succeed and report back the supported ones.
    target_info.set_feature_flags(
        u32::from(FeatureFlags::new().with_guest_specified_signal_parameters(true)) | 0xf0000000,
    );
    test_initiate_contact(
        &mut env,
        TestVersion::Supported {
            version: Version::Copper,
            expected_features: FeatureFlags::new()
                .with_guest_specified_signal_parameters(true)
                .into(),
        },
        target_info.into(),
    );

    env.c().handle_unload();
    env.complete_reset();
    env.notifier.messages.clear();
    // Verify client ID feature flag.
    target_info.set_feature_flags(FeatureFlags::new().with_client_id(true).into());
    test_initiate_contact(
        &mut env,
        TestVersion::Supported {
            version: Version::Copper,
            expected_features: FeatureFlags::new().with_client_id(true).into(),
        },
        target_info.into(),
    );
}

#[test]
fn test_version_negotiation_interrupt_page() {
    let mut env = TestEnv::new();
    test_initiate_contact(
        &mut env,
        TestVersion::Supported {
            version: Version::V1,
            expected_features: 0,
        },
        1234,
    );

    let mut env = TestEnv::new();
    test_initiate_contact(
        &mut env,
        TestVersion::Supported {
            version: Version::Win7,
            expected_features: 0,
        },
        1234,
    );

    let mut env = TestEnv::new();
    test_initiate_contact(
        &mut env,
        TestVersion::Supported {
            version: Version::Win8,
            expected_features: 0,
        },
        1234,
    );
}

enum TestVersion {
    Unsupported(u32),
    Supported {
        version: Version,
        expected_features: u32,
    },
}

fn test_initiate_contact(env: &mut TestEnv, version: TestVersion, target_info: u64) {
    let raw_version = match version {
        TestVersion::Unsupported(version) => version,
        TestVersion::Supported { version, .. } => version as u32,
    };
    env.c()
        .handle_synic_message(in_msg(
            protocol::MessageType::INITIATE_CONTACT,
            protocol::InitiateContact2 {
                initiate_contact: protocol::InitiateContact {
                    version_requested: raw_version,
                    target_message_vp: 1,
                    interrupt_page_or_target_info: target_info,
                    parent_to_child_monitor_page_gpa: 0,
                    child_to_parent_monitor_page_gpa: 0,
                },
                client_id: guid::guid!("e6e6e6e6-e6e6-e6e6-e6e6-e6e6e6e6e6e6"),
            },
        ))
        .unwrap();

    let (expect_supported, selected_version_or_connection_id, expected_features) =
        if let TestVersion::Supported {
            version,
            expected_features,
        } = version
        {
            let request = env.notifier.next_action();
            let interrupt_page = if raw_version < Version::Win8 as u32 {
                Update::Set(target_info)
            } else {
                Update::Reset
            };

            let target_message_vp = if raw_version < Version::Win8_1 as u32 {
                Some(0)
            } else {
                Some(1)
            };

            assert_eq!(
                request,
                ModifyConnectionRequest {
                    version: Some(VersionInfo {
                        version,
                        feature_flags: expected_features.into()
                    }),
                    monitor_page: Update::Reset,
                    interrupt_page,
                    target_message_vp,
                    ..Default::default()
                }
            );

            env.c()
                .complete_initiate_contact(ModifyConnectionResponse::Supported(
                    protocol::ConnectionState::SUCCESSFUL,
                    SUPPORTED_FEATURE_FLAGS,
                    None,
                ));

            let selected_version_or_connection_id = if raw_version >= Version::Win10Rs3_1 as u32 {
                1
            } else {
                raw_version
            };

            (true, selected_version_or_connection_id, expected_features)
        } else {
            (false, 0, 0)
        };

    let version_response = protocol::VersionResponse {
        version_supported: if expect_supported { 1 } else { 0 },
        connection_state: protocol::ConnectionState::SUCCESSFUL,
        padding: 0,
        selected_version_or_connection_id,
    };

    if raw_version >= Version::Copper as u32 && expect_supported {
        env.notifier
            .check_message(OutgoingMessage::new(&protocol::VersionResponse2 {
                version_response,
                supported_features: expected_features,
            }));
    } else {
        env.notifier
            .check_message(OutgoingMessage::new(&version_response));
        assert_eq!(expected_features, 0);
    }

    assert!(env.notifier.messages.is_empty());
    if expect_supported {
        assert!(matches!(
            env.server.state,
            ConnectionState::Connected { .. }
        ));
        if raw_version < Version::Win8_1 as u32 {
            assert_eq!(Some(0), env.notifier.target_message_vp);
        } else {
            assert_eq!(Some(1), env.notifier.target_message_vp);
        }
    } else {
        assert!(matches!(env.server.state, ConnectionState::Disconnected));
        assert!(env.notifier.target_message_vp.is_none());
    }

    if raw_version < Version::Win8 as u32 {
        assert_eq!(env.notifier.interrupt_page, Some(target_info));
    } else {
        assert!(env.notifier.interrupt_page.is_none());
    }
}

#[test]
fn test_channel_lifetime() {
    test_channel_lifetime_helper(Version::Win10Rs5, FeatureFlags::new());
}

#[test]
fn test_channel_lifetime_iron() {
    test_channel_lifetime_helper(Version::Iron, FeatureFlags::new());
}

#[test]
fn test_channel_lifetime_copper() {
    test_channel_lifetime_helper(Version::Copper, FeatureFlags::new());
}

#[test]
fn test_channel_lifetime_copper_guest_signal() {
    test_channel_lifetime_helper(
        Version::Copper,
        FeatureFlags::new().with_guest_specified_signal_parameters(true),
    );
}

#[test]
fn test_channel_lifetime_copper_open_flags() {
    test_channel_lifetime_helper(
        Version::Copper,
        FeatureFlags::new().with_channel_interrupt_redirection(true),
    );
}

fn test_channel_lifetime_helper(version: Version, feature_flags: FeatureFlags) {
    let mut env = TestEnv::new();
    let interface_id = Guid::new_random();
    let instance_id = Guid::new_random();
    let offer_id = env
        .c()
        .offer_channel(OfferParamsInternal {
            interface_name: "test".to_owned(),
            instance_id,
            interface_id,
            ..Default::default()
        })
        .unwrap();

    let mut target_info = TargetInfo::new()
        .with_sint(VMBUS_SINT)
        .with_vtl(2)
        .with_feature_flags(FeatureFlags::new().into());
    if version >= Version::Copper {
        target_info.set_feature_flags(feature_flags.into());
    }

    env.c()
        .handle_synic_message(in_msg(
            protocol::MessageType::INITIATE_CONTACT,
            protocol::InitiateContact {
                version_requested: version as u32,
                target_message_vp: 0,
                interrupt_page_or_target_info: target_info.into(),
                parent_to_child_monitor_page_gpa: 0,
                child_to_parent_monitor_page_gpa: 0,
            },
        ))
        .unwrap();

    let request = env.notifier.next_action();
    assert_eq!(
        request,
        ModifyConnectionRequest {
            version: Some(VersionInfo {
                version,
                feature_flags
            }),
            monitor_page: Update::Reset,
            interrupt_page: Update::Reset,
            target_message_vp: Some(0),
            ..Default::default()
        }
    );

    env.c()
        .complete_initiate_contact(ModifyConnectionResponse::Supported(
            protocol::ConnectionState::SUCCESSFUL,
            SUPPORTED_FEATURE_FLAGS,
            None,
        ));

    let version_response = protocol::VersionResponse {
        version_supported: 1,
        selected_version_or_connection_id: 1,
        ..FromZeros::new_zeroed()
    };

    if version >= Version::Copper {
        env.notifier
            .check_message(OutgoingMessage::new(&protocol::VersionResponse2 {
                version_response,
                supported_features: feature_flags.into(),
            }));
    } else {
        env.notifier
            .check_message(OutgoingMessage::new(&version_response));
    }

    env.c()
        .handle_synic_message(in_msg(protocol::MessageType::REQUEST_OFFERS, ()))
        .unwrap();

    let channel_id = ChannelId(1);
    env.notifier.check_messages([
        OutgoingMessage::new(&protocol::OfferChannel {
            interface_id,
            instance_id,
            channel_id,
            connection_id: 0x2001,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        }),
        OutgoingMessage::new(&protocol::AllOffersDelivered {}),
    ]);

    let open_channel = protocol::OpenChannel {
        channel_id,
        open_id: 1,
        ring_buffer_gpadl_id: GpadlId(1),
        target_vp: 3,
        downstream_ring_buffer_page_offset: 2,
        user_data: UserDefinedData::new_zeroed(),
    };

    let mut event_flag = 1;
    let mut connection_id = 0x2001;
    let mut expected_flags = protocol::OpenChannelFlags::new();
    if version >= Version::Copper
        && (feature_flags.guest_specified_signal_parameters()
            || feature_flags.channel_interrupt_redirection())
    {
        if feature_flags.channel_interrupt_redirection() {
            expected_flags.set_redirect_interrupt(true);
        }

        if feature_flags.guest_specified_signal_parameters() {
            event_flag = 2;
            connection_id = 0x2002;
        }

        env.c()
            .handle_synic_message(in_msg(
                protocol::MessageType::OPEN_CHANNEL,
                protocol::OpenChannel2 {
                    open_channel,
                    event_flag: 2,
                    connection_id: 0x2002,
                    flags: (u16::from(
                        protocol::OpenChannelFlags::new().with_redirect_interrupt(true),
                    ) | 0xabc)
                        .into(), // a real flag and some junk
                },
            ))
            .unwrap();
    } else {
        env.c()
            .handle_synic_message(in_msg(protocol::MessageType::OPEN_CHANNEL, open_channel))
            .unwrap();
    }

    let (id, action) = env.recv.recv().unwrap();
    assert_eq!(id, offer_id);
    let Action::Open(op, ..) = action else {
        panic!("unexpected action: {:?}", action);
    };
    assert_eq!(op.open_data.ring_gpadl_id, GpadlId(1));
    assert_eq!(op.open_data.ring_offset, 2);
    assert_eq!(op.open_data.target_vp, 3);
    assert_eq!(op.open_data.event_flag, event_flag);
    assert_eq!(op.open_data.connection_id, connection_id);
    assert_eq!(op.connection_id, connection_id);
    assert_eq!(op.event_flag, event_flag);
    assert_eq!(op.monitor_info, None);
    assert_eq!(op.flags, expected_flags);

    env.c().open_complete(offer_id, 0);

    env.notifier
        .check_message(OutgoingMessage::new(&protocol::OpenResult {
            channel_id,
            open_id: 1,
            status: 0,
        }));

    env.c()
        .handle_synic_message(in_msg(
            protocol::MessageType::MODIFY_CHANNEL,
            protocol::ModifyChannel {
                channel_id,
                target_vp: 4,
            },
        ))
        .unwrap();

    let (id, action) = env.recv.recv().unwrap();
    assert_eq!(id, offer_id);
    assert!(matches!(action, Action::Modify { target_vp: 4 }));

    env.c().modify_channel_complete(id, 0);

    if version >= Version::Iron {
        env.notifier
            .check_message(OutgoingMessage::new(&protocol::ModifyChannelResponse {
                channel_id,
                status: 0,
            }));
    }

    assert!(env.notifier.messages.is_empty());

    env.c().revoke_channel(offer_id);

    env.c()
        .handle_synic_message(in_msg(
            protocol::MessageType::REL_ID_RELEASED,
            protocol::RelIdReleased { channel_id },
        ))
        .unwrap();
}

#[test]
fn test_hvsock() {
    test_hvsock_helper(Version::Win10, false);
}

#[test]
fn test_hvsock_rs3() {
    test_hvsock_helper(Version::Win10Rs3_0, false);
}

#[test]
fn test_hvsock_rs5() {
    test_hvsock_helper(Version::Win10Rs5, false);
    test_hvsock_helper(Version::Win10Rs5, true);
}

fn test_hvsock_helper(version: Version, force_small_message: bool) {
    let mut env = TestEnv::new();

    env.c()
        .handle_synic_message(in_msg(
            protocol::MessageType::INITIATE_CONTACT,
            protocol::InitiateContact {
                version_requested: version as u32,
                target_message_vp: 0,
                interrupt_page_or_target_info: 0,
                parent_to_child_monitor_page_gpa: 0,
                child_to_parent_monitor_page_gpa: 0,
            },
        ))
        .unwrap();

    let request = env.notifier.next_action();
    assert_eq!(
        request,
        ModifyConnectionRequest {
            version: Some(VersionInfo {
                version,
                feature_flags: FeatureFlags::new()
            }),
            monitor_page: Update::Reset,
            interrupt_page: Update::Reset,
            target_message_vp: Some(0),
            ..Default::default()
        }
    );

    env.c()
        .complete_initiate_contact(ModifyConnectionResponse::Supported(
            protocol::ConnectionState::SUCCESSFUL,
            SUPPORTED_FEATURE_FLAGS,
            None,
        ));

    // Discard the version response message.
    env.notifier.messages.pop_front();

    let service_id = Guid::new_random();
    let endpoint_id = Guid::new_random();
    let request_msg = if version >= Version::Win10Rs5 && !force_small_message {
        in_msg(
            protocol::MessageType::TL_CONNECT_REQUEST,
            protocol::TlConnectRequest2 {
                base: protocol::TlConnectRequest {
                    service_id,
                    endpoint_id,
                },
                silo_id: Guid::ZERO,
            },
        )
    } else {
        in_msg(
            protocol::MessageType::TL_CONNECT_REQUEST,
            protocol::TlConnectRequest {
                service_id,
                endpoint_id,
            },
        )
    };

    env.c().handle_synic_message(request_msg).unwrap();

    let request = env.notifier.hvsock_requests.pop().unwrap();
    assert_eq!(request.service_id, service_id);
    assert_eq!(request.endpoint_id, endpoint_id);
    assert!(env.notifier.hvsock_requests.is_empty());

    // Notify the guest of connection failure.
    env.c()
        .send_tl_connect_result(HvsockConnectResult::from_request(&request, false));

    if version >= Version::Win10Rs3_0 {
        env.notifier
            .check_message(OutgoingMessage::new(&protocol::TlConnectResult {
                service_id: request.service_id,
                endpoint_id: request.endpoint_id,
                status: protocol::STATUS_CONNECTION_REFUSED,
            }));
    }

    assert!(env.notifier.messages.is_empty());
}

/// Ensure that channels can be offered at each stage of connection.
#[test]
fn test_hot_add() {
    let mut env = TestEnv::new();
    let offer_id1 = env.offer(1);
    let result = env.c().handle_initiate_contact(
        &protocol::InitiateContact2 {
            initiate_contact: protocol::InitiateContact {
                version_requested: Version::Win10 as u32,
                ..FromZeros::new_zeroed()
            },
            ..FromZeros::new_zeroed()
        },
        &SynicMessage::default(),
        true,
    );
    assert!(result.is_ok());
    let offer_id2 = env.offer(2);
    env.c()
        .complete_initiate_contact(ModifyConnectionResponse::Supported(
            protocol::ConnectionState::SUCCESSFUL,
            SUPPORTED_FEATURE_FLAGS,
            None,
        ));
    let offer_id3 = env.offer(3);
    env.c().handle_request_offers().unwrap();
    let offer_id4 = env.offer(4);
    env.open(1);
    env.open(2);
    env.open(3);
    env.open(4);
    env.c().open_complete(offer_id1, 0);
    env.c().open_complete(offer_id2, 0);
    env.c().open_complete(offer_id3, 0);
    env.c().open_complete(offer_id4, 0);
    env.c().reset();
    env.c().close_complete(offer_id1);
    env.c().close_complete(offer_id2);
    env.c().close_complete(offer_id3);
    env.c().close_complete(offer_id4);
    env.complete_reset();
    assert!(env.notifier.is_reset());
}

#[test]
fn test_save_restore_with_no_connection() {
    let mut env = TestEnv::new();

    let offer_id1 = env.offer(1);
    let _offer_id2 = env.offer(2);

    let state = env.server.save();
    env.c().reset();
    assert!(env.notifier.is_reset());
    env.c().restore(state).unwrap();
    env.c().restore_channel(offer_id1, false).unwrap();
}

#[test]
fn test_save_restore_with_connection() {
    let mut env = TestEnv::new();

    let offer_id1 = env.offer_with_mnf(1);
    let offer_id2 = env.offer(2);
    let offer_id3 = env.offer_with_mnf(3);
    let offer_id4 = env.offer(4);
    let offer_id5 = env.offer_with_mnf(5);
    let offer_id6 = env.offer(6);
    let offer_id7 = env.offer(7);
    let offer_id8 = env.offer(8);
    let offer_id9 = env.offer(9);
    let offer_id10 = env.offer(10);

    let expected_monitor = MonitorPageGpas {
        child_to_parent: 0x123f000,
        parent_to_child: 0x321f000,
    };

    env.connect(Version::Win10, FeatureFlags::new());
    assert_eq!(env.notifier.monitor_page, Some(expected_monitor));

    env.c().handle_request_offers().unwrap();
    assert_eq!(env.server.assigned_monitors.bitmap(), 7);

    env.open(1);
    env.open(2);
    env.open(3);
    env.open(5);

    env.c().open_complete(offer_id1, 0);
    env.c().open_complete(offer_id2, 0);
    env.c().open_complete(offer_id5, 0);

    env.gpadl(1, 10);
    env.c().gpadl_create_complete(offer_id1, GpadlId(10), 0);
    env.gpadl(1, 11);
    env.gpadl(2, 20);
    env.c().gpadl_create_complete(offer_id2, GpadlId(20), 0);
    env.gpadl(2, 21);
    env.gpadl(3, 30);
    env.c().gpadl_create_complete(offer_id3, GpadlId(30), 0);
    env.gpadl(3, 31);

    // Test Opening, Open, and Closing save for reserved channels
    env.open_reserved(7, 1, VMBUS_SINT.into());
    env.open_reserved(8, 2, VMBUS_SINT.into());
    env.open_reserved(9, 3, VMBUS_SINT.into());
    env.c().open_complete(offer_id8, 0);
    env.c().open_complete(offer_id9, 0);
    env.close_reserved(9, 3, VMBUS_SINT.into());

    // Revoke an offer but don't have the "guest" release it, so we can then mark it as
    // reoffered.
    env.c().revoke_channel(offer_id10);
    let offer_id10 = env.offer(10);

    let state = env.server.save();

    env.c().reset();

    env.c().close_complete(offer_id1);
    env.c().close_complete(offer_id2);
    env.c().open_complete(offer_id3, -1);
    env.c().close_complete(offer_id5);
    env.c().open_complete(offer_id7, -1);
    env.c().close_complete(offer_id8);
    env.c().close_complete(offer_id9);

    env.c().gpadl_teardown_complete(offer_id1, GpadlId(10));
    env.c().gpadl_create_complete(offer_id1, GpadlId(11), -1);
    env.c().gpadl_teardown_complete(offer_id2, GpadlId(20));
    env.c().gpadl_create_complete(offer_id2, GpadlId(21), -1);
    env.c().gpadl_teardown_complete(offer_id3, GpadlId(30));
    env.c().gpadl_create_complete(offer_id3, GpadlId(31), -1);

    env.complete_reset();
    env.notifier.check_reset();

    env.c().revoke_channel(offer_id5);
    env.c().revoke_channel(offer_id6);

    env.c().restore(state.clone()).unwrap();

    env.c().revoke_channel(offer_id1);
    env.c().revoke_channel(offer_id4);
    env.c().restore_channel(offer_id3, false).unwrap();
    let offer_id5 = env.offer_with_mnf(5);
    env.c().restore_channel(offer_id5, true).unwrap();
    env.c().restore_channel(offer_id7, false).unwrap();
    env.c().restore_channel(offer_id8, true).unwrap();
    env.c().restore_channel(offer_id9, true).unwrap();
    env.c().restore_channel(offer_id10, false).unwrap();
    assert!(matches!(
        env.server.channels[offer_id10].state,
        ChannelState::Reoffered
    ));

    env.c().revoke_unclaimed_channels();

    assert_eq!(env.notifier.monitor_page, Some(expected_monitor));
    assert_eq!(env.notifier.target_message_vp, Some(0));

    assert_eq!(env.server.assigned_monitors.bitmap(), 6);
    env.release(1);
    env.release(2);
    env.release(4);

    // Check reserved channels have been restored to the same state
    env.c().open_complete(offer_id7, 0);
    env.close_reserved(8, 2, VMBUS_SINT.into());
    env.c().close_complete(offer_id8);
    env.c().close_complete(offer_id9);

    env.c().reset();

    env.c().open_complete(offer_id3, -1);
    env.c().gpadl_teardown_complete(offer_id3, GpadlId(30));
    env.c().gpadl_create_complete(offer_id3, GpadlId(31), -1);
    env.c().close_complete(offer_id5);
    env.c().close_complete(offer_id7);

    env.complete_reset();
    env.notifier.check_reset();

    env.c().restore(state).unwrap();
    env.c().restore_channel(offer_id3, false).unwrap();
    assert_eq!(env.notifier.monitor_page, Some(expected_monitor));
    assert_eq!(env.notifier.target_message_vp, Some(0));
}

#[test]
fn test_save_restore_connecting() {
    let mut env = TestEnv::new();

    let offer_id1 = env.offer_with_mnf(1);
    let _offer_id2 = env.offer(2);

    env.start_connect(Version::Win10, FeatureFlags::new(), false, true);
    assert_eq!(
        env.notifier.monitor_page,
        Some(MonitorPageGpas {
            child_to_parent: 0x123f000,
            parent_to_child: 0x321f000
        })
    );

    let state = env.server.save();

    env.c().reset();
    // We have to "complete" the connection to let the reset go through.
    env.complete_connect();
    env.complete_reset();
    env.notifier.check_reset();

    env.c().restore(state).unwrap();
    env.c().restore_channel(offer_id1, false).unwrap();
    assert_eq!(
        env.notifier.monitor_page,
        Some(MonitorPageGpas {
            child_to_parent: 0x123f000,
            parent_to_child: 0x321f000
        })
    );

    // Restore should resend the modify connection request.
    let request = env.next_action();
    assert_eq!(
        request,
        ModifyConnectionRequest {
            version: Some(VersionInfo {
                version: Version::Win10,
                feature_flags: FeatureFlags::new()
            }),
            monitor_page: Update::Set(MonitorPageGpas {
                child_to_parent: 0x123f000,
                parent_to_child: 0x321f000,
            }),
            interrupt_page: Update::Reset,
            target_message_vp: Some(0),
            ..Default::default()
        }
    );

    assert_eq!(Some(0), env.notifier.target_message_vp);

    // We can successfully complete connecting after restore.
    env.complete_connect();
}

#[test]
fn test_save_restore_modifying() {
    let mut env = TestEnv::new();
    env.connect(
        Version::Copper,
        FeatureFlags::new().with_modify_connection(true),
    );

    let expected = MonitorPageGpas {
        parent_to_child: 0x123f000,
        child_to_parent: 0x321f000,
    };

    env.send_message(in_msg(
        protocol::MessageType::MODIFY_CONNECTION,
        protocol::ModifyConnection {
            parent_to_child_monitor_page_gpa: expected.parent_to_child,
            child_to_parent_monitor_page_gpa: expected.child_to_parent,
        },
    ));

    // Discard ModifyConnectionRequest
    env.next_action();

    assert_eq!(env.notifier.monitor_page, Some(expected));

    let state = env.server.save();
    env.c().reset();
    env.complete_reset();
    env.notifier.check_reset();

    env.c().restore(state).unwrap();

    // Restore should have resent the request.
    let request = env.next_action();
    assert_eq!(
        request,
        ModifyConnectionRequest {
            monitor_page: Update::Set(MonitorPageGpas {
                parent_to_child: 0x123f000,
                child_to_parent: 0x321f000,
            }),
            interrupt_page: Update::Reset,
            target_message_vp: Some(0),
            ..Default::default()
        }
    );

    assert_eq!(env.notifier.monitor_page, Some(expected));

    // We can complete the modify request after restore.
    env.c()
        .complete_modify_connection(ModifyConnectionResponse::Modified(
            protocol::ConnectionState::SUCCESSFUL,
        ));

    env.notifier
        .check_message(OutgoingMessage::new(&protocol::ModifyConnectionResponse {
            connection_state: protocol::ConnectionState::SUCCESSFUL,
        }));
}

#[test]
fn test_save_restore_disconnected_reserved() {
    let mut env = TestEnv::new();

    let offer_id1 = env.offer(1);
    let _offer_id2 = env.offer(2);
    let _offer_id3 = env.offer(3);

    env.connect(Version::Copper, FeatureFlags::new());
    env.c().handle_request_offers().unwrap();

    env.gpadl(1, 1);
    env.c().gpadl_create_complete(offer_id1, GpadlId(1), 0);
    env.open_reserved(1, 0, 3);
    env.c().open_complete(offer_id1, protocol::STATUS_SUCCESS);
    env.c().handle_unload();

    let state = env.server.save();
    let mut env = TestEnv::new();
    let offer_id1 = env.offer(1);
    let offer_id2 = env.offer(2);
    let offer_id3 = env.offer(3);

    env.c().restore(state).unwrap();

    // This will panic if the reserved channel was not restored.
    env.c().restore_channel(offer_id1, true).unwrap();
    env.c().restore_channel(offer_id2, false).unwrap();
    env.c().restore_channel(offer_id3, false).unwrap();

    // Make sure the gpadl was restored as well.
    assert!(env.server.gpadls.contains_key(&(GpadlId(1), offer_id1)));
}

#[test]
fn test_save_restore_offers_not_sent() {
    let mut env = TestEnv::new();

    let _offer_id1 = env.offer(1);
    let _offer_id2 = env.offer(2);
    let _offer_id3 = env.offer(3);

    env.connect(Version::Copper, FeatureFlags::new());

    // All offers are in the ClientReleased state, so they are not saved.
    let state = env.server.save();
    let mut env = TestEnv::new();

    let offer_id1 = env.offer(1);
    let offer_id2 = env.offer(2);
    let offer_id3 = env.offer(3);

    // Because the offers were not saved, they are treated as new during the restore.
    env.c().restore(state).unwrap();
    env.c().restore_channel(offer_id1, false).unwrap();
    env.c().restore_channel(offer_id2, false).unwrap();
    env.c().restore_channel(offer_id3, false).unwrap();

    // Because the guest has not yet requested offers, no messages should be sent at this point.
    env.c().revoke_unclaimed_channels();
    assert!(env.notifier.messages.is_empty());

    // When the guest requests offers, they should be all be sent.
    env.c().handle_request_offers().unwrap();
    env.notifier.check_messages([
        OutgoingMessage::new(&protocol::OfferChannel {
            interface_id: guid_from_id(1),
            instance_id: guid_from_id(1),
            channel_id: ChannelId(1),
            connection_id: 0x2001,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        }),
        OutgoingMessage::new(&protocol::OfferChannel {
            interface_id: guid_from_id(2),
            instance_id: guid_from_id(2),
            channel_id: ChannelId(2),
            connection_id: 0x2002,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        }),
        OutgoingMessage::new(&protocol::OfferChannel {
            interface_id: guid_from_id(3),
            instance_id: guid_from_id(3),
            channel_id: ChannelId(3),
            connection_id: 0x2003,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        }),
        OutgoingMessage::new(&protocol::AllOffersDelivered {}),
    ]);
}

#[test]
fn test_save_restore_hot_add_during_restore() {
    let mut env = TestEnv::new();

    let _offer_id1 = env.offer(1);
    let _offer_id2 = env.offer(2);
    let _offer_id3 = env.offer(3);

    env.connect(Version::Copper, FeatureFlags::new());
    env.c().handle_request_offers().unwrap();
    let state = env.server.save();
    let mut env = TestEnv::new();

    let offer_id1 = env.offer(1);
    let offer_id2 = env.offer(2);
    let offer_id3 = env.offer(3);
    // A new offer is created that is not present in the saved state.
    let _offer_id4 = env.offer(4);

    // Because the offers were not saved, they are treated as new during the restore.
    env.c().restore(state).unwrap();
    env.c().restore_channel(offer_id1, false).unwrap();
    env.c().restore_channel(offer_id2, false).unwrap();
    env.c().restore_channel(offer_id3, false).unwrap();
    assert!(env.notifier.messages.is_empty());

    // A message should be sent for the new offer only.
    env.c().revoke_unclaimed_channels();
    env.notifier
        .check_message(OutgoingMessage::new(&protocol::OfferChannel {
            interface_id: guid_from_id(4),
            instance_id: guid_from_id(4),
            channel_id: ChannelId(4),
            connection_id: 0x2004,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        }));

    assert!(env.notifier.messages.is_empty());
}

#[test]
fn test_pending_messages() {
    let mut env = TestEnv::new();

    let offer_id1 = env.offer(1);
    let offer_id2 = env.offer(2);
    let offer_id3 = env.offer(3);

    env.connect(Version::Copper, FeatureFlags::new());
    env.c().handle_request_offers().unwrap();

    env.notifier.messages.clear();
    env.notifier.pend_messages = true;
    env.open_reserved(2, 4, VMBUS_SINT.into());
    env.c().open_complete(offer_id2, protocol::STATUS_SUCCESS);

    // Reserved channel message should not be queued, but just discarded if it cannot be sent.
    assert!(env.notifier.messages.is_empty());
    assert!(!env.server.has_pending_messages());

    env.gpadl(1, 10);
    env.c()
        .gpadl_create_complete(offer_id1, GpadlId(10), protocol::STATUS_SUCCESS);

    // The next message should still be queued because there is already a queued message.
    env.notifier.pend_messages = true;
    env.open(3);
    env.c().open_complete(offer_id3, protocol::STATUS_SUCCESS);

    // No messages were received.
    assert!(env.notifier.messages.is_empty());
    assert!(env.server.has_pending_messages());
    env.notifier.pend_messages = false;

    let state = env.server.save();

    // Create a new env instead of resetting because the gpadl blocks the reset until released.
    let mut env = TestEnv::new();

    let offer_id1 = env.offer(1);
    let offer_id2 = env.offer(2);
    let offer_id3 = env.offer(3);

    env.c().restore(state).unwrap();
    env.c().restore_channel(offer_id1, false).unwrap();
    env.c().restore_channel(offer_id2, true).unwrap();
    env.c().restore_channel(offer_id3, true).unwrap();

    // The messages should be pending again.
    assert!(env.server.has_pending_messages());
    let mut pending_messages = Vec::new();
    let r = env.server.poll_flush_pending_messages(|msg| {
        pending_messages.push(msg.clone());
        Poll::Ready(())
    });
    assert!(r.is_ready());
    assert_eq!(pending_messages.len(), 2);
    assert_eq!(
        protocol::MessageHeader::read_from_prefix(pending_messages[0].data())
            .unwrap()
            .0
            .message_type(),
        protocol::MessageType::GPADL_CREATED
    );

    assert_eq!(
        protocol::MessageHeader::read_from_prefix(pending_messages[1].data())
            .unwrap()
            .0
            .message_type(),
        protocol::MessageType::OPEN_CHANNEL_RESULT
    );

    assert!(!env.server.has_pending_messages());
}

#[test]
fn test_modify_connection() {
    let mut env = TestEnv::new();
    env.connect(
        Version::Copper,
        FeatureFlags::new().with_modify_connection(true),
    );

    env.send_message(in_msg(
        protocol::MessageType::MODIFY_CONNECTION,
        protocol::ModifyConnection {
            parent_to_child_monitor_page_gpa: 5,
            child_to_parent_monitor_page_gpa: 6,
        },
    ));

    assert_eq!(
        env.notifier.monitor_page,
        Some(MonitorPageGpas {
            parent_to_child: 5,
            child_to_parent: 6
        })
    );

    let request = env.next_action();
    assert_eq!(
        request,
        ModifyConnectionRequest {
            monitor_page: Update::Set(MonitorPageGpas {
                child_to_parent: 6,
                parent_to_child: 5,
            }),
            ..Default::default()
        }
    );

    env.c()
        .complete_modify_connection(ModifyConnectionResponse::Modified(
            protocol::ConnectionState::FAILED_UNKNOWN_FAILURE,
        ));

    env.notifier
        .check_message(OutgoingMessage::new(&protocol::ModifyConnectionResponse {
            connection_state: protocol::ConnectionState::FAILED_UNKNOWN_FAILURE,
        }));
}

#[test]
fn test_modify_connection_unsupported() {
    let mut env = TestEnv::new();
    env.connect(Version::Copper, FeatureFlags::new());

    let err = env
        .try_send_message(in_msg(
            protocol::MessageType::MODIFY_CONNECTION,
            protocol::ModifyConnection {
                parent_to_child_monitor_page_gpa: 5,
                child_to_parent_monitor_page_gpa: 6,
            },
        ))
        .unwrap_err();

    assert!(matches!(
        err,
        ChannelError::ParseError(protocol::ParseError::InvalidMessageType(
            protocol::MessageType::MODIFY_CONNECTION
        ))
    ));
}

#[test]
fn test_reserved_channels() {
    let mut env = TestEnv::new();

    let offer_id1 = env.offer(1);
    let offer_id2 = env.offer(2);
    let offer_id3 = env.offer(3);

    env.connect(Version::Win10, FeatureFlags::new());
    env.c().handle_request_offers().unwrap();

    // Check gpadl doesn't prevent unload or get torndown on disconnect
    env.gpadl(1, 10);
    env.c().gpadl_create_complete(offer_id1, GpadlId(10), 0);

    env.notifier.messages.clear();

    // Open responses should be sent to the provided target
    env.open_reserved(1, 1, VMBUS_SINT.into());
    env.c().open_complete(offer_id1, 0);
    env.notifier.check_message_with_target(
        OutgoingMessage::new(&protocol::OpenResult {
            channel_id: ChannelId(1),
            ..FromZeros::new_zeroed()
        }),
        MessageTarget::ReservedChannel(
            offer_id1,
            ConnectionTarget {
                vp: 1,
                sint: VMBUS_SINT,
            },
        ),
    );
    env.open_reserved(2, 2, VMBUS_SINT.into());
    env.c().open_complete(offer_id2, 0);
    env.open_reserved(3, 3, VMBUS_SINT.into());
    env.c().open_complete(offer_id3, 0);

    // This should fail
    assert!(matches!(env.close(2), Err(ChannelError::ChannelReserved)));

    // Reserved channels and gpadls should stay open across unloads
    env.c().handle_unload();
    env.complete_reset();

    // Closing while disconnected should work
    env.close_reserved(2, 2, VMBUS_SINT.into());
    env.c().close_complete(offer_id2);

    env.notifier.messages.clear();
    env.connect(Version::Copper, FeatureFlags::new());
    env.c().handle_request_offers().unwrap();

    // Check reserved gpadl gets torndown on reset
    // Duplicate GPADL IDs across different channels should also work
    env.gpadl(2, 10);
    env.c().gpadl_create_complete(offer_id2, GpadlId(10), 0);

    // Reopening the same offer should work
    env.open_reserved(2, 3, VMBUS_SINT.into());
    env.c().open_complete(offer_id2, 0);

    env.notifier.messages.clear();

    // The channel should still be open after disconnect/reconnect
    // and close responses should be sent to the provided target
    env.close_reserved(1, 4, VMBUS_SINT.into());
    env.c().close_complete(offer_id1);
    env.notifier.check_message_with_target(
        OutgoingMessage::new(&protocol::CloseReservedChannelResponse {
            channel_id: ChannelId(1),
        }),
        MessageTarget::ReservedChannel(
            offer_id1,
            ConnectionTarget {
                vp: 4,
                sint: VMBUS_SINT,
            },
        ),
    );
    env.teardown_gpadl(1, 10);
    env.c().gpadl_teardown_complete(offer_id1, GpadlId(10));

    // Reset should force reserved channels closed
    env.c().reset();
    env.c().close_complete(offer_id2);
    env.c().gpadl_teardown_complete(offer_id2, GpadlId(10));
    env.c().close_complete(offer_id3);

    env.complete_reset();
    assert!(env.notifier.is_reset());
}

#[test]
fn test_disconnected_reset() {
    let mut env = TestEnv::new();

    let offer_id1 = env.offer(1);

    env.connect(Version::Win10, FeatureFlags::new());
    env.c().handle_request_offers().unwrap();

    env.gpadl(1, 10);
    env.c().gpadl_create_complete(offer_id1, GpadlId(10), 0);
    env.open_reserved(1, 1, VMBUS_SINT.into());
    env.c().open_complete(offer_id1, 0);

    env.c().handle_unload();
    env.complete_reset();

    // Reset while disconnected should cleanup reserved channels
    // and complete disconnect automatically
    env.c().reset();
    env.c().close_complete(offer_id1);
    env.c().gpadl_teardown_complete(offer_id1, GpadlId(10));

    env.complete_reset();
    assert!(env.notifier.is_reset());

    let offer_id2 = env.offer(2);

    env.notifier.messages.clear();
    env.connect(Version::Win10, FeatureFlags::new());
    env.c().handle_request_offers().unwrap();

    env.gpadl(2, 20);
    env.c().gpadl_create_complete(offer_id2, GpadlId(20), 0);
    env.open_reserved(2, 2, VMBUS_SINT.into());
    env.c().open_complete(offer_id2, 0);

    env.c().handle_unload();
    env.complete_reset();

    env.close_reserved(2, 2, VMBUS_SINT.into());
    env.c().close_complete(offer_id2);
    env.c().gpadl_teardown_complete(offer_id2, GpadlId(20));

    env.c().reset();
    assert!(env.notifier.is_reset());
}

#[test]
fn test_mnf_channel() {
    let mut env = TestEnv::new();

    // This test combines server-handled and preset MNF IDs, which can't happen normally, but
    // it simplifies the test.
    let _offer_id1 = env.offer(1);
    let _offer_id2 = env.offer_with_mnf(2);
    let _offer_id3 = env.offer_with_preset_mnf(3, 5);

    let mut expected_channels = [
        protocol::OfferChannel {
            interface_id: guid_from_id(1),
            instance_id: guid_from_id(1),
            channel_id: ChannelId(1),
            connection_id: 0x2001,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        },
        protocol::OfferChannel {
            interface_id: guid_from_id(2),
            instance_id: guid_from_id(2),
            channel_id: ChannelId(2),
            connection_id: 0x2002,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        },
        protocol::OfferChannel {
            interface_id: guid_from_id(3),
            instance_id: guid_from_id(3),
            channel_id: ChannelId(3),
            connection_id: 0x2003,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        },
    ];

    fn make_messages(
        channels: &[protocol::OfferChannel],
    ) -> impl Iterator<Item = OutgoingMessage> + '_ {
        channels
            .iter()
            .map(OutgoingMessage::new)
            .chain([OutgoingMessage::new(&protocol::AllOffersDelivered {})])
    }

    // If the guest does not send monitor pages, the monitor ID is not sent with the offers.
    env.connect_without_mnf(Version::Copper, FeatureFlags::new());
    env.c().handle_request_offers().unwrap();
    env.notifier
        .check_messages(make_messages(&expected_channels));
    env.reset();

    // If the guest sends monitor pages, but the server does not allow it, the monitor ID is
    // still not sent.
    env.server.set_require_server_allocated_mnf(true);
    env.connect(Version::Copper, FeatureFlags::new());
    env.c().handle_request_offers().unwrap();
    env.notifier
        .check_messages(make_messages(&expected_channels));
    env.reset();

    // Now we connect with regular MNF support, and the monitor IDs are sent.
    env.server.set_require_server_allocated_mnf(false);
    expected_channels[1].monitor_id = 0;
    expected_channels[1].monitor_allocated = 1;
    expected_channels[2].monitor_id = 5;
    expected_channels[2].monitor_allocated = 1;

    env.connect(Version::Copper, FeatureFlags::new());
    env.c().handle_request_offers().unwrap();

    // Preset monitor ID should not be in the bitmap.
    assert_eq!(env.server.assigned_monitors.bitmap(), 1);

    env.notifier
        .check_messages(make_messages(&expected_channels));
}

#[test]
fn test_server_monitor_page() {
    // Guest pages provided, but overridden by server-allocated pages.
    test_server_monitor_page_helper(true);

    // No guest pages supplied, server will allocate them.
    test_server_monitor_page_helper(false);
}

fn test_server_monitor_page_helper(provide_guest_pages: bool) {
    let mut env = TestEnv::new();
    let _offer_id1 = env.offer_with_mnf(1);

    let version = Version::Copper;
    let feature_flags = FeatureFlags::new().with_server_specified_monitor_pages(true);
    env.c()
        .handle_synic_message(in_msg(
            protocol::MessageType::INITIATE_CONTACT,
            protocol::InitiateContact {
                version_requested: version as u32,
                interrupt_page_or_target_info: TargetInfo::new()
                    .with_sint(VMBUS_SINT)
                    .with_vtl(0)
                    .with_feature_flags(feature_flags.into())
                    .into(),
                child_to_parent_monitor_page_gpa: if provide_guest_pages { 0x123f000 } else { 0 },
                parent_to_child_monitor_page_gpa: if provide_guest_pages { 0x321f000 } else { 0 },
                ..FromZeros::new_zeroed()
            },
        ))
        .unwrap();

    // The server will notify with the guest-provided pages (if present).
    let request = env.notifier.next_action();
    assert_eq!(
        request,
        ModifyConnectionRequest {
            version: Some(VersionInfo {
                version,
                feature_flags
            }),
            monitor_page: if provide_guest_pages {
                Update::Set(MonitorPageGpas {
                    child_to_parent: 0x123f000,
                    parent_to_child: 0x321f000,
                })
            } else {
                Update::Reset
            },
            interrupt_page: Update::Reset,
            target_message_vp: Some(0),
            ..Default::default()
        }
    );

    // Specify server-allocated pages when completing the request.
    env.c()
        .complete_initiate_contact(ModifyConnectionResponse::Supported(
            protocol::ConnectionState::SUCCESSFUL,
            feature_flags,
            Some(MonitorPageGpas {
                parent_to_child: 0x456f000,
                child_to_parent: 0x654f000,
            }),
        ));

    // This is reflected in the server state.
    assert!(matches!(
        env.server.state,
        ConnectionState::Connected(ConnectionInfo {
            monitor_page: Some(MonitorPageGpaInfo {
                gpas: MonitorPageGpas {
                    parent_to_child: 0x456f000,
                    child_to_parent: 0x654f000
                },
                server_allocated: true,
            }),
            ..
        })
    ));

    // The version response should include the server-allocated pages.
    env.notifier
        .check_message(OutgoingMessage::new(&protocol::VersionResponse3 {
            version_response2: protocol::VersionResponse2 {
                version_response: protocol::VersionResponse {
                    version_supported: 1,
                    connection_state: protocol::ConnectionState::SUCCESSFUL,
                    padding: 0,
                    selected_version_or_connection_id: 1,
                },
                supported_features: feature_flags.into(),
            },
            _padding: 0,
            parent_to_child_monitor_page_gpa: 0x456f000,
            child_to_parent_monitor_page_gpa: 0x654f000,
        }));

    // Make sure the monitor ID is sent for the channel, even if the guest didn't try to supply its
    // own monitor pages.
    env.c().handle_request_offers().unwrap();
    env.notifier.check_messages([
        OutgoingMessage::new(&protocol::OfferChannel {
            interface_id: guid_from_id(1),
            instance_id: guid_from_id(1),
            channel_id: ChannelId(1),
            connection_id: 0x2001,
            is_dedicated: 1,
            monitor_id: 0,
            monitor_allocated: 1,
            ..protocol::OfferChannel::new_zeroed()
        }),
        OutgoingMessage::new(&protocol::AllOffersDelivered {}),
    ]);
}

#[test]
fn test_channel_id_order() {
    let mut env = TestEnv::new();

    let _offer_id1 = env.offer(3);
    let _offer_id2 = env.offer(10);
    let _offer_id3 = env.offer(5);
    let _offer_id4 = env.offer(17);
    let _offer_id5 = env.offer_with_order(5, 6, Some(2));
    let _offer_id6 = env.offer_with_order(5, 8, Some(1));
    let _offer_id7 = env.offer_with_order(5, 1, None);

    env.connect(Version::Win10, FeatureFlags::new());
    env.c().handle_request_offers().unwrap();

    env.notifier.check_messages([
        OutgoingMessage::new(&protocol::OfferChannel {
            interface_id: guid_from_id(3),
            instance_id: guid_from_id(3),
            channel_id: ChannelId(1),
            connection_id: 0x2001,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        }),
        OutgoingMessage::new(&protocol::OfferChannel {
            interface_id: guid_from_id(5),
            instance_id: guid_from_id(8),
            channel_id: ChannelId(2),
            connection_id: 0x2002,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        }),
        OutgoingMessage::new(&protocol::OfferChannel {
            interface_id: guid_from_id(5),
            instance_id: guid_from_id(6),
            channel_id: ChannelId(3),
            connection_id: 0x2003,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        }),
        OutgoingMessage::new(&protocol::OfferChannel {
            interface_id: guid_from_id(5),
            instance_id: guid_from_id(1),
            channel_id: ChannelId(4),
            connection_id: 0x2004,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        }),
        OutgoingMessage::new(&protocol::OfferChannel {
            interface_id: guid_from_id(5),
            instance_id: guid_from_id(5),
            channel_id: ChannelId(5),
            connection_id: 0x2005,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        }),
        OutgoingMessage::new(&protocol::OfferChannel {
            interface_id: guid_from_id(10),
            instance_id: guid_from_id(10),
            channel_id: ChannelId(6),
            connection_id: 0x2006,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        }),
        OutgoingMessage::new(&protocol::OfferChannel {
            interface_id: guid_from_id(17),
            instance_id: guid_from_id(17),
            channel_id: ChannelId(7),
            connection_id: 0x2007,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        }),
        OutgoingMessage::new(&protocol::AllOffersDelivered {}),
    ])
}

#[test]
fn test_channel_id_order_absolute() {
    let mut env = TestEnv::with_params(true);

    let _offer_id1 = env.offer_with_order(3, 3, Some(1));
    let _offer_id3 = env.offer_with_order(5, 5, Some(3));
    let _offer_id4 = env.offer_with_order(17, 17, Some(4));
    let _offer_id7 = env.offer_with_order(5, 1, None);
    let _offer_id5 = env.offer_with_order(5, 6, Some(5));
    let _offer_id2 = env.offer_with_order(10, 10, Some(2));
    let _offer_id6 = env.offer_with_order(5, 8, Some(6));

    env.connect(Version::Win10, FeatureFlags::new());
    env.c().handle_request_offers().unwrap();

    // Channel IDs are assigned by the order values first.
    env.notifier.check_messages([
        OutgoingMessage::new(&protocol::OfferChannel {
            interface_id: guid_from_id(3),
            instance_id: guid_from_id(3),
            channel_id: ChannelId(1),
            connection_id: 0x2001,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        }),
        OutgoingMessage::new(&protocol::OfferChannel {
            interface_id: guid_from_id(10),
            instance_id: guid_from_id(10),
            channel_id: ChannelId(2),
            connection_id: 0x2002,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        }),
        OutgoingMessage::new(&protocol::OfferChannel {
            interface_id: guid_from_id(5),
            instance_id: guid_from_id(5),
            channel_id: ChannelId(3),
            connection_id: 0x2003,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        }),
        OutgoingMessage::new(&protocol::OfferChannel {
            interface_id: guid_from_id(17),
            instance_id: guid_from_id(17),
            channel_id: ChannelId(4),
            connection_id: 0x2004,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        }),
        OutgoingMessage::new(&protocol::OfferChannel {
            interface_id: guid_from_id(5),
            instance_id: guid_from_id(6),
            channel_id: ChannelId(5),
            connection_id: 0x2005,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        }),
        OutgoingMessage::new(&protocol::OfferChannel {
            interface_id: guid_from_id(5),
            instance_id: guid_from_id(8),
            channel_id: ChannelId(6),
            connection_id: 0x2006,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        }),
        OutgoingMessage::new(&protocol::OfferChannel {
            interface_id: guid_from_id(5),
            instance_id: guid_from_id(1),
            channel_id: ChannelId(7),
            connection_id: 0x2007,
            is_dedicated: 1,
            monitor_id: 0xff,
            ..protocol::OfferChannel::new_zeroed()
        }),
        OutgoingMessage::new(&protocol::AllOffersDelivered {}),
    ])
}

#[test]
fn test_confidential_connection() {
    let mut env = TestEnv::new();
    env.connect_trusted(
        Version::Copper,
        FeatureFlags::new().with_confidential_channels(true),
    );

    assert_eq!(
        env.version.unwrap(),
        VersionInfo {
            version: Version::Copper,
            feature_flags: FeatureFlags::new().with_confidential_channels(true)
        }
    );

    env.offer(1); // non-confidential
    env.offer_with_flags(2, OfferFlags::new().with_confidential_ring_buffer(true));
    env.offer_with_flags(
        3,
        OfferFlags::new()
            .with_confidential_ring_buffer(true)
            .with_confidential_external_memory(true),
    );

    // Untrusted messages are rejected when the connection is trusted.
    let error = env
        .try_send_message(in_msg(
            protocol::MessageType::REQUEST_OFFERS,
            protocol::RequestOffers {},
        ))
        .unwrap_err();

    assert!(matches!(error, ChannelError::UntrustedMessage));
    assert!(env.notifier.messages.is_empty());

    // Trusted messages are accepted.
    env.send_message(in_msg_ex(
        protocol::MessageType::REQUEST_OFFERS,
        protocol::RequestOffers {},
        false,
        true,
    ));

    let offer = env.notifier.get_message::<protocol::OfferChannel>();
    assert_eq!(offer.channel_id, ChannelId(1));
    assert_eq!(offer.flags, OfferFlags::new());

    let offer = env.notifier.get_message::<protocol::OfferChannel>();
    assert_eq!(offer.channel_id, ChannelId(2));
    assert_eq!(
        offer.flags,
        OfferFlags::new().with_confidential_ring_buffer(true)
    );

    let offer = env.notifier.get_message::<protocol::OfferChannel>();
    assert_eq!(offer.channel_id, ChannelId(3));
    assert_eq!(
        offer.flags,
        OfferFlags::new()
            .with_confidential_ring_buffer(true)
            .with_confidential_external_memory(true)
    );

    env.notifier
        .check_message(OutgoingMessage::new(&protocol::AllOffersDelivered {}));
}

#[test]
fn test_confidential_channels_unsupported() {
    let mut env = TestEnv::new();

    // A trusted connection without confidential channels is weird, but it makes sure the server
    // looks at the flag, not the trusted state.
    env.connect_trusted(Version::Copper, FeatureFlags::new());

    assert_eq!(
        env.version.unwrap(),
        VersionInfo {
            version: Version::Copper,
            feature_flags: FeatureFlags::new()
        }
    );

    env.offer_with_flags(1, OfferFlags::new().with_enumerate_device_interface(true)); // non-confidential
    env.offer_with_flags(
        2,
        OfferFlags::new()
            .with_named_pipe_mode(true)
            .with_confidential_ring_buffer(true)
            .with_confidential_external_memory(true),
    );

    env.send_message(in_msg_ex(
        protocol::MessageType::REQUEST_OFFERS,
        protocol::RequestOffers {},
        false,
        true,
    ));

    let offer = env.notifier.get_message::<protocol::OfferChannel>();
    assert_eq!(offer.channel_id, ChannelId(1));
    assert_eq!(
        offer.flags,
        OfferFlags::new().with_enumerate_device_interface(true)
    );

    // The confidential channel flags are not sent without the feature flag.
    let offer = env.notifier.get_message::<protocol::OfferChannel>();
    assert_eq!(offer.channel_id, ChannelId(2));
    assert_eq!(offer.flags, OfferFlags::new().with_named_pipe_mode(true));

    env.notifier
        .check_message(OutgoingMessage::new(&protocol::AllOffersDelivered {}));
}

#[test]
fn test_confidential_channels_untrusted() {
    let mut env = TestEnv::new();

    env.connect(
        Version::Copper,
        FeatureFlags::new().with_confidential_channels(true),
    );

    // The server should not offer confidential channel support to untrusted clients, even if
    // requested.
    assert_eq!(
        env.version.unwrap(),
        VersionInfo {
            version: Version::Copper,
            feature_flags: FeatureFlags::new()
        }
    );
}

#[test]
fn test_disconnect() {
    let mut env = TestEnv::new();
    let _offer_id1 = env.offer(1);
    let _offer_id2 = env.offer(2);
    let _offer_id3 = env.offer(3);

    env.connect(Version::Win10, FeatureFlags::new());
    env.c().handle_request_offers().unwrap();

    // Send unload message with all channels already closed.
    env.c().handle_unload();

    // Check that modify_connection was invoked on the notifier.
    let req = env.notifier.next_action();
    assert_eq!(
        req,
        ModifyConnectionRequest {
            monitor_page: Update::Reset,
            interrupt_page: Update::Reset,
            ..Default::default()
        }
    );

    env.notifier.messages.clear();
    env.c().complete_disconnect();
    env.notifier
        .check_message(OutgoingMessage::new(&protocol::UnloadComplete {}));
}

#[test]
fn test_disconnect_open_channels() {
    let mut env = TestEnv::new();
    let offer_id1 = env.offer(1);
    let offer_id2 = env.offer(2);
    let _offer_id3 = env.offer(3);

    env.connect(Version::Win10, FeatureFlags::new());
    env.c().handle_request_offers().unwrap();

    // Open two channels.
    env.open(1);
    env.open(2);

    env.c().open_complete(offer_id1, 0);
    env.c().open_complete(offer_id2, 0);

    // Send unload message with channels still open.
    env.c().handle_unload();

    assert!(env.notifier.modify_requests.is_empty());

    // Unload will close the channels, so complete that operation.
    env.c().close_complete(offer_id1);
    env.c().close_complete(offer_id2);

    // Modify connection will be invoked once all channels are closed.
    let req = env.notifier.next_action();
    assert_eq!(
        req,
        ModifyConnectionRequest {
            monitor_page: Update::Reset,
            interrupt_page: Update::Reset,
            ..Default::default()
        }
    );

    env.notifier.messages.clear();
    env.c().complete_disconnect();
    env.notifier
        .check_message(OutgoingMessage::new(&protocol::UnloadComplete {}));
}

#[test]
fn test_reinitiate_contact() {
    let mut env = TestEnv::new();
    let _offer_id1 = env.offer(1);
    let _offer_id2 = env.offer(2);
    let _offer_id3 = env.offer(3);

    env.connect(Version::Win10, FeatureFlags::new());
    env.c().handle_request_offers().unwrap();
    env.notifier.messages.clear();

    // Send a new InitiateContact message to force a disconnect without using reload.
    let result = env.c().handle_synic_message(in_msg_ex(
        protocol::MessageType::INITIATE_CONTACT,
        protocol::InitiateContact {
            version_requested: Version::Win10 as u32,
            interrupt_page_or_target_info: TargetInfo::new()
                .with_sint(VMBUS_SINT)
                .with_vtl(0)
                .into(),
            child_to_parent_monitor_page_gpa: 0x123f000,
            parent_to_child_monitor_page_gpa: 0x321f000,
            ..FromZeros::new_zeroed()
        },
        false,
        false,
    ));
    assert!(result.is_ok());

    // We will first receive a request indicating the forced disconnect.
    let req = env.notifier.next_action();
    assert_eq!(
        req,
        ModifyConnectionRequest {
            monitor_page: Update::Reset,
            interrupt_page: Update::Reset,
            ..Default::default()
        }
    );

    env.c().complete_disconnect();

    // No UnloadComplete is sent in this case since Unload was not sent.
    assert!(env.notifier.messages.is_empty());

    // Now we receive the request for the new connection.
    let req = env.notifier.next_action();
    assert_eq!(
        req,
        ModifyConnectionRequest {
            version: Some(VersionInfo {
                version: Version::Win10,
                feature_flags: FeatureFlags::new()
            }),
            monitor_page: Update::Set(MonitorPageGpas {
                child_to_parent: 0x123f000,
                parent_to_child: 0x321f000,
            }),
            interrupt_page: Update::Reset,
            target_message_vp: Some(0),
            ..Default::default()
        }
    );

    env.complete_connect();
}

#[test]
fn test_gpadl_create_failure() {
    let mut env = TestEnv::new();

    let offer_id1 = env.offer(1);

    env.connect(Version::Copper, FeatureFlags::new());
    env.c().handle_request_offers().unwrap();
    env.notifier.messages.clear();

    let good_range = [1u64, 0u64];
    // Byte offset is too large so this range isn't valid.
    let bad_range = [1u64 | (0x1000 << 32), 0u64];

    // Send a gpadl message for a channel that doesn't exist.
    env.c().handle_gpadl_header(
        &protocol::GpadlHeader {
            channel_id: ChannelId(100),
            gpadl_id: GpadlId(1),
            count: 1,
            len: good_range.as_bytes().len() as u16,
        },
        good_range.as_bytes(),
    );

    // Ensure an error response was sent.
    env.notifier
        .check_message(OutgoingMessage::new(&protocol::GpadlCreated {
            channel_id: ChannelId(100),
            gpadl_id: GpadlId(1),
            status: protocol::STATUS_UNSUCCESSFUL,
        }));

    // Send a gpadl message with an invalid range.
    env.c().handle_gpadl_header(
        &protocol::GpadlHeader {
            channel_id: ChannelId(1),
            gpadl_id: GpadlId(1),
            count: 1,
            len: bad_range.as_bytes().len() as u16,
        },
        bad_range.as_bytes(),
    );

    // Ensure an error response was sent.
    env.notifier
        .check_message(OutgoingMessage::new(&protocol::GpadlCreated {
            channel_id: ChannelId(1),
            gpadl_id: GpadlId(1),
            status: protocol::STATUS_UNSUCCESSFUL,
        }));

    // Send a gpadl message with an invalid range but in multiple pieces.
    env.c().handle_gpadl_header(
        &protocol::GpadlHeader {
            channel_id: ChannelId(1),
            gpadl_id: GpadlId(1),
            count: 2,
            len: (bad_range.as_bytes().len() + good_range.as_bytes().len()) as u16,
        },
        bad_range.as_bytes(),
    );

    // No response because the gpadl is incomplete.
    assert!(env.notifier.messages.is_empty());

    env.c()
        .handle_gpadl_body(
            &protocol::GpadlBody {
                rsvd: 0,
                gpadl_id: GpadlId(1),
            },
            good_range.as_bytes(),
        )
        .unwrap();

    // Ensure an error response was sent after the full gpadl is received.
    env.notifier
        .check_message(OutgoingMessage::new(&protocol::GpadlCreated {
            channel_id: ChannelId(1),
            gpadl_id: GpadlId(1),
            status: protocol::STATUS_UNSUCCESSFUL,
        }));

    // We can reuse the GPADL ID after a failure.
    env.gpadl(1, 1);

    let (offer_id, action) = env.recv.recv().unwrap();
    assert_eq!(offer_id, offer_id1);
    assert!(matches!(action, Action::Gpadl(GpadlId(1), ..)));

    env.c()
        .gpadl_create_complete(offer_id1, GpadlId(1), protocol::STATUS_SUCCESS);

    // Successful response message should be sent.
    env.notifier
        .check_message(OutgoingMessage::new(&protocol::GpadlCreated {
            channel_id: ChannelId(1),
            gpadl_id: GpadlId(1),
            status: protocol::STATUS_SUCCESS,
        }));
}

#[test]
fn test_gpadl_in_progress_reset() {
    let mut env = TestEnv::new();

    let offer_id1 = env.offer(1);

    const RANGE: [u64; 2] = [1u64, 0u64];

    env.connect(Version::Copper, FeatureFlags::new());
    env.c().handle_request_offers().unwrap();
    env.notifier.messages.clear();

    // Start a gpadl creation but do not complete it.
    env.c().handle_gpadl_header(
        &protocol::GpadlHeader {
            channel_id: ChannelId(1),
            gpadl_id: GpadlId(1),
            count: 2,
            len: 32,
        },
        RANGE.as_bytes(),
    );

    // Ensure no error was sent.
    assert!(env.notifier.messages.is_empty());

    // Reset the connection while gpadl creation is in progress.
    env.c().reset();
    env.complete_reset();
    assert!(env.notifier.is_reset());

    // Reconnect and start a new gpadl creation with the same ID, and complete it this time.
    env.connect(Version::Copper, FeatureFlags::new());
    env.c().handle_request_offers().unwrap();
    env.notifier.messages.clear();
    env.c().handle_gpadl_header(
        &protocol::GpadlHeader {
            channel_id: ChannelId(1),
            gpadl_id: GpadlId(1),
            count: 1,
            len: 32,
        },
        RANGE.as_bytes(),
    );

    env.c()
        .handle_gpadl_body(
            &protocol::GpadlBody {
                rsvd: 0,
                gpadl_id: GpadlId(1),
            },
            RANGE.as_bytes(),
        )
        .unwrap();

    // Ensure no error was sent.
    assert!(env.notifier.messages.is_empty());

    // Check for the gpadl creation notification.
    let (offer_id, action) = env.recv.recv().unwrap();
    assert_eq!(offer_id, offer_id1);
    assert!(matches!(action, Action::Gpadl(GpadlId(1), ..)));
}

struct TestNotifier {
    send: mpsc::Sender<(OfferId, Action)>,
    modify_requests: VecDeque<ModifyConnectionRequest>,
    messages: VecDeque<(OutgoingMessage, MessageTarget)>,
    hvsock_requests: Vec<HvsockConnectRequest>,
    forward_request: Option<InitiateContactRequest>,
    interrupt_page: Option<u64>,
    reset: bool,
    monitor_page: Option<MonitorPageGpas>,
    target_message_vp: Option<u32>,
    pend_messages: bool,
}

impl TestNotifier {
    fn new() -> (Self, mpsc::Receiver<(OfferId, Action)>) {
        let (send, recv) = mpsc::channel();
        (
            Self {
                send,
                modify_requests: VecDeque::new(),
                messages: VecDeque::new(),
                hvsock_requests: Vec::new(),
                forward_request: None,
                interrupt_page: None,
                reset: false,
                monitor_page: None,
                target_message_vp: None,
                pend_messages: false,
            },
            recv,
        )
    }

    fn check_message(&mut self, message: OutgoingMessage) {
        self.check_message_with_target(message, MessageTarget::Default);
    }

    fn check_message_with_target(&mut self, message: OutgoingMessage, target: MessageTarget) {
        assert_eq!(self.messages.pop_front().unwrap(), (message, target));
        assert!(self.messages.is_empty());
    }

    fn get_message<T: VmbusMessage + FromBytes + Immutable + KnownLayout>(&mut self) -> T {
        let (message, _) = self.messages.pop_front().unwrap();
        let (header, data) = protocol::MessageHeader::read_from_prefix(message.data()).unwrap();

        assert_eq!(header.message_type(), T::MESSAGE_TYPE);
        T::read_from_prefix(data).unwrap().0 // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    }

    fn check_messages(&mut self, messages: impl IntoIterator<Item = OutgoingMessage>) {
        let messages: Vec<_> = messages
            .into_iter()
            .map(|m| (m.clone(), MessageTarget::Default))
            .collect();
        assert_eq!(self.messages, messages.as_slice());
        self.messages.clear();
    }

    fn is_reset(&mut self) -> bool {
        std::mem::replace(&mut self.reset, false)
    }

    fn check_reset(&mut self) {
        assert!(self.is_reset());
        assert!(self.monitor_page.is_none());
        assert!(self.target_message_vp.is_none());
    }

    fn next_action(&mut self) -> ModifyConnectionRequest {
        self.modify_requests.pop_front().unwrap()
    }
}

impl Notifier for TestNotifier {
    fn notify(&mut self, offer_id: OfferId, action: Action) {
        tracing::debug!(?offer_id, ?action, "notify");
        self.send.send((offer_id, action)).unwrap()
    }

    fn forward_unhandled(&mut self, request: InitiateContactRequest) {
        assert!(self.forward_request.is_none());
        self.forward_request = Some(request);
    }

    fn modify_connection(&mut self, request: ModifyConnectionRequest) -> anyhow::Result<()> {
        match request.monitor_page {
            Update::Unchanged => (),
            Update::Reset => self.monitor_page = None,
            Update::Set(value) => self.monitor_page = Some(value),
        }

        if let Some(vp) = request.target_message_vp {
            self.target_message_vp = Some(vp);
        }

        match request.interrupt_page {
            Update::Unchanged => (),
            Update::Reset => self.interrupt_page = None,
            Update::Set(value) => self.interrupt_page = Some(value),
        }

        self.modify_requests.push_back(request);
        Ok(())
    }

    fn send_message(&mut self, message: &OutgoingMessage, target: MessageTarget) -> bool {
        if self.pend_messages {
            return false;
        }

        self.messages.push_back((message.clone(), target));
        true
    }

    fn notify_hvsock(&mut self, request: &HvsockConnectRequest) {
        tracing::debug!(?request, "notify_hvsock");
        // There is no hvsocket listener, so just drop everything.
        // N.B. No HvsockConnectResult will be sent to indicate failure.
        self.hvsock_requests.push(*request);
    }

    fn reset_complete(&mut self) {
        self.monitor_page = None;
        self.target_message_vp = None;
        self.reset = true;
    }

    fn unload_complete(&mut self) {}
}

struct TestEnv {
    server: Server,
    notifier: TestNotifier,
    version: Option<VersionInfo>,
    recv: mpsc::Receiver<(OfferId, Action)>,
}

impl TestEnv {
    fn new() -> Self {
        Self::with_params(false)
    }

    fn with_params(assign_channel_id_on_offer: bool) -> Self {
        let (notifier, recv) = TestNotifier::new();
        let server = Server::new(
            Vtl::Vtl0,
            MESSAGE_CONNECTION_ID,
            0,
            assign_channel_id_on_offer,
        );
        Self {
            server,
            notifier,
            version: None,
            recv,
        }
    }

    fn c(&mut self) -> ServerWithNotifier<'_, TestNotifier> {
        self.server.with_notifier(&mut self.notifier)
    }

    // Completes a reset operation if the server sends a modify request as part of it.
    fn complete_reset(&mut self) {
        let request = self.next_action();
        let response = if request.version.is_some() {
            ModifyConnectionResponse::Supported(
                protocol::ConnectionState::SUCCESSFUL,
                SUPPORTED_FEATURE_FLAGS,
                None,
            )
        } else {
            ModifyConnectionResponse::Modified(protocol::ConnectionState::SUCCESSFUL)
        };

        self.c().complete_modify_connection(response);
    }

    fn offer(&mut self, id: u32) -> OfferId {
        self.offer_inner(id, id, MnfUsage::Disabled, None, OfferFlags::new())
    }

    fn offer_with_mnf(&mut self, id: u32) -> OfferId {
        self.offer_inner(
            id,
            id,
            MnfUsage::Enabled {
                latency: Duration::from_micros(100),
            },
            None,
            OfferFlags::new(),
        )
    }

    fn offer_with_preset_mnf(&mut self, id: u32, monitor_id: u8) -> OfferId {
        self.offer_inner(
            id,
            id,
            MnfUsage::Relayed { monitor_id },
            None,
            OfferFlags::new(),
        )
    }

    fn offer_with_order(
        &mut self,
        interface_id: u32,
        instance_id: u32,
        order: Option<u64>,
    ) -> OfferId {
        self.offer_inner(
            interface_id,
            instance_id,
            MnfUsage::Disabled,
            order,
            OfferFlags::new(),
        )
    }

    fn offer_with_flags(&mut self, id: u32, flags: OfferFlags) -> OfferId {
        self.offer_inner(id, id, MnfUsage::Disabled, None, flags)
    }

    fn offer_inner(
        &mut self,
        interface_id: u32,
        instance_id: u32,
        use_mnf: MnfUsage,
        offer_order: Option<u64>,
        flags: OfferFlags,
    ) -> OfferId {
        self.c()
            .offer_channel(OfferParamsInternal {
                instance_id: guid_from_id(instance_id),
                interface_id: guid_from_id(interface_id),
                use_mnf,
                offer_order,
                flags,
                ..Default::default()
            })
            .unwrap()
    }

    fn open(&mut self, id: u32) {
        self.c()
            .handle_open_channel(&protocol::OpenChannel2 {
                open_channel: protocol::OpenChannel {
                    channel_id: ChannelId(id),
                    ..FromZeros::new_zeroed()
                },
                ..FromZeros::new_zeroed()
            })
            .unwrap()
    }

    fn close(&mut self, id: u32) -> Result<(), ChannelError> {
        self.c().handle_close_channel(&protocol::CloseChannel {
            channel_id: ChannelId(id),
        })
    }

    fn open_reserved(&mut self, id: u32, target_vp: u32, target_sint: u32) {
        let version = self.server.state.get_version().expect("vmbus connected");

        self.c()
            .handle_open_reserved_channel(
                &protocol::OpenReservedChannel {
                    channel_id: ChannelId(id),
                    target_vp,
                    target_sint,
                    ring_buffer_gpadl: GpadlId(id),
                    ..FromZeros::new_zeroed()
                },
                version,
            )
            .unwrap()
    }

    fn close_reserved(&mut self, id: u32, target_vp: u32, target_sint: u32) {
        self.c()
            .handle_close_reserved_channel(&protocol::CloseReservedChannel {
                channel_id: ChannelId(id),
                target_vp,
                target_sint,
            })
            .unwrap();
    }

    fn gpadl(&mut self, channel_id: u32, gpadl_id: u32) {
        self.c().handle_gpadl_header(
            &protocol::GpadlHeader {
                channel_id: ChannelId(channel_id),
                gpadl_id: GpadlId(gpadl_id),
                count: 1,
                len: 16,
            },
            [1u64, 0u64].as_bytes(),
        );
    }

    fn teardown_gpadl(&mut self, channel_id: u32, gpadl_id: u32) {
        self.c()
            .handle_gpadl_teardown(&protocol::GpadlTeardown {
                channel_id: ChannelId(channel_id),
                gpadl_id: GpadlId(gpadl_id),
            })
            .unwrap();
    }

    fn release(&mut self, id: u32) {
        self.c()
            .handle_rel_id_released(&protocol::RelIdReleased {
                channel_id: ChannelId(id),
            })
            .unwrap();
    }

    fn connect(&mut self, version: Version, feature_flags: FeatureFlags) {
        self.start_connect(version, feature_flags, false, true);
        self.complete_connect();
    }

    fn connect_trusted(&mut self, version: Version, feature_flags: FeatureFlags) {
        self.start_connect(version, feature_flags, true, true);
        self.complete_connect();
    }

    fn connect_without_mnf(&mut self, version: Version, feature_flags: FeatureFlags) {
        self.start_connect(version, feature_flags, false, false);
        self.complete_connect();
    }

    fn start_connect(
        &mut self,
        version: Version,
        mut feature_flags: FeatureFlags,
        trusted: bool,
        use_mnf: bool,
    ) {
        self.version = Some(VersionInfo {
            version,
            feature_flags,
        });

        let result = self.c().handle_synic_message(in_msg_ex(
            protocol::MessageType::INITIATE_CONTACT,
            protocol::InitiateContact2 {
                initiate_contact: protocol::InitiateContact {
                    version_requested: version as u32,
                    interrupt_page_or_target_info: TargetInfo::new()
                        .with_sint(VMBUS_SINT)
                        .with_vtl(0)
                        .with_feature_flags(feature_flags.into())
                        .into(),
                    child_to_parent_monitor_page_gpa: if use_mnf { 0x123f000 } else { 0 },
                    parent_to_child_monitor_page_gpa: if use_mnf { 0x321f000 } else { 0 },
                    ..FromZeros::new_zeroed()
                },
                client_id: Guid::ZERO,
            },
            false,
            trusted,
        ));
        assert!(result.is_ok());

        let request = self.notifier.next_action();
        if !trusted {
            feature_flags.set_confidential_channels(false);
        }

        assert_eq!(
            request,
            ModifyConnectionRequest {
                version: Some(VersionInfo {
                    version,
                    feature_flags
                }),
                monitor_page: if use_mnf && !self.server.require_server_allocated_mnf {
                    Update::Set(MonitorPageGpas {
                        child_to_parent: 0x123f000,
                        parent_to_child: 0x321f000,
                    })
                } else {
                    Update::Reset
                },
                interrupt_page: Update::Reset,
                target_message_vp: Some(0),
                ..Default::default()
            }
        );
    }

    fn complete_connect(&mut self) {
        self.c()
            .complete_initiate_contact(ModifyConnectionResponse::Supported(
                protocol::ConnectionState::SUCCESSFUL,
                SUPPORTED_FEATURE_FLAGS,
                None,
            ));

        let version = self.version.unwrap();
        if version.version >= Version::Copper {
            let response = self.notifier.get_message::<protocol::VersionResponse2>();
            assert_eq!(response.version_response.version_supported, 1);
            self.version = Some(VersionInfo {
                version: version.version,
                feature_flags: version.feature_flags & response.supported_features.into(),
            })
        } else {
            let response = self.notifier.get_message::<protocol::VersionResponse>();
            assert_eq!(response.version_supported, 1);
        }
    }

    fn send_message(&mut self, message: SynicMessage) {
        self.try_send_message(message).unwrap();
    }

    fn try_send_message(&mut self, message: SynicMessage) -> Result<(), ChannelError> {
        self.c().handle_synic_message(message)
    }

    fn next_action(&mut self) -> ModifyConnectionRequest {
        self.notifier.next_action()
    }

    fn reset(&mut self) {
        self.c().reset();
        assert!(self.notifier.next_action().version.is_none());
        self.c()
            .complete_modify_connection(ModifyConnectionResponse::Supported(
                protocol::ConnectionState::SUCCESSFUL,
                FeatureFlags::from_bits(u32::MAX),
                None,
            ));

        assert!(self.notifier.is_reset());
    }
}

fn in_msg<T: IntoBytes + Immutable + KnownLayout>(
    message_type: protocol::MessageType,
    t: T,
) -> SynicMessage {
    in_msg_ex(message_type, t, false, false)
}

fn in_msg_ex<T: IntoBytes + Immutable + KnownLayout>(
    message_type: protocol::MessageType,
    t: T,
    multiclient: bool,
    trusted: bool,
) -> SynicMessage {
    let mut data = Vec::new();
    data.extend_from_slice(&message_type.0.to_ne_bytes());
    data.extend_from_slice(&0u32.to_ne_bytes());
    data.extend_from_slice(t.as_bytes());
    SynicMessage {
        data,
        multiclient,
        trusted,
    }
}

fn guid_from_id(id: u32) -> Guid {
    Guid {
        data1: id,
        ..Guid::ZERO
    }
}
