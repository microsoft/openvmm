// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Host-testable VMBus wire encode and decode helpers.

use crate::Error;
use crate::Result;
use vmbus_core::protocol::HEADER_SIZE;
use vmbus_core::protocol::MAX_MESSAGE_SIZE;
use vmbus_core::protocol::MessageHeader;
use vmbus_core::protocol::MessageType;
use vmbus_core::protocol::VmbusMessage;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

/// Encode a message, including its VMBus header.
pub fn encode<M: VmbusMessage + IntoBytes + Immutable>(
    msg: &M,
    out: &mut [u8; MAX_MESSAGE_SIZE],
) -> usize {
    assert!(M::MESSAGE_SIZE <= MAX_MESSAGE_SIZE);
    let header = MessageHeader::new(M::MESSAGE_TYPE);
    out.fill(0);
    out[..HEADER_SIZE].copy_from_slice(header.as_bytes());
    out[HEADER_SIZE..M::MESSAGE_SIZE].copy_from_slice(msg.as_bytes());
    M::MESSAGE_SIZE
}

/// Read a message's type.
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

/// Parse a complete message of the requested type.
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
    let (message, _) = M::read_from_prefix(&bytes[HEADER_SIZE..]).map_err(|_| Error::Parse {
        ty: Some(ty),
        reason: "message body cast failed",
    })?;
    Ok(message)
}
