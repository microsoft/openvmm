// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core SCSI traits and types.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use inspect::Inspect;
use scsi_buffers::RequestBuffers;
use scsi_defs::srb::SrbStatus;
use scsi_defs::ScsiOp;
use scsi_defs::ScsiStatus;
use stackfuture::StackFuture;
use std::sync::Arc;
use vm_resource::kind::ScsiDeviceHandleKind;
use vm_resource::CanResolveTo;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::vm_task::VmTaskDriverSource;

impl CanResolveTo<ResolvedScsiDevice> for ScsiDeviceHandleKind {
    type Input<'a> = ResolveScsiDeviceHandleParams<'a>;
}

/// A resolved [`AsyncScsiDisk`].
pub struct ResolvedScsiDevice(pub Arc<dyn AsyncScsiDisk>);

impl<T: 'static + AsyncScsiDisk> From<T> for ResolvedScsiDevice {
    fn from(value: T) -> Self {
        Self(Arc::new(value))
    }
}

/// Parameters used when ersolving [`ScsiDeviceHandleKind`].
pub struct ResolveScsiDeviceHandleParams<'a> {
    /// The VM task driver source.
    pub driver_source: &'a VmTaskDriverSource,
}

/// The amount of space reserved for an AsyncScsiDisk-returned future
///
/// This was chosen by running `cargo test --package scsidisk --lib -- --exact --nocapture` and looking at the required
/// size that was given in the failure message
pub const ASYNC_SCSI_DISK_STACK_SIZE: usize = 1256 + 336;

/// Trait for issuing SCSI device requests.
pub trait AsyncScsiDisk: Send + Sync + Inspect + ScsiSaveRestore {
    /// Executes a SCSI request.
    fn execute_scsi<'a>(
        &'a self,
        external_data: &'a RequestBuffers<'a>,
        request: &'a Request,
    ) -> StackFuture<'a, ScsiResult, { ASYNC_SCSI_DISK_STACK_SIZE }>;
}

/// A SCSI request.
#[derive(Debug)]
pub struct Request {
    /// The SCSI CDB (Command Descriptor Block).
    pub cdb: [u8; 0x10],
    /// Additional flags from the SCSI request block.
    ///
    /// TODO: interpret these OOB flags in storvsp, not in the SCSI implementations.
    pub srb_flags: u32,
}

impl Request {
    /// Returns the SCSI request operation from the CDB.
    pub fn scsiop(&self) -> ScsiOp {
        ScsiOp(self.cdb[0])
    }
}

/// The result of a SCSI request.
#[derive(Debug)]
pub struct ScsiResult {
    /// The SCSI status.
    pub scsi_status: ScsiStatus,
    /// The SRB status.
    ///
    /// TODO: move computation of this to storvsp.
    pub srb_status: SrbStatus,
    /// The number of bytes that were transferred.
    pub tx: usize,
    /// The sense data for a failed request.
    pub sense_data: Option<scsi_defs::SenseData>,
}

/// Trait to save/restore SCSI devices.
pub trait ScsiSaveRestore {
    /// Save the device state.
    fn save(&self) -> Result<Option<save_restore::ScsiSavedState>, SaveError>;
    /// Restore the device state.
    fn restore(&self, state: &save_restore::ScsiSavedState) -> Result<(), RestoreError>;
}

pub mod save_restore {
    //! SCSI device saved state definitions.

    #![allow(missing_docs)]

    use mesh::payload::Protobuf;

    #[derive(Debug, Copy, Clone, Eq, PartialEq, Protobuf)]
    #[mesh(package = "storage.scsi.common")]
    pub struct SavedSenseData {
        #[mesh(1)]
        pub sense_key: u8,
        #[mesh(2)]
        pub additional_sense_code: u8,
        #[mesh(3)]
        pub additional_sense_code_qualifier: u8,
    }

    #[derive(Debug, Copy, Clone, Protobuf)]
    #[mesh(package = "storage.scsi.disk")]
    pub struct ScsiDiskSavedState {
        #[mesh(1)]
        pub sector_count: u64,
        #[mesh(2)]
        pub sense_data: Option<SavedSenseData>,
    }

    #[derive(Debug, Protobuf, Copy, Clone)]
    #[mesh(package = "storage.scsi")]
    pub enum ScsiSavedState {
        #[mesh(1)]
        ScsiDvd(ScsiDvdSavedState),
        #[mesh(2)]
        ScsiDisk(ScsiDiskSavedState),
    }

    #[derive(Debug, Protobuf, Copy, Clone)]
    #[mesh(package = "storage.scsi.dvd")]
    pub struct ScsiDvdSavedState {
        #[mesh(1)]
        pub sense_data: Option<SavedSenseData>,
        #[mesh(2)]
        pub persistent: bool,
        #[mesh(3)]
        pub prevent: bool,
        #[mesh(4)]
        pub drive_state: DriveState,
        #[mesh(5)]
        pub pending_medium_event: IsoMediumEvent,
    }

    #[derive(Debug, Default, PartialEq, Eq, Copy, Clone, inspect::Inspect, Protobuf)]
    #[allow(clippy::enum_variant_names)]
    #[mesh(package = "storage.scsi.dvd")]
    pub enum DriveState {
        #[mesh(1)]
        MediumPresentTrayOpen,
        #[mesh(2)]
        MediumPresentTrayClosed,
        #[mesh(3)]
        MediumNotPresentTrayOpen,
        #[default]
        #[mesh(4)]
        MediumNotPresentTrayClosed,
    }

    impl DriveState {
        pub fn tray_open(&self) -> bool {
            *self == DriveState::MediumPresentTrayOpen
                || *self == DriveState::MediumNotPresentTrayOpen
        }

        pub fn medium_present(&self) -> bool {
            *self == DriveState::MediumPresentTrayOpen
                || *self == DriveState::MediumPresentTrayClosed
        }
    }

    #[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Protobuf)]
    #[mesh(package = "storage.scsi.dvd")]
    pub enum IsoMediumEvent {
        #[default]
        #[mesh(1)]
        None = 0x00,
        #[mesh(2)]
        NoMediaToMedia = 0x01,
        #[mesh(3)]
        MediaToNoMedia = 0x02,
        #[mesh(4)]
        MediaToMedia = 0x03,
    }
}
