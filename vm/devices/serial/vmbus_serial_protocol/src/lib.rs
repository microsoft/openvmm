// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Protocol definitions for a VMBUS based serial device. Today this serial device is only offered to VTL2.

#![warn(missing_docs)]

use core::fmt::Debug;
use guid::Guid;
use open_enum::open_enum;
use static_assertions::const_assert_eq;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Maximum message size for all messages.
pub const MAX_MESSAGE_SIZE: usize = 512;

// {8b60ccf6-709f-4c11-90b5-229c959a9e6a}
/// VMBUS Interface Type GUID
pub const UART_INTERFACE_TYPE: Guid = Guid::from_static_str("8b60ccf6-709f-4c11-90b5-229c959a9e6a");

// {700df40e-b947-4776-b839-d1b0a35af034}
/// VMBUS Instance GUID for COM1
pub const UART_INTERFACE_INSTANCE_COM1: Guid =
    Guid::from_static_str("700df40e-b947-4776-b839-d1b0a35af034");

// {7e55f4b8-af84-4e98-9f1a-8e8d0bde3744}
/// VMBUS Instance GUID for COM2
pub const UART_INTERFACE_INSTANCE_COM2: Guid =
    Guid::from_static_str("7e55f4b8-af84-4e98-9f1a-8e8d0bde3744");

// {3f158fa1-b0aa-45e9-ba54-9fc73f6c59ec}
/// VMBUS Instance GUID for COM3
pub const UART_INTERFACE_INSTANCE_COM3: Guid =
    Guid::from_static_str("3f158fa1-b0aa-45e9-ba54-9fc73f6c59ec");

// {8688a06f-9b53-48ce-b408-7581626228c5}
/// VMBUS Instance GUID for COM4
pub const UART_INTERFACE_INSTANCE_COM4: Guid =
    Guid::from_static_str("8688a06f-9b53-48ce-b408-7581626228c5");

const fn make_version(major: u16, minor: u16) -> u32 {
    (minor as u32) | ((major as u32) << 16)
}

open_enum! {
    /// Protocol versions.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum ProtocolVersions: u32 {
        /// Represents the MANGANESE protocol version.
        MANGANESE = make_version(1, 0),
    }
}

open_enum! {
    /// Header message versions.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum MessageVersions: u8 {
        /// Invalid version.
        INVALID          = 0,
        /// Version 1
        HEADER_VERSION_1 = 1,
    }
}

open_enum! {
    /// Enum for the different message types.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum MessageTypes: u8 {
        /// Invalid message type.
        INVALID            = 0,
        /// A [`HostNotifications`](crate::HostNotifications) message.
        HOST_NOTIFICATION  = 1,
        /// A [`HostRequests`](crate::HostRequests) message.
        HOST_REQUEST       = 2,
        /// A response to a [`HostRequests`](crate::HostRequests) message.
        HOST_RESPONSE      = 3,
        /// A [`GuestNotifications`](crate::GuestNotifications) message.
        GUEST_NOTIFICATION = 4,
    }
}

open_enum! {
    /// Enum for the different host notification messages.
    /// These are aysynchronous messages sent by the HCL to the Host.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum HostNotifications: u8 {
        /// Invalid message.
        INVALID           = 0,
        /// Clear the associated RX buffer on the host side.
        RX_CLEAR_BUFFER   = 1,
        /// TX data from the guest, specified by [`TxDataAvailableMessage`](crate::TxDataAvailableMessage).
        TX_DATA_AVAILABLE = 2,
    }
}

open_enum! {
    /// Enum for the different guest notification messages.
    /// These are asynchronous messages sent by the Host to the HCL.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum GuestNotifications: u8 {
        /// Invalid message.
        INVALID           = 0,
        /// RX data available to be requested from the host.
        RX_DATA_AVAILABLE = 1,
        /// UART modem status bits have changed.
        SET_MODEM_STATUS  = 2,
        /// The TX specified by a [`HostNotifications::TX_DATA_AVAILABLE`] message has finished and another can now
        /// be sent.
        TX_COMPLETED      = 3,
    }
}

