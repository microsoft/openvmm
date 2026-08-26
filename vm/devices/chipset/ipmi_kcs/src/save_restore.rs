// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::IpmiKcs;
use crate::KCS_MESSAGE_MAX;
use crate::KCS_STATE_ERROR;
use crate::KCS_STATE_IDLE;
use crate::KCS_STATE_READ;
use crate::KCS_STATE_WRITE;
use crate::KcsTransaction;
use crate::STATUS_IBF;
use crate::STATUS_STATE_MASK;
use crate::SelStats;
use crate::sel::SEL_CAPACITY;
use crate::sel::SEL_RECORD_SIZE;
use crate::sel::SelState;
use mesh::payload::Protobuf;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateRoot;

/// Serializable state for [`IpmiKcs`].
#[derive(Clone, Debug, Protobuf, SavedStateRoot)]
#[mesh(package = "chipset.ipmi_kcs")]
pub struct SavedState {
    #[mesh(1)]
    pub(crate) status: u8,
    #[mesh(2)]
    pub(crate) data_out: u8,
    #[mesh(3)]
    pub(crate) request: Vec<u8>,
    #[mesh(4)]
    pub(crate) response: Vec<u8>,
    #[mesh(5)]
    pub(crate) response_position: u32,
    #[mesh(6)]
    pub(crate) write_end_pending: bool,
    #[mesh(7)]
    pub(crate) sel_records: Vec<Vec<u8>>,
    #[mesh(8)]
    pub(crate) sel_count: u32,
    #[mesh(9)]
    pub(crate) next_record_id: u16,
    #[mesh(10)]
    pub(crate) reservation_id: u16,
    #[mesh(11)]
    pub(crate) time_offset_seconds: i64,
    #[mesh(12)]
    pub(crate) last_erase_timestamp: u32,
}

#[derive(Debug, thiserror::Error)]
enum SavedStateValidationError {
    #[error("status contains unsupported bits: {0:#04x}")]
    UnsupportedStatusBits(u8),
    #[error("invalid KCS state: {0:#04x}")]
    InvalidKcsState(u8),
    #[error("request length {0} exceeds {KCS_MESSAGE_MAX}")]
    RequestTooLong(usize),
    #[error("response length {0} exceeds {KCS_MESSAGE_MAX}")]
    ResponseTooLong(usize),
    #[error("response position {position} exceeds response length {length}")]
    InvalidResponsePosition { position: usize, length: usize },
    #[error("read state has no response")]
    EmptyReadResponse,
    #[error("write-end-pending is set outside write state")]
    InvalidWriteEndPending,
    #[error("SEL count {count} does not match {records} saved records")]
    SelCountMismatch { count: usize, records: usize },
    #[error("SEL count {0} exceeds {SEL_CAPACITY}")]
    TooManySelRecords(usize),
    #[error("SEL record {index} has length {length}, expected {SEL_RECORD_SIZE}")]
    InvalidSelRecordLength { index: usize, length: usize },
    #[error("SEL record {index} has reserved record ID {record_id:#06x}")]
    InvalidSelRecordId { index: usize, record_id: u16 },
    #[error("SEL contains duplicate record ID {0:#06x}")]
    DuplicateSelRecordId(u16),
    #[error("next SEL record ID is reserved: {0:#06x}")]
    InvalidNextRecordId(u16),
    #[error("next SEL record ID {0:#06x} is already present")]
    DuplicateNextRecordId(u16),
    #[error("saved numeric field does not fit on this host")]
    NumericOverflow,
}

impl SaveRestore for IpmiKcs {
    type SavedState = SavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Ok(SavedState {
            status: self.transaction.status,
            data_out: self.transaction.data_out,
            request: self.transaction.request[..self.transaction.request_len].to_vec(),
            response: self.transaction.response[..self.transaction.response_len].to_vec(),
            response_position: self.transaction.response_pos as u32,
            write_end_pending: self.transaction.write_end_pending,
            sel_records: self
                .sel
                .records
                .iter()
                .map(|record| record.to_vec())
                .collect(),
            sel_count: self.sel.records.len() as u32,
            next_record_id: self.sel.next_record_id,
            reservation_id: self.sel.reservation_id,
            time_offset_seconds: self.sel.time_offset_seconds,
            last_erase_timestamp: self.sel.last_erase_timestamp,
        })
    }

    fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
        let restored = validate_saved_state(state)
            .map_err(|error| RestoreError::InvalidSavedState(error.into()))?;

        self.transaction = restored.transaction;
        self.sel = restored.sel;
        self.rate_limiter.reset();
        self.stats = SelStats::default();
        Ok(())
    }
}

