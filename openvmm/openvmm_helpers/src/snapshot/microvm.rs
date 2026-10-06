// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM snapshot machine-contract types, construction, and validation.

use super::SnapshotManifest;
use super::format::SCRATCH_FILE_NAME;
#[cfg(test)]
use super::format::SHA256_SIZE;
use super::format::SNAPSHOT_CONFIG_ALL;
use super::format::SNAPSHOT_CONFIG_INVARIANTS;
use super::format::SNAPSHOT_RESTORE_POLICY_CLONE;
use super::format::SNAPSHOT_RESTORE_POLICY_RESUME;
use super::format::SNAPSHOT_TIER_INSTANCE_CHECKPOINT;
use super::format::SNAPSHOT_TIER_PLATFORM;
use super::format::SNAPSHOT_TIER_WORKLOAD_START;
use super::format::validate_sha256;
use super::format::verify_digest;
use anyhow::Context;
use mesh::payload::Protobuf;
use sha2::Digest;
use std::collections::HashSet;
use std::path::Path;

/// Capability version for the always-present dormant microVM virtio-fs slot.
pub const MICROVM_FILESYSTEM_SLOT_VERSION: u32 = 1;
/// Linux-direct MP-table boot layout with shared interrupt status.
pub const MICROVM_BOOT_LAYOUT_VERSION: u32 = 2;
/// Contract version for one-shot restore-time microVM memory expansion.
pub const MICROVM_MEMORY_EXPANSION_VERSION: u32 = 1;
/// Linux memory-block granularity used by the x86-64 microVM guest.
pub const MICROVM_MEMORY_BLOCK_SIZE_BYTES: u64 =
    openvmm_defs::microvm::MICROVM_MEMORY_BLOCK_SIZE_BYTES;
/// Snapshot contract name for shared-status edge interrupts.
pub const MICROVM_SHARED_STATUS_INTERRUPT_MODE: &str = "edge-shared-status";
const MAX_MEMORY_RANGES: usize = 128;
const MAX_DEVICES: usize = 64;
const MAX_DEVICE_RANGES: usize = 16;
const MAX_STATE_UNITS: usize = 512;
const MAX_ATTACHMENTS: usize = 64;
const MAX_ATTACHMENT_IDENTITY_BYTES: usize = 4096;
const MAX_COMMAND_LINE_BYTES: usize = 64 * 1024;

/// A guest RAM range and its corresponding offset in `memory.bin`.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotMemoryRange {
    /// Guest physical base address.
    #[mesh(1)]
    pub gpa_start: u64,
    /// Range length in bytes.
    #[mesh(2)]
    pub length: u64,
    /// Byte offset in `memory.bin`.
    #[mesh(3)]
    pub file_offset: u64,
}

/// A restore-attachable guest RAM range that is absent from `memory.bin`.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotMemoryExpansionRange {
    /// Guest physical base address.
    #[mesh(1)]
    pub gpa_start: u64,
    /// Range length in bytes.
    #[mesh(2)]
    pub length: u64,
}

/// Processor topology that must be reproduced during restore.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotProcessorTopology {
    /// Socket count.
    #[mesh(1)]
    pub sockets: u32,
    /// Dies per socket.
    #[mesh(2)]
    pub dies_per_socket: u32,
    /// Cores per die.
    #[mesh(3)]
    pub cores_per_die: u32,
    /// Threads per core.
    #[mesh(4)]
    pub threads_per_core: u32,
    /// APIC IDs in virtual-processor order.
    #[mesh(5)]
    pub apic_ids: Vec<u32>,
}

/// An address range owned by a saved device.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotDeviceRange {
    /// Address space (`pmio` or `mmio`).
    #[mesh(1)]
    pub address_space: String,
    /// Range base address.
    #[mesh(2)]
    pub start: u64,
    /// Range length in bytes.
    #[mesh(3)]
    pub length: u64,
}

/// Guest-visible device identity and placement.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotDevice {
    /// Stable device identity.
    #[mesh(1)]
    pub stable_id: String,
    /// State-unit identity, including units with no mutable state.
    #[mesh(2)]
    pub state_unit_name: String,
    /// Device kind.
    #[mesh(3)]
    pub kind: String,
    /// Stable device order.
    #[mesh(4)]
    pub order: u32,
    /// PMIO or MMIO ranges.
    #[mesh(5)]
    pub ranges: Vec<SnapshotDeviceRange>,
    /// Interrupt number, or `None` for devices without an interrupt.
    #[mesh(6)]
    pub irq: Option<u32>,
    /// Transport kind, empty for non-transport devices.
    #[mesh(7)]
    pub transport: String,
    /// Effective feature banks in bank order.
    #[mesh(8)]
    pub feature_banks: Vec<u32>,
    /// Queue count.
    #[mesh(9)]
    pub queue_count: u32,
    /// Maximum queue sizes in queue order.
    #[mesh(10)]
    pub queue_max_sizes: Vec<u32>,
}

/// Stable identity for a host resource required by a device.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotAttachment {
    /// Stable attachment ID.
    #[mesh(1)]
    pub stable_id: String,
    /// Attachment kind.
    #[mesh(2)]
    pub kind: String,
    /// Whether restore requires the attachment.
    #[mesh(3)]
    pub required: bool,
    /// Reconnection policy.
    #[mesh(4)]
    pub reconnect_policy: String,
    /// Immutable identity kind.
    #[mesh(5)]
    pub identity_kind: String,
    /// Provider identity or SHA-256 bytes.
    #[mesh(6)]
    pub identity: Vec<u8>,
    /// Attachment length when applicable.
    #[mesh(7)]
    pub length: u64,
    /// Bounded reconnect timeout. Zero for non-client policies.
    #[mesh(8)]
    pub reconnect_timeout_ms: u64,
}

/// Canonical guest-visible identity of the microVM network.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotMicrovmNetwork {
    /// Required cross-platform host-network implementation contract.
    #[mesh(9)]
    pub profile: String,
    /// Guest IPv4 address in network byte order.
    #[mesh(1)]
    pub guest_ipv4: u32,
    /// IPv4 subnet prefix length.
    #[mesh(2)]
    pub prefix_length: u32,
    /// Derived gateway IPv4 address in network byte order.
    #[mesh(3)]
    pub gateway_ipv4: u32,
    /// Deterministic guest MAC address.
    #[mesh(4)]
    pub guest_mac: Vec<u8>,
    /// Deterministic gateway MAC address.
    #[mesh(5)]
    pub gateway_mac: Vec<u8>,
    /// Canonical run-scoped egress policy mode.
    #[mesh(6)]
    pub egress_policy_mode: String,
    /// SHA-256 of the canonical run-scoped egress policy.
    #[mesh(7)]
    pub egress_policy_sha256: Vec<u8>,
    /// Whether restore must supply the same policy contract.
    #[mesh(8)]
    pub egress_policy_required: bool,
    /// Version of the canonical egress-policy digest encoding.
    #[mesh(10)]
    pub egress_policy_encoding_version: u32,
}

/// Canonical guest-visible policy of the microVM filesystem.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotMicrovmFilesystem {
    /// Absolute guest mount target.
    #[mesh(1)]
    pub guest_mount_target: String,
    /// Snapshot-authoritative `ro` or `rw` access mode.
    #[mesh(2)]
    pub access_mode: String,
    /// Host attachment policy.
    #[mesh(3)]
    pub restore_mode: String,
    /// Fixed virtio-fs tag.
    #[mesh(4)]
    pub tag: String,
    /// Number of high-priority queues.
    #[mesh(5)]
    pub high_priority_queue_count: u32,
    /// Number of request queues.
    #[mesh(6)]
    pub request_queue_count: u32,
    /// DAX/shared-memory window size.
    #[mesh(7)]
    pub shared_memory_size: u64,
    /// Whether all opened files use direct I/O.
    #[mesh(8)]
    pub direct_io: bool,
    /// Guest entry-cache lifetime in nanoseconds.
    #[mesh(9)]
    pub entry_cache_timeout_ns: u64,
    /// Guest attribute-cache lifetime in nanoseconds.
    #[mesh(10)]
    pub attribute_cache_timeout_ns: u64,
    /// Canonical absolute host export path.
    #[mesh(11)]
    pub canonical_host_path: String,
    /// Canonical host-relative paths hidden by the virtio-fs server.
    #[mesh(12)]
    pub denied_paths: Vec<String>,
    /// Host identity of guest operations: empty for the VMM, which is also
    /// what snapshots that predate this field used, or `caller`.
    #[mesh(13)]
    pub owner_mode: String,
}

/// Authoritative identity and snapshot policy for a microVM sandbox block.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotMicrovmSandboxBlock {
    /// Stable role (`distro`, `runtime`, `custom`, or `scratch`).
    #[mesh(1)]
    pub role: String,
    /// Whether the guest sees the device as read-only.
    #[mesh(2)]
    pub read_only: bool,
    /// Logical device length in bytes.
    #[mesh(3)]
    pub length: u64,
    /// Kind of immutable identity carried in `identity`.
    #[mesh(4)]
    pub identity_kind: String,
    /// Immutable layer identity or paired-scratch digest.
    #[mesh(5)]
    pub identity: Vec<u8>,
    /// Fixed snapshot-relative artifact name; empty for external layers.
    #[mesh(6)]
    pub artifact: String,
    /// Guest-visible logical block size in bytes.
    #[mesh(7)]
    pub logical_block_size: u32,
    /// Guest-visible physical block size in bytes.
    #[mesh(8)]
    pub physical_block_size: u32,
}

impl SnapshotMicrovmFilesystem {
    fn new(
        config: &openvmm_defs::microvm::MicrovmFilesystemConfig,
        canonical_host_path: &str,
        slot: &openvmm_defs::microvm::MicrovmFilesystemSlot,
    ) -> Self {
        Self {
            guest_mount_target: config.guest_mount_target.clone(),
            access_mode: config.access.as_str().to_owned(),
            restore_mode: "live-revalidate".to_owned(),
            tag: slot.tag.to_owned(),
            high_priority_queue_count: 1,
            request_queue_count: 1,
            shared_memory_size: 0,
            direct_io: true,
            entry_cache_timeout_ns: 0,
            attribute_cache_timeout_ns: 0,
            canonical_host_path: canonical_host_path.to_owned(),
            denied_paths: config.denied_paths.clone(),
            owner_mode: match config.owner {
                openvmm_defs::microvm::MicrovmFilesystemOwner::Vmm => String::new(),
                openvmm_defs::microvm::MicrovmFilesystemOwner::Caller => {
                    openvmm_defs::microvm::MicrovmFilesystemOwner::Caller
                        .as_str()
                        .to_owned()
                }
            },
        }
    }
}

/// Parses the host identity of guest operations recorded in a snapshot
/// filesystem policy.
pub fn snapshot_microvm_filesystem_owner(
    owner_mode: &str,
) -> anyhow::Result<openvmm_defs::microvm::MicrovmFilesystemOwner> {
    match owner_mode {
        "" => Ok(openvmm_defs::microvm::MicrovmFilesystemOwner::Vmm),
        "caller" => Ok(openvmm_defs::microvm::MicrovmFilesystemOwner::Caller),
        mode => anyhow::bail!("snapshot filesystem owner mode '{mode}' is unsupported"),
    }
}

impl SnapshotMicrovmNetwork {
    fn new(
        config: &openvmm_defs::microvm::MicrovmNetworkConfig,
        egress_policy: &net_backend_resources::egress::EgressPolicy,
    ) -> Self {
        Self {
            profile: config.profile.as_str().to_owned(),
            guest_ipv4: u32::from(config.guest_ipv4),
            prefix_length: u32::from(config.prefix_length),
            gateway_ipv4: u32::from(config.derived_gateway_ipv4),
            guest_mac: config.guest_mac.to_bytes().to_vec(),
            gateway_mac: config.gateway_mac.to_bytes().to_vec(),
            egress_policy_mode: egress_policy.mode_name().to_owned(),
            egress_policy_sha256: sha2::Sha256::digest(egress_policy.canonical_bytes()).to_vec(),
            egress_policy_required: egress_policy.is_active(),
            egress_policy_encoding_version:
                net_backend_resources::egress::EGRESS_POLICY_ENCODING_VERSION,
        }
    }
}

/// Validates a restore-time policy against the snapshot's canonical contract.
pub fn validate_microvm_network_policy(
    saved: &SnapshotMicrovmNetwork,
    policy: &net_backend_resources::egress::EgressPolicy,
) -> anyhow::Result<()> {
    let encoding_version = match saved.egress_policy_encoding_version {
        0 => 1,
        version => version,
    };
    let canonical = policy
        .canonical_bytes_for_version(encoding_version)
        .with_context(|| {
            format!("snapshot egress policy encoding version {encoding_version} is unsupported")
        })?;
    let digest = sha2::Sha256::digest(canonical);
    anyhow::ensure!(
        saved.egress_policy_mode == policy.mode_name()
            && saved.egress_policy_sha256 == digest.as_slice()
            && saved.egress_policy_required == policy.is_active(),
        "restore-time egress policy does not match the snapshot contract"
    );
    Ok(())
}

/// Machine composition that becomes authoritative after capture.
#[derive(Clone, Debug, PartialEq, Eq, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotMachineContract {
    /// Machine profile name.
    #[mesh(1)]
    pub machine_profile: String,
    /// microVM ABI version.
    #[mesh(2)]
    pub microvm_abi_version: u32,
    /// Source hypervisor kind.
    #[mesh(3)]
    pub source_hypervisor: String,
    /// Effective kernel command line.
    #[mesh(4)]
    pub effective_command_line: String,
    /// SHA-256 of the effective command line.
    #[mesh(5)]
    pub effective_command_line_sha256: Vec<u8>,
    /// Guest RAM ranges in stable order.
    #[mesh(6)]
    pub memory_ranges: Vec<SnapshotMemoryRange>,
    /// Processor topology.
    #[mesh(7)]
    pub topology: SnapshotProcessorTopology,
    /// Guest-visible devices in stable order.
    #[mesh(8)]
    pub devices: Vec<SnapshotDevice>,
    /// Complete state-unit inventory in stable order.
    #[mesh(9)]
    pub state_unit_names: Vec<String>,
    /// Required host attachments.
    #[mesh(10)]
    pub attachments: Vec<SnapshotAttachment>,
    // Fields 11 to 15, 17, and 20 held the clock fields of manifest versions
    // 2 to 5. They are retired, and their numbers are never reused.
    /// Version of the fixed cold-boot and memory layout.
    #[mesh(16)]
    pub boot_layout_version: u32,
    /// Static identity of the optional microVM virtio-net device.
    #[mesh(18)]
    pub microvm_network: Option<SnapshotMicrovmNetwork>,
    /// Guest-visible policy of the optional microVM virtio-fs device in the
    /// first slot.
    #[mesh(19)]
    pub microvm_filesystem: Option<SnapshotMicrovmFilesystem>,
    /// Fixed-role sandbox blocks in guest-visible order.
    #[mesh(21)]
    pub microvm_sandbox_blocks: Vec<SnapshotMicrovmSandboxBlock>,
    /// Version of the reserved restore-attachable microVM virtio-fs slot.
    #[mesh(22)]
    pub microvm_filesystem_slot_version: u32,
    /// Virtual processors online at boot, or zero when restore activation is disabled.
    #[mesh(23)]
    pub boot_online_vp_count: u32,
    /// Virtio interrupt-delivery mode.
    #[mesh(24)]
    pub virtio_interrupt_mode: String,
    /// Guest-physical base of the shared interrupt-status page, or zero when absent.
    #[mesh(25)]
    pub virtio_shared_status_page_gpa: u64,
    /// Size of the shared interrupt-status page, or zero when absent.
    #[mesh(26)]
    pub virtio_shared_status_page_size: u64,
    /// Restore-time memory expansion capability version, or zero when absent.
    #[mesh(27)]
    pub memory_expansion_version: u32,
    /// Immutable maximum guest RAM size for restore-time expansion.
    #[mesh(28)]
    pub memory_capacity_bytes: u64,
    /// Required alignment of every restore-time memory target and range.
    #[mesh(29)]
    pub memory_block_size_bytes: u64,
    /// Canonical capacity ranges absent from the captured boot memory map.
    #[mesh(30)]
    pub memory_expansion_ranges: Vec<SnapshotMemoryExpansionRange>,
    /// The NVX time ABI time contract; required.
    #[mesh(31)]
    pub time: Option<openvmm_defs::time_abi::SnapshotTimeContract>,
    /// The NVX time ABI CPU profile record; required.
    #[mesh(32)]
    pub cpu_profile: Option<openvmm_defs::time_abi::SnapshotCpuProfile>,
    /// Guest-visible policies of the virtio-fs slots after the first, in slot
    /// order. Each of these slots exists only with its filesystem attached,
    /// which requires a filesystem in the first slot.
    #[mesh(33)]
    pub microvm_additional_filesystems: Vec<SnapshotMicrovmFilesystem>,
}

