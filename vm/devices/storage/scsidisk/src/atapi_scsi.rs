// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements ATAPI SCSI command handler for an IDE CD_ROM, wrapping a
//! [`AsyncScsiDisk`].
//!

use crate::illegal_request_sense;
use crate::SenseDataSlot;
use guestmem::MemoryWrite;
use inspect::Inspect;
use scsi::srb::SrbStatus;
use scsi::AdditionalSenseCode;
use scsi::ScsiOp;
use scsi::ScsiStatus;
use scsi::SenseKey;
use scsi_buffers::RequestBuffers;
use scsi_core::save_restore::SavedSenseData;
use scsi_core::save_restore::ScsiDvdSavedState;
use scsi_core::save_restore::ScsiSavedState;
use scsi_core::AsyncScsiDisk;
use scsi_core::Request;
use scsi_core::ScsiResult;
use scsi_core::ScsiSaveRestore;
use scsi_core::ASYNC_SCSI_DISK_STACK_SIZE;
use scsi_defs as scsi;
use stackfuture::StackFuture;
use std::sync::Arc;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

/// A wrapper to filter and redirect ATAPI SCSI commands from an IDE ISO to inner [`AsyncScsiDisk`].
#[derive(Inspect)]
pub struct AtapiScsiDisk {
    #[inspect(flatten)]
    inner: Arc<dyn AsyncScsiDisk>,
    #[inspect(skip)]
    sense_data: SenseDataSlot,
}

impl ScsiSaveRestore for AtapiScsiDisk {
    fn save(&self) -> Result<Option<ScsiSavedState>, SaveError> {
        let sense = self.sense_data.get();
        let sense_data = sense.map(|sense| SavedSenseData {
            sense_key: sense.header.sense_key.0,
            additional_sense_code: sense.additional_sense_code.0,
            additional_sense_code_qualifier: sense.additional_sense_code_qualifier,
        });
        let state = self.inner.save()?.unwrap();
        match state {
            ScsiSavedState::ScsiDvd(mut state) => {
                state.sense_data = sense_data;
                Ok(Some(ScsiSavedState::ScsiDvd(state)))
            }
            _ => Err(SaveError::InvalidChildSavedState(anyhow::anyhow!(
                "saved state didn't match expected ScsiSavedState::ScsiDvd"
            ))),
        }
    }

    fn restore(&self, state: &ScsiSavedState) -> Result<(), RestoreError> {
        self.inner.restore(state)?;
        match state {
            ScsiSavedState::ScsiDvd(state) => {
                let ScsiDvdSavedState { sense_data, .. } = state;
                // restore sense data
                self.sense_data.set(
                    sense_data
                        .map(|sense| {
                            scsi::SenseData::new(
                                SenseKey(sense.sense_key),
                                AdditionalSenseCode(sense.additional_sense_code),
                                sense.additional_sense_code_qualifier,
                            )
                        })
                        .as_ref(),
                );

                Ok(())
            }
            _ => Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                "saved state didn't match expected ScsiSavedState::ScsiDvd"
            ))),
        }
    }
}

impl AsyncScsiDisk for AtapiScsiDisk {
    fn execute_scsi<'a>(
        &'a self,
        external_data: &'a RequestBuffers<'a>,
        request: &'a Request,
    ) -> StackFuture<'a, ScsiResult, { ASYNC_SCSI_DISK_STACK_SIZE }> {
        StackFuture::from_or_box(async move {
            let op = request.scsiop();
            let result = match op {
                ScsiOp::READ
                | ScsiOp::READ12
                | ScsiOp::TEST_UNIT_READY
                | ScsiOp::READ_TOC
                | ScsiOp::INQUIRY
                | ScsiOp::GET_CONFIGURATION
                | ScsiOp::MODE_SENSE
                | ScsiOp::MODE_SENSE10
                | ScsiOp::READ_DVD_STRUCTURE
                | ScsiOp::READ_CAPACITY
                | ScsiOp::GET_EVENT_STATUS
                | ScsiOp::MEDIUM_REMOVAL
                | ScsiOp::START_STOP_UNIT => self.inner.execute_scsi(external_data, request).await,
                ScsiOp::REQUEST_SENSE => self.atapi_request_sense(external_data, request),
                ScsiOp::REPORT_LUNS => self.atapi_report_luns(external_data, request),
                ScsiOp::SEEK => {
                    // NO-OP's
                    self.atapi_noop()
                }
                _ => self.atapi_illegal_cmd(),
            };

            self.sense_data.set(result.sense_data.as_ref());
            tracing::debug!(scsi_result = ?result, ?op, "Atapi execute_scsi_async.");
            result
        })
    }
}

