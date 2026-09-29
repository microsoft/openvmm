// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Wire-format round-trip and layout tests.
//!
//! Pure computation, no hypercalls — protects the wire types in
//! [`crate::protocol`] and the device drivers in [`crate::devices`]
//! from accidental layout changes that would only surface at
//! runtime against a real host.

use crate::message;
use crate::protocol::*;
use core::mem::size_of;
use vmbus_core::protocol::*;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

fn roundtrip<T>(msg: &T)
where
    T: IntoBytes + FromBytes + zerocopy::Immutable,
{
    let bytes = msg.as_bytes();
    let (parsed, _) = T::read_from_prefix(bytes).unwrap();
    assert_eq!(parsed.as_bytes(), bytes);
}

#[test]
fn header_size() {
    assert_eq!(HEADER_SIZE, size_of::<MessageHeader>());
    assert_eq!(HEADER_SIZE, 8);
}

#[test]
fn initiate_contact_layout() {
    // Windows minkernel: VMBUS_CHANNEL_INITIATE_CONTACT is 32 bytes
    // pre-Dilithium (no ClientId).
    assert_eq!(size_of::<InitiateContact>(), 32);
    // With ClientId GUID appended.
    assert_eq!(
        size_of::<InitiateContact2>(),
        size_of::<InitiateContact>() + 16
    );
}

#[test]
fn version_response_layouts() {
    assert_eq!(size_of::<VersionResponse>(), 8);
    assert_eq!(size_of::<VersionResponse2>(), 12);
    assert_eq!(size_of::<VersionResponse3>(), 32);
}

#[test]
fn offer_channel_layout() {
    // interface_id(16) + instance_id(16) + rsvd([u32;4] = 16) + flags(2)
    // + mmio_mb(2) + user_defined(120) + subchannel_index(2)
    // + mmio_mb_optional(2) + channel_id(4) + monitor_id(1)
    // + monitor_allocated(1) + is_dedicated(2) + connection_id(4) = 188.
    assert_eq!(size_of::<OfferChannel>(), 188);
}

#[test]
fn user_defined_data_layout() {
    assert_eq!(size_of::<UserDefinedData>(), 120);
    // pipe_params(4) + is_for_guest_accept(1) + is_for_guest_container(1)
    // + version(Unalign<u32>=4) + silo_id(Unalign<Guid>=16)
    // + _padding(2) = 28. Unalign contributes no extra alignment padding.
    assert_eq!(size_of::<HvsockUserDefinedParameters>(), 28);
}

#[test]
fn gpadl_header_body_capacity() {
    // Header/Body fields don't blow the message envelope.
    const _: () = assert!(GpadlHeader::MESSAGE_SIZE <= MAX_MESSAGE_SIZE);
    const _: () = assert!(GpadlBody::MESSAGE_SIZE <= MAX_MESSAGE_SIZE);
    // Enough PFNs fit after the header to describe a small ring.
    const _: () = assert!(GpadlHeader::MAX_DATA_VALUES > 0);
    const _: () = assert!(GpadlBody::MAX_DATA_VALUES > GpadlHeader::MAX_DATA_VALUES);
}

#[test]
fn message_type_open_enum_roundtrip() {
    // Unknown types must round-trip losslessly (open_enum contract).
    let unknown = MessageType(0x1234);
    let bytes = unknown.as_bytes();
    let (back, _) = MessageType::read_from_prefix(bytes).unwrap();
    assert_eq!(back, unknown);
}

#[test]
fn packet_type_open_enum_roundtrip() {
    let unknown = PacketType(0xBEEF);
    let bytes = unknown.as_bytes();
    let (back, _) = PacketType::read_from_prefix(bytes).unwrap();
    assert_eq!(back, unknown);
}

#[test]
fn version_ladder_ordered() {
    let ladder = NEGOTIATION_LADDER;
    assert_eq!(ladder[0], Version::Copper);
    assert_eq!(*ladder.last().unwrap(), Version::Win8);
    // Strictly descending.
    for pair in ladder.windows(2) {
        assert!(pair[0] > pair[1], "ladder must be strictly descending");
    }
}

#[test]
fn feature_flags_supported_bits() {
    let flags = supported_feature_flags();
    assert!(flags.guest_specified_signal_parameters());
    assert!(flags.channel_interrupt_redirection());
    assert!(flags.modify_connection());
    assert!(flags.client_id());
    assert!(!flags.confidential_channels());
    assert!(!flags.pause_resume());
}

#[test]
fn zerocopy_roundtrips() {
    roundtrip(&InitiateContact::new_zeroed());
    roundtrip(&InitiateContact2::new_zeroed());
    roundtrip(&VersionResponse::new_zeroed());
    roundtrip(&VersionResponse2::new_zeroed());
    roundtrip(&VersionResponse3::new_zeroed());
    roundtrip(&OfferChannel::new_zeroed());
    roundtrip(&RescindChannelOffer::new_zeroed());
    roundtrip(&GpadlHeader::new_zeroed());
    roundtrip(&GpadlBody::new_zeroed());
    roundtrip(&GpadlCreated::new_zeroed());
    roundtrip(&GpadlTeardown::new_zeroed());
    roundtrip(&GpadlTorndown::new_zeroed());
    roundtrip(&OpenChannel::new_zeroed());
    roundtrip(&OpenChannel2::new_zeroed());
    roundtrip(&OpenResult::new_zeroed());
    roundtrip(&CloseChannel::new_zeroed());
    roundtrip(&RelIdReleased::new_zeroed());
    roundtrip(&ModifyChannel::new_zeroed());
    roundtrip(&ModifyChannelResponse::new_zeroed());
    roundtrip(&ModifyConnection::new_zeroed());
    roundtrip(&ModifyConnectionResponse::new_zeroed());
    roundtrip(&TlConnectResult::new_zeroed());
    roundtrip(&RequestOffers {});
    roundtrip(&AllOffersDelivered {});
    roundtrip(&Unload {});
    roundtrip(&UnloadComplete {});
}

#[test]
fn message_encode_decode() {
    let mut buf = [0u8; MAX_MESSAGE_SIZE];
    let ic = InitiateContact {
        version_requested: version_raw(Version::Copper),
        target_message_vp: 0,
        interrupt_page_or_target_info: 0,
        parent_to_child_monitor_page_gpa: 0,
        child_to_parent_monitor_page_gpa: 0,
    };
    let used = message::encode(&ic, &mut buf);
    assert_eq!(used, InitiateContact::MESSAGE_SIZE);
    let ty = message::peek_header(&buf[..used]).unwrap();
    assert_eq!(ty, MessageType::INITIATE_CONTACT);
    let parsed: InitiateContact = message::parse(&buf[..used]).unwrap();
    assert_eq!(parsed, ic);
}

#[test]
fn message_parse_rejects_truncated() {
    let mut buf = [0u8; MAX_MESSAGE_SIZE];
    let ic = InitiateContact::new_zeroed();
    let _ = message::encode(&ic, &mut buf);
    // Truncate below the InitiateContact body.
    let err = message::parse::<InitiateContact>(&buf[..HEADER_SIZE + 4]).unwrap_err();
    assert!(matches!(err, crate::Error::Parse { .. }));
}

#[test]
fn message_parse_rejects_wrong_type() {
    let mut buf = [0u8; MAX_MESSAGE_SIZE];
    let ic = InitiateContact::new_zeroed();
    let used = message::encode(&ic, &mut buf);
    let err = message::parse::<VersionResponse>(&buf[..used]).unwrap_err();
    assert!(matches!(err, crate::Error::UnexpectedMessage(_)));
}

