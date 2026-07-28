// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `InitiateContact` / `VersionResponse` negotiation and top-level
//! connection state.
//!
//! Implements the version-negotiation ladder from §3 of
//! `tasks/vmbus-port-design.md` and `RequestOffers` /
//! `AllOffersDelivered` enumeration.
//!
//! # Structure
//!
//! * The wire-format work (encode an `InitiateContact[/2]`, parse a
//!   `VersionResponse[/2/3]`) is host-testable pure computation.
//! * The state machine (walk the ladder, register a completion, post,
//!   poll a drain callback, take the response) is also host-testable
//!   by injecting a mock drain that delivers responses synchronously.
//! * Only the UEFI [`initiate`] / [`request_offers`] entry points
//!   actually touch the SIMP page and are `cfg(target_os = "uefi")`
//!   gated.

use crate::Error;
use crate::Result;
use crate::message::CompletionHandle;
use crate::message::CompletionKey;
use crate::message::CompletionTable;
use crate::message::MessageSink;
use crate::protocol::FeatureFlags;
use crate::protocol::Guid;
use crate::protocol::HEADER_SIZE;
use crate::protocol::InitiateContact;
use crate::protocol::InitiateContact2;
use crate::protocol::MAX_MESSAGE_SIZE;
use crate::protocol::MessageHeader;
use crate::protocol::MessageType;
use crate::protocol::OfferChannel;
use crate::protocol::RescindChannelOffer;
use crate::protocol::TargetInfo;
use crate::protocol::TlConnectResult;
use crate::protocol::Unload;
use crate::protocol::VMBUS_CONNECTION_ID_LEGACY;
use crate::protocol::VMBUS_CONNECTION_ID_MODERN;
use crate::protocol::Version;
use crate::protocol::VersionResponse;
use crate::protocol::VersionResponse2;
use crate::protocol::VersionResponse3;
use crate::synic::VMBUS_SINT;
use alloc::vec::Vec;
use core::mem::size_of;
use opentmk::context::HypercallTrait;
use spin::Mutex;
use zerocopy::IntoBytes;

/// Top-level connection state established after successful negotiation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionState {
    /// Version selected by the host.
    pub selected_version: Version,
    /// Connection ID to use for `HvCallPostMessage` from now on.
    pub post_message_connection_id: u32,
    /// Feature flags supported by both sides.
    pub feature_flags: FeatureFlags,
    /// Server-provided parent → child monitor page GPA, if any.
    pub parent_to_child_monitor_page_gpa: u64,
    /// Server-provided child → parent monitor page GPA, if any.
    pub child_to_parent_monitor_page_gpa: u64,
}

/// Singleton connection state.
static CONNECTION: Mutex<Option<ConnectionState>> = Mutex::new(None);

/// Return a lock guard over the negotiated connection state.
pub fn connection() -> spin::MutexGuard<'static, Option<ConnectionState>> {
    CONNECTION.lock()
}

/// The initial connection id to use when posting `InitiateContact` for
/// the given `version`.
///
/// * `< 5.0` → legacy id (1).
/// * `≥ 5.0` → modern id (4), overridden by the returned
///   `msg_conn_id` in `VersionResponse` after negotiation.
pub fn initial_connection_id(version: Version) -> u32 {
    if version < Version::Win10Rs3_1 {
        VMBUS_CONNECTION_ID_LEGACY
    } else {
        VMBUS_CONNECTION_ID_MODERN
    }
}