impl SnapshotMachineContract {
    /// Sets the effective command line and its digest together.
    pub fn set_effective_command_line(&mut self, command_line: String) {
        self.effective_command_line_sha256 = sha2::Sha256::digest(command_line.as_bytes()).to_vec();
        self.effective_command_line = command_line;
    }

    /// Returns the policies of the attached microVM filesystems, in
    /// virtio-fs slot order.
    pub fn microvm_filesystems(&self) -> impl Iterator<Item = &SnapshotMicrovmFilesystem> {
        self.microvm_filesystem
            .iter()
            .chain(&self.microvm_additional_filesystems)
    }
}

fn microvm_snapshot_topology(processor_count: u32) -> anyhow::Result<SnapshotProcessorTopology> {
    anyhow::ensure!(
        openvmm_defs::microvm::microvm_processor_count_supported(processor_count),
        "microVM does not support {processor_count} vCPUs"
    );
    Ok(SnapshotProcessorTopology {
        sockets: 1,
        dies_per_socket: 1,
        cores_per_die: processor_count,
        threads_per_core: 1,
        apic_ids: (0..processor_count).collect(),
    })
}

fn microvm_boot_online_vp_count(
    vp_capacity: u32,
    effective_command_line: &str,
) -> anyhow::Result<u32> {
    let mut boot_online_vp_count = None;
    for token in effective_command_line.split_ascii_whitespace() {
        let Some(value) = token.strip_prefix("maxcpus=") else {
            continue;
        };
        anyhow::ensure!(
            boot_online_vp_count.is_none(),
            "microVM command line contains multiple maxcpus values"
        );
        let count = value
            .parse::<u32>()
            .context("microVM maxcpus value is invalid")?;
        anyhow::ensure!(
            openvmm_defs::microvm::microvm_processor_count_supported(count),
            "microVM does not support a boot-online count of {count}"
        );
        anyhow::ensure!(
            count <= vp_capacity,
            "microVM boot-online count {count} exceeds VP capacity {vp_capacity}"
        );
        boot_online_vp_count = Some(count);
    }
    Ok(boot_online_vp_count.unwrap_or(0))
}

/// Validates a restore-time online VP target against an opt-in snapshot contract.
pub fn validate_restore_online_vp_count(
    manifest: &SnapshotManifest,
    restore_online_vp_count: u32,
) -> anyhow::Result<()> {
    let contract = manifest
        .machine_contract
        .as_ref()
        .context("snapshot is missing the authoritative machine contract")?;
    validate_supported_microvm_contract(contract)?;
    anyhow::ensure!(
        contract.boot_online_vp_count != 0,
        "snapshot does not declare restore-time VP activation support"
    );
    anyhow::ensure!(
        openvmm_defs::microvm::microvm_processor_count_supported(restore_online_vp_count),
        "restore-online VP count {restore_online_vp_count} is not supported"
    );
    anyhow::ensure!(
        restore_online_vp_count >= contract.boot_online_vp_count,
        "restore-online VP count {restore_online_vp_count} is below boot-online count {}",
        contract.boot_online_vp_count
    );
    anyhow::ensure!(
        restore_online_vp_count <= manifest.vp_count,
        "restore-online VP count {restore_online_vp_count} exceeds VP capacity {}",
        manifest.vp_count
    );
    Ok(())
}

/// Validates and selects the expansion ranges for a restore-time RAM target.
pub fn validate_restore_memory_target(
    manifest: &SnapshotManifest,
    restore_memory_size: u64,
) -> anyhow::Result<Vec<SnapshotMemoryExpansionRange>> {
    let contract = manifest
        .machine_contract
        .as_ref()
        .context("snapshot is missing the authoritative machine contract")?;
    validate_machine_contract_shape(contract, manifest.memory_size_bytes, manifest.vp_count)?;
    anyhow::ensure!(
        contract.memory_expansion_version == MICROVM_MEMORY_EXPANSION_VERSION,
        "snapshot does not declare restore-time memory expansion support"
    );
    anyhow::ensure!(
        restore_memory_size >= manifest.memory_size_bytes,
        "restore memory target {restore_memory_size} is below snapshot RAM {}",
        manifest.memory_size_bytes
    );
    anyhow::ensure!(
        restore_memory_size <= contract.memory_capacity_bytes,
        "restore memory target {restore_memory_size} exceeds RAM capacity {}",
        contract.memory_capacity_bytes
    );
    anyhow::ensure!(
        restore_memory_size.is_multiple_of(contract.memory_block_size_bytes),
        "restore memory target {restore_memory_size} is not aligned to the {}-byte memory block size",
        contract.memory_block_size_bytes
    );
    memory_expansion_prefix(
        &contract.memory_expansion_ranges,
        restore_memory_size - manifest.memory_size_bytes,
    )
}

fn canonical_microvm_memory_ranges(memory_size: u64) -> anyhow::Result<Vec<SnapshotMemoryRange>> {
    const LOW_RAM_END: u64 = 3 * 1024 * 1024 * 1024;
    const HIGH_RAM_START: u64 = 4 * 1024 * 1024 * 1024;
    anyhow::ensure!(memory_size != 0, "microVM RAM size must be nonzero");
    let low_length = memory_size.min(LOW_RAM_END);
    let mut ranges = vec![SnapshotMemoryRange {
        gpa_start: 0,
        length: low_length,
        file_offset: 0,
    }];
    if memory_size > LOW_RAM_END {
        let high_length = memory_size - LOW_RAM_END;
        HIGH_RAM_START
            .checked_add(high_length)
            .context("microVM RAM layout overflows GPA space")?;
        ranges.push(SnapshotMemoryRange {
            gpa_start: HIGH_RAM_START,
            length: high_length,
            file_offset: LOW_RAM_END,
        });
    }
    Ok(ranges)
}

fn canonical_memory_expansion_ranges(
    memory_size: u64,
    memory_capacity: u64,
) -> anyhow::Result<Vec<SnapshotMemoryExpansionRange>> {
    anyhow::ensure!(
        memory_capacity >= memory_size,
        "RAM capacity {memory_capacity} is below snapshot RAM {memory_size}"
    );
    let mut ranges = Vec::new();
    for capacity_range in canonical_microvm_memory_ranges(memory_capacity)? {
        let logical_start = capacity_range.file_offset.max(memory_size);
        let logical_end = capacity_range
            .file_offset
            .checked_add(capacity_range.length)
            .context("microVM capacity range overflows")?;
        if logical_start >= logical_end {
            continue;
        }
        ranges.push(SnapshotMemoryExpansionRange {
            gpa_start: capacity_range.gpa_start + logical_start - capacity_range.file_offset,
            length: logical_end - logical_start,
        });
    }
    Ok(ranges)
}

fn memory_expansion_prefix(
    ranges: &[SnapshotMemoryExpansionRange],
    size: u64,
) -> anyhow::Result<Vec<SnapshotMemoryExpansionRange>> {
    let mut remaining = size;
    let mut prefix = Vec::new();
    for range in ranges {
        if remaining == 0 {
            break;
        }
        let length = range.length.min(remaining);
        prefix.push(SnapshotMemoryExpansionRange {
            gpa_start: range.gpa_start,
            length,
        });
        remaining -= length;
    }
    anyhow::ensure!(
        remaining == 0,
        "memory expansion ranges do not cover target"
    );
    Ok(prefix)
}

/// Returns the snapshot device of a fixed virtio-fs slot.
fn microvm_filesystem_device(
    slot: &openvmm_defs::microvm::MicrovmFilesystemSlot,
    order: usize,
) -> SnapshotDevice {
    SnapshotDevice {
        stable_id: slot.stable_id.to_owned(),
        state_unit_name: format!("virtiofs-{}", slot.mmio_base),
        kind: "virtio-fs".to_owned(),
        order: order as u32,
        ranges: vec![SnapshotDeviceRange {
            address_space: "mmio".to_owned(),
            start: slot.mmio_base,
            length: openvmm_defs::microvm::MICROVM_VIRTIO_MMIO_LEN,
        }],
        irq: Some(slot.irq),
        transport: "virtio-mmio".to_owned(),
        feature_banks: vec![
            openvmm_defs::microvm::MICROVM_VIRTIO_FS_FEATURES as u32,
            (openvmm_defs::microvm::MICROVM_VIRTIO_FS_FEATURES >> 32) as u32,
        ],
        queue_count: 2,
        queue_max_sizes: vec![256, 256],
    }
}

/// Validates the live attachment of the filesystem in `slot` and returns its
/// snapshot policy.
fn microvm_filesystem_policy(
    source_hypervisor: &str,
    slot: &openvmm_defs::microvm::MicrovmFilesystemSlot,
    filesystem: &openvmm_defs::microvm::MicrovmFilesystemConfig,
    canonical_host_path: &Path,
    attachment: &SnapshotAttachment,
) -> anyhow::Result<SnapshotMicrovmFilesystem> {
    let canonical_host_path = canonical_host_path
        .to_str()
        .context("microVM filesystem canonical host path is not valid UTF-8")?;
    anyhow::ensure!(
        !canonical_host_path.is_empty(),
        "microVM filesystem canonical host path is empty"
    );
    anyhow::ensure!(
        attachment.stable_id == slot.stable_id
            && attachment.kind == "virtio-fs"
            && attachment.required
            && attachment.reconnect_policy == "live-revalidate"
            && match source_hypervisor {
                "kvm" | "mshv" => attachment.identity_kind == "unix-device-inode-v1",
                "whp" => attachment.identity_kind == "windows-volume-file-id-v1",
                _ => false,
            }
            && !attachment.identity.is_empty()
            && attachment.identity.len() <= MAX_ATTACHMENT_IDENTITY_BYTES
            && attachment.length == 0
            && attachment.reconnect_timeout_ms == 0,
        "microVM filesystem attachment has an unsupported live-revalidation policy"
    );
    Ok(SnapshotMicrovmFilesystem::new(
        filesystem,
        canonical_host_path,
        slot,
    ))
}

