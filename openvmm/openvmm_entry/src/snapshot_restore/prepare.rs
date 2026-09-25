// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Validation of an opened snapshot generation against the VM configuration,
//! and preparation of the copy-on-write guest RAM and saved state it restores.

use crate::Options;
use anyhow::Context;

/// An opened snapshot generation, validated against the VM configuration and
/// prepared for the VM worker.
pub(crate) struct PreparedSnapshotRestore {
    pub(crate) shared_memory: openvmm_defs::worker::SharedMemoryFd,
    pub(crate) guards: openvmm_defs::worker::SnapshotRestoreGuards,
    pub(crate) saved_state: mesh::payload::message::ProtobufMessage,
}

/// Validate an opened snapshot generation against the current VM config.
/// Returns the shared memory handle, lifetime guards, and saved device state.
pub(super) fn prepare_snapshot_restore(
    snapshot: openvmm_helpers::snapshot::restore::OpenedSnapshot,
    opt: &Options,
) -> anyhow::Result<PreparedSnapshotRestore> {
    prepare_snapshot_restore_for_config(snapshot, opt.memory_size(), opt.processors)
}

pub(crate) fn prepare_snapshot_restore_for_config(
    snapshot: openvmm_helpers::snapshot::restore::OpenedSnapshot,
    expected_memory_size: u64,
    expected_vp_count: u32,
) -> anyhow::Result<PreparedSnapshotRestore> {
    let artifact_prepare = openvmm_defs::profile::ProfileSpan::start();
    let manifest = snapshot.manifest();
    // Validate manifest against current VM config.
    openvmm_helpers::snapshot::validate_manifest(
        manifest,
        crate::GUEST_ARCH,
        expected_memory_size,
        expected_vp_count,
        crate::system_page_size(),
    )?;

    let state_msg: mesh::payload::message::ProtobufMessage =
        mesh::payload::decode(snapshot.state_bytes())
            .context("failed to decode saved state from snapshot")?;

    artifact_prepare.complete(
        "restore",
        "artifact_prepare",
        openvmm_defs::profile::ProfileCounters {
            logical_bytes: Some(expected_memory_size),
            ..Default::default()
        },
    );

    // Create the private mapping from a duplicate of the exact opened handle.
    // The original file and directory handles move to the worker and keep this
    // generation pinned until VM teardown.
    let cow_section_create = openvmm_defs::profile::ProfileSpan::start();
    let memory_file = snapshot.duplicate_memory_file_for_mapping(expected_memory_size)?;
    let shared_memory =
        openvmm_helpers::shared_memory::file_to_copy_on_write_memory_fd(memory_file)?;
    cow_section_create.complete(
        "restore",
        "cow_section_create",
        openvmm_defs::profile::ProfileCounters {
            logical_bytes: Some(expected_memory_size),
            ..Default::default()
        },
    );
    snapshot.validate_memory_generation(expected_memory_size)?;
    let (_, _, guards) = snapshot.into_parts();

    Ok(PreparedSnapshotRestore {
        shared_memory,
        guards,
        saved_state: state_msg,
    })
}
