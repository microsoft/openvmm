// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Methods to interact with a running [`PetriVmOpenVmm`].

use super::PetriVmResourcesOpenVmm;
use crate::OpenHclServicingFlags;
use crate::PetriHaltReason;
use crate::PetriVmFramebufferAccess;
use crate::PetriVmInspector;
use crate::PetriVmRuntime;
use crate::ShutdownKind;
use crate::VmScreenshotMeta;
use crate::openhcl_diag::OpenHclDiagHandler;
use crate::worker::Worker;
use anyhow::Context;
use async_trait::async_trait;
use framebuffer::View;
use futures::FutureExt;
use futures_concurrency::future::Race;
use get_resources::ged::FirmwareEvent;
use hyperv_ic_resources::shutdown::ShutdownRpc;
use mesh::CancelContext;
use mesh::Receiver;
use mesh::RecvError;
use mesh::rpc::RpcError;
use mesh::rpc::RpcSend;
use mesh_process::Mesh;
use openvmm_defs::rpc::PulseSaveRestoreError;
use pal_async::socket::PolledSocket;
use petri_artifacts_core::ResolvedArtifact;
use pipette_client::PipetteClient;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use vmm_core_defs::HaltReason;
use vtl2_settings_proto::Vtl2Settings;

/// A running VM that tests can interact with.
// DEVNOTE: Really the PetriVmInner is the actual VM and channels that we interact
// with. This struct exists as a wrapper to provide error handling, such as not
// hanging indefinitely when waiting on certain channels if the VM crashes.
pub struct PetriVmOpenVmm {
    inner: PetriVmInner,
    halt: PetriVmHaltReceiver,
}

#[async_trait]
impl PetriVmRuntime for PetriVmOpenVmm {
    type VmInspector = OpenVmmInspector;
    type VmFramebufferAccess = OpenVmmFramebufferAccess;

    async fn teardown(self) -> anyhow::Result<()> {
        tracing::info!("waiting for worker");
        let worker = Arc::into_inner(self.inner.worker)
            .context("all references to the OpenVMM worker have not been closed")?;
        worker.shutdown().await?;

        tracing::info!("Worker quit, waiting for mesh");
        self.inner.mesh.shutdown().await;

        tracing::info!("Mesh shutdown, waiting for logging tasks");
        for t in self.inner.resources.log_stream_tasks {
            t.await?;
        }

        Ok(())
    }

    async fn wait_for_halt(&mut self, allow_reset: bool) -> anyhow::Result<PetriHaltReason> {
        let halt_reason = if let Some(already) = self.halt.already_received.take() {
            already.map_err(anyhow::Error::from)
        } else {
            self.halt
                .halt_notif
                .recv()
                .await
                .context("Failed to get halt reason")
        }?;

        tracing::info!(?halt_reason, "Got halt reason");

        let halt_reason = match halt_reason {
            HaltReason::PowerOff => PetriHaltReason::PowerOff,
            HaltReason::Reset => PetriHaltReason::Reset,
            HaltReason::Hibernate => PetriHaltReason::Hibernate,
            HaltReason::TripleFault { .. } => PetriHaltReason::TripleFault,
            _ => PetriHaltReason::Other,
        };

        if allow_reset && halt_reason == PetriHaltReason::Reset {
            self.reset().await?
        }

        Ok(halt_reason)
    }

    async fn wait_for_agent(&mut self, set_high_vtl: bool) -> anyhow::Result<PipetteClient> {
        Self::wait_for_agent(self, set_high_vtl).await
    }

    fn openhcl_diag(&self) -> Option<OpenHclDiagHandler> {
        self.inner.resources.vtl2_vsock_path.as_ref().map(|path| {
            OpenHclDiagHandler::new(diag_client::DiagClient::from_hybrid_vsock(
                self.inner.resources.driver.clone(),
                path,
            ))
        })
    }

    async fn wait_for_boot_event(&mut self) -> anyhow::Result<FirmwareEvent> {
        Self::wait_for_boot_event(self).await
    }

    async fn wait_for_enlightened_shutdown_ready(&mut self) -> anyhow::Result<()> {
        Self::wait_for_enlightened_shutdown_ready(self)
            .await
            .map(|_| ())
    }

