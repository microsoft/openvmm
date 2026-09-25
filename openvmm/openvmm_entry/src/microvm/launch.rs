// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM launch state carried from the VM configuration to the VM worker
//! and the VM controller.

use super::ExpectedRestoreContract;
use super::MicrovmResources;
use super::MicrovmRestore;
use super::validate_microvm_filesystem_private_storage;
use crate::Options;
use crate::cli_args::microvm::MachineProfileCli;
use crate::vm_controller::MicrovmController;
use anyhow::Context;
use chipset_resources::microvm::MicrovmSnapshotBoundaryRequest;
use chipset_resources::microvm::MicrovmSnapshotScratchPolicy;
use openvmm_defs::config::Config;
use openvmm_defs::config::LoadMode;
use openvmm_defs::microvm::MicrovmFilesystemConfig;
use openvmm_defs::microvm::MicrovmNetworkConfig;
use openvmm_defs::worker::SharedMemoryFd;
use openvmm_helpers::snapshot::SnapshotManifest;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

/// MicroVM state derived from the VM configuration before the VM worker is
/// launched.
pub(crate) struct MicrovmLaunch {
    active: bool,
    effective_command_line: Option<String>,
    resources: MicrovmResources,
    network: Option<MicrovmNetworkConfig>,
    filesystem_slot: bool,
    filesystem: Option<MicrovmFilesystemConfig>,
    snapshot_destination: Option<PathBuf>,
    snapshot_memory_file: Option<tempfile::NamedTempFile>,
    snapshot_memory_handle: Option<std::fs::File>,
    restore: MicrovmRestore,
}