/// Builds the authoritative microVM machine contract, with the time ABI
/// records `time` and `cpu_profile`.
///
/// `filesystem_slot` reserves the first virtio-fs slot, and `filesystems`
/// occupy the virtio-fs slots in order.
pub fn microvm_machine_contract(
    source_hypervisor: &str,
    boot_layout_version: u32,
    effective_command_line: String,
    network: Option<(
        &openvmm_defs::microvm::MicrovmNetworkConfig,
        &net_backend_resources::egress::EgressPolicy,
        SnapshotAttachment,
    )>,
    filesystem_slot: bool,
    filesystems: Vec<(
        &openvmm_defs::microvm::MicrovmFilesystemConfig,
        &Path,
        SnapshotAttachment,
    )>,
    console_attachment: Option<SnapshotAttachment>,
    control_console_attachment: Option<SnapshotAttachment>,
    sandbox_blocks: Vec<SnapshotMicrovmSandboxBlock>,
    processor_count: u32,
    memory_size: u64,
    memory_capacity: Option<u64>,
    state_unit_names: Vec<String>,
    time: openvmm_defs::time_abi::SnapshotTimeContract,
    cpu_profile: openvmm_defs::time_abi::SnapshotCpuProfile,
) -> anyhow::Result<SnapshotMachineContract> {
    anyhow::ensure!(
        matches!(source_hypervisor, "kvm" | "mshv" | "whp"),
        "microVM snapshots require the KVM, MSHV, or WHP hypervisor"
    );
    let topology = microvm_snapshot_topology(processor_count)?;
    let boot_online_vp_count =
        microvm_boot_online_vp_count(processor_count, &effective_command_line)?;

    let memory_ranges = canonical_microvm_memory_ranges(memory_size)?;
    let (
        memory_expansion_version,
        memory_capacity_bytes,
        memory_block_size_bytes,
        memory_expansion_ranges,
    ) = if let Some(memory_capacity) = memory_capacity {
        anyhow::ensure!(
            memory_size.is_multiple_of(MICROVM_MEMORY_BLOCK_SIZE_BYTES),
            "snapshot RAM {memory_size} is not aligned to the {MICROVM_MEMORY_BLOCK_SIZE_BYTES}-byte memory block size"
        );
        anyhow::ensure!(
            memory_capacity.is_multiple_of(MICROVM_MEMORY_BLOCK_SIZE_BYTES),
            "RAM capacity {memory_capacity} is not aligned to the {MICROVM_MEMORY_BLOCK_SIZE_BYTES}-byte memory block size"
        );
        (
            MICROVM_MEMORY_EXPANSION_VERSION,
            memory_capacity,
            MICROVM_MEMORY_BLOCK_SIZE_BYTES,
            canonical_memory_expansion_ranges(memory_size, memory_capacity)?,
        )
    } else {
        (0, 0, 0, Vec::new())
    };

    let device = |stable_id: &str,
                  state_unit_name: &str,
                  kind: &str,
                  ranges: Vec<SnapshotDeviceRange>,
                  irq: Option<u32>,
                  order: u32| SnapshotDevice {
        stable_id: stable_id.to_owned(),
        state_unit_name: state_unit_name.to_owned(),
        kind: kind.to_owned(),
        order,
        ranges,
        irq,
        transport: String::new(),
        feature_banks: Vec::new(),
        queue_count: 0,
        queue_max_sizes: Vec::new(),
    };
    let pmio = |start, length| SnapshotDeviceRange {
        address_space: "pmio".to_owned(),
        start,
        length,
    };
    let mmio = |start, length| SnapshotDeviceRange {
        address_space: "mmio".to_owned(),
        start,
        length,
    };

    let mut devices = vec![
        device("partition", "partition", "partition", Vec::new(), None, 0),
        device("vp0", "partition", "vcpu", Vec::new(), None, 1),
        device("vmtime", "vmtime", "clock", Vec::new(), None, 2),
        device(
            "pic",
            "pic",
            "pic",
            vec![pmio(0x20, 2), pmio(0xa0, 2)],
            None,
            3,
        ),
        device(
            "ioapic",
            "ioapic",
            "ioapic",
            vec![mmio(0xfec0_0000, 0x1000)],
            None,
            4,
        ),
        device(
            "lapic",
            "partition",
            "lapic",
            vec![mmio(0xfee0_0000, 0x1000)],
            None,
            5,
        ),
        device("pit", "pit", "pit", vec![pmio(0x40, 4)], Some(0), 6),
        device("rtc", "rtc", "rtc", vec![pmio(0x70, 2)], Some(8), 7),
        device(
            "microvm-portb",
            "microvm-portb",
            "portb",
            vec![pmio(0xe9, 2)],
            None,
            8,
        ),
        device(
            "microvm-shutdown",
            "microvm-shutdown",
            "shutdown",
            vec![pmio(0x604, 1)],
            None,
            9,
        ),
        device(
            "microvm-snapshot-request",
            "microvm-snapshot-request",
            "snapshot-request",
            vec![pmio(0x605, 1)],
            None,
            10,
        ),
    ];
    let mut attachments = Vec::new();
    let microvm_network = if let Some((network, egress_policy, attachment)) = network {
        let policy_is_valid = matches!(source_hypervisor, "kvm" | "mshv" | "whp")
            && attachment.reconnect_policy == "recreate-endpoint"
            && !attachment.required
            && attachment.identity_kind == "user-mode-nat"
            && attachment.identity == b"consomme";
        anyhow::ensure!(
            attachment.stable_id == "net:microvm0"
                && attachment.kind == "virtio-net"
                && policy_is_valid
                && !attachment.identity.is_empty()
                && attachment.identity.len() <= MAX_ATTACHMENT_IDENTITY_BYTES
                && attachment.length == 0
                && attachment.reconnect_timeout_ms == 0,
            "microVM network attachment has an unsupported endpoint policy"
        );
        let irq = openvmm_defs::microvm::microvm_virtio_net_irq(Some(source_hypervisor))?;
        let discovery = format!(
            "virtio_mmio.device={:#x}@{:#x}:{irq}",
            openvmm_defs::microvm::MICROVM_VIRTIO_MMIO_LEN,
            openvmm_defs::microvm::MICROVM_VIRTIO_NET_MMIO_BASE,
        );
        let tokens = effective_command_line
            .split_ascii_whitespace()
            .collect::<HashSet<_>>();
        anyhow::ensure!(
            tokens.contains(discovery.as_str())
                && network
                    .command_line_fragment()
                    .split_ascii_whitespace()
                    .all(|token| tokens.contains(token)),
            "microVM network command line does not match its saved identity"
        );
        devices.push(SnapshotDevice {
            stable_id: "net:microvm0".to_owned(),
            state_unit_name: format!(
                "virtio-net-{}",
                openvmm_defs::microvm::MICROVM_VIRTIO_NET_MMIO_BASE
            ),
            kind: "virtio-net".to_owned(),
            order: devices.len() as u32,
            ranges: vec![mmio(
                openvmm_defs::microvm::MICROVM_VIRTIO_NET_MMIO_BASE,
                openvmm_defs::microvm::MICROVM_VIRTIO_MMIO_LEN,
            )],
            irq: Some(irq),
            transport: "virtio-mmio".to_owned(),
            feature_banks: vec![
                openvmm_defs::microvm::MICROVM_VIRTIO_NET_FEATURES as u32,
                (openvmm_defs::microvm::MICROVM_VIRTIO_NET_FEATURES >> 32) as u32,
            ],
            queue_count: 2,
            queue_max_sizes: vec![256, 256],
        });
        attachments.push(attachment);
        Some(SnapshotMicrovmNetwork::new(network, egress_policy))
    } else {
        None
    };
    let filesystem_slot_count =
        openvmm_defs::microvm::microvm_filesystem_slot_count(filesystem_slot, filesystems.len())?;
    let filesystem_slots =
        &openvmm_defs::microvm::MICROVM_FILESYSTEM_SLOTS[..filesystem_slot_count];
    openvmm_defs::microvm::validate_microvm_filesystems(
        &filesystems
            .iter()
            .map(|(filesystem, _, _)| (*filesystem).clone())
            .collect::<Vec<_>>(),
    )?;
    let tokens = effective_command_line
        .split_ascii_whitespace()
        .collect::<Vec<_>>();
    for (index, slot) in openvmm_defs::microvm::MICROVM_FILESYSTEM_SLOTS
        .iter()
        .enumerate()
    {
        let discovery = slot.discovery_token();
        let expected = usize::from(index < filesystem_slot_count);
        anyhow::ensure!(
            tokens.iter().filter(|token| **token == discovery).count() == expected,
            "microVM virtio-fs slot {} must be discovered {expected} time(s) by the effective command line",
            slot.stable_id
        );
    }
    // The bootstrap triplets of the attached filesystems, exactly and in slot
    // order.
    let bootstrap = tokens
        .iter()
        .copied()
        .filter(|token| {
            ["virtfs_dir=", "virtfs_tag=", "virtfs_mode="]
                .iter()
                .any(|prefix| token.starts_with(prefix))
        })
        .collect::<Vec<_>>();
    let expected_bootstrap = filesystems
        .iter()
        .zip(filesystem_slots)
        .map(|((filesystem, _, _), slot)| filesystem.command_line_fragment(slot))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        bootstrap
            == expected_bootstrap
                .iter()
                .flat_map(|fragment| fragment.split_ascii_whitespace())
                .collect::<Vec<_>>(),
        "microVM filesystem command line does not match its saved policy"
    );
    if let Some(slot) = filesystem_slots.first() {
        devices.push(microvm_filesystem_device(slot, devices.len()));
    }
    let mut filesystem_policies = Vec::with_capacity(filesystems.len());
    let mut filesystem_attachments = Vec::with_capacity(filesystems.len());
    for ((filesystem, canonical_host_path, attachment), slot) in
        filesystems.into_iter().zip(filesystem_slots)
    {
        filesystem_policies.push(microvm_filesystem_policy(
            source_hypervisor,
            slot,
            filesystem,
            canonical_host_path,
            &attachment,
        )?);
        filesystem_attachments.push(attachment);
    }
    let mut filesystem_policies = filesystem_policies.into_iter();
    let mut filesystem_attachments = filesystem_attachments.into_iter();
    let microvm_filesystem = filesystem_policies.next();
    attachments.extend(filesystem_attachments.next());
    if let Some(attachment) = console_attachment {
        let policy_is_valid = match attachment.reconnect_policy.as_str() {
            "recreate-listener" => {
                !attachment.required
                    && attachment.reconnect_timeout_ms == 0
                    && matches!(
                        attachment.identity_kind.as_str(),
                        "unix-socket" | "named-pipe" | "tcp"
                    )
            }
            "reconnect-client" => {
                attachment.required
                    && attachment.reconnect_timeout_ms
                        == openvmm_defs::microvm::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS
                    && matches!(
                        attachment.identity_kind.as_str(),
                        "unix-socket" | "named-pipe" | "tcp"
                    )
            }
            "require-inherited-attachment" => {
                attachment.required
                    && attachment.reconnect_timeout_ms == 0
                    && attachment.identity_kind == "provider"
                    && attachment.identity == b"console"
            }
            "discard-while-disconnected" => {
                !attachment.required
                    && attachment.reconnect_timeout_ms == 0
                    && attachment.identity_kind == "disconnected"
                    && attachment.identity == b"discard"
            }
            _ => false,
        };
        anyhow::ensure!(
            attachment.stable_id == "console:microvm-virtio0"
                && attachment.kind == "virtio-console"
                && policy_is_valid
                && !attachment.identity.is_empty()
                && attachment.identity.len() <= MAX_ATTACHMENT_IDENTITY_BYTES
                && attachment.length == 0,
            "microVM console attachment has an unsupported reconnect policy"
        );
        devices.push(SnapshotDevice {
            stable_id: "console:microvm-virtio0".to_owned(),
            state_unit_name: format!(
                "virtio-console-{}",
                openvmm_defs::microvm::MICROVM_VIRTIO_CONSOLE_MMIO_BASE
            ),
            kind: "virtio-console".to_owned(),
            order: devices.len() as u32,
            ranges: vec![mmio(
                openvmm_defs::microvm::MICROVM_VIRTIO_CONSOLE_MMIO_BASE,
                openvmm_defs::microvm::MICROVM_VIRTIO_MMIO_LEN,
            )],
            irq: Some(openvmm_defs::microvm::MICROVM_VIRTIO_CONSOLE_IRQ),
            transport: "virtio-mmio".to_owned(),
            feature_banks: vec![0x3000_0001, 0x0000_0003],
            queue_count: 2,
            queue_max_sizes: vec![256, 256],
        });
        attachments.push(attachment);
    }

    for block in &sandbox_blocks {
        let role = match block.role.as_str() {
            "distro" => openvmm_defs::microvm::MicrovmSandboxBlockRole::Distro,
            "runtime" => openvmm_defs::microvm::MicrovmSandboxBlockRole::Runtime,
            "custom" => openvmm_defs::microvm::MicrovmSandboxBlockRole::Custom,
            "scratch" => openvmm_defs::microvm::MicrovmSandboxBlockRole::Scratch,
            role => anyhow::bail!("snapshot sandbox block role '{role}' is unsupported"),
        };
        let discovery = format!(
            "virtio_mmio.device={:#x}@{:#x}:{}",
            openvmm_defs::microvm::MICROVM_VIRTIO_MMIO_LEN,
            role.mmio_base(),
            role.irq(),
        );
        anyhow::ensure!(
            effective_command_line
                .split_ascii_whitespace()
                .any(|token| token == discovery),
            "microVM sandbox block '{}' is missing from the effective command line",
            block.role
        );
        let features = openvmm_defs::microvm::microvm_sandbox_block_features(role);
        devices.push(SnapshotDevice {
            stable_id: format!("blk:sandbox:{}", role.as_str()),
            state_unit_name: format!("virtio-blk-{}", role.mmio_base()),
            kind: "virtio-blk".to_owned(),
            order: devices.len() as u32,
            ranges: vec![mmio(
                role.mmio_base(),
                openvmm_defs::microvm::MICROVM_VIRTIO_MMIO_LEN,
            )],
            irq: Some(role.irq()),
            transport: "virtio-mmio".to_owned(),
            feature_banks: vec![features as u32, (features >> 32) as u32],
            queue_count: 1,
            queue_max_sizes: vec![256],
        });
    }
    if let Some(attachment) = control_console_attachment {
        let policy_is_valid = match attachment.reconnect_policy.as_str() {
            "broker-authenticated-listener" => {
                !attachment.required
                    && attachment.reconnect_timeout_ms == 0
                    && matches!(
                        attachment.identity_kind.as_str(),
                        "unix-socket" | "named-pipe"
                    )
            }
            "broker-disconnected" => {
                !attachment.required
                    && attachment.reconnect_timeout_ms == 0
                    && attachment.identity_kind == "disconnected"
                    && attachment.identity == b"discard"
            }
            _ => false,
        };
        anyhow::ensure!(
            attachment.stable_id == "console:microvm-control0"
                && attachment.kind == "virtio-control-console"
                && policy_is_valid
                && !attachment.identity.is_empty()
                && attachment.identity.len() <= MAX_ATTACHMENT_IDENTITY_BYTES
                && attachment.length == 0,
            "microVM control console attachment has an unsupported reconnect policy"
        );
        let discovery = format!(
            "virtio_mmio.device={:#x}@{:#x}:{}",
            openvmm_defs::microvm::MICROVM_VIRTIO_MMIO_LEN,
            openvmm_defs::microvm::MICROVM_VIRTIO_CONTROL_CONSOLE_MMIO_BASE,
            openvmm_defs::microvm::MICROVM_VIRTIO_CONTROL_CONSOLE_IRQ,
        );
        let tokens = effective_command_line
            .split_ascii_whitespace()
            .collect::<HashSet<_>>();
        anyhow::ensure!(
            tokens.contains(discovery.as_str())
                && tokens.contains(openvmm_defs::microvm::MICROVM_CONTROL_TTY_COMMAND_LINE),
            "microVM control console command line does not match its saved identity"
        );
        devices.push(SnapshotDevice {
            stable_id: "console:microvm-control0".to_owned(),
            state_unit_name: format!(
                "virtio-control-console-{}",
                openvmm_defs::microvm::MICROVM_VIRTIO_CONTROL_CONSOLE_MMIO_BASE
            ),
            kind: "virtio-control-console".to_owned(),
            order: devices.len() as u32,
            ranges: vec![mmio(
                openvmm_defs::microvm::MICROVM_VIRTIO_CONTROL_CONSOLE_MMIO_BASE,
                openvmm_defs::microvm::MICROVM_VIRTIO_MMIO_LEN,
            )],
            irq: Some(openvmm_defs::microvm::MICROVM_VIRTIO_CONTROL_CONSOLE_IRQ),
            transport: "virtio-mmio".to_owned(),
            feature_banks: vec![0x3000_0001, 0x0000_0003],
            queue_count: 2,
            queue_max_sizes: vec![256, 256],
        });
        attachments.push(attachment);
    }
    for slot in filesystem_slots.iter().skip(1) {
        devices.push(microvm_filesystem_device(slot, devices.len()));
    }
    attachments.extend(filesystem_attachments);

    let mut contract = SnapshotMachineContract {
        machine_profile: "microvm".to_owned(),
        microvm_abi_version: openvmm_defs::microvm::MICROVM_ABI_VERSION_2,
        source_hypervisor: source_hypervisor.to_owned(),
        effective_command_line: String::new(),
        effective_command_line_sha256: Vec::new(),
        memory_ranges,
        topology,
        devices,
        state_unit_names,
        attachments,
        boot_layout_version,
        microvm_network,
        microvm_filesystem,
        microvm_additional_filesystems: filesystem_policies.collect(),
        microvm_sandbox_blocks: sandbox_blocks,
        microvm_filesystem_slot_version: if filesystem_slot {
            MICROVM_FILESYSTEM_SLOT_VERSION
        } else {
            0
        },
        boot_online_vp_count,
        virtio_interrupt_mode: MICROVM_SHARED_STATUS_INTERRUPT_MODE.to_owned(),
        virtio_shared_status_page_gpa: openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_GPA,
        virtio_shared_status_page_size: openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_SIZE,
        memory_expansion_version,
        memory_capacity_bytes,
        memory_block_size_bytes,
        memory_expansion_ranges,
        time: Some(time),
        cpu_profile: Some(cpu_profile),
    };
    contract.set_effective_command_line(effective_command_line);
    validate_machine_contract_shape(&contract, memory_size, processor_count)?;
    Ok(contract)
}

/// Returns whether restore must hold external device input until guest repair completes.
pub fn requires_post_restore_gate(manifest: &SnapshotManifest) -> bool {
    manifest.machine_contract.as_ref().is_some_and(|contract| {
        matches!(
            contract.microvm_abi_version,
            openvmm_defs::microvm::MICROVM_ABI_VERSION_2
        )
    }) && !manifest.snapshot_tier.is_empty()
}

pub(super) fn paired_scratch_block(
    manifest: &SnapshotManifest,
) -> Option<&SnapshotMicrovmSandboxBlock> {
    manifest
        .machine_contract
        .as_ref()?
        .microvm_sandbox_blocks
        .iter()
        .find(|block| block.artifact == SCRATCH_FILE_NAME)
}

/// Rejects a snapshot contract that does not use the supported persisted microVM identities.
pub fn validate_supported_microvm_contract(
    contract: &SnapshotMachineContract,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        contract.microvm_abi_version == openvmm_defs::microvm::MICROVM_ABI_VERSION_2,
        "snapshot microVM ABI version {} is unsupported; this OpenVMM supports version {}",
        contract.microvm_abi_version,
        openvmm_defs::microvm::MICROVM_ABI_VERSION_2,
    );
    anyhow::ensure!(
        contract.boot_layout_version == MICROVM_BOOT_LAYOUT_VERSION,
        "snapshot boot layout version {} is unsupported; this OpenVMM supports version {}",
        contract.boot_layout_version,
        MICROVM_BOOT_LAYOUT_VERSION,
    );
    Ok(())
}

/// Validate and exactly compare an authoritative microVM machine contract.
pub fn validate_microvm_machine_contract(
    manifest: &SnapshotManifest,
    expected: &SnapshotMachineContract,
) -> anyhow::Result<()> {
    let contract = manifest
        .machine_contract
        .as_ref()
        .context("snapshot is missing the authoritative machine contract")?;
    validate_machine_contract_shape(contract, manifest.memory_size_bytes, manifest.vp_count)?;

    anyhow::ensure!(
        contract.machine_profile == "microvm",
        "snapshot machine profile '{}' is not microvm",
        contract.machine_profile,
    );
    anyhow::ensure!(
        contract.machine_profile == expected.machine_profile,
        "snapshot machine profile doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.microvm_abi_version == expected.microvm_abi_version,
        "snapshot microVM ABI version {} doesn't match expected {}",
        contract.microvm_abi_version,
        expected.microvm_abi_version,
    );
    anyhow::ensure!(
        contract.microvm_filesystem_slot_version == expected.microvm_filesystem_slot_version,
        "snapshot microVM filesystem slot capability doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.source_hypervisor == expected.source_hypervisor,
        "snapshot source hypervisor '{}' doesn't match expected '{}'",
        contract.source_hypervisor,
        expected.source_hypervisor,
    );
    anyhow::ensure!(
        contract.boot_layout_version == expected.boot_layout_version,
        "snapshot boot layout version doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.virtio_interrupt_mode == expected.virtio_interrupt_mode
            && contract.virtio_shared_status_page_gpa == expected.virtio_shared_status_page_gpa
            && contract.virtio_shared_status_page_size == expected.virtio_shared_status_page_size,
        "snapshot virtio interrupt contract doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.effective_command_line == expected.effective_command_line,
        "snapshot effective command line doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.memory_ranges == expected.memory_ranges,
        "snapshot RAM layout doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.memory_expansion_version == expected.memory_expansion_version
            && contract.memory_capacity_bytes == expected.memory_capacity_bytes
            && contract.memory_block_size_bytes == expected.memory_block_size_bytes
            && contract.memory_expansion_ranges == expected.memory_expansion_ranges,
        "snapshot RAM capacity contract doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.topology == expected.topology,
        "snapshot processor topology doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.boot_online_vp_count == 0
            || contract.boot_online_vp_count == expected.boot_online_vp_count,
        "snapshot boot-online VP count doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.devices == expected.devices,
        "snapshot device inventory, order, or configuration doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.state_unit_names == expected.state_unit_names,
        "snapshot state-unit inventory or order doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.attachments == expected.attachments,
        "snapshot attachment inventory doesn't match the supplied attachments"
    );
    anyhow::ensure!(
        contract.microvm_network == expected.microvm_network,
        "snapshot static network identity doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.microvm_filesystem == expected.microvm_filesystem
            && contract.microvm_additional_filesystems == expected.microvm_additional_filesystems,
        "snapshot filesystem policy doesn't match the requested machine"
    );
    anyhow::ensure!(
        {
            let mut expected_blocks = expected.microvm_sandbox_blocks.clone();
            if manifest.snapshot_tier == SNAPSHOT_TIER_PLATFORM {
                for block in &mut expected_blocks {
                    if block.read_only {
                        block.identity_kind = "unbound".to_owned();
                        block.identity.clear();
                    }
                }
            }
            contract.microvm_sandbox_blocks == expected_blocks
        },
        "snapshot sandbox block topology or identity doesn't match the requested machine"
    );
    // The time ABI records are not compared: the restore preflight checks
    // them against this host's rates and CPU profile.
    Ok(())
}

