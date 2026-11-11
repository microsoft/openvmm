// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM snapshot restore packets and restore-time contract checks.

use super::console::MICROVM_CONSOLE_STABLE_ID;
use super::console::MICROVM_CONTROL_CONSOLE_STABLE_ID;
use super::console::microvm_console_discarding_listener_matches;
use super::console::microvm_console_listener_replacement_matches;
use super::filesystem::validate_microvm_filesystem_slot;
use crate::Options;
use crate::cli_args;
use crate::cli_args::DiskCliKind;
use crate::cli_args::microvm::MachineProfileCli;
use crate::storage_builder::microvm::MicrovmSandboxBlockGeometry;
use anyhow::Context;
use chipset_resources::microvm_time::MAX_RESTORE_ACTIVATION_LEN;
use chipset_resources::microvm_time::RESTORE_ENTROPY_LEN;
use chipset_resources::microvm_time::RestoreMemoryRange;
use chipset_resources::microvm_time::RestoreNetwork;
use chipset_resources::microvm_time::RestorePacketBase;
use chipset_resources::microvm_time::RestoreTimeRecord;
use net_backend_resources::egress::EgressPolicy;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::microvm::MicrovmNetworkConfig;
use openvmm_defs::microvm::MicrovmSandboxBlockRole;
use openvmm_defs::time_abi::RestoreTimeInput;
use openvmm_defs::time_abi::SnapshotCpuProfile;
use openvmm_defs::time_abi::SnapshotTimeContract;
use openvmm_helpers::snapshot::SnapshotManifest;
use openvmm_helpers::snapshot::microvm::MicrovmFilesystemSource;
use openvmm_helpers::snapshot::microvm::SnapshotAttachment;
use openvmm_helpers::snapshot::microvm::SnapshotMachineContract;
use openvmm_helpers::snapshot::microvm::SnapshotMemoryExpansionRange;
use openvmm_helpers::snapshot::microvm::SnapshotMicrovmSandboxBlock;
use openvmm_helpers::snapshot::restore::OpenedSnapshot;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use virt::time_abi::HostIdentity;
use virt::time_abi::TimeAbiTestHooks;

fn microvm_generation_id(entropy: &[u8; RESTORE_ENTROPY_LEN]) -> [u8; 16] {
    let mut generation_id = [0; 16];
    generation_id.copy_from_slice(&entropy[..16]);
    generation_id
}

pub(crate) fn fresh_microvm_generation_id() -> anyhow::Result<[u8; 16]> {
    let mut generation_id = [0; 16];
    getrandom::fill(&mut generation_id).context("failed to generate microVM generation ID")?;
    Ok(generation_id)
}

fn align_restore_console_listener_contract(
    saved: &SnapshotMachineContract,
    expected: &mut SnapshotMachineContract,
) {
    align_restore_console_listener_attachments(&saved.attachments, &mut expected.attachments);
}

fn align_restore_console_listener_attachments(
    saved: &[SnapshotAttachment],
    expected: &mut [SnapshotAttachment],
) {
    for stable_id in [MICROVM_CONSOLE_STABLE_ID, MICROVM_CONTROL_CONSOLE_STABLE_ID] {
        let Some(saved_attachment) = saved
            .iter()
            .find(|attachment| attachment.stable_id == stable_id)
        else {
            continue;
        };
        let Some(expected_attachment) = expected
            .iter_mut()
            .find(|attachment| attachment.stable_id == stable_id)
        else {
            continue;
        };
        if microvm_console_listener_replacement_matches(saved_attachment, expected_attachment) {
            expected_attachment.clone_from(saved_attachment);
        }
    }
}

/// Returns the boot console attachment from which a restore builds the
/// machine contract that the snapshot must match: the saved disconnected
/// attachment, if the restore attached a listener to it, which discards guest
/// output while no client is connected as the disconnected attachment does,
/// and `expected`, the restore's own attachment, otherwise.
fn restore_console_contract_attachment<'a>(
    saved: &'a [SnapshotAttachment],
    expected: Option<&'a SnapshotAttachment>,
) -> Option<&'a SnapshotAttachment> {
    let expected = expected?;
    saved
        .iter()
        .find(|attachment| attachment.stable_id == MICROVM_CONSOLE_STABLE_ID)
        .filter(|saved| microvm_console_discarding_listener_matches(saved, expected))
        .or(Some(expected))
}

/// Restore-time inputs that shape a microVM configuration.
#[derive(Default)]
pub(crate) struct MicrovmRestore {
    /// The authoritative machine contract of the snapshot being restored.
    pub(crate) machine_contract: Option<SnapshotMachineContract>,
    /// Whether the guest must complete post-restore repair before readiness.
    pub(crate) gate_required: bool,
    /// Whether the restore packet carries the network settings of the
    /// restore's policy, which the guest applies before it acknowledges the
    /// gated restore.
    pub(crate) network_settings: bool,
    /// The activation record that `--restore-activation-record` hands a
    /// platform snapshot's clone in its restore packet, which the guest reads
    /// before it acknowledges the gated restore.
    pub(crate) activation_record: Option<Vec<u8>>,
    /// Whether `--restore-memory` selected an explicit RAM target.
    pub(crate) memory_target_requested: bool,
    /// Fresh private GPA ranges selected for this restore.
    pub(crate) memory_ranges: Vec<SnapshotMemoryExpansionRange>,
    /// Private copy of a paired scratch image, kept alive for the VM lifetime.
    pub(crate) private_scratch_dir: Option<tempfile::TempDir>,
    /// The paired scratch of a direct-claimed resume snapshot, which the
    /// restore attaches in place: the opened generation's own `scratch.img`,
    /// open for writing through the snapshot's directory handle, with no other
    /// name, and its path. Only the VM writes it, and the VM worker starts
    /// only after [`OpenedSnapshot::claim_for_restore`] has made the claim
    /// durable.
    pub(crate) claimed_scratch: Option<(std::fs::File, PathBuf)>,
    /// The captured geometry of a platform snapshot's `distro` block, which
    /// the restore presents over an image that may be shorter.
    pub(crate) distro_capacity: Option<MicrovmSandboxBlockGeometry>,
    /// The captured geometry of a platform snapshot's fresh scratch, which
    /// `--restore-absent-scratch` attaches with no file.
    pub(crate) absent_scratch: Option<MicrovmSandboxBlockGeometry>,
}

impl MicrovmRestore {
    /// Returns the time ABI records of the snapshot being restored: its time
    /// contract and CPU profile record.
    pub(crate) fn time_abi_records(&self) -> Option<(&SnapshotTimeContract, &SnapshotCpuProfile)> {
        let contract = self.machine_contract.as_ref()?;
        contract.time.as_ref().zip(contract.cpu_profile.as_ref())
    }

    /// Returns the controller's fields of the restore packet for a time ABI
    /// restore, with `network`, the network settings of the restore's policy
    /// when [`network_settings`](Self::network_settings) asks for them, and
    /// the [`activation_record`](Self::activation_record), if any, and the
    /// generation ID they imply. The generation counter is the snapshot's
    /// plus one.
    pub(crate) fn time_abi_restore_packet(
        &self,
        restore_online_vp_count: Option<u32>,
        network: Option<RestoreNetwork>,
    ) -> anyhow::Result<([u8; 16], RestorePacketBase)> {
        let (time, _) = self
            .time_abi_records()
            .context("time ABI restore requires the snapshot's time contract")?;
        restore_packet_base(
            time,
            restore_online_vp_count,
            self.memory_target_requested,
            &self.memory_ranges,
            self.gate_required,
            network,
            self.activation_record.clone(),
        )
    }
}

