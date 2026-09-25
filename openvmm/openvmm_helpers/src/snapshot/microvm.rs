// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM snapshot machine-contract types, construction, and validation.

use super::SnapshotManifest;
use super::format::validate_sha256;
use super::format::verify_digest;
use anyhow::Context;
use mesh::payload::Protobuf;
use mesh::payload::Timestamp;
use sha2::Digest;
use std::collections::HashSet;
use std::path::Path;

/// Linux-direct MP-table boot layout with shared interrupt status.
pub const MICROVM_BOOT_LAYOUT_VERSION: u32 = 2;
/// Snapshot contract name for shared-status edge interrupts.
pub const MICROVM_SHARED_STATUS_INTERRUPT_MODE: &str = "edge-shared-status";
/// Clock policy applied when a snapshot is restored.
pub const ADVANCE_BY_HOST_DOWNTIME: &str = "advance_by_host_downtime";
const MAX_MEMORY_RANGES: usize = 128;
const MAX_DEVICES: usize = 64;
const MAX_DEVICE_RANGES: usize = 16;
const MAX_STATE_UNITS: usize = 512;
const MAX_ATTACHMENTS: usize = 64;
const MAX_ATTACHMENT_IDENTITY_BYTES: usize = 4096;
const MAX_COMMAND_LINE_BYTES: usize = 64 * 1024;
const MAX_CPU_CONTRACT_BYTES: usize = 1024 * 1024;

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
}

impl SnapshotMicrovmFilesystem {
    fn new(
        config: &openvmm_defs::microvm::MicrovmFilesystemConfig,
        canonical_host_path: &str,
    ) -> Self {
        Self {
            guest_mount_target: config.guest_mount_target.clone(),
            access_mode: config.access.as_str().to_owned(),
            restore_mode: "live-revalidate".to_owned(),
            tag: "microvm".to_owned(),
            high_priority_queue_count: 1,
            request_queue_count: 1,
            shared_memory_size: 0,
            direct_io: true,
            entry_cache_timeout_ns: 0,
            attribute_cache_timeout_ns: 0,
            canonical_host_path: canonical_host_path.to_owned(),
        }
    }
}

impl SnapshotMicrovmNetwork {
    fn new(config: &openvmm_defs::microvm::MicrovmNetworkConfig) -> Self {
        Self {
            profile: config.profile.as_str().to_owned(),
            guest_ipv4: u32::from(config.guest_ipv4),
            prefix_length: u32::from(config.prefix_length),
            gateway_ipv4: u32::from(config.derived_gateway_ipv4),
            guest_mac: config.guest_mac.to_bytes().to_vec(),
            gateway_mac: config.gateway_mac.to_bytes().to_vec(),
        }
    }
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
    /// Host wall time at the stopped capture boundary.
    #[mesh(11)]
    pub capture_wall_clock: Timestamp,
    /// Effective guest TSC frequency.
    #[mesh(12)]
    pub tsc_frequency_hz: u64,
    /// Accepted destination TSC frequency tolerance in parts per million.
    #[mesh(13)]
    pub tsc_tolerance_ppm: u32,
    /// Canonical protobuf-encoded effective CPU contract.
    #[mesh(14)]
    pub cpu_contract: Vec<u8>,
    /// SHA-256 of the canonical CPU contract.
    #[mesh(15)]
    pub cpu_contract_sha256: Vec<u8>,
    /// Version of the fixed cold-boot and memory layout.
    #[mesh(16)]
    pub boot_layout_version: u32,
    /// Policy used to advance clocks and deadlines over host downtime.
    #[mesh(17)]
    pub clock_policy: String,
    /// Static identity of the optional microVM virtio-net device.
    #[mesh(18)]
    pub microvm_network: Option<SnapshotMicrovmNetwork>,
    /// Guest-visible policy of the optional microVM virtio-fs device.
    #[mesh(19)]
    pub microvm_filesystem: Option<SnapshotMicrovmFilesystem>,
    /// Effective local APIC timer frequency.
    #[mesh(20)]
    pub apic_frequency_hz: Option<u64>,
    /// Virtio interrupt-delivery mode.
    #[mesh(24)]
    pub virtio_interrupt_mode: String,
    /// Guest-physical base of the shared interrupt-status page, or zero when absent.
    #[mesh(25)]
    pub virtio_shared_status_page_gpa: u64,
    /// Size of the shared interrupt-status page, or zero when absent.
    #[mesh(26)]
    pub virtio_shared_status_page_size: u64,
}

