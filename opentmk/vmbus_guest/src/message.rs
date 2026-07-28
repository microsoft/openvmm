// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-message-type encode / decode helpers and the outbound
//! request-completion table.
//!
//! Every vmbus message on the wire is a `MessageHeader` followed by a
//! payload. On the guest side we:
//! * Encode outbound messages into a `[u8; HV_MESSAGE_PAYLOAD_SIZE]` and
//!   post them via [`crate::hypercalls::post_message`].
//! * Decode inbound messages from the SIMP message page by matching on
//!   `MessageHeader::message_type()`.
//! * Route completions to the pending request that owns them, matched by
//!   `(msg_type, key)` — see [`CompletionKey`].

use crate::Error;
use crate::Result;
use crate::protocol::ChannelId;
use crate::protocol::GpadlId;
use crate::protocol::HEADER_SIZE;
use crate::protocol::MAX_MESSAGE_SIZE;
use crate::protocol::MessageHeader;
use crate::protocol::MessageType;
use crate::protocol::VmbusMessage;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::sync::Weak;
use alloc::vec::Vec;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::Ordering;
use spin::Mutex;
use spin::Once;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

/// Process-wide completion table, lazily created on first access.
///
/// The pump-based APIs in [`crate::connection`] and [`crate::channel`]
/// route completions through this table; callers that want an
/// isolated instance can build their own [`CompletionTable`] and use
/// the `_with` variants instead.
static COMPLETION_TABLE: Once<CompletionTable> = Once::new();

/// Return a reference to the process-wide completion table.
pub fn completion_table() -> &'static CompletionTable {
    COMPLETION_TABLE.call_once(CompletionTable::new)
}

/// Encode `msg` into a `HV_MESSAGE_PAYLOAD_SIZE`-sized buffer along with
/// its `MessageHeader`. Returns the number of used bytes.
pub fn encode<M: VmbusMessage>(msg: &M, out: &mut [u8; MAX_MESSAGE_SIZE]) -> usize {
    assert!(M::MESSAGE_SIZE <= MAX_MESSAGE_SIZE);
    let header = MessageHeader::new(M::MESSAGE_TYPE);
    out.fill(0);
    out[..HEADER_SIZE].copy_from_slice(header.as_bytes());
    out[HEADER_SIZE..M::MESSAGE_SIZE].copy_from_slice(msg.as_bytes());
    M::MESSAGE_SIZE
}

/// Peek at the header of an inbound message; returns the message type or
/// [`Error::Parse`] if the buffer is too small.
pub fn peek_header(bytes: &[u8]) -> Result<MessageType> {
    if bytes.len() < HEADER_SIZE {
        return Err(Error::Parse {
            ty: None,
            reason: "message shorter than header",
        });
    }
    let (header, _) = MessageHeader::ref_from_prefix(bytes).map_err(|_| Error::Parse {
        ty: None,
        reason: "message header cast failed",
    })?;
    Ok(header.message_type())
}

/// Try to parse the body of `bytes` as message type `M`. Fails if the
/// buffer isn't long enough or if the header type doesn't match `M`.
pub fn parse<M: VmbusMessage + FromBytes>(bytes: &[u8]) -> Result<M> {
    let ty = peek_header(bytes)?;
    if ty != M::MESSAGE_TYPE {
        return Err(Error::UnexpectedMessage(ty));
    }
    if bytes.len() < M::MESSAGE_SIZE {
        return Err(Error::Parse {
            ty: Some(ty),
            reason: "message body truncated",
        });
    }
    let (msg, _) = M::read_from_prefix(&bytes[HEADER_SIZE..]).map_err(|_| Error::Parse {
        ty: Some(ty),
        reason: "message body cast failed",
    })?;
    Ok(msg)
}

// ---------------------------------------------------------------------------
// Completion table
// ---------------------------------------------------------------------------

/// Discriminator used to route host completions back to the outbound
/// request that started them.
///
/// The key mirrors the disambiguation the host performs when it emits
/// each response: `VersionResponse`, `AllOffersDelivered` and
/// `UnloadComplete` are singletons (at most one such request is in
/// flight at any time by design), everything else keys on the unique
/// id the guest allocated for the request.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum CompletionKey {
    /// Completion for the current `InitiateContact`. Singleton.
    VersionResponse,
    /// Terminator for `RequestOffers`. Singleton.
    AllOffersDelivered,
    /// Completion for `Unload`. Singleton.
    UnloadComplete,
    /// Completion for `OpenChannel[/2]`, matched by `open_id`.
    OpenChannelResult(u32),
    /// Completion for `GpadlHeader`, matched by `gpadl_id`.
    GpadlCreated(GpadlId),
    /// Completion for `GpadlTeardown`, matched by `gpadl_id`.
    GpadlTorndown(GpadlId),
    /// Completion for `ModifyChannel`, matched by `channel_id`.
    ModifyChannelResponse(ChannelId),
    /// Result of a `TlConnectRequest[/2]`, keyed by `endpoint_id`
    /// packed into a `u128` (the exact packing is stable but arbitrary
    /// — it's only used for map ordering).
    TlConnectResult(u128),
}

