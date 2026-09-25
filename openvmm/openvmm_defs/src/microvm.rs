// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM machine profile definitions.

use crate::config::ArchTopologyConfig;
use crate::config::Config;
use crate::config::LinuxDirectBootMode;
use crate::config::LinuxIsolationConfig;
use crate::config::LoadMode;
use crate::config::SmbiosBiosOverrides;
use crate::config::SmbiosConfig;
use crate::config::SmbiosSystemOverrides;
use crate::config::VirtioBus;
use crate::config::X2ApicConfig;
use crate::config::X86TopologyConfig;
use guid::Guid;
use mesh::MeshPayload;
use net_backend_resources::mac_address::MacAddress;
use std::fmt::Write as _;
use vmotherboard::options::BaseChipsetManifest;

/// The persisted microVM ABI version.
pub const MICROVM_ABI_VERSION_2: u32 = 2;

/// Returns whether a processor count is valid for the microVM.
pub const fn microvm_processor_count_supported(processor_count: u32) -> bool {
    matches!(processor_count, 1 | 2 | 4 | 8)
}

/// Command line owned by the microVM profile.
pub const MICROVM_BASE_COMMAND_LINE: &str = "earlycon=xe9 console=hvc0 reboot=t panic=-1";
/// Command line when the microVM virtio console is present.
pub const MICROVM_CONSOLE_COMMAND_LINE: &str = "earlycon=xe9 console=hvc1 reboot=t panic=-1";
/// Maximum microVM command-line size, including its trailing NUL.
pub const MICROVM_COMMAND_LINE_MAX_SIZE: usize = 64 * 1024;
/// Fixed distro virtio-blk MMIO base.
pub const MICROVM_VIRTIO_BLK_MMIO_BASE: u64 = 0xd000_3000;
/// Reserved microVM virtio-net MMIO base.
pub const MICROVM_VIRTIO_NET_MMIO_BASE: u64 = 0xd000_0000;
/// Reserved microVM virtio-fs MMIO base.
pub const MICROVM_VIRTIO_FS_MMIO_BASE: u64 = 0xd000_1000;
/// Reserved microVM virtio-console MMIO base.
pub const MICROVM_VIRTIO_CONSOLE_MMIO_BASE: u64 = 0xd000_2000;
/// Fixed microVM control virtio-console MMIO base.
pub const MICROVM_VIRTIO_CONTROL_CONSOLE_MMIO_BASE: u64 = 0xd000_7000;
/// Fixed microVM virtio transport window length.
pub const MICROVM_VIRTIO_MMIO_LEN: u64 = 0x1000;
/// Fixed distro virtio-blk interrupt.
pub const MICROVM_VIRTIO_BLK_IRQ: u32 = 4;
/// Fixed virtio-blk interrupt for the runtime lower layer.
///
/// IRQ 8 is exclusively owned by the microVM RTC.
pub const MICROVM_VIRTIO_RUNTIME_BLK_IRQ: u32 = 12;
/// Fixed virtio-blk interrupt for the custom lower layer.
pub const MICROVM_VIRTIO_CUSTOM_BLK_IRQ: u32 = 9;
/// Fixed virtio-blk interrupt for the writable scratch layer.
pub const MICROVM_VIRTIO_SCRATCH_BLK_IRQ: u32 = 11;
/// Fixed microVM virtio-console interrupt.
pub const MICROVM_VIRTIO_CONSOLE_IRQ: u32 = 7;
/// Fixed microVM control virtio-console interrupt.
pub const MICROVM_VIRTIO_CONTROL_CONSOLE_IRQ: u32 = 3;
/// Fixed microVM virtio-fs interrupt.
pub const MICROVM_VIRTIO_FS_IRQ: u32 = 6;
/// Fixed microVM virtio-net interrupt on KVM.
pub const MICROVM_VIRTIO_NET_KVM_IRQ: u32 = 10;
/// Fixed microVM virtio-net interrupt on WHP.
pub const MICROVM_VIRTIO_NET_WHP_IRQ: u32 = 5;
/// Exact microVM virtio-net feature mask: MAC and virtio version 1.
pub const MICROVM_VIRTIO_NET_FEATURES: u64 = (1 << 5) | (1 << 32);
/// Exact microVM virtio-fs feature mask: indirect descriptors, event index,
/// virtio version 1, and access-platform.
pub const MICROVM_VIRTIO_FS_FEATURES: u64 = (1 << 28) | (1 << 29) | (1 << 32) | (1 << 33);
/// MicroVM client console reconnect timeout.
pub const MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS: u64 = 5_000;
/// MicroVM virtio MMIO reservations in stable device order.
pub const MICROVM_VIRTIO_MMIO_BASES: [u64; 8] = [
    MICROVM_VIRTIO_NET_MMIO_BASE,
    MICROVM_VIRTIO_FS_MMIO_BASE,
    MICROVM_VIRTIO_CONSOLE_MMIO_BASE,
    MICROVM_VIRTIO_BLK_MMIO_BASE,
    0xd000_4000,
    0xd000_5000,
    0xd000_6000,
    MICROVM_VIRTIO_CONTROL_CONSOLE_MMIO_BASE,
];
/// Fixed sandbox virtio-blk MMIO slots in layer order.
pub const MICROVM_VIRTIO_SANDBOX_BLOCK_MMIO_BASES: [u64; 4] = [
    MICROVM_VIRTIO_BLK_MMIO_BASE,
    0xd000_4000,
    0xd000_5000,
    0xd000_6000,
];
/// Guest-physical base of the shared virtio interrupt-status page.
pub const MICROVM_SHARED_STATUS_PAGE_GPA: u64 = 0x3_0000;
/// Size of the shared virtio interrupt-status page.
pub const MICROVM_SHARED_STATUS_PAGE_SIZE: u64 = 0x1000;
/// Shared-status offset for virtio-net.
pub const MICROVM_VIRTIO_NET_STATUS_OFFSET: u64 = 0x00;
/// Shared-status offset for virtio-fs.
pub const MICROVM_VIRTIO_FS_STATUS_OFFSET: u64 = 0x04;
/// Shared-status offset for virtio-console.
pub const MICROVM_VIRTIO_CONSOLE_STATUS_OFFSET: u64 = 0x08;
/// Shared-status offset for the distro virtio-blk slot.
pub const MICROVM_VIRTIO_DISTRO_BLK_STATUS_OFFSET: u64 = 0x0c;
/// Shared-status offset for the runtime virtio-blk slot.
pub const MICROVM_VIRTIO_RUNTIME_BLK_STATUS_OFFSET: u64 = 0x10;
/// Shared-status offset for the custom virtio-blk slot.
pub const MICROVM_VIRTIO_CUSTOM_BLK_STATUS_OFFSET: u64 = 0x14;
/// Shared-status offset for the scratch virtio-blk slot.
pub const MICROVM_VIRTIO_SCRATCH_BLK_STATUS_OFFSET: u64 = 0x18;
/// Shared-status offset for the dedicated control virtio-console.
pub const MICROVM_VIRTIO_CONTROL_CONSOLE_STATUS_OFFSET: u64 = 0x1c;

