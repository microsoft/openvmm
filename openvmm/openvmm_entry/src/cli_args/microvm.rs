// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command-line options of the microVM machine profile.

use super::DiskCli;
use super::EndpointConfigCli;
use super::Options;
use super::SerialConfigCli;
use super::SmtConfigCli;
use anyhow::Context;
use clap::ValueEnum;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::X2ApicConfig;
use openvmm_defs::microvm::MachineProfile;
use openvmm_defs::microvm::MicrovmNetworkProfile;
use openvmm_defs::microvm::MicrovmSandboxBlockRole;
use std::path::PathBuf;
use std::str::FromStr;

/// Guest-visible machine profile.
#[derive(Debug, Copy, Clone, ValueEnum, PartialEq, Eq)]
pub enum MachineProfileCli {
    /// The standard OpenVMM machine.
    Standard,
    /// The microVM fixed-topology shared-status machine.
    Microvm,
}

/// Required host-network implementation contract for a microVM NIC.
#[derive(Debug, Copy, Clone, ValueEnum, PartialEq, Eq)]
pub enum MicrovmNetworkProfileCli {
    /// Use the cross-platform user-mode Consomme NAT implementation.
    Portable,
}

/// Default action for one direction of microVM network traffic.
#[derive(Debug, Copy, Clone, ValueEnum, PartialEq, Eq)]
pub enum MicrovmNetworkActionCli {
    /// Permit traffic in this direction.
    Allow,
    /// Deny traffic in this direction.
    Deny,
}

/// Fixed numeric identity for microVM workloads.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct MicrovmWorkloadIdentityCli {
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

impl FromStr for MicrovmWorkloadIdentityCli {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (uid, gid) = value
            .split_once(':')
            .filter(|(_, gid)| !gid.contains(':'))
            .context("expected <UID>:<GID>")?;
        let uid = uid.parse::<u32>().context("invalid workload UID")?;
        let gid = gid.parse::<u32>().context("invalid workload GID")?;
        anyhow::ensure!(uid != 0, "microVM workload UID must be nonzero");
        anyhow::ensure!(gid != 0, "microVM workload GID must be nonzero");
        Ok(Self { uid, gid })
    }
}

/// Guest workload lifecycle for a microVM.
#[derive(Debug, Copy, Clone, ValueEnum, PartialEq, Eq)]
pub enum MicrovmLifecycleCli {
    /// Start one workload and destroy the VM when it exits.
    OneShot,
    /// Keep the VM resident and accept multiple control-session workloads.
    Managed,
}

/// Host identity that performs the guest's operations on the microVM share.
#[derive(Debug, Copy, Clone, ValueEnum, PartialEq, Eq)]
pub enum MicrovmMountOwnerCli {
    /// Run every operation as OpenVMM, which owns the files the guest creates.
    Vmm,
    /// Run each operation as its guest caller's UID and GID, with root
    /// squashed to the owner of the export root (Linux only).
    Caller,
}

impl From<MicrovmMountOwnerCli> for openvmm_defs::microvm::MicrovmFilesystemOwner {
    fn from(owner: MicrovmMountOwnerCli) -> Self {
        match owner {
            MicrovmMountOwnerCli::Vmm => Self::Vmm,
            MicrovmMountOwnerCli::Caller => Self::Caller,
        }
    }
}

/// Protocol for a localhost-to-guest port forward.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum MicrovmLoopbackForwardProtocol {
    /// Forward a TCP listener.
    Tcp,
    /// Forward a UDP socket.
    Udp,
}

/// Explicit localhost port allowed to initiate traffic toward the guest.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MicrovmLoopbackForwardCli {
    pub(crate) protocol: MicrovmLoopbackForwardProtocol,
    pub(crate) host_port: u16,
    pub(crate) guest_port: u16,
}

impl FromStr for MicrovmLoopbackForwardCli {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let fields = value.split(':').collect::<Vec<_>>();
        let [protocol, host_port, guest_port] = fields.as_slice() else {
            anyhow::bail!("expected <tcp|udp>:<HOST-PORT>:<GUEST-PORT>");
        };
        let protocol = match *protocol {
            "tcp" => MicrovmLoopbackForwardProtocol::Tcp,
            "udp" => MicrovmLoopbackForwardProtocol::Udp,
            other => anyhow::bail!("invalid loopback-forward protocol '{other}'"),
        };
        let host_port = host_port
            .parse::<u16>()
            .context("invalid loopback-forward host port")?;
        let guest_port = guest_port
            .parse::<u16>()
            .context("invalid loopback-forward guest port")?;
        anyhow::ensure!(host_port != 0, "loopback-forward host port must be nonzero");
        anyhow::ensure!(
            guest_port != 0,
            "loopback-forward guest port must be nonzero"
        );
        Ok(Self {
            protocol,
            host_port,
            guest_port,
        })
    }
}

/// Capture tier for a microVM sandbox snapshot.
#[derive(Debug, Copy, Clone, ValueEnum, PartialEq, Eq)]
pub enum SnapshotTierCli {
    /// Fleet-wide clone point before image or sandbox configuration is consumed.
    Platform,
    /// Tenant-scoped reusable clone point at the workload handoff.
    WorkloadStart,
    /// Single-use continuation of one stopped instance.
    InstanceCheckpoint,
}

impl SnapshotTierCli {
    pub(crate) fn manifest_name(self) -> &'static str {
        match self {
            Self::Platform => openvmm_helpers::snapshot::format::SNAPSHOT_TIER_PLATFORM,
            Self::WorkloadStart => openvmm_helpers::snapshot::format::SNAPSHOT_TIER_WORKLOAD_START,
            Self::InstanceCheckpoint => {
                openvmm_helpers::snapshot::format::SNAPSHOT_TIER_INSTANCE_CHECKPOINT
            }
        }
    }

    pub(crate) fn restore_policy(self) -> &'static str {
        match self {
            Self::Platform | Self::WorkloadStart => {
                openvmm_helpers::snapshot::format::SNAPSHOT_RESTORE_POLICY_CLONE
            }
            Self::InstanceCheckpoint => {
                openvmm_helpers::snapshot::format::SNAPSHOT_RESTORE_POLICY_RESUME
            }
        }
    }

    pub(crate) fn requires_paired_scratch(self) -> bool {
        !matches!(self, Self::Platform)
    }
}

impl From<MicrovmNetworkProfileCli> for MicrovmNetworkProfile {
    fn from(value: MicrovmNetworkProfileCli) -> Self {
        match value {
            MicrovmNetworkProfileCli::Portable => Self::Portable,
        }
    }
}

impl From<MicrovmNetworkActionCli> for net_backend_resources::egress::EgressAction {
    fn from(value: MicrovmNetworkActionCli) -> Self {
        match value {
            MicrovmNetworkActionCli::Allow => Self::Allow,
            MicrovmNetworkActionCli::Deny => Self::Deny,
        }
    }
}

impl From<MachineProfileCli> for MachineProfile {
    fn from(value: MachineProfileCli) -> Self {
        match value {
            MachineProfileCli::Standard => Self::Standard,
            MachineProfileCli::Microvm => Self::Microvm,
        }
    }
}

/// Options of the microVM machine profile.
#[derive(clap::Args)]
pub struct MicrovmCli {
    /// Accepted for compatibility: every microVM restore exposes restore packet
    /// version 4, which carries fresh entropy, through the private portb restore
    /// channel.
    #[clap(long, requires = "restore_snapshot")]
    pub restore_entropy: bool,

    /// Bring this contiguous prefix of capacity VPs online before restore readiness.
    #[clap(long, value_name = "COUNT", requires = "restore_snapshot")]
    pub restore_processors: Option<u32>,

    /// Restore a capable microVM snapshot with this total guest RAM size.
    #[clap(long, value_name = "SIZE", requires = "restore_snapshot")]
    pub restore_memory: Option<vmm_cli::MemorySize>,

    /// Maximum time allowed for a microVM guest to complete post-restore repair.
    #[clap(long, value_name = "MILLISECONDS", default_value_t = 60000)]
    pub restore_gate_timeout_ms: u64,

    /// Capture a microVM snapshot to this directory when the guest writes PMIO 0x605.
    #[clap(long, value_name = "DIR", conflicts_with = "restore_snapshot")]
    pub snapshot_destination: Option<PathBuf>,

    /// Reserve this immutable total RAM capacity in a captured microVM snapshot.
    #[clap(long, value_name = "SIZE", requires = "snapshot_destination")]
    pub memory_capacity: Option<vmm_cli::MemorySize>,