/// Encode an `InitiateContact` (or `InitiateContact2` when
/// `client_id.is_some()`) into wire bytes ready to be posted.
///
/// * `version` — the version being requested.
/// * `client_id` — advertised `CLIENT_ID` (Copper+ only). `None` means
///   post the legacy `InitiateContact` struct.
/// * `feature_flags` — the feature bits we advertise to the host.
pub fn encode_initiate_contact(
    version: Version,
    client_id: Option<Guid>,
    feature_flags: FeatureFlags,
) -> Vec<u8> {
    // `interrupt_page_or_target_info` is either a raw interrupt-page
    // GPA (< 5.0) or a `TargetInfo` bitfield (≥ 5.0). We never use
    // interrupt pages, so leave it zero for legacy versions.
    let interrupt_page_or_target_info = if version >= Version::Win10Rs3_1 {
        u64::from(
            TargetInfo::new()
                .with_sint(VMBUS_SINT)
                .with_vtl(0)
                .with_feature_flags(feature_flags.into_bits()),
        )
    } else {
        0
    };

    let base = InitiateContact {
        version_requested: version.raw(),
        target_message_vp: 0,
        interrupt_page_or_target_info,
        parent_to_child_monitor_page_gpa: 0,
        child_to_parent_monitor_page_gpa: 0,
    };

    let mut buf: Vec<u8>;
    match client_id {
        Some(id) => {
            let msg = InitiateContact2 {
                initiate_contact: base,
                client_id: id,
            };
            buf = Vec::with_capacity(HEADER_SIZE + size_of::<InitiateContact2>());
            buf.extend_from_slice(MessageHeader::new(MessageType::INITIATE_CONTACT).as_bytes());
            buf.extend_from_slice(msg.as_bytes());
        }
        None => {
            buf = Vec::with_capacity(HEADER_SIZE + size_of::<InitiateContact>());
            buf.extend_from_slice(MessageHeader::new(MessageType::INITIATE_CONTACT).as_bytes());
            buf.extend_from_slice(base.as_bytes());
        }
    }
    buf
}

/// Parsed `VersionResponse` broken down into its interesting fields.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ParsedVersionResponse {
    /// Whether the host accepted the requested version.
    pub version_supported: bool,
    /// New post-message connection id (≥ 5.0) or the selected
    /// pre-5.0 version. Only meaningful when `version_supported`.
    pub selected_version_or_connection_id: u32,
    /// Server-advertised feature flags (Copper+).
    pub supported_features: FeatureFlags,
    /// Server-provided monitor pages (Copper+ with
    /// `SERVER_SPECIFIED_MONITOR_PAGES`).
    pub parent_to_child_monitor_page_gpa: u64,
    /// Server-provided monitor pages.
    pub child_to_parent_monitor_page_gpa: u64,
}

/// Parse a raw `VersionResponse` message body into
/// [`ParsedVersionResponse`], picking `VersionResponse2` /
/// `VersionResponse3` layout based on the byte length.
///
/// `bytes` is expected to start at the vmbus `MessageHeader`.
pub fn parse_version_response(bytes: &[u8]) -> Result<ParsedVersionResponse> {
    let base: VersionResponse = crate::message::parse(bytes)?;
    let body_len = bytes.len() - HEADER_SIZE;

    let (supported_features, p2c, c2p) = if body_len >= size_of::<VersionResponse3>() {
        let v3: VersionResponse3 = crate::message::parse(bytes)?;
        (
            FeatureFlags::from_bits(v3.version_response2.supported_features),
            v3.parent_to_child_monitor_page_gpa,
            v3.child_to_parent_monitor_page_gpa,
        )
    } else if body_len >= size_of::<VersionResponse2>() {
        let v2: VersionResponse2 = crate::message::parse(bytes)?;
        (FeatureFlags::from_bits(v2.supported_features), 0, 0)
    } else {
        (FeatureFlags::new(), 0, 0)
    };

    Ok(ParsedVersionResponse {
        version_supported: base.version_supported != 0,
        selected_version_or_connection_id: base.selected_version_or_connection_id,
        supported_features,
        parent_to_child_monitor_page_gpa: p2c,
        child_to_parent_monitor_page_gpa: c2p,
    })
}

/// Convert a [`ParsedVersionResponse`] into a [`ConnectionState`] given
/// the version that produced it.
pub fn build_connection_state(version: Version, parsed: ParsedVersionResponse) -> ConnectionState {
    let post_message_connection_id = if version >= Version::Win10Rs3_1 {
        // On ≥ 5.0 the host returns the connection id to use.
        parsed.selected_version_or_connection_id
    } else {
        VMBUS_CONNECTION_ID_LEGACY
    };
    ConnectionState {
        selected_version: version,
        post_message_connection_id,
        feature_flags: parsed.supported_features,
        parent_to_child_monitor_page_gpa: parsed.parent_to_child_monitor_page_gpa,
        child_to_parent_monitor_page_gpa: parsed.child_to_parent_monitor_page_gpa,
    }
}

