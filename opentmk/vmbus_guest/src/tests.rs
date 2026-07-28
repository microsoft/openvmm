// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Wire-format round-trip and layout tests.
//!
//! These are the "unit tests without a VM" tests from §8.1 of the design
//! doc. Nothing here touches hypercalls; it exists purely to protect the
//! wire types in [`crate::protocol`] from accidental layout changes.

use crate::message;
use crate::protocol::*;
use core::mem::size_of;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

fn roundtrip<T>(msg: &T)
where
    T: IntoBytes + FromBytes + PartialEq + core::fmt::Debug + zerocopy::Immutable,
{
    let bytes = msg.as_bytes();
    let (parsed, _) = T::read_from_prefix(bytes).unwrap();
    assert_eq!(&parsed, msg);
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
    let ladder = Version::NEGOTIATION_LADDER;
    assert_eq!(ladder[0], Version::Copper);
    assert_eq!(*ladder.last().unwrap(), Version::Win8);
    // Strictly descending.
    for pair in ladder.windows(2) {
        assert!(pair[0] > pair[1], "ladder must be strictly descending");
    }
}

#[test]
fn feature_flags_supported_bits() {
    let flags = FeatureFlags::supported();
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
    roundtrip(&RequestOffers);
    roundtrip(&AllOffersDelivered);
    roundtrip(&Unload);
    roundtrip(&UnloadComplete);
}