open_enum! {
    /// Enum for the different host request and response messages.
    /// These are synchronous messages sent by the HCL to the host.
    /// Note that the host response shares the same enum.
    /// (Each request has a response of the same ID)
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum HostRequests: u16 {
        /// Invalid message.
        INVALID     = 0,
        /// Negotiate protocol version specified by the [`VersionRequestMessage`] with response
        /// [`VersionRequestResponse`].
        VERSION     = 1,
        /// Get RX data from the host with response [`RxDataResponse`].
        GET_RX_DATA = 2,
    }
}

/// A wrapper type for a protocol message id union.
///
/// NOTE: In C/C++, this is represented as a union like the following Rust definition:
///
/// ```ignore
/// #[repr(C)]
/// union MessageId {
///     as_u16: u16,
///     host_notification: HostNotifications,
///     host_request: HostRequests,
///     guest_notification: GuestNotifications,
/// }
/// ```
///
/// However, using unions in Rust requires unsafe code. Instead, since the upper byte is
/// currently unused for anything, just have a wrapper struct with accessor methods for
/// individual field types.
#[repr(transparent)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct MessageId(pub u16);

impl Debug for MessageId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MessageId")
            .field("as_u16", &self.0)
            .field("host_notification", &self.host_notification())
            .field("host_request", &self.host_request())
            .field("guest_notification", &self.guest_notification())
            .finish()
    }
}

impl MessageId {
    fn guest_notification(&self) -> GuestNotifications {
        GuestNotifications(self.0 as u8)
    }

    fn host_request(&self) -> HostRequests {
        HostRequests(self.0)
    }

    fn host_notification(&self) -> HostNotifications {
        HostNotifications(self.0 as u8)
    }

    fn new_guest_notification(guest_notification: GuestNotifications) -> Self {
        MessageId(guest_notification.0 as u16)
    }

    fn new_host_request(host_request: HostRequests) -> Self {
        MessageId(host_request.0)
    }

    fn new_host_notification(host_notification: HostNotifications) -> Self {
        MessageId(host_notification.0 as u16)
    }
}

/// A protocol message header.
#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq, Eq)]
pub struct Header {
    /// The message version.
    pub message_version: MessageVersions,
    /// The message type.
    pub message_type: MessageTypes,
    /// The message id.
    pub message_id: MessageId,
}

impl Debug for Header {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let host_notification = self.message_id.host_notification();
        let host_request = self.message_id.host_request();
        let guest_notification = self.message_id.guest_notification();

        f.debug_struct("Header")
            .field("message_version", &self.message_version)
            .field("message_type", &self.message_type)
            .field(
                "message_id",
                match self.message_type {
                    MessageTypes::HOST_NOTIFICATION => &host_notification,
                    MessageTypes::HOST_REQUEST => &host_request,
                    MessageTypes::HOST_RESPONSE => &host_request,
                    MessageTypes::GUEST_NOTIFICATION => &guest_notification,
                    _ => &self.message_id.0,
                },
            )
            .finish()
    }
}

impl Default for Header {
    fn default() -> Self {
        Self {
            message_version: MessageVersions::INVALID,
            message_type: MessageTypes::INVALID,
            message_id: MessageId::new_zeroed(),
        }
    }
}

impl Header {
    /// Create a new header for a guest notification message.
    pub fn new_guest_notification(guest_notification: GuestNotifications) -> Self {
        Self {
            message_version: MessageVersions::HEADER_VERSION_1,
            message_type: MessageTypes::GUEST_NOTIFICATION,
            message_id: MessageId::new_guest_notification(guest_notification),
        }
    }

    /// The associated guest notification message id, if any.
    pub fn guest_notification(&self) -> Option<GuestNotifications> {
        if self.message_type == MessageTypes::GUEST_NOTIFICATION {
            Some(self.message_id.guest_notification())
        } else {
            None
        }
    }

    /// The associated host response message id, if any.
    pub fn host_response(&self) -> Option<HostRequests> {
        // NOTE: Host responses are encoded in the host_request field,
        //       but have message type HostResponse.
        if self.message_type == MessageTypes::HOST_RESPONSE {
            Some(self.message_id.host_request())
        } else {
            None
        }
    }