/// Returns the controller's fields of the restore packet for a time ABI
/// restore of a snapshot with the time contract `time`, and the generation ID
/// they imply. The generation counter is the snapshot's plus one,
/// `ack_required` makes the guest acknowledge the restore through port
/// `0x605`, `network` carries the network settings of a restore that replaces
/// the network policy of a snapshot without a tier, and `activation` the
/// activation record of a platform snapshot's clone.
pub(crate) fn restore_packet_base(
    time: &SnapshotTimeContract,
    restore_online_vp_count: Option<u32>,
    memory_target_requested: bool,
    memory_ranges: &[SnapshotMemoryExpansionRange],
    ack_required: bool,
    network: Option<RestoreNetwork>,
    activation: Option<Vec<u8>>,
) -> anyhow::Result<([u8; 16], RestorePacketBase)> {
    let generation = time.capture_generation.checked_add(1).ok_or_else(|| {
        virt::time_abi::TimeAbiError::new(
            virt::time_abi::TimeAbiCode::GenerationExhausted,
            "the snapshot's generation counter cannot be incremented",
        )
    })?;
    anyhow::ensure!(
        memory_target_requested || memory_ranges.is_empty(),
        "restore memory ranges require an explicit memory target"
    );
    let generation_id_create = openvmm_defs::profile::ProfileSpan::start();
    let mut entropy = [0_u8; RESTORE_ENTROPY_LEN];
    getrandom::fill(&mut entropy).context("failed to generate restore entropy")?;
    let base = RestorePacketBase {
        online_vp_count: restore_online_vp_count
            .map(u8::try_from)
            .transpose()
            .context("restore-online VP count does not fit in u8")?
            .unwrap_or(0),
        memory_target: memory_target_requested,
        ack_required,
        generation,
        ranges: memory_ranges
            .iter()
            .map(|range| RestoreMemoryRange {
                gpa_start: range.gpa_start,
                length: range.length,
            })
            .collect(),
        entropy,
        network,
        activation,
    };
    base.validate().context("restore packet is invalid")?;
    generation_id_create.complete("restore", "generation_id_create", Default::default());
    Ok((microvm_generation_id(&entropy), base))
}

/// Validates a snapshot restore against the microVM profile and prepares the
/// restore-time inputs of the microVM configuration.
///
/// This may adjust `opt`: the snapshot selects the RAM size, a paired scratch
/// image is replaced by a private copy, and restore gates require entropy. A
/// direct-claimed resume snapshot's paired scratch instead stays in place: the
/// restore opens the opened generation's own `scratch.img` for the VM, and
/// gives it no other name, so that a restore that fails, or loses the claim
/// that precedes the VM worker, leaves nothing beside the snapshot that names
/// it. A platform snapshot's `distro` block keeps its captured capacity,
/// whatever the length of the image that the restore attaches to it, and
/// `--restore-absent-scratch` attaches its fresh scratch with no file. Only a
/// platform snapshot, or one without a tier, admits
/// `--restore-network-policy`, and only a platform snapshot admits
/// `--mount-aggregate` on the filesystem slot that it left dormant, and
/// `--restore-activation-record`. A restore that replaces the network policy
/// of a snapshot without a tier hands the guest the new policy's network
/// settings in its restore packet, and gates host input until the guest has
/// applied them, and so does a restore that hands a platform snapshot's clone
/// its activation record, until the guest has read it.
pub(crate) fn prepare_restore(
    opt: &mut Options,
    restore_snapshot: Option<&OpenedSnapshot>,
) -> anyhow::Result<MicrovmRestore> {
    let mut private_scratch_dir = None;
    let mut claimed_scratch = None;
    let mut distro_capacity = None;
    let mut absent_scratch = None;
    let mut restore_gate_required = false;
    let mut network_settings = false;
    let mut activation_record = None;
    let restore_memory_target_requested = opt.microvm.restore_memory.is_some();
    let mut restore_memory_ranges = Vec::new();
    if let Some(contract) =
        restore_snapshot.and_then(|snapshot| snapshot.manifest().machine_contract.as_ref())
        && contract.machine_profile == "microvm"
    {
        anyhow::ensure!(
            opt.machine == MachineProfileCli::Microvm,
            "microVM snapshot restore requires --machine microvm"
        );
        openvmm_helpers::snapshot::microvm::validate_supported_microvm_contract(contract)?;
    }
    let restore_machine_contract = if opt.machine == MachineProfileCli::Microvm
        && let Some(snapshot) = restore_snapshot
    {
        let manifest = snapshot.manifest();
        let snapshot_dir = opt
            .restore_snapshot
            .as_deref()
            .expect("restore manifest requires a snapshot path");
        restore_gate_required =
            openvmm_helpers::snapshot::microvm::requires_post_restore_gate(manifest);
        let contract = openvmm_helpers::snapshot::time::required_machine_contract(manifest)?;
        anyhow::ensure!(
            contract.machine_profile == "microvm",
            "snapshot machine profile does not match the requested microVM machine"
        );
        openvmm_helpers::snapshot::microvm::validate_supported_microvm_contract(contract)?;
        if let Some(time) = &contract.time {
            openvmm_helpers::snapshot::time::validate_time_contract(time)?;
        }
        validate_network_policy_restore(
            &manifest.snapshot_tier,
            opt.microvm.restore_network_policy,
        )?;
        if restore_packet_carries_network_settings(
            &manifest.snapshot_tier,
            opt.microvm.restore_network_policy,
        ) {
            network_settings = true;
            restore_gate_required = true;
        }
        if let Some(path) = opt.microvm.restore_activation_record.as_deref() {
            validate_activation_record_restore(&manifest.snapshot_tier)?;
            activation_record = Some(read_activation_record(path)?);
            restore_gate_required = true;
        }
        validate_filesystem_aggregate_restore(
            &manifest.snapshot_tier,
            contract.microvm_filesystem.is_some(),
            opt.microvm.microvm_mount_aggregate.is_some(),
        )?;
        if let Some(restore_processors) = opt.microvm.restore_processors {
            openvmm_helpers::snapshot::microvm::validate_restore_online_vp_count(
                manifest,
                restore_processors,
            )?;
            restore_gate_required = true;
        }
        let restore_memory_size = opt
            .microvm
            .restore_memory
            .map(|memory| memory.0)
            .unwrap_or(manifest.memory_size_bytes);
        if restore_memory_target_requested {
            restore_memory_ranges =
                openvmm_helpers::snapshot::microvm::validate_restore_memory_target(
                    manifest,
                    restore_memory_size,
                )?;
        }
        if !restore_memory_ranges.is_empty() {
            restore_gate_required = true;
        }
        anyhow::ensure!(
            opt.cmdline.is_empty(),
            "restore-time command-line overrides are not allowed"
        );
        anyhow::ensure!(
            opt.memory == Default::default()
                && !opt.deprecated_private_memory
                && !opt.deprecated_prefetch
                && !opt.deprecated_thp
                && opt.deprecated_memory_backing_file.is_none(),
            "restore-time memory overrides are not allowed"
        );
        opt.memory.size = Some(vmm_cli::MemorySize(restore_memory_size));
        validate_restore_generation(
            &contract.microvm_sandbox_blocks,
            opt.microvm
                .restore_generation_id
                .map(|generation| generation.0),
        )?;
        let blocks = plan_sandbox_block_restore(
            &manifest.snapshot_tier,
            &contract.microvm_sandbox_blocks,
            opt.microvm
                .microvm_sandbox_block
                .iter()
                .any(|block| block.role == MicrovmSandboxBlockRole::Scratch),
            opt.microvm.restore_absent_scratch,
        )?;
        distro_capacity = blocks.distro_capacity;
        absent_scratch = blocks.absent_scratch;
        if let Some(scratch) = blocks.paired_scratch {
            let restore_mode = if scratch.restore_mode.is_empty() {
                openvmm_helpers::snapshot::microvm::SNAPSHOT_SCRATCH_RESTORE_PRIVATE_COPY
            } else {
                scratch.restore_mode.as_str()
            };
            if restore_mode
                == openvmm_helpers::snapshot::microvm::SNAPSHOT_SCRATCH_RESTORE_DIRECT_CLAIMED
            {
                claimed_scratch = Some((
                    snapshot.open_direct_claimed_scratch_file()?,
                    snapshot_dir.join(openvmm_helpers::snapshot::format::SCRATCH_FILE_NAME),
                ));
            } else {
                let source = snapshot
                    .open_paired_scratch_file()?
                    .context("paired snapshot is missing scratch.img")?;
                let parent = snapshot_dir
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                let temp_dir = tempfile::Builder::new()
                    .prefix(".openvmm-private-scratch-")
                    .tempdir_in(parent)
                    .context("failed to create private restore scratch directory")?;
                let private_path = temp_dir
                    .path()
                    .join(openvmm_helpers::snapshot::format::SCRATCH_FILE_NAME);
                match restore_mode {
                    openvmm_helpers::snapshot::microvm::SNAPSHOT_SCRATCH_RESTORE_PRIVATE_COPY => {
                        openvmm_helpers::snapshot::restore::copy_paired_scratch_file(
                            &source,
                            &private_path,
                            scratch,
                        )?;
                    }
                    openvmm_helpers::snapshot::microvm::SNAPSHOT_SCRATCH_RESTORE_COPY_ON_WRITE => {
                        openvmm_helpers::snapshot::restore::clone_paired_scratch_file(
                            &source,
                            &private_path,
                            scratch,
                        )?;
                    }
                    mode => anyhow::bail!("snapshot scratch restore mode '{mode}' is unsupported"),
                }
                opt.microvm
                    .microvm_sandbox_block
                    .push(cli_args::microvm::MicrovmSandboxBlockCli {
                        role: MicrovmSandboxBlockRole::Scratch,
                        disk: cli_args::DiskCli {
                            vtl: DeviceVtl::Vtl0,
                            kind: DiskCliKind::File {
                                path: private_path,
                                create_with_len: None,
                                direct: false,
                            },
                            read_only: false,
                            is_dvd: false,
                            underhill: None,
                            pcie_port: None,
                            serial: None,
                            controller: None,
                            nsid: None,
                            lun: None,
                            relay: None,
                        },
                    });
                private_scratch_dir = Some(temp_dir);
            }
        }
        Some(contract.clone())
    } else {
        None
    };
    Ok(MicrovmRestore {
        machine_contract: restore_machine_contract,
        gate_required: restore_gate_required,
        network_settings,
        activation_record,
        memory_target_requested: restore_memory_target_requested,
        memory_ranges: restore_memory_ranges,
        private_scratch_dir,
        claimed_scratch,
        distro_capacity,
        absent_scratch,
    })
}