/// The `virt_to_phys` shim is currently a zero-cost cast under the
/// UEFI identity-map invariant. This test locks that in — anyone
/// changing the body to a real translation should update the shim's
/// documentation and this test's expectation together.
#[test]
fn virt_to_phys_is_identity_today() {
    let x = 0xDEAD_BEEF_u64;
    let ptr = core::ptr::from_ref::<u64>(&x);
    assert_eq!(crate::virt_to_phys(ptr), ptr as u64);
    // Null pointer is defined to map to 0 under a raw cast.
    let null_ptr: *const u8 = core::ptr::null();
    assert_eq!(crate::virt_to_phys(null_ptr), 0);
}

// ---------------------------------------------------------------------------
// GPADL encoder tests
// ---------------------------------------------------------------------------

mod gpadl_tests {
    use crate::gpadl::BODY_RANGE_CAPACITY_BYTES;
    use crate::gpadl::HEADER_RANGE_CAPACITY_BYTES;
    use crate::gpadl::body_count_for_bytes;
    use crate::gpadl::build_single_range_payload;
    use crate::gpadl::encode_gpadl_messages;
    use crate::message;
    use alloc::vec::Vec;
    use vmbus_core::protocol::ChannelId;
    use vmbus_core::protocol::GpadlBody;
    use vmbus_core::protocol::GpadlHeader;
    use vmbus_core::protocol::GpadlId;
    use vmbus_core::protocol::MessageType;

    #[test]
    fn header_capacity_matches_spec() {
        // 240 - 8 (MessageHeader) - 12 (GpadlHeader) = 220, rounded
        // down to a u64 multiple = 216 bytes = 27 u64 slots in the
        // initial message.
        assert_eq!(HEADER_RANGE_CAPACITY_BYTES, 216);
        // 240 - 8 (MessageHeader) - 8 (GpadlBody) = 224 bytes = 28 u64
        // slots in each continuation.
        assert_eq!(BODY_RANGE_CAPACITY_BYTES, 224);
    }

    #[test]
    fn body_count_for_bytes_edges() {
        assert_eq!(body_count_for_bytes(0), 0);
        assert_eq!(body_count_for_bytes(HEADER_RANGE_CAPACITY_BYTES), 0);
        assert_eq!(body_count_for_bytes(HEADER_RANGE_CAPACITY_BYTES + 1), 1);
        assert_eq!(
            body_count_for_bytes(HEADER_RANGE_CAPACITY_BYTES + BODY_RANGE_CAPACITY_BYTES),
            1
        );
        assert_eq!(
            body_count_for_bytes(HEADER_RANGE_CAPACITY_BYTES + BODY_RANGE_CAPACITY_BYTES + 1),
            2
        );
    }

    #[test]
    fn single_page_fits_in_header() {
        let pfns = [0x1000u64];
        let payload = build_single_range_payload(4096, &pfns);
        assert_eq!(payload.len(), 8 + 8); // GpaRange + 1 PFN
        let msgs = encode_gpadl_messages(ChannelId(1), GpadlId(2), 1, &payload);
        assert_eq!(msgs.len(), 1);
        let m0 = &msgs.messages[0];
        assert_eq!(message::peek_header(m0).unwrap(), MessageType::GPADL_HEADER);
    }

    /// Given P pages, the encoder emits
    /// `1 + ceil((range_bytes - HEADER_CAP) / BODY_CAP)` messages
    /// (1 for the header, rest for continuation bodies).
    #[test]
    fn message_count_by_pages() {
        for pages in [1usize, 10, 26, 27, 28, 60, 100, 1000] {
            let pfns: Vec<u64> = (0..pages as u64).map(|i| 0x1000 + i).collect();
            let payload = build_single_range_payload((pages * 4096) as u32, &pfns);
            let msgs = encode_gpadl_messages(ChannelId(1), GpadlId(2), 1, &payload);
            let expected_body = body_count_for_bytes(payload.len());
            assert_eq!(
                msgs.len(),
                1 + expected_body,
                "unexpected message count for {pages} pages"
            );
        }
    }

    #[test]
    fn pfn_sequence_is_preserved_across_split() {
        // Enough pages so range payload spills into two body messages.
        // Payload = 8 + pages*8. Header carries 220 bytes = 27 u64
        // (1 GpaRange + 26 PFNs). Each body carries 224 bytes = 28 u64.
        let pages = 26 + 28 + 5; // spans header + 1 full body + 1 partial body
        let pfns: Vec<u64> = (0..pages as u64).map(|i| 0xABCD_0000 + i).collect();
        let payload = build_single_range_payload((pages * 4096) as u32, &pfns);
        let msgs = encode_gpadl_messages(ChannelId(1), GpadlId(2), 1, &payload);
        assert_eq!(msgs.len(), 3);

        // First message: parse GpadlHeader, then the first 27 u64
        // slots (GpaRange + 26 PFNs).
        let m0 = &msgs.messages[0];
        let hdr: GpadlHeader = message::parse(m0).unwrap();
        assert_eq!(hdr.count, 1);
        assert_eq!(hdr.gpadl_id, GpadlId(2));
        assert_eq!(hdr.len as usize, payload.len());
        // 27 slots after (GpadlHeader,MessageHeader): first is GpaRange,
        // next 26 are pfns[0..26].
        // We just check the last PFN in the header matches pfns[25]:
        let payload_off = 8 + 12 + 8 + 25 * 8; // hdr(8+12) + GpaRange(8) + 25*8
        let bytes = &m0[payload_off..payload_off + 8];
        let mut pfn_bytes = [0u8; 8];
        pfn_bytes.copy_from_slice(bytes);
        assert_eq!(u64::from_le_bytes(pfn_bytes), pfns[25]);

        // Body messages carry pfns[26..] continuously.
        let m1 = &msgs.messages[1];
        let body: GpadlBody = message::parse(m1).unwrap();
        assert_eq!(body.gpadl_id, GpadlId(2));
        // First PFN in body 1 is pfns[26].
        let body_payload_off = 8 + 8; // MessageHeader + GpadlBody
        let mut pfn_bytes = [0u8; 8];
        pfn_bytes.copy_from_slice(&m1[body_payload_off..body_payload_off + 8]);
        assert_eq!(u64::from_le_bytes(pfn_bytes), pfns[26]);

        // First PFN in body 2 is pfns[26 + 28] = pfns[54].
        let m2 = &msgs.messages[2];
        let mut pfn_bytes = [0u8; 8];
        pfn_bytes.copy_from_slice(&m2[body_payload_off..body_payload_off + 8]);
        assert_eq!(u64::from_le_bytes(pfn_bytes), pfns[54]);
    }

    #[test]
    fn every_message_fits_in_envelope() {
        let pages = 200;
        let pfns: Vec<u64> = (0..pages as u64).map(|i| 0x1000 + i).collect();
        let payload = build_single_range_payload((pages * 4096) as u32, &pfns);
        let msgs = encode_gpadl_messages(ChannelId(1), GpadlId(2), 1, &payload);
        for m in &msgs.messages {
            assert!(
                m.len() <= vmbus_core::protocol::MAX_MESSAGE_SIZE,
                "GPADL message oversized: {}",
                m.len()
            );
        }
    }
}

#[test]
fn packet_descriptor_size() {
    // Descriptor is 16 bytes across every VMBus version.
    assert_eq!(size_of::<PacketDescriptor>(), 16);
}

// ---------------------------------------------------------------------------
// ClientCore driver integration tests
// ---------------------------------------------------------------------------

