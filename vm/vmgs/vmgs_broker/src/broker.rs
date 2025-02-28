// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mesh_channel::rpc::Rpc;
use mesh_channel::Receiver;
use vmgs::Vmgs;
use vmgs::VmgsFileInfo;
use vmgs_format::FileId;

pub enum VmgsBrokerRpc {
    Inspect(inspect::Deferred),
    GetFileInfo(Rpc<FileId, Result<VmgsFileInfo, vmgs::Error>>),
    ReadFile(Rpc<FileId, Result<Vec<u8>, vmgs::Error>>),
    WriteFile(Rpc<(FileId, Vec<u8>), Result<(), vmgs::Error>>),
    #[cfg(with_encryption)]
    WriteFileEncrypted(Rpc<(FileId, Vec<u8>), Result<(), vmgs::Error>>),
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
            VmgsBrokerRpc::GetFileInfo(rpc) => {
                rpc.handle_sync(|file_id| self.vmgs.get_file_info(file_id))
            }
            VmgsBrokerRpc::ReadFile(rpc) => {
                rpc.handle(async |file_id| self.vmgs.read_file(file_id).await)
                    .await
            }
            VmgsBrokerRpc::WriteFile(rpc) => {
                rpc.handle(async |(file_id, buf)| self.vmgs.write_file(file_id, &buf).await)
                    .await
            }
            #[cfg(with_encryption)]
            VmgsBrokerRpc::WriteFileEncrypted(rpc) => {
                rpc.handle(async |(file_id, buf)| {
                    self.vmgs.write_file_encrypted(file_id, &buf).await
                })
                .await
            }
            VmgsBrokerRpc::Save(rpc) => rpc.handle_sync(|()| self.vmgs.save()),
        }
    }
}
