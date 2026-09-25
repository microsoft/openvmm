// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM snapshot capture handling of the VM controller.

use super::VmController;
use crate::microvm::MicrovmResources;
use anyhow::Context;
use mesh::rpc::RpcSend;
use openvmm_defs::rpc::SnapshotQuiesceError;
use openvmm_defs::rpc::VmRpc;
use std::path::PathBuf;
use std::time::Duration;

/// MicroVM state owned by the VM controller.
#[derive(Default)]
pub(crate) struct MicrovmController {
    /// Whether the microVM machine profile is selected.
    pub(crate) active: bool,
    /// Exact open handle of the file backing guest RAM for snapshot capture.
    pub(crate) snapshot_memory_handle: Option<std::fs::File>,
    /// Guest-requested snapshot boundaries released by the VM worker.
    pub(crate) snapshot_requests: Option<mesh::Receiver<()>>,
    /// Directory receiving a guest-requested snapshot.
    pub(crate) snapshot_destination: Option<PathBuf>,
    /// Maximum time allowed to quiesce the VM for a guest-requested snapshot.
    pub(crate) snapshot_quiesce_timeout: Duration,
    /// Hypervisor backend recorded in captured machine contracts.
    pub(crate) source_hypervisor: String,
    /// Effective kernel command line of the MP-table boot.
    pub(crate) effective_command_line: Option<String>,
    /// Host attachments and cleanup guards of the microVM devices.
    pub(crate) resources: MicrovmResources,
    /// Static identity of the microVM virtio-net device.
    pub(crate) network: Option<openvmm_defs::microvm::MicrovmNetworkConfig>,
    /// Whether the microVM virtio-fs slot is present.
    pub(crate) filesystem_slot: bool,
    /// Guest-visible policy of the active microVM filesystem.
    pub(crate) filesystem: Option<openvmm_defs::microvm::MicrovmFilesystemConfig>,
    /// Automatic RAM backing created for snapshot capture.
    pub(crate) snapshot_memory_file: Option<tempfile::NamedTempFile>,
}

pub(super) enum GuestSnapshotAction {
    Continue,
    Terminate { exit_code: i32 },
}