impl AtapiScsiDisk {
    pub fn new(disk: Arc<dyn AsyncScsiDisk>) -> Self {
        AtapiScsiDisk {
            inner: disk,
            sense_data: Default::default(),
        }
    }

    fn atapi_request_sense(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> ScsiResult {
        let cdb = scsi::CdbInquiry::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let allocation_length = cdb.allocation_length.get() as usize;

        let min = size_of::<scsi::SenseDataHeader>();
        if allocation_length < min || allocation_length > external_data.len() {
            tracelimit::error_ratelimited!(
                allocation_length,
                min,
                external_data_len = external_data.len(),
                "srb error"
            );
            return ScsiResult {
                scsi_status: ScsiStatus::CHECK_CONDITION,
                srb_status: SrbStatus::ERROR,
                tx: 0,
                sense_data: None,
            };
        }

        let sense = self.sense_data.take().unwrap_or_else(|| {
            scsi::SenseData::new(SenseKey::NO_SENSE, AdditionalSenseCode::NO_SENSE, 0x00)
        });

        let tx = std::cmp::min(allocation_length, size_of::<scsi::SenseData>());
        let result = external_data.writer().write(&sense.as_bytes()[..tx]);

        match result {
            Err(err) => {
                tracelimit::error_ratelimited!(
                    ?err,
                    "SCSIOP_REQUEST_SENSE hit memory access error"
                );
                ScsiResult {
                    scsi_status: ScsiStatus::CHECK_CONDITION,
                    srb_status: SrbStatus::INVALID_REQUEST,
                    tx: 0,
                    sense_data: Some(illegal_request_sense(AdditionalSenseCode::INVALID_CDB)),
                }
            }
            Ok(_) => ScsiResult {
                scsi_status: ScsiStatus::GOOD,
                srb_status: SrbStatus::SUCCESS,
                tx,
                sense_data: None,
            },
        }
    }

    fn atapi_report_luns(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> ScsiResult {
        let cdb = scsi::ReportLuns::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let allocation_length = cdb.allocation_length.get() as usize;
        if allocation_length == 0 {
            return ScsiResult {
                scsi_status: ScsiStatus::GOOD,
                srb_status: SrbStatus::SUCCESS,
                tx: 0,
                sense_data: None,
            };
        }

        // Return LUN list with only one LUN ID 0
        let mut data: Vec<u64> = vec![0; 2];
        let tx = data.as_bytes().len();
        if allocation_length < tx || allocation_length > external_data.len() {
            tracelimit::error_ratelimited!(
                allocation_length,
                tx,
                external_data_len = external_data.len(),
                "srb error"
            );
            return ScsiResult {
                scsi_status: ScsiStatus::CHECK_CONDITION,
                srb_status: SrbStatus::ERROR,
                tx: 0,
                sense_data: None,
            };
        }

        const HEADER_SIZE: usize = size_of::<scsi::LunList>();
        let header = scsi::LunList {
            length: 8.into(),
            reserved: [0; 4],
        };
        data.as_mut_bytes()[..HEADER_SIZE].copy_from_slice(header.as_bytes());
        data[1].as_mut_bytes()[..2].copy_from_slice(&0_u16.to_be_bytes());

        let result = external_data.writer().write(&data.as_bytes()[..tx]);

        match result {
            Err(err) => {
                tracelimit::error_ratelimited!(?err, "SCSIOP_REPORT_LUNS hit memory access error");
                ScsiResult {
                    scsi_status: ScsiStatus::CHECK_CONDITION,
                    srb_status: SrbStatus::INVALID_REQUEST,
                    tx: 0,
                    sense_data: Some(illegal_request_sense(AdditionalSenseCode::INVALID_CDB)),
                }
            }
            Ok(_) => ScsiResult {
                scsi_status: ScsiStatus::GOOD,
                srb_status: SrbStatus::SUCCESS,
                tx,
                sense_data: None,
            },
        }
    }

    fn atapi_noop(&self) -> ScsiResult {
        ScsiResult {
            scsi_status: ScsiStatus::GOOD,
            srb_status: SrbStatus::SUCCESS,
            tx: 0,
            sense_data: None,
        }
    }

    fn atapi_illegal_cmd(&self) -> ScsiResult {
        ScsiResult {
            scsi_status: ScsiStatus::CHECK_CONDITION,
            srb_status: SrbStatus::INVALID_REQUEST,
            tx: 0,
            sense_data: Some(illegal_request_sense(AdditionalSenseCode::ILLEGAL_COMMAND)),
        }
    }
}