/// Returns the shared interrupt-status word for a fixed virtio-mmio slot.
pub const fn microvm_virtio_status_gpa(mmio_base: u64) -> Option<u64> {
    let offset = match mmio_base {
        MICROVM_VIRTIO_NET_MMIO_BASE => MICROVM_VIRTIO_NET_STATUS_OFFSET,
        MICROVM_VIRTIO_FS_MMIO_BASE => MICROVM_VIRTIO_FS_STATUS_OFFSET,
        MICROVM_VIRTIO_CONSOLE_MMIO_BASE => MICROVM_VIRTIO_CONSOLE_STATUS_OFFSET,
        MICROVM_VIRTIO_BLK_MMIO_BASE => MICROVM_VIRTIO_DISTRO_BLK_STATUS_OFFSET,
        0xd000_4000 => MICROVM_VIRTIO_RUNTIME_BLK_STATUS_OFFSET,
        0xd000_5000 => MICROVM_VIRTIO_CUSTOM_BLK_STATUS_OFFSET,
        0xd000_6000 => MICROVM_VIRTIO_SCRATCH_BLK_STATUS_OFFSET,
        MICROVM_VIRTIO_CONTROL_CONSOLE_MMIO_BASE => MICROVM_VIRTIO_CONTROL_CONSOLE_STATUS_OFFSET,
        _ => return None,
    };
    Some(MICROVM_SHARED_STATUS_PAGE_GPA + offset)
}
/// The microVM publishes no level-triggered virtio IRQs in its MP table.
pub const MICROVM_LEVEL_TRIGGERED_IRQS: [u32; 0] = [];

/// The stable role of a microVM sandbox block device.
#[derive(MeshPayload, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MicrovmSandboxBlockRole {
    /// The lowest, widest-shared read-only layer.
    Distro,
    /// The read-only runtime layer above the distro layer.
    Runtime,
    /// The optional read-only customer layer above the runtime layer.
    Custom,
    /// The writable overlayfs upper and work directories.
    Scratch,
}

impl MicrovmSandboxBlockRole {
    /// Returns the canonical manifest and CLI name of this role.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Distro => "distro",
            Self::Runtime => "runtime",
            Self::Custom => "custom",
            Self::Scratch => "scratch",
        }
    }

    /// Returns the role's fixed virtio-mmio address.
    pub const fn mmio_base(self) -> u64 {
        MICROVM_VIRTIO_SANDBOX_BLOCK_MMIO_BASES[self.index()]
    }

    /// Returns the role's fixed interrupt.
    pub const fn irq(self) -> u32 {
        match self {
            Self::Distro => MICROVM_VIRTIO_BLK_IRQ,
            Self::Runtime => MICROVM_VIRTIO_RUNTIME_BLK_IRQ,
            Self::Custom => MICROVM_VIRTIO_CUSTOM_BLK_IRQ,
            Self::Scratch => MICROVM_VIRTIO_SCRATCH_BLK_IRQ,
        }
    }

    /// Returns whether the role must be read-only.
    pub const fn is_read_only(self) -> bool {
        !matches!(self, Self::Scratch)
    }

    const fn index(self) -> usize {
        match self {
            Self::Distro => 0,
            Self::Runtime => 1,
            Self::Custom => 2,
            Self::Scratch => 3,
        }
    }
}

/// Returns the fixed virtio-blk feature mask for a sandbox role.
pub const fn microvm_sandbox_block_features(role: MicrovmSandboxBlockRole) -> u64 {
    const RING_INDIRECT_DESC: u64 = 1 << 28;
    const RING_EVENT_IDX: u64 = 1 << 29;
    const VERSION_1: u64 = 1 << 32;
    const ACCESS_PLATFORM: u64 = 1 << 33;
    const BLK_SEG_MAX: u64 = 1 << 2;
    const BLK_READ_ONLY: u64 = 1 << 5;
    const BLK_SIZE: u64 = 1 << 6;
    const BLK_FLUSH: u64 = 1 << 9;
    const BLK_TOPOLOGY: u64 = 1 << 10;

    RING_INDIRECT_DESC
        | RING_EVENT_IDX
        | VERSION_1
        | ACCESS_PLATFORM
        | BLK_SEG_MAX
        | BLK_SIZE
        | BLK_FLUSH
        | BLK_TOPOLOGY
        | if role.is_read_only() {
            BLK_READ_ONLY
        } else {
            0
        }
}

/// The immutable role and access mode of a microVM sandbox block device.
#[derive(MeshPayload, Clone, Copy, Debug, PartialEq, Eq)]
pub struct MicrovmSandboxBlockConfig {
    /// The fixed guest-visible role and transport location.
    pub role: MicrovmSandboxBlockRole,
    /// Whether writes are rejected by the VMM.
    pub read_only: bool,
}

/// Static guest-visible network identity for the microVM NIC.
#[derive(MeshPayload, Clone, Debug, PartialEq, Eq)]
pub struct MicrovmNetworkConfig {
    /// Required cross-platform host-network implementation contract.
    pub profile: MicrovmNetworkProfile,
    pub guest_ipv4: std::net::Ipv4Addr,
    pub prefix_length: u8,
    pub derived_gateway_ipv4: std::net::Ipv4Addr,
    pub guest_mac: MacAddress,
    pub gateway_mac: MacAddress,
}

/// Required host-network implementation contract for a microVM NIC.
///
/// Profiles are explicit so snapshots never silently acquire different host
/// networking semantics on another supported hypervisor.
#[derive(MeshPayload, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicrovmNetworkProfile {
    /// User-mode Consomme NAT on every supported host backend.
    Portable,
}

impl MicrovmNetworkProfile {
    /// Returns the stable command-line and snapshot spelling of this profile.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Portable => "portable",
        }
    }
}

/// Access policy for the microVM host filesystem.
#[derive(MeshPayload, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicrovmFilesystemAccess {
    /// Reject guest mutations before invoking host filesystem operations.
    ReadOnly,
    /// Permit the common cross-platform mutation contract.
    ReadWrite,
}