impl SnapshotMachineContract {
    /// Sets the effective command line and its digest together.
    pub fn set_effective_command_line(&mut self, command_line: String) {
        self.effective_command_line_sha256 = sha2::Sha256::digest(command_line.as_bytes()).to_vec();
        self.effective_command_line = command_line;
    }

    /// Sets the canonical CPU compatibility contract and its digest together.
    pub fn set_cpu_compatibility_contract(&mut self, cpu_contract: Vec<u8>) {
        self.cpu_contract_sha256 = sha2::Sha256::digest(&cpu_contract).to_vec();
        self.cpu_contract = cpu_contract;
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

/// Builds the authoritative microVM machine contract.
pub fn microvm_machine_contract(
    source_hypervisor: &str,
    boot_layout_version: u32,
    effective_command_line: String,
    network: Option<(
        &openvmm_defs::microvm::MicrovmNetworkConfig,
        SnapshotAttachment,
    )>,
    filesystem: Option<(
        &openvmm_defs::microvm::MicrovmFilesystemConfig,
        &Path,
        SnapshotAttachment,
    )>,
    console_attachment: Option<SnapshotAttachment>,
    processor_count: u32,
    memory_size: u64,
    state_unit_names: Vec<String>,
    capture_wall_clock: Timestamp,
    tsc_frequency_hz: u64,
    apic_frequency_hz: Option<u64>,
    cpu_contract: Vec<u8>,
) -> anyhow::Result<SnapshotMachineContract> {
    anyhow::ensure!(
        matches!(source_hypervisor, "kvm" | "mshv" | "whp"),
        "microVM snapshots require the KVM, MSHV, or WHP hypervisor"
    );
    let topology = microvm_snapshot_topology(processor_count)?;
    let memory_ranges = canonical_microvm_memory_ranges(memory_size)?;

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
    let microvm_network = if let Some((network, attachment)) = network {
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
        Some(SnapshotMicrovmNetwork::new(network))
    } else {
        None
    };
    if filesystem.is_some() {
        let discovery = format!(
            "virtio_mmio.device={:#x}@{:#x}:{}",
            openvmm_defs::microvm::MICROVM_VIRTIO_MMIO_LEN,
            openvmm_defs::microvm::MICROVM_VIRTIO_FS_MMIO_BASE,
            openvmm_defs::microvm::MICROVM_VIRTIO_FS_IRQ,
        );
        anyhow::ensure!(
            effective_command_line
                .split_ascii_whitespace()
                .any(|token| token == discovery),
            "microVM virtio-fs slot is missing from the effective command line"
        );
        devices.push(SnapshotDevice {
            stable_id: "fs:microvm0".to_owned(),
            state_unit_name: format!(
                "virtiofs-{}",
                openvmm_defs::microvm::MICROVM_VIRTIO_FS_MMIO_BASE
            ),
            kind: "virtio-fs".to_owned(),
            order: devices.len() as u32,
            ranges: vec![mmio(
                openvmm_defs::microvm::MICROVM_VIRTIO_FS_MMIO_BASE,
                openvmm_defs::microvm::MICROVM_VIRTIO_MMIO_LEN,
            )],
            irq: Some(openvmm_defs::microvm::MICROVM_VIRTIO_FS_IRQ),
            transport: "virtio-mmio".to_owned(),
            feature_banks: vec![
                openvmm_defs::microvm::MICROVM_VIRTIO_FS_FEATURES as u32,
                (openvmm_defs::microvm::MICROVM_VIRTIO_FS_FEATURES >> 32) as u32,
            ],
            queue_count: 2,
            queue_max_sizes: vec![256, 256],
        });
    }
    let microvm_filesystem = if let Some((filesystem, canonical_host_path, attachment)) = filesystem
    {
        let canonical_host_path = canonical_host_path
            .to_str()
            .context("microVM filesystem canonical host path is not valid UTF-8")?;
        anyhow::ensure!(
            !canonical_host_path.is_empty(),
            "microVM filesystem canonical host path is empty"
        );
        anyhow::ensure!(
            attachment.stable_id == "fs:microvm0"
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
        let tokens = effective_command_line
            .split_ascii_whitespace()
            .collect::<HashSet<_>>();
        anyhow::ensure!(
            filesystem
                .command_line_fragment()
                .split_ascii_whitespace()
                .all(|token| tokens.contains(token)),
            "microVM filesystem command line does not match its saved policy"
        );
        attachments.push(attachment);
        Some(SnapshotMicrovmFilesystem::new(
            filesystem,
            canonical_host_path,
        ))
    } else {
        None
    };
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
        capture_wall_clock,
        tsc_frequency_hz,
        tsc_tolerance_ppm: 0,
        cpu_contract: Vec::new(),
        cpu_contract_sha256: Vec::new(),
        boot_layout_version,
        clock_policy: ADVANCE_BY_HOST_DOWNTIME.to_owned(),
        microvm_network,
        microvm_filesystem,
        apic_frequency_hz,
        virtio_interrupt_mode: MICROVM_SHARED_STATUS_INTERRUPT_MODE.to_owned(),
        virtio_shared_status_page_gpa: openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_GPA,
        virtio_shared_status_page_size: openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_SIZE,
    };
    contract.set_effective_command_line(effective_command_line);
    contract.set_cpu_compatibility_contract(cpu_contract);
    validate_machine_contract_shape(&contract, memory_size, processor_count)?;
    Ok(contract)
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
        contract.clock_policy == expected.clock_policy,
        "snapshot clock policy doesn't match the requested machine"
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
        contract.topology == expected.topology,
        "snapshot processor topology doesn't match the requested machine"
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
        contract.microvm_filesystem == expected.microvm_filesystem,
        "snapshot filesystem policy doesn't match the requested machine"
    );
    anyhow::ensure!(
        contract.tsc_frequency_hz == expected.tsc_frequency_hz
            && contract.tsc_tolerance_ppm == expected.tsc_tolerance_ppm,
        "snapshot TSC frequency contract doesn't match the destination"
    );
    anyhow::ensure!(
        contract.apic_frequency_hz == expected.apic_frequency_hz,
        "snapshot APIC frequency contract doesn't match the destination"
    );
    anyhow::ensure!(
        contract.cpu_contract == expected.cpu_contract,
        "snapshot CPU compatibility contract doesn't match the destination"
    );
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
        contract.clock_policy == ADVANCE_BY_HOST_DOWNTIME,
        "snapshot clock policy '{}' is unsupported",
        contract.clock_policy
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
    anyhow::ensure!(
        contract.tsc_frequency_hz != 0,
        "snapshot TSC frequency must be nonzero"
    );
    if let Some(apic_frequency_hz) = contract.apic_frequency_hz {
        anyhow::ensure!(
            apic_frequency_hz != 0,
            "snapshot APIC frequency must be nonzero"
        );
    }
    anyhow::ensure!(
        !contract.cpu_contract.is_empty() && contract.cpu_contract.len() <= MAX_CPU_CONTRACT_BYTES,
        "snapshot CPU contract size is invalid"
    );
    validate_sha256(&contract.cpu_contract_sha256, "CPU contract")?;
    verify_digest(
        &contract.cpu_contract,
        &contract.cpu_contract_sha256,
        "CPU contract",
    )?;
    let _: std::time::SystemTime = contract
        .capture_wall_clock
        .try_into()
        .context("snapshot capture wall clock is invalid")?;

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
    }

