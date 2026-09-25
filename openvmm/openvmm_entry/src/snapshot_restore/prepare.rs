// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Validation of an opened snapshot generation against the VM configuration,
//! and preparation of the copy-on-write guest RAM and saved state it restores.

use crate::Options;
use crate::microvm;
use anyhow::Context;
use std::time::Duration;

/// An opened snapshot generation, validated against the VM configuration and
/// prepared for the VM worker.
pub(crate) struct PreparedSnapshotRestore {
    pub(crate) shared_memory: openvmm_defs::worker::SharedMemoryFd,
    pub(crate) guards: openvmm_defs::worker::SnapshotRestoreGuards,
    pub(crate) saved_state: mesh::payload::message::ProtobufMessage,
    pub(crate) restore_time: Option<(Duration, u64, Option<u64>, Vec<u8>)>,
}

/// Validate an opened snapshot generation against the current VM config.
/// Returns the shared memory handle, lifetime guards, and saved device state.
pub(super) fn prepare_snapshot_restore(
    snapshot: openvmm_helpers::snapshot::restore::OpenedSnapshot,
    opt: &Options,
    microvm: &microvm::MicrovmLaunch,
    expected_hypervisor: &str,
) -> anyhow::Result<PreparedSnapshotRestore> {
    let base_memory_size = snapshot.manifest().memory_size_bytes;
    let expected_microvm_contract =
        microvm.expected_restore_contract(opt, snapshot.manifest(), expected_hypervisor)?;
    prepare_snapshot_restore_for_config(
        snapshot,
        base_memory_size,
        opt.memory_size(),
        opt.processors,
        expected_microvm_contract,
    )
}

pub(crate) fn prepare_snapshot_restore_for_config(
    snapshot: openvmm_helpers::snapshot::restore::OpenedSnapshot,
    expected_memory_size: u64,
    selected_memory_size: u64,
    expected_vp_count: u32,
    expected_microvm_contract: Option<microvm::ExpectedRestoreContract<'_>>,
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
    let restore_time = expected_microvm_contract
        .map(|contract| {
            microvm::validate_restore_contract(
                manifest,
                expected_memory_size,
                expected_vp_count,
                contract,
            )
        })
        .transpose()?;

    // The manifest and state.bin inventories describe the same machine boundary.
    // Require them to agree before worker and partition construction.
    let state_msg: mesh::payload::message::ProtobufMessage =
        mesh::payload::decode(snapshot.state_bytes())
            .context("failed to decode saved state from snapshot")?;
    if let Some(contract) = &manifest.machine_contract {
        let inventory_msg: mesh::payload::message::ProtobufMessage =
            mesh::payload::decode(snapshot.state_bytes())
                .context("failed to decode saved state inventory from snapshot")?;
        let saved_state: openvmm_defs::worker::SavedState = inventory_msg
            .parse()
            .context("failed to parse saved state inventory from snapshot")?;
        anyhow::ensure!(
            saved_state.inventory == contract.state_unit_names,
            "snapshot manifest state-unit inventory does not match state.bin"
        );
    }

    snapshot.claim_for_restore()?;

    artifact_prepare.complete(
        "restore",
        "artifact_prepare",
        openvmm_defs::profile::ProfileCounters {
            logical_bytes: Some(selected_memory_size),
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
        restore_time,
    })
}
