// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::OpenHclServicingFlags;
use get_resources::ged::GuestServicingFlags;
use hvlite_defs::config::Config;
use hvlite_defs::rpc::PulseSaveRestoreError;
use hvlite_defs::rpc::VmRpc;
use hvlite_defs::worker::VmWorkerParameters;
use hvlite_defs::worker::VM_WORKER;
use mesh::rpc::RpcError;
use mesh::rpc::RpcSend;
use mesh_worker::WorkerHandle;
use mesh_worker::WorkerHost;
use vmm_core_defs::HaltReason;

pub(crate) struct Worker {
    handle: WorkerHandle,
    rpc: mesh::Sender<VmRpc>,
}

impl Worker {
    pub(crate) async fn launch(
        host: &WorkerHost,
        cfg: Config,
    ) -> anyhow::Result<(Self, mesh::Receiver<HaltReason>)> {
        let (vm_rpc, rpc_recv) = mesh::channel();
        let (notify_send, notify_recv) = mesh::channel();

        let params = VmWorkerParameters {
            hypervisor: None,
            cfg,
            saved_state: None,
            rpc: rpc_recv,
            notify: notify_send,
        };
        let vm_worker = host.launch_worker(VM_WORKER, params).await?;

        Ok((
            Self {
                handle: vm_worker,
                rpc: vm_rpc,
            },
            notify_recv,
        ))
    }

    pub(crate) async fn resume(&self) -> Result<bool, RpcError> {
        self.rpc.call(VmRpc::Resume, ()).await
    }

    pub(crate) async fn reset(&self) -> anyhow::Result<()> {
        self.rpc.call(VmRpc::Reset, ()).await??;
        Ok(())
    }

    pub(crate) async fn pulse_save_restore(&self) -> Result<(), RpcError<PulseSaveRestoreError>> {
        self.rpc.call_failable(VmRpc::PulseSaveRestore, ()).await
    }

    pub(crate) async fn restart_openhcl(
        &self,
        send: &mesh::Sender<get_resources::ged::GuestEmulationRequest>,
        flags: OpenHclServicingFlags,
        file: std::fs::File,
    ) -> anyhow::Result<()> {
        hvlite_helpers::underhill::service_underhill(
            &self.rpc,
            send,
            GuestServicingFlags {
                nvme_keepalive: flags.enable_nvme_keepalive,
            },
            file,
        )
        .await
    }

    pub(crate) async fn inspect_all(&self) -> String {
        let mut inspection = inspect::inspect("", &self.handle);
        inspection.resolve().await;
        let results = inspection.results();
        format!("{results:#}",)
    }

    pub(crate) async fn shutdown(mut self) -> anyhow::Result<()> {
        self.handle.stop();
        self.handle.join().await?;
        Ok(())
    }
}
