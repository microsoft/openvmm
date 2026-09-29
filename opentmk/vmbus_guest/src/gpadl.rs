// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! GPADL (Guest Physical Address Descriptor List) create / teardown.
//!
//! A GPADL is how the guest hands the host a page-aligned shared-memory
//! region (typically a ring buffer). We send:
//!
//! 1. A [`GpadlHeader`] message describing `channel_id`, `gpadl_id`,
//!    total range-payload byte-length, and the range count.
//! 2. Range payload bytes — the first ~26 u64 slots fit in the header
//!    message; anything more spills into one or more [`GpadlBody`]
//!    messages carrying the same `gpadl_id`.
//! 3. The host acknowledges with `GpadlCreated`, matched by
//!    `gpadl_id`.
//!
//! For a typical single-buffer GPADL the range payload is:
//!
//! ```text
//! [GpaRange { len, offset }] [pfn0] [pfn1] ... [pfnN]
//! ```
//!
//! where `len = total bytes`, `offset = 0`, and `pfnI = gpa_of_page_I >> 12`.
//!
//! The wire encoder splits large PFN lists across a
//! `GpadlHeader` + N `GpadlBody` messages — the header carries
//! `HEADER_RANGE_CAPACITY_BYTES` of payload; each body carries up
//! to `BODY_RANGE_CAPACITY_BYTES`. All messages share the same
//! `gpadl_id`. The host acknowledges the whole batch with a single
//! `GpadlCreated` matched by `gpadl_id`.
//!
//! # Example (register a 16 MiB receive buffer)
//!
//! ```ignore
//! use vmbus_guest::gpadl;
//!
//! let mut pfns = Vec::with_capacity(4096);
//! for i in 0..4096 {
//!     pfns.push(((base_gpa + (i * 4096) as u64) >> 12));
//! }
//! let handle = gpadl::establish_gpadl(&mut ctx, channel_id, 16 * 1024 * 1024, &pfns)?;
//! // handle.id() is the GpadlId the host will echo in subsequent
//! // messages that reference this buffer (e.g. netvsp's
//! // SendReceiveBuffer).
//!
//! // ...use the buffer...
//!
//! gpadl::teardown_gpadl(&mut ctx, channel_id, handle)?;
//! # Ok::<_, vmbus_guest::Error>(())
//! ```
//!
//! The encoder handles arbitrarily large PFN lists — netvsp
//! establishes a 4096-PFN GPADL via ~147 back-to-back messages,
//! which `establish_gpadl` posts as a batch before polling for the
//! completion.

use crate::Error;
use crate::Result;
use crate::client_driver::ClientDriver;
use crate::client_driver::MessagePump;
use crate::client_driver::driver;
use crate::interrupt::SimpPump;
use crate::protocol::GpaRange;
use crate::synic::synic_pages;
use alloc::vec::Vec;
use core::cmp::min;
use core::mem::size_of;
use core::mem::size_of_val;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering;
use opentmk_core::context::HypercallPlatformTrait;
use opentmk_core::platform::hyperv::ctx::HyperVHypercallConfig;
use vmbus_core::protocol::ChannelId;
use vmbus_core::protocol::GpadlBody;
use vmbus_core::protocol::GpadlHeader;
use vmbus_core::protocol::GpadlId;
use vmbus_core::protocol::HEADER_SIZE;
use vmbus_core::protocol::MAX_MESSAGE_SIZE;
use vmbus_core::protocol::MessageHeader;
use vmbus_core::protocol::MessageType;
use zerocopy::IntoBytes;

/// Bytes of range payload that fit inside a single [`GpadlHeader`]
/// message (after the `MessageHeader` and `GpadlHeader` fixed fields),
/// rounded down to a multiple of 8 so we never split a u64 slot.
pub const HEADER_RANGE_CAPACITY_BYTES: usize =
    (MAX_MESSAGE_SIZE - HEADER_SIZE - size_of::<GpadlHeader>()) & !7;

/// Bytes of range payload that fit inside a single [`GpadlBody`]
/// continuation message, rounded down to a multiple of 8.
pub const BODY_RANGE_CAPACITY_BYTES: usize =
    (MAX_MESSAGE_SIZE - HEADER_SIZE - size_of::<GpadlBody>()) & !7;

/// Handle returned by [`establish_gpadl`] and consumed by
/// [`teardown_gpadl`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GpadlHandle {
    pub(crate) channel_id: ChannelId,
    pub(crate) gpadl_id: GpadlId,
}

impl GpadlHandle {
    /// Return the underlying [`GpadlId`].
    pub fn id(&self) -> GpadlId {
        self.gpadl_id
    }

    /// Return the underlying [`ChannelId`].
    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }
}

