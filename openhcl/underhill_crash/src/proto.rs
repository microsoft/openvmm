// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for crash device protocol handling.

use get_protocol::crash::Header;
use get_protocol::crash::MessageType;
use guid::Guid;

/// Protocol error
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("message type is not supported '{0:?}'")]
    MessageTypeNotSupported(MessageType),
    #[error("message type is not valid '{0:?}'")]
    MessageTypeNotValid(MessageType),
}

pub fn make_header(activity_id_header: Option<&Header>, message_type: MessageType) -> Header {
    Header {
        activity_id: activity_id_header.map_or_else(Guid::new_random, |h| h.activity_id),
        message_type,
    }
}

pub fn check_header(header: &Header) -> Result<(), ProtocolError> {
    let ty = header.message_type;
    match ty {
        MessageType::REQUEST_GET_CAPABILITIES_V1
        | MessageType::REQUEST_GET_WINDOWS_DUMP_CONFIG_V1
        | MessageType::REQUEST_WINDOWS_DUMP_START_V1
        | MessageType::REQUEST_WINDOWS_DUMP_WRITE_V1
        | MessageType::REQUEST_WINDOWS_DUMP_COMPLETE_V1
        | MessageType::REQUEST_GET_NIX_DUMP_CONFIG_V1
        | MessageType::REQUEST_NIX_DUMP_START_V1
        | MessageType::REQUEST_NIX_DUMP_WRITE_V1
        | MessageType::REQUEST_NIX_DUMP_COMPLETE_V1
        | MessageType::RESPONSE_GET_CAPABILITIES_V1
        | MessageType::RESPONSE_GET_WINDOWS_DUMP_CONFIG_V1
        | MessageType::RESPONSE_WINDOWS_DUMP_START_V1
        | MessageType::RESPONSE_WINDOWS_DUMP_WRITE_V1
        | MessageType::RESPONSE_WINDOWS_DUMP_COMPLETE_V1
        | MessageType::RESPONSE_GET_NIX_DUMP_CONFIG_V1
        | MessageType::RESPONSE_NIX_DUMP_START_V1
        | MessageType::RESPONSE_NIX_DUMP_WRITE_V1
        | MessageType::RESPONSE_NIX_DUMP_COMPLETE_V1 => {}
        _ => return Err(ProtocolError::MessageTypeNotValid(ty)),
    }
    match ty {
        MessageType::REQUEST_GET_CAPABILITIES_V1
        | MessageType::REQUEST_GET_NIX_DUMP_CONFIG_V1
        | MessageType::REQUEST_NIX_DUMP_START_V1
        | MessageType::REQUEST_NIX_DUMP_WRITE_V1
        | MessageType::REQUEST_NIX_DUMP_COMPLETE_V1
        | MessageType::RESPONSE_GET_CAPABILITIES_V1
        | MessageType::RESPONSE_GET_NIX_DUMP_CONFIG_V1
        | MessageType::RESPONSE_NIX_DUMP_START_V1
        | MessageType::RESPONSE_NIX_DUMP_WRITE_V1
        | MessageType::RESPONSE_NIX_DUMP_COMPLETE_V1 => {}
        _ => return Err(ProtocolError::MessageTypeNotSupported(ty)),
    }
    Ok(())
}
