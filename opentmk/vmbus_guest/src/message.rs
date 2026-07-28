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
//!   `(msg_type, extra_key)`.

use crate::Error;
use crate::Result;
use crate::protocol::HEADER_SIZE;
use crate::protocol::MAX_MESSAGE_SIZE;
use crate::protocol::MessageHeader;
use crate::protocol::MessageType;
use crate::protocol::VmbusMessage;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

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
