// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mesh::MeshPayload;
use mesh::Receiver;
use mesh::error::RemoteError;
use mesh::rpc::Rpc;
use vmgs::Vmgs;
use vmgs::VmgsFileInfo;
use vmgs_format::FileId;

#[derive(MeshPayload)]
pub enum VmgsBrokerRpc {
    Inspect(inspect::Deferred),
    GetFileInfo(Rpc<FileId, Result<VmgsFileInfo, RemoteError>>),
    ReadFile(Rpc<FileId, Result<Vec<u8>, RemoteError>>),
    WriteFile(Rpc<(FileId, Vec<u8>), Result<(), RemoteError>>),
    #[cfg(with_encryption)]
    WriteFileEncrypted(Rpc<(FileId, Vec<u8>), Result<(), RemoteError>>),
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
                .handle_sync(|file_id| self.vmgs.get_file_info(file_id).map_err(RemoteError::new)),
            VmgsBrokerRpc::ReadFile(rpc) => {
                rpc.handle(async |file_id| {
                    self.vmgs.read_file(file_id).await.map_err(RemoteError::new)
                })
                .await
            }
            VmgsBrokerRpc::WriteFile(rpc) => {
                rpc.handle(async |(file_id, buf)| {
                    self.vmgs
                        .write_file(file_id, &buf)
                        .await
                        .map_err(RemoteError::new)
                })
                .await
            }
            #[cfg(with_encryption)]
            VmgsBrokerRpc::WriteFileEncrypted(rpc) => {
                rpc.handle(async |(file_id, buf)| {
                    self.vmgs
                        .write_file_encrypted(file_id, &buf)
                        .await
                        .map_err(RemoteError::new)
                })
                .await
            }
            VmgsBrokerRpc::Save(rpc) => rpc.handle_sync(|()| self.vmgs.save()),
        }
    }
}