/// Rejects `--restore-network-policy`, which `restore_network_policy`
/// reports, for a snapshot of the `workload-start` or `instance-checkpoint`
/// tier.
///
/// A platform snapshot is captured before any tenant input. A snapshot
/// without a tier may hold a workload that has already run, but the option
/// requires a NIC captured with the complete deny-all, under which that
/// workload had no connectivity: its endpoint held no flows, listeners, or
/// forwards, and its guest learned no DNS server, so nothing in the snapshot
/// depends on the policy, and the restore grants the workload connectivity
/// that it never had. The later tiers continue a tenant's workload, which
/// keeps the policy that it was captured with.
pub(super) fn validate_network_policy_restore(
    snapshot_tier: &str,
    restore_network_policy: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !restore_network_policy
            || snapshot_tier.is_empty()
            || snapshot_tier == openvmm_helpers::snapshot::format::SNAPSHOT_TIER_PLATFORM,
        "--restore-network-policy requires a platform snapshot or a snapshot without a tier"
    );
    Ok(())
}

/// Returns whether the restore packet of a restore of a snapshot of tier
/// `snapshot_tier` carries the network settings of the restore's policy,
/// given whether `--restore-network-policy`, which `restore_network_policy`
/// reports, replaces the snapshot's policy.
///
/// A cold boot names the guest's DNS server, and asks the guest to announce
/// its listeners, on its command line, which a restore keeps, and a NIC
/// template is captured under the complete deny-all, which asks neither. A
/// platform clone learns them from its activation, after the restore, but a
/// snapshot without a tier has no activation, so its restore packet carries
/// them, and the restore gates host input until the guest has applied them.
fn restore_packet_carries_network_settings(
    snapshot_tier: &str,
    restore_network_policy: bool,
) -> bool {
    restore_network_policy && snapshot_tier.is_empty()
}

/// Rejects `--restore-activation-record` for a snapshot of a tier other than
/// the platform tier, `snapshot_tier`.
///
/// A platform snapshot is captured before any tenant input, so that each of
/// its clones takes the settings of its own launch, which its activation
/// record carries. A snapshot of a later tier continues a tenant's workload
/// with the settings that it was captured with, and one without a tier has no
/// activation.
fn validate_activation_record_restore(snapshot_tier: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        snapshot_tier == openvmm_helpers::snapshot::format::SNAPSHOT_TIER_PLATFORM,
        "--restore-activation-record requires a platform snapshot"
    );
    Ok(())
}

/// Reads the activation record at `path`, which the restore packet hands the
/// guest without interpreting it: 1 to [`MAX_RESTORE_ACTIVATION_LEN`] bytes,
/// of which it reads at most one more, so that a longer file fails without
/// being read whole.
fn read_activation_record(path: &Path) -> anyhow::Result<Vec<u8>> {
    let file = fs_err::File::open(path).context("failed to open the activation record")?;
    let mut record = Vec::new();
    file.take(MAX_RESTORE_ACTIVATION_LEN as u64 + 1)
        .read_to_end(&mut record)
        .with_context(|| format!("failed to read the activation record {}", path.display()))?;
    anyhow::ensure!(
        (1..=MAX_RESTORE_ACTIVATION_LEN).contains(&record.len()),
        "activation record {} must hold 1 to {MAX_RESTORE_ACTIVATION_LEN} bytes",
        path.display()
    );
    Ok(record)
}

/// Rejects `--mount-aggregate`, which `aggregate` reports, on the filesystem
/// slot that a snapshot of a tier other than the platform tier left dormant,
/// which `captured_filesystem` reports when false.
///
/// A platform snapshot is captured before any tenant input, so its restore
/// may attach the filesystem that a cold boot would. Later tiers, and
/// untiered snapshots, keep what their dormant slot admitted before, one
/// `--mount` directory, and a snapshot that captured a filesystem requires it
/// again, whatever its tier.
fn validate_filesystem_aggregate_restore(
    snapshot_tier: &str,
    captured_filesystem: bool,
    aggregate: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        captured_filesystem
            || !aggregate
            || snapshot_tier == openvmm_helpers::snapshot::format::SNAPSHOT_TIER_PLATFORM,
        "--mount-aggregate on a dormant filesystem slot requires a platform snapshot"
    );
    Ok(())
}