    /// Sandbox capture tier. Required for microVM snapshot capture with sandbox blocks.
    #[clap(
        long,
        value_enum,
        value_name = "TIER",
        requires = "snapshot_destination"
    )]
    pub snapshot_tier: Option<SnapshotTierCli>,

    /// Maximum time allowed to quiesce the VM for a guest-requested snapshot.
    #[clap(long, value_name = "MILLISECONDS", default_value_t = 5000)]
    pub snapshot_quiesce_timeout_ms: u64,

    /// Attach a fixed-role microVM sandbox block device.
    ///
    /// The value is `<role>:<disk>`, where the roles are `distro`, `runtime`,
    /// `custom`, and `scratch`. Lower-layer roles must use `,ro`; `scratch`
    /// must be writable. The profile assigns each role a fixed virtio-mmio
    /// address and IRQ independent of option order.
    #[clap(long, value_name = "ROLE:DISK")]
    pub microvm_sandbox_block: Vec<MicrovmSandboxBlockCli>,

    /// Run guest workloads under this fixed non-root numeric identity.
    ///
    /// The identity is part of the initial-boot command line and cannot be
    /// replaced when restoring a snapshot.
    #[clap(long, value_name = "UID:GID")]
    pub microvm_workload_identity: Option<MicrovmWorkloadIdentityCli>,

    /// Select one-shot or managed microVM workload lifecycle.
    #[clap(long, value_enum, value_name = "MODE")]
    pub microvm_lifecycle: Option<MicrovmLifecycleCli>,

    /// Write one bounded local JSON outcome report after microVM teardown.
    #[clap(long, value_name = "PATH")]
    pub microvm_report: Option<PathBuf>,

    /// Required host-network implementation contract for microVM `--net`.
    #[clap(long, value_enum, value_name = "PROFILE")]
    pub network_profile: Option<MicrovmNetworkProfileCli>,

    /// Default action for connections initiated by the microVM guest.
    #[clap(long, value_enum, value_name = "ACTION")]
    pub network_egress: Option<MicrovmNetworkActionCli>,

    /// Default action for new connections initiated toward the microVM guest.
    ///
    /// The portable profile supports only `deny`: its NAT admits responses to
    /// guest-initiated flows but exposes no listener for new inbound connections.
    #[clap(long, value_enum, value_name = "ACTION")]
    pub network_ingress: Option<MicrovmNetworkActionCli>,

    /// Permit matching IPv4 destinations, optionally restricted by TCP/UDP port.
    #[clap(
        long = "network-egress-allow",
        value_name = "IPv4[/PREFIX][:tcp|udp:PORT]"
    )]
    pub network_egress_allow: Vec<net_backend_resources::egress::EgressRule>,

    /// Deny matching IPv4 destinations before evaluating allow rules.
    #[clap(
        long = "network-egress-deny",
        value_name = "IPv4[/PREFIX][:tcp|udp:PORT]"
    )]
    pub network_egress_deny: Vec<net_backend_resources::egress::EgressRule>,

    /// Deny host-loopback access, or allow it with explicit localhost forwards.
    ///
    /// Explicit `allow` without forwards is unsupported: portable NAT cannot
    /// provide generic bidirectional host-loopback connectivity.
    #[clap(long, value_enum, value_name = "ACTION")]
    pub host_loopback: Option<MicrovmNetworkActionCli>,

    /// Exact guest-gateway TCP endpoint retained when host loopback is denied.
    #[clap(long, value_name = "IPv4:TCP-PORT")]
    pub network_proxy: Option<net_backend_resources::egress::TcpEndpoint>,

    /// Forward one localhost TCP/UDP port into the guest.
    #[clap(long, value_name = "tcp|udp:HOST-PORT:GUEST-PORT")]
    pub host_loopback_forward: Vec<MicrovmLoopbackForwardCli>,

    /// Select a preconfigured Linux TAP for a microVM NIC.
    ///
    /// This is incompatible with the portable microVM network profile.
    #[clap(long, value_name = "NAME")]
    pub net_tap: Option<String>,

    /// Permit only these IPv4 destinations or CIDRs from the microVM guest.
    #[clap(long, value_name = "IPv4[/PREFIX]", conflicts_with_all = ["block_host", "allow_endpoint"])]
    pub allow_host: Vec<net_backend_resources::egress::Ipv4Cidr>,

    /// Permit IPv4 except for these destinations or CIDRs from the microVM guest.
    #[clap(long, value_name = "IPv4[/PREFIX]", conflicts_with_all = ["allow_host", "allow_endpoint"])]
    pub block_host: Vec<net_backend_resources::egress::Ipv4Cidr>,

    /// Permit only these exact IPv4 TCP destinations from the microVM guest.
    #[clap(long, value_name = "IPv4:TCP-PORT", conflicts_with_all = ["allow_host", "block_host"])]
    pub allow_endpoint: Vec<net_backend_resources::egress::TcpEndpoint>,

    /// attach a host directory to the next fixed microVM virtio-fs slot
    ///
    /// Repeat once to attach a second directory. The first directory uses the
    /// `microvm` tag and the second the `microvm1` tag. Guest targets and host
    /// directories must not overlap. An active snapshot requires the same
    /// canonical host paths, guest targets, and modes, in the same order. A
    /// dormant-slot snapshot may bind one new attachment on restore; the
    /// resumed guest must mount the `microvm` tag explicitly.
    #[clap(
        long = "mount",
        value_name = "GUEST_TARGET,HOST_PATH[,ro|rw]",
        conflicts_with_all = ["virtio_fs", "virtio_fs_shmem"]
    )]
    pub microvm_mount: Vec<MicrovmMountCli>,

    /// Hide an existing host path inside a microVM filesystem export.
    ///
    /// The path is hidden in the `--mount` whose host directory contains it.
    /// A relative path is relative to the host directory of the only
    /// `--mount`; with several, the path must be absolute.
    #[clap(
        long = "mount-deny",
        value_name = "HOST_PATH",
        requires = "microvm_mount"
    )]
    pub microvm_mount_deny: Vec<PathBuf>,

    /// Select the host identity of the guest's `--mount` operations
    ///
    /// `vmm` (the default) performs every operation as OpenVMM. `caller`
    /// performs each operation as the guest caller's UID and GID, without
    /// supplementary groups or capabilities, and squashes guest UID 0 and
    /// GID 0 to the owner of the export root, which must not be root.
    /// `caller` requires a Linux host. It also requires CAP_SETUID and
    /// CAP_SETGID unless every caller has OpenVMM's own UID and GID and
    /// OpenVMM has no other supplementary groups; an operation that cannot
    /// run as its caller fails with EPERM. The mode applies to every
    /// `--mount`.
    #[clap(
        long = "mount-owner",
        value_enum,
        value_name = "OWNER",
        requires = "microvm_mount"
    )]
    pub microvm_mount_owner: Option<MicrovmMountOwnerCli>,

    /// dedicated microVM control console backed by a local serial endpoint
    ///
    /// Accepts listen=\<path\> or none. The boot
    /// virtio-console is required and remains the only kernel console.
    #[clap(long, value_name = "SERIAL")]
    pub microvm_control_console: Option<SerialConfigCli>,

    /// read the control-console capability from a prepared stdin pipe
    #[clap(
        long,
        hide = true,
        requires("microvm_control_console"),
        conflicts_with_all = ["rpc", "ttrpc", "grpc", "relay_console_path", "write_saved_state_proto", "cpu_fingerprint", "paused"]
    )]
    pub microvm_control_auth_stdin: bool,

    /// maximum time for a control-console client to authenticate
    #[clap(
        long = "microvm-control-auth-timeout-ms",
        value_name = "MILLISECONDS",
        default_value_t = 5000,
        hide = true
    )]
    pub microvm_control_auth_timeout_ms: u64,

    /// microVM CPU profile: a pinned profile ID, `auto` (the default) to
    /// select the host's profile, or `host` to derive a development profile
    /// from this host; a restore must name the snapshot's profile
    #[clap(long = "cpu-profile", value_name = "ID", hide = true)]
    pub cpu_profile: Option<String>,

    /// time ABI test hook (repeatable; testing only)
    #[clap(long = "x-time-abi-test-hook", value_name = "HOOK", hide = true)]
    pub x_time_abi_test_hook: Vec<String>,

    /// build the partition and run the time ABI preflight without running
    /// the guest, print one `NVX-TIME-ABI-VERIFY:` line, and exit with status
    /// 0 or 1 (host qualification)
    #[clap(
        long = "x-time-abi-verify",
        hide = true,
        conflicts_with_all = ["restore_snapshot", "snapshot_destination"]
    )]
    pub x_time_abi_verify: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MicrovmMountCli {
    /// Absolute guest mount target.
    pub guest_target: String,
    /// Live host directory supplied for this run.
    pub host_path: PathBuf,
    /// Snapshot-authoritative access policy.
    pub access: openvmm_defs::microvm::MicrovmFilesystemAccess,
}

impl FromStr for MicrovmMountCli {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut fields = value.splitn(3, ',');
        let guest_target = fields
            .next()
            .filter(|value| !value.is_empty())
            .context("expected <guest-target>,<host-path>[,ro|rw]")?;
        let host_path = fields
            .next()
            .filter(|value| !value.is_empty())
            .context("expected <guest-target>,<host-path>[,ro|rw]")?;
        let access = match fields.next().unwrap_or("ro") {
            "ro" => openvmm_defs::microvm::MicrovmFilesystemAccess::ReadOnly,
            "rw" => openvmm_defs::microvm::MicrovmFilesystemAccess::ReadWrite,
            mode => anyhow::bail!("invalid microVM mount mode '{mode}'; expected ro or rw"),
        };
        openvmm_defs::microvm::MicrovmFilesystemConfig::new(guest_target.to_owned(), access)?;
        Ok(Self {
            guest_target: guest_target.to_owned(),
            host_path: PathBuf::from(host_path),
            access,
        })
    }
}

/// A fixed-role microVM sandbox block-device CLI argument.
#[derive(Clone)]
pub struct MicrovmSandboxBlockCli {
    /// The stable guest-visible role.
    pub role: MicrovmSandboxBlockRole,
    /// The generic disk backend and access mode.
    pub disk: DiskCli,
}