    async fn send_enlightened_shutdown(&mut self, kind: ShutdownKind) -> anyhow::Result<()> {
        Self::send_enlightened_shutdown(self, kind).await
    }

    async fn restart_openhcl(
        &mut self,
        new_openhcl: &ResolvedArtifact,
        flags: OpenHclServicingFlags,
    ) -> anyhow::Result<()> {
        Self::save_openhcl(self, new_openhcl, flags).await?;
        Self::restore_openhcl(self).await
    }

    async fn save_openhcl(
        &mut self,
        new_openhcl: &ResolvedArtifact,
        flags: OpenHclServicingFlags,
    ) -> anyhow::Result<()> {
        Self::save_openhcl(self, new_openhcl, flags).await
    }

    async fn restore_openhcl(&mut self) -> anyhow::Result<()> {
        Self::restore_openhcl(self).await
    }

    async fn update_command_line(&mut self, command_line: &str) -> anyhow::Result<()> {
        Self::update_command_line(self, command_line).await
    }

    fn inspector(&self) -> Option<OpenVmmInspector> {
        Some(OpenVmmInspector {
            worker: self.inner.worker.clone(),
        })
    }

    fn take_framebuffer_access(&mut self) -> Option<OpenVmmFramebufferAccess> {
        self.inner
            .framebuffer_view
            .take()
            .map(|view| OpenVmmFramebufferAccess { view })
    }

    async fn reset(&mut self) -> anyhow::Result<()> {
        Self::reset(self).await
    }

    async fn set_vtl2_settings(&mut self, settings: &Vtl2Settings) -> anyhow::Result<()> {
        Self::set_vtl2_settings(self, settings).await
    }

    async fn set_vmbus_drive(
        &mut self,
        _disk: &crate::Drive,
        _controller_id: &guid::Guid,
        _controller_location: u8,
    ) -> anyhow::Result<()> {
        todo!("openvmm set vmbus drive")
    }
}

pub(super) struct PetriVmInner {
    pub(super) resources: PetriVmResourcesOpenVmm,
    pub(super) mesh: Mesh,
    pub(super) worker: Arc<Worker>,
    pub(super) framebuffer_view: Option<View>,
}

struct PetriVmHaltReceiver {
    halt_notif: Receiver<HaltReason>,
    already_received: Option<Result<HaltReason, RecvError>>,
}