/// Checks `generation`, the storage generation of a restore's sandbox blocks
/// that `--restore-generation-id` states, against `blocks`, the sandbox
/// blocks of the snapshot.
///
/// A snapshot that binds its blocks, of the `workload-start` or
/// `instance-checkpoint` tier, requires the generation that it recorded, so
/// that a restore of other storage fails before it materializes the paired
/// scratch. A platform snapshot binds none and takes any generation, and a
/// snapshot without sandbox blocks takes none.
fn validate_restore_generation(
    blocks: &[SnapshotMicrovmSandboxBlock],
    generation: Option<[u8; 16]>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !blocks.is_empty() || generation.is_none(),
        "--restore-generation-id requires a snapshot with sandbox blocks"
    );
    let Some(recorded) = blocks.iter().find(|block| {
        block.identity_kind
            == openvmm_helpers::snapshot::microvm::SNAPSHOT_BLOCK_IDENTITY_GENERATION
    }) else {
        return Ok(());
    };
    let generation = generation.context(
        "the snapshot binds its sandbox blocks to a storage generation; pass it with --restore-generation-id",
    )?;
    let hex = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    anyhow::ensure!(
        recorded.identity == generation,
        "the snapshot binds its sandbox blocks to storage generation {}, not {}",
        hex(&recorded.identity),
        hex(&generation)
    );
    Ok(())
}

/// How a restore attaches the sandbox blocks of a snapshot.
#[derive(Debug, Default, PartialEq, Eq)]
struct SandboxBlockRestore<'a> {
    /// The paired scratch, whose `scratch.img` the restore attaches.
    paired_scratch: Option<&'a SnapshotMicrovmSandboxBlock>,
    /// The captured geometry of a platform snapshot's `distro` block, which
    /// the restore presents over an image that may be shorter.
    distro_capacity: Option<MicrovmSandboxBlockGeometry>,
    /// The captured geometry of a platform snapshot's fresh scratch, which the
    /// restore attaches with no file.
    absent_scratch: Option<MicrovmSandboxBlockGeometry>,
}

/// Plans how a restore attaches `blocks`, the sandbox blocks of a snapshot of
/// tier `snapshot_tier`, given whether the caller passed a scratch block and
/// whether it declared the scratch absent.
///
/// A paired snapshot supplies its own scratch. A fresh-scratch snapshot, the
/// platform tier's, takes the caller's, or attaches the scratch with no file
/// when the caller declares it absent; no other scratch may be absent. A
/// platform restore also presents the captured capacity of its `distro` block
/// whatever the image's length; other tiers keep exact lengths.
fn plan_sandbox_block_restore<'a>(
    snapshot_tier: &str,
    blocks: &'a [SnapshotMicrovmSandboxBlock],
    scratch_block: bool,
    absent_scratch: bool,
) -> anyhow::Result<SandboxBlockRestore<'a>> {
    const ABSENT_SCRATCH_REQUIRES_PLATFORM: &str =
        "--restore-absent-scratch requires the fresh scratch of a platform snapshot";
    if blocks.is_empty() {
        anyhow::ensure!(!absent_scratch, ABSENT_SCRATCH_REQUIRES_PLATFORM);
        return Ok(SandboxBlockRestore::default());
    }
    let scratch = blocks
        .last()
        .filter(|block| block.role == MicrovmSandboxBlockRole::Scratch.as_str())
        .context("microVM snapshot is missing its scratch contract")?;
    let platform = snapshot_tier == openvmm_helpers::snapshot::format::SNAPSHOT_TIER_PLATFORM;
    let paired_scratch = (scratch.artifact == openvmm_helpers::snapshot::format::SCRATCH_FILE_NAME)
        .then_some(scratch);
    anyhow::ensure!(
        !absent_scratch || (platform && paired_scratch.is_none()),
        ABSENT_SCRATCH_REQUIRES_PLATFORM
    );
    if paired_scratch.is_some() {
        anyhow::ensure!(
            !scratch_block,
            "paired snapshot restore supplies scratch.img; do not pass a scratch block"
        );
    } else if absent_scratch {
        anyhow::ensure!(
            !scratch_block,
            "--restore-absent-scratch attaches the scratch block with no file; do not pass a scratch block"
        );
    } else {
        anyhow::ensure!(
            scratch_block,
            "fresh-scratch snapshot restore requires a scratch block or --restore-absent-scratch"
        );
    }
    let distro_capacity = blocks
        .iter()
        .find(|block| block.role == MicrovmSandboxBlockRole::Distro.as_str())
        .filter(|_| platform)
        .map(MicrovmSandboxBlockGeometry::of);
    Ok(SandboxBlockRestore {
        paired_scratch,
        distro_capacity,
        absent_scratch: absent_scratch.then(|| MicrovmSandboxBlockGeometry::of(scratch)),
    })
}

/// The machine contract a microVM snapshot must match to be restored: the
/// hypervisor, effective command line, network, filesystem, boot and control
/// console attachments, and sandbox blocks.
pub(crate) type ExpectedRestoreContract<'a> = (
    &'a str,
    &'a str,
    Option<(
        &'a MicrovmNetworkConfig,
        &'a EgressPolicy,
        &'a SnapshotAttachment,
    )>,
    Option<MicrovmFilesystemSource<'a>>,
    Option<&'a SnapshotAttachment>,
    Option<&'a SnapshotAttachment>,
    Vec<SnapshotMicrovmSandboxBlock>,
);

/// The time ABI options of a microVM restore.
#[derive(Clone, Copy)]
pub(crate) struct TimeAbiRestoreOptions<'a> {
    /// The requested CPU profile: `auto` or a profile ID.
    pub(crate) cpu_profile: &'a str,
    /// The active test hooks.
    pub(crate) hooks: &'a TimeAbiTestHooks,
}

/// A time ABI restore that the controller validated: the time ABI preflight
/// passed against this host, and the snapshot's machine contract matches the
/// restore's configuration.
pub(crate) struct TimeAbiRestore {
    contract: SnapshotTimeContract,
    cpu_profile: SnapshotCpuProfile,
    destination: HostIdentity,
}

impl TimeAbiRestore {
    /// Returns the worker's restore input. The worker seals the time fields
    /// of the restore packet through `restore_record`, and `packet_selected`,
    /// when present, reports the guest's first selection of the packet.
    pub(crate) fn into_input(
        self,
        restore_record: mesh::OneshotSender<RestoreTimeRecord>,
        packet_selected: Option<mesh::OneshotReceiver<()>>,
    ) -> RestoreTimeInput {
        RestoreTimeInput {
            contract: self.contract,
            cpu_profile: self.cpu_profile,
            destination: self.destination,
            restore_record,
            packet_selected,
        }
    }
}

/// Validates the authoritative machine contract of a microVM snapshot against
/// the restore-time configuration.
///
/// The manifest must carry the time ABI
/// records: the time ABI preflight checks them, the backend, the CPU profile,
/// and the downtime against this host before the rest of the contract is
/// compared.
pub(crate) fn validate_restore_contract(
    manifest: &SnapshotManifest,
    expected_memory_size: u64,
    expected_vp_count: u32,
    (
        expected_hypervisor,
        effective_command_line,
        network,
        filesystem,
        console_attachment,
        control_console_attachment,
        sandbox_blocks,
    ): ExpectedRestoreContract<'_>,
    time_abi: TimeAbiRestoreOptions<'_>,
) -> anyhow::Result<TimeAbiRestore> {
    let saved_contract = openvmm_helpers::snapshot::time::required_machine_contract(manifest)?;
    let destination = preflight_time_abi_restore(saved_contract, expected_hypervisor, time_abi)?;
    let (Some(contract), Some(cpu_profile)) = (
        saved_contract.time.clone(),
        saved_contract.cpu_profile.clone(),
    ) else {
        unreachable!("the time ABI preflight requires both records");
    };
    let network = network.map(|(config, policy, attachment)| (config, policy, attachment.clone()));
    validate_microvm_filesystem_slot(saved_contract)?;
    // A dormant-slot snapshot captured no filesystem, so a filesystem that the
    // restore attaches to the slot is not part of the snapshot's contract.
    let filesystem = filesystem.filter(|_| saved_contract.microvm_filesystem.is_some());
    let console_attachment =
        restore_console_contract_attachment(&saved_contract.attachments, console_attachment);
    let mut expected_contract = openvmm_helpers::snapshot::microvm::microvm_machine_contract(
        expected_hypervisor,
        openvmm_helpers::snapshot::microvm::MICROVM_BOOT_LAYOUT_VERSION,
        effective_command_line.to_owned(),
        network,
        filesystem,
        console_attachment.cloned(),
        control_console_attachment.cloned(),
        sandbox_blocks,
        expected_vp_count,
        expected_memory_size,
        (saved_contract.memory_expansion_version != 0)
            .then_some(saved_contract.memory_capacity_bytes),
        saved_contract.state_unit_names.clone(),
        contract,
        cpu_profile,
    )?;
    align_restore_console_listener_contract(saved_contract, &mut expected_contract);
    openvmm_helpers::snapshot::microvm::validate_microvm_machine_contract(
        manifest,
        &expected_contract,
    )?;
    // The validation leaves the time ABI records to the preflight, so take
    // them back rather than copying them.
    let (Some(contract), Some(cpu_profile)) = (
        expected_contract.time.take(),
        expected_contract.cpu_profile.take(),
    ) else {
        unreachable!("the expected contract holds the records it was built with");
    };
    Ok(TimeAbiRestore {
        contract,
        cpu_profile,
        destination,
    })
}