pub(super) fn validate_machine_contract_shape(
    contract: &SnapshotMachineContract,
    memory_size: u64,
    vp_count: u32,
) -> anyhow::Result<()> {
    validate_supported_microvm_contract(contract)?;
    anyhow::ensure!(
        contract.virtio_interrupt_mode == MICROVM_SHARED_STATUS_INTERRUPT_MODE
            && contract.virtio_shared_status_page_gpa
                == openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_GPA
            && contract.virtio_shared_status_page_size
                == openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_SIZE,
        "snapshot microVM shared-status interrupt contract is invalid"
    );
    anyhow::ensure!(
        contract.effective_command_line.len() < MAX_COMMAND_LINE_BYTES,
        "snapshot command line exceeds the 64-KiB limit"
    );
    anyhow::ensure!(
        !contract.effective_command_line.contains('\0'),
        "snapshot command line contains an embedded NUL"
    );
    validate_sha256(
        &contract.effective_command_line_sha256,
        "effective command line",
    )?;
    verify_digest(
        contract.effective_command_line.as_bytes(),
        &contract.effective_command_line_sha256,
        "effective command line",
    )?;
    super::time::validate_time_abi_contract(contract)?;

    anyhow::ensure!(
        !contract.memory_ranges.is_empty() && contract.memory_ranges.len() <= MAX_MEMORY_RANGES,
        "snapshot RAM range count is invalid"
    );
    let mut total_memory = 0_u64;
    for (index, range) in contract.memory_ranges.iter().enumerate() {
        anyhow::ensure!(range.length != 0, "snapshot RAM range {index} is empty");
        let gpa_end = range
            .gpa_start
            .checked_add(range.length)
            .with_context(|| format!("snapshot RAM range {index} overflows GPA space"))?;
        let file_end = range
            .file_offset
            .checked_add(range.length)
            .with_context(|| format!("snapshot RAM range {index} overflows file space"))?;
        anyhow::ensure!(
            file_end <= memory_size,
            "snapshot RAM range {index} exceeds memory.bin"
        );
        total_memory = total_memory
            .checked_add(range.length)
            .context("snapshot total RAM size overflows")?;
        for previous in &contract.memory_ranges[..index] {
            let previous_gpa_end = previous.gpa_start + previous.length;
            let previous_file_end = previous.file_offset + previous.length;
            anyhow::ensure!(
                gpa_end <= previous.gpa_start || range.gpa_start >= previous_gpa_end,
                "snapshot RAM ranges overlap in GPA space"
            );
            anyhow::ensure!(
                file_end <= previous.file_offset || range.file_offset >= previous_file_end,
                "snapshot RAM ranges overlap in memory.bin"
            );
        }
    }
    anyhow::ensure!(
        total_memory == memory_size,
        "snapshot RAM ranges cover {total_memory} bytes, expected {memory_size}"
    );
    match contract.memory_expansion_version {
        0 => anyhow::ensure!(
            contract.memory_capacity_bytes == 0
                && contract.memory_block_size_bytes == 0
                && contract.memory_expansion_ranges.is_empty(),
            "legacy snapshot has an unexpected RAM capacity contract"
        ),
        MICROVM_MEMORY_EXPANSION_VERSION => {
            anyhow::ensure!(
                memory_size.is_multiple_of(MICROVM_MEMORY_BLOCK_SIZE_BYTES),
                "snapshot RAM is not memory-block aligned"
            );
            anyhow::ensure!(
                contract.memory_block_size_bytes == MICROVM_MEMORY_BLOCK_SIZE_BYTES,
                "snapshot memory block size {} is unsupported",
                contract.memory_block_size_bytes
            );
            anyhow::ensure!(
                contract.memory_capacity_bytes >= memory_size
                    && contract
                        .memory_capacity_bytes
                        .is_multiple_of(MICROVM_MEMORY_BLOCK_SIZE_BYTES),
                "snapshot RAM capacity is invalid"
            );
            anyhow::ensure!(
                contract.memory_ranges == canonical_microvm_memory_ranges(memory_size)?,
                "snapshot base RAM ranges are not canonical"
            );
            anyhow::ensure!(
                contract.memory_expansion_ranges
                    == canonical_memory_expansion_ranges(
                        memory_size,
                        contract.memory_capacity_bytes,
                    )?,
                "snapshot memory expansion ranges are not canonical"
            );
        }
        version => {
            anyhow::bail!("snapshot memory expansion contract version {version} is unsupported")
        }
    }

    let topology = &contract.topology;
    let topology_vp_count = u64::from(topology.sockets)
        .checked_mul(u64::from(topology.dies_per_socket))
        .and_then(|count| count.checked_mul(u64::from(topology.cores_per_die)))
        .and_then(|count| count.checked_mul(u64::from(topology.threads_per_core)))
        .context("snapshot processor topology overflows")?;
    anyhow::ensure!(
        topology_vp_count == u64::from(vp_count) && topology.apic_ids.len() == vp_count as usize,
        "snapshot processor topology doesn't describe {vp_count} virtual processors"
    );
    ensure_unique(&topology.apic_ids, "APIC ID")?;
    anyhow::ensure!(
        *topology == microvm_snapshot_topology(vp_count)?,
        "snapshot processor topology is not canonical for the microVM"
    );
    if contract.boot_online_vp_count != 0 {
        anyhow::ensure!(
            contract.boot_online_vp_count
                == microvm_boot_online_vp_count(vp_count, &contract.effective_command_line,)?,
            "snapshot boot-online VP count does not match the effective command line"
        );
    }

    if let Some(network) = &contract.microvm_network {
        anyhow::ensure!(
            network.profile == openvmm_defs::microvm::MicrovmNetworkProfile::Portable.as_str(),
            "snapshot microVM network profile '{}' is unsupported",
            network.profile
        );
        let prefix_length = u8::try_from(network.prefix_length)
            .context("snapshot network prefix does not fit in u8")?;
        let parsed = format!(
            "{}/{}",
            std::net::Ipv4Addr::from(network.guest_ipv4),
            prefix_length
        )
        .parse::<openvmm_defs::microvm::MicrovmNetworkConfig>()
        .context("snapshot static network identity is invalid")?;
        anyhow::ensure!(
            network.gateway_ipv4 == u32::from(parsed.derived_gateway_ipv4)
                && network.guest_mac == parsed.guest_mac.to_bytes()
                && network.gateway_mac == parsed.gateway_mac.to_bytes(),
            "snapshot static network identity is not canonical"
        );
        let valid_policy_requirement = match network.egress_policy_mode.as_str() {
            "allow-all" => !network.egress_policy_required,
            "deny-all" | "allow-list" | "block-list" | "endpoint" => network.egress_policy_required,
            "rules" => true,
            _ => false,
        };
        anyhow::ensure!(
            valid_policy_requirement,
            "snapshot egress policy requirement is invalid"
        );
        anyhow::ensure!(
            matches!(
                network.egress_policy_encoding_version,
                0 | 1 | 2 | net_backend_resources::egress::EGRESS_POLICY_ENCODING_VERSION
            ),
            "snapshot egress policy encoding version {} is unsupported",
            network.egress_policy_encoding_version
        );
        validate_sha256(&network.egress_policy_sha256, "egress policy")?;
    }
    let slots = &openvmm_defs::microvm::MICROVM_FILESYSTEM_SLOTS;
    anyhow::ensure!(
        contract.microvm_additional_filesystems.is_empty() || contract.microvm_filesystem.is_some(),
        "snapshot microVM filesystem slots after the first require a filesystem in the first slot"
    );
    anyhow::ensure!(
        contract.microvm_additional_filesystems.len() < slots.len(),
        "snapshot has more microVM filesystems than virtio-fs slots"
    );
    let mut filesystems = Vec::with_capacity(slots.len());
    for (filesystem, slot) in contract.microvm_filesystems().zip(slots) {
        anyhow::ensure!(
            !filesystem.canonical_host_path.is_empty(),
            "snapshot filesystem canonical host path is missing; this snapshot predates path-bound filesystem restore"
        );
        let access = match filesystem.access_mode.as_str() {
            "ro" => openvmm_defs::microvm::MicrovmFilesystemAccess::ReadOnly,
            "rw" => openvmm_defs::microvm::MicrovmFilesystemAccess::ReadWrite,
            mode => anyhow::bail!("snapshot filesystem access mode '{mode}' is unsupported"),
        };
        let parsed = openvmm_defs::microvm::MicrovmFilesystemConfig::new(
            filesystem.guest_mount_target.clone(),
            access,
        )
        .and_then(|config| config.with_denied_paths(filesystem.denied_paths.clone()))
        .context("snapshot filesystem policy is invalid")?
        .with_owner(snapshot_microvm_filesystem_owner(&filesystem.owner_mode)?);
        anyhow::ensure!(
            *filesystem
                == SnapshotMicrovmFilesystem::new(&parsed, &filesystem.canonical_host_path, slot),
            "snapshot filesystem policy is not canonical"
        );
        filesystems.push(parsed);
    }
    openvmm_defs::microvm::validate_microvm_filesystems(&filesystems)
        .context("snapshot filesystem policies are invalid")?;

    let has_filesystem_device = contract
        .devices
        .iter()
        .any(|device| device.stable_id == slots[0].stable_id);
    let has_filesystem_attachment = contract
        .attachments
        .iter()
        .any(|attachment| attachment.stable_id == slots[0].stable_id);
    match contract.microvm_filesystem_slot_version {
        0 => anyhow::ensure!(
            has_filesystem_device == contract.microvm_filesystem.is_some()
                && has_filesystem_attachment == contract.microvm_filesystem.is_some(),
            "legacy snapshot microVM filesystem device, policy, and attachment inventories disagree"
        ),
        MICROVM_FILESYSTEM_SLOT_VERSION => anyhow::ensure!(
            has_filesystem_device
                && has_filesystem_attachment == contract.microvm_filesystem.is_some(),
            "snapshot reserved microVM filesystem slot, policy, and attachment inventories disagree"
        ),
        version => anyhow::bail!(
            "snapshot microVM filesystem slot capability version {version} is unsupported"
        ),
    }
    // A slot after the first exists only with its filesystem attached.
    for (index, slot) in slots.iter().enumerate().skip(1) {
        let attached = contract.microvm_additional_filesystems.len() >= index;
        let has_device = contract
            .devices
            .iter()
            .any(|device| device.stable_id == slot.stable_id);
        let has_attachment = contract
            .attachments
            .iter()
            .any(|attachment| attachment.stable_id == slot.stable_id);
        anyhow::ensure!(
            has_device == attached && has_attachment == attached,
            "snapshot microVM filesystem slot {} device, policy, and attachment inventories disagree",
            slot.stable_id
        );
    }

    if !contract.microvm_sandbox_blocks.is_empty() {
        anyhow::ensure!(
            contract.microvm_sandbox_blocks.len() >= 2
                && contract.microvm_sandbox_blocks.len() <= 4,
            "microVM snapshot must contain one to three layers and scratch"
        );
        let mut previous_role = None;
        for block in &contract.microvm_sandbox_blocks {
            let role = match block.role.as_str() {
                "distro" => openvmm_defs::microvm::MicrovmSandboxBlockRole::Distro,
                "runtime" => openvmm_defs::microvm::MicrovmSandboxBlockRole::Runtime,
                "custom" => openvmm_defs::microvm::MicrovmSandboxBlockRole::Custom,
                "scratch" => openvmm_defs::microvm::MicrovmSandboxBlockRole::Scratch,
                role => anyhow::bail!("snapshot sandbox block role '{role}' is unsupported"),
            };
            anyhow::ensure!(
                previous_role.is_none_or(|previous| previous < role),
                "snapshot sandbox block roles are duplicated or out of order"
            );
            previous_role = Some(role);
            anyhow::ensure!(
                block.read_only == role.is_read_only(),
                "snapshot sandbox block '{}' has an invalid access mode",
                block.role
            );
            anyhow::ensure!(
                block.length != 0 && block.length % 512 == 0,
                "snapshot sandbox block '{}' has invalid geometry",
                block.role
            );
            anyhow::ensure!(
                block.logical_block_size >= 512
                    && block.logical_block_size.is_power_of_two()
                    && block.physical_block_size >= block.logical_block_size
                    && block.physical_block_size.is_power_of_two()
                    && block.length % u64::from(block.logical_block_size) == 0,
                "snapshot sandbox block '{}' has invalid block geometry",
                block.role
            );
            if role == openvmm_defs::microvm::MicrovmSandboxBlockRole::Scratch {
                anyhow::ensure!(
                    block.artifact.is_empty() || block.artifact == SCRATCH_FILE_NAME,
                    "snapshot scratch artifact name is invalid"
                );
                if block.artifact.is_empty() {
                    anyhow::ensure!(
                        block.identity_kind == "fresh" && block.identity.is_empty(),
                        "snapshot fresh scratch policy is invalid"
                    );
                } else {
                    anyhow::ensure!(
                        block.identity_kind == "sha256",
                        "snapshot paired scratch has an unsupported identity kind"
                    );
                    validate_sha256(&block.identity, "scratch block")?;
                }
            } else {
                anyhow::ensure!(
                    block.artifact.is_empty()
                        && matches!(block.identity_kind.as_str(), "sha256" | "unbound"),
                    "snapshot read-only layer '{}' has an invalid identity policy",
                    block.role
                );
                if block.identity_kind == "sha256" {
                    validate_sha256(&block.identity, &format!("{} block", block.role))?;
                } else {
                    anyhow::ensure!(
                        block.identity.is_empty(),
                        "snapshot unbound layer '{}' carries an identity",
                        block.role
                    );
                }
            }
        }
        anyhow::ensure!(
            previous_role == Some(openvmm_defs::microvm::MicrovmSandboxBlockRole::Scratch),
            "microVM snapshot is missing its scratch role"
        );
    }

    anyhow::ensure!(
        contract.devices.len() <= MAX_DEVICES,
        "snapshot device inventory is too large"
    );
    let mut device_ids = HashSet::new();
    for (index, device) in contract.devices.iter().enumerate() {
        anyhow::ensure!(
            device.order == index as u32,
            "snapshot device order is not canonical"
        );
        anyhow::ensure!(
            !device.stable_id.is_empty() && device_ids.insert(device.stable_id.as_str()),
            "snapshot contains an empty or duplicate device ID"
        );
        anyhow::ensure!(
            !device.state_unit_name.is_empty(),
            "snapshot device '{}' has no state-unit name",
            device.stable_id,
        );
        anyhow::ensure!(
            device.ranges.len() <= MAX_DEVICE_RANGES,
            "snapshot device '{}' has too many address ranges",
            device.stable_id,
        );
        for range in &device.ranges {
            anyhow::ensure!(
                matches!(range.address_space.as_str(), "pmio" | "mmio") && range.length != 0,
                "snapshot device '{}' has an invalid address range",
                device.stable_id,
            );
            range.start.checked_add(range.length).with_context(|| {
                format!("snapshot device '{}' range overflows", device.stable_id)
            })?;
        }
        anyhow::ensure!(
            device.queue_count as usize == device.queue_max_sizes.len(),
            "snapshot device '{}' queue inventory is inconsistent",
            device.stable_id,
        );
    }

    anyhow::ensure!(
        !contract.state_unit_names.is_empty() && contract.state_unit_names.len() <= MAX_STATE_UNITS,
        "snapshot state-unit inventory size is invalid"
    );
    ensure_unique(&contract.state_unit_names, "state-unit name")?;
    let state_units = contract
        .state_unit_names
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    for device in &contract.devices {
        anyhow::ensure!(
            state_units.contains(device.state_unit_name.as_str()),
            "snapshot device '{}' references an unknown state unit",
            device.stable_id,
        );
    }

    anyhow::ensure!(
        contract.attachments.len() <= MAX_ATTACHMENTS,
        "snapshot attachment inventory is too large"
    );
    let mut attachment_ids = HashSet::new();
    for attachment in &contract.attachments {
        anyhow::ensure!(
            !attachment.stable_id.is_empty()
                && attachment_ids.insert(attachment.stable_id.as_str()),
            "snapshot contains an empty or duplicate attachment ID"
        );
        anyhow::ensure!(
            !attachment.kind.is_empty()
                && !attachment.reconnect_policy.is_empty()
                && !attachment.identity_kind.is_empty()
                && !attachment.identity.is_empty()
                && attachment.identity.len() <= MAX_ATTACHMENT_IDENTITY_BYTES,
            "snapshot attachment '{}' has an incomplete identity",
            attachment.stable_id,
        );
        anyhow::ensure!(
            match attachment.reconnect_policy.as_str() {
                "recreate-listener" => {
                    !attachment.required && attachment.reconnect_timeout_ms == 0
                }
                "reconnect-client" => {
                    attachment.required && attachment.reconnect_timeout_ms != 0
                }
                "require-inherited-attachment" => {
                    attachment.required && attachment.reconnect_timeout_ms == 0
                }
                "discard-while-disconnected" => {
                    !attachment.required && attachment.reconnect_timeout_ms == 0
                }
                "broker-authenticated-listener" => {
                    !attachment.required
                        && attachment.reconnect_timeout_ms == 0
                        && matches!(
                            attachment.identity_kind.as_str(),
                            "unix-socket" | "named-pipe"
                        )
                }
                "broker-disconnected" => {
                    !attachment.required
                        && attachment.reconnect_timeout_ms == 0
                        && attachment.identity_kind == "disconnected"
                        && attachment.identity == b"discard"
                }
                "recreate-endpoint" => {
                    !attachment.required && attachment.reconnect_timeout_ms == 0
                }
                "live-revalidate" => {
                    attachment.required
                        && attachment.reconnect_timeout_ms == 0
                        && attachment.length == 0
                }
                _ => false,
            },
            "snapshot attachment '{}' has an invalid reconnect policy",
            attachment.stable_id,
        );
    }
    Ok(())
}

