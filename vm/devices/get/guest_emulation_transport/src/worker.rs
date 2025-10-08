// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::client::GuestEmulationTransportClient;
use super::process_loop::ProcessLoop;
use super::process_loop::msg::Msg;
use crate::FatalGetError;
use crate::process_loop::FatalError;
use pal_async::task::Spawn;
use tracing_helpers::ErrorValueExt;
use vmbus_async::pipe::MessagePipe;
use vmbus_ring::RingMem;

#[derive(Debug)]
pub(super) struct GuestEmulationTransportWorker {
    control: mesh::Sender<Msg>,
    version: get_protocol::ProtocolVersion,
}

impl GuestEmulationTransportWorker {
    pub async fn new(
        driver: impl pal_async::driver::SpawnDriver,
    ) -> Result<(Self, pal_async::task::Task<Result<(), FatalGetError>>), FatalError> {
        let pipe = vmbus_user_channel::message_pipe(
            &driver,
            vmbus_user_channel::open_uio_device(&get_protocol::GUEST_EMULATION_INTERFACE_INSTANCE)
                .map_err(FatalError::OpenPipe)?,
        )
        .map_err(FatalError::OpenPipe)?;
        GuestEmulationTransportWorker::with_pipe(driver, pipe).await
    }

    pub async fn with_pipe(
        spawn: impl Spawn,
        pipe: MessagePipe<impl 'static + RingMem + Sync>,
    ) -> Result<(Self, pal_async::task::Task<Result<(), FatalGetError>>), FatalError> {
        let (version_send, version_recv) = mesh::oneshot();
        let (control_send, control_recv) = mesh::channel();

        // Negotiate version for GET, then run the process loop to listen for requests and responses
        let process_loop_task = spawn.spawn("get read", async move {
            let mut process_loop = ProcessLoop::new(pipe);

            let version_result = process_loop.negotiate_version().await;
            let version_negotiation_successful = version_result.is_ok();
            version_send.send(version_result);

            // Exit the run loop if version_negotiation failed.
            if !version_negotiation_successful {
                tracing::warn!("Version negotiation failed, GET process loop exiting");
                return Ok(());
            }

            let res = process_loop.run(control_recv).await;

            if let Err(e) = &res {
                tracing::error!(error = e.as_error(), "GET process loop failed");
            }

            res.map_err(FatalGetError)
        });

        let version = version_recv
            .await
            .map_err(FatalError::VersionNegotiationTryRecvFailed)??;

        tracing::trace!("version negotiated successfully: {:?}", version);

        Ok((
            GuestEmulationTransportWorker {
                control: control_send,
                version,
            },
            process_loop_task,
        ))
    }

    pub fn new_client(self) -> GuestEmulationTransportClient {
        GuestEmulationTransportClient::new(self.control, self.version)
    }
}