/// Runs the controller's time ABI preflight against this host, and returns
/// the host's identity.
fn preflight_time_abi_restore(
    contract: &SnapshotMachineContract,
    hypervisor: &str,
    options: TimeAbiRestoreOptions<'_>,
) -> anyhow::Result<HostIdentity> {
    let destination = virt::time_abi::host::host_identity()?;
    let now = virt::time_abi::host::sample_host_time()?;
    let preflight = openvmm_helpers::snapshot::time::preflight_time_abi_restore(
        contract,
        hypervisor,
        options.cpu_profile,
        openvmm_helpers::snapshot::time::host_cpu(),
        &destination,
        &now,
        options.hooks,
    )?;
    if let Some(step_ns) = preflight.downtime.host_wall_clock_step_ns {
        tracing::warn!(
            step_ns,
            "host wall clock was stepped since capture; the downtime uses host monotonic time"
        );
    }
    tracing::info!(
        downtime_ns = preflight.downtime.nanos,
        source = ?preflight.downtime.source,
        generation = preflight.generation,
        cpu_profile = preflight.cpu_profile,
        "time ABI restore preflight passed"
    );
    Ok(destination)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use openvmm_helpers::snapshot::microvm::SnapshotMemoryExpansionRange;
    use test_with_tracing::test;

    /// Returns a valid time contract for test machine contracts.
    pub(crate) fn test_time_contract() -> SnapshotTimeContract {
        SnapshotTimeContract {
            time_abi_version: virt::time_abi::TIME_ABI_VERSION,
            tsc_frequency_hz: 2_100_000_000,
            tsc_tolerance_ppm: 250,
            apic_frequency_hz: virt::time_abi::rate::LAPIC_HZ_HYPERV,
            capture_tsc: 1,
            capture_utc_ns: 2,
            capture_monotonic_ns: 3,
            host_clock: "linux-boottime".to_owned(),
            host_id: vec![1; 16],
            host_boot_id: vec![2; 16],
            capture_generation: 0,
            vm_time_cut_to_anchor_ns: 0,
        }
    }

    /// Returns a well-formed CPU profile record for test machine contracts.
    pub(crate) fn test_cpu_profile() -> SnapshotCpuProfile {
        SnapshotCpuProfile {
            id: "intel.skylake-sp.v1".to_owned(),
            sha256: vec![0; 32],
            profile: vec![1],
            effective_cpuid: openvmm_defs::time_abi::encode_effective_cpuid([
                openvmm_defs::time_abi::EffectiveCpuidEntry {
                    function: 0,
                    index: None,
                    result: [0xd, 0, 0, 0],
                    mask: [!0, 0, 0, 0],
                },
            ]),
            capture_cpu_signature: 0,
        }
    }

    #[test]
    fn listener_replacement_alignment_preserves_saved_machine_contract() {
        let saved_attachment = SnapshotAttachment {
            stable_id: MICROVM_CONSOLE_STABLE_ID.to_owned(),
            kind: "virtio-console".to_owned(),
            required: false,
            reconnect_policy: "recreate-listener".to_owned(),
            identity_kind: "unix-socket".to_owned(),
            identity: b"source.sock".to_vec(),
            length: 0,
            reconnect_timeout_ms: 0,
        };
        let mut requested_attachment = saved_attachment.clone();
        requested_attachment.identity = b"restored.sock".to_vec();
        let saved = vec![saved_attachment.clone()];
        let mut expected = vec![requested_attachment];

        align_restore_console_listener_attachments(&saved, &mut expected);

        assert_eq!(expected, vec![saved_attachment]);
    }

    #[test]
    fn a_restore_listener_of_a_disconnected_boot_console_keeps_the_saved_contract() {
        let saved = SnapshotAttachment {
            stable_id: MICROVM_CONSOLE_STABLE_ID.to_owned(),
            kind: "virtio-console".to_owned(),
            required: false,
            reconnect_policy: "discard-while-disconnected".to_owned(),
            identity_kind: "disconnected".to_owned(),
            identity: b"discard".to_vec(),
            length: 0,
            reconnect_timeout_ms: 0,
        };
        let listener = SnapshotAttachment {
            identity_kind: "unix-socket".to_owned(),
            identity: b"/snapshots/clone.console".to_vec(),
            ..saved.clone()
        };
        let saved_attachments = [saved.clone()];

        // The restore's contract records the saved disconnected attachment,
        // whose device discards output as the listener's does.
        assert_eq!(
            restore_console_contract_attachment(&saved_attachments, Some(&listener)),
            Some(&saved)
        );

        // Any other attachment is checked as the restore supplies it.
        let retaining = SnapshotAttachment {
            reconnect_policy: "recreate-listener".to_owned(),
            ..listener.clone()
        };
        assert_eq!(
            restore_console_contract_attachment(&saved_attachments, Some(&retaining)),
            Some(&retaining)
        );
        assert_eq!(
            restore_console_contract_attachment(&[], Some(&listener)),
            Some(&listener)
        );
        assert_eq!(
            restore_console_contract_attachment(&saved_attachments, None),
            None
        );
    }

    #[test]
    fn generation_id_is_the_entropy_prefix() {
        let mut entropy = [0x5a; RESTORE_ENTROPY_LEN];
        entropy[16] = 0xa5;
        assert_eq!(microvm_generation_id(&entropy), [0x5a; 16]);
    }

    /// Returns the contract of a sandbox block of `role` and `length` bytes,
    /// with a platform snapshot's identity.
    fn sandbox_block(role: &str, length: u64) -> SnapshotMicrovmSandboxBlock {
        let scratch = role == MicrovmSandboxBlockRole::Scratch.as_str();
        SnapshotMicrovmSandboxBlock {
            role: role.to_owned(),
            read_only: !scratch,
            length,
            identity_kind: if scratch { "fresh" } else { "unbound" }.to_owned(),
            identity: Vec::new(),
            artifact: String::new(),
            logical_block_size: 512,
            physical_block_size: 4096,
            restore_mode: String::new(),
        }
    }

    #[test]
    fn only_a_snapshot_that_binds_its_blocks_checks_the_restore_generation() {
        const GENERATION: [u8; 16] = [0x42; 16];
        let bound = |role: &str| SnapshotMicrovmSandboxBlock {
            identity_kind: openvmm_helpers::snapshot::microvm::SNAPSHOT_BLOCK_IDENTITY_GENERATION
                .to_owned(),
            identity: GENERATION.to_vec(),
            ..sandbox_block(role, 1 << 20)
        };
        let paired = [bound("distro"), bound("scratch")];
        validate_restore_generation(&paired, Some(GENERATION)).unwrap();
        assert_eq!(
            validate_restore_generation(&paired, Some([0x24; 16]))
                .unwrap_err()
                .to_string(),
            format!(
                "the snapshot binds its sandbox blocks to storage generation {}, not {}",
                "42".repeat(16),
                "24".repeat(16)
            )
        );
        assert_eq!(
            validate_restore_generation(&paired, None)
                .unwrap_err()
                .to_string(),
            "the snapshot binds its sandbox blocks to a storage generation; pass it with --restore-generation-id"
        );

        // A platform snapshot binds no block, so it takes any generation.
        let platform = [
            sandbox_block("distro", 1 << 30),
            sandbox_block("scratch", 1 << 20),
        ];
        for generation in [None, Some(GENERATION)] {
            validate_restore_generation(&platform, generation).unwrap();
        }

        // A snapshot without sandbox blocks has no block to bind.
        validate_restore_generation(&[], None).unwrap();
        assert_eq!(
            validate_restore_generation(&[], Some(GENERATION))
                .unwrap_err()
                .to_string(),
            "--restore-generation-id requires a snapshot with sandbox blocks"
        );
    }

    #[test]
    fn only_a_platform_restore_presents_the_distro_capacity() {
        use openvmm_helpers::snapshot::format::SCRATCH_FILE_NAME;
        use openvmm_helpers::snapshot::format::SNAPSHOT_TIER_PLATFORM;
        use openvmm_helpers::snapshot::format::SNAPSHOT_TIER_WORKLOAD_START;

        let blocks = [
            sandbox_block("distro", 1 << 30),
            sandbox_block("scratch", 1 << 20),
        ];
        assert_eq!(
            plan_sandbox_block_restore(SNAPSHOT_TIER_PLATFORM, &blocks, true, false).unwrap(),
            SandboxBlockRestore {
                paired_scratch: None,
                distro_capacity: Some(MicrovmSandboxBlockGeometry {
                    length: 1 << 30,
                    logical_block_size: 512,
                    physical_block_size: 4096,
                }),
                absent_scratch: None,
            }
        );
        // The fresh scratch of a platform snapshot needs a file unless it is
        // declared absent.
        let err =
            plan_sandbox_block_restore(SNAPSHOT_TIER_PLATFORM, &blocks, false, false).unwrap_err();
        assert_eq!(
            err.to_string(),
            "fresh-scratch snapshot restore requires a scratch block or --restore-absent-scratch"
        );

        // A paired snapshot of another tier keeps the exact lengths and
        // supplies its own scratch.
        let mut paired = blocks.clone();
        paired[1].artifact = SCRATCH_FILE_NAME.to_owned();
        assert_eq!(
            plan_sandbox_block_restore(SNAPSHOT_TIER_WORKLOAD_START, &paired, false, false)
                .unwrap(),
            SandboxBlockRestore {
                paired_scratch: Some(&paired[1]),
                distro_capacity: None,
                absent_scratch: None,
            }
        );
        let err = plan_sandbox_block_restore(SNAPSHOT_TIER_WORKLOAD_START, &paired, true, false)
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "paired snapshot restore supplies scratch.img; do not pass a scratch block"
        );

        // A snapshot without sandbox blocks has nothing to attach.
        assert_eq!(
            plan_sandbox_block_restore("", &[], false, false).unwrap(),
            SandboxBlockRestore::default()
        );
        let err = plan_sandbox_block_restore(SNAPSHOT_TIER_PLATFORM, &blocks[..1], true, false)
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "microVM snapshot is missing its scratch contract"
        );
    }

    #[test]
    fn only_a_platform_restore_attaches_an_absent_scratch() {
        use openvmm_helpers::snapshot::format::SCRATCH_FILE_NAME;
        use openvmm_helpers::snapshot::format::SNAPSHOT_TIER_INSTANCE_CHECKPOINT;
        use openvmm_helpers::snapshot::format::SNAPSHOT_TIER_PLATFORM;
        use openvmm_helpers::snapshot::format::SNAPSHOT_TIER_WORKLOAD_START;

        let blocks = [
            sandbox_block("distro", 1 << 30),
            sandbox_block("scratch", 1 << 20),
        ];
        // A platform restore attaches its fresh scratch at the captured
        // geometry, with no file.
        assert_eq!(
            plan_sandbox_block_restore(SNAPSHOT_TIER_PLATFORM, &blocks, false, true).unwrap(),
            SandboxBlockRestore {
                paired_scratch: None,
                distro_capacity: Some(MicrovmSandboxBlockGeometry::of(&blocks[0])),
                absent_scratch: Some(MicrovmSandboxBlockGeometry {
                    length: 1 << 20,
                    logical_block_size: 512,
                    physical_block_size: 4096,
                }),
            }
        );
        // An absent scratch has no file.
        let err =
            plan_sandbox_block_restore(SNAPSHOT_TIER_PLATFORM, &blocks, true, true).unwrap_err();
        assert_eq!(
            err.to_string(),
            "--restore-absent-scratch attaches the scratch block with no file; do not pass a scratch block"
        );

        // Outside a platform restore, the scratch is never absent: not for a
        // paired snapshot, with or without a scratch block, and not for a
        // snapshot without sandbox blocks.
        let mut paired = blocks.clone();
        paired[1].artifact = SCRATCH_FILE_NAME.to_owned();
        for tier in [
            SNAPSHOT_TIER_WORKLOAD_START,
            SNAPSHOT_TIER_INSTANCE_CHECKPOINT,
        ] {
            for scratch_block in [false, true] {
                let err =
                    plan_sandbox_block_restore(tier, &paired, scratch_block, true).unwrap_err();
                assert_eq!(
                    err.to_string(),
                    "--restore-absent-scratch requires the fresh scratch of a platform snapshot"
                );
            }
        }
        // The fresh scratch of a manifest of another tier, which the manifest
        // validation rejects, is not absent either.
        let err = plan_sandbox_block_restore(SNAPSHOT_TIER_WORKLOAD_START, &blocks, false, true)
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "--restore-absent-scratch requires the fresh scratch of a platform snapshot"
        );
        let err = plan_sandbox_block_restore("", &[], false, true).unwrap_err();
        assert_eq!(
            err.to_string(),
            "--restore-absent-scratch requires the fresh scratch of a platform snapshot"
        );
    }

    #[test]
    fn only_a_platform_or_untiered_restore_replaces_the_network_policy() {
        use openvmm_helpers::snapshot::format::SNAPSHOT_TIER_INSTANCE_CHECKPOINT;
        use openvmm_helpers::snapshot::format::SNAPSHOT_TIER_PLATFORM;
        use openvmm_helpers::snapshot::format::SNAPSHOT_TIER_WORKLOAD_START;

        for (tier, replaces) in [
            (SNAPSHOT_TIER_PLATFORM, true),
            ("", true),
            (SNAPSHOT_TIER_WORKLOAD_START, false),
            (SNAPSHOT_TIER_INSTANCE_CHECKPOINT, false),
        ] {
            validate_network_policy_restore(tier, false).unwrap();
            let replaced = validate_network_policy_restore(tier, true);
            if replaces {
                replaced.unwrap();
            } else {
                assert_eq!(
                    replaced.unwrap_err().to_string(),
                    "--restore-network-policy requires a platform snapshot or a snapshot without a tier",
                    "{tier}"
                );
            }
        }
    }

    #[test]
    fn only_an_untiered_restore_that_replaces_the_policy_carries_network_settings() {
        use openvmm_helpers::snapshot::format::SNAPSHOT_TIER_INSTANCE_CHECKPOINT;
        use openvmm_helpers::snapshot::format::SNAPSHOT_TIER_PLATFORM;
        use openvmm_helpers::snapshot::format::SNAPSHOT_TIER_WORKLOAD_START;

        for tier in [
            SNAPSHOT_TIER_PLATFORM,
            "",
            SNAPSHOT_TIER_WORKLOAD_START,
            SNAPSHOT_TIER_INSTANCE_CHECKPOINT,
        ] {
            assert!(!restore_packet_carries_network_settings(tier, false));
            // A platform clone learns its settings from its activation.
            assert_eq!(
                restore_packet_carries_network_settings(tier, true),
                tier.is_empty(),
                "{tier}"
            );
        }
    }

    #[test]
    fn only_a_platform_restore_hands_over_an_activation_record() {
        use openvmm_helpers::snapshot::format::SNAPSHOT_TIER_INSTANCE_CHECKPOINT;
        use openvmm_helpers::snapshot::format::SNAPSHOT_TIER_PLATFORM;
        use openvmm_helpers::snapshot::format::SNAPSHOT_TIER_WORKLOAD_START;

        validate_activation_record_restore(SNAPSHOT_TIER_PLATFORM).unwrap();
        for tier in [
            "",
            SNAPSHOT_TIER_WORKLOAD_START,
            SNAPSHOT_TIER_INSTANCE_CHECKPOINT,
        ] {
            assert_eq!(
                validate_activation_record_restore(tier)
                    .unwrap_err()
                    .to_string(),
                "--restore-activation-record requires a platform snapshot",
                "{tier}"
            );
        }
    }

    #[test]
    fn activation_records_hold_one_to_the_most_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record");
        for length in [1, 602, MAX_RESTORE_ACTIVATION_LEN] {
            let record: Vec<u8> = (0..length).map(|i| i as u8).collect();
            fs_err::write(&path, &record).unwrap();
            assert_eq!(read_activation_record(&path).unwrap(), record);
        }
        for length in [0, MAX_RESTORE_ACTIVATION_LEN + 1, 1 << 20] {
            fs_err::write(&path, vec![0x5a; length]).unwrap();
            let err = read_activation_record(&path).unwrap_err();
            assert!(
                err.to_string().ends_with(&format!(
                    "must hold 1 to {MAX_RESTORE_ACTIVATION_LEN} bytes"
                )),
                "{err:#}"
            );
        }
        assert!(read_activation_record(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn only_a_platform_restore_attaches_an_aggregate_to_a_dormant_slot() {
        use openvmm_helpers::snapshot::format::SNAPSHOT_TIER_INSTANCE_CHECKPOINT;
        use openvmm_helpers::snapshot::format::SNAPSHOT_TIER_PLATFORM;
        use openvmm_helpers::snapshot::format::SNAPSHOT_TIER_WORKLOAD_START;

        for tier in [
            SNAPSHOT_TIER_PLATFORM,
            SNAPSHOT_TIER_WORKLOAD_START,
            SNAPSHOT_TIER_INSTANCE_CHECKPOINT,
            "",
        ] {
            // A dormant slot may stay dormant or bind a --mount, and a slot
            // that the snapshot attached requires what it captured, at every
            // tier.
            for (captured_filesystem, aggregate) in [(false, false), (true, false), (true, true)] {
                validate_filesystem_aggregate_restore(tier, captured_filesystem, aggregate)
                    .unwrap();
            }
            let attached = validate_filesystem_aggregate_restore(tier, false, true);
            if tier == SNAPSHOT_TIER_PLATFORM {
                attached.unwrap();
            } else {
                assert_eq!(
                    attached.unwrap_err().to_string(),
                    "--mount-aggregate on a dormant filesystem slot requires a platform snapshot"
                );
            }
        }
    }

    fn time_abi_restore(capture_generation: u32) -> MicrovmRestore {
        let mut contract: SnapshotMachineContract = mesh::payload::decode(&[]).unwrap();
        contract.time = Some(SnapshotTimeContract {
            time_abi_version: virt::time_abi::TIME_ABI_VERSION,
            tsc_frequency_hz: 2_100_000_000,
            tsc_tolerance_ppm: 250,
            apic_frequency_hz: 200_000_000,
            capture_tsc: 1,
            capture_utc_ns: 2,
            capture_monotonic_ns: 3,
            host_clock: "linux-boottime".to_owned(),
            host_id: vec![1; 16],
            host_boot_id: vec![2; 16],
            capture_generation,
            vm_time_cut_to_anchor_ns: 0,
        });
        contract.cpu_profile = Some(SnapshotCpuProfile {
            id: "intel.skylake-sp.v1".to_owned(),
            sha256: Vec::new(),
            profile: Vec::new(),
            effective_cpuid: Vec::new(),
            capture_cpu_signature: 0,
        });
        MicrovmRestore {
            machine_contract: Some(contract),
            gate_required: true,
            network_settings: false,
            activation_record: None,
            memory_target_requested: true,
            memory_ranges: vec![SnapshotMemoryExpansionRange {
                gpa_start: 0x1_0000_0000,
                length: 0x4000_0000,
            }],
            private_scratch_dir: None,
            claimed_scratch: None,
            distro_capacity: None,
            absent_scratch: None,
        }
    }

    #[test]
    fn time_abi_restore_packet_carries_the_controller_fields() {
        let restore = time_abi_restore(6);
        let (generation_id, base) = restore.time_abi_restore_packet(Some(4), None).unwrap();
        assert_eq!(base.generation, 7);
        assert_eq!(base.online_vp_count, 4);
        assert!(base.memory_target && base.ack_required);
        assert_eq!(
            base.ranges,
            [RestoreMemoryRange {
                gpa_start: 0x1_0000_0000,
                length: 0x4000_0000,
            }]
        );
        assert_eq!(base.network, None);
        assert_eq!(generation_id, microvm_generation_id(&base.entropy));
        let (_, other) = restore.time_abi_restore_packet(None, None).unwrap();
        assert_eq!(other.online_vp_count, 0);
        assert_ne!(other.entropy, base.entropy);

        // The packet carries the network settings of an untiered restore
        // that replaces its policy.
        let network = RestoreNetwork {
            dns_server: Some(chipset_resources::microvm_time::RestoreDnsServer::Ipv4(
                std::net::Ipv4Addr::new(10, 0, 0, 1),
            )),
            announce_listeners: true,
        };
        let restore = MicrovmRestore {
            network_settings: true,
            ..time_abi_restore(6)
        };
        let (_, base) = restore
            .time_abi_restore_packet(None, Some(network))
            .unwrap();
        assert_eq!(base.network, Some(network));
        assert!(base.ack_required);

        // The packet carries a platform clone's activation record.
        let restore = MicrovmRestore {
            activation_record: Some(vec![0xa1, 0xa2]),
            ..time_abi_restore(6)
        };
        let (_, base) = restore.time_abi_restore_packet(None, None).unwrap();
        assert_eq!(base.activation.as_deref(), Some(&[0xa1, 0xa2][..]));
        assert_eq!(base.network, None);
        assert!(base.ack_required);

        assert!(restore.time_abi_restore_packet(Some(3), None).is_err());
        let err = time_abi_restore(u32::MAX)
            .time_abi_restore_packet(None, None)
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("[E_GENERATION_EXHAUSTED]"),
            "{err:#}"
        );
        assert!(
            MicrovmRestore::default()
                .time_abi_restore_packet(None, None)
                .is_err()
        );
    }

    /// The storage generation that the test instance checkpoint binds its
    /// sandbox blocks to.
    const CHECKPOINT_GENERATION: [u8; 16] = [0x42; 16];

    /// Publishes `root/snapshot`, an instance checkpoint of a one-processor
    /// microVM with a distro layer and the paired scratch `scratch`, which
    /// records the direct-claimed restore mode, and returns its path.
    fn publish_direct_claimed_checkpoint(root: &Path, scratch: &[u8]) -> PathBuf {
        use openvmm_defs::microvm::MicrovmSandboxBlockConfig;
        use openvmm_helpers::snapshot::format;
        use openvmm_helpers::snapshot::microvm;

        let roles = [
            MicrovmSandboxBlockRole::Distro,
            MicrovmSandboxBlockRole::Scratch,
        ];
        let mut command_line =
            openvmm_defs::microvm::build_microvm_command_line(&[], false).unwrap();
        openvmm_defs::microvm::append_microvm_virtio_discovery(
            &mut command_line,
            None,
            &[],
            false,
            false,
            &roles.map(|role| MicrovmSandboxBlockConfig {
                role,
                read_only: role.is_read_only(),
            }),
        )
        .unwrap();
        command_line.push_str(" nvx_snapshot_tier=");
        command_line.push_str(format::SNAPSHOT_TIER_INSTANCE_CHECKPOINT);
        let blocks = roles.map(|role| {
            let paired = role == MicrovmSandboxBlockRole::Scratch;
            SnapshotMicrovmSandboxBlock {
                role: role.as_str().to_owned(),
                read_only: role.is_read_only(),
                length: if paired {
                    scratch.len() as u64
                } else {
                    1 << 20
                },
                identity_kind: microvm::SNAPSHOT_BLOCK_IDENTITY_GENERATION.to_owned(),
                identity: CHECKPOINT_GENERATION.to_vec(),
                artifact: if paired {
                    format::SCRATCH_FILE_NAME
                } else {
                    ""
                }
                .to_owned(),
                logical_block_size: 512,
                physical_block_size: 4096,
                restore_mode: if paired {
                    microvm::SNAPSHOT_SCRATCH_RESTORE_DIRECT_CLAIMED
                } else {
                    ""
                }
                .to_owned(),
            }
        });
        let mut state_unit_names = [
            "partition",
            "vmtime",
            "pic",
            "ioapic",
            "pit",
            "rtc",
            "microvm-portb",
            "microvm-shutdown",
            "microvm-snapshot-request",
        ]
        .map(str::to_owned)
        .to_vec();
        state_unit_names.push(format!(
            "virtiofs-{}",
            openvmm_defs::microvm::MICROVM_FILESYSTEM_SLOTS[0].mmio_base
        ));
        state_unit_names.extend(roles.map(|role| format!("virtio-blk-{}", role.mmio_base())));
        let contract = microvm::microvm_machine_contract(
            "kvm",
            microvm::MICROVM_BOOT_LAYOUT_VERSION,
            command_line,
            None,
            None,
            None,
            None,
            blocks.to_vec(),
            1,
            1024,
            None,
            state_unit_names,
            test_time_contract(),
            test_cpu_profile(),
        )
        .unwrap();
        let manifest = SnapshotManifest {
            memory_size_bytes: 1024,
            vp_count: 1,
            page_size: 4096,
            architecture: "x86_64".to_owned(),
            machine_contract: Some(contract),
            snapshot_tier: format::SNAPSHOT_TIER_INSTANCE_CHECKPOINT.to_owned(),
            restore_policy: format::SNAPSHOT_RESTORE_POLICY_RESUME.to_owned(),
            consumed_config_sections: format::SNAPSHOT_CONFIG_INVARIANTS
                | format::SNAPSHOT_CONFIG_IMAGE_BINDING
                | format::SNAPSHOT_CONFIG_SANDBOX,
            ..Default::default()
        };
        let memory_source = root.join("memory-source");
        let scratch_source = root.join("scratch-source");
        std::fs::write(&memory_source, [0; 1024]).unwrap();
        std::fs::write(&scratch_source, scratch).unwrap();
        let snapshot_dir = root.join("snapshot");
        openvmm_helpers::snapshot::publish::write_snapshot_from_memory_and_scratch_files(
            &snapshot_dir,
            &manifest,
            b"state",
            &std::fs::File::open(&memory_source).unwrap(),
            Some(&std::fs::File::open(&scratch_source).unwrap()),
        )
        .unwrap();
        std::fs::remove_file(memory_source).unwrap();
        std::fs::remove_file(scratch_source).unwrap();
        snapshot_dir
    }

    /// Returns the options of a restore of the checkpoint at `snapshot_dir`.
    fn checkpoint_restore_options(snapshot_dir: &Path) -> Options {
        use clap::Parser as _;

        Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            snapshot_dir.to_str().unwrap(),
            "--restore-generation-id",
            &"42".repeat(16),
        ])
        .unwrap()
    }

    /// Returns the names in `directory`, sorted.
    fn directory_names(directory: &Path) -> Vec<String> {
        let mut names = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    /// A restore that opened an instance checkpoint before another restore
    /// claimed it loses the claim, and never names the checkpoint's
    /// direct-claimed scratch on its way: a name that it created and removed
    /// as it failed would outlive a crash in between, as a writable name for
    /// the live instance's scratch beside the checkpoint.
    #[test]
    fn a_restore_that_loses_the_claim_never_names_the_paired_scratch() {
        let root = tempfile::tempdir().unwrap();
        let snapshot_dir = publish_direct_claimed_checkpoint(root.path(), &[0x5a; 4096]);
        let loser = OpenedSnapshot::open(&snapshot_dir).unwrap();
        OpenedSnapshot::open(&snapshot_dir)
            .unwrap()
            .claim_for_restore()
            .unwrap();

        // The restore's inputs stay alive, as they would until it fails.
        let _restore =
            prepare_restore(&mut checkpoint_restore_options(&snapshot_dir), Some(&loser)).unwrap();
        assert_eq!(
            loser.claim_for_restore().unwrap_err().to_string(),
            "resume snapshot has already been claimed"
        );
        assert_eq!(directory_names(root.path()), ["snapshot"]);
        assert_eq!(
            directory_names(&snapshot_dir),
            [
                "manifest.bin",
                "memory.bin",
                "resume.claim",
                "scratch.img",
                "state.bin"
            ]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let scratch = snapshot_dir.join(openvmm_helpers::snapshot::format::SCRATCH_FILE_NAME);
            assert_eq!(std::fs::metadata(scratch).unwrap().nlink(), 1);
        }
    }

    /// A direct-claimed restore attaches the checkpoint's own scratch in place,
    /// which only the VM writes, once the claim that precedes it is durable.
    #[test]
    fn a_direct_claimed_restore_attaches_the_paired_scratch_in_place() {
        use std::io::Write;

        let root = tempfile::tempdir().unwrap();
        let snapshot_dir = publish_direct_claimed_checkpoint(root.path(), &[0x5a; 4096]);
        let snapshot = OpenedSnapshot::open(&snapshot_dir).unwrap();
        let mut options = checkpoint_restore_options(&snapshot_dir);

        let restore = prepare_restore(&mut options, Some(&snapshot)).unwrap();
        // The restore adds no scratch block of its own and copies nothing.
        assert!(options.microvm.microvm_sandbox_block.is_empty());
        assert!(restore.private_scratch_dir.is_none());
        assert_eq!(directory_names(root.path()), ["snapshot"]);
        let (mut scratch, path) = restore.claimed_scratch.unwrap();
        assert_eq!(
            path,
            snapshot_dir.join(openvmm_helpers::snapshot::format::SCRATCH_FILE_NAME)
        );

        snapshot.claim_for_restore().unwrap();
        scratch.write_all(b"claimed").unwrap();
        scratch.sync_all().unwrap();
        drop(scratch);
        assert_eq!(&std::fs::read(&path).unwrap()[..7], b"claimed");
    }
}