pub(super) fn validate_snapshot_tier(manifest: &SnapshotManifest) -> anyhow::Result<()> {
    let Some(contract) = manifest.machine_contract.as_ref() else {
        anyhow::ensure!(
            manifest.snapshot_tier.is_empty()
                && manifest.restore_policy.is_empty()
                && manifest.consumed_config_sections == 0,
            "snapshot tier metadata requires a microVM machine contract"
        );
        return Ok(());
    };
    if contract.microvm_sandbox_blocks.is_empty() {
        anyhow::ensure!(
            manifest.snapshot_tier.is_empty()
                && manifest.restore_policy.is_empty()
                && manifest.consumed_config_sections == 0,
            "snapshot tier metadata requires microVM sandbox blocks"
        );
        return Ok(());
    }

    let paired_scratch = paired_scratch_block(manifest).is_some();
    let expected_consumed_sections = match manifest.snapshot_tier.as_str() {
        SNAPSHOT_TIER_PLATFORM => SNAPSHOT_CONFIG_INVARIANTS,
        SNAPSHOT_TIER_WORKLOAD_START | SNAPSHOT_TIER_INSTANCE_CHECKPOINT => SNAPSHOT_CONFIG_ALL,
        _ => 0,
    };
    let valid = manifest.consumed_config_sections == expected_consumed_sections
        && matches!(
            (
                manifest.snapshot_tier.as_str(),
                manifest.restore_policy.as_str(),
                paired_scratch,
            ),
            (SNAPSHOT_TIER_PLATFORM, SNAPSHOT_RESTORE_POLICY_CLONE, false)
                | (
                    SNAPSHOT_TIER_WORKLOAD_START,
                    SNAPSHOT_RESTORE_POLICY_CLONE,
                    true
                )
                | (
                    SNAPSHOT_TIER_INSTANCE_CHECKPOINT,
                    SNAPSHOT_RESTORE_POLICY_RESUME,
                    true
                )
        );
    anyhow::ensure!(
        valid,
        "snapshot tier '{}', restore policy '{}', and scratch policy are not a canonical microVM combination",
        manifest.snapshot_tier,
        manifest.restore_policy,
    );
    let expected_tier_token = format!("nvx_snapshot_tier={}", manifest.snapshot_tier);
    let tier_tokens = contract
        .effective_command_line
        .split_ascii_whitespace()
        .filter(|token| token.starts_with("nvx_snapshot_tier="))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        tier_tokens == [expected_tier_token.as_str()],
        "snapshot tier '{}' does not match its saved host policy",
        manifest.snapshot_tier,
    );
    let layers_are_unbound = contract
        .microvm_sandbox_blocks
        .iter()
        .filter(|block| block.read_only)
        .all(|block| block.identity_kind == "unbound" && block.identity.is_empty());
    anyhow::ensure!(
        layers_are_unbound == (manifest.snapshot_tier == SNAPSHOT_TIER_PLATFORM),
        "snapshot layer identity binding does not match tier '{}'",
        manifest.snapshot_tier,
    );
    if manifest.snapshot_tier == SNAPSHOT_TIER_PLATFORM {
        let control_tty_count = contract
            .effective_command_line
            .split_ascii_whitespace()
            .filter(|token| *token == openvmm_defs::microvm::MICROVM_CONTROL_TTY_COMMAND_LINE)
            .count();
        anyhow::ensure!(
            control_tty_count <= 1,
            "platform snapshot command line contains duplicate control tty configuration"
        );
        let processor_limit_tokens = contract
            .effective_command_line
            .split_ascii_whitespace()
            .filter(|token| token.starts_with("nr_cpus="))
            .collect::<Vec<_>>();
        anyhow::ensure!(
            processor_limit_tokens.len() <= 1,
            "platform snapshot command line contains duplicate processor capacity"
        );
        if let Some(processor_limit) = processor_limit_tokens.first() {
            let expected = format!("nr_cpus={}", contract.topology.apic_ids.len());
            anyhow::ensure!(
                **processor_limit == expected,
                "platform snapshot command line processor capacity does not match its machine contract"
            );
        }
        anyhow::ensure!(
            contract
                .effective_command_line
                .split_ascii_whitespace()
                .all(platform_command_line_token_is_invariant),
            "platform snapshot command line contains tenant or unsupported configuration"
        );
    }
    Ok(())
}

fn platform_command_line_token_is_invariant(token: &str) -> bool {
    matches!(
        token,
        "earlycon=xe9"
            | "console=hvc0"
            | "console=hvc1"
            | "reboot=t"
            | "panic=-1"
            | "nvx_sandbox=1"
            | "nvx_config=0xd0010000,65536"
            | "nvx_snapshot_tier=platform"
    ) || token.starts_with("nr_cpus=")
        || token == openvmm_defs::microvm::MICROVM_CONTROL_TTY_COMMAND_LINE
        || [
            "virtio_mmio.device=",
            "virtnet_ip=",
            "virtnet_mask=",
            "virtnet_gw=",
            "virtnet_dns=",
        ]
        .iter()
        .any(|prefix| token.starts_with(prefix))
}

fn ensure_unique<T>(values: &[T], description: &str) -> anyhow::Result<()>
where
    T: Eq + std::hash::Hash,
{
    let mut unique = HashSet::with_capacity(values.len());
    anyhow::ensure!(
        values.iter().all(|value| unique.insert(value)),
        "snapshot contains a duplicate {description}"
    );
    Ok(())
}

#[cfg(test)]
const TEST_DISTRO_IDENTITY_BYTE: u8 = 0x11;

#[cfg(test)]
pub(super) fn paired_scratch_manifest(scratch: &[u8]) -> SnapshotManifest {
    let mut manifest = super::tests::test_manifest();
    let mut contract = test_machine_contract();
    contract.microvm_sandbox_blocks = vec![
        SnapshotMicrovmSandboxBlock {
            role: "distro".to_owned(),
            read_only: true,
            length: 512,
            identity_kind: "sha256".to_owned(),
            identity: vec![TEST_DISTRO_IDENTITY_BYTE; SHA256_SIZE],
            artifact: String::new(),
            logical_block_size: 512,
            physical_block_size: 4096,
        },
        SnapshotMicrovmSandboxBlock {
            role: "scratch".to_owned(),
            read_only: false,
            length: scratch.len() as u64,
            identity_kind: "sha256".to_owned(),
            identity: sha2::Sha256::digest(scratch).to_vec(),
            artifact: SCRATCH_FILE_NAME.to_owned(),
            logical_block_size: 512,
            physical_block_size: 4096,
        },
    ];
    contract.set_effective_command_line("console=hvc0 nvx_snapshot_tier=workload-start".to_owned());
    manifest.machine_contract = Some(contract);
    manifest.snapshot_tier = SNAPSHOT_TIER_WORKLOAD_START.to_owned();
    manifest.restore_policy = SNAPSHOT_RESTORE_POLICY_CLONE.to_owned();
    manifest.consumed_config_sections = SNAPSHOT_CONFIG_ALL;
    manifest
}

#[cfg(test)]
pub(super) fn test_machine_contract() -> SnapshotMachineContract {
    let mut contract = SnapshotMachineContract {
        machine_profile: "microvm".to_owned(),
        microvm_abi_version: openvmm_defs::microvm::MICROVM_ABI_VERSION_2,
        source_hypervisor: "whp".to_owned(),
        effective_command_line: String::new(),
        effective_command_line_sha256: Vec::new(),
        memory_ranges: vec![SnapshotMemoryRange {
            gpa_start: 0,
            length: 1024,
            file_offset: 0,
        }],
        topology: SnapshotProcessorTopology {
            sockets: 1,
            dies_per_socket: 1,
            cores_per_die: 2,
            threads_per_core: 1,
            apic_ids: vec![0, 1],
        },
        devices: vec![
            SnapshotDevice {
                stable_id: "portb".to_owned(),
                state_unit_name: "portb".to_owned(),
                kind: "portb".to_owned(),
                order: 0,
                ranges: vec![SnapshotDeviceRange {
                    address_space: "pmio".to_owned(),
                    start: 0xe9,
                    length: 2,
                }],
                irq: None,
                transport: String::new(),
                feature_banks: Vec::new(),
                queue_count: 0,
                queue_max_sizes: Vec::new(),
            },
            SnapshotDevice {
                stable_id: "shutdown".to_owned(),
                state_unit_name: "shutdown".to_owned(),
                kind: "shutdown".to_owned(),
                order: 1,
                ranges: vec![SnapshotDeviceRange {
                    address_space: "pmio".to_owned(),
                    start: 0x604,
                    length: 1,
                }],
                irq: None,
                transport: String::new(),
                feature_banks: Vec::new(),
                queue_count: 0,
                queue_max_sizes: Vec::new(),
            },
        ],
        state_unit_names: vec!["portb".to_owned(), "shutdown".to_owned()],
        attachments: Vec::new(),
        boot_layout_version: MICROVM_BOOT_LAYOUT_VERSION,
        microvm_network: None,
        microvm_filesystem: None,
        microvm_additional_filesystems: Vec::new(),
        microvm_sandbox_blocks: Vec::new(),
        microvm_filesystem_slot_version: 0,
        boot_online_vp_count: 0,
        virtio_interrupt_mode: MICROVM_SHARED_STATUS_INTERRUPT_MODE.to_owned(),
        virtio_shared_status_page_gpa: openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_GPA,
        virtio_shared_status_page_size: openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_SIZE,
        memory_expansion_version: 0,
        memory_capacity_bytes: 0,
        memory_block_size_bytes: 0,
        memory_expansion_ranges: Vec::new(),
        time: Some(super::time::tests::test_time_contract()),
        cpu_profile: Some(super::time::tests::test_cpu_profile()),
    };
    contract.set_effective_command_line("console=hvc0".to_owned());
    contract
}

#[cfg(test)]
mod tests {
    use super::super::format::validate_manifest_contents;
    use super::super::tests::test_manifest;
    use super::*;

    const TEST_SCRATCH_IDENTITY_BYTE: u8 = 0x22;

    fn virtio_state_unit_name(kind: &str, mmio_base: u64) -> String {
        format!("{kind}-{mmio_base}")
    }

    #[test]
    fn restore_online_vp_count_is_bounded_by_template() {
        assert_eq!(
            microvm_boot_online_vp_count(8, "console=hvc0 maxcpus=1").unwrap(),
            1
        );
        assert_eq!(microvm_boot_online_vp_count(8, "console=hvc0").unwrap(), 0);
        assert!(microvm_boot_online_vp_count(8, "maxcpus=3").is_err());

        let mut manifest = test_manifest();
        manifest.vp_count = 8;
        let mut contract = test_machine_contract();
        contract.boot_online_vp_count = 2;
        manifest.machine_contract = Some(contract);

        for target in [2, 4, 8] {
            validate_restore_online_vp_count(&manifest, target).unwrap();
        }
        for target in [1, 3, 16] {
            assert!(validate_restore_online_vp_count(&manifest, target).is_err());
        }

        manifest.vp_count = 4;
        let error = validate_restore_online_vp_count(&manifest, 8).unwrap_err();
        assert!(error.to_string().contains("exceeds VP capacity 4"));
        manifest.vp_count = 8;

        manifest
            .machine_contract
            .as_mut()
            .unwrap()
            .boot_online_vp_count = 0;
        let error = validate_restore_online_vp_count(&manifest, 8).unwrap_err();
        assert!(error.to_string().contains("does not declare"));
    }

    fn microvm_console_attachment() -> SnapshotAttachment {
        SnapshotAttachment {
            stable_id: "console:microvm-virtio0".to_owned(),
            kind: "virtio-console".to_owned(),
            required: false,
            reconnect_policy: "recreate-listener".to_owned(),
            identity_kind: "tcp".to_owned(),
            identity: b"127.0.0.1:5555".to_vec(),
            length: 0,
            reconnect_timeout_ms: 0,
        }
    }

    fn microvm_control_console_attachment() -> SnapshotAttachment {
        SnapshotAttachment {
            stable_id: "console:microvm-control0".to_owned(),
            kind: "virtio-control-console".to_owned(),
            required: false,
            reconnect_policy: "broker-disconnected".to_owned(),
            identity_kind: "disconnected".to_owned(),
            identity: b"discard".to_vec(),
            length: 0,
            reconnect_timeout_ms: 0,
        }
    }

    fn microvm_control_console_listener_attachment() -> SnapshotAttachment {
        SnapshotAttachment {
            stable_id: "console:microvm-control0".to_owned(),
            kind: "virtio-control-console".to_owned(),
            required: false,
            reconnect_policy: "broker-authenticated-listener".to_owned(),
            identity_kind: "unix-socket".to_owned(),
            identity: b"/run/nvx/control.sock".to_vec(),
            length: 0,
            reconnect_timeout_ms: 0,
        }
    }

    fn microvm_network_attachment(source_hypervisor: &str) -> SnapshotAttachment {
        assert!(matches!(source_hypervisor, "kvm" | "mshv" | "whp"));
        SnapshotAttachment {
            stable_id: "net:microvm0".to_owned(),
            kind: "virtio-net".to_owned(),
            required: false,
            reconnect_policy: "recreate-endpoint".to_owned(),
            identity_kind: "user-mode-nat".to_owned(),
            identity: b"consomme".to_vec(),
            length: 0,
            reconnect_timeout_ms: 0,
        }
    }

    fn microvm_filesystem_attachment(source_hypervisor: &str) -> SnapshotAttachment {
        microvm_slot_attachment(source_hypervisor, 0)
    }

    fn microvm_slot_attachment(source_hypervisor: &str, slot: usize) -> SnapshotAttachment {
        SnapshotAttachment {
            stable_id: openvmm_defs::microvm::MICROVM_FILESYSTEM_SLOTS[slot]
                .stable_id
                .to_owned(),
            kind: "virtio-fs".to_owned(),
            required: true,
            reconnect_policy: "live-revalidate".to_owned(),
            identity_kind: match source_hypervisor {
                "kvm" | "mshv" => "unix-device-inode-v1",
                "whp" => "windows-volume-file-id-v1",
                _ => unreachable!(),
            }
            .to_owned(),
            identity: format!("root-object-v1-{slot}").into_bytes(),
            length: 0,
            reconnect_timeout_ms: 0,
        }
    }

    fn generated_network_contract(source_hypervisor: &str) -> SnapshotMachineContract {
        generated_network_contract_with_mode(
            source_hypervisor,
            net_backend_resources::egress::EgressPolicyMode::AllowList(vec![
                "192.0.2.0/24".parse().unwrap(),
            ]),
        )
    }

    fn generated_network_contract_with_mode(
        source_hypervisor: &str,
        mode: net_backend_resources::egress::EgressPolicyMode,
    ) -> SnapshotMachineContract {
        let network: openvmm_defs::microvm::MicrovmNetworkConfig = "10.0.0.2/24".parse().unwrap();
        let egress_policy = net_backend_resources::egress::EgressPolicy::bind(
            network.guest_ipv4,
            network.prefix_length,
            network.guest_mac,
            network.derived_gateway_ipv4,
            mode,
        )
        .unwrap();
        let irq = openvmm_defs::microvm::microvm_virtio_net_irq(Some(source_hypervisor)).unwrap();
        let command_line = format!(
            "earlycon=xe9 console=hvc0 reboot=t panic=-1 virtio_mmio.device=0x1000@0xd0000000:{irq} {}",
            network.command_line_fragment_with_dns(egress_policy.allows_gateway_dns())
        );
        microvm_machine_contract(
            source_hypervisor,
            MICROVM_BOOT_LAYOUT_VERSION,
            command_line,
            Some((
                &network,
                &egress_policy,
                microvm_network_attachment(source_hypervisor),
            )),
            false,
            Vec::new(),
            None,
            None,
            Vec::new(),
            1,
            1024,
            None,
            [
                "partition",
                "vmtime",
                "pic",
                "ioapic",
                "pit",
                "rtc",
                "microvm-portb",
                "microvm-shutdown",
                "microvm-snapshot-request",
                "virtio-net-3489660928",
            ]
            .map(str::to_owned)
            .to_vec(),
            crate::snapshot::time::tests::test_time_contract(),
            crate::snapshot::time::tests::test_cpu_profile(),
        )
        .unwrap()
    }

