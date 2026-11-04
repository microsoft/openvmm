// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Carries TDISP commands for a VPCI device over its vmbus channel.

use inspect::Inspect;
use mesh::rpc::RpcSend;
use openhcl_tdisp::GuestToHostCommand;
use openhcl_tdisp::GuestToHostResponse;
use openhcl_tdisp::TdispCommandTransport;
use std::future::Future;
use std::pin::Pin;
use vpci_protocol::MAX_VPCI_TDISP_COMMAND_SIZE;
use vpci_protocol::SlotNumber;

use super::WorkerRequest;

/// Sends TDISP commands to the host as VPCI packets on the device's channel.
#[derive(Inspect)]
pub(super) struct VpciTdispTransport {
    #[inspect(skip)]
    worker_req: mesh::Sender<WorkerRequest>,
    /// The VPCI slot, which both addresses the packet and identifies the TDI
    /// to the host.
    slot: u64,
}

impl VpciTdispTransport {
    /// * `worker_req` - Reaches the worker that owns the device's vmbus
    ///   channel.
    /// * `slot` - The device's VPCI slot number.
    pub(super) fn new(worker_req: mesh::Sender<WorkerRequest>, slot: u64) -> Self {
        Self { worker_req, slot }
    }
}

impl TdispCommandTransport for VpciTdispTransport {
    fn bus_device_id(&self) -> u64 {
        self.slot
    }

    fn send_command<'a>(
        &'a self,
        command: GuestToHostCommand,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<GuestToHostResponse>> + Send + Sync + 'a>> {
        Box::pin(async move {
            let serialized = openhcl_tdisp::serialize_command(&command);

            // Ensure that the length does not exceed the VMBUS maximum packet size.
            // This shouldn't be possible since the host should reject the command anyways,
            // but fail earlier for safety.
            if serialized.len() > MAX_VPCI_TDISP_COMMAND_SIZE {
                return Err(anyhow::anyhow!(
                    "serialized TDISP command exceeds VMBUS maximum packet size ({} > {})",
                    serialized.len(),
                    MAX_VPCI_TDISP_COMMAND_SIZE
                ));
            }

            // Make a mesh call to send the VMBUS packet to the host and await a response
            // packet from the host.
            self.worker_req
                .call_failable(
                    WorkerRequest::TdispCommand,
                    vpci_protocol::VpciTdispCommand {
                        header: vpci_protocol::VpciTdispCommandHeader {
                            message_type: vpci_protocol::MessageType::VPCI_TDISP_COMMAND,
                            slot: SlotNumber::from_bits(self.slot as u32),
                            data_length: serialized.len() as u64,
                        },
                        data: serialized,
                    },
                )
                .await
                .map_err(|err: mesh::rpc::RpcError<mesh::error::RemoteError>| {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "failed to send tdisp command"
                    );
                    anyhow::anyhow!("failed to send tdisp command")
                })
        })
    }
}