    /// Create a new header for a host response message.
    pub fn new_host_response(host_response: HostRequests) -> Self {
        Self {
            message_version: MessageVersions::HEADER_VERSION_1,
            message_type: MessageTypes::HOST_RESPONSE,
            message_id: MessageId::new_host_request(host_response),
        }
    }

    /// The associated host request message id, if any.
    pub fn host_request(&self) -> Option<HostRequests> {
        if self.message_type == MessageTypes::HOST_REQUEST {
            Some(self.message_id.host_request())
        } else {
            None
        }
    }

    /// Create a new header for a host request message.
    pub fn new_host_request(host_request: HostRequests) -> Self {
        Self {
            message_version: MessageVersions::HEADER_VERSION_1,
            message_type: MessageTypes::HOST_REQUEST,
            message_id: MessageId::new_host_request(host_request),
        }
    }

    /// Create a new header for a host notification message.
    pub fn new_host_notification(host_notification: HostNotifications) -> Self {
        Self {
            message_version: MessageVersions::HEADER_VERSION_1,
            message_type: MessageTypes::HOST_NOTIFICATION,
            message_id: MessageId::new_host_notification(host_notification),
        }
    }

    /// The associated host notification message id, if any.
    pub fn host_notification(&self) -> Option<HostNotifications> {
        if self.message_type == MessageTypes::HOST_NOTIFICATION {
            Some(self.message_id.host_notification())
        } else {
            None
        }
    }
}

const_assert_eq!(4, size_of::<Header>());

/// The maximum data size for a TX or RX.
pub const UART_MSG_MAX_PAYLOAD: usize = 64;

// Each protocol message starts with a header, and has potential additional data.
// Messages that do not have additional structures present below are header only messages.

// Host Notifications

/// Host notification message that TX data is available.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TxDataAvailableMessage {
    /// The message header.
    pub header: Header,
    /// The number of bytes valid in buffer.
    pub buffer_length: u8,
    /// The TX data buffer.
    pub buffer: [u8; UART_MSG_MAX_PAYLOAD],
    /// Padding that must be zero.
    pub pad: u8,
}

impl Default for TxDataAvailableMessage {
    fn default() -> Self {
        Self {
            header: Header::default(),
            buffer_length: 0,
            buffer: [0; UART_MSG_MAX_PAYLOAD],
            pad: 0,
        }
    }
}

const_assert_eq!(70, size_of::<TxDataAvailableMessage>());

// Guest Notifications

/// Guest notification that the connection state of the serial port has changed.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SetModumStatusMessage {
    /// The message header.
    pub header: Header,
    /// 16550-style modem status. Ignored.
    pub modem_status: u8,
    /// A boolean indicating if the modem is connected.
    pub is_connected: u8,
}

const_assert_eq!(6, size_of::<SetModumStatusMessage>());

// Host Requests and Responses

/// A version negotiation request sent from the guest to host.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VersionRequestMessage {
    /// The message header.
    pub header: Header,
    /// The requested protocol version to use.
    pub requested_version: ProtocolVersions,
}

impl Default for VersionRequestMessage {
    fn default() -> Self {
        Self {
            header: Header::default(),
            requested_version: ProtocolVersions(0),
        }
    }
}

const_assert_eq!(8, size_of::<VersionRequestMessage>());

/// A version negotiation response sent from host to guest.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VersionRequestResponse {
    /// The message header.
    pub header: Header,
    /// 1 if the host accepted the version requested. 0 if the version was rejected.
    pub version_accepted: u8,
    /// Padding that must be zero.
    pub pad: u8,
}

const_assert_eq!(6, size_of::<VersionRequestResponse>());

/// A response to an RX Data host request message that contains RX data.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct RxDataResponse {
    /// The message header.
    pub header: Header,
    /// The number of valid data bytes in buffer.
    pub buffer_length: u8,
    /// 1 if more RX data is available on the host. 0 if no further data is available.
    pub more_data_available: u8,
    /// The RX data buffer.
    pub buffer: [u8; UART_MSG_MAX_PAYLOAD],
}

const_assert_eq!(70, size_of::<RxDataResponse>());

impl Default for RxDataResponse {
    fn default() -> Self {
        Self {
            header: Header::default(),
            buffer_length: 0,
            more_data_available: 0,
            buffer: [0; UART_MSG_MAX_PAYLOAD],
        }
    }
}