    fn generated_console_contract() -> SnapshotMachineContract {
        microvm_machine_contract(
            "whp",
            MICROVM_BOOT_LAYOUT_VERSION,
            "earlycon=xe9 console=hvc1 reboot=t panic=-1 virtio_mmio.device=0x1000@0xd0002000:7"
                .to_owned(),
            None,
            false,
            Vec::new(),
            Some(microvm_console_attachment()),
            None,
            Vec::new(),
            1,
            1024,
            None,
            [
                "partition",
                "vmtime",
                "pic",
                "ioapic",
                "pit",
                "rtc",
                "microvm-portb",
                "microvm-shutdown",
                "microvm-snapshot-request",
                "virtio-console-3489669120",
            ]
            .map(str::to_owned)
            .to_vec(),
            crate::snapshot::time::tests::test_time_contract(),
            crate::snapshot::time::tests::test_cpu_profile(),
        )
        .unwrap()
    }

    fn generated_control_console_contract_with_attachment(
        control_console_attachment: SnapshotAttachment,
    ) -> SnapshotMachineContract {
        let command_line = format!(
            "{} \
             virtio_mmio.device={:#x}@{:#x}:{} \
             virtio_mmio.device={:#x}@{:#x}:{} \
             virtio_mmio.device={:#x}@{:#x}:{} \
             virtio_mmio.device={:#x}@{:#x}:{} \
             {}",
            openvmm_defs::microvm::MICROVM_CONSOLE_COMMAND_LINE,
            openvmm_defs::microvm::MICROVM_VIRTIO_MMIO_LEN,
            openvmm_defs::microvm::MICROVM_VIRTIO_CONSOLE_MMIO_BASE,
            openvmm_defs::microvm::MICROVM_VIRTIO_CONSOLE_IRQ,
            openvmm_defs::microvm::MICROVM_VIRTIO_MMIO_LEN,
            openvmm_defs::microvm::MICROVM_VIRTIO_BLK_MMIO_BASE,
            openvmm_defs::microvm::MICROVM_VIRTIO_BLK_IRQ,
            openvmm_defs::microvm::MICROVM_VIRTIO_MMIO_LEN,
            openvmm_defs::microvm::MICROVM_VIRTIO_SANDBOX_BLOCK_MMIO_BASES[3],
            openvmm_defs::microvm::MICROVM_VIRTIO_SCRATCH_BLK_IRQ,
            openvmm_defs::microvm::MICROVM_VIRTIO_MMIO_LEN,
            openvmm_defs::microvm::MICROVM_VIRTIO_CONTROL_CONSOLE_MMIO_BASE,
            openvmm_defs::microvm::MICROVM_VIRTIO_CONTROL_CONSOLE_IRQ,
            openvmm_defs::microvm::MICROVM_CONTROL_TTY_COMMAND_LINE,
        );
        microvm_machine_contract(
            "whp",
            MICROVM_BOOT_LAYOUT_VERSION,
            command_line,
            None,
            false,
            Vec::new(),
            Some(microvm_console_attachment()),
            Some(control_console_attachment),
            vec![
                SnapshotMicrovmSandboxBlock {
                    role: "distro".to_owned(),
                    read_only: true,
                    length: 512,
                    identity_kind: "sha256".to_owned(),
                    identity: vec![0x11; SHA256_SIZE],
                    artifact: String::new(),
                    logical_block_size: 512,
                    physical_block_size: 4096,
                },
                SnapshotMicrovmSandboxBlock {
                    role: "scratch".to_owned(),
                    read_only: false,
                    length: 512,
                    identity_kind: "sha256".to_owned(),
                    identity: vec![TEST_SCRATCH_IDENTITY_BYTE; SHA256_SIZE],
                    artifact: SCRATCH_FILE_NAME.to_owned(),
                    logical_block_size: 512,
                    physical_block_size: 4096,
                },
            ],
            1,
            1024,
            None,
            vec![
                "partition".to_owned(),
                "vmtime".to_owned(),
                "pic".to_owned(),
                "ioapic".to_owned(),
                "pit".to_owned(),
                "rtc".to_owned(),
                "microvm-portb".to_owned(),
                "microvm-shutdown".to_owned(),
                "microvm-snapshot-request".to_owned(),
                virtio_state_unit_name(
                    "virtio-console",
                    openvmm_defs::microvm::MICROVM_VIRTIO_CONSOLE_MMIO_BASE,
                ),
                virtio_state_unit_name(
                    "virtio-blk",
                    openvmm_defs::microvm::MICROVM_VIRTIO_BLK_MMIO_BASE,
                ),
                virtio_state_unit_name(
                    "virtio-blk",
                    openvmm_defs::microvm::MICROVM_VIRTIO_SANDBOX_BLOCK_MMIO_BASES[3],
                ),
                virtio_state_unit_name(
                    "virtio-control-console",
                    openvmm_defs::microvm::MICROVM_VIRTIO_CONTROL_CONSOLE_MMIO_BASE,
                ),
            ],
            crate::snapshot::time::tests::test_time_contract(),
            crate::snapshot::time::tests::test_cpu_profile(),
        )
        .unwrap()
    }

    fn generated_control_console_contract() -> SnapshotMachineContract {
        generated_control_console_contract_with_attachment(microvm_control_console_attachment())
    }

    fn generated_filesystem_contract(source_hypervisor: &str) -> SnapshotMachineContract {
        generated_filesystem_contract_with_owner(
            source_hypervisor,
            openvmm_defs::microvm::MicrovmFilesystemOwner::Vmm,
        )
    }

    fn generated_filesystem_contract_with_owner(
        source_hypervisor: &str,
        owner: openvmm_defs::microvm::MicrovmFilesystemOwner,
    ) -> SnapshotMachineContract {
        let filesystem = openvmm_defs::microvm::MicrovmFilesystemConfig::new(
            "/mnt/share".to_owned(),
            openvmm_defs::microvm::MicrovmFilesystemAccess::ReadOnly,
        )
        .and_then(|config| config.with_denied_paths(vec!["secrets".to_owned()]))
        .unwrap()
        .with_owner(owner);
        let command_line = format!(
            "earlycon=xe9 console=hvc0 reboot=t panic=-1 virtio_mmio.device=0x1000@0xd0001000:6 {}",
            filesystem.command_line_fragment(&openvmm_defs::microvm::MICROVM_FILESYSTEM_SLOTS[0])
        );
        microvm_machine_contract(
            source_hypervisor,
            MICROVM_BOOT_LAYOUT_VERSION,
            command_line,
            None,
            true,
            vec![(
                &filesystem,
                Path::new(if cfg!(windows) {
                    r"C:\microvm-share"
                } else {
                    "/microvm-share"
                }),
                microvm_filesystem_attachment(source_hypervisor),
            )],
            None,
            None,
            Vec::new(),
            1,
            1024,
            None,
            [
                "partition",
                "vmtime",
                "pic",
                "ioapic",
                "pit",
                "rtc",
                "microvm-portb",
                "microvm-shutdown",
                "microvm-snapshot-request",
                "virtiofs-3489665024",
            ]
            .map(str::to_owned)
            .to_vec(),
            crate::snapshot::time::tests::test_time_contract(),
            crate::snapshot::time::tests::test_cpu_profile(),
        )
        .unwrap()
    }

    fn generated_dormant_filesystem_contract() -> SnapshotMachineContract {
        microvm_machine_contract(
            "whp",
            MICROVM_BOOT_LAYOUT_VERSION,
            "earlycon=xe9 console=hvc0 reboot=t panic=-1 virtio_mmio.device=0x1000@0xd0001000:6"
                .to_owned(),
            None,
            true,
            Vec::new(),
            None,
            None,
            Vec::new(),
            1,
            1024,
            None,
            [
                "partition",
                "vmtime",
                "pic",
                "ioapic",
                "pit",
                "rtc",
                "microvm-portb",
                "microvm-shutdown",
                "microvm-snapshot-request",
                "virtiofs-3489665024",
            ]
            .map(str::to_owned)
            .to_vec(),
            crate::snapshot::time::tests::test_time_contract(),
            crate::snapshot::time::tests::test_cpu_profile(),
        )
        .unwrap()
    }

    fn two_filesystems() -> [openvmm_defs::microvm::MicrovmFilesystemConfig; 2] {
        [
            openvmm_defs::microvm::MicrovmFilesystemConfig::new(
                "/workspace".to_owned(),
                openvmm_defs::microvm::MicrovmFilesystemAccess::ReadWrite,
            )
            .and_then(|config| config.with_denied_paths(vec!["secrets".to_owned()]))
            .unwrap(),
            openvmm_defs::microvm::MicrovmFilesystemConfig::new(
                "/opt/hostedtoolcache".to_owned(),
                openvmm_defs::microvm::MicrovmFilesystemAccess::ReadOnly,
            )
            .unwrap(),
        ]
    }

