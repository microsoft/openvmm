use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, TryFromBytes};

use crate::command::{
    TdispCommandRequestGetTdiReport, TdispCommandRequestPayload, TdispCommandRequestUnbind,
    TdispCommandResponseGetTdiReport, TdispSerializedCommandRequestGetTdiReport,
};
use crate::{
    GuestToHostCommand, GuestToHostResponse, TdispCommandResponsePayload, TdispDeviceReport,
    TdispDeviceReportType, TdispGuestOperationError, TdispTdiReport,
};
use crate::{TdispCommandId, TdispDeviceInterfaceInfo};

/// Serialized form of the header for a GuestToHostCommand packet
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct GuestToHostCommandSerializedHeader {
    pub device_id: u64,
    pub command_id: u64,
}

/// Serialized form of the header for a GuestToHostResponse packet
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct GuestToHostResponseSerializedHeader {
    pub command_id: u64,
    pub result: u64,
    pub tdi_state_before: u64,
    pub tdi_state_after: u64,
}

// [TDISP TODO] There's probably a better way to do these conversions.
impl From<&GuestToHostCommand> for GuestToHostCommandSerializedHeader {
    fn from(value: &GuestToHostCommand) -> Self {
        GuestToHostCommandSerializedHeader {
            device_id: value.device_id,
            command_id: value.command_id.into(),
        }
    }
}

impl From<&GuestToHostResponse> for GuestToHostResponseSerializedHeader {
    fn from(value: &GuestToHostResponse) -> Self {
        GuestToHostResponseSerializedHeader {
            command_id: value.command_id.into(),
            result: value.result.into(),
            tdi_state_before: value.tdi_state_before.into(),
            tdi_state_after: value.tdi_state_after.into(),
        }
    }
}

impl From<&GuestToHostCommandSerializedHeader> for GuestToHostCommand {
    fn from(value: &GuestToHostCommandSerializedHeader) -> Self {
        GuestToHostCommand {
            device_id: value.device_id,
            command_id: value.command_id.into(),
            payload: TdispCommandRequestPayload::None,
        }
    }
}

impl From<&GuestToHostResponseSerializedHeader> for GuestToHostResponse {
    fn from(value: &GuestToHostResponseSerializedHeader) -> Self {
        GuestToHostResponse {
            command_id: value.command_id.into(),
            result: value.result.into(),
            tdi_state_before: value.tdi_state_before.into(),
            tdi_state_after: value.tdi_state_after.into(),
            payload: TdispCommandResponsePayload::None,
        }
    }
}
pub trait SerializePacket: Sized {
    fn serialize_to_bytes(self) -> Vec<u8>;
    fn deserialize_from_bytes(bytes: &[u8]) -> Result<Self, anyhow::Error>;
}

impl SerializePacket for GuestToHostCommand {
    fn serialize_to_bytes(self) -> Vec<u8> {
        let header = GuestToHostCommandSerializedHeader::from(&self);
        let bytes = header.as_bytes();

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
        tracing::error!(msg = format!("deserialize_from_bytes: header_length={header_length}"));
        tracing::error!(msg = format!("deserialize_from_bytes: {:?}", bytes));

        let header_bytes = &bytes[0..header_length];
        tracing::error!(msg = format!("deserialize_from_bytes: header_bytes={:?}", header_bytes));

        let header =
            GuestToHostCommandSerializedHeader::try_ref_from_bytes(header_bytes).map_err(|e| {
                anyhow::anyhow!("failed to deserialize GuestToHostCommand header: {:?}", e)
            })?;

        let payload_slice = &bytes[header_length..];

        let mut packet: Self = header.into();
        let payload = match packet.command_id {
            TdispCommandId::Unbind => TdispCommandRequestPayload::Unbind(
                TdispCommandRequestUnbind::try_read_from_bytes(payload_slice).map_err(|e| {
                    anyhow::anyhow!("failed to deserialize TdispCommandRequestUnbind: {:?}", e)
                })?,
            ),
            TdispCommandId::Bind => TdispCommandRequestPayload::None,
            TdispCommandId::GetDeviceInterfaceInfo => TdispCommandRequestPayload::None,
            TdispCommandId::StartTdi => TdispCommandRequestPayload::None,
            TdispCommandId::GetTdiReport => TdispCommandRequestPayload::GetTdiReport(
                TdispCommandRequestGetTdiReport::try_read_from_bytes(payload_slice).map_err(
                    |e| {
                        anyhow::anyhow!(
                            "failed to deserialize TdispCommandRequestGetTdiReport: {:?}",
                            e
                        )
                    },
                )?,
            ),
            TdispCommandId::Unknown => {
                return Err(anyhow::anyhow!(
                    "Unknown payload type for command id {:?} while deserializing GuestToHostCommand",
                    header.command_id
                ));
            }
        };

        packet.payload = payload;

        Ok(packet)
    }
}

impl SerializePacket for GuestToHostResponse {
    fn serialize_to_bytes(self) -> Vec<u8> {
        let header = GuestToHostResponseSerializedHeader::from(&self);
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

        let mut packet: Self = header.into();

        // If the result is not success, then we don't need to deserialize the payload.
        match packet.result {
            TdispGuestOperationError::Success => {}
            _ => {
                return Ok(packet);
            }
        }

        let payload_slice = &bytes[header_length..];

        let payload = match packet.command_id {
            TdispCommandId::GetDeviceInterfaceInfo => {
                TdispCommandResponsePayload::GetDeviceInterfaceInfo(
                    TdispDeviceInterfaceInfo::try_read_from_bytes(payload_slice).map_err(|e| {
                        anyhow::anyhow!("failed to deserialize TdispDeviceInterfaceInfo: {:?}", e)
                    })?,
                )
            }
            TdispCommandId::Bind => TdispCommandResponsePayload::None,
            TdispCommandId::Unbind => TdispCommandResponsePayload::None,
            TdispCommandId::StartTdi => TdispCommandResponsePayload::None,
            TdispCommandId::GetTdiReport => {
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
            TdispCommandId::Unknown => {
                return Err(anyhow::anyhow!(
                    "invalid payload type in GuestToHostResponse: {:?}",
                    header.result
                ));
            }
        };

        packet.payload = payload;

        Ok(packet)
    }
}