/// Slot holding a single pending completion.
#[derive(Debug)]
struct CompletionSlot {
    completed: AtomicBool,
    /// The raw wire bytes of the completion message (starting at the
    /// `MessageHeader`). Filled once, taken once.
    response: Mutex<Option<Vec<u8>>>,
}

impl CompletionSlot {
    fn new() -> Self {
        Self {
            completed: AtomicBool::new(false),
            response: Mutex::new(None),
        }
    }
}

/// Handle held by a caller waiting on a specific completion.
///
/// Cloneable so multiple observers may wait; the response bytes may be
/// taken by exactly one of them.
#[derive(Clone, Debug)]
pub struct CompletionHandle {
    key: CompletionKey,
    slot: Arc<CompletionSlot>,
    table: Arc<CompletionTableInner>,
}

impl CompletionHandle {
    /// Whether the host has delivered the completion.
    pub fn completed(&self) -> bool {
        self.slot.completed.load(Ordering::Acquire)
    }

    /// Take the completion response bytes, leaving `None` behind.
    /// Returns `None` if the completion has not arrived or has already
    /// been taken.
    pub fn take_response(&self) -> Option<Vec<u8>> {
        if !self.completed() {
            return None;
        }
        self.slot.response.lock().take()
    }

    /// Return the key this handle was registered for.
    pub fn key(&self) -> CompletionKey {
        self.key
    }
}

impl Drop for CompletionHandle {
    fn drop(&mut self) {
        // The table only holds a `Weak`, so dropping this handle
        // implicitly makes the slot unreachable (Weak::upgrade returns
        // None) — but we still want to prune the map entry so
        // `pending()` reflects reality. Remove only when this is the
        // final strong reference to the slot.
        if Arc::strong_count(&self.slot) == 1 {
            let _ = self.table.entries.lock().remove(&self.key);
        }
    }
}

/// Guts of [`CompletionTable`] — kept behind an [`Arc`] so
/// [`CompletionHandle::drop`] can reach the entries map without holding
/// a reference to the table itself.
#[derive(Debug)]
struct CompletionTableInner {
    entries: Mutex<BTreeMap<CompletionKey, Weak<CompletionSlot>>>,
}

/// Registry mapping in-flight requests to their pending completions.
///
/// Cheap to clone (`Arc` inside).
#[derive(Clone, Debug)]
pub struct CompletionTable {
    inner: Arc<CompletionTableInner>,
}

impl Default for CompletionTable {
    fn default() -> Self {
        Self::new()
    }
}

impl CompletionTable {
    /// Create an empty table.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(CompletionTableInner {
                entries: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// Register that a completion is expected for `key`.
    ///
    /// Returns a [`CompletionHandle`] the caller polls / passes to a
    /// waiter until [`CompletionHandle::completed`] returns `true`.
    ///
    /// Duplicate keys aren't rejected — the newer registration wins.
    /// The caller is responsible for ensuring only one request per key
    /// is in flight at any time.
    pub fn register(&self, key: CompletionKey) -> CompletionHandle {
        let slot = Arc::new(CompletionSlot::new());
        self.inner.entries.lock().insert(key, Arc::downgrade(&slot));
        CompletionHandle {
            key,
            slot,
            table: self.inner.clone(),
        }
    }

    /// Deliver the completion for `key` by handing off `response` bytes.
    ///
    /// Returns [`Error::OrphanCompletion`] if there is no matching
    /// pending registration (host sent a completion we didn't ask for,
    /// or the last waiter dropped out before it arrived).
    pub fn deliver(&self, key: CompletionKey, response: Vec<u8>) -> Result<()> {
        let slot = {
            let entries = self.inner.entries.lock();
            entries.get(&key).and_then(Weak::upgrade)
        };
        let Some(slot) = slot else {
            return Err(Error::OrphanCompletion);
        };
        *slot.response.lock() = Some(response);
        slot.completed.store(true, Ordering::Release);
        Ok(())
    }