impl VmController {
    pub(super) async fn handle_guest_snapshot_request(&mut self) -> GuestSnapshotAction {
        let Some(destination) = self.microvm.snapshot_destination.clone() else {
            tracelimit::warn_ratelimited!(
                "ignoring microVM snapshot request because no destination is configured"
            );
            return self.release_snapshot_boundary_without_capture().await;
        };

        let preflight = (|| -> anyhow::Result<()> {
            anyhow::ensure!(
                self.microvm.active,
                "guest-requested snapshot capture requires the microVM profile"
            );
            anyhow::ensure!(
                matches!(
                    self.microvm.source_hypervisor.as_str(),
                    "kvm" | "mshv" | "whp"
                ),
                "microVM snapshot source backend must be KVM, MSHV, or WHP"
            );
            anyhow::ensure!(
                fs_err::symlink_metadata(&destination)
                    .is_err_and(|error| { error.kind() == std::io::ErrorKind::NotFound }),
                "snapshot destination already exists or cannot be inspected: {}",
                destination.display()
            );
            let memory_path = self
                .memory_backing_file
                .clone()
                .context("microVM snapshot capture requires file-backed RAM")?;
            let memory_file = self
                .microvm
                .snapshot_memory_handle
                .as_ref()
                .context("microVM snapshot capture lost its exact RAM handle")?;
            anyhow::ensure!(
                memory_file
                    .metadata()
                    .context("failed to inspect snapshot RAM handle")?
                    .len()
                    == self.memory,
                "snapshot RAM handle size does not match the VM"
            );
            if let Some(file) = &self.microvm.snapshot_memory_file {
                anyhow::ensure!(
                    file.path() == memory_path,
                    "automatic snapshot memory backing path changed unexpectedly"
                );
            }
            anyhow::ensure!(
                self.microvm.effective_command_line.is_some(),
                "microVM snapshot capture requires an effective command line"
            );
            Ok(())
        })();
        if let Err(error) = preflight {
            tracing::error!(
                error = error.as_ref() as &dyn std::error::Error,
                "microVM snapshot preflight failed; guest continues"
            );
            return self.release_snapshot_boundary_without_capture().await;
        }

        let response = match self
            .vm_rpc
            .call(
                VmRpc::QuiesceForSnapshot,
                self.microvm.snapshot_quiesce_timeout,
            )
            .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(SnapshotQuiesceError::Rejected(error))) => {
                tracing::error!(
                    error = &error as &dyn std::error::Error,
                    "microVM snapshot request was rejected; guest continues"
                );
                return self.release_snapshot_boundary_without_capture().await;
            }
            Ok(Err(SnapshotQuiesceError::RollbackSafe(error))) => {
                return self.rollback_failed_guest_snapshot(error.into()).await;
            }
            Ok(Err(SnapshotQuiesceError::Uncertain(error))) => {
                tracing::error!(
                    error = &error as &dyn std::error::Error,
                    "microVM snapshot quiesce left uncertain state; terminating source"
                );
                return GuestSnapshotAction::Terminate { exit_code: 1 };
            }
            Err(error) => {
                tracing::error!(
                    error = &error as &dyn std::error::Error,
                    "lost VM worker during snapshot quiesce; terminating source"
                );
                return GuestSnapshotAction::Terminate { exit_code: 1 };
            }
        };
        let command_line = response.effective_command_line;

        let result = (|| -> anyhow::Result<()> {
            let network = self
                .microvm
                .network
                .as_ref()
                .zip(self.microvm.resources.network_attachment.clone());
            let filesystem = self
                .microvm
                .filesystem
                .as_ref()
                .zip(self.microvm.resources.filesystem_root_path.as_deref())
                .zip(self.microvm.resources.filesystem_attachment.clone())
                .map(|((filesystem, root_path), attachment)| (filesystem, root_path, attachment));
            let machine_contract = openvmm_helpers::snapshot::microvm::microvm_machine_contract(
                &self.microvm.source_hypervisor,
                openvmm_helpers::snapshot::microvm::MICROVM_BOOT_LAYOUT_VERSION,
                command_line,
                network,
                self.microvm.filesystem_slot,
                filesystem,
                self.microvm.resources.console_attachment.clone(),
                self.processors,
                self.memory,
                response.state_unit_names,
                response.capture_wall_clock,
                response.tsc_frequency_hz,
                Some(response.apic_frequency_hz),
                response.cpu_contract,
            )?;
            let manifest = openvmm_helpers::snapshot::SnapshotManifest {
                version: openvmm_helpers::snapshot::MANIFEST_VERSION,
                created_at: std::time::SystemTime::now().into(),
                openvmm_version: env!("CARGO_PKG_VERSION").to_owned(),
                memory_size_bytes: self.memory,
                vp_count: self.processors,
                page_size: crate::system_page_size(),
                architecture: crate::GUEST_ARCH.to_owned(),
                state_size_bytes: 0,
                state_sha256: Vec::new(),
                memory_sha256: Vec::new(),
                machine_contract: Some(machine_contract),
                format_magic: openvmm_helpers::snapshot::format::SNAPSHOT_FORMAT_MAGIC.to_vec(),
                saved_state_schema_version:
                    openvmm_helpers::snapshot::format::SAVED_STATE_SCHEMA_VERSION,
                saved_state_root_type: openvmm_helpers::snapshot::format::SAVED_STATE_ROOT_TYPE
                    .to_owned(),
            };
            let saved_state_bytes = mesh::payload::encode(response.saved_state);
            let memory_file = self
                .microvm
                .snapshot_memory_handle
                .as_ref()
                .context("microVM snapshot capture lost its exact RAM handle")?;
            let memory_handle_flush = openvmm_defs::profile::ProfileSpan::start();
            memory_file
                .sync_all()
                .context("failed to flush snapshot RAM handle")?;
            memory_handle_flush.complete(
                "capture",
                "memory_handle_flush",
                openvmm_defs::profile::ProfileCounters {
                    logical_bytes: Some(self.memory),
                    ..Default::default()
                },
            );
            let publication = openvmm_defs::profile::ProfileSpan::start();
            let write_result = if self.microvm.snapshot_memory_file.is_some() {
                openvmm_helpers::snapshot::publish::write_snapshot_from_owned_memory_file(
                    &destination,
                    &manifest,
                    &saved_state_bytes,
                    memory_file,
                )
            } else {
                openvmm_helpers::snapshot::publish::write_snapshot_from_memory_file(
                    &destination,
                    &manifest,
                    &saved_state_bytes,
                    memory_file,
                )
            };
            if write_result.is_ok() {
                publication.complete_milestone(
                    "capture",
                    "publication",
                    openvmm_defs::profile::ProfileCounters {
                        logical_bytes: Some(self.memory),
                        ..Default::default()
                    },
                );
            }
            write_result.map_err(anyhow::Error::new)
        })();

        match result {
            Ok(()) => {
                if let Some(cleanup) = self.microvm.resources.console_socket_cleanup.take()
                    && let Err(error) = cleanup.remove_if_owned()
                {
                    tracing::error!(
                        error = error.as_ref() as &dyn std::error::Error,
                        "snapshot committed but the source console socket could not be removed"
                    );
                    return GuestSnapshotAction::Terminate { exit_code: 1 };
                }
                tracing::info!(
                    path = %destination.display(),
                    "microVM snapshot committed; terminating source process"
                );
                GuestSnapshotAction::Terminate { exit_code: 0 }
            }
            Err(error) => {
                let write_error =
                    error.downcast_ref::<openvmm_helpers::snapshot::publish::SnapshotWriteError>();
                if write_error.is_some_and(|error| error.is_committed()) {
                    if let Some(cleanup) = self.microvm.resources.console_socket_cleanup.take()
                        && let Err(cleanup_error) = cleanup.remove_if_owned()
                    {
                        tracing::error!(
                            error = cleanup_error.as_ref() as &dyn std::error::Error,
                            "committed snapshot console socket could not be removed"
                        );
                    }
                    tracing::error!(
                        error = error.as_ref() as &dyn std::error::Error,
                        path = %destination.display(),
                        "snapshot committed but final durability reporting failed; terminating source"
                    );
                    GuestSnapshotAction::Terminate { exit_code: 1 }
                } else if write_error.is_some_and(|error| !error.is_rollback_safe()) {
                    tracing::error!(
                        error = error.as_ref() as &dyn std::error::Error,
                        "snapshot failed before commit but automatic RAM alias cleanup is uncertain; terminating source"
                    );
                    GuestSnapshotAction::Terminate { exit_code: 1 }
                } else {
                    self.rollback_failed_guest_snapshot(error).await
                }
            }
        }
    }

    async fn rollback_failed_guest_snapshot(
        &mut self,
        error: anyhow::Error,
    ) -> GuestSnapshotAction {
        tracing::error!(
            error = error.as_ref() as &dyn std::error::Error,
            "microVM snapshot failed before commit; attempting rollback"
        );
        match self
            .vm_rpc
            .call_failable(
                VmRpc::ResumeAfterFailedSnapshot,
                self.microvm.snapshot_quiesce_timeout,
            )
            .await
        {
            Ok(()) => {
                tracing::info!("microVM snapshot rollback succeeded; guest resumed");
                GuestSnapshotAction::Continue
            }
            Err(rollback_error) => {
                tracing::error!(
                    error = &rollback_error as &dyn std::error::Error,
                    "microVM snapshot rollback failed; terminating source"
                );
                GuestSnapshotAction::Terminate { exit_code: 1 }
            }
        }
    }

    async fn release_snapshot_boundary_without_capture(&mut self) -> GuestSnapshotAction {
        match self
            .vm_rpc
            .call_failable(VmRpc::ReleaseSnapshotBoundary, ())
            .await
        {
            Ok(()) => GuestSnapshotAction::Continue,
            Err(error) => {
                tracing::error!(
                    error = &error as &dyn std::error::Error,
                    "failed to release microVM snapshot boundary; terminating source"
                );
                GuestSnapshotAction::Terminate { exit_code: 1 }
            }
        }
    }
}
