// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use bitfield_struct::bitfield;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct SrbStatusAndFlags {
    #[bits(6)]
    status_bits: u8,
    pub frozen: bool,
    pub autosense_valid: bool,
}

impl SrbStatusAndFlags {
    pub fn status(&self) -> SrbStatus {
        SrbStatus(self.status_bits())
    }

    pub fn with_status(self, status: SrbStatus) -> Self {
        self.with_status_bits(status.0)
    }

    pub fn set_status(&mut self, status: SrbStatus) {
        self.set_status_bits(status.0)
    }
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum SrbStatus: u8 {
        PENDING = 0x00,
        SUCCESS = 0x01,
        ABORTED = 0x02,
        ABORT_FAILED = 0x03,
        ERROR = 0x04,
        BUSY = 0x05,
        INVALID_REQUEST = 0x06,
        INVALID_PATH_ID = 0x07,
        NO_DEVICE = 0x08,
        TIMEOUT = 0x09,
        SELECTION_TIMEOUT = 0x0A,
        COMMAND_TIMEOUT = 0x0B,
        MESSAGE_REJECTED = 0x0D,
        BUS_RESET = 0x0E,
        PARITY_ERROR = 0x0F,
        REQUEST_SENSE_FAILED = 0x10,
        NO_HBA = 0x11,
        DATA_OVERRUN = 0x12,
        UNEXPECTED_BUS_FREE = 0x13,
        PHASE_SEQUENCE_FAILURE = 0x14,
        BAD_SRB_BLOCK_LENGTH = 0x15,
        REQUEST_FLUSHED = 0x16,
        INVALID_LUN = 0x20,
        INVALID_TARGET_ID = 0x21,
        BAD_FUNCTION = 0x22,
        ERROR_RECOVERY = 0x23,
        NOT_POWERED = 0x24,
        LINK_DOWN = 0x25,
    }
}

pub const SRB_FLAGS_DATA_IN: u32 = 0x00000040;
pub const SRB_FLAGS_DATA_OUT: u32 = 0x00000080;