    if let Some(filesystem) = &contract.microvm_filesystem {
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
        .context("snapshot filesystem policy is invalid")?;
        anyhow::ensure!(
            *filesystem == SnapshotMicrovmFilesystem::new(&parsed, &filesystem.canonical_host_path),
            "snapshot filesystem policy is not canonical"
        );
    }

    let has_filesystem_device = contract
        .devices
        .iter()
        .any(|device| device.stable_id == "fs:microvm0");
    let has_filesystem_attachment = contract
        .attachments
        .iter()
        .any(|attachment| attachment.stable_id == "fs:microvm0");
    anyhow::ensure!(
        has_filesystem_device == contract.microvm_filesystem.is_some()
            && has_filesystem_attachment == contract.microvm_filesystem.is_some(),
        "snapshot microVM filesystem device, policy, and attachment inventories disagree"
    );

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
fn test_machine_contract() -> SnapshotMachineContract {
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
        capture_wall_clock: std::time::SystemTime::now().into(),
        tsc_frequency_hz: 1_000_000_000,
        tsc_tolerance_ppm: 0,
        cpu_contract: Vec::new(),
        cpu_contract_sha256: Vec::new(),
        boot_layout_version: MICROVM_BOOT_LAYOUT_VERSION,
        clock_policy: ADVANCE_BY_HOST_DOWNTIME.to_owned(),
        microvm_network: None,
        microvm_filesystem: None,
        apic_frequency_hz: Some(1_000_000_000),
        virtio_interrupt_mode: MICROVM_SHARED_STATUS_INTERRUPT_MODE.to_owned(),
        virtio_shared_status_page_gpa: openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_GPA,
        virtio_shared_status_page_size: openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_SIZE,
    };
    contract.set_effective_command_line("console=hvc0".to_owned());
    contract.set_cpu_compatibility_contract(vec![1, 2, 3]);
    contract
}