/// Number of [`GpadlBody`] continuation messages required to carry
/// `range_payload_bytes` of range payload after the initial
/// [`GpadlHeader`] message.
pub fn body_count_for_bytes(range_payload_bytes: usize) -> usize {
    if range_payload_bytes <= HEADER_RANGE_CAPACITY_BYTES {
        0
    } else {
        (range_payload_bytes - HEADER_RANGE_CAPACITY_BYTES).div_ceil(BODY_RANGE_CAPACITY_BYTES)
    }
}

/// Build the range payload for a single contiguous page-aligned buffer
/// of `total_bytes` covered by `pfns`.
///
/// Layout:
///
/// ```text
/// [GpaRange { len: total_bytes, offset: 0 }] [pfn0] [pfn1] ... [pfnN-1]
/// ```
///
/// Returns a `Vec<u8>` of size `8 + pfns.len() * 8`.
pub fn build_single_range_payload(total_bytes: u32, pfns: &[u64]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(size_of::<GpaRange>() + size_of_val(pfns));
    let range = GpaRange {
        byte_count: total_bytes,
        byte_offset: 0,
    };
    buf.extend_from_slice(range.as_bytes());
    for pfn in pfns {
        buf.extend_from_slice(pfn.as_bytes());
    }
    buf
}

/// Encoded GPADL exchange: the header message plus zero or more body
/// messages, ready to be posted with [`crate::hypercalls::post_message`].
pub struct GpadlMessages {
    /// One entry per vmbus message; each is a pre-encoded byte buffer
    /// starting with a `MessageHeader`. Length is at most
    /// `MAX_MESSAGE_SIZE`.
    pub messages: Vec<Vec<u8>>,
}