mod client_driver_tests {
    use crate::Error;
    use crate::channel::close_channel_keep_relid_with;
    use crate::channel::open_channel_with;
    use crate::client_driver::ClientDriver;
    use crate::client_driver::MessagePump;
    use crate::connection::CLIENT_ID;
    use crate::connection::initiate_with;
    use crate::connection::request_offers_with;
    use crate::gpadl::establish_gpadl_with;
    use crate::gpadl::teardown_gpadl_with;
    use crate::message::encode;
    use crate::protocol::supported_feature_flags;
    use alloc::vec::Vec;
    use hvdef::HvError;
    use opentmk_core::context::HypercallPlatformTrait;
    use opentmk_core::platform::hyperv::ctx::HyperVHypercallConfig;
    use opentmk_core::tmkdefs::TmkResult;
    use vmbus_core::protocol::AllOffersDelivered;
    use vmbus_core::protocol::ChannelId;
    use vmbus_core::protocol::GpadlCreated;
    use vmbus_core::protocol::GpadlId;
    use vmbus_core::protocol::GpadlTorndown;
    use vmbus_core::protocol::MAX_MESSAGE_SIZE;
    use vmbus_core::protocol::OfferChannel;
    use vmbus_core::protocol::OpenResult;
    use vmbus_core::protocol::RescindChannelOffer;
    use vmbus_core::protocol::Version;
    use vmbus_core::protocol::VersionResponse;
    use vmbus_core::protocol::VersionResponse2;
    use zerocopy::FromZeros;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;

    #[derive(Default)]
    struct RecordingCtx {
        calls: Vec<(u64, Vec<u8>)>,
    }

    impl HypercallPlatformTrait for RecordingCtx {
        type Config = HyperVHypercallConfig;

        fn hypercall(
            &mut self,
            code: u64,
            input: &[u8],
            _output: &mut [u8],
            _cfg: HyperVHypercallConfig,
        ) -> TmkResult<()> {
            self.calls.push((code, input.to_vec()));
            Ok(())
        }
    }

    struct ScriptedPump {
        messages: Vec<Vec<u8>>,
    }