    fn microvm_share_path(slot: usize) -> &'static Path {
        Path::new(match (cfg!(windows), slot) {
            (true, 0) => r"C:\workspace",
            (true, _) => r"C:\toolcache",
            (false, 0) => "/workspace",
            (false, _) => "/toolcache",
        })
    }

    fn filesystems_contract(
        source_hypervisor: &str,
        command_line: String,
        filesystem_slot: bool,
        filesystems: Vec<(
            &openvmm_defs::microvm::MicrovmFilesystemConfig,
            &Path,
            SnapshotAttachment,
        )>,
    ) -> anyhow::Result<SnapshotMachineContract> {
        microvm_machine_contract(
            source_hypervisor,
            MICROVM_BOOT_LAYOUT_VERSION,
            command_line,
            None,
            filesystem_slot,
            filesystems,
            None,
            None,
            Vec::new(),
            1,
            1024,
            None,
            [
                "partition",
                "vmtime",
                "pic",
                "ioapic",
                "pit",
                "rtc",
                "microvm-portb",
                "microvm-shutdown",
                "microvm-snapshot-request",
                "virtiofs-3489665024",
                "virtiofs-3489693696",
            ]
            .map(str::to_owned)
            .to_vec(),
            crate::snapshot::time::tests::test_time_contract(),
            crate::snapshot::time::tests::test_cpu_profile(),
        )
    }

    fn slot_filesystems<'a>(
        source_hypervisor: &str,
        filesystems: &'a [openvmm_defs::microvm::MicrovmFilesystemConfig],
    ) -> Vec<(
        &'a openvmm_defs::microvm::MicrovmFilesystemConfig,
        &'static Path,
        SnapshotAttachment,
    )> {
        filesystems
            .iter()
            .enumerate()
            .map(|(slot, filesystem)| {
                (
                    filesystem,
                    microvm_share_path(slot),
                    microvm_slot_attachment(source_hypervisor, slot),
                )
            })
            .collect()
    }

    fn two_filesystem_command_line() -> String {
        filesystems_command_line(&two_filesystems())
    }

    fn filesystems_command_line(
        filesystems: &[openvmm_defs::microvm::MicrovmFilesystemConfig],
    ) -> String {
        let mut command_line =
            openvmm_defs::microvm::build_microvm_command_line(&[], false).unwrap();
        openvmm_defs::microvm::append_microvm_virtio_discovery(
            &mut command_line,
            None,
            true,
            filesystems,
            false,
            false,
            &[],
        )
        .unwrap();
        command_line
    }

    fn generated_two_filesystem_contract(source_hypervisor: &str) -> SnapshotMachineContract {
        let filesystems = two_filesystems();
        filesystems_contract(
            source_hypervisor,
            two_filesystem_command_line(),
            true,
            slot_filesystems(source_hypervisor, &filesystems),
        )
        .unwrap()
    }

    fn two_filesystem_manifest(contract: &SnapshotMachineContract) -> SnapshotManifest {
        let mut manifest = test_manifest();
        manifest.memory_size_bytes = 1024;
        manifest.vp_count = 1;
        manifest.machine_contract = Some(contract.clone());
        manifest
    }

    #[test]
    fn generated_microvm_two_filesystem_contract_has_fixed_abi() {
        for source_hypervisor in ["kvm", "mshv", "whp"] {
            let contract = generated_two_filesystem_contract(source_hypervisor);
            let first = contract
                .devices
                .iter()
                .find(|device| device.stable_id == "fs:microvm0")
                .unwrap();
            assert_eq!(first.state_unit_name, "virtiofs-3489665024");
            assert_eq!(first.ranges[0].start, 0xd000_1000);
            assert_eq!(first.irq, Some(6));
            // The second slot follows every other device, in address order.
            let second = contract.devices.last().unwrap();
            assert_eq!(second.stable_id, "fs:microvm1");
            assert_eq!(second.state_unit_name, "virtiofs-3489693696");
            assert_eq!(second.ranges[0].start, 0xd000_8000);
            assert_eq!(second.ranges[0].length, 0x1000);
            assert_eq!(second.irq, Some(13));
            assert_eq!(second.transport, "virtio-mmio");
            assert_eq!(second.feature_banks, first.feature_banks);
            assert_eq!(second.queue_max_sizes, first.queue_max_sizes);
            assert_eq!(
                contract.attachments,
                [
                    microvm_slot_attachment(source_hypervisor, 0),
                    microvm_slot_attachment(source_hypervisor, 1),
                ]
            );

            let workspace = contract.microvm_filesystem.as_ref().unwrap();
            assert_eq!(workspace.guest_mount_target, "/workspace");
            assert_eq!(workspace.access_mode, "rw");
            assert_eq!(workspace.tag, "microvm");
            assert_eq!(workspace.denied_paths, ["secrets"]);
            let [toolcache] = contract.microvm_additional_filesystems.as_slice() else {
                panic!("the second share is missing from the contract");
            };
            assert_eq!(toolcache.guest_mount_target, "/opt/hostedtoolcache");
            assert_eq!(toolcache.access_mode, "ro");
            assert_eq!(toolcache.tag, "microvm1");
            assert!(toolcache.denied_paths.is_empty());
            assert_eq!(
                toolcache.canonical_host_path,
                microvm_share_path(1).to_str().unwrap()
            );

            validate_microvm_machine_contract(&two_filesystem_manifest(&contract), &contract)
                .unwrap();
        }
    }

    #[test]
    fn validate_microvm_contract_rejects_a_missing_or_changed_second_share() {
        let contract = generated_two_filesystem_contract("kvm");
        let manifest = two_filesystem_manifest(&contract);
        let filesystems = two_filesystems();

        // A machine with only the first share has another command line and
        // device inventory, so it can't restore the two-share snapshot.
        let only_first = filesystems_contract(
            "kvm",
            filesystems_command_line(&filesystems[..1]),
            true,
            slot_filesystems("kvm", &filesystems[..1]),
        )
        .unwrap();
        let error = validate_microvm_machine_contract(&manifest, &only_first).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("doesn't match the requested machine")
        );

        let mut writable = filesystems.clone();
        writable[1] = openvmm_defs::microvm::MicrovmFilesystemConfig::new(
            "/opt/hostedtoolcache".to_owned(),
            openvmm_defs::microvm::MicrovmFilesystemAccess::ReadWrite,
        )
        .unwrap();
        let error = filesystems_contract(
            "kvm",
            contract.effective_command_line.clone(),
            true,
            slot_filesystems("kvm", &writable),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not match its saved policy")
        );

        let mut replaced = contract.clone();
        replaced.attachments[1].identity = b"another-root".to_vec();
        let error = validate_microvm_machine_contract(&manifest, &replaced).unwrap_err();
        assert!(error.to_string().contains("attachment inventory"));

        let mut moved = contract.clone();
        moved.microvm_additional_filesystems[0].canonical_host_path = "/moved".to_owned();
        let error = validate_microvm_machine_contract(&manifest, &moved).unwrap_err();
        assert!(error.to_string().contains("filesystem policy"));
    }

    #[test]
    fn microvm_contract_shape_rejects_an_inconsistent_second_slot() {
        let contract = generated_two_filesystem_contract("whp");
        validate_machine_contract_shape(&contract, 1024, 1).unwrap();

        let mut missing_device = contract.clone();
        missing_device.devices.pop();
        let mut missing_attachment = contract.clone();
        missing_attachment.attachments.pop();
        let mut missing_policy = contract.clone();
        missing_policy.microvm_additional_filesystems.clear();
        for invalid in [missing_device, missing_attachment, missing_policy] {
            let error = validate_machine_contract_shape(&invalid, 1024, 1).unwrap_err();
            assert!(error.to_string().contains("fs:microvm1"), "{error:#}");
        }

        let mut without_first = contract.clone();
        without_first.microvm_filesystem = None;
        let error = validate_machine_contract_shape(&without_first, 1024, 1).unwrap_err();
        assert!(error.to_string().contains("first slot"));

        let mut first_tag = contract.clone();
        first_tag.microvm_additional_filesystems[0].tag = "microvm".to_owned();
        let error = validate_machine_contract_shape(&first_tag, 1024, 1).unwrap_err();
        assert!(error.to_string().contains("not canonical"));

        let mut nested = contract.clone();
        nested.microvm_additional_filesystems[0].guest_mount_target = "/workspace/cache".to_owned();
        let error = validate_machine_contract_shape(&nested, 1024, 1).unwrap_err();
        assert!(format!("{error:#}").contains("overlap"));

        let mut too_many = contract.clone();
        too_many
            .microvm_additional_filesystems
            .push(too_many.microvm_additional_filesystems[0].clone());
        let error = validate_machine_contract_shape(&too_many, 1024, 1).unwrap_err();
        assert!(error.to_string().contains("more microVM filesystems"));
    }

    #[test]
    fn microvm_contract_builder_rejects_misplaced_second_share() {
        let filesystems = two_filesystems();
        let command_line = two_filesystem_command_line();

        // Shares occupy the slots after the reserved first slot.
        assert!(
            filesystems_contract(
                "kvm",
                command_line.clone(),
                false,
                slot_filesystems("kvm", &filesystems),
            )
            .is_err()
        );

        let mut swapped = slot_filesystems("kvm", &filesystems);
        swapped[0].2 = microvm_slot_attachment("kvm", 1);
        swapped[1].2 = microvm_slot_attachment("kvm", 0);
        let error = filesystems_contract("kvm", command_line.clone(), true, swapped).unwrap_err();
        assert!(error.to_string().contains("live-revalidation policy"));

        let undiscovered = command_line.replace(" virtio_mmio.device=0x1000@0xd0008000:13", "");
        let error = filesystems_contract(
            "kvm",
            undiscovered,
            true,
            slot_filesystems("kvm", &filesystems),
        )
        .unwrap_err();
        assert!(error.to_string().contains("fs:microvm1"));

        // The contract requires each slot's discovery token once and the
        // bootstrap triplets exactly, in slot order.
        let first = "virtfs_dir=/workspace virtfs_tag=microvm virtfs_mode=rw";
        let second = "virtfs_dir=/opt/hostedtoolcache virtfs_tag=microvm1 virtfs_mode=ro";
        assert!(command_line.ends_with(&format!("{first} {second}")));
        for invalid in [
            command_line.replace(&format!("{first} {second}"), &format!("{second} {first}")),
            format!("{command_line} {second}"),
            format!("{command_line} virtfs_mode=ro"),
            command_line.replace(
                " virtio_mmio.device=0x1000@0xd0008000:13",
                " virtio_mmio.device=0x1000@0xd0008000:13 virtio_mmio.device=0x1000@0xd0008000:13",
            ),
        ] {
            assert!(
                filesystems_contract("kvm", invalid, true, slot_filesystems("kvm", &filesystems))
                    .is_err()
            );
        }
        let error = filesystems_contract(
            "kvm",
            command_line.clone(),
            true,
            slot_filesystems("kvm", &filesystems[..1]),
        )
        .unwrap_err();
        assert!(error.to_string().contains("fs:microvm1"), "{error:#}");
    }

    #[test]
    fn generated_microvm_console_contract_has_fixed_abi() {
        let contract = generated_console_contract();
        let console = contract.devices.last().unwrap();
        assert_eq!(console.stable_id, "console:microvm-virtio0");
        assert_eq!(console.state_unit_name, "virtio-console-3489669120");
        assert_eq!(console.ranges[0].start, 0xd000_2000);
        assert_eq!(console.ranges[0].length, 0x1000);
        assert_eq!(console.irq, Some(7));
        assert_eq!(console.transport, "virtio-mmio");
        assert_eq!(console.feature_banks, [0x3000_0001, 0x0000_0003]);
        assert_eq!(console.queue_max_sizes, [256, 256]);
        assert_eq!(contract.attachments, [microvm_console_attachment()]);
    }

    #[test]
    fn generated_microvm_control_console_contract_has_distinct_fixed_abi() {
        let contract = generated_control_console_contract();
        let boot = contract
            .devices
            .iter()
            .find(|device| device.stable_id == "console:microvm-virtio0")
            .unwrap();
        let control = contract
            .devices
            .iter()
            .find(|device| device.stable_id == "console:microvm-control0")
            .unwrap();
        assert_eq!(boot.stable_id, "console:microvm-virtio0");
        assert_eq!(control.stable_id, "console:microvm-control0");
        assert_eq!(control.state_unit_name, "virtio-control-console-3489689600");
        assert_eq!(control.ranges[0].start, 0xd000_7000);
        assert_eq!(control.ranges[0].length, 0x1000);
        assert_eq!(control.irq, Some(3));
        assert_eq!(control.transport, "virtio-mmio");
        assert_eq!(control.feature_banks, boot.feature_banks);
        assert_eq!(control.queue_max_sizes, [256, 256]);
        assert_eq!(
            contract.attachments,
            [
                microvm_console_attachment(),
                microvm_control_console_attachment()
            ]
        );
    }

    #[test]
    fn generated_microvm_control_console_contract_accepts_broker_listener() {
        let attachment = microvm_control_console_listener_attachment();
        let contract = generated_control_console_contract_with_attachment(attachment.clone());
        assert_eq!(contract.attachments[1], attachment);
        validate_machine_contract_shape(&contract, 1024, 1).unwrap();
    }

    #[test]
    fn generated_microvm_network_contract_has_backend_specific_fixed_abi() {
        for (source_hypervisor, irq) in [("kvm", 10), ("mshv", 10), ("whp", 5)] {
            let contract = generated_network_contract(source_hypervisor);
            let network_device = contract.devices.last().unwrap();
            assert_eq!(network_device.stable_id, "net:microvm0");
            assert_eq!(network_device.state_unit_name, "virtio-net-3489660928");
            assert_eq!(network_device.ranges[0].start, 0xd000_0000);
            assert_eq!(network_device.ranges[0].length, 0x1000);
            assert_eq!(network_device.irq, Some(irq));
            assert_eq!(network_device.transport, "virtio-mmio");
            assert_eq!(network_device.feature_banks, [0x20, 0x1]);
            assert_eq!(network_device.queue_count, 2);
            assert_eq!(network_device.queue_max_sizes, [256, 256]);
            assert_eq!(
                contract.attachments,
                [microvm_network_attachment(source_hypervisor)]
            );

            let network = contract.microvm_network.unwrap();
            assert_eq!(network.profile, "portable");
            assert_eq!(
                network.guest_ipv4,
                u32::from(std::net::Ipv4Addr::new(10, 0, 0, 2))
            );
            assert_eq!(network.prefix_length, 24);
            assert_eq!(
                network.gateway_ipv4,
                u32::from(std::net::Ipv4Addr::new(10, 0, 0, 1))
            );
            assert_eq!(network.guest_mac, [0x52, 0x54, 0, 0, 0, 2]);
            assert_eq!(network.gateway_mac, [0x52, 0x54, 0, 0, 0, 1]);
            assert_eq!(network.egress_policy_mode, "allow-list");
            assert_eq!(network.egress_policy_sha256.len(), 32);
            assert!(network.egress_policy_required);
            assert_eq!(
                network.egress_policy_encoding_version,
                net_backend_resources::egress::EGRESS_POLICY_ENCODING_VERSION
            );
        }
    }

    #[test]
    fn generated_microvm_network_contract_accepts_deny_all_egress() {
        let contract = generated_network_contract_with_mode(
            "whp",
            net_backend_resources::egress::EgressPolicyMode::DenyAll,
        );

        let network = contract.microvm_network.as_ref().unwrap();
        assert_eq!(network.egress_policy_mode, "deny-all");
        assert!(network.egress_policy_required);
        assert!(!contract.effective_command_line.contains("virtnet_dns="));
        validate_supported_microvm_contract(&contract).unwrap();
    }

    #[test]
    fn legacy_network_policy_digest_remains_valid() {
        let network: openvmm_defs::microvm::MicrovmNetworkConfig = "10.0.0.2/24".parse().unwrap();
        let policy = net_backend_resources::egress::EgressPolicy::bind(
            network.guest_ipv4,
            network.prefix_length,
            network.guest_mac,
            network.derived_gateway_ipv4,
            net_backend_resources::egress::EgressPolicyMode::TcpEndpoints(vec![
                "10.0.0.9:443".parse().unwrap(),
                "192.0.2.7:443".parse().unwrap(),
            ]),
        )
        .unwrap();
        let mut saved = SnapshotMicrovmNetwork::new(&network, &policy);
        saved.egress_policy_encoding_version = 0;
        saved.egress_policy_sha256 =
            sha2::Sha256::digest(policy.canonical_bytes_for_version(1).unwrap()).to_vec();

        validate_microvm_network_policy(&saved, &policy).unwrap();
        saved.egress_policy_encoding_version = u32::MAX;
        assert!(validate_microvm_network_policy(&saved, &policy).is_err());
    }

    #[test]
    fn generated_microvm_filesystem_contract_has_fixed_abi() {
        for source_hypervisor in ["kvm", "mshv", "whp"] {
            let contract = generated_filesystem_contract(source_hypervisor);
            let filesystem_device = contract.devices.last().unwrap();
            assert_eq!(filesystem_device.stable_id, "fs:microvm0");
            assert_eq!(filesystem_device.state_unit_name, "virtiofs-3489665024");
            assert_eq!(filesystem_device.ranges[0].start, 0xd000_1000);
            assert_eq!(filesystem_device.ranges[0].length, 0x1000);
            assert_eq!(filesystem_device.irq, Some(6));
            assert_eq!(filesystem_device.transport, "virtio-mmio");
            assert_eq!(filesystem_device.feature_banks, [0x3000_0000, 0x0000_0003]);
            assert_eq!(filesystem_device.queue_count, 2);
            assert_eq!(filesystem_device.queue_max_sizes, [256, 256]);
            assert_eq!(
                contract.attachments,
                [microvm_filesystem_attachment(source_hypervisor)]
            );

            let filesystem = contract.microvm_filesystem.unwrap();
            assert_eq!(filesystem.guest_mount_target, "/mnt/share");
            assert_eq!(filesystem.access_mode, "ro");
            assert_eq!(
                filesystem.canonical_host_path,
                if cfg!(windows) {
                    r"C:\microvm-share"
                } else {
                    "/microvm-share"
                }
            );
            assert_eq!(filesystem.restore_mode, "live-revalidate");
            assert_eq!(filesystem.tag, "microvm");
            assert_eq!(filesystem.high_priority_queue_count, 1);
            assert_eq!(filesystem.request_queue_count, 1);
            assert_eq!(filesystem.shared_memory_size, 0);
            assert!(filesystem.direct_io);
            assert_eq!(filesystem.entry_cache_timeout_ns, 0);
            assert_eq!(filesystem.attribute_cache_timeout_ns, 0);
        }
    }

    #[test]
    fn generated_dormant_microvm_filesystem_contract_reserves_fixed_slot() {
        let contract = generated_dormant_filesystem_contract();
        assert_eq!(
            contract.microvm_filesystem_slot_version,
            MICROVM_FILESYSTEM_SLOT_VERSION
        );
        assert!(contract.microvm_filesystem.is_none());
        assert!(contract.attachments.is_empty());
        let filesystem = contract.devices.last().unwrap();
        assert_eq!(filesystem.stable_id, "fs:microvm0");
        assert_eq!(filesystem.state_unit_name, "virtiofs-3489665024");
        assert_eq!(filesystem.ranges[0].start, 0xd000_1000);
        assert_eq!(filesystem.irq, Some(6));
    }

    #[test]
    fn dormant_microvm_filesystem_capability_requires_fixed_device() {
        let mut contract = generated_dormant_filesystem_contract();
        contract.devices.pop();
        assert!(validate_machine_contract_shape(&contract, 1024, 1).is_err());
    }

    #[test]
    fn microvm_filesystem_owner_mode_is_canonical() {
        use openvmm_defs::microvm::MicrovmFilesystemOwner;
        assert_eq!(
            snapshot_microvm_filesystem_owner("").unwrap(),
            MicrovmFilesystemOwner::Vmm
        );
        assert_eq!(
            snapshot_microvm_filesystem_owner("caller").unwrap(),
            MicrovmFilesystemOwner::Caller
        );
        for mode in ["vmm", "root", "Caller"] {
            assert!(snapshot_microvm_filesystem_owner(mode).is_err());
        }
        // VMM ownership keeps the encoding of snapshots that predate the field.
        for (owner, mode) in [
            (MicrovmFilesystemOwner::Vmm, ""),
            (MicrovmFilesystemOwner::Caller, "caller"),
        ] {
            let contract = generated_filesystem_contract_with_owner("kvm", owner);
            assert_eq!(
                contract.microvm_filesystem.as_ref().unwrap().owner_mode,
                mode
            );
            validate_machine_contract_shape(&contract, 1024, 1).unwrap();
        }
        let mut contract = generated_filesystem_contract("kvm");
        contract.microvm_filesystem.as_mut().unwrap().owner_mode = "vmm".to_owned();
        let error = validate_machine_contract_shape(&contract, 1024, 1).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("owner mode 'vmm' is unsupported")
        );
    }

    #[test]
    fn validate_microvm_filesystem_contract_rejects_policy_change() {
        let contract = generated_filesystem_contract("whp");
        let mut manifest = test_manifest();
        manifest.memory_size_bytes = 1024;
        manifest.vp_count = 1;
        manifest.machine_contract = Some(contract.clone());
        manifest
            .machine_contract
            .as_mut()
            .unwrap()
            .microvm_filesystem
            .as_mut()
            .unwrap()
            .request_queue_count = 2;

        let error = validate_microvm_machine_contract(&manifest, &contract).unwrap_err();
        assert!(error.to_string().contains("not canonical"));
    }

    #[test]
    fn validate_microvm_network_contract_rejects_noncanonical_identity() {
        let contract = generated_network_contract("whp");
        let mut manifest = test_manifest();
        manifest.memory_size_bytes = 1024;
        manifest.vp_count = 1;
        manifest.machine_contract = Some(contract.clone());
        manifest
            .machine_contract
            .as_mut()
            .unwrap()
            .microvm_network
            .as_mut()
            .unwrap()
            .guest_mac[5] = 3;

        let error = validate_microvm_machine_contract(&manifest, &contract).unwrap_err();
        assert!(error.to_string().contains("not canonical"));
    }

    #[test]
    fn validate_microvm_network_contract_rejects_unsupported_profile() {
        let contract = generated_network_contract("whp");
        let mut manifest = test_manifest();
        manifest.memory_size_bytes = 1024;
        manifest.vp_count = 1;
        manifest.machine_contract = Some(contract.clone());
        manifest
            .machine_contract
            .as_mut()
            .unwrap()
            .microvm_network
            .as_mut()
            .unwrap()
            .profile = "other".to_owned();

        let error = validate_microvm_machine_contract(&manifest, &contract).unwrap_err();
        assert!(error.to_string().contains("profile"));
    }

    #[test]
    fn validate_microvm_network_contract_rejects_legacy_missing_profile() {
        let contract = generated_network_contract("whp");
        let mut manifest = test_manifest();
        manifest.memory_size_bytes = 1024;
        manifest.vp_count = 1;
        manifest.machine_contract = Some(contract.clone());
        manifest
            .machine_contract
            .as_mut()
            .unwrap()
            .microvm_network
            .as_mut()
            .unwrap()
            .profile
            .clear();

        let error = validate_microvm_machine_contract(&manifest, &contract).unwrap_err();
        assert!(error.to_string().contains("profile '' is unsupported"));
    }

    #[test]
    fn validate_microvm_console_contract_rejects_attachment_change() {
        let contract = generated_console_contract();
        let mut manifest = test_manifest();
        manifest.memory_size_bytes = 1024;
        manifest.vp_count = 1;
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.attachments[0].identity = b"127.0.0.1:6666".to_vec();
        let error = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(error.to_string().contains("attachment inventory"));
    }

    #[test]
    fn validate_microvm_control_console_contract_rejects_attachment_change() {
        let contract = generated_control_console_contract();
        let mut manifest = test_manifest();
        manifest.memory_size_bytes = 1024;
        manifest.vp_count = 1;
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected
            .attachments
            .iter_mut()
            .find(|attachment| attachment.stable_id == "console:microvm-control0")
            .unwrap()
            .identity = b"unexpected".to_vec();
        let error = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(error.to_string().contains("attachment inventory"));
    }

    #[test]
    fn validate_microvm_machine_contract_ok() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        validate_microvm_machine_contract(&manifest, &contract).unwrap();
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_abi() {
        let mut manifest = test_manifest();
        let expected = test_machine_contract();
        let mut contract = expected.clone();
        contract.microvm_abi_version = 1;
        manifest.machine_contract = Some(contract);
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("ABI version 1 is unsupported"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_backend() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.source_hypervisor = "kvm".to_owned();
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("source hypervisor"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_command_line() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.set_effective_command_line("console=other".to_owned());
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("command line"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_unsupported_boot_layout() {
        let mut manifest = test_manifest();
        let expected = test_machine_contract();
        let mut contract = expected.clone();
        contract.boot_layout_version = 1;
        manifest.machine_contract = Some(contract);
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(
            err.to_string()
                .contains("boot layout version 1 is unsupported")
        );
    }

    #[test]
    fn microvm_snapshot_topology_is_canonical() {
        for processor_count in [1, 2, 4, 8] {
            let topology = microvm_snapshot_topology(processor_count).unwrap();
            assert_eq!(topology.sockets, 1);
            assert_eq!(topology.dies_per_socket, 1);
            assert_eq!(topology.cores_per_die, processor_count);
            assert_eq!(topology.threads_per_core, 1);
            assert_eq!(topology.apic_ids, (0..processor_count).collect::<Vec<_>>());
            assert_eq!(MICROVM_BOOT_LAYOUT_VERSION, 2);
        }

        for processor_count in [0, 3, 5, 16] {
            assert!(microvm_snapshot_topology(processor_count).is_err());
        }
    }

    #[test]
    fn microvm_snapshot_identifies_shared_status_page() {
        let contract = test_machine_contract();
        let mut manifest = test_manifest();
        manifest.machine_contract = Some(contract.clone());

        validate_microvm_machine_contract(&manifest, &contract).unwrap();

        manifest
            .machine_contract
            .as_mut()
            .unwrap()
            .virtio_shared_status_page_gpa += 0x1000;
        let error = validate_microvm_machine_contract(&manifest, &contract).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("shared-status interrupt contract")
        );
    }

    #[test]
    fn validate_microvm_snapshot_rejects_noncanonical_apic_ids() {
        let mut manifest = test_manifest();
        let mut contract = test_machine_contract();
        contract.topology.apic_ids = vec![0, 2];
        manifest.machine_contract = Some(contract.clone());

        let error = validate_microvm_machine_contract(&manifest, &contract).unwrap_err();
        assert!(error.to_string().contains("not canonical"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_topology() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.topology.apic_ids.swap(0, 1);
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("processor topology"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_device_order() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.devices.reverse();
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("device inventory"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_removed_device() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.devices.pop();
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("device inventory"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_state_unit_order() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.state_unit_names.reverse();
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("state-unit inventory"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_duplicate_state_unit() {
        let mut manifest = test_manifest();
        let mut contract = test_machine_contract();
        contract.state_unit_names.push("portb".to_owned());
        manifest.machine_contract = Some(contract.clone());
        let err = validate_microvm_machine_contract(&manifest, &contract).unwrap_err();
        assert!(err.to_string().contains("duplicate state-unit name"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_overlapping_ram() {
        let mut manifest = test_manifest();
        let mut contract = test_machine_contract();
        contract.memory_ranges = vec![
            SnapshotMemoryRange {
                gpa_start: 0,
                length: 768,
                file_offset: 0,
            },
            SnapshotMemoryRange {
                gpa_start: 512,
                length: 256,
                file_offset: 768,
            },
        ];
        manifest.machine_contract = Some(contract.clone());
        let err = validate_microvm_machine_contract(&manifest, &contract).unwrap_err();
        assert!(err.to_string().contains("overlap in GPA space"));
    }

    fn expandable_memory_manifest(base_memory: u64, capacity: u64) -> SnapshotManifest {
        let mut manifest = test_manifest();
        manifest.memory_size_bytes = base_memory;
        let mut contract = test_machine_contract();
        contract.memory_ranges = canonical_microvm_memory_ranges(base_memory).unwrap();
        contract.memory_expansion_version = MICROVM_MEMORY_EXPANSION_VERSION;
        contract.memory_capacity_bytes = capacity;
        contract.memory_block_size_bytes = MICROVM_MEMORY_BLOCK_SIZE_BYTES;
        contract.memory_expansion_ranges =
            canonical_memory_expansion_ranges(base_memory, capacity).unwrap();
        manifest.machine_contract = Some(contract);
        manifest
    }

    #[test]
    fn restore_memory_targets_select_canonical_prefixes() {
        const MB: u64 = 1024 * 1024;
        const GB: u64 = 1024 * MB;
        let manifest = expandable_memory_manifest(512 * MB, 2 * GB);

        assert_eq!(
            validate_restore_memory_target(&manifest, 512 * MB).unwrap(),
            []
        );
        assert_eq!(
            validate_restore_memory_target(&manifest, GB).unwrap(),
            [SnapshotMemoryExpansionRange {
                gpa_start: 512 * MB,
                length: 512 * MB,
            }]
        );
        assert_eq!(
            validate_restore_memory_target(&manifest, 2 * GB).unwrap(),
            [SnapshotMemoryExpansionRange {
                gpa_start: 512 * MB,
                length: 1536 * MB,
            }]
        );
    }

    #[test]
    fn restore_memory_target_validation_rejects_invalid_requests() {
        const MB: u64 = 1024 * 1024;
        const GB: u64 = 1024 * MB;
        let manifest = expandable_memory_manifest(512 * MB, 2 * GB);

        assert!(
            validate_restore_memory_target(&manifest, 384 * MB)
                .unwrap_err()
                .to_string()
                .contains("below snapshot RAM")
        );
        assert!(
            validate_restore_memory_target(&manifest, 640 * MB + 1)
                .unwrap_err()
                .to_string()
                .contains("not aligned")
        );
        assert!(
            validate_restore_memory_target(&manifest, 2 * GB + 128 * MB)
                .unwrap_err()
                .to_string()
                .contains("exceeds RAM capacity")
        );
    }

    #[test]
    fn restore_memory_contract_rejects_malformed_and_legacy_snapshots() {
        const MB: u64 = 1024 * 1024;
        const GB: u64 = 1024 * MB;

        let mut overlap = expandable_memory_manifest(512 * MB, 2 * GB);
        overlap
            .machine_contract
            .as_mut()
            .unwrap()
            .memory_expansion_ranges[0]
            .gpa_start = 256 * MB;
        assert!(
            validate_restore_memory_target(&overlap, GB)
                .unwrap_err()
                .to_string()
                .contains("not canonical")
        );

        let mut version = expandable_memory_manifest(512 * MB, 2 * GB);
        version
            .machine_contract
            .as_mut()
            .unwrap()
            .memory_expansion_version += 1;
        assert!(
            validate_restore_memory_target(&version, GB)
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );

        let mut legacy = test_manifest();
        legacy.machine_contract = Some(test_machine_contract());
        assert!(
            validate_restore_memory_target(&legacy, 1024)
                .unwrap_err()
                .to_string()
                .contains("does not declare")
        );
    }

    #[test]
    fn memory_capacity_contract_rejects_alignment_and_overflow() {
        const MB: u64 = 1024 * 1024;
        assert!(canonical_memory_expansion_ranges(512 * MB, 512 * MB - 1).is_err());
        assert!(
            microvm_machine_contract(
                "whp",
                MICROVM_BOOT_LAYOUT_VERSION,
                "console=hvc0".to_owned(),
                None,
                false,
                Vec::new(),
                None,
                None,
                Vec::new(),
                1,
                512 * MB,
                Some(512 * MB + 1),
                Vec::new(),
                crate::snapshot::time::tests::test_time_contract(),
                crate::snapshot::time::tests::test_cpu_profile(),
            )
            .is_err()
        );
        assert!(canonical_microvm_memory_ranges(u64::MAX).is_err());
    }

    fn canonical_worker_platform_command_line(processor_count: u32) -> String {
        let mut command_line = openvmm_defs::microvm::build_microvm_control_command_line(
            &[
                "nvx_sandbox=1".to_owned(),
                "nvx_config=0xd0010000,65536".to_owned(),
            ],
            true,
        )
        .unwrap();
        openvmm_defs::microvm::append_microvm_processor_limit(&mut command_line, processor_count)
            .unwrap();
        command_line.push_str(" nvx_snapshot_tier=platform");
        openvmm_defs::microvm::append_microvm_virtio_discovery(
            &mut command_line,
            None,
            false,
            &[],
            true,
            true,
            &[
                openvmm_defs::microvm::MicrovmSandboxBlockConfig {
                    role: openvmm_defs::microvm::MicrovmSandboxBlockRole::Distro,
                    read_only: true,
                },
                openvmm_defs::microvm::MicrovmSandboxBlockConfig {
                    role: openvmm_defs::microvm::MicrovmSandboxBlockRole::Scratch,
                    read_only: false,
                },
            ],
        )
        .unwrap();
        command_line
    }

    fn make_platform_snapshot(manifest: &mut SnapshotManifest) {
        manifest.snapshot_tier = SNAPSHOT_TIER_PLATFORM.to_owned();
        manifest.restore_policy = SNAPSHOT_RESTORE_POLICY_CLONE.to_owned();
        manifest.consumed_config_sections = SNAPSHOT_CONFIG_INVARIANTS;
        let contract = manifest.machine_contract.as_mut().unwrap();
        for block in contract
            .microvm_sandbox_blocks
            .iter_mut()
            .filter(|block| block.read_only)
        {
            block.identity_kind = "unbound".to_owned();
            block.identity.clear();
        }
        let scratch = contract.microvm_sandbox_blocks.last_mut().unwrap();
        scratch.identity_kind = "fresh".to_owned();
        scratch.identity.clear();
        scratch.artifact.clear();
        let processor_count = u32::try_from(contract.topology.apic_ids.len()).unwrap();
        contract
            .set_effective_command_line(canonical_worker_platform_command_line(processor_count));
    }

    #[test]
    fn abi_v2_snapshot_tier_contract_is_canonical() {
        let scratch = vec![0x5a; 512];
        let mut manifest = paired_scratch_manifest(&scratch);
        validate_manifest_contents(&manifest).unwrap();

        manifest.snapshot_tier = SNAPSHOT_TIER_INSTANCE_CHECKPOINT.to_owned();
        manifest.restore_policy = SNAPSHOT_RESTORE_POLICY_RESUME.to_owned();
        manifest.consumed_config_sections = SNAPSHOT_CONFIG_ALL;
        manifest
            .machine_contract
            .as_mut()
            .unwrap()
            .set_effective_command_line(
                "console=hvc0 nvx_snapshot_tier=instance-checkpoint".to_owned(),
            );
        validate_manifest_contents(&manifest).unwrap();

        manifest.snapshot_tier = SNAPSHOT_TIER_WORKLOAD_START.to_owned();
        manifest.restore_policy = SNAPSHOT_RESTORE_POLICY_CLONE.to_owned();
        assert!(validate_manifest_contents(&manifest).is_err());

        manifest.snapshot_tier = SNAPSHOT_TIER_PLATFORM.to_owned();
        manifest.restore_policy = SNAPSHOT_RESTORE_POLICY_CLONE.to_owned();
        manifest.consumed_config_sections = SNAPSHOT_CONFIG_INVARIANTS;
        assert!(validate_manifest_contents(&manifest).is_err());

        make_platform_snapshot(&mut manifest);
        validate_manifest_contents(&manifest).unwrap();
    }

    #[test]
    fn platform_snapshot_rejects_tenant_command_line() {
        let scratch = vec![0x5a; 512];
        let mut manifest = paired_scratch_manifest(&scratch);
        make_platform_snapshot(&mut manifest);
        let contract = manifest.machine_contract.as_mut().unwrap();
        contract.set_effective_command_line(contract.effective_command_line.replace(
            "nvx_snapshot_tier=platform",
            "nvx_snapshot_tier=platform nvx_entrypoint=/tenant",
        ));

        assert!(
            validate_manifest_contents(&manifest)
                .unwrap_err()
                .to_string()
                .contains("contains tenant or unsupported configuration")
        );

        make_platform_snapshot(&mut manifest);
        validate_manifest_contents(&manifest).unwrap();

        let contract = manifest.machine_contract.as_mut().unwrap();
        contract.set_effective_command_line(
            contract
                .effective_command_line
                .replace("nvx_config=0xd0010000,65536", "nvx_config=tenant-data"),
        );
        assert!(
            validate_manifest_contents(&manifest)
                .unwrap_err()
                .to_string()
                .contains("contains tenant or unsupported configuration")
        );
    }

    #[test]
    fn platform_snapshot_rejects_mismatched_processor_limit() {
        let scratch = vec![0x5a; 512];
        let mut manifest = paired_scratch_manifest(&scratch);
        make_platform_snapshot(&mut manifest);
        let contract = manifest.machine_contract.as_mut().unwrap();
        contract.set_effective_command_line(
            contract
                .effective_command_line
                .replace("nr_cpus=2", "nr_cpus=1"),
        );

        assert!(
            validate_manifest_contents(&manifest)
                .unwrap_err()
                .to_string()
                .contains("processor capacity")
        );
    }

    #[test]
    fn platform_snapshot_accepts_worker_effective_canonical_command_line() {
        let scratch = vec![0x5a; 512];
        let mut manifest = paired_scratch_manifest(&scratch);
        make_platform_snapshot(&mut manifest);

        assert_eq!(
            manifest
                .machine_contract
                .as_ref()
                .unwrap()
                .effective_command_line,
            "earlycon=xe9 console=hvc1 reboot=t panic=-1 \
                  nvx_sandbox=1 nvx_config=0xd0010000,65536 nr_cpus=2 \
                 nvx_snapshot_tier=platform \
                 virtio_mmio.device=0x1000@0xd0002000:7 \
                 virtio_mmio.device=0x1000@0xd0003000:4 \
                 virtio_mmio.device=0x1000@0xd0006000:11 \
             virtio_mmio.device=0x1000@0xd0007000:3 \
             nvx_control_tty=hvc2"
        );
        validate_manifest_contents(&manifest).unwrap();
    }

    #[test]
    fn platform_snapshot_carries_no_clock_token() {
        let scratch = vec![0x5a; 512];
        let mut manifest = paired_scratch_manifest(&scratch);
        make_platform_snapshot(&mut manifest);
        validate_manifest_contents(&manifest).unwrap();
        let canonical = manifest
            .machine_contract
            .as_ref()
            .unwrap()
            .effective_command_line
            .clone();

        for token in ["tsc_early_khz=2100000", "lapic_timer_hz=200000000"] {
            manifest
                .machine_contract
                .as_mut()
                .unwrap()
                .set_effective_command_line(format!("{canonical} {token}"));
            let err = validate_manifest_contents(&manifest)
                .unwrap_err()
                .to_string();
            assert!(err.contains("[E_CMDLINE_CLOCK_TOKEN]"), "{err}");
            assert!(!platform_command_line_token_is_invariant(token));
        }
    }

    #[test]
    fn platform_snapshot_rejects_invalid_control_tokens() {
        let scratch = vec![0x5a; 512];
        let mut manifest = paired_scratch_manifest(&scratch);
        make_platform_snapshot(&mut manifest);
        let canonical = manifest
            .machine_contract
            .as_ref()
            .unwrap()
            .effective_command_line
            .clone();

        for invalid_control in [
            "nvx_control_tty=hvc9",
            "nvx_control_tty=hvc2 nvx_control_tty=hvc2",
        ] {
            manifest
                .machine_contract
                .as_mut()
                .unwrap()
                .set_effective_command_line(
                    canonical.replace("nvx_control_tty=hvc2", invalid_control),
                );
            assert!(validate_manifest_contents(&manifest).is_err());
        }
    }

    #[test]
    fn abi_v2_contract_rejects_scratch_without_a_lower_layer() {
        let scratch = vec![0x5a_u8; 1024];
        let mut manifest = paired_scratch_manifest(&scratch);
        manifest
            .machine_contract
            .as_mut()
            .unwrap()
            .microvm_sandbox_blocks
            .remove(0);
        let contract = manifest.machine_contract.clone().unwrap();

        let error = validate_microvm_machine_contract(&manifest, &contract).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("one to three layers and scratch")
        );
    }
}
