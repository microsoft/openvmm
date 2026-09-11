// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mesh::MeshPayload;
use mesh::Receiver;
use mesh::error::RemoteError;
use mesh::payload::Protobuf;
use mesh::rpc::Rpc;
use thiserror::Error;
use vmgs::Vmgs;
use vmgs::VmgsFileInfo;
use vmgs_format::FileId;

#[cfg(all(test, feature = "encryption"))]
mod tests;

/// An error returned by a VMGS broker operation.
#[derive(Protobuf, Error, Debug)]
pub enum VmgsBrokerError {
    /// The requested file has no allocated bytes (i.e. does not exist).
    #[error("no allocated bytes for file id being read")]
    FileInfoNotAllocated,
    /// Another VMGS error.
    #[error(transparent)]
    Other(RemoteError),
}

impl From<vmgs::Error> for VmgsBrokerError {
    fn from(value: vmgs::Error) -> Self {
        match value {
            vmgs::Error::FileInfoNotAllocated(_) => VmgsBrokerError::FileInfoNotAllocated,
            other => VmgsBrokerError::Other(RemoteError::new(other)),
        }
    }
}

#[derive(Protobuf)]
pub struct BrokerFileId(u32);

impl From<FileId> for BrokerFileId {
    fn from(value: FileId) -> Self {
        BrokerFileId(value.0)
    }
}

impl From<BrokerFileId> for FileId {
    fn from(value: BrokerFileId) -> Self {
        FileId(value.0)
    }
}

#[derive(MeshPayload)]
pub enum VmgsBrokerRpc {
    Inspect(inspect::Deferred),
    GetFileInfo(Rpc<BrokerFileId, Result<VmgsFileInfo, VmgsBrokerError>>),
    ReadFile(Rpc<BrokerFileId, Result<Vec<u8>, VmgsBrokerError>>),
    WriteFile(Rpc<(BrokerFileId, Vec<u8>), Result<(), VmgsBrokerError>>),
    #[cfg(feature = "encryption")]
    WriteFileEncrypted(Rpc<(BrokerFileId, Vec<u8>), Result<(), VmgsBrokerError>>),
    Save(Rpc<(), vmgs::save_restore::state::SavedVmgsState>),
    DeleteFile(Rpc<BrokerFileId, Result<(), VmgsBrokerError>>),
    // These payloads contain sensitive keys. Do not derive Debug or Inspect.
    #[cfg(feature = "encryption")]
    ActiveEncryptionKey(Rpc<(), Result<[u8; 32], VmgsBrokerError>>),
    #[cfg(feature = "encryption")]
    WriteFileIfActiveKeyMatches(
        Rpc<(BrokerFileId, Vec<u8>, [u8; 32]), Result<bool, VmgsBrokerError>>,
    ),
}

pub struct VmgsBrokerTask {
    vmgs: Vmgs,
}

impl VmgsBrokerTask {
    /// Initialize the data store with the underlying block storage interface.
    pub fn new(vmgs: Vmgs) -> VmgsBrokerTask {
        VmgsBrokerTask { vmgs }
    }

    pub async fn run(&mut self, mut recv: Receiver<VmgsBrokerRpc>) {
        loop {
            match recv.recv().await {
                Ok(message) => self.process_message(message).await,
                Err(_) => return, // all mpsc senders went away
            }
        }
    }

    async fn process_message(&mut self, message: VmgsBrokerRpc) {
        match message {
            VmgsBrokerRpc::Inspect(req) => {
                req.inspect(&self.vmgs);
            }
            VmgsBrokerRpc::GetFileInfo(rpc) => rpc
                .handle_sync(|file_id| self.vmgs.get_file_info(file_id.into()).map_err(Into::into)),
            VmgsBrokerRpc::ReadFile(rpc) => {
                rpc.handle(async |file_id| {
                    self.vmgs
                        .read_file(file_id.into())
                        .await
                        .map_err(Into::into)
                })
                .await
            }
            VmgsBrokerRpc::WriteFile(rpc) => {
                rpc.handle(async |(file_id, buf)| {
                    self.vmgs
                        .write_file(file_id.into(), &buf)
                        .await
                        .map_err(Into::into)
                })
                .await
            }
            #[cfg(feature = "encryption")]
            VmgsBrokerRpc::WriteFileEncrypted(rpc) => {
                rpc.handle(async |(file_id, buf)| {
                    self.vmgs
                        .write_file_encrypted(file_id.into(), &buf)
                        .await
                        .map_err(Into::into)
                })
                .await
            }
            VmgsBrokerRpc::Save(rpc) => rpc.handle_sync(|()| self.vmgs.save()),
            #[cfg(feature = "encryption")]
            VmgsBrokerRpc::ActiveEncryptionKey(rpc) => rpc.handle_sync(|()| {
                self.vmgs
                    .active_encryption_key()
                    .copied()
                    .map_err(Into::into)
            }),
            #[cfg(feature = "encryption")]
            VmgsBrokerRpc::WriteFileIfActiveKeyMatches(rpc) => {
                rpc.handle(async |(file_id, buf, expected_key)| {
                    // Keep the comparison, write, and final flush in this one
                    // serially processed request: no intervening broker RPC
                    // may change the active key after the comparison.
                    let active_key = self.vmgs.active_encryption_key()?;
                    if !constant_time_eq::constant_time_eq_32(active_key, &expected_key) {
                        return Ok(false);
                    }

                    self.vmgs.write_file(file_id.into(), &buf).await?;
                    self.vmgs.flush().await?;
                    Ok(true)
                })
                .await
            }
            VmgsBrokerRpc::DeleteFile(rpc) => {
                rpc.handle(async |file_id| {
                    self.vmgs
                        .delete_file(file_id.into())
                        .await
                        .map_err(Into::into)
                })
                .await
            }
        }
    }
}