#[test]
fn message_encode_decode() {
    let mut buf = [0u8; MAX_MESSAGE_SIZE];
    let ic = InitiateContact {
        version_requested: Version::Copper.raw(),
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

// ---------------------------------------------------------------------------
// GPADL tests (§8.1 test 4)
// ---------------------------------------------------------------------------

mod gpadl_tests {
    use crate::gpadl::BODY_RANGE_CAPACITY_BYTES;
    use crate::gpadl::HEADER_RANGE_CAPACITY_BYTES;
    use crate::gpadl::body_count_for_bytes;
    use crate::gpadl::build_single_range_payload;
    use crate::gpadl::encode_gpadl_messages;
    use crate::message;
    use crate::protocol::ChannelId;
    use crate::protocol::GpadlBody;
    use crate::protocol::GpadlHeader;
    use crate::protocol::GpadlId;
    use crate::protocol::MessageType;
    use alloc::vec::Vec;

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

    /// Exact §8.1 test 4: given P pages, we emit
    /// `1 + ceil((range_bytes - HEADER_CAP) / BODY_CAP)` messages.
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
                m.len() <= crate::protocol::MAX_MESSAGE_SIZE,
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
// Completion table tests
// ---------------------------------------------------------------------------

mod completion_tests {
    use crate::Error;
    use crate::message::CompletionKey;
    use crate::message::CompletionTable;
    use crate::message::completion_key_for;
    use crate::message::encode;
    use crate::protocol::ChannelId;
    use crate::protocol::GpadlCreated;
    use crate::protocol::GpadlId;
    use crate::protocol::GpadlTorndown;
    use crate::protocol::MAX_MESSAGE_SIZE;
    use crate::protocol::OpenResult;
    use crate::protocol::UnloadComplete;
    use crate::protocol::VersionResponse;
    use zerocopy::FromZeros;

    #[test]
    fn register_deliver_take() {
        let table = CompletionTable::new();
        let handle = table.register(CompletionKey::VersionResponse);
        assert!(!handle.completed());
        assert_eq!(handle.take_response(), None);

        table
            .deliver(CompletionKey::VersionResponse, alloc::vec![1u8, 2, 3])
            .unwrap();
        assert!(handle.completed());
        assert_eq!(handle.take_response(), Some(alloc::vec![1u8, 2, 3]));
        // Second take yields None.
        assert_eq!(handle.take_response(), None);
    }

    #[test]
    fn deliver_orphan_completion() {
        let table = CompletionTable::new();
        let err = table
            .deliver(CompletionKey::VersionResponse, alloc::vec![])
            .unwrap_err();
        assert!(matches!(err, Error::OrphanCompletion));
    }

    #[test]
    fn multiple_keys_independent() {
        let table = CompletionTable::new();
        let a = table.register(CompletionKey::GpadlCreated(GpadlId(1)));
        let b = table.register(CompletionKey::GpadlCreated(GpadlId(2)));
        assert_eq!(table.pending(), 2);
        table
            .deliver(CompletionKey::GpadlCreated(GpadlId(2)), alloc::vec![])
            .unwrap();
        assert!(b.completed());
        assert!(!a.completed());
    }

    #[test]
    fn drop_removes_registration() {
        let table = CompletionTable::new();
        {
            let _h = table.register(CompletionKey::VersionResponse);
            assert_eq!(table.pending(), 1);
        }
        assert_eq!(table.pending(), 0);
    }

    #[test]
    fn completion_key_for_version_response() {
        let mut buf = [0u8; MAX_MESSAGE_SIZE];
        let vr = VersionResponse::new_zeroed();
        let used = encode(&vr, &mut buf);
        let key = completion_key_for(&buf[..used]).unwrap().unwrap();
        assert_eq!(key, CompletionKey::VersionResponse);
    }

    #[test]
    fn completion_key_for_gpadl_created_uses_id() {
        let mut buf = [0u8; MAX_MESSAGE_SIZE];
        let mut gc = GpadlCreated::new_zeroed();
        gc.gpadl_id = GpadlId(0xDEAD_BEEF);
        let used = encode(&gc, &mut buf);
        let key = completion_key_for(&buf[..used]).unwrap().unwrap();
        assert_eq!(key, CompletionKey::GpadlCreated(GpadlId(0xDEAD_BEEF)));
    }

    #[test]
    fn completion_key_for_open_result_uses_open_id() {
        let mut buf = [0u8; MAX_MESSAGE_SIZE];
        let mut or = OpenResult::new_zeroed();
        or.channel_id = ChannelId(1);
        or.open_id = 0x1234;
        let used = encode(&or, &mut buf);
        let key = completion_key_for(&buf[..used]).unwrap().unwrap();
        assert_eq!(key, CompletionKey::OpenChannelResult(0x1234));
    }

    #[test]
    fn completion_key_for_torndown_and_unload() {
        let mut buf = [0u8; MAX_MESSAGE_SIZE];
        let mut gt = GpadlTorndown::new_zeroed();
        gt.gpadl_id = GpadlId(9);
        let used = encode(&gt, &mut buf);
        let key = completion_key_for(&buf[..used]).unwrap().unwrap();
        assert_eq!(key, CompletionKey::GpadlTorndown(GpadlId(9)));

        let used = encode(&UnloadComplete, &mut buf);
        let key = completion_key_for(&buf[..used]).unwrap().unwrap();
        assert_eq!(key, CompletionKey::UnloadComplete);
    }

    #[test]
    fn completion_key_for_non_completion_returns_none() {
        let mut buf = [0u8; MAX_MESSAGE_SIZE];
        // OfferChannel is not a completion.
        let oc = crate::protocol::OfferChannel::new_zeroed();
        let used = encode(&oc, &mut buf);
        assert!(completion_key_for(&buf[..used]).unwrap().is_none());
    }
}

// ---------------------------------------------------------------------------
// Connection / negotiation tests
// ---------------------------------------------------------------------------

mod connection_tests {
    use crate::Error;
    use crate::connection::CLIENT_ID;
    use crate::connection::MessagePump;
    use crate::connection::OfferCollector;
    use crate::connection::encode_initiate_contact;
    use crate::connection::negotiate_version;
    use crate::connection::parse_version_response;
    use crate::connection::request_offers_with;
    use crate::message::CompletionHandle;
    use crate::message::CompletionKey;
    use crate::message::CompletionTable;
    use crate::message::MessageSink;
    use crate::message::encode;
    use crate::protocol::AllOffersDelivered;
    use crate::protocol::FeatureFlags;
    use crate::protocol::HEADER_SIZE;
    use crate::protocol::InitiateContact;
    use crate::protocol::InitiateContact2;
    use crate::protocol::MAX_MESSAGE_SIZE;
    use crate::protocol::OfferChannel;
    use crate::protocol::TargetInfo;
    use crate::protocol::VMBUS_CONNECTION_ID_LEGACY;
    use crate::protocol::VMBUS_CONNECTION_ID_MODERN;
    use crate::protocol::Version;
    use crate::protocol::VersionResponse;
    use crate::protocol::VersionResponse2;
    use alloc::vec::Vec;
    use core::mem::size_of;
    use hvdef::HvError;
    use opentmk::context::HypercallConfig;
    use opentmk::context::HypercallTrait;
    use opentmk::tmkdefs::TmkResult;
    use zerocopy::FromZeros;
    use zerocopy::IntoBytes;

    /// Mock ctx that captures every hypercall.
    #[derive(Default)]
    struct RecordingCtx {
        calls: Vec<(u64, Vec<u8>)>,
        fail_next: bool,
    }

    impl HypercallTrait for RecordingCtx {
        fn hypercall(
            &mut self,
            code: u64,
            input: &[u8],
            _output: &mut [u8],
            _cfg: HypercallConfig,
        ) -> TmkResult<()> {
            self.calls.push((code, input.to_vec()));
            if self.fail_next {
                self.fail_next = false;
                return Err(HvError::OperationFailed.into());
            }
            Ok(())
        }
    }

    /// Pump that delivers a predetermined sequence of responses on the
    /// N-th poll, keyed to the completion table it wraps.
    struct ScriptedPump {
        table: CompletionTable,
        script: Vec<(CompletionKey, Vec<u8>)>,
        offers: Vec<Vec<u8>>,
    }

    impl MessagePump for ScriptedPump {
        fn poll_until<C: HypercallTrait>(
            &mut self,
            _ctx: &mut C,
            handle: &CompletionHandle,
            sink: &mut dyn MessageSink,
        ) -> Result<(), Error> {
            // Flush any queued OfferChannel bytes into the sink.
            for offer_bytes in self.offers.drain(..) {
                crate::message::route_message(&offer_bytes, &self.table, sink)?;
            }
            if handle.completed() {
                return Ok(());
            }
            // Deliver the next scripted response.
            if let Some((key, bytes)) = self.script.pop() {
                self.table.deliver(key, bytes)?;
            }
            if !handle.completed() {
                return Err(Error::Timeout);
            }
            Ok(())
        }
    }

    fn encode_message<M: crate::protocol::VmbusMessage + IntoBytes + zerocopy::Immutable>(
        msg: &M,
    ) -> Vec<u8> {
        let mut buf = [0u8; MAX_MESSAGE_SIZE];
        let used = encode(msg, &mut buf);
        buf[..used].to_vec()
    }

    #[test]
    fn encode_initiate_contact_pre_copper_no_client_id() {
        let bytes = encode_initiate_contact(Version::Win10Rs5, None, FeatureFlags::supported());
        assert_eq!(bytes.len(), HEADER_SIZE + size_of::<InitiateContact>());
        let ic: InitiateContact = crate::message::parse(&bytes).unwrap();
        assert_eq!(ic.version_requested, Version::Win10Rs5.raw());
        // For ≥ 5.0, interrupt_page_or_target_info is a TargetInfo.
        let ti = TargetInfo::from(ic.interrupt_page_or_target_info);
        assert_eq!(ti.sint(), crate::synic::VMBUS_SINT);
        assert_eq!(ti.vtl(), 0);
        assert_eq!(ti.feature_flags(), FeatureFlags::supported().into_bits());
    }

    #[test]
    fn encode_initiate_contact_copper_uses_v2() {
        let bytes =
            encode_initiate_contact(Version::Copper, Some(CLIENT_ID), FeatureFlags::supported());
        assert_eq!(bytes.len(), HEADER_SIZE + size_of::<InitiateContact2>());
        let ic2: InitiateContact2 = crate::message::parse(&bytes).unwrap();
        assert_eq!(
            ic2.initiate_contact.version_requested,
            Version::Copper.raw()
        );
        assert_eq!(ic2.client_id, CLIENT_ID);
    }

    #[test]
    fn encode_initiate_contact_v1_no_target_info() {
        let bytes = encode_initiate_contact(Version::Win10, None, FeatureFlags::supported());
        let ic: InitiateContact = crate::message::parse(&bytes).unwrap();
        assert_eq!(ic.interrupt_page_or_target_info, 0);
    }

    #[test]
    fn parse_version_response_v1_layout() {
        let vr = VersionResponse {
            version_supported: 1,
            connection_state: crate::protocol::ConnectionState::SUCCESSFUL,
            padding: 0,
            selected_version_or_connection_id: 42,
        };
        let bytes = encode_message(&vr);
        let parsed = parse_version_response(&bytes).unwrap();
        assert!(parsed.version_supported);
        assert_eq!(parsed.selected_version_or_connection_id, 42);
        assert_eq!(parsed.supported_features, FeatureFlags::new());
    }

    #[test]
    fn parse_version_response_copper_v2_reads_features() {
        let vr2 = VersionResponse2 {
            version_response: VersionResponse {
                version_supported: 1,
                connection_state: crate::protocol::ConnectionState::SUCCESSFUL,
                padding: 0,
                selected_version_or_connection_id: 4,
            },
            supported_features: FeatureFlags::supported().into_bits(),
        };
        let bytes = encode_message(&vr2);
        let parsed = parse_version_response(&bytes).unwrap();
        assert_eq!(parsed.supported_features, FeatureFlags::supported());
    }

    fn make_success_response(version: Version, conn_id: u32) -> Vec<u8> {
        if version >= Version::Copper {
            let vr2 = VersionResponse2 {
                version_response: VersionResponse {
                    version_supported: 1,
                    connection_state: crate::protocol::ConnectionState::SUCCESSFUL,
                    padding: 0,
                    selected_version_or_connection_id: conn_id,
                },
                supported_features: FeatureFlags::supported().into_bits(),
            };
            encode_message(&vr2)
        } else {
            let vr = VersionResponse {
                version_supported: 1,
                connection_state: crate::protocol::ConnectionState::SUCCESSFUL,
                padding: 0,
                selected_version_or_connection_id: conn_id,
            };
            encode_message(&vr)
        }
    }

    fn make_fail_response() -> Vec<u8> {
        let vr = VersionResponse {
            version_supported: 0,
            connection_state: crate::protocol::ConnectionState::FAILED_UNKNOWN_FAILURE,
            padding: 0,
            selected_version_or_connection_id: 0,
        };
        encode_message(&vr)
    }

    #[test]
    fn negotiate_first_version_accepted() {
        let mut ctx = RecordingCtx::default();
        let table = CompletionTable::new();
        let mut pump = ScriptedPump {
            table: table.clone(),
            script: alloc::vec![(
                CompletionKey::VersionResponse,
                make_success_response(Version::Copper, 0xABCD),
            )],
            offers: Vec::new(),
        };

        let state =
            negotiate_version(&mut ctx, &table, &mut pump, CLIENT_ID, &[Version::Copper]).unwrap();
        assert_eq!(state.selected_version, Version::Copper);
        assert_eq!(state.post_message_connection_id, 0xABCD);
        assert_eq!(state.feature_flags, FeatureFlags::supported());

        // ctx captured a single post_message hypercall for InitiateContact.
        assert_eq!(ctx.calls.len(), 1);
        assert_eq!(
            ctx.calls[0].0,
            hvdef::HypercallCode::HvCallPostMessage.0 as u64
        );
    }

    #[test]
    fn negotiate_walks_ladder_on_failure() {
        let mut ctx = RecordingCtx::default();
        let table = CompletionTable::new();
        // Script is popped, so the last entry is delivered first.
        // Order: Copper fails, Iron succeeds.
        let mut pump = ScriptedPump {
            table: table.clone(),
            script: alloc::vec![
                (
                    CompletionKey::VersionResponse,
                    make_success_response(Version::Iron, 4),
                ),
                (CompletionKey::VersionResponse, make_fail_response()),
            ],
            offers: Vec::new(),
        };

        let state = negotiate_version(
            &mut ctx,
            &table,
            &mut pump,
            CLIENT_ID,
            &[Version::Copper, Version::Iron],
        )
        .unwrap();
        assert_eq!(state.selected_version, Version::Iron);
        assert_eq!(state.post_message_connection_id, 4);
        assert_eq!(ctx.calls.len(), 2);
    }

    #[test]
    fn negotiate_empty_ladder_returns_mismatch() {
        let mut ctx = RecordingCtx::default();
        let table = CompletionTable::new();
        let mut pump = ScriptedPump {
            table: table.clone(),
            script: Vec::new(),
            offers: Vec::new(),
        };
        let err = negotiate_version(&mut ctx, &table, &mut pump, CLIENT_ID, &[]).unwrap_err();
        assert!(matches!(err, Error::VersionMismatch));
    }

    #[test]
    fn initial_connection_ids() {
        use crate::connection::initial_connection_id;
        assert_eq!(
            initial_connection_id(Version::Win8),
            VMBUS_CONNECTION_ID_LEGACY
        );
        assert_eq!(
            initial_connection_id(Version::Win10Rs3_1),
            VMBUS_CONNECTION_ID_MODERN
        );
        assert_eq!(
            initial_connection_id(Version::Copper),
            VMBUS_CONNECTION_ID_MODERN
        );
    }

    #[test]
    fn request_offers_collects_offers_before_terminator() {
        let mut ctx = RecordingCtx::default();
        let table = CompletionTable::new();
        let offer_a = OfferChannel::new_zeroed();
        let mut offer_b = OfferChannel::new_zeroed();
        offer_b.channel_id = crate::protocol::ChannelId(7);
        let offer_bytes_a = encode_message(&offer_a);
        let offer_bytes_b = encode_message(&offer_b);
        let mut pump = ScriptedPump {
            table: table.clone(),
            script: alloc::vec![(
                CompletionKey::AllOffersDelivered,
                encode_message(&AllOffersDelivered),
            )],
            offers: alloc::vec![offer_bytes_a, offer_bytes_b],
        };

        let mut sink = OfferCollector::default();
        request_offers_with(&mut ctx, &table, &mut pump, &mut sink, 4).unwrap();
        assert_eq!(sink.offers.len(), 2);
        assert_eq!(sink.offers[1].channel_id, crate::protocol::ChannelId(7));
        assert!(sink.rescinds.is_empty());
    }

    #[test]
    fn build_connection_state_pre_5_uses_legacy_conn_id() {
        use crate::connection::build_connection_state;
        let parsed = crate::connection::ParsedVersionResponse {
            version_supported: true,
            selected_version_or_connection_id: 999,
            supported_features: FeatureFlags::new(),
            parent_to_child_monitor_page_gpa: 0,
            child_to_parent_monitor_page_gpa: 0,
        };
        let state = build_connection_state(Version::Win8, parsed);
        assert_eq!(state.post_message_connection_id, VMBUS_CONNECTION_ID_LEGACY);
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
    use crate::message::CompletionKey;
    use crate::message::CompletionTable;
    use crate::message::MessageSink;
    use crate::message::encode;
    use crate::protocol::MAX_MESSAGE_SIZE;
    use crate::protocol::VersionResponse;
    use alloc::vec::Vec;
    use hvdef::HV_MESSAGE_SIZE;
    use hvdef::HvMessageType;
    use hvdef::HypercallCode;
    use opentmk::context::HypercallConfig;
    use opentmk::context::HypercallTrait;
    use opentmk::tmkdefs::TmkResult;
    use zerocopy::FromZeros;

    #[derive(Default)]
    struct NullSink;
    impl MessageSink for NullSink {
        fn offer(&mut self, _: &crate::protocol::OfferChannel) {}
        fn rescind(&mut self, _: &crate::protocol::RescindChannelOffer) {}
    }

    #[derive(Default)]
    struct RecordingCtx {
        calls: Vec<u64>,
    }
    impl HypercallTrait for RecordingCtx {
        fn hypercall(
            &mut self,
            code: u64,
            _input: &[u8],
            _output: &mut [u8],
            _cfg: HypercallConfig,
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
    fn drain_once_routes_and_writes_eom_when_pending() {
        // Prepare a VersionResponse in the slot payload.
        let vr = VersionResponse::new_zeroed();
        let mut payload = [0u8; MAX_MESSAGE_SIZE];
        let used = encode(&vr, &mut payload);
        let mut slot = build_slot_bytes(1, true, &payload[..used]);

        let mut ctx = RecordingCtx::default();
        let table = CompletionTable::new();
        let _handle = table.register(CompletionKey::VersionResponse);
        let mut sink = NullSink;

        let drained = drain_once(&mut ctx, &mut slot, &table, &mut sink).unwrap();
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
        let table = CompletionTable::new();
        let _handle = table.register(CompletionKey::VersionResponse);
        let mut sink = NullSink;

        drain_once(&mut ctx, &mut slot, &table, &mut sink).unwrap();
        assert!(ctx.calls.is_empty());
    }

    #[test]
    fn drain_once_empty_slot_returns_false() {
        let mut slot = [0u8; HV_MESSAGE_SIZE];
        let mut ctx = RecordingCtx::default();
        let table = CompletionTable::new();
        let mut sink = NullSink;
        let drained = drain_once(&mut ctx, &mut slot, &table, &mut sink).unwrap();
        assert!(!drained);
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
        // Sanity: EOM register index is 0x40000084 per Hyper-V TLFS.
        assert_eq!(HV_REGISTER_EOM, 0x40000084);
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
    use opentmk::context::HypercallConfig;
    use opentmk::context::HypercallTrait;
    use opentmk::tmkdefs::TmkResult;
    use zerocopy::FromBytes;

    /// Mock context that records every hypercall.
    #[derive(Default)]
    struct MockCtx {
        calls: Vec<(u64, Vec<u8>, Option<usize>)>,
    }

    impl HypercallTrait for MockCtx {
        fn hypercall(
            &mut self,
            code: u64,
            input: &[u8],
            _output: &mut [u8],
            cfg: HypercallConfig,
        ) -> TmkResult<()> {
            self.calls.push((code, input.to_vec(), cfg.rep_count));
            Ok(())
        }
    }

    #[test]
    fn program_synic_registers_writes_four_registers() {
        let mut ctx = MockCtx::default();
        program_synic_registers(&mut ctx, 0x1000, 0x2000, VMBUS_INTERRUPT_VECTOR).unwrap();

        assert_eq!(ctx.calls.len(), 1);
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

        // SINT2 third: vector = 0xF3, masked = false, auto_eoi = true.
        let sint2: HvSynicSint = regs[2].value.as_u64().into();
        assert_eq!(sint2.vector(), VMBUS_INTERRUPT_VECTOR);
        assert!(!sint2.masked());
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
        impl HypercallTrait for FailCtx {
            fn hypercall(
                &mut self,
                _code: u64,
                _input: &[u8],
                _output: &mut [u8],
                _cfg: HypercallConfig,
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
// Ring buffer tests (§8.1 test 3)
// ---------------------------------------------------------------------------

mod ring_tests {
    use crate::Error;
    use crate::protocol::PacketFlags;
    use crate::protocol::PacketType;
    use crate::ring::FlatRingMem;
    use crate::ring::RecvRing;
    use crate::ring::RingMem;
    use crate::ring::SendRing;
    use alloc::sync::Arc;

    /// Small helper: build a paired sender + receiver over the same
    /// underlying [`FlatRingMem`].
    fn pair(data_len: usize) -> (SendRing<Arc<FlatRingMem>>, RecvRing<Arc<FlatRingMem>>) {
        let mem = Arc::new(FlatRingMem::new(data_len));
        (SendRing::new(mem.clone()), RecvRing::new(mem))
    }

    // Delegate RingMem for Arc so send/recv can share the same backing.
    impl<M: RingMem> RingMem for Arc<M> {
        fn control(&self) -> &[core::sync::atomic::AtomicU32] {
            (**self).control()
        }
        fn data_len(&self) -> usize {
            (**self).data_len()
        }
        fn read_at(&self, off: usize, data: &mut [u8]) {
            (**self).read_at(off, data)
        }
        fn write_at(&self, off: usize, data: &[u8]) {
            (**self).write_at(off, data)
        }
    }

    #[test]
    fn write_then_read_single_packet() {
        let (send, recv) = pair(4096);
        let payload = b"hello world";
        let signal = send.write_inband(payload, PacketFlags::new(), 42).unwrap();
        assert!(signal, "empty→non-empty should signal");

        let mut buf = [0u8; 256];
        let pkt = recv.read(&mut buf).unwrap();
        assert_eq!(pkt.descriptor.packet_type, PacketType::VM_PKT_DATA_INBAND);
        assert_eq!(pkt.descriptor.transaction_id, 42);
        assert_eq!(pkt.ext_header_len, 0);
        assert_eq!(&pkt.payload[..payload.len()], payload);
        // Padding bytes are undefined per protocol; only the payload
        // length is meaningful. Trailing bytes are zero here because we
        // zero-fill on write.
        assert_eq!(recv.available(), 0);
    }

    #[test]
    fn write_n_read_back_fifo() {
        let (send, recv) = pair(4096);
        for i in 0..8u64 {
            let payload = [i as u8; 40];
            let _ = send.write_inband(&payload, PacketFlags::new(), i).unwrap();
        }
        let mut buf = [0u8; 128];
        for i in 0..8u64 {
            let pkt = recv.read(&mut buf).unwrap();
            assert_eq!(pkt.descriptor.transaction_id, i);
            assert_eq!(pkt.payload[0], i as u8);
        }
        assert!(matches!(recv.read(&mut buf), Err(Error::RingEmpty)));
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
        recv.read(&mut buf).unwrap();
        recv.read(&mut buf).unwrap();
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
        recv.read(&mut buf).unwrap();
        assert!(send.write_inband(b"y", PacketFlags::new(), 0).unwrap());
    }

    #[test]
    fn wraparound() {
        // 64-byte data area — smallest legal power-of-two that fits
        // more than one 32-byte inband packet (16-byte descriptor +
        // 8-byte payload padded + 8-byte footer = 32 bytes).
        let (send, recv) = pair(64);
        let mut buf = [0u8; 64];
        // Fill and drain enough times to cross the boundary.
        for i in 0..20u64 {
            let _ = send
                .write_inband(&[i as u8; 4], PacketFlags::new(), i)
                .unwrap();
            let pkt = recv.read(&mut buf).unwrap();
            assert_eq!(pkt.descriptor.transaction_id, i);
            assert_eq!(pkt.payload[0], i as u8);
        }
    }

    #[test]
    fn ring_full_returns_error() {
        // 64-byte ring: room for exactly one 32-byte packet minus the
        // reserved slot (56 bytes usable).
        let (send, _recv) = pair(64);
        // First 32-byte packet fits.
        send.write_inband(&[0u8; 4], PacketFlags::new(), 0).unwrap();
        // Second 32-byte packet does not (56 - 32 = 24 < 32).
        assert!(matches!(
            send.write_inband(&[0u8; 4], PacketFlags::new(), 1),
            Err(Error::RingFull)
        ));
    }

    #[test]
    fn completion_packet_type() {
        let (send, recv) = pair(4096);
        send.write_completion(&[1, 2, 3, 4], 99).unwrap();
        let mut buf = [0u8; 64];
        let pkt = recv.read(&mut buf).unwrap();
        assert_eq!(pkt.descriptor.packet_type, PacketType::VM_PKT_COMP);
        assert_eq!(pkt.descriptor.transaction_id, 99);
        assert_eq!(&pkt.payload[..4], &[1, 2, 3, 4]);
    }

    #[test]
    fn read_empty_returns_ring_empty() {
        let (_send, recv) = pair(4096);
        let mut buf = [0u8; 32];
        assert!(matches!(recv.read(&mut buf), Err(Error::RingEmpty)));
    }

    #[test]
    fn pending_send_size_hint_persists() {
        let (_send, recv) = pair(4096);
        recv.set_pending_send_size(2048);
        // Read it back through the control-word slice.
        let v = recv
            .mem()
            .control()
            .get(3)
            .unwrap()
            .load(core::sync::atomic::Ordering::Relaxed);
        assert_eq!(v, 2048);
    }

    #[test]
    fn packet_with_ext_header() {
        let (send, recv) = pair(4096);
        let ext = [0xAAu8; 8];
        let payload = [0xBBu8; 16];
        send.write_packet(
            PacketType::VM_PKT_DATA_USING_GPA_DIRECT,
            &ext,
            &payload,
            PacketFlags::new(),
            7,
        )
        .unwrap();
        let mut buf = [0u8; 128];
        let pkt = recv.read(&mut buf).unwrap();
        assert_eq!(
            pkt.descriptor.packet_type,
            PacketType::VM_PKT_DATA_USING_GPA_DIRECT
        );
        assert_eq!(pkt.ext_header_len, 8);
        assert_eq!(&pkt.payload[..payload.len()], &payload);
        // The ext header sits before the payload in the read buffer.
        assert_eq!(&buf[..8], &ext);
    }
}
