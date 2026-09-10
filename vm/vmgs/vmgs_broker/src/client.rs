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
    #[instrument(skip_all, fields(file_id = %file_id))]
    pub async fn get_file_info(&self, file_id: FileId) -> Result<VmgsFileInfo, VmgsClientError> {
        let res = self
            .control
            .call_failable(VmgsBrokerRpc::GetFileInfo, file_id.into())
            .await?;

        Ok(res)
    }

    /// Reads the specified `file_id`.
    #[instrument(skip_all, fields(file_id = %file_id))]
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
    #[instrument(skip_all, fields(file_id = %file_id))]
    pub async fn write_file(&self, file_id: FileId, buf: Vec<u8>) -> Result<(), VmgsClientError> {
        self.control
            .call_failable(VmgsBrokerRpc::WriteFile, (file_id.into(), buf))
            .await?;

        Ok(())
    }

    /// Returns a copy of the active root encryption key of an unlocked VMGS.
    ///
    /// This key is sensitive and must not be logged or inspected. It may become
    /// stale after this call; use [`Self::write_file_if_encryption_key_matches`]
    /// to publish data that depends on it.
    #[cfg(feature = "encryption")]
    #[instrument(skip_all)]
    pub async fn active_encryption_key(&self) -> Result<[u8; 32], VmgsClientError> {
        let key = self
            .control
            .call_failable(VmgsBrokerRpc::ActiveEncryptionKey, ())
            .await?;
        Ok(key)
    }

    /// Writes plaintext `buf` only if `expected_key` is still the active root key.
    ///
    /// The comparison, write, and flush are processed as one serial broker
    /// operation. Returns `false` for a stale key without writing or flushing,
    /// and `true` only after both the write and final flush succeed. A locked or
    /// plaintext store returns an error, as does overwriting an encrypted file.
    /// A flush error does not roll back a completed write.
    #[cfg(feature = "encryption")]
    #[instrument(skip_all, fields(file_id = %file_id))]
    pub async fn write_file_if_encryption_key_matches(
        &self,
        file_id: FileId,
        buf: Vec<u8>,
        expected_key: [u8; 32],
    ) -> Result<bool, VmgsClientError> {
        let written = self
            .control
            .call_failable(
                VmgsBrokerRpc::WriteFileIfEncryptionKeyMatches,
                (file_id.into(), buf, expected_key),
            )
            .await?;
        Ok(written)
    }

    /// Deletes the specified `file_id`.
    #[instrument(skip_all, fields(file_id = %file_id))]
    pub async fn delete_file(&self, file_id: FileId) -> Result<(), VmgsClientError> {
        self.control
            .call_failable(VmgsBrokerRpc::DeleteFile, file_id.into())
            .await?;

        Ok(())
    }

    /// If VMGS has been configured with encryption, encrypt + write `buf` to
    /// the specified `file_id`. Otherwise, perform a regular plaintext write
    /// instead.
    #[cfg(feature = "encryption")]
    #[instrument(skip_all, fields(file_id = %file_id))]
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