impl MicrovmFilesystemAccess {
    /// Returns the command-line spelling of this policy.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "ro",
            Self::ReadWrite => "rw",
        }
    }

    /// Returns whether host filesystem mutations are allowed.
    pub fn is_read_only(self) -> bool {
        matches!(self, Self::ReadOnly)
    }
}

/// Guest-visible configuration for the microVM virtio-fs device.
#[derive(MeshPayload, Clone, Debug, PartialEq, Eq)]
pub struct MicrovmFilesystemConfig {
    /// Absolute guest path at which the initramfs mounts the filesystem.
    pub guest_mount_target: String,
    /// Snapshot-authoritative access policy.
    pub access: MicrovmFilesystemAccess,
}

impl MicrovmFilesystemConfig {
    /// Validates and constructs the microVM filesystem configuration.
    pub fn new(
        guest_mount_target: String,
        access: MicrovmFilesystemAccess,
    ) -> Result<Self, InvalidMicrovmFilesystemConfig> {
        if guest_mount_target.is_empty()
            || !guest_mount_target.starts_with('/')
            || guest_mount_target == "/"
            || guest_mount_target.len() > 4096
            || guest_mount_target.chars().any(|character| {
                character.is_whitespace() || matches!(character, '\0' | '\\' | '=')
            })
            || guest_mount_target
                .split('/')
                .skip(1)
                .any(|component| component.is_empty() || matches!(component, "." | ".."))
        {
            return Err(InvalidMicrovmFilesystemConfig::InvalidGuestTarget(
                guest_mount_target,
            ));
        }
        Ok(Self {
            guest_mount_target,
            access,
        })
    }

    /// Returns the pinned guest bootstrap command-line tokens.
    pub fn command_line_fragment(&self) -> String {
        format!(
            "virtfs_dir={} virtfs_tag=microvm virtfs_mode={}",
            self.guest_mount_target,
            self.access.as_str()
        )
    }
}

/// Error returned for an invalid microVM filesystem specification.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InvalidMicrovmFilesystemConfig {
    /// The guest mount target is not a canonical absolute Linux path.
    #[error(
        "invalid guest mount target '{0}': expected an absolute non-root Linux path without empty, dot, parent, whitespace, backslash, or '=' components"
    )]
    InvalidGuestTarget(String),
}

impl MicrovmNetworkConfig {
    /// Returns the subnet mask derived from `prefix_length`.
    pub fn netmask(&self) -> std::net::Ipv4Addr {
        std::net::Ipv4Addr::from(u32::MAX << (32 - self.prefix_length))
    }

    /// Returns the pinned NVX guest-bootstrap command-line tokens.
    pub fn command_line_fragment(&self) -> String {
        self.command_line_fragment_with_dns(false)
    }

    /// Returns the pinned bootstrap tokens, optionally including gateway DNS.
    pub fn command_line_fragment_with_dns(&self, gateway_dns: bool) -> String {
        let dns = if gateway_dns {
            format!(" virtnet_dns={}", self.derived_gateway_ipv4)
        } else {
            String::new()
        };
        format!(
            "virtnet_ip={} virtnet_mask={} virtnet_gw={}{}",
            self.guest_ipv4,
            self.netmask(),
            self.derived_gateway_ipv4,
            dns,
        )
    }

    fn derive_mac(address: std::net::Ipv4Addr) -> MacAddress {
        let [_, second, third, fourth] = address.octets();
        MacAddress::new([0x52, 0x54, 0x00, second, third, fourth])
    }
}

/// Error returned for an invalid microVM static network specification.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InvalidMicrovmNetworkConfig {
    #[error("expected <IPv4>/<prefix>, for example 10.0.0.2/24")]
    InvalidFormat,
    #[error("invalid IPv4 address '{0}'")]
    InvalidAddress(String),
    #[error("invalid IPv4 prefix '{0}'")]
    InvalidPrefix(String),
    #[error("IPv4 prefix /{0} is outside the supported range /1 through /30")]
    PrefixOutOfRange(u8),
    #[error("guest IPv4 address {0} is the subnet network address")]
    NetworkAddress(std::net::Ipv4Addr),
    #[error("guest IPv4 address {0} is the subnet broadcast address")]
    BroadcastAddress(std::net::Ipv4Addr),
    #[error("guest IPv4 address {0} collides with the derived gateway")]
    GatewayCollision(std::net::Ipv4Addr),
}

impl std::str::FromStr for MicrovmNetworkConfig {
    type Err = InvalidMicrovmNetworkConfig;

    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        let (address, prefix) = spec
            .split_once('/')
            .filter(|(_, prefix)| !prefix.contains('/'))
            .ok_or(InvalidMicrovmNetworkConfig::InvalidFormat)?;
        let guest_ipv4 = address
            .parse::<std::net::Ipv4Addr>()
            .map_err(|_| InvalidMicrovmNetworkConfig::InvalidAddress(address.to_owned()))?;
        let prefix_length = prefix
            .parse::<u8>()
            .map_err(|_| InvalidMicrovmNetworkConfig::InvalidPrefix(prefix.to_owned()))?;
        if !(1..=30).contains(&prefix_length) {
            return Err(InvalidMicrovmNetworkConfig::PrefixOutOfRange(prefix_length));
        }

        let mask = u32::MAX << (32 - prefix_length);
        let guest = u32::from(guest_ipv4);
        let network = guest & mask;
        let broadcast = network | !mask;
        if guest == network {
            return Err(InvalidMicrovmNetworkConfig::NetworkAddress(guest_ipv4));
        }
        if guest == broadcast {
            return Err(InvalidMicrovmNetworkConfig::BroadcastAddress(guest_ipv4));
        }

        let derived_gateway_ipv4 = std::net::Ipv4Addr::from(network + 1);
        if guest_ipv4 == derived_gateway_ipv4 {
            return Err(InvalidMicrovmNetworkConfig::GatewayCollision(guest_ipv4));
        }

        Ok(Self {
            profile: MicrovmNetworkProfile::Portable,
            guest_ipv4,
            prefix_length,
            derived_gateway_ipv4,
            guest_mac: Self::derive_mac(guest_ipv4),
            gateway_mac: Self::derive_mac(derived_gateway_ipv4),
        })
    }
}

