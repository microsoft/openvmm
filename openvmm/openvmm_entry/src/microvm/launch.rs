// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM launch state carried from the VM configuration to the VM worker
//! and the VM controller.

use super::MicrovmResources;
use crate::Options;
use crate::cli_args::microvm::MachineProfileCli;
use crate::vm_controller::MicrovmController;
use anyhow::Context;
use chipset_resources::microvm::MicrovmSnapshotBoundaryRequest;
use openvmm_defs::config::Config;
use openvmm_defs::config::LoadMode;
use openvmm_defs::worker::SharedMemoryFd;
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
    snapshot_destination: Option<PathBuf>,
    snapshot_memory_file: Option<tempfile::NamedTempFile>,
    snapshot_memory_handle: Option<std::fs::File>,
}

impl MicrovmLaunch {
    /// Captures the microVM state of `vm_config` and prepares the RAM backing
    /// used by guest-requested snapshot capture.
    pub(crate) fn new(
        opt: &Options,
        vm_config: &Config,
        resources: MicrovmResources,
    ) -> anyhow::Result<Self> {
        let effective_command_line = match &vm_config.load_mode {
            LoadMode::Linux {
                cmdline,
                boot_mode: openvmm_defs::config::LinuxDirectBootMode::MpTable,
                ..
            } => Some(cmdline.clone()),
            _ => None,
        };

        let snapshot_destination = opt.microvm.snapshot_destination.as_ref().map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                std::env::current_dir().unwrap_or_default().join(path)
            }
        });
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
            snapshot_destination,
            snapshot_memory_file,
            snapshot_memory_handle,
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

    /// Creates the channels that carry guest-requested snapshot boundaries from
    /// the VM worker to the VM controller.
    pub(crate) fn snapshot_channels(
        &mut self,
    ) -> (
        Option<mesh::Receiver<MicrovmSnapshotBoundaryRequest>>,
        Option<mesh::Sender<()>>,
        Option<mesh::Receiver<()>>,
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
        snapshot_requests: Option<mesh::Receiver<()>>,
    ) -> MicrovmController {
        MicrovmController {
            active: self.active,
            snapshot_memory_handle: self.snapshot_memory_handle,
            snapshot_requests,
            snapshot_destination: self.snapshot_destination,
            snapshot_quiesce_timeout: Duration::from_millis(
                opt.microvm.snapshot_quiesce_timeout_ms,
            ),
            source_hypervisor,
            effective_command_line: self.effective_command_line,
            snapshot_memory_file: self.snapshot_memory_file,
        }
    }
}
