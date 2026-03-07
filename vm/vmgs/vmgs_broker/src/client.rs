// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The Vmgs worker will send messages to the Vmgs dispatch, allowing
//! tasks to queue for the dispatcher to handle synchronously

use crate::broker::VmgsBrokerError;
use crate::broker::VmgsBrokerRpc;
use inspect::Inspect;
use mesh::MeshPayload;
use mesh::rpc::RpcError;
use mesh::rpc::RpcSend;
use thiserror::Error;
use tracing::instrument;
use vmgs::VmgsFileInfo;
use vmgs_format::FileId;

/// VMGS broker errors.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum VmgsClientError {
    /// VMGS broker is offline
    #[error("broker is offline")]
    BrokerOffline(#[source] RpcError),
    /// VMGS error
    #[error("vmgs error")]
    Vmgs(#[source] VmgsBrokerError),
}

impl From<RpcError> for VmgsClientError {
    fn from(value: RpcError) -> Self {
        match value {
            RpcError::Channel(e) => VmgsClientError::BrokerOffline(RpcError::Channel(e)),
        }
    }
}

impl From<RpcError<VmgsBrokerError>> for VmgsClientError {
    fn from(value: RpcError<VmgsBrokerError>) -> Self {
        match value {
            RpcError::Call(e) => VmgsClientError::Vmgs(e),
            RpcError::Channel(e) => VmgsClientError::BrokerOffline(RpcError::Channel(e)),
        }
    }
}

/// Client to interact with a backend-agnostic VMGS instance.
#[derive(Clone, Inspect, MeshPayload)]
pub struct VmgsClient {
    #[inspect(flatten, send = "VmgsBrokerRpc::Inspect")]
    pub(crate) control: mesh::Sender<VmgsBrokerRpc>,
}

impl VmgsClient {
    /// Get allocated and valid bytes from File Control Block for file_id.
    #[instrument(skip_all, fields(file_id))]
    pub async fn get_file_info(&self, file_id: FileId) -> Result<VmgsFileInfo, VmgsClientError> {
        let res = self
            .control
            .call_failable(VmgsBrokerRpc::GetFileInfo, file_id.into())
            .await?;

        Ok(res)
    }

    /// Reads the specified `file_id`.
    #[instrument(skip_all, fields(file_id))]
    pub async fn read_file(&self, file_id: FileId) -> Result<Vec<u8>, VmgsClientError> {
        let res = self
            .control
            .call_failable(VmgsBrokerRpc::ReadFile, file_id.into())
            .await?;

        Ok(res)
    }

    /// Writes `buf` to a file_id.
    ///
    /// NOTE: It is an error to overwrite a previously encrypted FileId with
    /// plaintext data.
    #[instrument(skip_all, fields(file_id))]
    pub async fn write_file(&self, file_id: FileId, buf: Vec<u8>) -> Result<(), VmgsClientError> {
        self.control
            .call_failable(VmgsBrokerRpc::WriteFile, (file_id.into(), buf))
            .await?;

        Ok(())
    }

    /// If VMGS has been configured with encryption, encrypt + write `bug` to
    /// the specified `file_id`. Otherwise, perform a regular plaintext write
    /// instead.
    #[cfg(with_encryption)]
    #[instrument(skip_all, fields(file_id))]
    pub async fn write_file_encrypted(
        &self,
        file_id: FileId,
        buf: Vec<u8>,
    ) -> Result<(), VmgsClientError> {
        self.control
            .call_failable(VmgsBrokerRpc::WriteFileEncrypted, (file_id.into(), buf))
            .await?;

        Ok(())
    }

    /// Save the in-memory VMGS file metadata.
    ///
    /// This saved state can be used alongside `open_from_saved` to obtain a
    /// new `Vmgs` instance _without_ needing to invoke any IOs on the
    /// underlying storage.
    pub async fn save(&self) -> Result<vmgs::save_restore::state::SavedVmgsState, VmgsClientError> {
        let res = self.control.call(VmgsBrokerRpc::Save, ()).await?;
        Ok(res)
    }
}
