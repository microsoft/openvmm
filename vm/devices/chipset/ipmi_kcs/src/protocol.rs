// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::IpmiKcs;
use crate::KCS_MESSAGE_MAX;
use crate::KCS_STATE_READ;
use crate::STATUS_OBF;

pub(crate) const NETFN_APPLICATION: u8 = 0x06;
pub(crate) const NETFN_STORAGE: u8 = 0x0a;

pub(crate) const COMMAND_GET_DEVICE_ID: u8 = 0x01;
pub(crate) const COMMAND_GET_SEL_INFO: u8 = 0x40;
pub(crate) const COMMAND_RESERVE_SEL: u8 = 0x42;
pub(crate) const COMMAND_GET_SEL_ENTRY: u8 = 0x43;
pub(crate) const COMMAND_ADD_SEL_ENTRY: u8 = 0x44;
pub(crate) const COMMAND_CLEAR_SEL: u8 = 0x47;
pub(crate) const COMMAND_GET_SEL_TIME: u8 = 0x48;
pub(crate) const COMMAND_SET_SEL_TIME: u8 = 0x49;

pub(crate) const COMPLETION_SUCCESS: u8 = 0x00;
pub(crate) const COMPLETION_INVALID_COMMAND: u8 = 0xc1;
pub(crate) const COMPLETION_SEL_FULL: u8 = 0xc4;
pub(crate) const COMPLETION_RESERVATION_CANCELED: u8 = 0xc5;
pub(crate) const COMPLETION_INVALID_REQUEST_LENGTH: u8 = 0xc7;
pub(crate) const COMPLETION_PARAMETER_OUT_OF_RANGE: u8 = 0xc9;
pub(crate) const COMPLETION_RECORD_NOT_PRESENT: u8 = 0xcb;
pub(crate) const COMPLETION_INVALID_DATA_FIELD: u8 = 0xcc;

impl IpmiKcs {
    pub(crate) fn process_ipmi_message(&mut self) {
        if self.transaction.request_len < 2 {
            self.enter_error_state();
            return;
        }

        let request = self.transaction.request;
        let request_len = self.transaction.request_len;
        let netfn_lun = request[0];
        let command = request[1];
        let data = &request[2..request_len];
        let mut body = [0; KCS_MESSAGE_MAX];

        let body_len = match netfn_lun >> 2 {
            NETFN_APPLICATION => self.handle_application_command(command, data, &mut body),
            NETFN_STORAGE => self.handle_sel_command(command, data, &mut body),
            _ => {
                body[0] = COMPLETION_INVALID_COMMAND;
                1
            }
        };

        self.stage_response(netfn_lun, command, &body[..body_len]);
    }

    fn stage_response(&mut self, request_netfn_lun: u8, command: u8, body: &[u8]) {
        self.transaction.response.fill(0);
        self.transaction.response[0] = request_netfn_lun | 0x04;
        self.transaction.response[1] = command;

        let body_len = body.len().min(KCS_MESSAGE_MAX - 2);
        self.transaction.response[2..2 + body_len].copy_from_slice(&body[..body_len]);
        self.transaction.response_len = body_len + 2;
        self.transaction.response_pos = 1;
        self.transaction.data_out = self.transaction.response[0];
        self.transaction.status |= STATUS_OBF;
        self.transaction.set_state(KCS_STATE_READ);
    }

    fn handle_application_command(
        &mut self,
        command: u8,
        _data: &[u8],
        out: &mut [u8; KCS_MESSAGE_MAX],
    ) -> usize {
        match command {
            COMMAND_GET_DEVICE_ID => {
                const RESPONSE: [u8; 12] = [
                    COMPLETION_SUCCESS,
                    0x20,
                    0x01,
                    0x02,
                    0x00,
                    0x02,
                    0x04,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                ];
                out[..RESPONSE.len()].copy_from_slice(&RESPONSE);
                RESPONSE.len()
            }
            _ => completion(out, COMPLETION_INVALID_COMMAND),
        }
    }
}

pub(crate) fn completion(out: &mut [u8; KCS_MESSAGE_MAX], code: u8) -> usize {
    out[0] = code;
    1
}

pub(crate) fn invalid_length(out: &mut [u8; KCS_MESSAGE_MAX]) -> usize {
    completion(out, COMPLETION_INVALID_REQUEST_LENGTH)
}