impl GpadlMessages {
    /// Number of messages that will be posted.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Whether the exchange is empty (never a valid GPADL — retained
    /// for `clippy::len_without_is_empty`).
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

/// Encode the `GpadlHeader` + `GpadlBody` chain for a GPADL carrying a
/// single contiguous range payload.
///
/// The `range_payload` slice is exactly the bytes returned by
/// [`build_single_range_payload`] (or equivalent for multi-range).
///
/// The wire encoding for each message is:
///
/// * `messages[0]` = [`MessageHeader`] {`GPADL_HEADER`} + [`GpadlHeader`]
///   + first `HEADER_RANGE_CAPACITY_BYTES` (or fewer) of `range_payload`.
/// * `messages[i>0]` = [`MessageHeader`] {`GPADL_BODY`} + [`GpadlBody`]
///   {`gpadl_id`} + next `BODY_RANGE_CAPACITY_BYTES` (or fewer) of
///   `range_payload`.
pub fn encode_gpadl_messages(
    channel_id: ChannelId,
    gpadl_id: GpadlId,
    range_count: u16,
    range_payload: &[u8],
) -> GpadlMessages {
    let total_len_bytes: u16 = range_payload
        .len()
        .try_into()
        .expect("GPADL range payload exceeds u16 length");

    let mut messages: Vec<Vec<u8>> = Vec::new();

    // Header message.
    let header_take = min(range_payload.len(), HEADER_RANGE_CAPACITY_BYTES);
    {
        let header = GpadlHeader {
            channel_id,
            gpadl_id,
            len: total_len_bytes,
            count: range_count,
        };
        let mut msg = Vec::with_capacity(HEADER_SIZE + size_of::<GpadlHeader>() + header_take);
        msg.extend_from_slice(MessageHeader::new(MessageType::GPADL_HEADER).as_bytes());
        msg.extend_from_slice(header.as_bytes());
        msg.extend_from_slice(&range_payload[..header_take]);
        messages.push(msg);
    }

    // Body messages.
    let mut cursor = header_take;
    while cursor < range_payload.len() {
        let take = min(range_payload.len() - cursor, BODY_RANGE_CAPACITY_BYTES);
        let body = GpadlBody { rsvd: 0, gpadl_id };
        let mut msg = Vec::with_capacity(HEADER_SIZE + size_of::<GpadlBody>() + take);
        msg.extend_from_slice(MessageHeader::new(MessageType::GPADL_BODY).as_bytes());
        msg.extend_from_slice(body.as_bytes());
        msg.extend_from_slice(&range_payload[cursor..cursor + take]);
        messages.push(msg);
        cursor += take;
    }

    GpadlMessages { messages }
}

/// Full `establish_gpadl` — posts the header/body chain and waits for
/// `GpadlCreated`.
///
/// This is the pump-based variant that host tests can drive with a
/// scripted `MessagePump`. The UEFI entry point [`establish_gpadl`]
/// wraps it with the process-wide driver and SIMP pump.
pub fn establish_gpadl_with<C, P>(
    ctx: &mut C,
    driver: &mut ClientDriver,
    pump: &mut P,
    channel_id: ChannelId,
    gpadl_id: GpadlId,
    total_bytes: u32,
    pfns: &[u64],
) -> Result<GpadlHandle>
where
    C: HypercallPlatformTrait<Config = HyperVHypercallConfig>,
    P: MessagePump,
{
    if !(total_bytes as u64).is_multiple_of(hvdef::HV_PAGE_SIZE) {
        return Err(Error::Parse {
            ty: None,
            reason: "GPADL total_bytes must be page-aligned",
        });
    }
    let expected_pfns = (total_bytes as u64 / hvdef::HV_PAGE_SIZE) as usize;
    if pfns.len() != expected_pfns {
        return Err(Error::Parse {
            ty: None,
            reason: "GPADL pfn count doesn't match total_bytes",
        });
    }

    let mut buf = Vec::with_capacity(1 + pfns.len());
    buf.push(total_bytes as u64);
    buf.extend_from_slice(pfns);
    let request_id = driver.request_id();
    driver.step(
        ctx,
        vmbus_client_core::Event::EstablishGpadl {
            request_id,
            channel_id,
            gpadl_id,
            request: vmbus_client_core::GpadlRequest {
                id: gpadl_id,
                count: 1,
                buf,
            },
        },
    )?;
    match driver.wait_for(ctx, pump, request_id)? {
        vmbus_client_core::CompletionResult::EstablishGpadl(Ok(())) => Ok(GpadlHandle {
            channel_id,
            gpadl_id,
        }),
        vmbus_client_core::CompletionResult::EstablishGpadl(Err(())) => Err(Error::GpadlFailed),
        _ => Err(Error::UnexpectedCompletion),
    }
}

/// Post `GpadlTeardown` for `handle` and wait for `GpadlTorndown`.
pub fn teardown_gpadl_with<C, P>(
    ctx: &mut C,
    driver: &mut ClientDriver,
    pump: &mut P,
    handle: GpadlHandle,
) -> Result<()>
where
    C: HypercallPlatformTrait<Config = HyperVHypercallConfig>,
    P: MessagePump,
{
    let request_id = driver.request_id();
    driver.step(
        ctx,
        vmbus_client_core::Event::TeardownGpadl {
            request_id,
            channel_id: handle.channel_id,
            gpadl_id: handle.gpadl_id,
        },
    )?;
    match driver.wait_for(ctx, pump, request_id)? {
        vmbus_client_core::CompletionResult::TeardownGpadl => Ok(()),
        _ => Err(Error::UnexpectedCompletion),
    }
}

/// UEFI entry point: [`establish_gpadl_with`] using the process-wide
/// client driver and SIMP pump.
pub fn establish_gpadl<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
    channel_id: ChannelId,
    total_bytes: u32,
    pfns: &[u64],
) -> Result<GpadlHandle> {
    let pages = synic_pages().ok_or(Error::VersionMismatch)?;
    let mut pump = SimpPump::new(pages.simp_gpa);
    let gpadl_id = allocate_gpadl_id();
    establish_gpadl_with(
        ctx,
        &mut driver(),
        &mut pump,
        channel_id,
        gpadl_id,
        total_bytes,
        pfns,
    )
}

/// UEFI entry point: [`teardown_gpadl_with`] using the process-wide
/// client driver and SIMP pump.
pub fn teardown_gpadl<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
    handle: GpadlHandle,
) -> Result<()> {
    let pages = synic_pages().ok_or(Error::VersionMismatch)?;
    let mut pump = SimpPump::new(pages.simp_gpa);
    teardown_gpadl_with(ctx, &mut driver(), &mut pump, handle)
}

/// Allocate a fresh `GpadlId`. Uses a process-wide atomic counter,
/// starting at 1 so a zero id can be used as a sentinel.
pub fn allocate_gpadl_id() -> GpadlId {
    static NEXT_ID: AtomicU32 = AtomicU32::new(1);
    GpadlId(NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

// -----------------------------------------------------------------------
// Encoder helpers used by [`crate::message::parse`] / decoders. Small
// message-body decoders for the completion side (`GpadlCreated`,
// `GpadlTorndown`) live in [`crate::message`]; nothing to add here yet.
// -----------------------------------------------------------------------

/// Compute how many `GpadlBody` messages are needed to describe `pages`
/// PFNs for a single contiguous range after the initial header message.
///
/// Retained for API compatibility with the earlier scaffold. Prefer
/// [`body_count_for_bytes`] for new code.
pub fn body_count_for(pages: usize) -> usize {
    // A single-range payload is 8 bytes (GpaRange) + pages * 8.
    body_count_for_bytes(size_of::<GpaRange>() + pages * size_of::<u64>())
}