/// Returns the pinned virtio-net IRQ for the selected microVM backend.
pub fn microvm_virtio_net_irq(hypervisor_id: Option<&str>) -> anyhow::Result<u32> {
    match hypervisor_id {
        Some("kvm" | "mshv") => Ok(MICROVM_VIRTIO_NET_KVM_IRQ),
        Some("whp") => Ok(MICROVM_VIRTIO_NET_WHP_IRQ),
        Some(other) => anyhow::bail!("microVM virtio-net does not support hypervisor '{other}'"),
        None if cfg!(target_os = "linux") => Ok(MICROVM_VIRTIO_NET_KVM_IRQ),
        None if cfg!(windows) => Ok(MICROVM_VIRTIO_NET_WHP_IRQ),
        None => {
            anyhow::bail!("microVM virtio-net requires an explicit KVM, MSHV, or WHP hypervisor")
        }
    }
}

fn validate_microvm_virtio_reservations() -> anyhow::Result<()> {
    let bases = &MICROVM_VIRTIO_MMIO_BASES;
    for (index, base) in bases.iter().copied().enumerate() {
        let end = base
            .checked_add(MICROVM_VIRTIO_MMIO_LEN)
            .ok_or_else(|| anyhow::anyhow!("microVM virtio MMIO reservation overflows"))?;
        anyhow::ensure!(
            base >= 0xc000_0000 && end <= 0x1_0000_0000,
            "microVM virtio MMIO reservation {index} is outside the fixed aperture"
        );
        if let Some(next) = bases.get(index + 1) {
            anyhow::ensure!(end <= *next, "microVM virtio MMIO reservations overlap");
        }
    }
    let mut previous_status_gpa = None;
    for base in bases {
        let status_gpa = microvm_virtio_status_gpa(*base).ok_or_else(|| {
            anyhow::anyhow!("microVM virtio MMIO slot {base:#x} has no shared-status word")
        })?;
        anyhow::ensure!(
            status_gpa.is_multiple_of(size_of::<u32>() as u64)
                && status_gpa >= MICROVM_SHARED_STATUS_PAGE_GPA
                && status_gpa + size_of::<u32>() as u64
                    <= MICROVM_SHARED_STATUS_PAGE_GPA + MICROVM_SHARED_STATUS_PAGE_SIZE,
            "microVM shared-status word for slot {base:#x} is outside the reserved page"
        );
        anyhow::ensure!(
            previous_status_gpa.is_none_or(|previous| previous < status_gpa),
            "microVM shared-status words are duplicated or out of order"
        );
        previous_status_gpa = Some(status_gpa);
    }
    Ok(())
}

fn validate_microvm_sandbox_blocks(blocks: &[MicrovmSandboxBlockConfig]) -> anyhow::Result<()> {
    anyhow::ensure!(
        blocks.len() <= MICROVM_VIRTIO_SANDBOX_BLOCK_MMIO_BASES.len(),
        "microVM permits at most three read-only layers and one writable scratch device"
    );
    for (index, block) in blocks.iter().enumerate() {
        anyhow::ensure!(
            block.read_only == block.role.is_read_only(),
            "microVM sandbox block role {:?} must be {}",
            block.role,
            if block.role.is_read_only() {
                "read-only"
            } else {
                "writable"
            }
        );
        if let Some(previous) = index.checked_sub(1).and_then(|index| blocks.get(index)) {
            anyhow::ensure!(
                previous.role < block.role,
                "microVM sandbox block roles must be unique and in fixed order"
            );
        }
    }
    if !blocks.is_empty() {
        anyhow::ensure!(
            blocks
                .last()
                .is_some_and(|block| block.role == MicrovmSandboxBlockRole::Scratch),
            "microVM sandbox block topology requires a writable scratch device"
        );
    }
    Ok(())
}

/// Appends sandbox virtio devices in fixed-address order.
pub fn append_microvm_virtio_discovery(
    cmdline: &mut String,
    network: Option<(&MicrovmNetworkConfig, u32, bool)>,
    filesystem_slot: bool,
    filesystem: Option<&MicrovmFilesystemConfig>,
    has_console: bool,
    blocks: &[MicrovmSandboxBlockConfig],
) -> anyhow::Result<()> {
    validate_microvm_sandbox_blocks(blocks)?;
    anyhow::ensure!(
        !cmdline
            .split_ascii_whitespace()
            .any(|token| token.starts_with("virtio_mmio.device=")),
        "microVM command line already contains virtio-mmio discovery"
    );
    anyhow::ensure!(
        filesystem.is_none() || filesystem_slot,
        "microVM filesystem policy requires the fixed virtio-fs slot"
    );

    use std::fmt::Write as _;
    if let Some((_, irq, _)) = network {
        anyhow::ensure!(
            matches!(irq, MICROVM_VIRTIO_NET_KVM_IRQ | MICROVM_VIRTIO_NET_WHP_IRQ),
            "microVM virtio-net IRQ {irq} is not part of the fixed machine contract"
        );
        write!(
            cmdline,
            " virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{MICROVM_VIRTIO_NET_MMIO_BASE:#x}:{irq}"
        )?;
    }
    if filesystem_slot {
        write!(
            cmdline,
            " virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{MICROVM_VIRTIO_FS_MMIO_BASE:#x}:{MICROVM_VIRTIO_FS_IRQ}"
        )?;
    }
    if has_console {
        write!(
            cmdline,
            " virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{MICROVM_VIRTIO_CONSOLE_MMIO_BASE:#x}:{MICROVM_VIRTIO_CONSOLE_IRQ}"
        )?;
    }
    for block in blocks {
        write!(
            cmdline,
            " virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{:#x}:{}",
            block.role.mmio_base(),
            block.role.irq()
        )?;
    }
    if let Some((network, _, gateway_dns)) = network {
        write!(
            cmdline,
            " {}",
            network.command_line_fragment_with_dns(gateway_dns)
        )?;
    }
    if let Some(filesystem) = filesystem {
        write!(cmdline, " {}", filesystem.command_line_fragment())?;
    }
    anyhow::ensure!(
        cmdline.len() < MICROVM_COMMAND_LINE_MAX_SIZE,
        "microVM kernel command line exceeds the 64-KiB ABI limit after device discovery"
    );
    Ok(())
}