impl FromStr for MicrovmSandboxBlockCli {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> anyhow::Result<Self> {
        let (role, disk) = value
            .split_once(':')
            .context("expected ROLE:DISK for --microvm-sandbox-block")?;
        let role = match role {
            "distro" => MicrovmSandboxBlockRole::Distro,
            "runtime" => MicrovmSandboxBlockRole::Runtime,
            "custom" => MicrovmSandboxBlockRole::Custom,
            "scratch" => MicrovmSandboxBlockRole::Scratch,
            _ => anyhow::bail!(
                "unknown microVM sandbox block role '{role}'; expected distro, runtime, custom, or scratch"
            ),
        };
        Ok(Self {
            role,
            disk: disk.parse()?,
        })
    }
}

/// Parses a bare `<IPv4>/<prefix>` `--net` endpoint into the microVM network
/// configuration.
pub(super) fn parse_endpoint(network: &str) -> Result<EndpointConfigCli, String> {
    network
        .parse()
        .map(EndpointConfigCli::Microvm)
        .map_err(|error| format!("invalid microVM network: {error}"))
}

impl Options {
    /// Rejects unsupported microVM combinations before opening host resources.
    pub(crate) fn validate_microvm_host_loopback(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.machine != MachineProfileCli::Microvm
                || self.microvm.host_loopback != Some(MicrovmNetworkActionCli::Allow)
                || !self.microvm.host_loopback_forward.is_empty(),
            "the portable microVM network profile does not support generic host-loopback connectivity; explicit --host-loopback allow requires --host-loopback-forward for deliberate port publishing"
        );
        Ok(())
    }

    pub(crate) fn validate_microvm_options(&self) -> anyhow::Result<()> {
        self.validate_microvm_host_loopback()?;
        if self.machine != MachineProfileCli::Microvm {
            anyhow::ensure!(
                self.microvm.net_tap.is_none()
                    && self.microvm.network_profile.is_none()
                    && self.microvm.network_egress.is_none()
                    && self.microvm.network_ingress.is_none()
                    && self.microvm.network_egress_allow.is_empty()
                    && self.microvm.network_egress_deny.is_empty()
                    && self.microvm.host_loopback.is_none()
                    && self.microvm.network_proxy.is_none()
                    && self.microvm.host_loopback_forward.is_empty()
                    && self.microvm.allow_host.is_empty()
                    && self.microvm.block_host.is_empty()
                    && self.microvm.allow_endpoint.is_empty()
                    && self.microvm.microvm_mount.is_empty()
                    && self.microvm.microvm_mount_deny.is_empty()
                    && self.microvm.microvm_sandbox_block.is_empty()
                    && self.microvm.microvm_workload_identity.is_none()
                    && self.microvm.microvm_lifecycle.is_none()
                    && self.microvm.microvm_report.is_none()
                    && self.microvm.restore_processors.is_none()
                    && self.microvm.restore_memory.is_none()
                    && self.microvm.memory_capacity.is_none()
                    && self.microvm.microvm_control_console.is_none()
                    && !self.microvm.microvm_control_auth_stdin,
                "--network-profile, --net-tap, --mount, --microvm-sandbox-block, --microvm-workload-identity, --microvm-lifecycle, --microvm-control-console, --microvm-control-auth-stdin, --restore-processors, --restore-memory, --memory-capacity, and microVM network policy require a microVM machine"
            );
            return Ok(());
        }

        anyhow::ensure!(
            cfg!(guest_arch = "x86_64"),
            "microVM requires an x86-64 guest"
        );
        anyhow::ensure!(
            openvmm_defs::microvm::microvm_processor_count_supported(self.processors),
            "microVM does not support {} vCPUs",
            self.processors
        );
        anyhow::ensure!(
            self.numa.is_none() && self.numa_distance.is_none(),
            "microVM does not support custom NUMA topology"
        );
        anyhow::ensure!(
            self.vps_per_socket.is_none()
                && self.smt == SmtConfigCli::Auto
                && self.apic_id_offset == 0
                && matches!(self.x2apic, X2ApicConfig::Auto),
            "microVM owns CPU topology and APIC configuration"
        );
        if self.microvm.snapshot_destination.is_some() {
            anyhow::ensure!(
                !self.private_memory(),
                "microVM snapshot capture requires shared file-backed RAM"
            );
            anyhow::ensure!(
                !self.memory.hugepages,
                "microVM snapshot capture does not support explicit hugepage backing"
            );
            anyhow::ensure!(
                self.microvm.snapshot_quiesce_timeout_ms != 0,
                "microVM snapshot quiesce timeout must be nonzero"
            );
            anyhow::ensure!(
                self.microvm.snapshot_tier.is_some()
                    != self.microvm.microvm_sandbox_block.is_empty(),
                "--snapshot-tier is required exactly for microVM snapshot capture with sandbox blocks"
            );
            if let Some(memory_capacity) = self.microvm.memory_capacity {
                anyhow::ensure!(
                    memory_capacity.0 >= self.memory_size(),
                    "--memory-capacity must be at least the base --memory size"
                );
                anyhow::ensure!(
                    self.memory_size().is_multiple_of(
                        openvmm_helpers::snapshot::microvm::MICROVM_MEMORY_BLOCK_SIZE_BYTES
                    ) && memory_capacity.0.is_multiple_of(
                        openvmm_helpers::snapshot::microvm::MICROVM_MEMORY_BLOCK_SIZE_BYTES
                    ),
                    "--memory and --memory-capacity must be aligned to the 128-MiB microVM memory block size"
                );
            }
        }
        if self.restore_snapshot.is_some() {
            anyhow::ensure!(
                self.microvm.microvm_workload_identity.is_none(),
                "--microvm-workload-identity is fixed by the captured microVM command line"
            );
            anyhow::ensure!(
                self.microvm.microvm_lifecycle.is_none(),
                "--microvm-lifecycle is fixed by the captured microVM command line"
            );
            anyhow::ensure!(
                self.net.is_empty(),
                "microVM restore takes network addressing from saved state; do not pass --net"
            );
            anyhow::ensure!(
                self.microvm.restore_gate_timeout_ms != 0,
                "microVM post-restore gate timeout must be nonzero"
            );
            if let Some(restore_processors) = self.microvm.restore_processors {
                anyhow::ensure!(
                    openvmm_defs::microvm::microvm_processor_count_supported(restore_processors,),
                    "microVM does not support a restore-online count of {restore_processors}"
                );
            }
        }
        anyhow::ensure!(
            self.uefi.is_none() && !self.pcat && self.igvm.is_none() && !self.device_tree,
            "microVM requires Linux direct boot"
        );
        anyhow::ensure!(
            self.deprecated_uefi_firmware.is_none()
                && !self.deprecated_uefi_debug
                && !self.deprecated_uefi_enable_memory_protections
                && !self.deprecated_uefi_force_dma_bounce
                && !self.deprecated_uefi_force_firmware_version
                && !self.deprecated_disable_frontpage
                && self.pcat_firmware.is_none()
                && self.pcat_boot_order.is_none()
                && self.vga_firmware.is_none()
                && !self.secure_boot
                && self.secure_boot_template.is_none()
                && self.custom_uefi_json.is_none()
                && self.deprecated_uefi_console_mode.is_none()
                && self.deprecated_efi_diagnostics_log_level.is_none()
                && !self.deprecated_default_boot_always_attempt,
            "microVM does not support firmware options"
        );
        anyhow::ensure!(
            !self.hv
                && !self.vtl2
                && self.isolation.is_none()
                && !self.nested_virt
                && !self.get
                && !self.vmbus_redirect
                && self.vmbus_vsock_path.is_none()
                && self.vmbus_vtl2_vsock_path.is_none()
                && self.openhcl_dump_path.is_none()
                && self.gdb.is_none(),
            "microVM does not support Hyper-V, VTL2, isolation, nested virtualization, GET, or VMBus"
        );
        if let Some(hypervisor) = self.hypervisor.as_deref() {
            let name = hypervisor.split(':').next().unwrap_or(hypervisor);
            anyhow::ensure!(
                (cfg!(target_os = "linux") && matches!(name, "kvm" | "mshv"))
                    || (cfg!(windows) && name == "whp"),
                "microVM requires KVM or MSHV on Linux, or WHP on Windows"
            );
        }

        anyhow::ensure!(
            self.com1.is_none()
                && self.com2.is_none()
                && self.com3.is_none()
                && self.com4.is_none()
                && self.vmbus_com1_serial.is_none()
                && self.vmbus_com2_serial.is_none()
                && self.debugcon.is_none()
                && !self.serial_tx_only,
            "microVM exposes only portb and virtio-console serial devices"
        );
        anyhow::ensure!(
            self.virtio_console_pcie_port.is_none(),
            "microVM requires virtio-console on its fixed MMIO transport"
        );
        if let Some(console) = &self.virtio_console {
            anyhow::ensure!(
                matches!(
                    console,
                    SerialConfigCli::Pipe(_)
                        | SerialConfigCli::Tcp(_)
                        | SerialConfigCli::ConnectPipe(_)
                        | SerialConfigCli::ConnectTcp(_)
                        | SerialConfigCli::Console
                        | SerialConfigCli::None
                ),
                "microVM virtio-console requires listen=..., connect=..., console, or none"
            );
        }
        anyhow::ensure!(
            self.virtio_console.is_some() || self.virtio_console_pcie_port.is_none(),
            "--virtio-console-pcie-port requires --virtio-console"
        );
        self.validate_control_stdin_console(self.virtio_console.as_ref())?;
        if let Some(control_console) = &self.microvm.microvm_control_console {
            anyhow::ensure!(
                self.virtio_console.is_some() || self.restore_snapshot.is_some(),
                "--microvm-control-console requires --virtio-console for a fresh boot"
            );
            anyhow::ensure!(
                matches!(
                    control_console,
                    SerialConfigCli::Pipe(_) | SerialConfigCli::None
                ),
                "microVM control console requires listen=... or none"
            );
            anyhow::ensure!(
                (1..=60_000).contains(&self.microvm.microvm_control_auth_timeout_ms),
                "--microvm-control-auth-timeout-ms must be between 1 and 60000"
            );
            if matches!(control_console, SerialConfigCli::None) {
                anyhow::ensure!(
                    !self.microvm.microvm_control_auth_stdin,
                    "--microvm-control-auth-stdin is not used with a disconnected control console"
                );
            } else {
                anyhow::ensure!(
                    cfg!(any(target_os = "linux", windows)),
                    "live microVM control consoles require Linux Unix sockets or Windows named pipes"
                );
                anyhow::ensure!(
                    self.microvm.microvm_control_auth_stdin,
                    "live microVM control console requires --microvm-control-auth-stdin"
                );
            }
        } else {
            anyhow::ensure!(
                !self.microvm.microvm_control_auth_stdin,
                "--microvm-control-auth-stdin requires --microvm-control-console"
            );
        }
        if self.microvm.microvm_lifecycle == Some(MicrovmLifecycleCli::Managed) {
            anyhow::ensure!(
                self.microvm.microvm_workload_identity.is_some(),
                "--microvm-lifecycle managed requires --microvm-workload-identity"
            );
            anyhow::ensure!(
                self.microvm
                    .microvm_control_console
                    .as_ref()
                    .is_some_and(|console| !matches!(console, SerialConfigCli::None)),
                "--microvm-lifecycle managed requires a live --microvm-control-console"
            );
            anyhow::ensure!(
                self.microvm.microvm_control_auth_stdin,
                "--microvm-lifecycle managed requires --microvm-control-auth-stdin"
            );
        }
        anyhow::ensure!(
            self.disk.is_empty()
                && self.nvme.is_empty()
                && self.nvme_pci.is_empty()
                && self.vmbus_scsi.is_empty()
                && self.openhcl_controller.is_empty()
                && self.ide.is_empty()
                && self.floppy.is_empty(),
            "microVM supports only its fixed MMIO storage devices"
        );
        anyhow::ensure!(
            self.virtio_blk.is_empty(),
            "microVM requires --microvm-sandbox-block instead of --virtio-blk"
        );
        anyhow::ensure!(
            self.microvm.microvm_sandbox_block.len() <= 4,
            "microVM permits at most three read-only layers and one writable scratch device"
        );
        anyhow::ensure!(
            self.microvm.microvm_mount_deny.len() <= 128,
            "microVM filesystem permits at most 128 denied paths"
        );
        openvmm_defs::microvm::validate_microvm_filesystems(
            &self
                .microvm
                .microvm_mount
                .iter()
                .map(|mount| {
                    openvmm_defs::microvm::MicrovmFilesystemConfig::new(
                        mount.guest_target.clone(),
                        mount.access,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?,
        )
        .context("invalid --mount options")?;
        anyhow::ensure!(
            cfg!(target_os = "linux")
                || self.microvm.microvm_mount_owner != Some(MicrovmMountOwnerCli::Caller),
            "--mount-owner caller requires a Linux host"
        );
        for (index, block) in self.microvm.microvm_sandbox_block.iter().enumerate() {
            anyhow::ensure!(
                block.disk.read_only == block.role.is_read_only(),
                "microVM sandbox block role {:?} must be {}",
                block.role,
                if block.role.is_read_only() {
                    "read-only"
                } else {
                    "writable"
                }
            );
            if let Some(previous) = index
                .checked_sub(1)
                .and_then(|index| self.microvm.microvm_sandbox_block.get(index))
            {
                anyhow::ensure!(
                    previous.role < block.role,
                    "microVM sandbox block roles must be unique and in fixed order"
                );
            }
        }
        if !self.microvm.microvm_sandbox_block.is_empty() && self.restore_snapshot.is_none() {
            anyhow::ensure!(
                self.microvm
                    .microvm_sandbox_block
                    .last()
                    .is_some_and(|block| block.role == MicrovmSandboxBlockRole::Scratch),
                "microVM sandbox block topology requires a writable scratch device"
            );
        }
        anyhow::ensure!(
            self.virtio_9p.is_empty()
                && self.virtio_fs.is_empty()
                && self.virtio_fs_shmem.is_empty()
                && self.virtio_pmem.is_none()
                && !self.virtio_rng
                && self.virtio_vsock_path.is_none()
                && self.virtio_net.is_empty(),
            "microVM does not expose additional virtio devices"
        );
        #[cfg(target_os = "linux")]
        anyhow::ensure!(
            self.vhost_user.is_empty(),
            "microVM does not support vhost-user devices"
        );
        #[cfg(target_os = "linux")]
        anyhow::ensure!(
            self.virtio_vsock_vhost_cid.is_none(),
            "microVM does not support vhost-vsock"
        );
        anyhow::ensure!(
            !self.nic
                && self.mana.is_empty()
                && !self.gfx
                && !self.vtl2_gfx
                && !self.vnc.vnc
                && self.tpm.is_none()
                && !self.guest_watchdog
                && self.imc.is_none()
                && !self.battery
                && self.vmgs.is_none(),
            "microVM does not expose legacy NIC, MANA, graphics, TPM, watchdog, IMC, battery, or VMGS devices"
        );
        anyhow::ensure!(
            self.net.len() <= 1,
            "microVM permits at most one virtio-net device"
        );
        anyhow::ensure!(
            self.net.is_empty()
                || self.microvm.network_profile == Some(MicrovmNetworkProfileCli::Portable),
            "microVM --net requires --network-profile portable"
        );
        anyhow::ensure!(
            self.microvm.network_profile.is_none()
                || !self.net.is_empty()
                || self.restore_snapshot.is_some(),
            "--network-profile portable requires --net or --restore-snapshot"
        );
        anyhow::ensure!(
            self.net.iter().all(|network| {
                matches!(network.endpoint, EndpointConfigCli::Microvm(_))
                    && network.vtl == DeviceVtl::Vtl0
                    && network.max_queues.is_none()
                    && !network.underhill
                    && network.pcie_port.is_none()
            }),
            "microVM --net requires a bare IPv4/prefix and does not permit queue, VTL, Underhill, or PCIe modifiers"
        );
        if let [network] = self.net.as_slice()
            && let EndpointConfigCli::Microvm(config) = &network.endpoint
        {
            self.microvm_egress_policy(config)?;
        }
        anyhow::ensure!(
            self.microvm.net_tap.is_none(),
            "--net-tap is incompatible with the portable microVM network profile"
        );
        anyhow::ensure!(
            self.microvm.network_ingress != Some(MicrovmNetworkActionCli::Allow),
            "--network-ingress allow is unsupported by the portable microVM network profile"
        );
        anyhow::ensure!(
            self.microvm.network_egress_allow.len() <= 256
                && self.microvm.network_egress_deny.len() <= 256,
            "microVM egress policy permits at most 256 allow rules and 256 deny rules"
        );
        anyhow::ensure!(
            self.microvm.host_loopback_forward.len() <= 64,
            "microVM host loopback permits at most 64 explicit port forwards"
        );
        if !self.microvm.host_loopback_forward.is_empty() {
            anyhow::ensure!(
                self.microvm.host_loopback == Some(MicrovmNetworkActionCli::Allow),
                "--host-loopback-forward requires explicit --host-loopback allow"
            );
            anyhow::ensure!(
                self.microvm.snapshot_destination.is_none() && self.restore_snapshot.is_none(),
                "microVM snapshots do not support live host-loopback port forwards"
            );
            let mut forwards = self.microvm.host_loopback_forward.clone();
            forwards.sort_unstable();
            forwards.dedup();
            anyhow::ensure!(
                forwards.len() == self.microvm.host_loopback_forward.len(),
                "microVM host-loopback port forwards must be unique"
            );
            anyhow::ensure!(
                forwards.windows(2).all(|pair| {
                    pair[0].protocol != pair[1].protocol || pair[0].host_port != pair[1].host_port
                }),
                "microVM host-loopback forwards cannot bind one protocol and host port more than once"
            );
        }
        anyhow::ensure!(
            self.microvm.network_egress_allow.is_empty()
                && self.microvm.network_egress_deny.is_empty()
                || self.microvm.network_egress.is_some(),
            "--network-egress is required with --network-egress-allow or --network-egress-deny"
        );
        anyhow::ensure!(
            (self.microvm.network_egress_allow.is_empty()
                && self.microvm.network_egress_deny.is_empty())
                || (self.microvm.allow_host.is_empty()
                    && self.microvm.block_host.is_empty()
                    && self.microvm.allow_endpoint.is_empty()),
            "L3/L4 egress rules cannot be combined with legacy --allow-host, --block-host, or --allow-endpoint policy"
        );
        anyhow::ensure!(
            !matches!(
                self.microvm.network_egress,
                Some(MicrovmNetworkActionCli::Allow)
            ) || (self.microvm.allow_host.is_empty() && self.microvm.allow_endpoint.is_empty()),
            "--network-egress allow conflicts with default-deny egress allow rules"
        );
        anyhow::ensure!(
            self.microvm.network_egress != Some(MicrovmNetworkActionCli::Deny)
                || self.microvm.block_host.is_empty(),
            "--network-egress deny conflicts with default-allow egress block rules"
        );
        anyhow::ensure!(
            (self.microvm.allow_host.is_empty()
                && self.microvm.block_host.is_empty()
                && self.microvm.allow_endpoint.is_empty()
                && self.microvm.network_egress_allow.is_empty()
                && self.microvm.network_egress_deny.is_empty()
                && self.microvm.host_loopback.is_none()
                && self.microvm.network_proxy.is_none()
                && self.microvm.host_loopback_forward.is_empty()
                && self.microvm.network_egress.is_none()
                && self.microvm.network_ingress.is_none())
                || !self.net.is_empty()
                || self.restore_snapshot.is_some(),
            "microVM network policy requires --net or a networked snapshot restore"
        );
        anyhow::ensure!(
            self.cxl_test.is_empty()
                && self.pcie_root_complex.is_empty()
                && self.pcie_root_port.is_empty()
                && self.pcie_switch.is_empty()
                && self.pcie_generic_initiator.is_empty()
                && self.pcie_remote.is_empty()
                && self.amd_iommu.is_empty()
                && self.intel_vtd.is_empty(),
            "microVM does not support PCIe or IOMMU devices"
        );
        #[cfg(windows)]
        anyhow::ensure!(
            self.device.is_empty() && self.kernel_vmnic.is_empty(),
            "microVM does not support assigned devices or kernel VM NICs"
        );
        #[cfg(target_os = "linux")]
        anyhow::ensure!(
            self.vfio.is_empty() && self.iommu.is_empty(),
            "microVM does not support VFIO or IOMMU devices"
        );

        Ok(())
    }

    pub(crate) fn validate_control_stdin_console(
        &self,
        boot_console: Option<&SerialConfigCli>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.microvm.microvm_control_auth_stdin
                || !matches!(boot_console, Some(SerialConfigCli::Console)),
            "--microvm-control-auth-stdin reserves stdin; use a socket or none for the boot console"
        );
        Ok(())
    }

    pub(crate) fn microvm_egress_policy(
        &self,
        network: &openvmm_defs::microvm::MicrovmNetworkConfig,
    ) -> Result<
        net_backend_resources::egress::EgressPolicy,
        net_backend_resources::egress::InvalidEgressPolicy,
    > {
        use net_backend_resources::egress::EgressPolicyMode;

        let mode = if !self.microvm.network_egress_allow.is_empty()
            || !self.microvm.network_egress_deny.is_empty()
        {
            EgressPolicyMode::Rules {
                default_action: self
                    .microvm
                    .network_egress
                    .unwrap_or(MicrovmNetworkActionCli::Allow)
                    .into(),
                allow: self.microvm.network_egress_allow.clone(),
                deny: self.microvm.network_egress_deny.clone(),
            }
        } else if !self.microvm.allow_host.is_empty() {
            EgressPolicyMode::AllowList(self.microvm.allow_host.clone())
        } else if !self.microvm.block_host.is_empty() {
            EgressPolicyMode::BlockList(self.microvm.block_host.clone())
        } else if !self.microvm.allow_endpoint.is_empty() {
            EgressPolicyMode::TcpEndpoints(self.microvm.allow_endpoint.clone())
        } else if self.microvm.network_egress == Some(MicrovmNetworkActionCli::Deny) {
            EgressPolicyMode::DenyAll
        } else {
            EgressPolicyMode::AllowAll
        };
        net_backend_resources::egress::EgressPolicy::bind(
            network.guest_ipv4,
            network.prefix_length,
            network.guest_mac,
            network.derived_gateway_ipv4,
            mode,
        )
        .and_then(|policy| {
            policy.with_host_loopback(
                self.microvm
                    .host_loopback
                    .unwrap_or(MicrovmNetworkActionCli::Allow)
                    .into(),
                self.microvm.network_proxy,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use openvmm_defs::microvm::MICROVM_BASE_COMMAND_LINE;
    use openvmm_defs::microvm::MICROVM_COMMAND_LINE_MAX_SIZE;
    use openvmm_defs::microvm::MICROVM_CONSOLE_COMMAND_LINE;
    use openvmm_defs::microvm::append_microvm_virtio_discovery;
    use openvmm_defs::microvm::build_microvm_command_line;
    use openvmm_defs::microvm::build_microvm_control_command_line;
    use test_with_tracing::test;

    #[test]
    fn test_machine_profile_option_parsed() {
        let opt = Options::try_parse_from(["openvmm"]).unwrap();
        assert_eq!(opt.machine, MachineProfileCli::Standard);
        assert_eq!(MachineProfile::from(opt.machine), MachineProfile::Standard);

        let opt = Options::try_parse_from(["openvmm", "--machine", "microvm"]).unwrap();
        assert_eq!(opt.machine, MachineProfileCli::Microvm);
        assert_eq!(MachineProfile::from(opt.machine), MachineProfile::Microvm);

        assert!(Options::try_parse_from(["openvmm", "--machine", "microvm-v2"]).is_err());
        assert!(Options::try_parse_from(["openvmm", "--machine", "microvm-v3"]).is_err());
        assert!(Options::try_parse_from(["openvmm", "--machine", "nvx"]).is_err());
        assert!(Options::try_parse_from(["openvmm", "--machine", "unknown"]).is_err());
    }

    #[test]
    fn test_microvm_processor_validation() {
        for processors in [1, 2, 4, 8] {
            let options = Options::try_parse_from([
                "openvmm",
                "--machine",
                "microvm",
                "--processors",
                &processors.to_string(),
            ])
            .unwrap();
            options.validate_microvm_options().unwrap();
        }

        for processors in [0, 3, 5, 16] {
            let options = Options::try_parse_from([
                "openvmm",
                "--machine",
                "microvm",
                "--processors",
                &processors.to_string(),
            ])
            .unwrap();
            assert!(options.validate_microvm_options().is_err());
        }

        for restore_processors in [1, 2, 4, 8] {
            let options = Options::try_parse_from([
                "openvmm",
                "--machine",
                "microvm",
                "--restore-snapshot",
                "snapshot",
                "--restore-processors",
                &restore_processors.to_string(),
            ])
            .unwrap();
            options.validate_microvm_options().unwrap();
        }
        let noncanonical_restore = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--restore-processors",
            "3",
        ])
        .unwrap();
        assert!(noncanonical_restore.validate_microvm_options().is_err());

        for args in [
            vec!["openvmm", "--machine", "microvm", "--vps-per-socket", "1"],
            vec!["openvmm", "--machine", "microvm", "--smt", "off"],
            vec!["openvmm", "--machine", "microvm", "--apic-id-offset", "1"],
            vec!["openvmm", "--machine", "microvm", "--x2apic", "on"],
            vec!["openvmm", "--machine", "microvm", "--numa", "size=128M"],
        ] {
            let options = Options::try_parse_from(args).unwrap();
            assert!(options.validate_microvm_options().is_err());
        }
    }

    #[test]
    fn test_microvm_memory_capacity_and_restore_target_parsing() {
        let capture = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--memory",
            "512M",
            "--snapshot-destination",
            "snapshot",
            "--memory-capacity",
            "2G",
        ])
        .unwrap();
        capture.validate_microvm_options().unwrap();
        assert_eq!(
            capture.microvm.memory_capacity.unwrap().0,
            2 * 1024 * 1024 * 1024
        );

        let restore = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--restore-memory",
            "512M",
        ])
        .unwrap();
        restore.validate_microvm_options().unwrap();
        assert_eq!(restore.microvm.restore_memory.unwrap().0, 512 * 1024 * 1024);

        assert!(
            Options::try_parse_from(
                ["openvmm", "--machine", "microvm", "--memory-capacity", "2G",]
            )
            .is_err()
        );
        assert!(
            Options::try_parse_from(["openvmm", "--machine", "microvm", "--restore-memory", "1G",])
                .is_err()
        );

        let unaligned = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--memory",
            "513M",
            "--snapshot-destination",
            "snapshot",
            "--memory-capacity",
            "2G",
        ])
        .unwrap();
        assert!(unaligned.validate_microvm_options().is_err());
    }

    #[test]
    fn test_microvm_sandbox_block_parser_and_validation() {
        let valid = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--microvm-sandbox-block",
            "distro:mem:1M,ro",
            "--microvm-sandbox-block",
            "runtime:mem:1M,ro",
            "--microvm-sandbox-block",
            "custom:mem:1M,ro",
            "--microvm-sandbox-block",
            "scratch:mem:1M",
        ])
        .unwrap();
        valid.validate_microvm_options().unwrap();

        for args in [
            vec![
                "openvmm",
                "--machine",
                "microvm",
                "--microvm-sandbox-block",
                "distro:mem:1M",
                "--microvm-sandbox-block",
                "scratch:mem:1M",
            ],
            vec![
                "openvmm",
                "--machine",
                "microvm",
                "--microvm-sandbox-block",
                "distro:mem:1M,ro",
                "--microvm-sandbox-block",
                "distro:mem:1M,ro",
                "--microvm-sandbox-block",
                "scratch:mem:1M",
            ],
            vec![
                "openvmm",
                "--machine",
                "microvm",
                "--microvm-sandbox-block",
                "scratch:mem:1M,ro",
            ],
            vec!["openvmm", "--machine", "microvm", "--virtio-blk", "mem:1M"],
        ] {
            let options = Options::try_parse_from(args).unwrap();
            assert!(options.validate_microvm_options().is_err());
        }
    }

    #[test]
    fn test_microvm_snapshot_tier_is_explicit() {
        for tier in ["platform", "workload-start", "instance-checkpoint"] {
            let options = Options::try_parse_from([
                "openvmm",
                "--machine",
                "microvm",
                "--snapshot-destination",
                "snapshot",
                "--snapshot-tier",
                tier,
                "--microvm-sandbox-block",
                "distro:mem:1M,ro",
                "--microvm-sandbox-block",
                "scratch:mem:1M",
            ])
            .unwrap();
            assert_eq!(options.microvm.restore_gate_timeout_ms, 60_000);
            options.validate_microvm_options().unwrap();
        }

        let missing = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--snapshot-destination",
            "snapshot",
            "--microvm-sandbox-block",
            "distro:mem:1M,ro",
            "--microvm-sandbox-block",
            "scratch:mem:1M",
        ])
        .unwrap();
        assert!(missing.validate_microvm_options().is_err());

        let blockless = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--processors",
            "2",
            "--snapshot-destination",
            "snapshot",
        ])
        .unwrap();
        blockless.validate_microvm_options().unwrap();

        assert!(
            Options::try_parse_from([
                "openvmm",
                "--machine",
                "microvm",
                "--snapshot-tier",
                "platform",
            ])
            .is_err()
        );

        let zero_timeout = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--restore-gate-timeout-ms",
            "0",
        ])
        .unwrap();
        assert!(zero_timeout.validate_microvm_options().is_err());
    }

    #[test]
    fn test_microvm_command_line_is_owned_and_bounded() {
        assert_eq!(
            build_microvm_command_line(&[], false).unwrap(),
            MICROVM_BASE_COMMAND_LINE
        );
        assert_eq!(
            build_microvm_command_line(&["foo=bar".into()], false).unwrap(),
            format!("{MICROVM_BASE_COMMAND_LINE} foo=bar")
        );
        assert_eq!(
            build_microvm_command_line(&[], true).unwrap(),
            MICROVM_CONSOLE_COMMAND_LINE
        );

        let mut with_devices = build_microvm_command_line(&[], true).unwrap();
        let blocks = [
            openvmm_defs::microvm::MicrovmSandboxBlockConfig {
                role: MicrovmSandboxBlockRole::Distro,
                read_only: true,
            },
            openvmm_defs::microvm::MicrovmSandboxBlockConfig {
                role: MicrovmSandboxBlockRole::Scratch,
                read_only: false,
            },
        ];
        append_microvm_virtio_discovery(&mut with_devices, None, false, &[], true, false, &blocks)
            .unwrap();
        assert_eq!(
            with_devices,
            format!(
                "{MICROVM_CONSOLE_COMMAND_LINE} virtio_mmio.device=0x1000@0xd0002000:7 virtio_mmio.device=0x1000@0xd0003000:4 virtio_mmio.device=0x1000@0xd0006000:11"
            )
        );
        let mut with_control_console = build_microvm_control_command_line(&[], true).unwrap();
        append_microvm_virtio_discovery(
            &mut with_control_console,
            None,
            false,
            &[],
            true,
            true,
            &[],
        )
        .unwrap();
        assert_eq!(
            with_control_console,
            format!(
                "{MICROVM_CONSOLE_COMMAND_LINE} \
                 virtio_mmio.device=0x1000@0xd0002000:7 \
                 virtio_mmio.device=0x1000@0xd0007000:3 \
                 {}",
                openvmm_defs::microvm::MICROVM_CONTROL_TTY_COMMAND_LINE
            )
        );

        let network = "10.0.0.2/24".parse().unwrap();
        let mut with_network = build_microvm_command_line(&[], false).unwrap();
        append_microvm_virtio_discovery(
            &mut with_network,
            Some((
                &network,
                openvmm_defs::microvm::MICROVM_VIRTIO_NET_KVM_IRQ,
                false,
            )),
            false,
            &[],
            false,
            false,
            &[],
        )
        .unwrap();
        assert_eq!(
            with_network,
            format!(
                "{MICROVM_BASE_COMMAND_LINE} virtio_mmio.device=0x1000@0xd0000000:10 virtnet_ip=10.0.0.2 virtnet_mask=255.255.255.0 virtnet_gw=10.0.0.1"
            )
        );
        let mut with_whp_network = build_microvm_command_line(&[], false).unwrap();
        append_microvm_virtio_discovery(
            &mut with_whp_network,
            Some((
                &network,
                openvmm_defs::microvm::MICROVM_VIRTIO_NET_WHP_IRQ,
                true,
            )),
            false,
            &[],
            false,
            false,
            &[],
        )
        .unwrap();
        assert!(with_whp_network.ends_with("virtnet_dns=10.0.0.1"));

        let filesystem = openvmm_defs::microvm::MicrovmFilesystemConfig::new(
            "/mnt/share".to_owned(),
            openvmm_defs::microvm::MicrovmFilesystemAccess::ReadOnly,
        )
        .unwrap();
        let mut with_filesystem = build_microvm_command_line(&[], false).unwrap();
        append_microvm_virtio_discovery(
            &mut with_filesystem,
            None,
            true,
            std::slice::from_ref(&filesystem),
            false,
            false,
            &[],
        )
        .unwrap();
        assert_eq!(
            with_filesystem,
            format!(
                "{MICROVM_BASE_COMMAND_LINE} virtio_mmio.device=0x1000@0xd0001000:6 virtfs_dir=/mnt/share virtfs_tag=microvm virtfs_mode=ro"
            )
        );

        for reserved in [
            "earlycon=uart",
            "console=ttyS0",
            "virtio_mmio.device=bad",
            "virtnet_ip=10.0.0.3",
            "virtnet_mask=255.255.255.0",
            "virtnet_gw=10.0.0.1",
            "virtnet_dns=10.0.0.1",
            "nr_cpus=1",
            "virtfs_dir=/other",
            "virtfs_tag=other",
            "virtfs_mode=rw",
        ] {
            assert!(build_microvm_command_line(&[reserved.into()], false).is_err());
        }
        for reserved in [
            "nvx_control_tty=hvc9",
            "nvx-control-tty=hvc9",
            "driver_async_probe=virtio_console",
            "driver-async-probe=virtio_console",
            "virtio-mmio.device=0x1000@0xd0007000:3",
        ] {
            assert!(build_microvm_control_command_line(&[reserved.into()], false).is_err());
            assert!(build_microvm_command_line(&[reserved.into()], false).is_ok());
        }
        for delimiter in ["--", "\"driver-async-probe=virtio_console\""] {
            assert!(build_microvm_control_command_line(&[delimiter.into()], false).is_err());
            assert!(build_microvm_command_line(&[delimiter.into()], false).is_ok());
        }
        assert!(build_microvm_command_line(&["foo=bar\0baz".into()], false).is_err());
        assert!(
            build_microvm_command_line(&["x".repeat(MICROVM_COMMAND_LINE_MAX_SIZE)], false)
                .is_err()
        );
    }

    #[test]
    fn test_microvm_preflight_rejects_unsupported_combinations() {
        let valid = Options::try_parse_from(["openvmm", "--machine", "microvm"]).unwrap();
        valid.validate_microvm_options().unwrap();
        if cfg!(target_os = "linux") {
            let valid_mshv = Options::try_parse_from([
                "openvmm",
                "--machine",
                "microvm",
                "--hypervisor",
                "mshv",
            ])
            .unwrap();
            valid_mshv.validate_microvm_options().unwrap();
        }
        let valid_console = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--virtio-console",
            "listen=tcp:127.0.0.1:5555",
        ])
        .unwrap();
        valid_console.validate_microvm_options().unwrap();
        let valid_control_console = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--virtio-console",
            "none",
            "--microvm-control-console",
            "none",
        ])
        .unwrap();
        valid_control_console.validate_microvm_options().unwrap();
        assert!(
            Options::try_parse_from([
                "openvmm",
                "--machine",
                "microvm",
                "--microvm-control-protocol-version",
                "1",
            ])
            .is_err()
        );
        let control_endpoint = if cfg!(windows) {
            "listen=//./pipe/openvmm-microvm-restore-test"
        } else {
            "listen=control.sock"
        };
        let valid_restore_control_console = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--microvm-control-console",
            control_endpoint,
            "--microvm-control-auth-stdin",
        ])
        .unwrap();
        if cfg!(any(target_os = "linux", windows)) {
            valid_restore_control_console
                .validate_microvm_options()
                .unwrap();
        } else {
            assert!(
                valid_restore_control_console
                    .validate_microvm_options()
                    .is_err()
            );
        }
        let valid_network = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--net",
            "10.0.0.2/24",
            "--network-profile",
            "portable",
        ])
        .unwrap();
        valid_network.validate_microvm_options().unwrap();
        match EndpointConfigCli::from_str("10.0.0.2/24").unwrap() {
            EndpointConfigCli::Microvm(network) => {
                assert_eq!(network.guest_ipv4, std::net::Ipv4Addr::new(10, 0, 0, 2));
                assert_eq!(network.prefix_length, 24);
                assert_eq!(network.netmask(), std::net::Ipv4Addr::new(255, 255, 255, 0));
                assert_eq!(
                    network.derived_gateway_ipv4,
                    std::net::Ipv4Addr::new(10, 0, 0, 1)
                );
                assert_eq!(network.guest_mac.to_bytes(), [0x52, 0x54, 0, 0, 0, 2]);
                assert_eq!(network.gateway_mac.to_bytes(), [0x52, 0x54, 0, 0, 0, 1]);
            }
            _ => panic!("Expected microVM network variant"),
        }
        for invalid in [
            "10.0.0.2",
            "10.0.0.2/0",
            "10.0.0.2/31",
            "10.0.0.0/24",
            "10.0.0.1/24",
            "10.0.0.255/24",
            "fd00::2/64",
        ] {
            assert!(EndpointConfigCli::from_str(invalid).is_err(), "{invalid}");
        }
        let valid_filesystem = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--mount",
            "/mnt/share,host,ro",
        ])
        .unwrap();
        valid_filesystem.validate_microvm_options().unwrap();

        for args in [
            vec!["openvmm", "--machine", "microvm", "--net", "10.0.0.2/24"],
            vec![
                "openvmm",
                "--machine",
                "microvm",
                "--network-profile",
                "portable",
                "--net-tap",
                "tap0",
            ],
            vec!["openvmm", "--machine", "microvm", "--uefi"],
            vec!["openvmm", "--machine", "microvm", "--hypervisor", "unknown"],
            vec!["openvmm", "--machine", "microvm", "--virtio-rng"],
            vec!["openvmm", "--machine", "microvm", "--com1", "none"],
            vec![
                "openvmm",
                "--machine",
                "microvm",
                "--virtio-console",
                "stderr",
            ],
            vec![
                "openvmm",
                "--machine",
                "microvm",
                "--virtio-console",
                "listen=tcp:127.0.0.1:5555",
                "--virtio-console-pcie-port",
                "port0",
            ],
            vec![
                "openvmm",
                "--machine",
                "microvm",
                "--microvm-control-console",
                "none",
            ],
            vec![
                "openvmm",
                "--machine",
                "microvm",
                "--virtio-console",
                "none",
                "--microvm-control-console",
                "listen=tcp:127.0.0.1:5555",
            ],
            vec![
                "openvmm",
                "--machine",
                "microvm",
                "--virtio-console",
                "none",
                "--microvm-control-console",
                "listen=control.sock",
            ],
            vec![
                "openvmm",
                "--machine",
                "microvm",
                "--virtio-console",
                "none",
                "--microvm-control-console",
                "none",
                "--microvm-control-auth-stdin",
            ],
            vec!["openvmm", "--machine", "microvm", "--net", "consomme"],
            vec![
                "openvmm",
                "--machine",
                "microvm",
                "--net",
                "10.0.0.2/24",
                "--net",
                "10.0.1.2/24",
            ],
            vec![
                "openvmm",
                "--machine",
                "microvm",
                "--net",
                "queues=1:10.0.0.2/24",
            ],
        ] {
            let options = Options::try_parse_from(args).unwrap();
            assert!(options.validate_microvm_options().is_err());
        }
    }

    #[test]
    fn control_auth_stdin_is_explicit_and_exclusive() {
        let endpoint = if cfg!(windows) {
            "listen=//./pipe/openvmm-microvm-test"
        } else {
            "listen=control.sock"
        };
        let args = [
            "openvmm",
            "--machine",
            "microvm",
            "--virtio-console",
            "none",
            "--microvm-control-console",
            endpoint,
            "--microvm-control-auth-stdin",
        ];
        let options = Options::try_parse_from(args).unwrap();
        assert!(options.microvm.microvm_control_auth_stdin);
        assert_eq!(
            options.validate_microvm_options().is_ok(),
            cfg!(any(target_os = "linux", windows))
        );
        assert!(
            options
                .validate_control_stdin_console(Some(&SerialConfigCli::Console))
                .unwrap_err()
                .to_string()
                .contains("reserves stdin")
        );
        for console in [
            SerialConfigCli::None,
            SerialConfigCli::Pipe("boot.sock".into()),
        ] {
            options
                .validate_control_stdin_console(Some(&console))
                .unwrap();
        }
        for extra in [
            vec!["--rpc", "path=management.sock"],
            vec!["--ttrpc", "management.sock"],
            vec!["--grpc", "management.sock"],
            vec!["--relay-console-path", "console.sock"],
            vec!["--write-saved-state-proto", "proto"],
            vec!["--paused"],
            vec!["--microvm-control-auth-handle", "0"],
        ] {
            assert!(Options::try_parse_from(args.into_iter().chain(extra)).is_err());
        }
        assert!(Options::try_parse_from(["openvmm", "--microvm-control-auth-stdin"]).is_err());
        assert!(
            Options::try_parse_from([
                "openvmm",
                "--microvm-control-console",
                "listen=control.sock",
                "--microvm-control-auth-stdin=0",
            ])
            .is_err()
        );
    }

    #[test]
    fn test_microvm_host_loopback_generic_allow_is_rejected() {
        let common = [
            "openvmm",
            "--machine",
            "microvm",
            "--network-profile",
            "portable",
            "--host-loopback",
            "allow",
        ];
        for extra in [
            vec!["--net", "10.0.0.2/24"],
            vec!["--net", "10.0.0.2/24", "--snapshot-destination", "snapshot"],
            vec!["--restore-snapshot", "snapshot"],
        ] {
            let options = Options::try_parse_from(common.into_iter().chain(extra)).unwrap();
            let error = options.validate_microvm_options().unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("generic host-loopback connectivity"),
                "{error}"
            );
        }
        let forwarded = Options::try_parse_from(common.into_iter().chain([
            "--net",
            "10.0.0.2/24",
            "--network-ingress",
            "deny",
            "--host-loopback-forward",
            "tcp:3000:8080",
            "--host-loopback-forward",
            "udp:3000:8080",
        ]))
        .unwrap();
        forwarded.validate_microvm_options().unwrap();
    }

    #[test]
    fn test_microvm_egress_policy_is_typed_and_canonical() {
        let options = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--net",
            "10.0.0.2/24",
            "--network-profile",
            "portable",
            "--allow-host",
            "192.168.1.9/24",
            "--allow-host",
            "10.0.0.1",
            "--allow-host",
            "192.168.1.0/24",
        ])
        .unwrap();
        options.validate_microvm_options().unwrap();
        let network: openvmm_defs::microvm::MicrovmNetworkConfig = "10.0.0.2/24".parse().unwrap();
        let policy = options.microvm_egress_policy(&network).unwrap();
        let net_backend_resources::egress::EgressPolicyMode::AllowList(rules) = policy.mode()
        else {
            panic!("expected allow-list policy")
        };
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].network(), std::net::Ipv4Addr::new(10, 0, 0, 1));
        assert_eq!(rules[1].network(), std::net::Ipv4Addr::new(192, 168, 1, 0));
        assert!(policy.allows_gateway_dns());

        let endpoint = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--net",
            "10.0.0.2/24",
            "--network-profile",
            "portable",
            "--allow-endpoint",
            "10.0.0.9:8443",
            "--allow-endpoint",
            "192.0.2.7:443",
            "--allow-endpoint",
            "10.0.0.9:443",
        ])
        .unwrap();
        endpoint.validate_microvm_options().unwrap();
        let endpoint_policy = endpoint.microvm_egress_policy(&network).unwrap();
        assert_eq!(
            endpoint_policy.next_hops(),
            &[
                std::net::Ipv4Addr::new(10, 0, 0, 1),
                std::net::Ipv4Addr::new(10, 0, 0, 9),
            ]
        );

        assert!(
            Options::try_parse_from([
                "openvmm",
                "--machine",
                "microvm",
                "--net",
                "10.0.0.2/24",
                "--allow-host",
                "192.0.2.0/24",
                "--block-host",
                "198.51.100.1",
            ])
            .is_err()
        );
        let missing_network = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--allow-host",
            "192.0.2.0/24",
        ])
        .unwrap();
        assert!(missing_network.validate_microvm_options().is_err());
        let standard =
            Options::try_parse_from(["openvmm", "--allow-endpoint", "192.0.2.7:443"]).unwrap();
        assert!(standard.validate_microvm_options().is_err());
    }

    #[test]
    fn test_microvm_workload_identity_is_non_root_and_not_restorable() {
        for identity in ["0:1", "1:0", "root:1", "1:root", "1", "1:2:3"] {
            assert!(
                Options::try_parse_from([
                    "openvmm",
                    "--machine",
                    "microvm",
                    "--microvm-workload-identity",
                    identity,
                ])
                .is_err()
            );
        }

        #[test]
        fn test_microvm_l3_l4_rules_are_explicit_and_composable() {
            let options = Options::try_parse_from([
                "openvmm",
                "--machine",
                "microvm",
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--network-egress",
                "deny",
                "--network-egress-allow",
                "192.0.2.0/24:tcp:443",
                "--network-egress-allow",
                "192.0.2.7:udp:53",
                "--network-egress-deny",
                "192.0.2.9:tcp:443",
            ])
            .unwrap();
            options.validate_microvm_options().unwrap();
            let network: openvmm_defs::microvm::MicrovmNetworkConfig =
                "10.0.0.2/24".parse().unwrap();
            let policy = options.microvm_egress_policy(&network).unwrap();
            let net_backend_resources::egress::EgressPolicyMode::Rules {
                default_action,
                allow,
                deny,
            } = policy.mode()
            else {
                panic!("expected rule-based egress policy")
            };
            assert_eq!(
                *default_action,
                net_backend_resources::egress::EgressAction::Deny
            );
            assert_eq!((allow.len(), deny.len()), (2, 1));

            let missing_default = Options::try_parse_from([
                "openvmm",
                "--machine",
                "microvm",
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--network-egress-allow",
                "192.0.2.0/24",
            ])
            .unwrap();
            assert!(missing_default.validate_microvm_options().is_err());

            let mixed_legacy = Options::try_parse_from([
                "openvmm",
                "--machine",
                "microvm",
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--network-egress",
                "deny",
                "--network-egress-allow",
                "192.0.2.0/24",
                "--allow-host",
                "192.0.2.0/24",
            ])
            .unwrap();
            assert!(mixed_legacy.validate_microvm_options().is_err());
        }

        fn check_microvm_host_loopback_policy_and_explicit_forwards() {
            let network: openvmm_defs::microvm::MicrovmNetworkConfig =
                "10.0.0.2/24".parse().unwrap();
            let denied = Options::try_parse_from([
                "openvmm",
                "--machine",
                "microvm",
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--host-loopback",
                "deny",
                "--network-proxy",
                "10.0.0.1:8443",
            ])
            .unwrap();
            denied.validate_microvm_options().unwrap();
            let policy = denied.microvm_egress_policy(&network).unwrap();
            assert_eq!(
                policy.host_loopback_action(),
                net_backend_resources::egress::EgressAction::Deny
            );
            assert_eq!(policy.proxy_endpoint().unwrap().port(), 8443);

            let wrong_proxy = Options::try_parse_from([
                "openvmm",
                "--machine",
                "microvm",
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--host-loopback",
                "deny",
                "--network-proxy",
                "192.0.2.1:8443",
            ])
            .unwrap();
            assert!(wrong_proxy.validate_microvm_options().is_err());

            let denied_forward = Options::try_parse_from([
                "openvmm",
                "--machine",
                "microvm",
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--host-loopback",
                "deny",
                "--host-loopback-forward",
                "tcp:3000:8080",
            ])
            .unwrap();
            assert!(denied_forward.validate_microvm_options().is_err());

            let allowed_forward = Options::try_parse_from([
                "openvmm",
                "--machine",
                "microvm",
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--host-loopback",
                "allow",
                "--host-loopback-forward",
                "tcp:3000:8080",
            ])
            .unwrap();
            allowed_forward.validate_microvm_options().unwrap();

            let duplicate_forward = Options::try_parse_from([
                "openvmm",
                "--machine",
                "microvm",
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--host-loopback",
                "allow",
                "--host-loopback-forward",
                "tcp:3000:8080",
                "--host-loopback-forward",
                "tcp:3000:8081",
            ])
            .unwrap();
            assert!(duplicate_forward.validate_microvm_options().is_err());
        }
        check_microvm_host_loopback_policy_and_explicit_forwards();

        let options = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--microvm-workload-identity",
            "65534:65534",
        ])
        .unwrap();
        assert_eq!(
            options.microvm.microvm_workload_identity,
            Some(MicrovmWorkloadIdentityCli {
                uid: 65_534,
                gid: 65_534,
            })
        );
        options.validate_microvm_options().unwrap();

        let restore = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--microvm-workload-identity",
            "65534:65534",
        ])
        .unwrap();
        assert!(restore.validate_microvm_options().is_err());
    }

    #[test]
    fn test_managed_lifecycle_requires_authenticated_control() {
        let missing_control = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--microvm-workload-identity",
            "65534:65534",
            "--microvm-lifecycle",
            "managed",
        ])
        .unwrap();
        assert!(missing_control.validate_microvm_options().is_err());

        let disconnected_control = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--virtio-console",
            "none",
            "--microvm-control-console",
            "none",
            "--microvm-workload-identity",
            "65534:65534",
            "--microvm-lifecycle",
            "managed",
        ])
        .unwrap();
        assert!(disconnected_control.validate_microvm_options().is_err());
    }

    #[test]
    fn test_microvm_directional_network_policy_is_independent_and_fail_closed() {
        let network: openvmm_defs::microvm::MicrovmNetworkConfig = "10.0.0.2/24".parse().unwrap();
        for (arguments, expected_mode) in [
            (
                ["--network-egress", "allow", "--network-ingress", "deny"].as_slice(),
                "allow-all",
            ),
            (
                ["--network-egress", "deny", "--network-ingress", "deny"].as_slice(),
                "deny-all",
            ),
            (["--network-egress", "deny"].as_slice(), "deny-all"),
            (["--network-ingress", "deny"].as_slice(), "allow-all"),
            (
                ["--network-egress", "allow", "--block-host", "192.0.2.1"].as_slice(),
                "block-list",
            ),
            (
                ["--network-egress", "deny", "--allow-host", "192.0.2.1"].as_slice(),
                "allow-list",
            ),
        ] {
            let options = Options::try_parse_from(
                [
                    "openvmm",
                    "--machine",
                    "microvm",
                    "--net",
                    "10.0.0.2/24",
                    "--network-profile",
                    "portable",
                ]
                .into_iter()
                .chain(arguments.iter().copied()),
            )
            .unwrap();
            options.validate_microvm_options().unwrap();
            assert_eq!(
                options.microvm_egress_policy(&network).unwrap().mode_name(),
                expected_mode
            );
        }

        for arguments in [
            ["--network-ingress", "allow"].as_slice(),
            ["--network-egress", "deny", "--network-ingress", "allow"].as_slice(),
            ["--network-egress", "allow", "--allow-host", "192.0.2.1"].as_slice(),
            ["--network-egress", "deny", "--block-host", "192.0.2.1"].as_slice(),
        ] {
            let options = Options::try_parse_from(
                [
                    "openvmm",
                    "--machine",
                    "microvm",
                    "--net",
                    "10.0.0.2/24",
                    "--network-profile",
                    "portable",
                ]
                .into_iter()
                .chain(arguments.iter().copied()),
            )
            .unwrap();
            assert!(options.validate_microvm_options().is_err());
        }
    }

    #[test]
    fn test_microvm_endpoint_policy_rejects_invalid_identities_before_resources() {
        for address in [
            "0.0.0.0",
            "127.0.0.1",
            "169.254.1.1",
            "224.0.0.1",
            "240.0.0.1",
            "10.0.0.0",
            "10.0.0.2",
            "10.0.0.255",
        ] {
            let options = Options::try_parse_from([
                "openvmm",
                "--machine",
                "microvm",
                "--net",
                "10.0.0.2/24",
                "--network-profile",
                "portable",
                "--allow-endpoint",
                &format!("{address}:443"),
            ])
            .unwrap();
            assert!(
                options.validate_microvm_options().is_err(),
                "invalid endpoint address {address} was accepted"
            );
        }
    }

    #[test]
    fn test_microvm_mount_from_str() {
        let read_only = MicrovmMountCli::from_str("/mnt/share,host").unwrap();
        assert_eq!(read_only.guest_target, "/mnt/share");
        assert_eq!(read_only.host_path, PathBuf::from("host"));
        assert_eq!(
            read_only.access,
            openvmm_defs::microvm::MicrovmFilesystemAccess::ReadOnly
        );

        let read_write = MicrovmMountCli::from_str("/srv/data,host,rw").unwrap();
        assert_eq!(
            read_write.access,
            openvmm_defs::microvm::MicrovmFilesystemAccess::ReadWrite
        );
        assert!(MicrovmMountCli::from_str("relative,host").is_err());
        assert!(MicrovmMountCli::from_str("/mnt/../escape,host").is_err());
        assert!(MicrovmMountCli::from_str("/mnt/share,host,write").is_err());
    }

    #[test]
    fn test_microvm_mount_is_repeatable_up_to_the_slot_count() {
        let parse = |mounts: &[&str]| {
            let mut args = vec!["openvmm", "--machine", "microvm"];
            for mount in mounts {
                args.extend(["--mount", mount]);
            }
            Options::try_parse_from(args).unwrap()
        };
        let options = parse(&["/workspace,work,rw", "/opt/hostedtoolcache,tools"]);
        options.validate_microvm_options().unwrap();
        let targets = options
            .microvm
            .microvm_mount
            .iter()
            .map(|mount| (mount.guest_target.as_str(), mount.access))
            .collect::<Vec<_>>();
        assert_eq!(
            targets,
            [
                (
                    "/workspace",
                    openvmm_defs::microvm::MicrovmFilesystemAccess::ReadWrite
                ),
                (
                    "/opt/hostedtoolcache",
                    openvmm_defs::microvm::MicrovmFilesystemAccess::ReadOnly
                ),
            ]
        );

        for (mounts, expected) in [
            (&["/a,a", "/b,b", "/c,c"][..], "at most 2 filesystems"),
            (&["/workspace,a,rw", "/workspace,b"][..], "overlap"),
            (&["/workspace,a,rw", "/workspace/cache,b"][..], "overlap"),
            (&["/opt/tools/node,a", "/opt,b,rw"][..], "overlap"),
        ] {
            let error = parse(mounts).validate_microvm_options().unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }

        // --mount-deny and --mount-owner still require a --mount.
        assert!(
            Options::try_parse_from(["openvmm", "--machine", "microvm", "--mount-deny", "x"])
                .is_err()
        );
    }
}
