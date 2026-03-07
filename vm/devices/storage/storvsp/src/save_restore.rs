// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Save/restore support for StorVSP.

use self::state::Drive;
use self::state::DriveSavedState;
use crate::Range;
use crate::ScsiPath;
use crate::ScsiRequestAndRange;
use crate::ScsiRequestState;
use crate::StorageDevice;
use crate::UnsupportedVersion;
use crate::Version;
use scsi_core::save_restore::ScsiSavedState;
use std::sync::Arc;
use thiserror::Error;
use vmbus_channel::bus::OpenRequest;
use vmbus_channel::channel::ChannelRestoreError;
use vmbus_channel::channel::RestoreControl;
use vmbus_ring::gparange::MultiPagedRangeBuf;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

mod state {
    use mesh::payload::Protobuf;
    use scsi_core::save_restore::ScsiDiskSavedState;
    use scsi_core::save_restore::ScsiDvdSavedState;
    use vmcore::save_restore::SavedStateRoot;

    #[derive(Debug, Protobuf)]
    #[mesh(package = "storage.storvsp")]
    pub struct Drive {
        #[mesh(1)]
        pub path: ScsiPath,
        #[mesh(2)]
        pub state: DriveSavedState,
    }

    #[derive(Debug, Protobuf)]
    #[mesh(package = "storage.storvsp")]
    pub enum DriveSavedState {
        #[mesh(1)]
        Disk(ScsiDiskSavedState),
        #[mesh(2)]
        Dvd(ScsiDvdSavedState),
    }

    #[derive(Debug, Protobuf, SavedStateRoot)]
    #[mesh(package = "storage.storvsp")]
    pub struct SavedState {
        /// Saved protocol state.
        #[mesh(1)]
        pub protocol_state: ProtocolState,
        /// Saved channel state.
        #[mesh(2)]
        pub channels: Vec<ChannelSavedState>,
        /// Saved states of all disks/dvds on drives.
        #[mesh(3)]
        pub drives: Vec<Drive>,
    }

    #[derive(Debug, Copy, Clone, Protobuf)]
    #[mesh(package = "storage.storvsp")]
    pub struct ScsiPath {
        #[mesh(1)]
        pub path: u8,
        #[mesh(2)]
        pub target: u8,
        #[mesh(3)]
        pub lun: u8,
    }

    #[derive(Debug, Protobuf)]
    #[mesh(package = "storage.storvsp")]
    pub enum ProtocolState {
        #[mesh(1)]
        Init(InitState),
        #[mesh(2)]
        Ready {
            #[mesh(1)]
            version: u16,
        },
    }

    #[derive(Debug, Protobuf)]
    #[mesh(package = "storage.storvsp")]
    pub enum InitState {
        #[mesh(1)]
        Begin,
        #[mesh(2)]
        QueryVersion,
        #[mesh(3)]
        QueryProperties {
            #[mesh(1)]
            version: u16,
        },
        #[mesh(4)]
        EndInitialization {
            #[mesh(1)]
            version: u16,
            #[mesh(2)]
            can_create_subchannels: bool,
        },
    }

    #[derive(Clone, Debug, Protobuf)]
    #[mesh(package = "storage.storvsp")]
    pub struct ChannelSavedState {
        #[mesh(1)]
        pub channel: Option<WorkerSavedState>,
    }

    #[derive(Clone, Debug, Protobuf)]
    #[mesh(package = "storage.storvsp")]
    pub struct WorkerSavedState {
        #[mesh(1)]
        pub scsi_requests: Vec<ScsiRequestSavedState>,
    }

    #[derive(Clone, Debug, Protobuf)]
    #[mesh(package = "storage.storvsp")]
    pub struct RangeSavedState {
        #[mesh(1)]
        pub buf: Vec<u64>,
        #[mesh(2)]
        pub count: usize,
    }

