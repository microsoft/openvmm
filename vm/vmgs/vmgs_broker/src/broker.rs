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

#[derive(Protobuf, Error, Debug)]
pub enum VmgsBrokerError {
    #[error("no allocated bytes for file id being read")]
    FileInfoNotAllocated,
    #[error(transparent)]
    Other(RemoteError),
}

impl From<vmgs::Error> for VmgsBrokerError {
    fn from(value: vmgs::Error) -> Self {
        match value {
            vmgs::Error::FileInfoNotAllocated => VmgsBrokerError::FileInfoNotAllocated,
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
    #[cfg(with_encryption)]
    WriteFileEncrypted(Rpc<(BrokerFileId, Vec<u8>), Result<(), VmgsBrokerError>>),
    Save(Rpc<(), vmgs::save_restore::state::SavedVmgsState>),
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
            #[cfg(with_encryption)]
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
        }
    }
}
