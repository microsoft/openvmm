// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TTRPC message handling.

use crate::service::Status;
use anyhow::Context;
use futures::AsyncRead;
use futures::AsyncReadExt;
use futures::AsyncWrite;
use futures::AsyncWriteExt;
use mesh::payload::Protobuf;
use std::io::ErrorKind;
use thiserror::Error;
use zerocopy::BigEndian;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::U32;

/// The wire format header for a message.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
struct MessageHeader {
    length: U32<BigEndian>,
    stream_id: U32<BigEndian>,
    message_type: u8,
    flags: u8,
}

pub const MESSAGE_TYPE_REQUEST: u8 = 1;
pub const MESSAGE_TYPE_RESPONSE: u8 = 2;

/// The maximum ttrpc message size.
///
/// The spec specifies 4MB as the maximum, but it's not quite large enough for
/// our use cases.
///
/// This only affects the receiving side. The reference implementation only
/// enforces this on the receiving side, so receivers already have to cope with
/// messages that are too large (by rejecting them).
///
/// So allow a larger size here, which should be a compatible relaxation of the
/// spec.
///
/// Note however, that 16MB - 1 is a hard maximum, because the spec specifies
/// that the top byte may be reused for something else in the future. (I am
/// still skeptical that this is possible because existing senders do not
/// validate this at all. But let's not take a dependency on messages bigger
/// than this.)
const MAX_MESSAGE_SIZE: usize = 0xffffff;

#[derive(Debug, Error)]
#[error("message length {0} exceeds maximum allowed size {MAX_MESSAGE_SIZE}")]
pub struct TooLongError(usize);

pub struct ReadResult {
    pub stream_id: u32,
    pub message_type: u8,
    pub payload: Result<Vec<u8>, TooLongError>,
}

pub async fn read_message(
    reader: &mut (impl AsyncRead + Unpin),
) -> std::io::Result<Option<ReadResult>> {
    let mut header = MessageHeader::new_zeroed();
    match reader.read_exact(header.as_mut_bytes()).await {
        Ok(_) => (),
        Err(err) if err.kind() == ErrorKind::UnexpectedEof => {
            return Ok(None);
        }
        Err(err) => return Err(err),
    }

    let stream_id = header.stream_id.get();
    let length = header.length.get() as usize;
    let payload = if length <= MAX_MESSAGE_SIZE {
        let mut buf = vec![0; length];
        reader.read_exact(&mut buf).await?;
        Ok(buf)
    } else {
        // Discard the message that was too long.
        futures::io::copy(reader.take(length as u64), &mut futures::io::sink()).await?;
        Err(TooLongError(length))
    };

    Ok(Some(ReadResult {
        stream_id,
        message_type: header.message_type,
        payload,
    }))
}

pub async fn write_message(
    writer: &mut (impl AsyncWrite + Unpin),
    stream_id: u32,
    message_type: u8,
    payload: &[u8],
) -> anyhow::Result<()> {
    let header = MessageHeader {
        stream_id: stream_id.into(),
        message_type,
        length: (payload.len() as u32).into(),
        flags: 0,
    };

    writer
        .write_all(header.as_bytes())
        .await
        .context("failed writing message header")?;

    writer
        .write_all(payload)
        .await
        .context("failed writing message payload")?;

    Ok(())
}

/// A request message payload.
#[derive(Protobuf)]
pub struct Request {
    pub service: String,
    pub method: String,
    pub payload: Vec<u8>,
    pub timeout_nano: u64,
    pub metadata: Vec<(String, String)>,
}

/// A response message payload.
#[derive(Protobuf)]
pub enum Response {
    #[mesh(1, transparent)]
    Status(Status),
    #[mesh(2, transparent)]
    Payload(Vec<u8>),
}