/// Trait describing how the negotiation state machine drains the
/// message page.
///
/// The state machine posts a request and then repeatedly calls
/// [`MessagePump::poll_until`] until the supplied completion handle is
/// filled or the pump gives up (via [`Error::Timeout`]).
///
/// The `sink` is fed every non-completion message the pump encounters
/// while draining — this is how `OfferChannel` messages reach the
/// caller during [`request_offers_with`].
pub trait MessagePump {
    /// Poll the message page (or equivalent) until `handle.completed()`
    /// returns `true`, or return [`Error::Timeout`]. Non-completion
    /// messages seen during the drain are forwarded to `sink`.
    ///
    /// `ctx` is passed through so the pump can issue further
    /// hypercalls (notably `HvCallSetVpRegisters` to write EOM).
    fn poll_until<C: HypercallTrait>(
        &mut self,
        ctx: &mut C,
        handle: &CompletionHandle,
        sink: &mut dyn MessageSink,
    ) -> Result<()>;
}

/// Walk `ladder` in order, posting `InitiateContact` for each version
/// and waiting for a `VersionResponse`. Returns the first
/// [`ConnectionState`] the host accepts.
///
/// * `ctx` — carries the hypercall interface.
/// * `table` — completion table used to receive the response.
/// * `pump` — drains the message page (see [`MessagePump`]).
/// * `client_id` — advertised to the host as a Dilithium `CLIENT_ID`.
///   Passed only for Copper (6.0+); older versions still send the
///   legacy [`InitiateContact`] struct.
///
/// Any offers that race negotiation are forwarded to a nested
/// [`OfferCollector`] and discarded (they belong to `RequestOffers`,
/// not `InitiateContact`).
pub fn negotiate_version<C, P>(
    ctx: &mut C,
    table: &CompletionTable,
    pump: &mut P,
    client_id: Guid,
    ladder: &[Version],
) -> Result<ConnectionState>
where
    C: HypercallTrait,
    P: MessagePump,
{
    let advertised_flags = FeatureFlags::supported();
    let mut discard = OfferCollector::default();
    for &version in ladder {
        let handle = table.register(CompletionKey::VersionResponse);
        let client = if version >= Version::Copper {
            Some(client_id)
        } else {
            None
        };
        let payload = encode_initiate_contact(version, client, advertised_flags);
        crate::hypercalls::post_message(ctx, initial_connection_id(version), &payload)?;

        pump.poll_until(ctx, &handle, &mut discard)?;
        let bytes = handle.take_response().ok_or(Error::Timeout)?;
        let parsed = parse_version_response(&bytes)?;
        if parsed.version_supported {
            return Ok(build_connection_state(version, parsed));
        }
    }
    Err(Error::VersionMismatch)
}

/// Sink used during offer enumeration: collect `OfferChannel`s and
/// forward rescinds to a small side buffer.
#[derive(Default)]
pub struct OfferCollector {
    /// Offers received so far.
    pub offers: Vec<OfferChannel>,
    /// Rescinds received while enumerating (rare but possible if the
    /// host churns channels).
    pub rescinds: Vec<RescindChannelOffer>,
}

impl MessageSink for OfferCollector {
    fn offer(&mut self, offer: &OfferChannel) {
        self.offers.push(*offer);
    }
    fn rescind(&mut self, rescind: &RescindChannelOffer) {
        self.rescinds.push(*rescind);
    }
    fn tl_connect_result(&mut self, _result: &TlConnectResult) {}
}

