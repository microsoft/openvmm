// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, TryFromBytes};

use crate::command::TdispCommandRequestGetTdiReport;
use crate::command::TdispCommandRequestPayload;
use crate::command::TdispCommandRequestUnbind;
use crate::command::TdispCommandResponseGetTdiReport;
use crate::command::TdispSerializedCommandRequestGetTdiReport;

use crate::GuestToHostCommand;
use crate::GuestToHostResponse;
use crate::TdispCommandId;
use crate::TdispCommandResponsePayload;
use crate::TdispDeviceInterfaceInfo;
use crate::TdispGuestOperationError;
use crate::TdispGuestOperationErrorCode;
use crate::TdispTdiState;

/// Serialized form of the header for a GuestToHostCommand packet
#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct GuestToHostCommandSerializedHeader {
    /// The logical TDISP device ID of the device that the command is being sent to.
    pub device_id: u64,

    /// The command ID of the command that is being sent. See: `TdispCommandId`
    pub command_id: u64,
}

/// Serialized form of the header for a GuestToHostResponse packet
#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct GuestToHostResponseSerializedHeader {
    /// The command ID of the command that was processed. See: `TdispCommandId`
    pub command_id: u64,

    /// The result of the command. See: `TdispGuestOperationError`
    pub result: u64,

    /// The TDI state before the command was processed. See: `TdispTdiState`
    pub tdi_state_before: u64,

    /// The TDI state after the command was processed. See: `TdispTdiState`
    pub tdi_state_after: u64,
}

/// Trait used to serialize a command or response header into its serializable form.
trait SerializeHeader {
    type SerializedHeader;

    fn to_serializable_header(&self) -> Self::SerializedHeader;
}

/// Trait used to deserialize a command or response header from its serializable form.
trait DeserializeHeader {
    type DeserializedHeader;

    fn from_serializable_header(header: &Self::DeserializedHeader) -> Self;
}

impl SerializeHeader for GuestToHostCommand {
    type SerializedHeader = GuestToHostCommandSerializedHeader;

    fn to_serializable_header(&self) -> Self::SerializedHeader {
        GuestToHostCommandSerializedHeader {
            device_id: self.device_id,
            command_id: self.command_id.0,
        }
    }
}

impl SerializeHeader for GuestToHostResponse {
    type SerializedHeader = GuestToHostResponseSerializedHeader;

    fn to_serializable_header(&self) -> Self::SerializedHeader {
        let serialized_err_code: TdispGuestOperationErrorCode = self.result.into();
        GuestToHostResponseSerializedHeader {
            command_id: self.command_id.0,
            result: serialized_err_code.0,
            tdi_state_before: self.tdi_state_before.0,
            tdi_state_after: self.tdi_state_after.0,
        }
    }
}

impl DeserializeHeader for GuestToHostCommand {
    type DeserializedHeader = GuestToHostCommandSerializedHeader;

    fn from_serializable_header(header: &Self::DeserializedHeader) -> Self {
        GuestToHostCommand {
            device_id: header.device_id,
            command_id: TdispCommandId(header.command_id),
            payload: TdispCommandRequestPayload::None,
        }
    }
}

impl DeserializeHeader for GuestToHostResponse {
    type DeserializedHeader = GuestToHostResponseSerializedHeader;

    fn from_serializable_header(header: &Self::DeserializedHeader) -> Self {
        let serialized_err_code: TdispGuestOperationErrorCode =
            TdispGuestOperationErrorCode(header.result);
        GuestToHostResponse {
            command_id: TdispCommandId(header.command_id),
            result: serialized_err_code.into(),
            tdi_state_before: TdispTdiState(header.tdi_state_before),
            tdi_state_after: TdispTdiState(header.tdi_state_after),
            payload: TdispCommandResponsePayload::None,
        }
    }
}

/// Trait implemented by the guest-to-host command and response structs to allow serialization and deserialization.
pub trait SerializePacket: Sized {
    /// Serialize the struct to a byte vector.
    fn serialize_to_bytes(self) -> Vec<u8>;

    /// Deserialize a byte slice into a struct.
    fn deserialize_from_bytes(bytes: &[u8]) -> Result<Self, anyhow::Error>;
}

impl SerializePacket for GuestToHostCommand {
    fn serialize_to_bytes(self) -> Vec<u8> {
        let header = self.to_serializable_header();
        let bytes = header.as_bytes();
        tracing::debug!(msg = format!("serialize_to_bytes: header={:?}", header));
        tracing::debug!(msg = format!("serialize_to_bytes: {:?}", bytes));

        let mut bytes = bytes.to_vec();
        match self.payload {
            TdispCommandRequestPayload::None => {}
            TdispCommandRequestPayload::Unbind(info) => bytes.extend_from_slice(info.as_bytes()),
            TdispCommandRequestPayload::GetTdiReport(info) => {
                bytes.extend_from_slice(info.as_bytes())
            }
        };

        bytes
    }