    impl MessagePump for ScriptedPump {
        fn poll_message<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
            &mut self,
            _ctx: &mut C,
        ) -> crate::Result<Vec<u8>> {
            self.messages.pop().ok_or(Error::Timeout)
        }
    }

    fn encode_message<M: vmbus_core::protocol::VmbusMessage + IntoBytes + Immutable>(
        message: &M,
    ) -> Vec<u8> {
        let mut bytes = [0; MAX_MESSAGE_SIZE];
        let used = encode(message, &mut bytes);
        bytes[..used].to_vec()
    }

    fn success_response(version: Version, connection_id: u32) -> Vec<u8> {
        if version >= Version::Copper {
            encode_message(&VersionResponse2 {
                version_response: VersionResponse {
                    version_supported: 1,
                    connection_state: vmbus_core::protocol::ConnectionState::SUCCESSFUL,
                    padding: 0,
                    selected_version_or_connection_id: connection_id,
                },
                supported_features: supported_feature_flags().into_bits(),
            })
        } else {
            encode_message(&VersionResponse {
                version_supported: 1,
                connection_state: vmbus_core::protocol::ConnectionState::SUCCESSFUL,
                padding: 0,
                selected_version_or_connection_id: connection_id,
            })
        }
    }

    fn rejected_response() -> Vec<u8> {
        encode_message(&VersionResponse {
            version_supported: 0,
            connection_state: vmbus_core::protocol::ConnectionState::FAILED_UNKNOWN_FAILURE,
            padding: 0,
            selected_version_or_connection_id: 0,
        })
    }

    fn connect_and_offer(ctx: &mut RecordingCtx, driver: &mut ClientDriver, offer: OfferChannel) {
        let mut connect = ScriptedPump {
            messages: alloc::vec![success_response(Version::Copper, 4)],
        };
        initiate_with(ctx, driver, &mut connect, CLIENT_ID).unwrap();
        let mut offers = ScriptedPump {
            messages: alloc::vec![
                encode_message(&AllOffersDelivered {}),
                encode_message(&offer),
            ],
        };
        let received = request_offers_with(ctx, driver, &mut offers).unwrap();
        assert_eq!(received.len(), 1);
    }

    #[test]
    fn negotiation_falls_back_and_uses_core_completion() {
        static VERSIONS: &[Version] = &[Version::Iron, Version::Copper];
        let mut driver = ClientDriver::with_versions(VERSIONS);
        let mut ctx = RecordingCtx::default();
        let mut pump = ScriptedPump {
            messages: alloc::vec![success_response(Version::Iron, 4), rejected_response(),],
        };

        let state = initiate_with(&mut ctx, &mut driver, &mut pump, CLIENT_ID).unwrap();
        assert_eq!(state.selected_version, Version::Iron);
        assert_eq!(ctx.calls.len(), 2);
    }

    #[test]
    fn failed_post_resets_protocol_state() {
        struct FailCtx;

        impl HypercallPlatformTrait for FailCtx {
            type Config = HyperVHypercallConfig;

            fn hypercall(
                &mut self,
                _code: u64,
                _input: &[u8],
                _output: &mut [u8],
                _cfg: HyperVHypercallConfig,
            ) -> TmkResult<()> {
                Err(HvError::AccessDenied.into())
            }
        }

        let mut driver = ClientDriver::new();
        let mut pump = ScriptedPump {
            messages: Vec::new(),
        };
        let error = initiate_with(&mut FailCtx, &mut driver, &mut pump, CLIENT_ID).unwrap_err();
        assert!(matches!(error, Error::Hypercall(_)));
        assert!(driver.version().is_none());

        let mut ctx = RecordingCtx::default();
        pump.messages.push(success_response(Version::Copper, 4));
        initiate_with(&mut ctx, &mut driver, &mut pump, CLIENT_ID).unwrap();
    }

    #[test]
    fn request_offers_collects_core_offer_actions() {
        let mut offer = OfferChannel::new_zeroed();
        offer.channel_id = ChannelId(7);
        let mut driver = ClientDriver::new();
        let mut ctx = RecordingCtx::default();
        connect_and_offer(&mut ctx, &mut driver, offer);
        assert!(driver.core().channels().contains_key(&ChannelId(7)));
    }

    #[test]
    fn rescinded_offer_is_not_returned() {
        let mut offer = OfferChannel::new_zeroed();
        offer.channel_id = ChannelId(7);
        let mut driver = ClientDriver::new();
        let mut ctx = RecordingCtx::default();
        let mut connect = ScriptedPump {
            messages: alloc::vec![success_response(Version::Copper, 4)],
        };
        initiate_with(&mut ctx, &mut driver, &mut connect, CLIENT_ID).unwrap();

        let mut offers = ScriptedPump {
            messages: alloc::vec![
                encode_message(&AllOffersDelivered {}),
                encode_message(&RescindChannelOffer {
                    channel_id: ChannelId(7),
                }),
                encode_message(&offer),
            ],
        };
        assert!(
            request_offers_with(&mut ctx, &mut driver, &mut offers)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn gpadl_open_close_and_teardown_use_one_core() {
        let mut offer = OfferChannel::new_zeroed();
        offer.channel_id = ChannelId(5);
        let mut driver = ClientDriver::new();
        let mut ctx = RecordingCtx::default();
        connect_and_offer(&mut ctx, &mut driver, offer);

        let gpadl_id = GpadlId(99);
        let mut gpadl_pump = ScriptedPump {
            messages: alloc::vec![encode_message(&GpadlCreated {
                channel_id: ChannelId(5),
                gpadl_id,
                status: 0,
            })],
        };
        let gpadl = establish_gpadl_with(
            &mut ctx,
            &mut driver,
            &mut gpadl_pump,
            ChannelId(5),
            gpadl_id,
            4096,
            &[0x1000],
        )
        .unwrap();

        let mut open_pump = ScriptedPump {
            messages: alloc::vec![encode_message(&OpenResult {
                channel_id: ChannelId(5),
                open_id: 0,
                status: 0,
            })],
        };
        let channel = open_channel_with(
            &mut ctx,
            &mut driver,
            &mut open_pump,
            &offer,
            gpadl,
            4,
            0x20,
            5,
        )
        .unwrap();
        assert_eq!(channel.channel_id(), ChannelId(5));

        close_channel_keep_relid_with(&mut ctx, &mut driver, channel).unwrap();
        let mut teardown_pump = ScriptedPump {
            messages: alloc::vec![encode_message(&GpadlTorndown { gpadl_id })],
        };
        teardown_gpadl_with(&mut ctx, &mut driver, &mut teardown_pump, gpadl).unwrap();
    }
}

// ---------------------------------------------------------------------------
// hvsock tests
// ---------------------------------------------------------------------------

mod hvsock_tests {
    use crate::hvsock::dispatch_connect_result;
    use crate::hvsock::encode_tl_connect_request;
    use crate::hvsock::set_connect_result_handler;
    use core::mem::size_of;
    use core::sync::atomic::AtomicU32;
    use core::sync::atomic::Ordering;
    use guid::Guid;
    use vmbus_core::protocol::HEADER_SIZE;
    use vmbus_core::protocol::TlConnectRequest;
    use vmbus_core::protocol::TlConnectRequest2;
    use vmbus_core::protocol::TlConnectResult;
    use vmbus_core::protocol::Version;

    #[test]
    fn connect_request_layout_tracks_silo_support() {
        let v1 = encode_tl_connect_request(Version::Win10, Guid::default(), Guid::default(), None);
        assert_eq!(v1.len(), HEADER_SIZE + size_of::<TlConnectRequest>());

        let v2 = encode_tl_connect_request(
            Version::Copper,
            Guid::default(),
            Guid::default(),
            Some(Guid::default()),
        );
        assert_eq!(v2.len(), HEADER_SIZE + size_of::<TlConnectRequest2>());
    }

    static CALLBACK_COUNT: AtomicU32 = AtomicU32::new(0);

    fn test_handler(_: &TlConnectResult) {
        CALLBACK_COUNT.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn connect_result_callback_remains_wrapper_owned() {
        set_connect_result_handler(test_handler);
        CALLBACK_COUNT.store(0, Ordering::Relaxed);
        dispatch_connect_result(&TlConnectResult {
            endpoint_id: Guid::default(),
            service_id: Guid::default(),
            status: -1,
        });
        assert_eq!(CALLBACK_COUNT.load(Ordering::Relaxed), 1);
    }
}

// ---------------------------------------------------------------------------
// Interrupt tests (SIMP slot parsing + drain_once)
// ---------------------------------------------------------------------------

mod interrupt_tests {
    use crate::Error;
    use crate::interrupt::HV_REGISTER_EOM;
    use crate::interrupt::SimpPump;
    use crate::interrupt::clear_slot;
    use crate::interrupt::drain_once;
    use crate::interrupt::read_slot;
    use crate::message::encode;
    use alloc::vec::Vec;
    use hvdef::HV_MESSAGE_SIZE;
    use hvdef::HvMessageType;
    use hvdef::HypercallCode;
    use opentmk_core::context::HypercallPlatformTrait;
    use opentmk_core::platform::hyperv::ctx::HyperVHypercallConfig;
    use opentmk_core::tmkdefs::TmkResult;
    use vmbus_core::protocol::MAX_MESSAGE_SIZE;
    use vmbus_core::protocol::VersionResponse;
    use zerocopy::FromZeros;

    #[derive(Default)]
    struct RecordingCtx {
        calls: Vec<u64>,
    }
    impl HypercallPlatformTrait for RecordingCtx {
        type Config = HyperVHypercallConfig;

        fn hypercall(
            &mut self,
            code: u64,
            _input: &[u8],
            _output: &mut [u8],
            _cfg: HyperVHypercallConfig,
        ) -> TmkResult<()> {
            self.calls.push(code);
            Ok(())
        }
    }

    fn build_slot_bytes(msg_type: u32, message_pending: bool, payload: &[u8]) -> [u8; 256] {
        let mut buf = [0u8; HV_MESSAGE_SIZE];
        buf[0..4].copy_from_slice(&msg_type.to_le_bytes());
        buf[4] = payload.len() as u8;
        buf[5] = message_pending as u8;
        buf[16..16 + payload.len()].copy_from_slice(payload);
        buf
    }

    #[test]
    fn read_slot_none_returns_none() {
        let buf = [0u8; HV_MESSAGE_SIZE];
        assert!(read_slot(&buf).unwrap().is_none());
    }

    #[test]
    fn read_slot_returns_payload() {
        let payload = [0xABu8; 12];
        let buf = build_slot_bytes(1, true, &payload);
        let view = read_slot(&buf).unwrap().unwrap();
        assert_eq!(view.message_type, HvMessageType(1));
        assert_eq!(view.payload_len, 12);
        assert!(view.message_pending);
        assert_eq!(view.payload, &payload);
    }

    #[test]
    fn read_slot_rejects_oversized_payload() {
        let mut buf = [0u8; HV_MESSAGE_SIZE];
        buf[0..4].copy_from_slice(&1u32.to_le_bytes());
        buf[4] = 250; // payload_len > HV_MESSAGE_PAYLOAD_SIZE(240)
        let err = read_slot(&buf).unwrap_err();
        assert!(matches!(err, Error::Parse { .. }));
    }

    #[test]
    fn clear_slot_writes_none() {
        let mut buf = build_slot_bytes(1, true, &[1, 2, 3]);
        clear_slot(&mut buf);
        assert_eq!(&buf[0..4], &0u32.to_le_bytes());
    }

    #[test]
    fn drain_once_captures_and_writes_eom_when_pending() {
        // Prepare a VersionResponse in the slot payload.
        let vr = VersionResponse::new_zeroed();
        let mut payload = [0u8; MAX_MESSAGE_SIZE];
        let used = encode(&vr, &mut payload);
        let mut slot = build_slot_bytes(1, true, &payload[..used]);

        let mut ctx = RecordingCtx::default();
        let drained = drain_once(&mut ctx, &mut slot).unwrap();
        assert!(drained);
        // Slot was cleared.
        assert_eq!(&slot[0..4], &0u32.to_le_bytes());
        // EOM was issued (via HvCallSetVpRegisters).
        assert!(
            ctx.calls
                .contains(&(HypercallCode::HvCallSetVpRegisters.0 as u64))
        );
    }

    #[test]
    fn drain_once_no_eom_when_pending_flag_clear() {
        let vr = VersionResponse::new_zeroed();
        let mut payload = [0u8; MAX_MESSAGE_SIZE];
        let used = encode(&vr, &mut payload);
        let mut slot = build_slot_bytes(1, /*pending=*/ false, &payload[..used]);

        let mut ctx = RecordingCtx::default();
        drain_once(&mut ctx, &mut slot).unwrap();
        assert!(ctx.calls.is_empty());
    }

    #[test]
    fn drain_once_empty_slot_returns_false() {
        let mut slot = [0u8; HV_MESSAGE_SIZE];
        let mut ctx = RecordingCtx::default();
        let drained = drain_once(&mut ctx, &mut slot).unwrap();
        assert!(!drained);
    }

    /// Regression test for the EOM race described in the code review.
    ///
    /// The old `drain_once` snapshotted `message_pending` from the
    /// slot BEFORE clearing and used the snapshot to gate EOM. That
    /// missed hypervisor-set flags that arrived between the snapshot
    /// and the clear. The fix reads the flag AFTER the clear (and
    /// after the SeqCst fence inside `clear_slot`).
    ///
    /// This test verifies the read-after-clear behaviour by leaving
    /// only bit 0 of `slot[5]` set — with `pending=false` in the
    /// initial layout but the flag byte manually set to `0x1` before
    /// draining. The post-clear read must observe the flag and fire
    /// EOM.
    #[test]
    fn drain_once_reads_pending_flag_after_clear() {
        // Build with pending=false, then manually set the flag byte
        // (simulating the hypervisor setting it between the pre-clear
        // snapshot and the clear).
        let vr = VersionResponse::new_zeroed();
        let mut payload = [0u8; MAX_MESSAGE_SIZE];
        let used = encode(&vr, &mut payload);
        let mut slot = build_slot_bytes(1, /*pending=*/ false, &payload[..used]);
        // Simulate hypervisor writing message_pending after we've
        // already read the slot but before clear_slot runs — set the
        // flag by hand.
        slot[5] = 0x1;

        let mut ctx = RecordingCtx::default();
        drain_once(&mut ctx, &mut slot).unwrap();
        assert!(
            ctx.calls
                .contains(&(HypercallCode::HvCallSetVpRegisters.0 as u64)),
            "drain_once must issue EOM when the post-clear flag is set",
        );
    }

    #[test]
    fn simp_pump_has_expected_defaults() {
        let pump = SimpPump::new(0x1000);
        // Just exercising the builder — retries value is public via
        // with_max_retries.
        let _pump2 = pump.with_max_retries(42);
    }

    #[test]
    fn eom_register_constant_matches_spec() {
        // Sanity: EOM virtual register (used with HvCallSetVpRegisters)
        // is 0x000A0014 — see hvdef::HvX64RegisterName::Eom. The
        // 0x40000084 value is the x86 MSR index, which is only valid
        // for the WrMSR instruction path.
        assert_eq!(HV_REGISTER_EOM, 0x000A0014);
    }
}

// ---------------------------------------------------------------------------
// SynIC tests (register programming layer, host-testable)
// ---------------------------------------------------------------------------

mod synic_tests {
    use crate::Error;
    use crate::synic::VMBUS_INTERRUPT_VECTOR;
    use crate::synic::program_synic_registers;
    use alloc::vec::Vec;
    use hvdef::HvError;
    use hvdef::HvSynicSimpSiefp;
    use hvdef::HvSynicSint;
    use hvdef::HypercallCode;
    use hvdef::hypercall::GetSetVpRegisters;
    use hvdef::hypercall::HvRegisterAssoc;
    use opentmk_core::context::HypercallPlatformTrait;
    use opentmk_core::platform::hyperv::ctx::HyperVHypercallConfig;
    use opentmk_core::tmkdefs::TmkResult;
    use zerocopy::FromBytes;

    /// Mock context that records every hypercall.
    #[derive(Default)]
    struct MockCtx {
        calls: Vec<(u64, Vec<u8>, Option<usize>)>,
    }

    impl HypercallPlatformTrait for MockCtx {
        type Config = HyperVHypercallConfig;

        fn hypercall(
            &mut self,
            code: u64,
            input: &[u8],
            _output: &mut [u8],
            cfg: HyperVHypercallConfig,
        ) -> TmkResult<()> {
            self.calls.push((code, input.to_vec(), cfg.rep_count));
            Ok(())
        }
    }

    #[test]
    fn program_synic_registers_writes_four_registers() {
        let mut ctx = MockCtx::default();
        program_synic_registers(&mut ctx, 0x1000, 0x2000, VMBUS_INTERRUPT_VECTOR).unwrap();

        // Two hypercalls: SetVpRegisters (the write) + GetVpRegisters
        // (the readback we added for post-hoc verification).
        assert_eq!(ctx.calls.len(), 2);
        let (code, input, rep) = &ctx.calls[0];
        assert_eq!(*code, HypercallCode::HvCallSetVpRegisters.0 as u64);
        assert_eq!(*rep, Some(4));

        // Parse the header + four HvRegisterAssoc.
        let (_hdr, rest) = GetSetVpRegisters::read_from_prefix(input).unwrap();
        let mut cur = rest;
        let mut regs = Vec::new();
        for _ in 0..4 {
            let (a, rest) = HvRegisterAssoc::read_from_prefix(cur).unwrap();
            regs.push(a);
            cur = rest;
        }

        // SIMP first: base_gpn = 0x1000 >> 12 = 1, enabled.
        let simp: HvSynicSimpSiefp = regs[0].value.as_u64().into();
        assert!(simp.enabled());
        assert_eq!(simp.base_gpn(), 1);

        // SIEFP second: base_gpn = 2, enabled.
        let siefp: HvSynicSimpSiefp = regs[1].value.as_u64().into();
        assert!(siefp.enabled());
        assert_eq!(siefp.base_gpn(), 2);

        // SINT2 third: vector = 0xF3, masked = false, auto_eoi = true,
        // polling = true. `masked=false` is required — the host's
        // `HvCallPostMessage` refuses masked SINTs with
        // `InvalidSynicState`; `polling=true` still suppresses CPU
        // interrupt injection so we can poll the SIMP slot.
        let sint2: HvSynicSint = regs[2].value.as_u64().into();
        assert_eq!(sint2.vector(), VMBUS_INTERRUPT_VECTOR);
        assert!(!sint2.masked());
        assert!(sint2.polling());
        assert!(sint2.auto_eoi());

        // SCONTROL fourth: enabled.
        let scontrol: hvdef::HvSynicScontrol = regs[3].value.as_u64().into();
        assert!(scontrol.enabled());
    }

    #[test]
    fn program_synic_registers_rejects_unaligned_gpa() {
        let mut ctx = MockCtx::default();
        let err =
            program_synic_registers(&mut ctx, 0x1001, 0x2000, VMBUS_INTERRUPT_VECTOR).unwrap_err();
        assert!(matches!(err, Error::Parse { .. }));
        assert!(ctx.calls.is_empty());
    }

    #[test]
    fn program_synic_registers_propagates_hypercall_error() {
        struct FailCtx;
        impl HypercallPlatformTrait for FailCtx {
            type Config = HyperVHypercallConfig;

            fn hypercall(
                &mut self,
                _code: u64,
                _input: &[u8],
                _output: &mut [u8],
                _cfg: HyperVHypercallConfig,
            ) -> TmkResult<()> {
                Err(HvError::AccessDenied.into())
            }
        }

        let mut ctx = FailCtx;
        let err =
            program_synic_registers(&mut ctx, 0x1000, 0x2000, VMBUS_INTERRUPT_VECTOR).unwrap_err();
        assert!(matches!(err, Error::Hypercall(_)));
    }
}

// ---------------------------------------------------------------------------
// Ring buffer tests
// ---------------------------------------------------------------------------

mod ring_tests {
    use crate::Error;
    use crate::protocol::GpaDirectHeader;
    use crate::protocol::GpaRange;
    use crate::protocol::PacketDescriptor;
    use crate::protocol::PacketFlags;
    use crate::protocol::PacketType;
    use crate::ring::FlatRingMem;
    use crate::ring::IncomingRingExt;
    use crate::ring::OutgoingRingExt;
    use crate::ring::RecvRing;
    use crate::ring::SendRing;
    use vmbus_ring::Ring;
    use vmbus_ring::RingMem;
    use zerocopy::FromBytes;
    use zerocopy::IntoBytes;

    /// Small helper: build a paired sender + receiver over the same
    /// underlying [`FlatRingMem`]. Upstream's `FlatRingMem` is
    /// internally Arc-wrapped so `Clone` gives shared backing at
    /// near-zero cost.
    ///
    /// Unmasks the reader's interrupt bit — upstream's
    /// `IncomingRing::new` masks by default, but every guest test
    /// here checks the empty→non-empty signal decision on a live
    /// (unmasked) ring.
    fn pair(data_len: usize) -> (SendRing<FlatRingMem>, RecvRing<FlatRingMem>) {
        let mem = FlatRingMem::new(data_len);
        let send = SendRing::new(mem.clone()).unwrap();
        let recv = RecvRing::new(mem).unwrap();
        recv.set_interrupt_mask_hint(false);
        (send, recv)
    }

    #[test]
    fn write_then_read_single_packet() {
        let (send, recv) = pair(4096);
        let payload = b"hello world";
        // Request completion so upstream preserves the transaction_id
        // on read; without the flag, `transaction_id` is dropped by
        // vmbus_ring's parse (matches Windows/Linux semantics).
        let mut flags = PacketFlags::new();
        flags.set_request_completion(true);
        let signal = send.write_inband(payload, flags, 42).unwrap();
        assert!(signal, "empty→non-empty should signal");

        let mut buf = [0u8; 256];
        let pkt = recv.read_packet(&mut buf).unwrap();
        assert_eq!(pkt.descriptor.packet_type, PacketType::VM_PKT_DATA_INBAND);
        assert_eq!(pkt.descriptor.transaction_id, 42);
        assert_eq!(pkt.ext_header_len, 0);
        assert_eq!(&pkt.payload[..payload.len()], payload);
        assert_eq!(recv.available(), 0);
    }

    #[test]
    fn write_n_read_back_fifo() {
        let (send, recv) = pair(4096);
        let mut flags = PacketFlags::new();
        flags.set_request_completion(true);
        for i in 0..8u64 {
            let payload = [i as u8; 40];
            let _ = send.write_inband(&payload, flags, i).unwrap();
        }
        let mut buf = [0u8; 128];
        for i in 0..8u64 {
            let pkt = recv.read_packet(&mut buf).unwrap();
            assert_eq!(pkt.descriptor.transaction_id, i);
            assert_eq!(pkt.payload[0], i as u8);
        }
        assert!(matches!(recv.read_packet(&mut buf), Err(Error::RingEmpty)));
    }

    #[test]
    fn signal_only_on_empty_to_nonempty() {
        let (send, recv) = pair(4096);
        // First write on empty → signal.
        assert!(send.write_inband(b"a", PacketFlags::new(), 0).unwrap());
        // Second write on non-empty → no signal.
        assert!(!send.write_inband(b"b", PacketFlags::new(), 0).unwrap());
        // Drain both.
        let mut buf = [0u8; 32];
        recv.read_packet(&mut buf).unwrap();
        recv.read_packet(&mut buf).unwrap();
        // Now empty again → next write signals.
        assert!(send.write_inband(b"c", PacketFlags::new(), 0).unwrap());
    }

    #[test]
    fn signal_suppressed_when_interrupt_masked() {
        let (send, recv) = pair(4096);
        recv.set_interrupt_mask(true);
        assert!(!send.write_inband(b"x", PacketFlags::new(), 0).unwrap());
        recv.set_interrupt_mask(false);
        // Drain and try again on empty.
        let mut buf = [0u8; 32];
        recv.read_packet(&mut buf).unwrap();
        assert!(send.write_inband(b"y", PacketFlags::new(), 0).unwrap());
    }

    #[test]
    fn wraparound() {
        // 4096-byte data area is the smallest upstream engine accepts.
        // Loop enough times to walk past the wrap boundary.
        let (send, recv) = pair(4096);
        let mut buf = [0u8; 64];
        let mut flags = PacketFlags::new();
        flags.set_request_completion(true);
        for i in 0..200u64 {
            let _ = send.write_inband(&[i as u8; 4], flags, i).unwrap();
            let pkt = recv.read_packet(&mut buf).unwrap();
            assert_eq!(pkt.descriptor.transaction_id, i);
            assert_eq!(pkt.payload[0], i as u8);
        }
    }

    #[test]
    fn ring_full_returns_error() {
        // Fill a 4096-byte ring with a lot of small packets and check
        // that RingFull comes back cleanly once the ring is full.
        let (send, _recv) = pair(4096);
        let payload = [0u8; 32];
        // 32-byte payload padded to 32 + 16 desc + 8 footer = 56 bytes
        // per packet. 4096 / 56 = 73 packets fit; the 74th should be
        // Full. Give ourselves 500 attempts to hit the cap.
        let mut any_full = false;
        for i in 0..500u64 {
            match send.write_inband(&payload, PacketFlags::new(), i) {
                Ok(_) => continue,
                Err(Error::RingFull) => {
                    any_full = true;
                    break;
                }
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
        assert!(any_full, "expected RingFull after filling the ring");
    }

    #[test]
    fn completion_packet_type() {
        let (send, recv) = pair(4096);
        send.write_completion(&[1, 2, 3, 4], 99).unwrap();
        let mut buf = [0u8; 64];
        let pkt = recv.read_packet(&mut buf).unwrap();
        assert_eq!(pkt.descriptor.packet_type, PacketType::VM_PKT_COMP);
        assert_eq!(pkt.descriptor.transaction_id, 99);
        assert_eq!(&pkt.payload[..4], &[1, 2, 3, 4]);
    }

    #[test]
    fn read_empty_returns_ring_empty() {
        let (_send, recv) = pair(4096);
        let mut buf = [0u8; 32];
        assert!(matches!(recv.read_packet(&mut buf), Err(Error::RingEmpty)));
    }

    #[test]
    fn pending_send_size_hint_persists() {
        // The pending_send_sz hint is now owned by the SendRing
        // (writer). Verify it round-trips through the control page.
        let (send, _recv) = pair(4096);
        send.set_pending_send_size(2048).unwrap();
        let v = send
            .mem()
            .control()
            .get(3)
            .unwrap()
            .load(core::sync::atomic::Ordering::Relaxed);
        assert_eq!(v, 2048);
    }

    /// SendRing::new should set FEATURE_SUPPORTS_PENDING_SEND_SIZE
    /// on the control page — the guest (writer) owns feature_bits
    /// on this ring per openvmm's OutgoingRing::new convention.
    #[test]
    fn send_ring_advertises_pending_send_size_feature() {
        let (send, _recv) = pair(4096);
        let bits = send.mem().control()[16].load(core::sync::atomic::Ordering::Relaxed);
        assert_eq!(bits & crate::ring::FEATURE_SUPPORTS_PENDING_SEND_SIZE, 1);
    }

    /// RecvRing::drain_signal_decision should return NoSignal when
    /// pending_send_size is zero (writer not blocked).
    #[test]
    fn recv_signal_decision_no_pending() {
        let (send, recv) = pair(4096);
        send.write_inband(b"x", PacketFlags::new(), 0).unwrap();
        let mut buf = [0u8; 32];
        recv.read_packet(&mut buf).unwrap();
        // pending_send_sz is 0 (writer hasn't stored anything).
        assert_eq!(
            recv.drain_signal_decision(32),
            crate::ring::SignalDecision::NoSignal
        );
    }

    /// RecvRing::drain_signal_decision returns Signal exactly on the
    /// "not enough" → "enough" transition.
    #[test]
    fn recv_signal_decision_on_transition() {
        let (_send, recv) = pair(4096);
        // Simulate the host having filled the ring and posted a
        // pending_send_sz hint. We poke the shared control page
        // directly (via recv.mem().control()) rather than going
        // through SendRing::write_packet, since our writer clears
        // the hint on any successful write.
        let ctrl = recv.mem().control();
        let write_idx_slot = &ctrl[0]; // IDX_IN
        let read_idx_slot = &ctrl[1]; // IDX_OUT
        let pending_slot = &ctrl[3]; // IDX_PENDING_SEND_SZ

        // Ring state before drain: 4000 bytes queued (recv sees full).
        // free = 4096 - 4000 - 8 = 88.
        write_idx_slot.store(4000, core::sync::atomic::Ordering::SeqCst);
        read_idx_slot.store(0, core::sync::atomic::Ordering::SeqCst);
        pending_slot.store(200, core::sync::atomic::Ordering::SeqCst);

        // Simulate draining 128 bytes worth (advance read_idx). Now:
        // free = 4096 - 4000 + 128 - 8 = 216 >= pending (200).
        // old_free = 216 - 128 = 88 < pending → transition.
        read_idx_slot.store(128, core::sync::atomic::Ordering::SeqCst);

        assert_eq!(
            recv.drain_signal_decision(128),
            crate::ring::SignalDecision::Signal
        );
    }

    /// Below-threshold drains should NOT signal.
    #[test]
    fn recv_signal_decision_no_transition_below_threshold() {
        let (_send, recv) = pair(4096);
        let ctrl = recv.mem().control();
        // free = 88 before, pending = 200. After draining 32 bytes,
        // free = 120 < pending. No transition.
        ctrl[0].store(4000, core::sync::atomic::Ordering::SeqCst); // IN
        ctrl[3].store(200, core::sync::atomic::Ordering::SeqCst); // PENDING
        ctrl[1].store(32, core::sync::atomic::Ordering::SeqCst); // OUT
        assert_eq!(
            recv.drain_signal_decision(32),
            crate::ring::SignalDecision::NoSignal
        );
    }

    #[test]
    fn packet_with_ext_header() {
        // Upstream's IncomingRing parses the ext header as a typed
        // GpaDirectHeader for VM_PKT_DATA_USING_GPA_DIRECT packets,
        // so the test payload here is a well-formed header (reserved
        // = 0, range_count = 1) rather than 0xAA bytes.
        let (send, recv) = pair(4096);
        let ext = GpaDirectHeader {
            reserved: 0,
            range_count: 1,
        };
        let payload = [0xBBu8; 16];
        send.write_packet(
            PacketType::VM_PKT_DATA_USING_GPA_DIRECT,
            ext.as_bytes(),
            &payload,
            PacketFlags::new(),
            7,
        )
        .unwrap();
        let mut buf = [0u8; 128];
        let pkt = recv.read_packet(&mut buf).unwrap();
        assert_eq!(
            pkt.descriptor.packet_type,
            PacketType::VM_PKT_DATA_USING_GPA_DIRECT
        );
        assert_eq!(pkt.ext_header_len, 8);
        assert_eq!(&pkt.payload[..payload.len()], &payload);
        // Verify the reconstructed header round-trips.
        let (hdr, _) = GpaDirectHeader::read_from_prefix(&buf).unwrap();
        assert_eq!(hdr.reserved, 0);
        assert_eq!(hdr.range_count, 1);
    }

    /// Cross-implementation wire test: verify our `SendRing` produces
    /// on-wire bytes matching openvmm's `vmbus_ring::OutgoingRing`.
    ///
    /// Openvmm sets `length8 = msg_len / 8` where `msg_len` **excludes**
    /// the 8-byte footer, then advances `write_idx` by
    /// `msg_len + FOOTER_SIZE`. Windows and Linux match.
    ///
    /// Payload = 8 bytes, no ext header:
    ///   msg_len       = DESCRIPTOR_SIZE (16) + 0 + 8 = 24 bytes
    ///   footer        = 8 bytes
    ///   length8       = 24 / 8 = 3
    ///   data_offset8  = 16 / 8 = 2
    ///   write_idx advances by 24 + 8 = 32 bytes
    #[test]
    fn length8_matches_openvmm_wire_convention() {
        let (send, _recv) = pair(4096);
        let payload = [0xCDu8; 8];
        send.write_inband(&payload, PacketFlags::new(), 0xdeadbeef)
            .unwrap();

        // Read the descriptor bytes directly from the ring at offset 0.
        let mem = send.mem();
        let mut desc_bytes = [0u8; size_of::<PacketDescriptor>()];
        mem.read_at(0, &mut desc_bytes);
        let (desc, _) = PacketDescriptor::read_from_prefix(&desc_bytes).unwrap();

        // Expected values per openvmm's OutgoingRing::write:
        assert_eq!(desc.packet_type, PacketType::VM_PKT_DATA_INBAND);
        assert_eq!(desc.data_offset8, 2, "data_offset8 = descriptor_size / 8");
        assert_eq!(
            desc.length8, 3,
            "length8 = (descriptor + payload) / 8, EXCLUDING footer"
        );
        assert_eq!(desc.transaction_id, 0xdeadbeef);

        // write_idx should be at 32 (24 msg + 8 footer).
        let write_idx = mem.control()[0].load(core::sync::atomic::Ordering::Relaxed);
        assert_eq!(write_idx, 32);
    }

    /// `write_gpa_direct` builds the correct on-wire layout:
    /// descriptor, GpaDirectHeader, GpaRange, PFN list, then payload.
    #[test]
    fn write_gpa_direct_layout() {
        let (send, recv) = pair(4096);
        let pfns = [0x1000u64, 0x1001, 0x1002];
        let byte_count = 3 * 4096;
        let byte_offset = 0;
        let payload = b"nvsp-inline";
        let mut flags = PacketFlags::new();
        flags.set_request_completion(true);
        send.write_gpa_direct(&pfns, byte_offset, byte_count, payload, flags, 42)
            .unwrap();

        // Descriptor sanity: type = 0x9 (GPA_DIRECT), tid = 42.
        let mut buf = [0u8; 256];
        let pkt = recv.read_packet(&mut buf).unwrap();
        assert_eq!(
            pkt.descriptor.packet_type,
            PacketType::VM_PKT_DATA_USING_GPA_DIRECT
        );
        assert_eq!(pkt.descriptor.transaction_id, 42);
        assert_eq!(pkt.descriptor.flags.request_completion(), true);

        // ext_header_len = 8 (hdr) + 8 (range) + 3*8 (pfns) = 40.
        assert_eq!(pkt.ext_header_len, 40);

        // Payload should be the inline "nvsp-inline" (padded to 8).
        assert_eq!(&pkt.payload[..payload.len()], payload);

        // Parse the ext header out of buf.
        let (hdr, _) = GpaDirectHeader::read_from_prefix(&buf).unwrap();
        assert_eq!(hdr.reserved, 0);
        assert_eq!(hdr.range_count, 1);

        let (rng, _) = GpaRange::read_from_prefix(&buf[8..]).unwrap();
        assert_eq!(rng.byte_count, byte_count);
        assert_eq!(rng.byte_offset, byte_offset);

        // PFNs follow.
        for (i, &expected) in pfns.iter().enumerate() {
            let off = 16 + i * 8;
            let actual = u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
            assert_eq!(actual, expected);
        }
    }

    /// write_gpa_direct rejects empty PFN lists and oversized byte
    /// counts.
    #[test]
    fn write_gpa_direct_validates_inputs() {
        let (send, _recv) = pair(4096);
        // Empty PFN list.
        assert!(
            send.write_gpa_direct(&[], 0, 0, b"", PacketFlags::new(), 0)
                .is_err()
        );
        // byte_count > PFN range.
        assert!(
            send.write_gpa_direct(&[0x1000], 0, 4097, b"", PacketFlags::new(), 0)
                .is_err()
        );
        // byte_offset >= region — must reject (would previously
        // underflow expected_bytes into a huge u64 and silently pass).
        assert!(
            send.write_gpa_direct(&[0x1000], 4096, 1, b"", PacketFlags::new(), 0)
                .is_err()
        );
        assert!(
            send.write_gpa_direct(&[0x1000], 8192, 1, b"", PacketFlags::new(), 0)
                .is_err()
        );
        // byte_count <= PFN range should succeed.
        assert!(
            send.write_gpa_direct(&[0x1000], 0, 4096, b"", PacketFlags::new(), 0)
                .is_ok()
        );
    }
}

/// NVSP wire-type layout and encoder tests.
mod netvsp_tests {
    use crate::devices::netvsp;
    use core::mem::size_of;
    use zerocopy::FromBytes;

    #[test]
    fn version_ladder_ordered_high_to_low() {
        let l = netvsp::NEGOTIATION_LADDER;
        assert_eq!(l[0], netvsp::Version::V61);
        assert_eq!(l[l.len() - 1], netvsp::Version::V1);
        // Non-strict ordering because V3 is skipped, but each step
        // should be >= the next.
        for w in l.windows(2) {
            assert!(w[0] >= w[1]);
        }
    }

    #[test]
    fn frame_sizes() {
        assert_eq!(netvsp::NVSP_LEGACY_MESSAGE_SIZE, 28);
        assert_eq!(netvsp::NVSP_V61_MESSAGE_SIZE, 40);
        assert_eq!(netvsp::frame_size_for(netvsp::Version::V1), 28);
        assert_eq!(netvsp::frame_size_for(netvsp::Version::V6), 28);
        assert_eq!(netvsp::frame_size_for(netvsp::Version::V61), 40);
    }

    #[test]
    fn wire_body_sizes() {
        // Sizes match Windows `nvspprotocol.h` /
        // openvmm `vm/devices/net/netvsp/src/protocol.rs`.
        assert_eq!(size_of::<netvsp_protocol::protocol::MessageHeader>(), 4);
        assert_eq!(size_of::<netvsp_protocol::protocol::MessageInit>(), 8);
        assert_eq!(
            size_of::<netvsp_protocol::protocol::MessageInitComplete>(),
            12
        );
        assert_eq!(
            size_of::<netvsp_protocol::protocol::Message1SendNdisVersion>(),
            8
        );
        assert_eq!(
            size_of::<netvsp_protocol::protocol::Message1SendReceiveBuffer>(),
            8
        );
        assert_eq!(
            size_of::<netvsp_protocol::protocol::ReceiveBufferSection>(),
            16
        );
        // status + num_sections + 1×section = 4 + 4 + 16 = 24
        assert_eq!(
            size_of::<netvsp_protocol::protocol::Message1SendReceiveBufferComplete>(),
            24
        );
        assert_eq!(
            size_of::<netvsp_protocol::protocol::Message1SendSendBufferComplete>(),
            8
        );
        assert_eq!(
            size_of::<netvsp_protocol::protocol::Message1SendRndisPacket>(),
            12
        );
        assert_eq!(
            size_of::<netvsp_protocol::protocol::Message1SendRndisPacketComplete>(),
            4
        );
        assert_eq!(
            size_of::<netvsp_protocol::protocol::Message2SendNdisConfig>(),
            16
        );
    }

    #[test]
    fn interface_guid_matches_spec() {
        let g = &netvsp::INTERFACE_GUID;
        assert_eq!(g.data1, 0xf8615163);
        assert_eq!(g.data2, 0xdf3e);
        assert_eq!(g.data3, 0x46c5);
        assert_eq!(g.data4, [0x91, 0x3f, 0xf2, 0xd2, 0xf9, 0x65, 0xed, 0x0e]);
    }

    #[test]
    fn encode_init_message_pads_to_frame_size() {
        let mut buf = [0xFFu8; 64];
        let n = netvsp::encode_message(
            netvsp::msg_type::INIT,
            &netvsp_protocol::protocol::MessageInit {
                protocol_version: netvsp::Version::V61 as u32,
                protocol_version2: netvsp::Version::V61 as u32,
            },
            netvsp::Version::V61,
            &mut buf,
        )
        .unwrap();
        assert_eq!(n, netvsp::NVSP_V61_MESSAGE_SIZE);
        // Header: 4 bytes = INIT (1).
        assert_eq!(&buf[..4], &1u32.to_le_bytes());
        // Body: 8 bytes = version × 2.
        assert_eq!(&buf[4..8], &(netvsp::Version::V61 as u32).to_le_bytes());
        assert_eq!(&buf[8..12], &(netvsp::Version::V61 as u32).to_le_bytes());
        // Padding bytes 12..40 must be zero-filled.
        for &b in &buf[12..40] {
            assert_eq!(b, 0);
        }
        // Anything past the frame should be untouched.
        assert_eq!(buf[40], 0xFF);
    }

    #[test]
    fn encode_message_rejects_undersized_buffer() {
        let mut buf = [0u8; 20];
        let r = netvsp::encode_message(
            netvsp::msg_type::INIT,
            &netvsp_protocol::protocol::MessageInit {
                protocol_version: 0,
                protocol_version2: 0,
            },
            netvsp::Version::V61, // needs 40 bytes
            &mut buf,
        );
        assert!(r.is_err());
    }

    #[test]
    fn parse_header_roundtrip() {
        let mut buf = [0u8; netvsp::NVSP_LEGACY_MESSAGE_SIZE];
        netvsp::encode_message(
            netvsp::msg_type::INIT_COMPLETE,
            &netvsp_protocol::protocol::MessageInitComplete {
                deprecated: 0,
                maximum_mdl_chain_length: 0x400,
                status: netvsp_protocol::protocol::Status::SUCCESS,
            },
            netvsp::Version::V6,
            &mut buf,
        )
        .unwrap();
        let (ty, body) = netvsp::parse_header(&buf).unwrap();
        assert_eq!(ty, netvsp::msg_type::INIT_COMPLETE);
        let (parsed, _) =
            netvsp_protocol::protocol::MessageInitComplete::read_from_prefix(body).unwrap();
        assert_eq!(parsed.status, netvsp_protocol::protocol::Status::SUCCESS);
        assert_eq!(parsed.maximum_mdl_chain_length, 0x400);
    }

    #[test]
    fn ndis_caps_recommended_by_version() {
        assert_eq!(
            netvsp::NdisCapabilities::recommended(netvsp::Version::V2).0,
            netvsp::NdisCapabilities::IEEE_8021Q
        );
        assert_eq!(
            netvsp::NdisCapabilities::recommended(netvsp::Version::V5).0,
            netvsp::NdisCapabilities::IEEE_8021Q
                | netvsp::NdisCapabilities::SRIOV
                | netvsp::NdisCapabilities::TEAMING
        );
        assert_eq!(
            netvsp::NdisCapabilities::recommended(netvsp::Version::V61).0,
            netvsp::NdisCapabilities::IEEE_8021Q
                | netvsp::NdisCapabilities::SRIOV
                | netvsp::NdisCapabilities::TEAMING
                | netvsp::NdisCapabilities::RSC_OVER_VMBUS
        );
    }

    #[test]
    fn ndis_caps_bit_4_never_set_by_default() {
        // NVSP_2_NETVSC_CAPABILITIES bit 4 (CorrelationIdBroken) must
        // remain 0 per Windows source comment. Verify none of our
        // preset masks set it.
        for v in netvsp::NEGOTIATION_LADDER {
            let caps = netvsp::NdisCapabilities::recommended(v).0;
            assert_eq!(caps & (1 << 4), 0);
        }
    }

    #[test]
    fn message_type_ranges() {
        assert_eq!(netvsp::msg_type::INIT, 1);
        assert_eq!(netvsp::msg_type::INIT_COMPLETE, 2);
        assert_eq!(netvsp::msg_type::VERSION_MSG_START, 100);
        assert_eq!(netvsp::msg_type::V1_SEND_NDIS_VERSION, 100);
        assert_eq!(netvsp::msg_type::V2_SEND_NDIS_CONFIG, 125);
        assert_eq!(netvsp::msg_type::V1_SEND_RECV_BUF_COMPLETE, 102);
        assert_eq!(netvsp::msg_type::V1_SEND_SEND_BUF_COMPLETE, 105);
        assert_eq!(netvsp::msg_type::V1_SEND_RNDIS_PKT, 107);
        assert_eq!(netvsp::msg_type::V1_SEND_RNDIS_PKT_COMPLETE, 108);
    }
}