/// Post `RequestOffers` and drain the message page until
/// `AllOffersDelivered` arrives, forwarding every `OfferChannel` (and
/// any race-time `RescindChannelOffer`) to `sink`.
pub fn request_offers_with<C, P>(
    ctx: &mut C,
    table: &CompletionTable,
    pump: &mut P,
    sink: &mut OfferCollector,
    connection_id: u32,
) -> Result<()>
where
    C: HypercallTrait,
    P: MessagePump,
{
    let handle = table.register(CompletionKey::AllOffersDelivered);

    let mut buf = [0u8; MAX_MESSAGE_SIZE];
    let used = crate::message::encode(&crate::protocol::RequestOffers, &mut buf);
    crate::hypercalls::post_message(ctx, connection_id, &buf[..used])?;

    pump.poll_until(ctx, &handle, sink)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// UEFI entry points
// ---------------------------------------------------------------------------

/// A default client id advertised by the guest ("opentmk-invariant").
/// The exact GUID is arbitrary but stable — the host doesn't interpret
/// it beyond echoing it back in diagnostics.
pub const CLIENT_ID: Guid = Guid {
    data1: 0x6f_70_74_6d, // 'o''p''t''m'
    data2: 0x6b_5f,       // 'k''_'
    data3: 0x69_6e,       // 'i''n'
    data4: *b"variant\0",
};

/// UEFI entry point: full negotiation using a SIMP-page-backed pump.
///
/// Requires [`crate::synic::init_synic`] to have already programmed
/// the SIMP page. Uses [`crate::message::completion_table`] and a
/// [`crate::interrupt::SimpPump`] internally.
pub fn initiate<C: HypercallTrait>(ctx: &mut C) -> Result<()> {
    let pages = crate::synic::synic_pages().ok_or(Error::VersionMismatch)?;
    let table = crate::message::completion_table();
    let mut pump = crate::interrupt::SimpPump::new(pages.simp_gpa);
    let state = negotiate_version(
        ctx,
        table,
        &mut pump,
        CLIENT_ID,
        Version::NEGOTIATION_LADDER,
    )?;
    *CONNECTION.lock() = Some(state);
    Ok(())
}

/// UEFI entry point: post `RequestOffers`, drain until
/// `AllOffersDelivered`, and collect offers.
pub fn request_offers<C: HypercallTrait>(ctx: &mut C) -> Result<Vec<OfferChannel>> {
    let pages = crate::synic::synic_pages().ok_or(Error::VersionMismatch)?;
    let state = connection().clone().ok_or(Error::VersionMismatch)?;
    let table = crate::message::completion_table();
    let mut pump = crate::interrupt::SimpPump::new(pages.simp_gpa);
    let mut sink = OfferCollector::default();
    request_offers_with(
        ctx,
        table,
        &mut pump,
        &mut sink,
        state.post_message_connection_id,
    )?;
    Ok(sink.offers)
}

/// Send `Unload` and wait for `UnloadComplete`.
///
/// Skeleton for the shutdown path — real polling lives with
/// [`crate::interrupt`].
pub fn unload_with<C, P>(
    ctx: &mut C,
    table: &CompletionTable,
    pump: &mut P,
    sink: &mut OfferCollector,
) -> Result<()>
where
    C: HypercallTrait,
    P: MessagePump,
{
    let state = connection().clone().ok_or(Error::VersionMismatch)?;
    let handle = table.register(CompletionKey::UnloadComplete);

    let mut buf = [0u8; MAX_MESSAGE_SIZE];
    let used = crate::message::encode(&Unload, &mut buf);
    crate::hypercalls::post_message(ctx, state.post_message_connection_id, &buf[..used])?;

    pump.poll_until(ctx, &handle, sink)?;
    *CONNECTION.lock() = None;
    Ok(())
}

/// UEFI entry point for `Unload`.
pub fn unload<C: HypercallTrait>(ctx: &mut C) -> Result<()> {
    let pages = crate::synic::synic_pages().ok_or(Error::VersionMismatch)?;
    let table = crate::message::completion_table();
    let mut pump = crate::interrupt::SimpPump::new(pages.simp_gpa);
    let mut sink = OfferCollector::default();
    unload_with(ctx, table, &mut pump, &mut sink)
}

// Route a single message via [`route_message`], for callers that want
// to re-use the same routing behaviour from a custom pump. This is a
// thin re-export to keep [`crate::message`] private-ish for the guest
// pump implementation to consume without a second import site.
pub use crate::message::route_message as route;