fn validate_microvm_command_line(
    config: &Config,
    hypervisor_id: Option<&str>,
) -> anyhow::Result<()> {
    let MachineProfile::Microvm = config.machine_profile else {
        unreachable!("microVM command-line validation requires the microVM profile");
    };
    let cmdline = match &config.load_mode {
        LoadMode::Linux {
            cmdline,
            boot_mode: LinuxDirectBootMode::MpTable,
            ..
        } => cmdline,
        _ => anyhow::bail!("microVM requires Linux MP-table load mode"),
    };
    anyhow::ensure!(
        !cmdline.contains('\0'),
        "microVM command line contains an embedded NUL"
    );
    anyhow::ensure!(
        cmdline.len() < MICROVM_COMMAND_LINE_MAX_SIZE,
        "microVM command line exceeds the 64-KiB ABI limit"
    );

    let tokens = cmdline.split_ascii_whitespace().collect::<Vec<_>>();
    let has_console = config
        .virtio_devices
        .iter()
        .any(|(_, device)| device.id() == "virtio-console");
    let block_count = config
        .virtio_devices
        .iter()
        .filter(|(_, device)| device.id() == "virtio-blk")
        .count();
    let has_network = config
        .virtio_devices
        .iter()
        .any(|(_, device)| device.id() == "virtio-net");
    let has_filesystem = config
        .virtio_devices
        .iter()
        .any(|(_, device)| device.id() == "virtiofs");
    anyhow::ensure!(
        has_network == config.microvm.network.is_some(),
        "microVM virtio-net device and static network identity must be configured together"
    );
    anyhow::ensure!(
        config.microvm.filesystem.is_none() || has_filesystem,
        "microVM filesystem policy requires a virtio-fs device"
    );
    anyhow::ensure!(
        !config.microvm.filesystem_bootstrap || config.microvm.filesystem.is_some(),
        "microVM filesystem bootstrap requires an active filesystem policy"
    );
    validate_microvm_sandbox_blocks(&config.microvm.sandbox_blocks)?;
    anyhow::ensure!(
        block_count == config.microvm.sandbox_blocks.len(),
        "microVM sandbox block roles do not match the virtio-blk device inventory"
    );
    let base_tokens = if has_console {
        MICROVM_CONSOLE_COMMAND_LINE
    } else {
        MICROVM_BASE_COMMAND_LINE
    }
    .split_ascii_whitespace()
    .collect::<Vec<_>>();
    anyhow::ensure!(
        tokens.starts_with(&base_tokens),
        "microVM command line does not begin with the ABI base tokens"
    );
    for prefix in [
        "earlycon=",
        "console=",
        "virtio_mmio.device=",
        "virtnet_ip=",
        "virtnet_mask=",
        "virtnet_gw=",
        "virtfs_dir=",
        "virtfs_tag=",
        "virtfs_mode=",
    ] {
        let count = tokens
            .iter()
            .filter(|token| token.starts_with(prefix))
            .count();
        let expected = match prefix {
            "virtio_mmio.device=" => config.virtio_devices.len(),
            "virtnet_ip=" | "virtnet_mask=" | "virtnet_gw=" => usize::from(has_network),
            "virtfs_dir=" | "virtfs_tag=" | "virtfs_mode=" => {
                usize::from(config.microvm.filesystem_bootstrap)
            }
            _ => 1,
        };
        anyhow::ensure!(
            count == expected,
            "microVM command line has an invalid number of {prefix} tokens"
        );
    }
    let dns_tokens = tokens
        .iter()
        .filter(|token| token.starts_with("virtnet_dns="))
        .copied()
        .collect::<Vec<_>>();
    anyhow::ensure!(
        dns_tokens.len() <= 1,
        "microVM command line has an invalid number of virtnet_dns= tokens"
    );
    if let Some(dns) = dns_tokens.first() {
        let network = config
            .microvm
            .network
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("microVM DNS bootstrap requires virtio-net"))?;
        anyhow::ensure!(
            **dns == format!("virtnet_dns={}", network.derived_gateway_ipv4),
            "microVM DNS bootstrap does not match the portable gateway"
        );
    }
    let mut expected_discovery = Vec::new();
    if has_network {
        let irq = microvm_virtio_net_irq(hypervisor_id)?;
        expected_discovery.push(format!(
            "virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{MICROVM_VIRTIO_NET_MMIO_BASE:#x}:{irq}"
        ));
    }
    if has_filesystem {
        expected_discovery.push(format!(
            "virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{MICROVM_VIRTIO_FS_MMIO_BASE:#x}:{MICROVM_VIRTIO_FS_IRQ}"
        ));
    }
    if has_console {
        expected_discovery.push(format!(
            "virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{MICROVM_VIRTIO_CONSOLE_MMIO_BASE:#x}:{MICROVM_VIRTIO_CONSOLE_IRQ}"
        ));
    }
    expected_discovery.extend(config.microvm.sandbox_blocks.iter().map(|block| {
        format!(
            "virtio_mmio.device={MICROVM_VIRTIO_MMIO_LEN:#x}@{:#x}:{}",
            block.role.mmio_base(),
            block.role.irq()
        )
    }));
    if let Some(network) = &config.microvm.network {
        expected_discovery.extend(
            network
                .command_line_fragment_with_dns(!dns_tokens.is_empty())
                .split_ascii_whitespace()
                .map(str::to_owned),
        );
    }
    if config.microvm.filesystem_bootstrap {
        let filesystem = config
            .microvm
            .filesystem
            .as_ref()
            .expect("filesystem bootstrap policy was validated above");
        expected_discovery.extend(
            filesystem
                .command_line_fragment()
                .split_ascii_whitespace()
                .map(str::to_owned),
        );
    }
    if !expected_discovery.is_empty() {
        anyhow::ensure!(
            tokens.ends_with(
                &expected_discovery
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
            ),
            "microVM virtio discovery tokens are not in fixed-address order"
        );
    }
    Ok(())
}

/// Builds the microVM command line and rejects profile-owned user tokens.
pub fn build_microvm_command_line(
    user_args: &[String],
    has_console: bool,
) -> anyhow::Result<String> {
    for arg in user_args {
        if arg.contains('\0') {
            anyhow::bail!("microVM kernel command line contains an embedded NUL");
        }
        if arg.split_ascii_whitespace().any(|token| {
            [
                "earlycon=",
                "console=",
                "virtio_mmio.device=",
                "virtnet_ip=",
                "virtnet_mask=",
                "virtnet_gw=",
                "virtnet_dns=",
                "virtfs_dir=",
                "virtfs_tag=",
                "virtfs_mode=",
                "nvx_snapshot_tier=",
                "nr_cpus=",
            ]
            .iter()
            .any(|reserved| token.starts_with(reserved))
        }) {
            anyhow::bail!(
                "microVM kernel command line cannot override profile-owned configuration"
            );
        }
    }

    let mut cmdline = if has_console {
        MICROVM_CONSOLE_COMMAND_LINE
    } else {
        MICROVM_BASE_COMMAND_LINE
    }
    .to_owned();
    for arg in user_args.iter().filter(|arg| !arg.is_empty()) {
        cmdline.push(' ');
        cmdline.push_str(arg);
    }
    if cmdline.len() >= MICROVM_COMMAND_LINE_MAX_SIZE {
        anyhow::bail!("microVM kernel command line exceeds the 64-KiB ABI limit");
    }
    Ok(cmdline)
}

