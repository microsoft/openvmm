// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod protocol;

use futures::FutureExt;
use futures::StreamExt;
use guid::Guid;
use inspect::Inspect;
use protocol::HEADER_SIZE;
use protocol::MAX_MESSAGE_SIZE;
use protocol::MessageHeader;
use protocol::VmbusMessage;
use std::future::Future;
use std::str::FromStr;
use std::task::Poll;
use thiserror::Error;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// The standard, non-redirected synthetic interrupt used by VMBus.
pub const VMBUS_SINT: u8 = 2;

#[derive(Debug)]
pub struct TaggedStream<T, S>(Option<T>, S);

impl<T: Clone, S: futures::Stream + Unpin> TaggedStream<T, S> {
    pub fn new(t: T, s: S) -> Self {
        Self(Some(t), s)
    }

    pub fn value(&self) -> Option<&T> {
        self.0.as_ref()
    }
}

impl<T: Clone, S: futures::Stream + Unpin> futures::Stream for TaggedStream<T, S>
where
    Self: Unpin,
{
    type Item = (T, Option<S::Item>);

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(t) = this.0.clone() {
            let v = std::task::ready!(this.1.poll_next_unpin(cx));
            if v.is_none() {
                // Return `None` next time poll_next is called.
                this.0 = None;
            }
            Poll::Ready(Some((t, v)))
        } else {
            Poll::Ready(None)
        }
    }
}

#[derive(Debug)]
pub struct TaggedFuture<T, F>(T, F);

impl<T: Clone, F: Future + Unpin> Future for TaggedFuture<T, F>
where
    Self: Unpin,
{
    type Output = (T, F::Output);

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Self::Output> {
        let r = std::task::ready!(self.1.poll_unpin(cx));
        Poll::Ready((self.0.clone(), r))
    }
}

/// Represents information about a negotiated version.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Inspect)]
pub struct VersionInfo {
    pub version: protocol::Version,
    pub feature_flags: protocol::FeatureFlags,
}

/// Represents a constraint on the version or features allowed.
#[derive(Copy, Clone, Debug)]
pub struct MaxVersionInfo {
    pub version: u32,
    pub feature_flags: protocol::FeatureFlags,
}

impl MaxVersionInfo {
    pub fn new(version: u32) -> Self {
        Self {
            version,
            feature_flags: protocol::FeatureFlags::new(),
        }
    }
}

impl From<VersionInfo> for MaxVersionInfo {
    fn from(info: VersionInfo) -> Self {
        Self {
            version: info.version as u32,
            feature_flags: info.feature_flags,
        }
    }
}

/// Parses a string of the form "major.minor" (e.g "5.3") into a vmbus version number.
///
/// N.B. This doesn't check whether the specified version actually exists.
pub fn parse_vmbus_version(value: &str) -> Result<u32, String> {
    || -> Option<u32> {
        let (major, minor) = value.split_once('.')?;
        let major = u16::from_str(major).ok()?;
        let minor = u16::from_str(minor).ok()?;
        Some(protocol::make_version(major, minor))
    }()
    .ok_or_else(|| format!("invalid vmbus version '{}'", value))
}

#[derive(Clone, Debug)]
pub struct OutgoingMessage {
    data: [u8; MAX_MESSAGE_SIZE],
    len: u8,
}

/// Represents a vmbus message to be sent using the synic.
impl OutgoingMessage {
    /// Creates a new `OutgoingMessage` for the specified protocol message.
    pub fn new<T: IntoBytes + Immutable + KnownLayout + VmbusMessage>(message: &T) -> Self {
        let mut data = [0; MAX_MESSAGE_SIZE];
        let header = MessageHeader::new(T::MESSAGE_TYPE);
        let message_bytes = message.as_bytes();
        let len = HEADER_SIZE + message_bytes.len();
        data[..HEADER_SIZE].copy_from_slice(header.as_bytes());
        data[HEADER_SIZE..len].copy_from_slice(message_bytes);
        Self {
            data,
            len: len as u8,
        }
    }