#[cfg(test)]
mod tests {
    use super::super::tests::test_manifest;
    use super::*;

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

    fn generated_network_contract(source_hypervisor: &str) -> SnapshotMachineContract {
        let network: openvmm_defs::microvm::MicrovmNetworkConfig = "10.0.0.2/24".parse().unwrap();
        let irq = openvmm_defs::microvm::microvm_virtio_net_irq(Some(source_hypervisor)).unwrap();
        let command_line = format!(
            "earlycon=xe9 console=hvc0 reboot=t panic=-1 virtio_mmio.device=0x1000@0xd0000000:{irq} {}",
            network.command_line_fragment_with_dns(true)
        );
        microvm_machine_contract(
            source_hypervisor,
            MICROVM_BOOT_LAYOUT_VERSION,
            command_line,
            Some((&network, microvm_network_attachment(source_hypervisor))),
            None,
            None,
            1,
            1024,
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
            std::time::SystemTime::now().into(),
            1_000_000_000,
            Some(1_000_000_000),
            vec![1, 2, 3],
        )
        .unwrap()
    }

    fn microvm_filesystem_attachment(source_hypervisor: &str) -> SnapshotAttachment {
        SnapshotAttachment {
            stable_id: "fs:microvm0".to_owned(),
            kind: "virtio-fs".to_owned(),
            required: true,
            reconnect_policy: "live-revalidate".to_owned(),
            identity_kind: match source_hypervisor {
                "kvm" | "mshv" => "unix-device-inode-v1",
                "whp" => "windows-volume-file-id-v1",
                _ => unreachable!(),
            }
            .to_owned(),
            identity: b"root-object-v1".to_vec(),
            length: 0,
            reconnect_timeout_ms: 0,
        }
    }

    fn generated_filesystem_contract(source_hypervisor: &str) -> SnapshotMachineContract {
        let filesystem = openvmm_defs::microvm::MicrovmFilesystemConfig::new(
            "/mnt/share".to_owned(),
            openvmm_defs::microvm::MicrovmFilesystemAccess::ReadOnly,
        )
        .unwrap();
        let command_line = format!(
            "earlycon=xe9 console=hvc0 reboot=t panic=-1 virtio_mmio.device=0x1000@0xd0001000:6 {}",
            filesystem.command_line_fragment()
        );
        microvm_machine_contract(
            source_hypervisor,
            MICROVM_BOOT_LAYOUT_VERSION,
            command_line,
            None,
            Some((
                &filesystem,
                Path::new(if cfg!(windows) {
                    r"C:\microvm-share"
                } else {
                    "/microvm-share"
                }),
                microvm_filesystem_attachment(source_hypervisor),
            )),
            None,
            1,
            1024,
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
            std::time::SystemTime::now().into(),
            1_000_000_000,
            Some(1_000_000_000),
            vec![1, 2, 3],
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
            None,
            Some(microvm_console_attachment()),
            1,
            1024,
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
            std::time::SystemTime::now().into(),
            1_000_000_000,
            Some(1_000_000_000),
            vec![1, 2, 3],
        )
        .unwrap()
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
        }
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
    fn validate_microvm_machine_contract_rejects_cpu_contract() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.set_cpu_compatibility_contract(vec![9, 9, 9]);
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("CPU compatibility"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_tsc_frequency() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.tsc_frequency_hz += 1;
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("TSC frequency"));
    }

    #[test]
    fn validate_microvm_machine_contract_rejects_apic_frequency() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.apic_frequency_hz = expected.apic_frequency_hz.map(|frequency| frequency + 1);
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("APIC frequency"));
    }

    #[test]
    fn validate_microvm_machine_contract_accepts_legacy_apic_frequency() {
        let mut manifest = test_manifest();
        let mut contract = test_machine_contract();
        contract.apic_frequency_hz = None;
        manifest.machine_contract = Some(contract.clone());

        validate_microvm_machine_contract(&manifest, &contract).unwrap();
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
    fn validate_microvm_machine_contract_rejects_clock_policy() {
        let mut manifest = test_manifest();
        let contract = test_machine_contract();
        manifest.machine_contract = Some(contract.clone());
        let mut expected = contract;
        expected.clock_policy = "freeze_during_downtime".to_owned();
        let err = validate_microvm_machine_contract(&manifest, &expected).unwrap_err();
        assert!(err.to_string().contains("clock policy"));
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
}