    fn deserialize_from_bytes(bytes: &[u8]) -> Result<Self, anyhow::Error> {
        let header_length = size_of::<GuestToHostCommandSerializedHeader>();
        tracing::debug!(msg = format!("deserialize_from_bytes: header_length={header_length}"));
        tracing::debug!(msg = format!("deserialize_from_bytes: {:?}", bytes));

        let header_bytes = &bytes[0..header_length];
        tracing::debug!(msg = format!("deserialize_from_bytes: header_bytes={:?}", header_bytes));

        let header =
            GuestToHostCommandSerializedHeader::try_ref_from_bytes(header_bytes).map_err(|e| {
                anyhow::anyhow!("failed to deserialize GuestToHostCommand header: {:?}", e)
            })?;

        let payload_slice = &bytes[header_length..];

        let mut packet: Self = GuestToHostCommand::from_serializable_header(header);

        if !payload_slice.is_empty() {
            let payload = match packet.command_id {
                TdispCommandId::UNBIND => TdispCommandRequestPayload::Unbind(
                    TdispCommandRequestUnbind::try_read_from_bytes(payload_slice).map_err(|e| {
                        anyhow::anyhow!("failed to deserialize TdispCommandRequestUnbind: {:?}", e)
                    })?,
                ),
                TdispCommandId::BIND => TdispCommandRequestPayload::None,
                TdispCommandId::GET_DEVICE_INTERFACE_INFO => TdispCommandRequestPayload::None,
                TdispCommandId::START_TDI => TdispCommandRequestPayload::None,
                TdispCommandId::GET_TDI_REPORT => TdispCommandRequestPayload::GetTdiReport(
                    TdispCommandRequestGetTdiReport::try_read_from_bytes(payload_slice).map_err(
                        |e| {
                            anyhow::anyhow!(
                                "failed to deserialize TdispCommandRequestGetTdiReport: {:?}",
                                e
                            )
                        },
                    )?,
                ),
                TdispCommandId::UNKNOWN => {
                    return Err(anyhow::anyhow!(
                        "Unknown payload type for command id {:?} while deserializing GuestToHostCommand",
                        header.command_id
                    ));
                }
                _ => {
                    return Err(anyhow::anyhow!(
                        "Unknown payload type for command id {:?} while deserializing GuestToHostCommand",
                        header.command_id
                    ));
                }
            };

            packet.payload = payload;
        }

        Ok(packet)
    }
}

impl SerializePacket for GuestToHostResponse {
    fn serialize_to_bytes(self) -> Vec<u8> {
        let header = self.to_serializable_header();
        let bytes = header.as_bytes();

        let mut bytes = bytes.to_vec();
        match self.payload {
            TdispCommandResponsePayload::None => {}
            TdispCommandResponsePayload::GetDeviceInterfaceInfo(info) => {
                bytes.extend_from_slice(info.as_bytes())
            }
            TdispCommandResponsePayload::GetTdiReport(info) => {
                let header = TdispSerializedCommandRequestGetTdiReport {
                    report_type: info.report_type,
                    report_buffer_size: info.report_buffer.len() as u32,
                };

                bytes.extend_from_slice(header.as_bytes());
                bytes.extend_from_slice(info.report_buffer.as_bytes());
            }
        };

        bytes
    }

    // [TDISP TODO] Clean up this serialization code to be a bit more generic.
    fn deserialize_from_bytes(bytes: &[u8]) -> Result<Self, anyhow::Error> {
        let header_length = size_of::<GuestToHostResponseSerializedHeader>();
        let header =
            GuestToHostResponseSerializedHeader::try_ref_from_bytes(&bytes[0..header_length])
                .map_err(|e| {
                    anyhow::anyhow!("failed to deserialize GuestToHostResponse header: {:?}", e)
                })?;

        let mut packet: Self = GuestToHostResponse::from_serializable_header(header);

        // If the result is not success, then we don't need to deserialize the payload.
        match packet.result {
            TdispGuestOperationError::Success => {}
            _ => {
                return Ok(packet);
            }
        }

        let payload_slice = &bytes[header_length..];

        if !payload_slice.is_empty() {
            let payload = match packet.command_id {
                TdispCommandId::GET_DEVICE_INTERFACE_INFO => {
                    TdispCommandResponsePayload::GetDeviceInterfaceInfo(
                        TdispDeviceInterfaceInfo::try_read_from_bytes(payload_slice).map_err(
                            |e| {
                                anyhow::anyhow!(
                                    "failed to deserialize TdispDeviceInterfaceInfo: {:?}",
                                    e
                                )
                            },
                        )?,
                    )
                }
                TdispCommandId::BIND => TdispCommandResponsePayload::None,
                TdispCommandId::UNBIND => TdispCommandResponsePayload::None,
                TdispCommandId::START_TDI => TdispCommandResponsePayload::None,
                TdispCommandId::GET_TDI_REPORT => {
                    // Peel off the header from the payload
                    let payload_header_len = size_of::<TdispSerializedCommandRequestGetTdiReport>();
                    let payload_header_slice = &payload_slice[0..payload_header_len];

                    // Read the header
                    let payload_header =
                        TdispSerializedCommandRequestGetTdiReport::try_read_from_bytes(
                            payload_header_slice,
                        )
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "failed to deserialize TdispSerializedCommandRequestGetTdiReport: {:?}",
                                e
                            )
                        })?;

                    // Determine the number of bytes to read from the payload for the report buffer
                    let payload_bytes = &payload_slice[payload_header_len
                        ..(payload_header_len + payload_header.report_buffer_size as usize)];

                    // Convert this to the response type
                    TdispCommandResponsePayload::GetTdiReport(TdispCommandResponseGetTdiReport {
                        report_type: payload_header.report_type,
                        report_buffer: payload_bytes.to_vec(),
                    })
                }
                TdispCommandId::UNKNOWN => {
                    return Err(anyhow::anyhow!(
                        "invalid command id in GuestToHostResponse: {:?}",
                        header.result
                    ));
                }
                _ => {
                    return Err(anyhow::anyhow!(
                        "invalid command id in GuestToHostResponse: {:?}",
                        header.result
                    ));
                }
            };

            packet.payload = payload;
        }

        Ok(packet)
    }
}