    /// Creates a new `OutgoingMessage` for the specified protocol message, including additional
    /// data at the end of the message.
    pub fn with_data<T: IntoBytes + Immutable + KnownLayout + VmbusMessage>(
        message: &T,
        data: &[u8],
    ) -> Self {
        let mut message = OutgoingMessage::new(message);
        let old_len = message.len as usize;
        let len = old_len + data.len();
        message.data[old_len..len].copy_from_slice(data);
        message.len = len as u8;
        message
    }

    /// Converts an existing binary message to an `OutgoingMessage`. The slice
    /// is assumed to contain a valid message.
    pub fn from_message(message: &[u8]) -> Result<Self, MessageTooLarge> {
        if message.len() > MAX_MESSAGE_SIZE {
            return Err(MessageTooLarge);
        }
        let mut data = [0; MAX_MESSAGE_SIZE];
        data[0..message.len()].copy_from_slice(message);
        Ok(Self {
            data,
            len: message.len() as u8,
        })
    }

    /// Gets the binary representation of the message.
    pub fn data(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }
}

impl PartialEq for OutgoingMessage {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.data[..self.len as usize] == other.data[..self.len as usize]
    }
}

#[derive(Debug, Error)]
#[error("a synic message exceeds the maximum length")]
pub struct MessageTooLarge;

/// A request from the guest to connect to the specified hvsocket endpoint.
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq, Inspect)]
pub struct HvsockConnectRequest {
    pub service_id: Guid,
    pub endpoint_id: Guid,
    pub silo_id: Guid,
    pub hosted_silo_unaware: bool,
}

impl HvsockConnectRequest {
    pub fn from_message(value: protocol::TlConnectRequest2, hosted_silo_unaware: bool) -> Self {
        Self {
            service_id: value.base.service_id,
            endpoint_id: value.base.endpoint_id,
            silo_id: value.silo_id,
            hosted_silo_unaware,
        }
    }
}

impl From<HvsockConnectRequest> for protocol::TlConnectRequest2 {
    fn from(value: HvsockConnectRequest) -> Self {
        Self {
            base: protocol::TlConnectRequest {
                endpoint_id: value.endpoint_id,
                service_id: value.service_id,
            },
            silo_id: value.silo_id,
        }
    }
}

/// A notification from the host that a connection request has been handled.
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub struct HvsockConnectResult {
    pub service_id: Guid,
    pub endpoint_id: Guid,
    pub success: bool,
}

impl HvsockConnectResult {
    /// Create a new result using the service and endpoint ID from the specified request.
    pub fn from_request(request: &HvsockConnectRequest, success: bool) -> Self {
        Self {
            service_id: request.service_id,
            endpoint_id: request.endpoint_id,
            success,
        }
    }
}

impl From<protocol::TlConnectResult> for HvsockConnectResult {
    fn from(value: protocol::TlConnectResult) -> Self {
        Self {
            service_id: value.service_id,
            endpoint_id: value.endpoint_id,
            success: value.status == protocol::STATUS_SUCCESS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ChannelId;
    use crate::protocol::GpadlId;

    #[test]
    fn test_outgoing_message() {
        let message = OutgoingMessage::new(&protocol::CloseChannel {
            channel_id: ChannelId(5),
        });

        assert_eq!(&[0x7, 0, 0, 0, 0, 0, 0, 0, 0x5, 0, 0, 0], message.data())
    }

    #[test]
    fn test_outgoing_message_empty() {
        let message = OutgoingMessage::new(&protocol::Unload {});

        assert_eq!(&[0x10, 0, 0, 0, 0, 0, 0, 0], message.data())
    }

    #[test]
    fn test_outgoing_message_with_data() {
        let message = OutgoingMessage::with_data(
            &protocol::GpadlHeader {
                channel_id: ChannelId(5),
                gpadl_id: GpadlId(1),
                len: 7,
                count: 6,
            },
            &[0xa, 0xb, 0xc, 0xd],
        );

        assert_eq!(
            &[
                0x8, 0, 0, 0, 0, 0, 0, 0, 0x5, 0, 0, 0, 0x1, 0, 0, 0, 0x7, 0, 0x6, 0, 0xa, 0xb,
                0xc, 0xd
            ],
            message.data()
        )
    }
}
