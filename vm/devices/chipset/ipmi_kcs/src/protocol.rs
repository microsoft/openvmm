// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! IPMI request dispatch and KCS response staging.

use crate::IpmiKcs;
use crate::KCS_MESSAGE_MAX;
use crate::KCS_STATE_READ;
use crate::STATUS_OBF;
use core::mem::size_of;
use ipmi_protocol::COMMAND_GET_DEVICE_ID;
use ipmi_protocol::COMPLETION_INVALID_COMMAND;
use ipmi_protocol::COMPLETION_INVALID_REQUEST_LENGTH;
use ipmi_protocol::CompletionResponse;
use ipmi_protocol::GetDeviceIdResponse;
use ipmi_protocol::MessageHeader;
use ipmi_protocol::NETFN_APPLICATION;
use ipmi_protocol::NETFN_STORAGE;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

impl IpmiKcs {
    /// Dispatches the accumulated IPMI request and stages its KCS response.
    pub(crate) fn process_ipmi_message(&mut self) {
        let request = self.transaction.request;
        let request = &request[..self.transaction.request_len];
        let Ok((header, data)) = MessageHeader::read_from_prefix(request) else {
            self.enter_error_state();
            return;
        };
        let mut body = [0; KCS_MESSAGE_MAX];

        let body_len = match header.netfn() {
            NETFN_APPLICATION => self.handle_application_command(header.command, data, &mut body),
            NETFN_STORAGE => self.handle_sel_command(header.command, data, &mut body),
            _ => {
                body[0] = COMPLETION_INVALID_COMMAND;
                1
            }
        };

        self.stage_response(header.response(), &body[..body_len]);
    }

    /// Builds an IPMI response and exposes its first byte through the KCS data register.
    fn stage_response(&mut self, header: MessageHeader, body: &[u8]) {
        self.transaction.response.fill(0);
        self.transaction.response[..size_of::<MessageHeader>()].copy_from_slice(header.as_bytes());

        let header_len = size_of::<MessageHeader>();
        let body_len = body.len().min(KCS_MESSAGE_MAX - header_len);
        self.transaction.response[header_len..header_len + body_len]
            .copy_from_slice(&body[..body_len]);
        self.transaction.response_len = body_len + header_len;
        self.transaction.response_pos = 1;
        self.transaction.data_out = self.transaction.response[0];
        self.transaction.status |= STATUS_OBF;
        self.transaction.set_state(KCS_STATE_READ);
    }

    /// Handles commands in the IPMI application network function.
    fn handle_application_command(
        &mut self,
        command: u8,
        _data: &[u8],
        out: &mut [u8; KCS_MESSAGE_MAX],
    ) -> usize {
        match command {
            COMMAND_GET_DEVICE_ID => write_response(out, &GetDeviceIdResponse::new()),
            _ => completion(out, COMPLETION_INVALID_COMMAND),
        }
    }
}

/// Writes a fixed-format response body into the KCS output buffer.
pub(crate) fn write_response<T: IntoBytes + Immutable>(
    out: &mut [u8; KCS_MESSAGE_MAX],
    response: &T,
) -> usize {
    let bytes = response.as_bytes();
    out[..bytes.len()].copy_from_slice(bytes);
    bytes.len()
}

/// Writes a response containing only an IPMI completion code.
pub(crate) fn completion(out: &mut [u8; KCS_MESSAGE_MAX], code: u8) -> usize {
    write_response(out, &CompletionResponse::new(code))
}

/// Writes the standard invalid-request-length response.
pub(crate) fn invalid_length(out: &mut [u8; KCS_MESSAGE_MAX]) -> usize {
    completion(out, COMPLETION_INVALID_REQUEST_LENGTH)
}