/// Appends the host-owned processor capacity to a microVM command line.
pub fn append_microvm_processor_limit(
    cmdline: &mut String,
    processor_count: u32,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        microvm_processor_count_supported(processor_count),
        "microVM does not support {processor_count} vCPUs"
    );
    write!(cmdline, " nr_cpus={processor_count}")?;
    anyhow::ensure!(
        cmdline.len() < MICROVM_COMMAND_LINE_MAX_SIZE,
        "microVM kernel command line exceeds the 64-KiB ABI limit"
    );
    Ok(())
}

/// The guest-visible machine contract, independent of the hypervisor backend.
#[derive(MeshPayload, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum MachineProfile {
    /// The standard OpenVMM machine.
    #[default]
    Standard,
    /// The microVM machine.
    Microvm,
}

/// VM configuration specific to the microVM machine profile
/// ([`MachineProfile::Microvm`]), held in [`Config::microvm`].
#[derive(MeshPayload, Debug, Default)]
pub struct MicrovmConfig {
    /// Static identity for the optional microVM virtio-net device.
    pub network: Option<MicrovmNetworkConfig>,
    /// Guest-visible policy for the optional microVM virtio-fs device.
    pub filesystem: Option<MicrovmFilesystemConfig>,
    /// Stable sandbox block-device roles in virtio-blk device order.
    pub sandbox_blocks: Vec<MicrovmSandboxBlockConfig>,
    /// Whether the effective command line bootstraps the active microVM filesystem.
    pub filesystem_bootstrap: bool,
}

fn validate_machine_load_mode(
    machine_profile: MachineProfile,
    load_mode: &LoadMode,
) -> anyhow::Result<()> {
    if machine_profile == MachineProfile::Standard {
        anyhow::ensure!(
            !matches!(
                load_mode,
                LoadMode::Linux {
                    boot_mode: LinuxDirectBootMode::MpTable,
                    ..
                }
            ),
            "Linux MP-table boot mode requires the microVM profile"
        );
        return Ok(());
    }

    match load_mode {
        LoadMode::Linux {
            enable_serial,
            isolation,
            boot_mode,
            smbios,
            ..
        } => {
            anyhow::ensure!(
                *boot_mode == LinuxDirectBootMode::MpTable,
                "microVM Linux direct boot requires MP tables"
            );
            anyhow::ensure!(
                *isolation == LinuxIsolationConfig::None,
                "microVM MP-table boot does not support isolation"
            );
            anyhow::ensure!(
                !enable_serial,
                "microVM MP-table boot does not support emulated serial"
            );
            let SmbiosConfig { bios, system } = &**smbios;
            let SmbiosBiosOverrides {
                vendor,
                version: bios_version,
                release_date,
                release,
            } = bios;
            let SmbiosSystemOverrides {
                manufacturer,
                product_name,
                version: system_version,
                serial_number,
                sku_number,
                family,
                uuid,
            } = system;
            anyhow::ensure!(
                vendor.is_none()
                    && bios_version.is_none()
                    && release_date.is_none()
                    && release.is_none()
                    && manufacturer.is_none()
                    && product_name.is_none()
                    && system_version.is_none()
                    && serial_number.is_none()
                    && sku_number.is_none()
                    && family.is_none()
                    && *uuid == Guid::ZERO,
                "microVM MP-table boot does not expose SMBIOS overrides"
            );
            Ok(())
        }
        _ => anyhow::bail!("microVM requires Linux MP-table load mode"),
    }
}