// Wrap a PetriVmInner function in [`PetriVmOpenVmm::wait_for_halt_or_internal`] to
// provide better error handling.
macro_rules! petri_vm_fn {
    ($(#[$($attrss:tt)*])* $vis:vis async fn $fn_name:ident (&mut self $(,$arg:ident: $ty:ty)*) $(-> $ret:ty)?) => {
        $(#[$($attrss)*])*
        $vis async fn $fn_name(&mut self, $($arg:$ty,)*) $(-> $ret)? {
            Self::wait_for_halt_or_internal(&mut self.halt, self.inner.$fn_name($($arg,)*)).await
        }
    };
}

// TODO: Add all runtime functions that are not backend specific
// to the `PetriVmRuntime` trait
impl PetriVmOpenVmm {
    pub(super) fn new(inner: PetriVmInner, halt_notif: Receiver<HaltReason>) -> Self {
        Self {
            inner,
            halt: PetriVmHaltReceiver {
                halt_notif,
                already_received: None,
            },
        }
    }

    /// Get the path to the VTL 2 vsock socket, if the VM is configured with OpenHCL.
    pub fn vtl2_vsock_path(&self) -> anyhow::Result<&Path> {
        self.inner
            .resources
            .vtl2_vsock_path
            .as_deref()
            .context("VM is not configured with OpenHCL")
    }

    petri_vm_fn!(
        /// Waits for an event emitted by the firmware about its boot status, and
        /// returns that status.
        pub async fn wait_for_boot_event(&mut self) -> anyhow::Result<FirmwareEvent>
    );
    petri_vm_fn!(
        /// Waits for the Hyper-V shutdown IC to be ready, returning a receiver
        /// that will be closed when it is no longer ready.
        pub async fn wait_for_enlightened_shutdown_ready(&mut self) -> anyhow::Result<mesh::OneshotReceiver<()>>
    );
    petri_vm_fn!(
        /// Instruct the guest to shutdown via the Hyper-V shutdown IC.
        pub async fn send_enlightened_shutdown(&mut self, kind: ShutdownKind) -> anyhow::Result<()>
    );
    petri_vm_fn!(
        /// Waits for the KVP IC to be ready, returning a sender that can be used
        /// to send requests to it.
        pub async fn wait_for_kvp(&mut self) -> anyhow::Result<mesh::Sender<hyperv_ic_resources::kvp::KvpRpc>>
    );
    petri_vm_fn!(
        /// Stages the new OpenHCL file and saves the existing state.
        pub async fn save_openhcl(
            &mut self,
            new_openhcl: &ResolvedArtifact,
            flags: OpenHclServicingFlags
        ) -> anyhow::Result<()>
    );
    petri_vm_fn!(
        /// Restores OpenHCL from a previously saved state.
        pub async fn restore_openhcl(
            &mut self
        ) -> anyhow::Result<()>
    );
    petri_vm_fn!(
        /// Updates the command line parameters of the running VM.
        pub async fn update_command_line(
            &mut self,
            command_line: &str
        ) -> anyhow::Result<()>
    );
    petri_vm_fn!(
        /// Resets the hardware state of the VM, simulating a power cycle.
        pub async fn reset(&mut self) -> anyhow::Result<()>
    );
    petri_vm_fn!(
        /// Wait for a connection from a pipette agent
        pub async fn wait_for_agent(&mut self, set_high_vtl: bool) -> anyhow::Result<PipetteClient>
    );
    petri_vm_fn!(
        /// Set the OpenHCL VTL2 settings.
        pub async fn set_vtl2_settings(&mut self, settings: &Vtl2Settings) -> anyhow::Result<()>
    );

    petri_vm_fn!(pub(crate) async fn resume(&mut self) -> anyhow::Result<()>);
    petri_vm_fn!(pub(crate) async fn verify_save_restore(&mut self) -> anyhow::Result<()>);
    petri_vm_fn!(pub(crate) async fn launch_linux_direct_pipette(&mut self) -> anyhow::Result<()>);

    /// Wrap the provided future in a race with the worker process's halt
    /// notification channel. This is useful for preventing a future from
    /// waiting indefinitely if the VM dies for any reason. If the worker
    /// process crashes the halt notification channel will return an error, and
    /// if the VM halts for any other reason the future will complete with that
    /// reason.
    pub async fn wait_for_halt_or<T, F: Future<Output = anyhow::Result<T>>>(
        &mut self,
        future: F,
    ) -> anyhow::Result<T> {
        Self::wait_for_halt_or_internal(&mut self.halt, future).await
    }

    async fn wait_for_halt_or_internal<T, F: Future<Output = anyhow::Result<T>>>(
        halt: &mut PetriVmHaltReceiver,
        future: F,
    ) -> anyhow::Result<T> {
        let future = &mut std::pin::pin!(future);
        enum Either<T> {
            Future(anyhow::Result<T>),
            Halt(Result<HaltReason, RecvError>),
        }
        let res = (
            future.map(Either::Future),
            halt.halt_notif.recv().map(Either::Halt),
        )
            .race()
            .await;

        match res {
            Either::Future(Ok(success)) => Ok(success),
            Either::Future(Err(e)) => {
                tracing::warn!(
                    ?e,
                    "Future returned with an error, sleeping for 5 seconds to let outstanding work finish"
                );
                let mut c = CancelContext::new().with_timeout(Duration::from_secs(5));
                c.cancelled().await;
                Err(e)
            }
            Either::Halt(halt_result) => {
                tracing::warn!(
                    halt_result = format_args!("{:x?}", halt_result),
                    "Halt channel returned while waiting for other future, sleeping for 5 seconds to let outstanding work finish"
                );
                let mut c = CancelContext::new().with_timeout(Duration::from_secs(5));
                let try_again = c.until_cancelled(future).await;

                match try_again {
                    Ok(fut_result) => {
                        halt.already_received = Some(halt_result);
                        if let Err(e) = &fut_result {
                            tracing::warn!(
                                ?e,
                                "Future returned with an error, sleeping for 5 seconds to let outstanding work finish"
                            );
                            let mut c = CancelContext::new().with_timeout(Duration::from_secs(5));
                            c.cancelled().await;
                        }
                        fut_result
                    }
                    Err(_cancel) => match halt_result {
                        Ok(halt_reason) => Err(anyhow::anyhow!("VM halted: {:x?}", halt_reason)),
                        Err(e) => Err(e).context("VM disappeared"),
                    },
                }
            }
        }
    }
}

impl PetriVmInner {
    async fn wait_for_boot_event(&mut self) -> anyhow::Result<FirmwareEvent> {
        self.resources
            .firmware_event_recv
            .recv()
            .await
            .context("Failed to get firmware boot event")
    }

    async fn wait_for_enlightened_shutdown_ready(
        &mut self,
    ) -> anyhow::Result<mesh::OneshotReceiver<()>> {
        let recv = self
            .resources
            .shutdown_ic_send
            .call(ShutdownRpc::WaitReady, ())
            .await?;

        Ok(recv)
    }

    async fn send_enlightened_shutdown(&mut self, kind: ShutdownKind) -> anyhow::Result<()> {
        let shutdown_result = self
            .resources
            .shutdown_ic_send
            .call(
                ShutdownRpc::Shutdown,
                hyperv_ic_resources::shutdown::ShutdownParams {
                    shutdown_type: match kind {
                        ShutdownKind::Shutdown => {
                            hyperv_ic_resources::shutdown::ShutdownType::PowerOff
                        }
                        ShutdownKind::Reboot => hyperv_ic_resources::shutdown::ShutdownType::Reboot,
                    },
                    force: false,
                },
            )
            .await?;

        tracing::info!(?shutdown_result, "Shutdown sent");
        anyhow::ensure!(
            shutdown_result == hyperv_ic_resources::shutdown::ShutdownResult::Ok,
            "Got non-Ok shutdown response"
        );

        Ok(())
    }

    async fn wait_for_kvp(
        &mut self,
    ) -> anyhow::Result<mesh::Sender<hyperv_ic_resources::kvp::KvpRpc>> {
        tracing::info!("Waiting for KVP IC");
        let (send, _) = self
            .resources
            .kvp_ic_send
            .call_failable(hyperv_ic_resources::kvp::KvpConnectRpc::WaitForGuest, ())
            .await
            .context("failed to connect to KVP IC")?;

        Ok(send)
    }

    async fn save_openhcl(
        &self,
        new_openhcl: &ResolvedArtifact,
        flags: OpenHclServicingFlags,
    ) -> anyhow::Result<()> {
        let ged_send = self
            .resources
            .ged_send
            .as_ref()
            .context("openhcl not configured")?;

        let igvm_file = fs_err::File::open(new_openhcl).context("failed to open igvm file")?;
        self.worker
            .save_openhcl(ged_send, flags, igvm_file.into())
            .await
    }

    async fn update_command_line(&mut self, command_line: &str) -> anyhow::Result<()> {
        self.worker.update_command_line(command_line).await
    }

    async fn restore_openhcl(&self) -> anyhow::Result<()> {
        let ged_send = self
            .resources
            .ged_send
            .as_ref()
            .context("openhcl not configured")?;

        self.worker.restore_openhcl(ged_send).await
    }

    async fn set_vtl2_settings(&self, settings: &Vtl2Settings) -> anyhow::Result<()> {
        let ged_send = self
            .resources
            .ged_send
            .as_ref()
            .context("openhcl not configured")?;

        ged_send
            .call_failable(
                get_resources::ged::GuestEmulationRequest::ModifyVtl2Settings,
                prost::Message::encode_to_vec(settings),
            )
            .await?;

        Ok(())
    }

    async fn reset(&mut self) -> anyhow::Result<()> {
        tracing::info!("Resetting VM");
        self.worker.reset().await?;
        // On linux direct pipette won't auto start, start it over serial
        if let Some(agent) = self.resources.linux_direct_serial_agent.as_mut() {
            agent.reset();

            if self
                .resources
                .agent_image
                .as_ref()
                .is_some_and(|x| x.contains_pipette())
            {
                self.launch_linux_direct_pipette().await?;
            }
        }
        Ok(())
    }

    async fn wait_for_agent(&mut self, set_high_vtl: bool) -> anyhow::Result<PipetteClient> {
        let listener = if set_high_vtl {
            self.resources
                .vtl2_pipette_listener
                .as_mut()
                .context("VM is not configured with VTL 2")?
        } else {
            &mut self.resources.pipette_listener
        };

        tracing::info!(set_high_vtl, "listening for pipette connection");
        let (conn, _) = listener
            .accept()
            .await
            .context("failed to accept pipette connection")?;
        tracing::info!(set_high_vtl, "handshaking with pipette");
        let client = PipetteClient::new(
            &self.resources.driver,
            PolledSocket::new(&self.resources.driver, conn)?,
            &self.resources.output_dir,
        )
        .await
        .context("failed to connect to pipette");
        tracing::info!(set_high_vtl, "completed pipette handshake");
        client
    }

    async fn resume(&self) -> anyhow::Result<()> {
        self.worker.resume().await?;
        Ok(())
    }

    async fn verify_save_restore(&self) -> anyhow::Result<()> {
        for i in 0..2 {
            let result = self.worker.pulse_save_restore().await;
            match result {
                Ok(()) => {}
                Err(RpcError::Channel(err)) => return Err(err.into()),
                Err(RpcError::Call(PulseSaveRestoreError::ResetNotSupported)) => {
                    tracing::warn!("Reset not supported, could not test save + restore.");
                    break;
                }
                Err(RpcError::Call(PulseSaveRestoreError::Other(err))) => {
                    return Err(anyhow::Error::from(err))
                        .context(format!("Save + restore {i} failed."));
                }
            }
        }

        Ok(())
    }

    async fn launch_linux_direct_pipette(&mut self) -> anyhow::Result<()> {
        // Start pipette through serial on linux direct.
        self.resources
            .linux_direct_serial_agent
            .as_mut()
            .unwrap()
            .run_command("mkdir /cidata && mount LABEL=cidata /cidata && sh -c '/cidata/pipette &'")
            .await?;
        Ok(())
    }
}

/// Interface for inspecting OpenVMM
pub struct OpenVmmInspector {
    worker: Arc<Worker>,
}

#[async_trait]
impl PetriVmInspector for OpenVmmInspector {
    async fn inspect_all(&self) -> anyhow::Result<inspect::Node> {
        Ok(self.worker.inspect_all().await)
    }
}

/// Interface to the OpenVMM framebuffer
pub struct OpenVmmFramebufferAccess {
    view: View,
}

#[async_trait]
impl PetriVmFramebufferAccess for OpenVmmFramebufferAccess {
    async fn screenshot(
        &mut self,
        image: &mut Vec<u8>,
    ) -> anyhow::Result<Option<VmScreenshotMeta>> {
        // Our framebuffer uses 4 bytes per pixel, approximating an
        // BGRA image, however it only actually contains BGR data.
        // The fourth byte is effectively noise. We can set the 'alpha'
        // value to 0xFF to make the image opaque.
        const BYTES_PER_PIXEL: usize = 4;
        let (width, height) = self.view.resolution();
        let (widthsize, heightsize) = (width as usize, height as usize);
        let len = widthsize * heightsize * BYTES_PER_PIXEL;

        image.resize(len, 0);
        for (i, line) in (0..height).zip(image.chunks_exact_mut(widthsize * BYTES_PER_PIXEL)) {
            self.view.read_line(i, line);
            for pixel in line.chunks_exact_mut(BYTES_PER_PIXEL) {
                pixel.swap(0, 2);
                pixel[3] = 0xFF;
            }
        }

        Ok(Some(VmScreenshotMeta {
            color: image::ExtendedColorType::Rgba8,
            width,
            height,
        }))
    }
}