    #[derive(Clone, Debug, Protobuf)]
    #[mesh(package = "storage.storvsp")]
    pub struct ScsiRequestSavedState {
        #[mesh(1)]
        pub transaction_id: u64,
        #[mesh(2)]
        pub external_data: RangeSavedState,
        #[mesh(3)]
        pub request: Vec<u8>,
    }
}

#[derive(Debug, Error)]
enum StorvspRestoreError {
    #[error("failed to restore a vmbus channel")]
    Channel(#[from] ChannelRestoreError),
    #[error("failed to parse gpa range")]
    GpaRange(#[source] vmbus_ring::gparange::Error),
    #[error("range/request conflict")]
    RangeConflict,
    #[error("failed to create worker")]
    Worker(#[source] anyhow::Error),
    #[error("scsi request is too large")]
    RequestTooLarge,
    #[error("invalid protocol version")]
    Version(#[from] UnsupportedVersion),
    #[error("wrong number of channel states: {0}")]
    InvalidChannelCount(usize),
    #[error("unexpected scsi request in non-ready channel")]
    UnexpectedScsiRequest,
}

impl From<StorvspRestoreError> for RestoreError {
    fn from(value: StorvspRestoreError) -> Self {
        Self::InvalidSavedState(value.into())
    }
}

impl state::RangeSavedState {
    fn save(v: &MultiPagedRangeBuf) -> Self {
        Self {
            buf: v.range_buffer().to_vec(),
            count: v.range_count(),
        }
    }

    fn restore(&self) -> Result<MultiPagedRangeBuf, StorvspRestoreError> {
        MultiPagedRangeBuf::from_range_buffer(self.count, self.buf.clone())
            .map_err(StorvspRestoreError::GpaRange)
    }
}

impl From<ScsiPath> for state::ScsiPath {
    fn from(value: ScsiPath) -> Self {
        let ScsiPath { path, target, lun } = value;
        Self { path, target, lun }
    }
}

impl From<state::ScsiPath> for ScsiPath {
    fn from(value: state::ScsiPath) -> Self {
        let state::ScsiPath { path, target, lun } = value;
        Self { path, target, lun }
    }
}

impl state::ScsiRequestSavedState {
    fn save(v: &ScsiRequestState) -> Self {
        let &ScsiRequestState {
            transaction_id,
            ref request,
        } = v;
        Self {
            transaction_id,
            external_data: state::RangeSavedState::save(&request.external_data_buf),
            request: request.request.as_bytes()[..request.request_size].to_vec(),
        }
    }

    fn restore(&self) -> Result<ScsiRequestState, StorvspRestoreError> {
        let Self {
            transaction_id,
            external_data,
            request,
        } = self;

        let mut protocol_request = storvsp_protocol::ScsiRequest::new_zeroed();
        protocol_request
            .as_mut_bytes()
            .get_mut(..request.len())
            .ok_or(StorvspRestoreError::RequestTooLarge)?
            .copy_from_slice(request);

        let external_data_buf = external_data.restore()?;
        let external_data = Range::new(&external_data_buf, &protocol_request)
            .ok_or(StorvspRestoreError::RangeConflict)?;

        Ok(ScsiRequestState {
            transaction_id: *transaction_id,
            request: Arc::new(ScsiRequestAndRange {
                external_data,
                external_data_buf,
                request: protocol_request,
                request_size: request.len(),
            }),
        })
    }
}

impl StorageDevice {
    pub(super) fn save(&mut self) -> Result<state::SavedState, SaveError> {
        let drives = self.save_drives()?;
        let protocol_state = self.save_protocol_state();

        // Determine the number of subchannels that have been offered.
        let subchannel_count = match *self.protocol.state.read() {
            crate::ProtocolState::Ready {
                subchannel_count, ..
            } => subchannel_count,
            crate::ProtocolState::Init(state) => match state {
                crate::InitState::Begin
                | crate::InitState::QueryVersion
                | crate::InitState::QueryProperties { .. } => 0,
                crate::InitState::EndInitialization {
                    subchannel_count, ..
                } => subchannel_count.unwrap_or(0),
            },
        };
        let channels = self.save_workers(subchannel_count);

        Ok(state::SavedState {
            protocol_state,
            drives,
            channels,
        })
    }

    pub(super) async fn restore(
        &mut self,
        control: RestoreControl<'_>,
        state: state::SavedState,
    ) -> Result<(), RestoreError> {
        let state::SavedState {
            protocol_state,
            drives,
            channels,
        } = state;

        if channels.is_empty() || channels.len() - 1 > self.max_sub_channel_count.into() {
            return Err(StorvspRestoreError::InvalidChannelCount(channels.len()))?;
        }

        let subchannel_count = channels.len() as u16 - 1;

        self.restore_protocol_state(protocol_state, subchannel_count)
            .await?;
        self.restore_drives(&drives)?;
        self.restore_workers(control, channels).await?;
        Ok(())
    }

    /// Save all sub-channels' states. Panics if any task is running. Need be call after Stop().
    fn save_workers(&self, subchannel_count: u16) -> Vec<state::ChannelSavedState> {
        let mut states = Vec::new();
        for task in &self.workers[..subchannel_count as usize + 1] {
            if let Some(worker) = task.worker.state() {
                let state = {
                    let scsi_requests = worker
                        .inner
                        .scsi_requests_states
                        .iter()
                        .map(|(_, req)| state::ScsiRequestSavedState::save(req))
                        .collect();

                    state::WorkerSavedState { scsi_requests }
                };
                states.push(state::ChannelSavedState {
                    channel: Some(state),
                });
            } else {
                states.push(state::ChannelSavedState { channel: None });
            }
        }
        states
    }

    /// Restore all sub-channels' open states.
    async fn restore_channels(
        &mut self,
        mut control: RestoreControl<'_>,
        states: &Vec<state::ChannelSavedState>,
    ) -> Result<Vec<Option<OpenRequest>>, StorvspRestoreError> {
        let mut is_open = Vec::new();
        for channel_state in states {
            is_open.push(channel_state.channel.is_some());
        }

        Ok(control.restore(&is_open).await?)
    }

    /// Restore all sub-channels' states and underlying workers' states.
    async fn restore_workers(
        &mut self,
        control: RestoreControl<'_>,
        states: Vec<state::ChannelSavedState>,
    ) -> Result<(), StorvspRestoreError> {
        // Compute the maximum packet size for the worker. Just leave the
        // default if the version has not been negotiated yet.
        let mut ready = false;
        let version = match *self.protocol.state.read() {
            crate::ProtocolState::Init(state) => match state {
                crate::InitState::Begin => None,
                crate::InitState::QueryVersion => None,
                crate::InitState::QueryProperties { version, .. }
                | crate::InitState::EndInitialization { version, .. } => Some(version),
            },
            crate::ProtocolState::Ready { version, .. } => {
                ready = true;
                Some(version)
            }
        };

        let request_size = version.map(|v| v.max_request_size());

        let open_requests = self.restore_channels(control, &states).await?;

        for (channel_index, state::ChannelSavedState { channel }) in states.iter().enumerate() {
            let Some(channel) = channel else { continue };

            let open_request = open_requests[channel_index]
                .as_ref()
                .expect("open state mismatch");

            let worker = self
                .new_worker(open_request, channel_index as u16)
                .map_err(StorvspRestoreError::Worker)?;

            if let Some(request_size) = request_size {
                worker.inner.request_size = request_size;
            }
            for saved_state in &channel.scsi_requests {
                if !ready {
                    return Err(StorvspRestoreError::UnexpectedScsiRequest);
                }
                let state = saved_state.restore()?;
                worker
                    .inner
                    .push_scsi_request(state.transaction_id, state.request);
            }
        }
        Ok(())
    }

    /// Save drives's states.
    fn save_drives(&self) -> Result<Vec<Drive>, SaveError> {
        let mut states = Vec::new();
        let disks = self.controller.disks.read();

        for (&path, controller_disk) in disks.iter() {
            let state = controller_disk.disk.save()?;

            match state {
                Some(ScsiSavedState::ScsiDisk(scsi_saved_state)) => states.push(Drive {
                    path: path.into(),
                    state: DriveSavedState::Disk(scsi_saved_state),
                }),
                Some(ScsiSavedState::ScsiDvd(scsi_saved_state)) => states.push(Drive {
                    path: path.into(),
                    state: DriveSavedState::Dvd(scsi_saved_state),
                }),
                None => return Err(SaveError::NotSupported),
            }
        }

        Ok(states)
    }

    /// Restore drive's states.
    fn restore_drives(&mut self, drives: &Vec<Drive>) -> Result<(), RestoreError> {
        let disks = self.controller.disks.read();

        for (scsi_path, controller_disk) in disks.iter() {
            for drive in drives {
                let Drive { path, state } = drive;
                if *scsi_path == (*path).into() {
                    match state {
                        DriveSavedState::Disk(state) => controller_disk
                            .disk
                            .restore(&ScsiSavedState::ScsiDisk(*state))?,
                        DriveSavedState::Dvd(state) => controller_disk
                            .disk
                            .restore(&ScsiSavedState::ScsiDvd(*state))?,
                    }
                    break;
                }
            }
        }

        Ok(())
    }

    /// Save protocol state and packet size.
    fn save_protocol_state(&self) -> state::ProtocolState {
        match *self.protocol.state.read() {
            crate::ProtocolState::Init(init_state) => {
                state::ProtocolState::Init(match init_state {
                    crate::InitState::Begin => state::InitState::Begin,
                    crate::InitState::QueryVersion => state::InitState::QueryVersion,
                    crate::InitState::QueryProperties { version } => {
                        state::InitState::QueryProperties {
                            version: version as u16,
                        }
                    }
                    crate::InitState::EndInitialization {
                        version,
                        subchannel_count,
                    } => state::InitState::EndInitialization {
                        version: version as u16,
                        can_create_subchannels: subchannel_count.is_none(),
                    },
                })
            }
            crate::ProtocolState::Ready {
                version,
                subchannel_count: _,
            } => state::ProtocolState::Ready {
                version: version as u16,
            },
        }
    }

    /// Restore protocol state.
    async fn restore_protocol_state(
        &mut self,
        state: state::ProtocolState,
        subchannel_count: u16,
    ) -> Result<(), StorvspRestoreError> {
        let mut subchannels_allowed = false;
        let state = match state {
            state::ProtocolState::Init(init_state) => {
                crate::ProtocolState::Init(match init_state {
                    state::InitState::Begin => crate::InitState::Begin,
                    state::InitState::QueryVersion => crate::InitState::QueryVersion,
                    state::InitState::QueryProperties { version } => {
                        crate::InitState::QueryProperties {
                            version: Version::parse(version)?,
                        }
                    }
                    state::InitState::EndInitialization {
                        version,
                        can_create_subchannels,
                    } => {
                        let subchannel_count = if can_create_subchannels {
                            None
                        } else {
                            subchannels_allowed = true;
                            Some(subchannel_count)
                        };
                        crate::InitState::EndInitialization {
                            version: Version::parse(version)?,
                            subchannel_count,
                        }
                    }
                })
            }
            state::ProtocolState::Ready { version } => {
                let version = Version::parse(version)?;
                subchannels_allowed = true;
                crate::ProtocolState::Ready {
                    version,
                    subchannel_count,
                }
            }
        };
        if subchannel_count != 0 && !subchannels_allowed {
            return Err(StorvspRestoreError::InvalidChannelCount(
                subchannel_count as usize + 1,
            ));
        }
        *self.protocol.state.write() = state;
        Ok(())
    }
}