impl MicrovmLaunch {
    /// Captures the microVM state of `vm_config` and prepares the RAM backing
    /// used by guest-requested snapshot capture.
    pub(crate) fn new(
        opt: &Options,
        vm_config: &Config,
        resources: MicrovmResources,
        restore: MicrovmRestore,
    ) -> anyhow::Result<Self> {
        let effective_command_line = match &vm_config.load_mode {
            LoadMode::Linux {
                cmdline,
                boot_mode: openvmm_defs::config::LinuxDirectBootMode::MpTable,
                ..
            } => Some(cmdline.clone()),
            _ => None,
        };
        let filesystem_slot = vm_config
            .virtio_devices
            .iter()
            .any(|(_, device)| device.id() == "virtiofs");

        let snapshot_destination = opt.microvm.snapshot_destination.as_ref().map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                std::env::current_dir().unwrap_or_default().join(path)
            }
        });
        if let Some(root_path) = resources.filesystem_root_path.as_deref() {
            validate_microvm_filesystem_private_storage(
                root_path,
                snapshot_destination.as_deref(),
                opt.restore_snapshot.as_deref(),
                opt.memory_backing_file().map(PathBuf::as_path),
            )?;
        }
        let snapshot_memory_file = if let Some(destination) = &snapshot_destination
            && opt.memory_backing_file().is_none()
        {
            let parent = destination
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let parent_metadata = fs_err::symlink_metadata(parent).with_context(|| {
                format!("failed to inspect snapshot parent {}", parent.display())
            })?;
            anyhow::ensure!(
                parent_metadata.file_type().is_dir(),
                "snapshot parent is not a directory: {}",
                parent.display()
            );
            anyhow::ensure!(
                fs_err::symlink_metadata(destination)
                    .is_err_and(|error| { error.kind() == io::ErrorKind::NotFound }),
                "snapshot destination already exists or cannot be inspected: {}",
                destination.display()
            );
            let file = tempfile::Builder::new()
                .prefix(".openvmm-microvm-memory-")
                .tempfile_in(parent)
                .context("failed to create snapshot memory backing")?;
            openvmm_helpers::snapshot::fs::initialize_snapshot_memory_backing_file(
                file.as_file(),
                opt.memory_size(),
            )
            .context("failed to initialize snapshot memory backing")?;
            Some(file)
        } else {
            None
        };
        let snapshot_memory_handle = if snapshot_destination.is_some() {
            if let Some(file) = &snapshot_memory_file {
                Some(
                    file.reopen()
                        .context("failed to duplicate automatic snapshot RAM handle")?,
                )
            } else {
                opt.memory_backing_file()
                    .map(|path| {
                        openvmm_helpers::shared_memory::open_memory_backing_file_handle(
                            path,
                            opt.memory_size(),
                        )
                    })
                    .transpose()?
            }
        } else {
            None
        };

        Ok(Self {
            active: opt.machine == MachineProfileCli::Microvm,
            effective_command_line,
            resources,
            network: vm_config.microvm.network.clone(),
            filesystem_slot,
            filesystem: vm_config.microvm.filesystem.clone(),
            snapshot_destination,
            snapshot_memory_file,
            snapshot_memory_handle,
            restore,
        })
    }

    /// Returns whether guest-requested snapshot capture is configured.
    pub(crate) fn snapshot_capture_enabled(&self) -> bool {
        self.snapshot_destination.is_some()
    }

    /// Duplicates the exact RAM handle used for guest-requested snapshot
    /// capture, as the VM worker's shared guest RAM.
    pub(crate) fn capture_shared_memory(&self) -> anyhow::Result<Option<SharedMemoryFd>> {
        let Some(file) = &self.snapshot_memory_handle else {
            return Ok(None);
        };
        let file = file
            .try_clone()
            .context("failed to duplicate snapshot RAM handle for worker")?;
        let shared_memory = openvmm_helpers::shared_memory::file_to_shared_memory_fd(file)?;
        Ok(Some(shared_memory))
    }

    /// Returns the time allowed for post-restore guest repair, if required.
    pub(crate) fn restore_gate_timeout(&self, opt: &Options) -> Option<Duration> {
        self.restore
            .gate_required
            .then_some(Duration::from_millis(opt.microvm.restore_gate_timeout_ms))
    }

    /// Creates the channels that carry guest-requested snapshot boundaries from
    /// the VM worker to the VM controller.
    pub(crate) fn snapshot_channels(
        &mut self,
    ) -> (
        Option<mesh::Receiver<MicrovmSnapshotBoundaryRequest>>,
        Option<mesh::Sender<MicrovmSnapshotScratchPolicy>>,
        Option<mesh::Receiver<MicrovmSnapshotScratchPolicy>>,
    ) {
        let snapshot_boundary_requests = self.resources.snapshot_requests.take();
        let (snapshot_ready, snapshot_requests) = if snapshot_boundary_requests.is_some() {
            let (ready, requests) = mesh::channel();
            (Some(ready), Some(requests))
        } else {
            (None, None)
        };
        (
            snapshot_boundary_requests,
            snapshot_ready,
            snapshot_requests,
        )
    }

    /// Returns the machine contract a microVM snapshot must match to be
    /// restored with this configuration.
    pub(crate) fn expected_restore_contract<'a>(
        &'a self,
        opt: &Options,
        manifest: &SnapshotManifest,
        expected_hypervisor: &'a str,
    ) -> anyhow::Result<Option<ExpectedRestoreContract<'a>>> {
        if opt.machine != MachineProfileCli::Microvm {
            return Ok(None);
        }
        let scratch_policy = if manifest
            .machine_contract
            .as_ref()
            .and_then(|contract| contract.microvm_sandbox_blocks.last())
            .is_some_and(|block| {
                block.artifact == openvmm_helpers::snapshot::format::SCRATCH_FILE_NAME
            }) {
            MicrovmSnapshotScratchPolicy::Paired
        } else {
            MicrovmSnapshotScratchPolicy::Fresh
        };
        let sandbox_blocks = crate::storage_builder::microvm::snapshot_block_contract(
            &self.resources.sandbox_block_sources,
            scratch_policy,
        )?;
        let resources = &self.resources;
        Ok(Some((
            expected_hypervisor,
            self.effective_command_line
                .as_deref()
                .context("microVM restore requires an effective command line")?,
            self.network
                .as_ref()
                .zip(resources.network_attachment.as_ref()),
            self.filesystem
                .as_ref()
                .zip(resources.filesystem_root_path.as_deref())
                .zip(resources.filesystem_attachment.as_ref())
                .map(|((filesystem, root_path), attachment)| (filesystem, root_path, attachment)),
            resources.console_attachment.as_ref(),
            sandbox_blocks,
        )))
    }

    /// Returns the path of the file backing guest RAM, including the automatic
    /// snapshot RAM backing.
    pub(crate) fn memory_backing_file(&self, opt: &Options) -> Option<PathBuf> {
        opt.memory_backing_file().cloned().or_else(|| {
            self.snapshot_memory_file
                .as_ref()
                .map(|file| file.path().to_owned())
        })
    }

    /// Hands the microVM state over to the VM controller.
    pub(crate) fn into_controller(
        self,
        opt: &Options,
        source_hypervisor: String,
        snapshot_requests: Option<mesh::Receiver<MicrovmSnapshotScratchPolicy>>,
    ) -> MicrovmController {
        MicrovmController {
            active: self.active,
            snapshot_memory_handle: self.snapshot_memory_handle,
            snapshot_requests,
            snapshot_destination: self.snapshot_destination,
            snapshot_tier: opt.microvm.snapshot_tier,
            snapshot_quiesce_timeout: Duration::from_millis(
                opt.microvm.snapshot_quiesce_timeout_ms,
            ),
            source_hypervisor,
            effective_command_line: self.effective_command_line,
            resources: self.resources,
            network: self.network,
            filesystem_slot: self.filesystem_slot,
            filesystem: self.filesystem,
            snapshot_memory_file: self.snapshot_memory_file,
            _private_scratch_dir: self.restore.private_scratch_dir,
        }
    }
}