/// Validates the microVM machine contract. Standard-machine configurations are unchanged.
pub fn validate_machine_config(config: &Config, hypervisor_id: Option<&str>) -> anyhow::Result<()> {
    let MachineProfile::Microvm = config.machine_profile else {
        validate_machine_load_mode(config.machine_profile, &config.load_mode)?;
        anyhow::ensure!(
            config.microvm.network.is_none(),
            "static microVM network identity requires the microVM profile"
        );
        anyhow::ensure!(
            config.microvm.filesystem.is_none(),
            "microVM filesystem policy requires the microVM profile"
        );
        anyhow::ensure!(
            config.microvm.sandbox_blocks.is_empty(),
            "microVM sandbox block roles require the microVM profile"
        );
        return Ok(());
    };

    validate_microvm_virtio_reservations()?;
    validate_microvm_command_line(config, hypervisor_id)?;
    validate_machine_load_mode(config.machine_profile, &config.load_mode)?;
    if let Some(hypervisor_id) = hypervisor_id {
        anyhow::ensure!(
            matches!(hypervisor_id, "kvm" | "mshv" | "whp"),
            "microVM requires the KVM, MSHV, or WHP hypervisor"
        );
    }
    anyhow::ensure!(
        microvm_processor_count_supported(config.processor_topology.proc_count),
        "microVM does not support {} vCPUs",
        config.processor_topology.proc_count
    );
    let has_expected_topology = config.processor_topology.vps_per_socket
        == Some(config.processor_topology.proc_count)
        && config.processor_topology.enable_smt == Some(false)
        && matches!(
            &config.processor_topology.arch,
            Some(ArchTopologyConfig::X86(X86TopologyConfig {
                apic_id_offset: 0,
                x2apic: X2ApicConfig::Unsupported,
            }))
        );
    anyhow::ensure!(
        has_expected_topology,
        "microVM requires its fixed x86 APIC topology"
    );
    anyhow::ensure!(
        config.numa.nodes.len() == 1 && config.numa.distances.is_empty(),
        "microVM requires a single NUMA node"
    );
    anyhow::ensure!(config.numa.nodes[0].mem.is_some(), "microVM requires RAM");
    anyhow::ensure!(
        !config.hypervisor.with_hv
            && config.hypervisor.with_vtl2.is_none()
            && config.hypervisor.with_isolation.is_none()
            && !config.hypervisor.nested_virt,
        "microVM does not support Hyper-V enlightenments, VTL2, isolation, or nested virtualization"
    );

    let expected_chipset = BaseChipsetManifest {
        with_generic_cmos_rtc: true,
        ..BaseChipsetManifest::empty()
    };
    anyhow::ensure!(
        config.chipset == expected_chipset,
        "microVM chipset is not the microVM allowlist"
    );
    anyhow::ensure!(
        config.chipset_capabilities.with_ioapic
            && config.chipset_capabilities.with_pic
            && config.chipset_capabilities.with_pit
            && !config.chipset_capabilities.with_generic_isa_dma
            && !config.chipset_capabilities.with_psp
            && !config.chipset_capabilities.with_guest_watchdog
            && !config.chipset_capabilities.with_i440bx_host_pci_bridge,
        "microVM chipset capabilities do not match the fixed profile"
    );

    let mut chipset_ids = config
        .chipset_devices
        .iter()
        .map(|device| (device.name.as_str(), device.resource.id()))
        .collect::<Vec<_>>();
    chipset_ids.sort_unstable();
    anyhow::ensure!(
        chipset_ids
            == [
                ("ioapic", "generic-ioapic"),
                ("microvm-portb", "microvm-portb"),
                ("microvm-shutdown", "microvm-shutdown"),
                ("microvm-snapshot-request", "microvm-snapshot-request"),
                ("pic", "pic"),
                ("pit", "pit"),
            ],
        "microVM chipset-device inventory is not exact"
    );

    anyhow::ensure!(
        config.floppy_disks.is_empty() && config.ide_disks.is_empty(),
        "microVM does not support floppy or IDE devices"
    );
    anyhow::ensure!(
        config.pcie_root_complexes.is_empty()
            && config.pcie_devices.is_empty()
            && config.pcie_switches.is_empty()
            && config.pcie_generic_initiators.is_empty()
            && config.vpci_devices.is_empty()
            && config.pci_chipset_devices.is_empty()
            && config.isa_dma_controller.is_none(),
        "microVM does not support PCI, PCIe, VPCI, or ISA DMA"
    );
    anyhow::ensure!(
        config.vmbus.is_none() && config.vtl2_vmbus.is_none() && config.vmbus_devices.is_empty(),
        "microVM does not support VMBus"
    );
    anyhow::ensure!(
        config.framebuffer.is_none() && config.vga_firmware.is_none() && !config.vtl2_gfx,
        "microVM does not support graphics or VGA firmware"
    );
    anyhow::ensure!(config.vmgs.is_none(), "microVM does not support VMGS");
    anyhow::ensure!(
        config.firmware_event_send.is_none() && config.debugger_rpc.is_none(),
        "microVM does not support firmware or debugger resources"
    );
    anyhow::ensure!(
        config.rtc_delta_milliseconds == 0,
        "microVM RTC must be anchored directly to UTC"
    );
    #[cfg(windows)]
    anyhow::ensure!(
        config.kernel_vmnics.is_empty() && config.vpci_resources.is_empty(),
        "microVM does not support kernel NIC or VPCI resources"
    );

    anyhow::ensure!(
        config.virtio_devices.len() <= 8,
        "microVM has too many virtio devices"
    );
    let mut has_network = false;
    let mut has_filesystem = false;
    let mut has_console = false;
    let mut block_count = 0;
    for (bus, device) in &config.virtio_devices {
        anyhow::ensure!(
            *bus == VirtioBus::Mmio,
            "microVM permits only virtio-mmio devices"
        );
        match device.id() {
            "virtio-net" => anyhow::ensure!(
                !std::mem::replace(&mut has_network, true),
                "microVM permits only one virtio-net device"
            ),
            "virtiofs" => anyhow::ensure!(
                !std::mem::replace(&mut has_filesystem, true),
                "microVM permits only one virtio-fs device"
            ),
            "virtio-console" => anyhow::ensure!(
                !std::mem::replace(&mut has_console, true),
                "microVM permits only one virtio-console device"
            ),
            "virtio-blk" => block_count += 1,
            id => anyhow::bail!("microVM does not permit virtio device '{id}'"),
        }
    }
    validate_microvm_sandbox_blocks(&config.microvm.sandbox_blocks)?;
    anyhow::ensure!(
        block_count == config.microvm.sandbox_blocks.len(),
        "microVM sandbox block roles do not match the virtio-blk device inventory"
    );
    anyhow::ensure!(
        has_network == config.microvm.network.is_some(),
        "microVM virtio-net device and static network identity must be configured together"
    );
    anyhow::ensure!(
        config.microvm.filesystem.is_none() || has_filesystem,
        "microVM filesystem policy requires a virtio-fs device"
    );
    anyhow::ensure!(
        !config.microvm.filesystem_bootstrap || config.microvm.filesystem.is_some(),
        "microVM filesystem bootstrap requires an active filesystem policy"
    );
    anyhow::ensure!(
        config.layout.chipset_low_mmio_size == 1024 * 1024 * 1024
            && config.layout.chipset_high_mmio_size == 0
            && config.layout.vtl2_chipset_mmio_size == 0,
        "microVM requires the fixed 3-GiB/4-GiB RAM split"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn linux_load_mode(
        boot_mode: LinuxDirectBootMode,
        isolation: LinuxIsolationConfig,
        enable_serial: bool,
        smbios: SmbiosConfig,
    ) -> LoadMode {
        LoadMode::Linux {
            kernel: File::open(std::env::current_exe().unwrap()).unwrap(),
            initrd: None,
            cmdline: MICROVM_BASE_COMMAND_LINE.to_owned(),
            enable_serial,
            isolation,
            boot_mode,
            smbios: Box::new(smbios),
        }
    }

    #[test]
    fn validates_linux_direct_boot_mode_matrix() {
        validate_machine_load_mode(
            MachineProfile::Microvm,
            &linux_load_mode(
                LinuxDirectBootMode::MpTable,
                LinuxIsolationConfig::None,
                false,
                SmbiosConfig::default(),
            ),
        )
        .unwrap();
        validate_machine_load_mode(
            MachineProfile::Standard,
            &linux_load_mode(
                LinuxDirectBootMode::Acpi,
                LinuxIsolationConfig::None,
                true,
                SmbiosConfig::default(),
            ),
        )
        .unwrap();

        for boot_mode in [LinuxDirectBootMode::Acpi, LinuxDirectBootMode::DeviceTree] {
            assert!(
                validate_machine_load_mode(
                    MachineProfile::Microvm,
                    &linux_load_mode(
                        boot_mode,
                        LinuxIsolationConfig::None,
                        false,
                        SmbiosConfig::default(),
                    ),
                )
                .is_err()
            );
        }
        assert!(
            validate_machine_load_mode(
                MachineProfile::Standard,
                &linux_load_mode(
                    LinuxDirectBootMode::MpTable,
                    LinuxIsolationConfig::None,
                    false,
                    SmbiosConfig::default(),
                ),
            )
            .is_err()
        );
        assert!(
            validate_machine_load_mode(
                MachineProfile::Microvm,
                &linux_load_mode(
                    LinuxDirectBootMode::MpTable,
                    LinuxIsolationConfig::Snp {
                        restricted_injection: false,
                    },
                    false,
                    SmbiosConfig::default(),
                ),
            )
            .is_err()
        );
        assert!(
            validate_machine_load_mode(
                MachineProfile::Microvm,
                &linux_load_mode(
                    LinuxDirectBootMode::MpTable,
                    LinuxIsolationConfig::None,
                    true,
                    SmbiosConfig::default(),
                ),
            )
            .is_err()
        );
        let mut smbios = SmbiosConfig::default();
        smbios.system.product_name = Some("override".to_owned());
        assert!(
            validate_machine_load_mode(
                MachineProfile::Microvm,
                &linux_load_mode(
                    LinuxDirectBootMode::MpTable,
                    LinuxIsolationConfig::None,
                    false,
                    smbios,
                ),
            )
            .is_err()
        );
    }

    #[test]
    fn microvm_snapshot_tier_command_line_token_is_host_owned() {
        assert!(
            build_microvm_command_line(&["nvx_snapshot_tier=platform".to_owned()], false).is_err()
        );
    }

    #[test]
    fn microvm_processor_limit_is_host_owned() {
        let mut cmdline = build_microvm_command_line(&[], false).unwrap();
        append_microvm_processor_limit(&mut cmdline, 8).unwrap();
        assert_eq!(cmdline, format!("{MICROVM_BASE_COMMAND_LINE} nr_cpus=8"));
        assert!(append_microvm_processor_limit(&mut cmdline, 3).is_err());
        assert!(build_microvm_command_line(&["nr_cpus=1".to_owned()], false).is_err());
    }

    #[test]
    fn microvm_network_identity_has_portable_profile() {
        let network: MicrovmNetworkConfig = "10.0.0.2/24".parse().unwrap();
        assert_eq!(network.profile, MicrovmNetworkProfile::Portable);
        assert_eq!(network.profile.as_str(), "portable");
    }

    #[test]
    fn microvm_sandbox_block_slots_are_stable() {
        let blocks = [
            MicrovmSandboxBlockConfig {
                role: MicrovmSandboxBlockRole::Distro,
                read_only: true,
            },
            MicrovmSandboxBlockConfig {
                role: MicrovmSandboxBlockRole::Runtime,
                read_only: true,
            },
            MicrovmSandboxBlockConfig {
                role: MicrovmSandboxBlockRole::Custom,
                read_only: true,
            },
            MicrovmSandboxBlockConfig {
                role: MicrovmSandboxBlockRole::Scratch,
                read_only: false,
            },
        ];
        validate_microvm_sandbox_blocks(&blocks).unwrap();

        let mut cmdline = MICROVM_BASE_COMMAND_LINE.to_owned();
        append_microvm_virtio_discovery(&mut cmdline, None, false, None, false, &blocks).unwrap();
        assert_eq!(
            cmdline,
            format!(
                "{MICROVM_BASE_COMMAND_LINE} \
                 virtio_mmio.device=0x1000@0xd0003000:4 \
                virtio_mmio.device=0x1000@0xd0004000:12 \
                 virtio_mmio.device=0x1000@0xd0005000:9 \
                 virtio_mmio.device=0x1000@0xd0006000:11"
            )
        );
    }

    #[test]
    fn microvm_block_irqs_avoid_rtc_and_are_edge_triggered() {
        assert_eq!(MICROVM_VIRTIO_CONTROL_CONSOLE_IRQ, 3);
        assert_eq!(MICROVM_VIRTIO_RUNTIME_BLK_IRQ, 12);
        assert!(MICROVM_LEVEL_TRIGGERED_IRQS.is_empty());
    }

    #[test]
    fn microvm_shared_status_slots_are_stable() {
        let slots = [
            (MICROVM_VIRTIO_NET_MMIO_BASE, 0x3_0000),
            (MICROVM_VIRTIO_FS_MMIO_BASE, 0x3_0004),
            (MICROVM_VIRTIO_CONSOLE_MMIO_BASE, 0x3_0008),
            (MICROVM_VIRTIO_BLK_MMIO_BASE, 0x3_000c),
            (MICROVM_VIRTIO_SANDBOX_BLOCK_MMIO_BASES[1], 0x3_0010),
            (MICROVM_VIRTIO_SANDBOX_BLOCK_MMIO_BASES[2], 0x3_0014),
            (MICROVM_VIRTIO_SANDBOX_BLOCK_MMIO_BASES[3], 0x3_0018),
            (MICROVM_VIRTIO_CONTROL_CONSOLE_MMIO_BASE, 0x3_001c),
        ];
        for (mmio_base, expected_gpa) in slots {
            assert_eq!(microvm_virtio_status_gpa(mmio_base), Some(expected_gpa));
            assert_eq!(expected_gpa % size_of::<u32>() as u64, 0);
            assert!(
                expected_gpa < MICROVM_SHARED_STATUS_PAGE_GPA + MICROVM_SHARED_STATUS_PAGE_SIZE
            );
        }
        assert_eq!(microvm_virtio_status_gpa(0xd000_8000), None);
    }
    #[test]
    fn microvm_sandbox_block_validation_rejects_invalid_layouts() {
        assert!(
            validate_microvm_sandbox_blocks(&[MicrovmSandboxBlockConfig {
                role: MicrovmSandboxBlockRole::Scratch,
                read_only: true,
            }])
            .is_err()
        );
        assert!(
            validate_microvm_sandbox_blocks(&[
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Runtime,
                    read_only: true,
                },
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Distro,
                    read_only: true,
                },
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Scratch,
                    read_only: false,
                },
            ])
            .is_err()
        );
        assert!(
            validate_microvm_sandbox_blocks(&[
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Distro,
                    read_only: true,
                },
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Distro,
                    read_only: true,
                },
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Scratch,
                    read_only: false,
                },
            ])
            .is_err()
        );
        assert!(
            validate_microvm_sandbox_blocks(&[
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Distro,
                    read_only: true,
                },
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Runtime,
                    read_only: true,
                },
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Custom,
                    read_only: true,
                },
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Scratch,
                    read_only: false,
                },
                MicrovmSandboxBlockConfig {
                    role: MicrovmSandboxBlockRole::Scratch,
                    read_only: false,
                },
            ])
            .is_err()
        );
    }
}