struct RestoredState {
    transaction: KcsTransaction,
    sel: SelState,
}

fn validate_saved_state(state: SavedState) -> Result<RestoredState, SavedStateValidationError> {
    let SavedState {
        status,
        data_out,
        request,
        response,
        response_position,
        write_end_pending,
        sel_records,
        sel_count,
        next_record_id,
        reservation_id,
        time_offset_seconds,
        last_erase_timestamp,
    } = state;

    if status & STATUS_IBF != 0 || status & 0x30 != 0 {
        return Err(SavedStateValidationError::UnsupportedStatusBits(status));
    }
    let kcs_state = status & STATUS_STATE_MASK;
    if !matches!(
        kcs_state,
        KCS_STATE_IDLE | KCS_STATE_READ | KCS_STATE_WRITE | KCS_STATE_ERROR
    ) {
        return Err(SavedStateValidationError::InvalidKcsState(kcs_state));
    }
    if request.len() > KCS_MESSAGE_MAX {
        return Err(SavedStateValidationError::RequestTooLong(request.len()));
    }
    if response.len() > KCS_MESSAGE_MAX {
        return Err(SavedStateValidationError::ResponseTooLong(response.len()));
    }
    let response_position = usize::try_from(response_position)
        .map_err(|_| SavedStateValidationError::NumericOverflow)?;
    if response_position > response.len() {
        return Err(SavedStateValidationError::InvalidResponsePosition {
            position: response_position,
            length: response.len(),
        });
    }
    if kcs_state == KCS_STATE_READ && response.is_empty() {
        return Err(SavedStateValidationError::EmptyReadResponse);
    }
    if write_end_pending && kcs_state != KCS_STATE_WRITE {
        return Err(SavedStateValidationError::InvalidWriteEndPending);
    }

    let sel_count =
        usize::try_from(sel_count).map_err(|_| SavedStateValidationError::NumericOverflow)?;
    if sel_count != sel_records.len() {
        return Err(SavedStateValidationError::SelCountMismatch {
            count: sel_count,
            records: sel_records.len(),
        });
    }
    if sel_count > SEL_CAPACITY {
        return Err(SavedStateValidationError::TooManySelRecords(sel_count));
    }

    let mut records = Vec::with_capacity(sel_count);
    for (index, record) in sel_records.into_iter().enumerate() {
        let length = record.len();
        let Ok(record) = <[u8; SEL_RECORD_SIZE]>::try_from(record) else {
            return Err(SavedStateValidationError::InvalidSelRecordLength { index, length });
        };
        let record_id = u16::from_le_bytes([record[0], record[1]]);
        if record_id == 0 || record_id == 0xffff {
            return Err(SavedStateValidationError::InvalidSelRecordId { index, record_id });
        }
        if records
            .iter()
            .any(|existing: &[u8; SEL_RECORD_SIZE]| existing[0..2] == record[0..2])
        {
            return Err(SavedStateValidationError::DuplicateSelRecordId(record_id));
        }
        records.push(record);
    }

    if next_record_id == 0 || next_record_id == 0xffff {
        return Err(SavedStateValidationError::InvalidNextRecordId(
            next_record_id,
        ));
    }
    if records
        .iter()
        .any(|record| u16::from_le_bytes([record[0], record[1]]) == next_record_id)
    {
        return Err(SavedStateValidationError::DuplicateNextRecordId(
            next_record_id,
        ));
    }

    let mut transaction = KcsTransaction {
        status,
        data_out,
        request_len: request.len(),
        response_len: response.len(),
        response_pos: response_position,
        write_end_pending,
        ..KcsTransaction::default()
    };
    transaction.request[..request.len()].copy_from_slice(&request);
    transaction.response[..response.len()].copy_from_slice(&response);

    Ok(RestoredState {
        transaction,
        sel: SelState {
            records,
            next_record_id,
            reservation_id,
            time_offset_seconds,
            last_erase_timestamp,
        },
    })
}
