// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot restore of a VM launched from the command line.
//!
//! A `--restore-snapshot` launch opens one exact snapshot generation first
//! ([`SnapshotRestore::open`]). Just before the VM worker launches, the opened
//! generation is validated against the VM configuration and its RAM is mapped
//! copy-on-write ([`SnapshotRestore::prepare`]). Its open artifact handles then
//! move to the worker, which keeps the generation pinned until VM teardown.
//!
//! With `OPENVMM_STARTUP_PROFILE` set, these steps record the `restore` phases
//! `artifact_open`, `artifact_prepare`, and `cow_section_create`, and the
//! launch records the `startup` milestone `worker_launch`
//! ([`worker_launched`]).

mod prepare;

use crate::Options;
use anyhow::Context;
use mesh::payload::message::ProtobufMessage;
use openvmm_defs::profile::ProfileSpan;
use openvmm_defs::worker::SharedMemoryFd;
use openvmm_defs::worker::SnapshotRestoreGuards;
use openvmm_helpers::snapshot::restore::OpenedSnapshot;

/// The snapshot restore of a VM launched from the command line.
pub(crate) struct SnapshotRestore {
    /// The opened snapshot generation, until [`Self::prepare`] takes it.
    snapshot: Option<OpenedSnapshot>,
    /// The restore inputs of the VM worker, recorded by [`Self::prepare`].
    worker: WorkerRestore,
}

/// Restore inputs of the VM worker beyond its guest RAM and saved state.
#[derive(Default)]
pub(crate) struct WorkerRestore {
    /// Whether writes to the worker's guest RAM must remain private to this VM.
    pub(crate) shared_memory_copy_on_write: bool,
    /// Snapshot generation handles that must outlive the restored VM.
    pub(crate) guards: Option<SnapshotRestoreGuards>,
}

impl SnapshotRestore {
    /// Opens the `--restore-snapshot` generation, if any.
    pub(crate) fn open(opt: &Options) -> anyhow::Result<Self> {
        let artifact_open = ProfileSpan::start();
        let snapshot = opt
            .restore_snapshot
            .as_deref()
            .map(OpenedSnapshot::open)
            .transpose()?;
        if opt.restore_snapshot.is_some() {
            let counters = if openvmm_defs::profile::enabled() {
                snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.artifact_size_counters().ok())
                    .map(|(logical_bytes, allocated_bytes)| {
                        openvmm_defs::profile::ProfileCounters {
                            logical_bytes: Some(logical_bytes),
                            allocated_bytes: Some(allocated_bytes),
                            ..Default::default()
                        }
                    })
                    .unwrap_or_default()
            } else {
                Default::default()
            };
            artifact_open.complete("restore", "artifact_open", counters);
        }
        Ok(Self {
            snapshot,
            worker: WorkerRestore::default(),
        })
    }

    /// Validates the opened snapshot generation against the VM configuration
    /// and prepares it for the VM worker.
    ///
    /// Returns the worker's copy-on-write guest RAM and saved state, and
    /// records the worker's other restore inputs for [`Self::into_worker`].
    pub(crate) fn prepare(
        &mut self,
        opt: &Options,
    ) -> anyhow::Result<(SharedMemoryFd, ProtobufMessage)> {
        let prepared = prepare::prepare_snapshot_restore(
            self.snapshot
                .take()
                .context("snapshot restore is missing its opened generation")?,
            opt,
        )?;
        self.worker.shared_memory_copy_on_write = true;
        self.worker.guards = Some(prepared.guards);
        Ok((prepared.shared_memory, prepared.saved_state))
    }

    /// Returns the restore inputs of the VM worker: the defaults of a VM that
    /// is not restored, or those recorded by [`Self::prepare`].
    pub(crate) fn into_worker(self) -> WorkerRestore {
        self.worker
    }
}

/// Records the `startup/worker_launch` profile milestone once the VM worker
/// has launched.
pub(crate) fn worker_launched(worker_launch: ProfileSpan) {
    worker_launch.complete_milestone("startup", "worker_launch", Default::default());
}