    /// Number of live entries (dead `Weak` entries are pruned on read).
    pub fn pending(&self) -> usize {
        let mut entries = self.inner.entries.lock();
        entries.retain(|_, w| w.strong_count() > 0);
        entries.len()
    }
}

/// Given `bytes` starting at a `MessageHeader`, return the
/// [`CompletionKey`] the message satisfies, or `None` if the message
/// isn't a completion type.
///
/// Used by the message dispatcher to route inbound messages to
/// [`CompletionTable::deliver`].
pub fn completion_key_for(bytes: &[u8]) -> Result<Option<CompletionKey>> {
    let ty = peek_header(bytes)?;
    let key = match ty {
        MessageType::VERSION_RESPONSE => Some(CompletionKey::VersionResponse),
        MessageType::ALL_OFFERS_DELIVERED => Some(CompletionKey::AllOffersDelivered),
        MessageType::UNLOAD_COMPLETE => Some(CompletionKey::UnloadComplete),
        MessageType::OPEN_CHANNEL_RESULT => {
            let msg: crate::protocol::OpenResult = parse(bytes)?;
            Some(CompletionKey::OpenChannelResult(msg.open_id))
        }
        MessageType::GPADL_CREATED => {
            let msg: crate::protocol::GpadlCreated = parse(bytes)?;
            Some(CompletionKey::GpadlCreated(msg.gpadl_id))
        }
        MessageType::GPADL_TORNDOWN => {
            let msg: crate::protocol::GpadlTorndown = parse(bytes)?;
            Some(CompletionKey::GpadlTorndown(msg.gpadl_id))
        }
        MessageType::MODIFY_CHANNEL_RESPONSE => {
            let msg: crate::protocol::ModifyChannelResponse = parse(bytes)?;
            Some(CompletionKey::ModifyChannelResponse(msg.channel_id))
        }
        MessageType::TL_CONNECT_RESULT => {
            let msg: crate::protocol::TlConnectResult = parse(bytes)?;
            Some(CompletionKey::TlConnectResult(guid_to_key(
                &msg.endpoint_id,
            )))
        }
        _ => None,
    };
    Ok(key)
}

/// Pack a `Guid` into a `u128` so it can serve as an ordered map key.
fn guid_to_key(g: &crate::protocol::Guid) -> u128 {
    let bytes = g.as_bytes();
    let mut out = [0u8; 16];
    out.copy_from_slice(bytes);
    u128::from_le_bytes(out)
}

/// Non-completion sinks the message dispatcher can invoke.
///
/// Passed to [`route_message`]. Concrete implementations live in
/// [`crate::connection`] (offer collector) and [`crate::channel`]
/// (rescind handling).
pub trait MessageSink {
    /// Called when the host delivers an `OfferChannel`.
    fn offer(&mut self, offer: &crate::protocol::OfferChannel);
    /// Called when the host delivers a `RescindChannelOffer`.
    fn rescind(&mut self, rescind: &crate::protocol::RescindChannelOffer);
    /// Called when the host delivers a `TlConnectResult` (routed as a
    /// completion too — the sink sees a copy).
    fn tl_connect_result(&mut self, _result: &crate::protocol::TlConnectResult) {}
}

/// Dispatch a single message (starting at a `MessageHeader`) to the
/// completion table and/or the sink.
///
/// Routing rules:
///   * completion-type messages call [`CompletionTable::deliver`]. An
///     orphan completion (no matching pending request) is logged and
///     ignored, not fatal — the host may race the guest here.
///   * `OfferChannel` → `sink.offer`.
///   * `RescindChannelOffer` → `sink.rescind`.
///   * everything else is ignored.
pub fn route_message<S: MessageSink + ?Sized>(
    bytes: &[u8],
    table: &CompletionTable,
    sink: &mut S,
) -> Result<()> {
    let ty = peek_header(bytes)?;
    match ty {
        MessageType::OFFER_CHANNEL => {
            let offer: crate::protocol::OfferChannel = parse(bytes)?;
            sink.offer(&offer);
        }
        MessageType::RESCIND_CHANNEL_OFFER => {
            let rescind: crate::protocol::RescindChannelOffer = parse(bytes)?;
            sink.rescind(&rescind);
        }
        _ => {}
    }
    if let Some(key) = completion_key_for(bytes)? {
        if let MessageType::TL_CONNECT_RESULT = ty {
            let result: crate::protocol::TlConnectResult = parse(bytes)?;
            sink.tl_connect_result(&result);
        }
        match table.deliver(key, bytes.to_vec()) {
            Ok(()) | Err(Error::OrphanCompletion) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
