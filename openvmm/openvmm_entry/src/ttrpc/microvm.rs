// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM support for the management RPC endpoint.
//!
//! The server converts a microVM request to the command line's options, its
//! command-line view, and creates, captures, and restores the microVM with the
//! command line's microVM code: [`crate::microvm::prepare_restore`],
//! [`MicrovmConfigBuilder`], and [`MicrovmLaunch`]. A request field takes the
//! value of the command-line option that `vmservice.proto` names for it, and
//! that option's rules validate it. What the protocol adds stays here: which
//! fields a request may combine, portb's endpoint on serial port 0, and the
//! refusal of control-console snapshots.

use super::ControllerLaunch;
use super::IommufdContexts;
use super::Vm;
use super::VmLaunch;
use super::VmLifecycle;
use super::grpc_error;
use crate::Options;
use crate::cli_args::DiskCli;
use crate::cli_args::IsolationCli;
use crate::cli_args::NicConfigCli;
use crate::cli_args::OptionalPathBuf;
use crate::cli_args::SerialConfigCli;
use crate::cli_args::microvm::MicrovmLoopbackForwardCli;
use crate::cli_args::microvm::MicrovmMountChildCli;
use crate::cli_args::microvm::MicrovmMountCli;
use crate::cli_args::microvm::MicrovmMountOwnerCli;
use crate::cli_args::microvm::MicrovmNetworkActionCli;
use crate::cli_args::microvm::MicrovmNetworkProfileCli;
use crate::cli_args::microvm::MicrovmSandboxBlockCli;
use crate::cli_args::microvm::SnapshotGenerationIdCli;
use crate::cli_args::microvm::SnapshotScratchRestoreModeCli;
use crate::cli_args::microvm::SnapshotTierCli;
use crate::microvm::MicrovmConfigBuilder;
use crate::microvm::MicrovmLaunch;
use crate::vm_controller::VmControllerEvent;
use anyhow::Context;
use openvmm_defs::config::Config;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::HypervisorConfig;
use openvmm_defs::config::MemoryConfig;
use openvmm_defs::config::NumaNode;
use openvmm_defs::config::NumaTopology;
use openvmm_defs::config::ProcessorTopologyConfig;
use openvmm_defs::config::VirtioBus;
use openvmm_defs::config::VpAssignment;
use openvmm_defs::microvm::MachineProfile;
use openvmm_defs::microvm::MicrovmFilesystemAccess;
use openvmm_defs::microvm::MicrovmFilesystemChildKind;
use openvmm_defs::microvm::MicrovmSandboxBlockRole;
use openvmm_defs::worker::VmWorkerParameters;
use openvmm_ttrpc_vmservice as vmservice;
use pal_async::DefaultDriver;
use std::cell::RefCell;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use vm_manifest_builder::BaseChipsetType;
use vm_manifest_builder::VmManifestBuilder;
use vm_resource::IntoResource;

/// Returns whether `request` creates a microVM: a request of the microVM
/// machine profile, or a restore, whose snapshot selects the profile.
pub(super) fn requested(request: &vmservice::CreateVmRequest) -> anyhow::Result<bool> {
    if request
        .microvm_snapshot
        .as_ref()
        .is_some_and(|snapshot| !snapshot.restore_path.is_empty())
    {
        return Ok(true);
    }
    let Some(config) = &request.config else {
        return Ok(false);
    };
    let profile = vmservice::vm_config::MachineProfile::from_i32(config.machine_profile)
        .with_context(|| format!("unknown machine profile {}", config.machine_profile))?;
    Ok(profile == vmservice::vm_config::MachineProfile::Microvm)
}

/// Rejects the microVM settings of a standard-machine request.
pub(super) fn validate_standard_request(
    request: &vmservice::CreateVmRequest,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        request
            .microvm_snapshot
            .as_ref()
            .is_none_or(|snapshot| *snapshot == Default::default()),
        "microVM snapshot options require the microVM machine profile"
    );
    let Some(config) = &request.config else {
        return Ok(());
    };
    anyhow::ensure!(
        config.cpu_profile.is_empty(),
        "cpu_profile requires the microVM machine profile"
    );
    anyhow::ensure!(
        config
            .memory_config
            .as_ref()
            .is_none_or(|memory| memory.backing_file_path.is_empty()),
        "backing_file_path requires the microVM machine profile"
    );
    anyhow::ensure!(
        config.devices_config.as_ref().is_none_or(|devices| {
            devices.microvm_sandbox_blocks.is_empty() && devices.microvm_network.is_none()
        }),
        "microvm_sandbox_blocks and microvm_network require the microVM machine profile"
    );
    Ok(())
}

/// A microVM request in the command line's terms.
struct MicrovmRequest {
    /// The command-line view of the request.
    opt: Options,
    /// The processor count that a restore asserts.
    restore_processor_count: Option<u32>,
    /// portb's host endpoint, serial port 0.
    portb: Option<vmservice::serial_config::Config>,
    /// The memory configuration of a cold boot, whose MMIO gaps size the
    /// machine's layout.
    memory_config: Option<vmservice::MemoryConfig>,
    guest_power_actions: Option<vmservice::vm_config::GuestPowerActions>,
    crash_dump_path: Option<String>,
}

impl MicrovmRequest {
    /// Converts a microVM request to the command line's options, after
    /// checking which fields the request combines. The command line's rules
    /// validate the options as the microVM is built.
    fn new(request: vmservice::CreateVmRequest) -> anyhow::Result<Self> {
        let vmservice::CreateVmRequest {
            config,
            log_id: _,
            microvm_snapshot,
        } = request;
        let vmservice::MicrovmSnapshotConfig {
            destination_path,
            restore_path,
            restore_ready_path,
            restore_processor_count,
            restore_gate_timeout_ms,
            memory_capacity_bytes,
            restore_memory_bytes,
            snapshot_tier,
            snapshot_generation_id,
            snapshot_scratch_restore_mode,
            restore_generation_id,
            restore_absent_scratch,
            restore_network_policy,
            restore_activation_record_path,
        } = microvm_snapshot.unwrap_or_default();
        let snapshot_destination = absolute_path(destination_path)?;
        let restore_snapshot = absolute_path(restore_path)?;
        let restore = restore_snapshot.is_some();
        anyhow::ensure!(
            snapshot_destination.is_none() || !restore,
            "snapshot capture and restore paths are mutually exclusive"
        );
        for (present, field) in [
            (restore_processor_count != 0, "restore_processor_count"),
            (restore_gate_timeout_ms != 0, "restore_gate_timeout_ms"),
            (restore_memory_bytes != 0, "restore_memory_bytes"),
            (!restore_ready_path.is_empty(), "restore_ready_path"),
            (!restore_generation_id.is_empty(), "restore_generation_id"),
            (restore_absent_scratch, "restore_absent_scratch"),
            (restore_network_policy, "restore_network_policy"),
            (
                !restore_activation_record_path.is_empty(),
                "restore_activation_record_path",
            ),
        ] {
            anyhow::ensure!(restore || !present, "{field} requires restore_path");
        }
        for (present, field) in [
            (memory_capacity_bytes != 0, "memory_capacity_bytes"),
            (snapshot_tier != 0, "snapshot_tier"),
            (!snapshot_generation_id.is_empty(), "snapshot_generation_id"),
            (
                snapshot_scratch_restore_mode != 0,
                "snapshot_scratch_restore_mode",
            ),
        ] {
            anyhow::ensure!(
                snapshot_destination.is_some() || !present,
                "{field} requires destination_path"
            );
        }

        let config = match config {
            Some(config) => config,
            // A restore takes its machine from the snapshot.
            None if restore => vmservice::VmConfig {
                machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                ..Default::default()
            },
            None => anyhow::bail!("missing configuration"),
        };
        let vmservice::VmConfig {
            memory_config,
            processor_config,
            devices_config,
            serial_config,
            boot_config,
            windows_options,
            hvsocket_config,
            numa_config,
            pcie,
            guest_power_actions,
            // A microVM has no SMBIOS tables, Hyper-V enlightenments, or
            // VMBus.
            smbios_config: _,
            disable_vmbus: _,
            disable_hv: _,
            crash_dump_path,
            iommufds,
            isolation_config,
            nested_virt,
            vmgs,
            guest_watchdog,
            machine_profile,
            cpu_profile,
        } = config;
        anyhow::ensure!(
            machine_profile == vmservice::vm_config::MachineProfile::Microvm as i32,
            "microVM snapshot restore requires the matching microVM ABI profile"
        );

        let mut opt = crate::cli_args::microvm_options()?;
        let mut asserted_processor_count = None;
        if restore {
            anyhow::ensure!(
                memory_config.is_none()
                    && boot_config.is_none()
                    && windows_options.is_none()
                    && hvsocket_config.is_none()
                    && numa_config.is_none()
                    && pcie.is_none(),
                "restore configuration may contain only processor identity, serial attachments, devices, and guest power actions"
            );
            if let Some(processors) = processor_config {
                anyhow::ensure!(
                    processors.processor_weight == 0 && processors.processor_limit == 0,
                    "restore processor count does not match the snapshot"
                );
                asserted_processor_count = Some(processors.processor_count);
            }
        } else {
            anyhow::ensure!(
                numa_config.is_none(),
                "microVM does not support custom NUMA topology"
            );
            anyhow::ensure!(pcie.is_none(), "microVM does not support PCIe");
            anyhow::ensure!(
                hvsocket_config.is_none(),
                "microVM does not support hvsocket"
            );
            let Some(vmservice::vm_config::BootConfig::DirectBoot(boot)) = boot_config else {
                anyhow::bail!("the microVM profile requires direct_boot");
            };
            let vmservice::DirectBoot {
                kernel_path,
                initrd_path,
                kernel_cmdline,
            } = boot;
            opt.kernel = OptionalPathBuf(Some(kernel_path.into()));
            opt.initrd = OptionalPathBuf((!initrd_path.is_empty()).then(|| initrd_path.into()));
            opt.cmdline = if kernel_cmdline.is_empty() {
                Vec::new()
            } else {
                vec![kernel_cmdline]
            };
            let memory = memory_config
                .as_ref()
                .context("missing memory configuration")?;
            opt.memory.size = Some(vmm_cli::MemorySize(
                memory
                    .memory_mb
                    .checked_mul(0x100000)
                    .context("invalid memory configuration")?,
            ));
            opt.memory.file = (!memory.backing_file_path.is_empty())
                .then(|| memory.backing_file_path.clone().into());
            opt.processors = processor_config.map_or(1, |processors| processors.processor_count);
        }
        anyhow::ensure!(
            iommufds.is_empty(),
            "microVM ABI version 1 does not support iommufd contexts"
        );
        anyhow::ensure!(vmgs.is_none(), "microVM does not support VMGS");
        if let Some(isolation) = isolation_config {
            match isolation
                .isolation_type
                .context("missing or unsupported isolation type")?
            {
                vmservice::isolation_config::IsolationType::None(_) => {}
                vmservice::isolation_config::IsolationType::Snp(_) => {
                    opt.isolation = Some(IsolationCli::Snp);
                }
            }
        }
        opt.nested_virt = nested_virt;
        opt.guest_watchdog = guest_watchdog;

        let mut ports = serial_config.into_iter().flat_map(|config| config.ports);
        let portb = ports.next();
        anyhow::ensure!(
            ports.next().is_none() && portb.as_ref().is_none_or(|port| port.port == 0),
            "microVM accepts only serial port 0 as its portb endpoint"
        );

        if let Some(devices) = devices_config {
            let vmservice::DevicesConfig {
                scsi_disks,
                vpmem_disks,
                nic_config,
                windows_device,
                virtiofs_config,
                virtio_consoles,
                microvm_sandbox_blocks,
                microvm_network,
            } = devices;
            anyhow::ensure!(
                scsi_disks.is_empty()
                    && vpmem_disks.is_empty()
                    && nic_config.is_empty()
                    && windows_device.is_empty()
                    && virtiofs_config.len() <= 1
                    && virtio_consoles.len() <= 1,
                "microVM supports only one fixed virtio-fs and one fixed virtio-console device besides its sandbox blocks and network device"
            );
            if let Some(filesystem) = virtiofs_config.into_iter().next() {
                set_filesystem_options(&mut opt, filesystem)?;
            }
            if let Some(console) = virtio_consoles.into_iter().next() {
                opt.virtio_console = Some(console_option(console)?);
            }
            opt.microvm.microvm_sandbox_block = microvm_sandbox_blocks
                .into_iter()
                .map(sandbox_block_option)
                .collect::<anyhow::Result<_>>()?;
            if let Some(network) = microvm_network {
                set_network_options(&mut opt, network)?;
            }
        }

        opt.restore_snapshot = restore_snapshot;
        opt.restore_ready_path =
            (!restore_ready_path.is_empty()).then(|| restore_ready_path.into());
        opt.microvm.snapshot_destination = snapshot_destination;
        opt.microvm.restore_processors =
            (restore_processor_count != 0).then_some(restore_processor_count);
        opt.microvm.restore_memory =
            (restore_memory_bytes != 0).then_some(vmm_cli::MemorySize(restore_memory_bytes));
        // Zero keeps the command line's default.
        if restore_gate_timeout_ms != 0 {
            opt.microvm.restore_gate_timeout_ms = restore_gate_timeout_ms;
        }
        opt.microvm.memory_capacity =
            (memory_capacity_bytes != 0).then_some(vmm_cli::MemorySize(memory_capacity_bytes));
        opt.microvm.snapshot_tier = snapshot_tier_option(snapshot_tier)?;
        opt.microvm.snapshot_generation_id =
            generation_id_option(&snapshot_generation_id, "snapshot_generation_id")?;
        opt.microvm.snapshot_scratch_restore_mode =
            scratch_restore_mode_option(snapshot_scratch_restore_mode)?;
        opt.microvm.restore_generation_id =
            generation_id_option(&restore_generation_id, "restore_generation_id")?;
        opt.microvm.restore_absent_scratch = restore_absent_scratch;
        opt.microvm.restore_network_policy = restore_network_policy;
        opt.microvm.restore_activation_record = absolute_path(restore_activation_record_path)?;
        opt.microvm.cpu_profile = (!cpu_profile.is_empty()).then_some(cpu_profile);

        Ok(Self {
            opt,
            restore_processor_count: asserted_processor_count,
            portb,
            memory_config,
            guest_power_actions,
            crash_dump_path,
        })
    }
}

/// Resolves a request path relative to the server's working directory, or
/// returns `None` for an empty path.
fn absolute_path(value: String) -> anyhow::Result<Option<PathBuf>> {
    if value.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(value);
    Ok(Some(if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .context("failed to resolve current directory")?
            .join(path)
    }))
}

/// Parses `value` as the command line parses the value of an option, naming
/// the request's `field` in the error.
fn parse_option_value<T>(value: &str, field: &str) -> anyhow::Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|error| anyhow::anyhow!("invalid {field} '{value}': {error}"))
}

/// Parses each of `values` with [`parse_option_value`].
fn parse_option_values<T>(values: Vec<String>, field: &str) -> anyhow::Result<Vec<T>>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    values
        .iter()
        .map(|value| parse_option_value(value, field))
        .collect()
}

/// Returns the `--virtio-console` value of `console`: a listener or, with
/// `connect`, a client at its Unix socket or named pipe, or, without a path,
/// `none`, the console with no endpoint. A request names no other endpoint,
/// so it never selects a TCP endpoint, which a microVM console refuses.
fn console_option(console: vmservice::VirtioConsoleConfig) -> anyhow::Result<SerialConfigCli> {
    let vmservice::VirtioConsoleConfig {
        socket_path,
        connect,
    } = console;
    if socket_path.is_empty() {
        anyhow::ensure!(
            !connect,
            "a microVM virtio-console client requires a socket path"
        );
        return Ok(SerialConfigCli::None);
    }
    let path = PathBuf::from(socket_path);
    Ok(if connect {
        SerialConfigCli::ConnectPipe(path)
    } else {
        SerialConfigCli::Pipe(path)
    })
}

/// Returns the `--microvm-sandbox-block` value of `block`.
fn sandbox_block_option(
    block: vmservice::MicrovmSandboxBlock,
) -> anyhow::Result<MicrovmSandboxBlockCli> {
    use vmservice::microvm_sandbox_block::Role;

    let vmservice::MicrovmSandboxBlock {
        role,
        backend,
        read_only,
    } = block;
    let role = match Role::from_i32(role)
        .with_context(|| format!("unknown microVM sandbox block role {role}"))?
    {
        Role::Unspecified => anyhow::bail!("a microVM sandbox block requires a role"),
        Role::Distro => MicrovmSandboxBlockRole::Distro,
        Role::Scratch => MicrovmSandboxBlockRole::Scratch,
    };
    let kind = super::disk_backend_from_proto(
        backend.context("a microVM sandbox block requires a backend")?,
    )?;
    Ok(MicrovmSandboxBlockCli {
        role,
        disk: DiskCli {
            vtl: DeviceVtl::Vtl0,
            kind,
            read_only,
            is_dvd: false,
            underhill: None,
            pcie_port: None,
            serial: None,
            controller: None,
            nsid: None,
            lun: None,
            relay: None,
        },
    })
}

/// Sets the `--net` and microVM network options of `network`.
fn set_network_options(
    opt: &mut Options,
    network: vmservice::MicrovmNetwork,
) -> anyhow::Result<()> {
    use vmservice::microvm_network::Profile;

    let vmservice::MicrovmNetwork {
        address,
        profile,
        egress,
        ingress,
        egress_allow,
        egress_deny,
        host_loopback,
        proxy,
        host_loopback_forwards,
        allow_hosts,
        block_hosts,
        allow_endpoints,
    } = network;
    if !address.is_empty() {
        opt.net
            .push(parse_option_value::<NicConfigCli>(&address, "address")?);
    }
    opt.microvm.network_profile = match Profile::from_i32(profile)
        .with_context(|| format!("unknown microVM network profile {profile}"))?
    {
        Profile::Unspecified => None,
        Profile::Portable => Some(MicrovmNetworkProfileCli::Portable),
    };
    opt.microvm.network_egress = network_action_option(egress, "egress")?;
    opt.microvm.network_ingress = network_action_option(ingress, "ingress")?;
    opt.microvm.host_loopback = network_action_option(host_loopback, "host_loopback")?;
    opt.microvm.network_egress_allow = parse_option_values(egress_allow, "egress_allow")?;
    opt.microvm.network_egress_deny = parse_option_values(egress_deny, "egress_deny")?;
    opt.microvm.network_proxy = (!proxy.is_empty())
        .then(|| parse_option_value(&proxy, "proxy"))
        .transpose()?;
    opt.microvm.host_loopback_forward = host_loopback_forwards
        .into_iter()
        .map(host_loopback_forward_option)
        .collect::<anyhow::Result<_>>()?;
    opt.microvm.allow_host = parse_option_values(allow_hosts, "allow_hosts")?;
    opt.microvm.block_host = parse_option_values(block_hosts, "block_hosts")?;
    opt.microvm.allow_endpoint = parse_option_values(allow_endpoints, "allow_endpoints")?;
    anyhow::ensure!(
        [
            opt.microvm.allow_host.is_empty(),
            opt.microvm.block_host.is_empty(),
            opt.microvm.allow_endpoint.is_empty(),
        ]
        .into_iter()
        .filter(|empty| !empty)
        .count()
            <= 1,
        "allow_hosts, block_hosts, and allow_endpoints are mutually exclusive"
    );
    Ok(())
}

/// Returns the value of a microVM network action option, or `None` when the
/// request leaves the option absent.
fn network_action_option(
    action: i32,
    field: &str,
) -> anyhow::Result<Option<MicrovmNetworkActionCli>> {
    use vmservice::microvm_network::Action;

    Ok(
        match Action::from_i32(action)
            .with_context(|| format!("unknown microVM network {field} action {action}"))?
        {
            Action::Unspecified => None,
            Action::Allow => Some(MicrovmNetworkActionCli::Allow),
            Action::Deny => Some(MicrovmNetworkActionCli::Deny),
        },
    )
}

/// Returns the `--host-loopback-forward` value of `forward`, which the command
/// line's parser checks.
fn host_loopback_forward_option(
    forward: vmservice::microvm_network::HostLoopbackForward,
) -> anyhow::Result<MicrovmLoopbackForwardCli> {
    use vmservice::microvm_network::host_loopback_forward::Protocol;

    let vmservice::microvm_network::HostLoopbackForward {
        protocol,
        host_port,
        guest_port,
    } = forward;
    let protocol = match Protocol::from_i32(protocol)
        .with_context(|| format!("unknown host loopback forward protocol {protocol}"))?
    {
        Protocol::Unspecified => anyhow::bail!("a host loopback forward requires a protocol"),
        Protocol::Tcp => "tcp",
        Protocol::Udp => "udp",
    };
    parse_option_value(
        &format!("{protocol}:{host_port}:{guest_port}"),
        "host_loopback_forwards",
    )
}

/// Sets the `--mount`, or `--mount-aggregate` and `--mount-child`, options of
/// `filesystem`, with its access policy and owner.
fn set_filesystem_options(
    opt: &mut Options,
    filesystem: vmservice::VirtioFs,
) -> anyhow::Result<()> {
    use vmservice::virtio_fs::Owner;

    let vmservice::VirtioFs {
        tag,
        root_path,
        read_only,
        guest_mount_target,
        read_write,
        denied_paths,
        allowed_paths,
        writable_paths,
        owner,
        aggregate_children,
    } = filesystem;
    let access = |read_write| {
        if read_write {
            MicrovmFilesystemAccess::ReadWrite
        } else {
            MicrovmFilesystemAccess::ReadOnly
        }
    };
    anyhow::ensure!(
        !read_only,
        "microVM virtio-fs uses read_write rather than read_only"
    );
    if aggregate_children.is_empty() {
        anyhow::ensure!(
            tag == "microvm" && !root_path.is_empty(),
            "microVM virtio-fs requires tag 'microvm' and a host root path"
        );
        opt.microvm.microvm_mount = Some(MicrovmMountCli {
            guest_target: guest_mount_target,
            host_path: root_path.into(),
            access: access(read_write),
        });
    } else {
        anyhow::ensure!(
            tag == "microvm" && root_path.is_empty() && !read_write,
            "an aggregate microVM virtio-fs requires tag 'microvm' and no host root path, and its root is read-only"
        );
        opt.microvm.microvm_mount_aggregate = Some(guest_mount_target);
        opt.microvm.microvm_mount_child = aggregate_children
            .into_iter()
            .map(|child| {
                let vmservice::virtio_fs::AggregateChild {
                    name,
                    host_path,
                    read_write,
                    file,
                } = child;
                anyhow::ensure!(
                    !host_path.is_empty(),
                    "microVM aggregate child '{name}' requires a host path"
                );
                Ok(MicrovmMountChildCli {
                    name,
                    host_path: host_path.into(),
                    access: access(read_write),
                    kind: if file {
                        MicrovmFilesystemChildKind::File
                    } else {
                        MicrovmFilesystemChildKind::Directory
                    },
                })
            })
            .collect::<anyhow::Result<_>>()?;
    }
    opt.microvm.microvm_mount_deny = denied_paths.into_iter().map(PathBuf::from).collect();
    opt.microvm.microvm_mount_allow = allowed_paths.into_iter().map(PathBuf::from).collect();
    opt.microvm.microvm_mount_write = writable_paths.into_iter().map(PathBuf::from).collect();
    opt.microvm.microvm_mount_owner = match Owner::from_i32(owner)
        .with_context(|| format!("unknown microVM virtio-fs owner {owner}"))?
    {
        Owner::Unspecified => None,
        Owner::Vmm => Some(MicrovmMountOwnerCli::Vmm),
        Owner::Caller => Some(MicrovmMountOwnerCli::Caller),
    };
    Ok(())
}

/// Returns the `--snapshot-tier` value of `tier`, or `None` without a tier.
fn snapshot_tier_option(tier: i32) -> anyhow::Result<Option<SnapshotTierCli>> {
    use vmservice::microvm_snapshot_config::SnapshotTier;

    Ok(
        match SnapshotTier::from_i32(tier)
            .with_context(|| format!("unknown microVM snapshot tier {tier}"))?
        {
            SnapshotTier::Unspecified => None,
            SnapshotTier::Platform => Some(SnapshotTierCli::Platform),
            SnapshotTier::WorkloadStart => Some(SnapshotTierCli::WorkloadStart),
            SnapshotTier::InstanceCheckpoint => Some(SnapshotTierCli::InstanceCheckpoint),
        },
    )
}

/// Returns the `--snapshot-scratch-restore-mode` value of `mode`, or `None`
/// for the default.
fn scratch_restore_mode_option(mode: i32) -> anyhow::Result<Option<SnapshotScratchRestoreModeCli>> {
    use vmservice::microvm_snapshot_config::ScratchRestoreMode;

    Ok(
        match ScratchRestoreMode::from_i32(mode)
            .with_context(|| format!("unknown microVM scratch restore mode {mode}"))?
        {
            ScratchRestoreMode::Unspecified => None,
            ScratchRestoreMode::PrivateCopy => Some(SnapshotScratchRestoreModeCli::PrivateCopy),
            ScratchRestoreMode::CopyOnWrite => Some(SnapshotScratchRestoreModeCli::CopyOnWrite),
            ScratchRestoreMode::DirectClaimed => Some(SnapshotScratchRestoreModeCli::DirectClaimed),
        },
    )
}

/// Returns the storage generation of `generation`, or `None` when it is
/// empty, with the command line's rules for its value.
fn generation_id_option(
    generation: &[u8],
    field: &str,
) -> anyhow::Result<Option<SnapshotGenerationIdCli>> {
    (!generation.is_empty())
        .then(|| {
            SnapshotGenerationIdCli::try_from(generation)
                .with_context(|| format!("invalid {field}"))
        })
        .transpose()
}

/// Creates the microVM that `request` describes from its command-line view,
/// as the command line creates, captures, and restores the microVM of those
/// options, and returns its launch.
pub(super) async fn create(
    driver: &DefaultDriver,
    request: vmservice::CreateVmRequest,
) -> anyhow::Result<VmLaunch> {
    let MicrovmRequest {
        mut opt,
        restore_processor_count,
        portb,
        memory_config,
        guest_power_actions,
        crash_dump_path,
    } = MicrovmRequest::new(request)?;

    let mut restore = crate::snapshot_restore::SnapshotRestore::open(&opt)?;
    if let Some(snapshot) = restore.snapshot() {
        let manifest = snapshot.manifest();
        if let Some(count) = restore_processor_count {
            anyhow::ensure!(
                count == manifest.vp_count,
                "restore processor count does not match the snapshot"
            );
        }
        // The snapshot sets the processor count, which the command line's
        // `--processors` must repeat.
        opt.processors = manifest.vp_count;
        if let Some(contract) = &manifest.machine_contract {
            validate_restore_console_attachments(&contract.attachments)?;
        }
    }
    let microvm_restore = crate::microvm::prepare_restore(&mut opt, restore.snapshot())?;
    let hypervisor = openvmm_helpers::hypervisor::microvm::choose_microvm_hypervisor()?;
    let source_hypervisor = hypervisor.id().to_owned();

    let mut builder = MicrovmConfigBuilder::new(&opt, &microvm_restore)?;
    let portb = portb
        .map(|port| {
            let (serial_fn, action) = super::open_socket_backend(port.connect);
            serial_fn(port.socket_path.as_ref()).with_context(|| {
                format!("failed to {} serial socket: {}", action, port.socket_path)
            })
        })
        .transpose()?;
    builder.set_portb_endpoint(portb);
    let console = builder.setup_virtio_consoles(&RefCell::new(None), driver)?;
    let mut storage = crate::storage_builder::StorageBuilder::new(None);
    builder.add_sandbox_blocks(&mut storage).await?;

    #[cfg(guest_arch = "aarch64")]
    let arch = vm_manifest_builder::MachineArch::Aarch64;
    #[cfg(guest_arch = "x86_64")]
    let arch = vm_manifest_builder::MachineArch::X86_64;
    let chipset_builder = VmManifestBuilder::new(BaseChipsetType::Microvm, arch);
    let layout =
        super::apply_mmio_overrides(chipset_builder.layout_config(), memory_config.as_ref())?;
    let mut chipset = chipset_builder
        .build()
        .context("failed to build vm configuration")?;
    builder.add_chipset_devices(&mut chipset.chipset_devices)?;
    let load_mode = builder
        .load_mode(arch)?
        .context("the microVM profile requires a load mode")?;

    let memory = opt.memory_size();
    let mut config = Config {
        machine_profile: MachineProfile::Microvm,
        microvm: Default::default(),
        load_mode,
        ide_disks: Vec::new(),
        floppy_disks: Vec::new(),
        pcie_root_complexes: Vec::new(),
        pcie_ecam_below_4gb: false,
        pcie_devices: Vec::new(),
        pcie_switches: Vec::new(),
        pcie_generic_initiators: Vec::new(),
        vpci_devices: Vec::new(),
        numa: NumaTopology {
            nodes: vec![NumaNode {
                mem: Some(MemoryConfig {
                    mem_size: memory,
                    prefetch_memory: opt.prefetch_memory(),
                    private_memory: opt.private_memory(),
                    transparent_hugepages: opt.transparent_hugepages(),
                    hugepages: opt.memory.hugepages,
                    hugepage_size: opt.memory.hugepage_size.map(|size| size.0),
                    host_numa_node: None,
                }),
                vps: VpAssignment::FromTopology,
            }],
            distances: Vec::new(),
        },
        chipset: chipset.chipset,
        processor_topology: ProcessorTopologyConfig {
            proc_count: opt.processors,
            vps_per_socket: None,
            enable_smt: None,
            arch: None,
        },
        hypervisor: HypervisorConfig::default(),
        #[cfg(windows)]
        kernel_vmnics: Vec::new(),
        input: mesh::Receiver::new(),
        framebuffer: None,
        vga_firmware: None,
        vtl2_gfx: false,
        virtio_devices: Vec::new(),
        vmbus: None,
        vtl2_vmbus: None,
        vmbus_devices: Vec::new(),
        #[cfg(windows)]
        vpci_resources: Vec::new(),
        vmgs: None,
        firmware_event_send: None,
        debugger_rpc: None,
        chipset_devices: chipset.chipset_devices,
        pci_chipset_devices: chipset.pci_chipset_devices,
        isa_dma_controller: chipset.isa_dma_controller,
        chipset_capabilities: chipset.capabilities,
        layout,
        rtc_delta_milliseconds: 0,
    };
    // The command line orders the devices so: its fixed-slot network and
    // filesystem devices, the boot console, and then the sandbox blocks.
    builder.add_virtio_devices(&mut |_, resource| {
        config.virtio_devices.push((VirtioBus::Mmio, resource))
    })?;
    if let Some(backend) = console {
        config.virtio_devices.push((
            VirtioBus::Mmio,
            builder.virtio_console_handle(backend).into_resource(),
        ));
    }
    let mut resources = crate::microvm::MicrovmResources::default();
    storage.build_microvm_sandbox_blocks(&mut config, &mut resources)?;
    builder.finish(&mut config, &mut resources)?;

    let mut launch = MicrovmLaunch::new(&opt, &config, resources, microvm_restore)?;
    let (snapshot_boundary_requests, snapshot_ready, snapshot_requests) =
        launch.snapshot_channels();
    let (shared_memory, saved_state) = if opt.restore_snapshot.is_some() {
        let (shared_memory, saved_state) =
            restore.prepare(&opt, &mut launch, &source_hypervisor)?;
        (Some(shared_memory), Some(saved_state))
    } else if let Some(shared_memory) = launch.capture_shared_memory()? {
        (Some(shared_memory), None)
    } else {
        let shared_memory = opt
            .memory_backing_file()
            .map(|path| openvmm_helpers::shared_memory::open_memory_backing_file(path, memory))
            .transpose()?;
        (shared_memory, None)
    };
    let worker_restore = restore.into_worker();
    let restore_ready_sink = crate::snapshot_restore::restore_ready_sink(&opt)?;
    let guest_power_actions = super::guest_power_actions_from_proto(guest_power_actions)?;
    let restore_gate_timeout = launch.restore_gate_timeout(&opt);
    let memory_backing_file = launch.memory_backing_file(&opt);
    let microvm = launch.into_controller(&opt, source_hypervisor, snapshot_requests);
    let (worker_rpc, rpc) = mesh::channel();
    let (notify_send, notify) = mesh::channel();
    Ok(VmLaunch {
        worker: VmWorkerParameters {
            hypervisor,
            cfg: config,
            saved_state,
            shared_memory,
            shared_memory_copy_on_write: worker_restore.shared_memory_copy_on_write,
            snapshot_restore_guards: worker_restore.guards,
            snapshot_boundary_requests,
            snapshot_ready,
            restore_time: worker_restore.time,
            restore_ready_sink,
            restore_gate_timeout,
            restore_vp_count: opt.microvm.restore_processors,
            rpc,
            notify: notify_send,
        },
        notify,
        vm: Vm {
            worker_rpc,
            shutdown_rpc: None,
            acpi_power_button_rpc: None,
            scsi_rpc: None,
            consomme_rpc: Default::default(),
            iommufds: Arc::new(IommufdContexts::default()),
        },
        controller: ControllerLaunch {
            igvm_path: None,
            memory_backing_file,
            memory,
            processors: opt.processors,
            crash_dump_path: crash_dump_path.map(Into::into),
            microvm,
            guest_power_actions,
        },
    })
}

/// Handles controller events emitted by the guest-exit path.
///
/// A guest exit marks the VM halted, completes a pending `WaitVM`, successfully
/// only for status 0, and returns the status, with which the process exits as
/// it does on the command line. A failed exit fails a pending `WaitVM` and
/// returns the error that ends the server.
pub(super) fn handle_exit_event(
    event: VmControllerEvent,
    lifecycle: &mut VmLifecycle,
    wait_vm_response: &mut Option<(
        mesh::CancelContext,
        mesh::OneshotSender<Result<(), mesh_rpc::service::Status>>,
    )>,
) -> anyhow::Result<Option<i32>> {
    match event {
        VmControllerEvent::ExitRequested { code } => {
            let reason = format!("guest exited with status {code}");
            tracing::info!(code, "guest halted with process status");
            *lifecycle = VmLifecycle::Halted(reason.clone());
            if let Some((_, response)) = wait_vm_response.take() {
                response.send(if code == 0 {
                    Ok(())
                } else {
                    Err(grpc_error(anyhow::anyhow!(reason)))
                });
            }
            Ok(Some(code))
        }
        VmControllerEvent::ExitFailed { error } => {
            *lifecycle = VmLifecycle::Halted(error.clone());
            if let Some((_, response)) = wait_vm_response.take() {
                response.send(Err(grpc_error(anyhow::anyhow!(error.clone()))));
            }
            Err(anyhow::anyhow!(error))
        }
        _ => unreachable!("only guest exit events are delegated"),
    }
}

fn validate_restore_console_attachments(
    attachments: &[openvmm_helpers::snapshot::microvm::SnapshotAttachment],
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !attachments.iter().any(|attachment| {
            attachment.stable_id == crate::microvm::MICROVM_CONTROL_CONSOLE_STABLE_ID
                || attachment.kind == crate::microvm::MICROVM_CONTROL_CONSOLE_ATTACHMENT_KIND
        }),
        "OpenVMM management RPC cannot restore control-console snapshots before authenticated broker activation"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use clap::Parser as _;
    use test_with_tracing::test;

    #[test]
    fn rpc_service_reports_guest_exit_status() {
        DefaultPool::run_with(async |driver| {
            for (event, expected_result, expected_response) in [
                (VmControllerEvent::ExitRequested { code: 0 }, Ok(0), Ok(())),
                (
                    VmControllerEvent::ExitRequested { code: 1 },
                    Ok(1),
                    Err("guest exited with status 1"),
                ),
                (
                    VmControllerEvent::ExitFailed {
                        error: "console output drain timed out".to_owned(),
                    },
                    Err("console output drain timed out"),
                    Err("console output drain timed out"),
                ),
            ] {
                let directory = tempfile::tempdir().unwrap();
                let listener = UnixListener::bind(directory.path().join("rpc")).unwrap();
                let (events, event_recv) = mesh::channel();
                let (worker_send, worker_recv) = mesh::channel();
                let (response, received) = mesh::oneshot();
                let mut service = VmService {
                    driver: driver.clone(),
                    vm: None,
                    vm_controller: None,
                    vm_controller_events: Some(event_recv),
                    controller_task: None,
                    wait_vm_response: Some((mesh::CancelContext::new(), response)),
                    lifecycle: VmLifecycle::Running,
                    restore_ready_pending: false,
                    rpc_tasks: Vec::new(),
                    transport: ResolvedTransport::Auto,
                    registry: FdRegistry::default(),
                };
                events.send(event);
                let result = mesh::CancelContext::new()
                    .with_timeout(Duration::from_secs(5))
                    .until_cancelled(service.run(listener, worker_recv))
                    .await
                    .expect("guest exit must stop the RPC service");
                drop(worker_send);
                let response = received.await.unwrap();
                match expected_result {
                    Ok(status) => assert_eq!(result.unwrap(), status),
                    Err(expected_error) => {
                        assert!(result.unwrap_err().to_string().contains(expected_error));
                    }
                }
                match expected_response {
                    Ok(()) => response.unwrap(),
                    Err(expected_error) => {
                        assert!(response.unwrap_err().message.contains(expected_error));
                    }
                }
                assert!(matches!(service.lifecycle, VmLifecycle::Halted(_)));
            }
        });
    }

    /// Asserts that `message` encodes as `encoded`, and decodes from it.
    macro_rules! assert_wire {
        ($message:expr, $encoded:expr $(,)?) => {{
            fn same_type<T>(_: &T, value: T) -> T {
                value
            }
            let message = $message;
            let encoded: &[u8] = $encoded;
            assert_eq!(mesh::payload::encode(message.clone()), encoded);
            let decoded = same_type(&message, mesh::payload::decode(encoded).unwrap());
            assert_eq!(decoded, message);
        }};
    }

    #[test]
    fn ttrpc_fork_fields_follow_upstream_wire_fields() {
        // Upstream's fields keep upstream's numbers, and the fork's follow
        // them: `machine_profile` is field 23 and `cpu_profile` field 24,
        // after upstream's `guest_watchdog` field 22.
        assert_wire!(
            vmservice::VmConfig {
                crash_dump_path: Some("dump".to_owned()),
                ..Default::default()
            },
            b"\x72\x04dump",
        );
        assert_wire!(
            vmservice::VmConfig {
                disable_vmbus: true,
                ..Default::default()
            },
            b"\x80\x01\x01",
        );
        assert_wire!(
            vmservice::VmConfig {
                machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                ..Default::default()
            },
            b"\xb8\x01\x01",
        );
        assert_wire!(
            vmservice::VmConfig {
                cpu_profile: "host".to_owned(),
                ..Default::default()
            },
            b"\xc2\x01\x04host",
        );

        // `read_only` is upstream's field 3, and the microVM's mount fields
        // follow it: the mount target and access mode take fields 4 and 5,
        // and the access policy, owner, and aggregate children 6 to 10.
        assert_wire!(
            vmservice::VirtioFs {
                read_only: true,
                guest_mount_target: "/share".to_owned(),
                read_write: true,
                ..Default::default()
            },
            b"\x18\x01\x22\x06/share\x28\x01",
        );
        assert_wire!(
            vmservice::VirtioFs {
                denied_paths: vec!["d".to_owned()],
                allowed_paths: vec!["a".to_owned()],
                writable_paths: vec!["w".to_owned()],
                owner: vmservice::virtio_fs::Owner::Caller as i32,
                aggregate_children: vec![vmservice::virtio_fs::AggregateChild {
                    name: "c".to_owned(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            b"\x32\x01d\x3a\x01a\x42\x01w\x48\x02\x52\x03\x0a\x01c",
        );

        // The RAM backing file follows upstream's nine `MemoryConfig` fields,
        // and the sandbox blocks and network follow upstream's six
        // `DevicesConfig` fields.
        assert_wire!(
            vmservice::MemoryConfig {
                backing_file_path: "ram".to_owned(),
                ..Default::default()
            },
            b"\x52\x03ram",
        );
        assert_wire!(
            vmservice::DevicesConfig {
                microvm_sandbox_blocks: vec![vmservice::MicrovmSandboxBlock {
                    role: vmservice::microvm_sandbox_block::Role::Distro as i32,
                    ..Default::default()
                }],
                microvm_network: Some(vmservice::MicrovmNetwork {
                    profile: vmservice::microvm_network::Profile::Portable as i32,
                    ..Default::default()
                }),
                ..Default::default()
            },
            b"\x3a\x02\x08\x01\x42\x02\x10\x01",
        );

        // The sandbox block and network settings of a capture or restore
        // follow the snapshot fields 1 to 8, of which 3 is reserved.
        let mut encoded = b"\x48\x02\x52\x10".to_vec();
        encoded.extend([0x11; 16]);
        encoded.extend(b"\x58\x03\x62\x10");
        encoded.extend([0x22; 16]);
        encoded.extend(b"\x68\x01\x70\x01");
        assert_wire!(
            vmservice::MicrovmSnapshotConfig {
                snapshot_tier: vmservice::microvm_snapshot_config::SnapshotTier::WorkloadStart
                    as i32,
                snapshot_generation_id: vec![0x11; 16],
                snapshot_scratch_restore_mode:
                    vmservice::microvm_snapshot_config::ScratchRestoreMode::DirectClaimed as i32,
                restore_generation_id: vec![0x22; 16],
                restore_absent_scratch: true,
                restore_network_policy: true,
                ..Default::default()
            },
            &encoded,
        );
    }

    /// Returns the command-line view of `request`.
    fn command_line_view(request: vmservice::CreateVmRequest) -> anyhow::Result<Options> {
        Ok(MicrovmRequest::new(request)?.opt)
    }

    /// Parses the command line `arguments` of `openvmm`.
    fn command_line(arguments: &[&str]) -> Options {
        Options::try_parse_from(std::iter::once("openvmm").chain(arguments.iter().copied()))
            .unwrap()
    }

    /// Asserts that `rpc` holds the microVM options of `cli`.
    fn assert_same_microvm_options(rpc: &Options, cli: &Options) {
        assert_eq!(rpc.machine, cli.machine);
        assert_eq!(rpc.processors, cli.processors);
        assert_eq!(rpc.memory, cli.memory);
        assert_eq!(rpc.kernel.0, cli.kernel.0);
        assert_eq!(rpc.initrd.0, cli.initrd.0);
        assert_eq!(rpc.cmdline, cli.cmdline);
        assert_eq!(rpc.restore_snapshot, cli.restore_snapshot);
        assert_eq!(rpc.restore_ready_path, cli.restore_ready_path);
        assert_eq!(rpc.virtio_console, cli.virtio_console);
        assert_eq!(rpc.net, cli.net);
        assert_eq!(
            format!("{:?}", rpc.isolation),
            format!("{:?}", cli.isolation)
        );
        assert_eq!(rpc.nested_virt, cli.nested_virt);
        assert_eq!(rpc.guest_watchdog, cli.guest_watchdog);
        let (rpc, cli) = (&rpc.microvm, &cli.microvm);
        assert_eq!(rpc.restore_processors, cli.restore_processors);
        assert_eq!(rpc.restore_memory, cli.restore_memory);
        assert_eq!(rpc.restore_absent_scratch, cli.restore_absent_scratch);
        assert_eq!(rpc.restore_network_policy, cli.restore_network_policy);
        assert_eq!(rpc.restore_activation_record, cli.restore_activation_record);
        assert_eq!(rpc.restore_generation_id, cli.restore_generation_id);
        assert_eq!(rpc.restore_gate_timeout_ms, cli.restore_gate_timeout_ms);
        assert_eq!(rpc.snapshot_destination, cli.snapshot_destination);
        assert_eq!(rpc.memory_capacity, cli.memory_capacity);
        assert_eq!(rpc.snapshot_tier, cli.snapshot_tier);
        assert_eq!(rpc.snapshot_generation_id, cli.snapshot_generation_id);
        assert_eq!(
            rpc.snapshot_scratch_restore_mode,
            cli.snapshot_scratch_restore_mode
        );
        assert_eq!(
            rpc.microvm_sandbox_block.len(),
            cli.microvm_sandbox_block.len()
        );
        for (rpc, cli) in rpc
            .microvm_sandbox_block
            .iter()
            .zip(&cli.microvm_sandbox_block)
        {
            assert_eq!(rpc.role, cli.role);
            assert_eq!(rpc.disk.vtl, cli.disk.vtl);
            assert_eq!(rpc.disk.kind, cli.disk.kind);
            assert_eq!(rpc.disk.read_only, cli.disk.read_only);
        }
        assert_eq!(rpc.network_profile, cli.network_profile);
        assert_eq!(rpc.network_egress, cli.network_egress);
        assert_eq!(rpc.network_ingress, cli.network_ingress);
        assert_eq!(rpc.network_egress_allow, cli.network_egress_allow);
        assert_eq!(rpc.network_egress_deny, cli.network_egress_deny);
        assert_eq!(rpc.host_loopback, cli.host_loopback);
        assert_eq!(rpc.network_proxy, cli.network_proxy);
        assert_eq!(rpc.host_loopback_forward, cli.host_loopback_forward);
        assert_eq!(rpc.allow_host, cli.allow_host);
        assert_eq!(rpc.block_host, cli.block_host);
        assert_eq!(rpc.allow_endpoint, cli.allow_endpoint);
        assert_eq!(rpc.microvm_mount, cli.microvm_mount);
        assert_eq!(rpc.microvm_mount_aggregate, cli.microvm_mount_aggregate);
        assert_eq!(rpc.microvm_mount_child, cli.microvm_mount_child);
        assert_eq!(rpc.microvm_mount_deny, cli.microvm_mount_deny);
        assert_eq!(rpc.microvm_mount_allow, cli.microvm_mount_allow);
        assert_eq!(rpc.microvm_mount_write, cli.microvm_mount_write);
        assert_eq!(rpc.microvm_mount_owner, cli.microvm_mount_owner);
        assert_eq!(rpc.cpu_profile, cli.cpu_profile);
    }

    /// Returns a `FileDisk` backend at `path`.
    fn file_disk(path: &str) -> Option<vmservice::DiskBackend> {
        Some(vmservice::DiskBackend {
            kind: Some(vmservice::disk_backend::Kind::File(vmservice::FileDisk {
                path: path.to_owned(),
                ..Default::default()
            })),
        })
    }

    /// Returns a cold-boot microVM request that captures a snapshot to
    /// `destination` with every device setting, and the command line that
    /// requests the same.
    fn capture_request(destination: &str) -> (vmservice::CreateVmRequest, Vec<String>) {
        use vmservice::microvm_network::Action;
        use vmservice::microvm_snapshot_config::ScratchRestoreMode;
        use vmservice::microvm_snapshot_config::SnapshotTier;

        let share = std::env::temp_dir().join("share");
        let share = share.to_str().unwrap();
        let path = |name: &str| format!("{share}/{name}");
        let request = vmservice::CreateVmRequest {
            config: Some(vmservice::VmConfig {
                machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                memory_config: Some(vmservice::MemoryConfig {
                    memory_mb: 256,
                    backing_file_path: "ram.bin".to_owned(),
                    ..Default::default()
                }),
                processor_config: Some(vmservice::ProcessorConfig {
                    processor_count: 2,
                    ..Default::default()
                }),
                boot_config: Some(vmservice::vm_config::BootConfig::DirectBoot(
                    vmservice::DirectBoot {
                        kernel_path: "vmlinux".to_owned(),
                        initrd_path: "initrd".to_owned(),
                        kernel_cmdline: "rdinit=/init quiet".to_owned(),
                    },
                )),
                devices_config: Some(vmservice::DevicesConfig {
                    virtiofs_config: vec![vmservice::VirtioFs {
                        tag: "microvm".to_owned(),
                        guest_mount_target: "/mnt/share".to_owned(),
                        denied_paths: vec![path("data/secret")],
                        allowed_paths: vec![path("data/secret/public")],
                        writable_paths: vec![path("data/out")],
                        owner: vmservice::virtio_fs::Owner::Vmm as i32,
                        aggregate_children: vec![
                            vmservice::virtio_fs::AggregateChild {
                                name: "data".to_owned(),
                                host_path: path("data"),
                                read_write: true,
                                file: false,
                            },
                            vmservice::virtio_fs::AggregateChild {
                                name: "config.json".to_owned(),
                                host_path: path("config.json"),
                                read_write: false,
                                file: true,
                            },
                        ],
                        ..Default::default()
                    }],
                    virtio_consoles: vec![vmservice::VirtioConsoleConfig {
                        socket_path: "console.sock".to_owned(),
                        connect: false,
                    }],
                    microvm_sandbox_blocks: vec![
                        vmservice::MicrovmSandboxBlock {
                            role: vmservice::microvm_sandbox_block::Role::Distro as i32,
                            backend: file_disk("distro.img"),
                            read_only: true,
                        },
                        vmservice::MicrovmSandboxBlock {
                            role: vmservice::microvm_sandbox_block::Role::Scratch as i32,
                            backend: file_disk("scratch.img"),
                            read_only: false,
                        },
                    ],
                    microvm_network: Some(vmservice::MicrovmNetwork {
                        address: "10.0.0.2/24".to_owned(),
                        profile: vmservice::microvm_network::Profile::Portable as i32,
                        egress: Action::Deny as i32,
                        egress_allow: vec![
                            "10.0.0.1/32:tcp:443".to_owned(),
                            "fd00::a00:1/128:udp:53".to_owned(),
                        ],
                        egress_deny: vec!["10.0.0.9/32".to_owned()],
                        host_loopback: Action::Deny as i32,
                        proxy: "10.0.0.1:3128".to_owned(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                cpu_profile: "host".to_owned(),
                ..Default::default()
            }),
            log_id: String::new(),
            microvm_snapshot: Some(vmservice::MicrovmSnapshotConfig {
                destination_path: destination.to_owned(),
                memory_capacity_bytes: 512 << 20,
                snapshot_tier: SnapshotTier::WorkloadStart as i32,
                snapshot_generation_id: vec![0x11; 16],
                snapshot_scratch_restore_mode: ScratchRestoreMode::PrivateCopy as i32,
                ..Default::default()
            }),
        };
        let arguments = [
            "--machine",
            "microvm",
            "-p",
            "2",
            "--memory",
            "size=256M,file=ram.bin",
            "--kernel",
            "vmlinux",
            "--initrd",
            "initrd",
            "-c",
            "rdinit=/init quiet",
            "--virtio-console",
            "listen=console.sock",
            "--mount-aggregate",
            "/mnt/share",
            "--mount-child",
            &format!("data,{},rw", path("data")),
            "--mount-child",
            &format!("config.json,{},ro,file", path("config.json")),
            "--mount-deny",
            &path("data/secret"),
            "--mount-allow",
            &path("data/secret/public"),
            "--mount-write",
            &path("data/out"),
            "--mount-owner",
            "vmm",
            "--microvm-sandbox-block",
            "distro:file:distro.img,ro",
            "--microvm-sandbox-block",
            "scratch:file:scratch.img",
            "--net",
            "10.0.0.2/24",
            "--network-profile",
            "portable",
            "--network-egress",
            "deny",
            "--network-egress-allow",
            "10.0.0.1/32:tcp:443",
            "--network-egress-allow",
            "fd00::a00:1/128:udp:53",
            "--network-egress-deny",
            "10.0.0.9/32",
            "--host-loopback",
            "deny",
            "--network-proxy",
            "10.0.0.1:3128",
            "--cpu-profile",
            "host",
            "--snapshot-destination",
            destination,
            "--memory-capacity",
            "512M",
            "--snapshot-tier",
            "workload-start",
            "--snapshot-generation-id",
            "11111111111111111111111111111111",
            "--snapshot-scratch-restore-mode",
            "private-copy",
        ]
        .map(str::to_owned)
        .to_vec();
        (request, arguments)
    }

    #[test]
    fn ttrpc_microvm_capture_requests_take_the_command_line_options() {
        let destination = std::env::temp_dir().join("snapshot");
        let destination = destination.to_str().unwrap();
        let (request, arguments) = capture_request(destination);
        let arguments = arguments.iter().map(String::as_str).collect::<Vec<_>>();
        assert_same_microvm_options(
            &command_line_view(request).unwrap(),
            &command_line(&arguments),
        );
    }

    #[test]
    fn ttrpc_microvm_restore_requests_take_the_command_line_options() {
        use vmservice::microvm_network::Action;
        use vmservice::microvm_network::host_loopback_forward::Protocol;

        let snapshot = std::env::temp_dir().join("snapshot");
        let snapshot = snapshot.to_str().unwrap();
        let ready = std::env::temp_dir().join("ready.sock");
        let ready = ready.to_str().unwrap();
        let record = std::env::temp_dir().join("activation.bin");
        let record = record.to_str().unwrap();
        let request = vmservice::CreateVmRequest {
            config: Some(vmservice::VmConfig {
                machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                devices_config: Some(vmservice::DevicesConfig {
                    virtiofs_config: vec![vmservice::VirtioFs {
                        tag: "microvm".to_owned(),
                        root_path: "share".to_owned(),
                        guest_mount_target: "/mnt".to_owned(),
                        read_write: true,
                        denied_paths: vec!["secret".to_owned()],
                        owner: vmservice::virtio_fs::Owner::Caller as i32,
                        ..Default::default()
                    }],
                    virtio_consoles: vec![vmservice::VirtioConsoleConfig {
                        socket_path: "console.sock".to_owned(),
                        connect: true,
                    }],
                    microvm_sandbox_blocks: vec![vmservice::MicrovmSandboxBlock {
                        role: vmservice::microvm_sandbox_block::Role::Distro as i32,
                        backend: file_disk("distro.img"),
                        read_only: true,
                    }],
                    microvm_network: Some(vmservice::MicrovmNetwork {
                        profile: vmservice::microvm_network::Profile::Portable as i32,
                        egress: Action::Allow as i32,
                        ingress: Action::Allow as i32,
                        host_loopback: Action::Allow as i32,
                        host_loopback_forwards: vec![
                            vmservice::microvm_network::HostLoopbackForward {
                                protocol: Protocol::Tcp as i32,
                                host_port: 3000,
                                guest_port: 8080,
                            },
                        ],
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                cpu_profile: "auto".to_owned(),
                ..Default::default()
            }),
            log_id: String::new(),
            microvm_snapshot: Some(vmservice::MicrovmSnapshotConfig {
                restore_path: snapshot.to_owned(),
                restore_ready_path: ready.to_owned(),
                restore_processor_count: 2,
                restore_gate_timeout_ms: 1000,
                restore_memory_bytes: 256 << 20,
                restore_generation_id: vec![0x22; 16],
                restore_absent_scratch: true,
                restore_network_policy: true,
                restore_activation_record_path: record.to_owned(),
                ..Default::default()
            }),
        };
        let cli = command_line(&[
            "--machine",
            "microvm",
            "--restore-snapshot",
            snapshot,
            "--restore-ready-path",
            ready,
            "--restore-activation-record",
            record,
            "--restore-processors",
            "2",
            "--restore-gate-timeout-ms",
            "1000",
            "--restore-memory",
            "256M",
            "--restore-generation-id",
            "22222222222222222222222222222222",
            "--restore-absent-scratch",
            "--restore-network-policy",
            "--mount",
            "/mnt,share,rw",
            "--mount-deny",
            "secret",
            "--mount-owner",
            "caller",
            "--virtio-console",
            "connect=console.sock",
            "--microvm-sandbox-block",
            "distro:file:distro.img,ro",
            "--network-profile",
            "portable",
            "--network-egress",
            "allow",
            "--network-ingress",
            "allow",
            "--host-loopback",
            "allow",
            "--host-loopback-forward",
            "tcp:3000:8080",
            "--cpu-profile",
            "auto",
        ]);
        assert_same_microvm_options(&command_line_view(request).unwrap(), &cli);

        // A restore may leave out its configuration, which the snapshot
        // provides.
        let request = vmservice::CreateVmRequest {
            microvm_snapshot: Some(vmservice::MicrovmSnapshotConfig {
                restore_path: snapshot.to_owned(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_same_microvm_options(
            &command_line_view(request).unwrap(),
            &command_line(&["--machine", "microvm", "--restore-snapshot", snapshot]),
        );
    }

    #[test]
    fn ttrpc_microvm_cold_boot_uses_the_requested_cpu_profile() {
        for (requested, selected) in [
            ("", "auto"),
            ("auto", "auto"),
            ("host", "host"),
            ("intel.skylake-sp.v1", "intel.skylake-sp.v1"),
        ] {
            let view = command_line_view(vmservice::CreateVmRequest {
                config: Some(vmservice::VmConfig {
                    machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                    memory_config: Some(vmservice::MemoryConfig {
                        memory_mb: 128,
                        ..Default::default()
                    }),
                    boot_config: Some(vmservice::vm_config::BootConfig::DirectBoot(
                        Default::default(),
                    )),
                    cpu_profile: requested.to_owned(),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .unwrap();
            // The command line selects `auto` without `--cpu-profile`.
            assert_eq!(
                view.microvm.cpu_profile.as_deref().unwrap_or("auto"),
                selected
            );
        }
    }

    #[test]
    fn ttrpc_microvm_consoles_take_the_command_line_endpoints() {
        let view = |socket_path: &str, connect| {
            command_line_view(vmservice::CreateVmRequest {
                config: Some(vmservice::VmConfig {
                    machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                    memory_config: Some(vmservice::MemoryConfig {
                        memory_mb: 128,
                        ..Default::default()
                    }),
                    boot_config: Some(vmservice::vm_config::BootConfig::DirectBoot(
                        Default::default(),
                    )),
                    devices_config: Some(vmservice::DevicesConfig {
                        virtio_consoles: vec![vmservice::VirtioConsoleConfig {
                            socket_path: socket_path.to_owned(),
                            connect,
                        }],
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            })
        };
        for (socket_path, connect, option) in [
            ("console.sock", false, "listen=console.sock"),
            ("console.sock", true, "connect=console.sock"),
            ("", false, "none"),
        ] {
            assert_eq!(
                view(socket_path, connect).unwrap().virtio_console,
                command_line(&["--virtio-console", option]).virtio_console,
                "{option}"
            );
        }
        // A client needs an endpoint to connect to.
        assert!(
            format!("{:#}", view("", true).err().unwrap())
                .contains("a microVM virtio-console client requires a socket path")
        );
        // A request names a Unix socket or named pipe, and never a TCP
        // endpoint, which the command line's `listen=tcp:` selects and a
        // microVM console refuses.
        assert_eq!(
            view("tcp:127.0.0.1:5555", false).unwrap().virtio_console,
            Some(SerialConfigCli::Pipe(PathBuf::from("tcp:127.0.0.1:5555")))
        );
    }

    /// A restore resolves its boot console from its request's console entry
    /// with the command line's rules: it refuses a snapshot whose console has
    /// a TCP endpoint, which older captures could record, whatever the entry,
    /// and it attaches a listener to a console captured without an endpoint,
    /// which keeps discarding the guest's output while no client is connected.
    #[test]
    fn ttrpc_microvm_restore_consoles_follow_the_command_line_rules() {
        use crate::microvm::microvm_console_attachment_from_snapshot;
        use openvmm_helpers::snapshot::microvm::SnapshotAttachment;
        use virtio_resources::console::attachment::VirtioConsoleAttachmentMode;
        use virtio_resources::console::attachment::VirtioConsoleDisconnectPolicy;

        let directory = tempfile::tempdir().unwrap();
        let snapshot = directory.path().join("snapshot");
        let listener = if cfg!(windows) {
            "//./pipe/openvmm-microvm-ttrpc-restore-console".to_owned()
        } else {
            directory
                .path()
                .join("console.sock")
                .to_str()
                .unwrap()
                .to_owned()
        };
        let restore = |socket_path: Option<&str>, connect| {
            command_line_view(vmservice::CreateVmRequest {
                config: Some(vmservice::VmConfig {
                    machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                    devices_config: Some(vmservice::DevicesConfig {
                        virtio_consoles: socket_path
                            .map(|socket_path| vmservice::VirtioConsoleConfig {
                                socket_path: socket_path.to_owned(),
                                connect,
                            })
                            .into_iter()
                            .collect(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                log_id: String::new(),
                microvm_snapshot: Some(vmservice::MicrovmSnapshotConfig {
                    restore_path: snapshot.to_str().unwrap().to_owned(),
                    ..Default::default()
                }),
            })
            .unwrap()
            .virtio_console
        };
        let saved =
            |reconnect_policy: &str, identity_kind: &str, identity: &[u8]| SnapshotAttachment {
                stable_id: crate::microvm::MICROVM_CONSOLE_STABLE_ID.to_owned(),
                kind: crate::microvm::MICROVM_CONSOLE_ATTACHMENT_KIND.to_owned(),
                required: false,
                reconnect_policy: reconnect_policy.to_owned(),
                identity_kind: identity_kind.to_owned(),
                identity: identity.to_vec(),
                length: 0,
                reconnect_timeout_ms: 0,
            };

        let tcp = saved("recreate-listener", "tcp", b"127.0.0.1:5555");
        for (socket_path, connect) in [
            (None, false),
            (Some(listener.as_str()), false),
            (Some(listener.as_str()), true),
            (Some(""), false),
        ] {
            let requested = restore(socket_path, connect);
            let error = microvm_console_attachment_from_snapshot(&tcp, requested.as_ref())
                .err()
                .unwrap();
            assert!(error.to_string().contains("TCP endpoint"), "{error}");
        }

        let disconnected = saved("discard-while-disconnected", "disconnected", b"discard");
        let (endpoint, attachment, _) = microvm_console_attachment_from_snapshot(
            &disconnected,
            restore(Some(&listener), false).as_ref(),
        )
        .unwrap();
        assert!(matches!(endpoint, SerialConfigCli::Pipe(_)));
        assert_eq!(attachment.mode, VirtioConsoleAttachmentMode::Listen);
        assert_eq!(
            crate::microvm::microvm_console_disconnect_policy(&attachment),
            VirtioConsoleDisconnectPolicy::Discard
        );
        // A client does not attach to it, and a restore without a console
        // entry keeps it without an endpoint.
        assert!(
            microvm_console_attachment_from_snapshot(
                &disconnected,
                restore(Some(&listener), true).as_ref()
            )
            .is_err()
        );
        let (endpoint, _, _) =
            microvm_console_attachment_from_snapshot(&disconnected, restore(None, false).as_ref())
                .unwrap();
        assert_eq!(endpoint, SerialConfigCli::None);
    }

    /// Returns the snapshot options of `request`.
    fn snapshot_options(
        request: &mut vmservice::CreateVmRequest,
    ) -> &mut vmservice::MicrovmSnapshotConfig {
        request.microvm_snapshot.as_mut().unwrap()
    }

    /// Returns the devices of `request`.
    fn devices(request: &mut vmservice::CreateVmRequest) -> &mut vmservice::DevicesConfig {
        request
            .config
            .as_mut()
            .unwrap()
            .devices_config
            .as_mut()
            .unwrap()
    }

    /// Returns the error with which the command line's rules refuse the
    /// options of `request`.
    #[cfg(guest_arch = "x86_64")]
    fn command_line_error(request: vmservice::CreateVmRequest) -> String {
        format!(
            "{:#}",
            command_line_view(request)
                .unwrap()
                .validate_microvm_options()
                .unwrap_err()
        )
    }

    #[cfg(guest_arch = "x86_64")]
    #[test]
    fn ttrpc_microvm_requests_follow_the_command_line_rules() {
        use vmservice::microvm_network::Action;
        use vmservice::microvm_snapshot_config::ScratchRestoreMode;
        use vmservice::microvm_snapshot_config::SnapshotTier;

        let destination = std::env::temp_dir().join("snapshot");
        let destination = destination.to_str().unwrap();
        let capture = |change: &dyn Fn(&mut vmservice::CreateVmRequest)| {
            let (mut request, _) = capture_request(destination);
            change(&mut request);
            request
        };
        let snapshot = snapshot_options;
        fn network(request: &mut vmservice::CreateVmRequest) -> &mut vmservice::MicrovmNetwork {
            devices(request).microvm_network.as_mut().unwrap()
        }
        // The capture request itself follows the rules.
        command_line_view(capture(&|_| {}))
            .unwrap()
            .validate_microvm_options()
            .unwrap();
        for (request, expected) in [
            (
                capture(&|request| snapshot(request).snapshot_tier = 0),
                "--snapshot-tier is required exactly for microVM snapshot capture with sandbox blocks",
            ),
            (
                capture(&|request| snapshot(request).snapshot_generation_id = Vec::new()),
                "--snapshot-tier workload-start binds the sandbox blocks and requires --snapshot-generation-id",
            ),
            (
                capture(&|request| {
                    snapshot(request).snapshot_tier = SnapshotTier::InstanceCheckpoint as i32;
                    snapshot(request).snapshot_scratch_restore_mode =
                        ScratchRestoreMode::PrivateCopy as i32;
                    devices(request).microvm_sandbox_blocks[1].read_only = true;
                }),
                "microVM sandbox block role Scratch must be writable",
            ),
            (
                capture(&|request| {
                    snapshot(request).snapshot_scratch_restore_mode =
                        ScratchRestoreMode::DirectClaimed as i32;
                }),
                "direct-claimed scratch requires instance-checkpoint resume semantics",
            ),
            (
                capture(&|request| {
                    devices(request)
                        .microvm_sandbox_blocks
                        .pop()
                        .map(drop)
                        .unwrap()
                }),
                "microVM sandbox block topology requires a writable scratch device",
            ),
            (
                capture(&|request| network(request).profile = 0),
                "microVM --net requires --network-profile portable",
            ),
            (
                capture(&|request| network(request).address = String::new()),
                "--network-profile portable requires --net or --restore-snapshot",
            ),
            (
                capture(&|request| {
                    network(request).address = String::new();
                    network(request).profile = 0;
                }),
                "microVM network policy requires --net or a networked snapshot restore",
            ),
            (
                capture(&|request| network(request).host_loopback = Action::Allow as i32),
                "microVM snapshots do not support --host-loopback allow",
            ),
            (
                capture(&|request| {
                    let children = &mut devices(request).virtiofs_config[0].aggregate_children;
                    children[1].name = children[0].name.clone();
                }),
                "invalid --mount-aggregate or --mount-child options",
            ),
        ] {
            let error = command_line_error(request);
            assert!(error.contains(expected), "{expected}: {error}");
        }
    }

    #[test]
    fn ttrpc_microvm_requests_combine_only_the_fields_that_the_protocol_allows() {
        use vmservice::microvm_network::host_loopback_forward::Protocol;

        let destination = std::env::temp_dir().join("snapshot");
        let destination = destination.to_str().unwrap();
        let capture = |change: &dyn Fn(&mut vmservice::CreateVmRequest)| {
            let (mut request, _) = capture_request(destination);
            change(&mut request);
            request
        };
        let snapshot = snapshot_options;
        for (request, expected) in [
            (
                capture(&|request| snapshot(request).destination_path = String::new()),
                "memory_capacity_bytes requires destination_path",
            ),
            (
                capture(&|request| {
                    snapshot(request).memory_capacity_bytes = 0;
                    snapshot(request).destination_path = String::new();
                }),
                "snapshot_tier requires destination_path",
            ),
            (
                capture(&|request| snapshot(request).restore_absent_scratch = true),
                "restore_absent_scratch requires restore_path",
            ),
            (
                capture(&|request| snapshot(request).restore_generation_id = vec![1; 16]),
                "restore_generation_id requires restore_path",
            ),
            (
                capture(&|request| snapshot(request).restore_network_policy = true),
                "restore_network_policy requires restore_path",
            ),
            (
                capture(&|request| {
                    snapshot(request).restore_activation_record_path = "record".to_owned();
                }),
                "restore_activation_record_path requires restore_path",
            ),
            (
                capture(&|request| snapshot(request).restore_path = "snapshot".to_owned()),
                "snapshot capture and restore paths are mutually exclusive",
            ),
            (
                capture(&|request| snapshot(request).snapshot_generation_id = vec![1; 15]),
                "invalid snapshot_generation_id: snapshot generation ID must be exactly 16 bytes",
            ),
            (
                capture(&|request| snapshot(request).snapshot_generation_id = vec![0; 16]),
                "invalid snapshot_generation_id: snapshot generation ID must be nonzero",
            ),
            (
                capture(&|request| devices(request).microvm_sandbox_blocks[0].role = 0),
                "a microVM sandbox block requires a role",
            ),
            (
                capture(&|request| devices(request).microvm_sandbox_blocks[0].backend = None),
                "a microVM sandbox block requires a backend",
            ),
            (
                capture(&|request| {
                    let network = devices(request).microvm_network.as_mut().unwrap();
                    network.allow_hosts = vec!["192.0.2.0/24".to_owned()];
                    network.block_hosts = vec!["198.51.100.0/24".to_owned()];
                }),
                "allow_hosts, block_hosts, and allow_endpoints are mutually exclusive",
            ),
            (
                capture(&|request| {
                    devices(request)
                        .microvm_network
                        .as_mut()
                        .unwrap()
                        .egress_allow = vec!["10.0.0.1/33".to_owned()];
                }),
                "invalid egress_allow '10.0.0.1/33'",
            ),
            (
                capture(&|request| {
                    devices(request).microvm_network.as_mut().unwrap().address =
                        "10.0.0.2".to_owned();
                }),
                "invalid address '10.0.0.2'",
            ),
            (
                capture(&|request| {
                    devices(request)
                        .microvm_network
                        .as_mut()
                        .unwrap()
                        .host_loopback_forwards =
                        vec![vmservice::microvm_network::HostLoopbackForward {
                            protocol: Protocol::Udp as i32,
                            host_port: 0,
                            guest_port: 53,
                        }];
                }),
                "invalid host_loopback_forwards 'udp:0:53': loopback-forward host port must be nonzero",
            ),
            (
                capture(&|request| {
                    devices(request).virtiofs_config[0].root_path = "share".to_owned();
                }),
                "an aggregate microVM virtio-fs requires tag 'microvm' and no host root path",
            ),
            (
                capture(&|request| {
                    devices(request).virtiofs_config[0].aggregate_children[0].host_path =
                        String::new();
                }),
                "microVM aggregate child 'data' requires a host path",
            ),
            (
                capture(&|request| devices(request).virtiofs_config[0].read_only = true),
                "microVM virtio-fs uses read_write rather than read_only",
            ),
            (
                capture(&|request| {
                    request
                        .config
                        .as_mut()
                        .unwrap()
                        .memory_config
                        .as_mut()
                        .unwrap()
                        .memory_mb = u64::MAX;
                }),
                "invalid memory configuration",
            ),
        ] {
            let error = format!("{:#}", command_line_view(request).err().unwrap());
            assert!(error.contains(expected), "{expected}: {error}");
        }

        // A restore takes its machine, memory, and boot from its snapshot.
        let restore = |config: vmservice::VmConfig| vmservice::CreateVmRequest {
            config: Some(vmservice::VmConfig {
                machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                ..config
            }),
            microvm_snapshot: Some(vmservice::MicrovmSnapshotConfig {
                restore_path: "snapshot".to_owned(),
                ..Default::default()
            }),
            ..Default::default()
        };
        for config in [
            vmservice::VmConfig {
                memory_config: Some(vmservice::MemoryConfig {
                    backing_file_path: "ram.bin".to_owned(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            vmservice::VmConfig {
                boot_config: Some(vmservice::vm_config::BootConfig::DirectBoot(
                    Default::default(),
                )),
                ..Default::default()
            },
        ] {
            let error = format!("{:#}", command_line_view(restore(config)).err().unwrap());
            assert!(
                error.contains("restore configuration may contain only processor identity"),
                "{error}"
            );
        }
    }

    #[test]
    fn ttrpc_standard_requests_reject_microvm_settings() {
        for request in [
            vmservice::CreateVmRequest {
                config: Some(vmservice::VmConfig {
                    cpu_profile: "host".to_owned(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            vmservice::CreateVmRequest {
                config: Some(vmservice::VmConfig::default()),
                microvm_snapshot: Some(vmservice::MicrovmSnapshotConfig {
                    destination_path: "snapshot".to_owned(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            vmservice::CreateVmRequest {
                config: Some(vmservice::VmConfig {
                    memory_config: Some(vmservice::MemoryConfig {
                        backing_file_path: "ram.bin".to_owned(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            vmservice::CreateVmRequest {
                config: Some(vmservice::VmConfig {
                    devices_config: Some(vmservice::DevicesConfig {
                        microvm_sandbox_blocks: vec![Default::default()],
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            vmservice::CreateVmRequest {
                config: Some(vmservice::VmConfig {
                    devices_config: Some(vmservice::DevicesConfig {
                        microvm_network: Some(Default::default()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ] {
            assert!(!requested(&request).unwrap());
            let error = validate_standard_request(&request).unwrap_err().to_string();
            assert!(
                error.contains("require the microVM machine profile")
                    || error.contains("requires the microVM machine profile"),
                "{error}"
            );
        }
        let standard = vmservice::CreateVmRequest {
            config: Some(vmservice::VmConfig::default()),
            ..Default::default()
        };
        assert!(!requested(&standard).unwrap());
        validate_standard_request(&standard).unwrap();

        // A restore is a microVM's, whose snapshot selects the profile, and an
        // unknown profile is an error.
        assert!(
            requested(&vmservice::CreateVmRequest {
                microvm_snapshot: Some(vmservice::MicrovmSnapshotConfig {
                    restore_path: "snapshot".to_owned(),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .unwrap()
        );
        assert_eq!(
            requested(&vmservice::CreateVmRequest {
                config: Some(vmservice::VmConfig {
                    machine_profile: 2,
                    ..Default::default()
                }),
                ..Default::default()
            })
            .unwrap_err()
            .to_string(),
            "unknown machine profile 2"
        );
    }

    #[test]
    fn ttrpc_standard_virtio_fs_preserves_access_mode() {
        for read_only in [false, true] {
            let handle = build_virtio_fs(vmservice::VirtioFs {
                tag: "share".to_owned(),
                root_path: "host-root".to_owned(),
                read_only,
                ..Default::default()
            })
            .unwrap();
            assert!(matches!(
                handle.profile,
                virtio_resources::fs::microvm::VirtioFsProfile::Standard
            ));
            let virtio_resources::fs::VirtioFsBackend::HostFs {
                root_path,
                mount_options,
            } = handle.fs
            else {
                panic!("standard virtio-fs must use HostFs");
            };
            assert_eq!(root_path, "host-root");
            assert_eq!(mount_options, if read_only { "ro" } else { "" });
        }
    }

    #[test]
    fn ttrpc_standard_virtio_fs_rejects_microvm_mount_fields() {
        let microvm_fields = [
            vmservice::VirtioFs {
                guest_mount_target: "/share".to_owned(),
                ..Default::default()
            },
            vmservice::VirtioFs {
                read_write: true,
                ..Default::default()
            },
            vmservice::VirtioFs {
                denied_paths: vec!["secret".to_owned()],
                ..Default::default()
            },
            vmservice::VirtioFs {
                allowed_paths: vec!["public".to_owned()],
                ..Default::default()
            },
            vmservice::VirtioFs {
                writable_paths: vec!["out".to_owned()],
                ..Default::default()
            },
            vmservice::VirtioFs {
                owner: vmservice::virtio_fs::Owner::Vmm as i32,
                ..Default::default()
            },
            vmservice::VirtioFs {
                aggregate_children: vec![Default::default()],
                ..Default::default()
            },
        ];
        for fields in microvm_fields {
            let error = build_virtio_fs(vmservice::VirtioFs {
                tag: "share".to_owned(),
                root_path: "host-root".to_owned(),
                ..fields
            })
            .err()
            .expect("microVM mount fields must be rejected");
            assert!(
                error
                    .to_string()
                    .contains("standard-machine virtio-fs does not accept microVM mount fields")
            );
        }
    }

    #[test]
    fn management_restore_rejects_control_console_attachments_explicitly() {
        let boot = openvmm_helpers::snapshot::microvm::SnapshotAttachment {
            stable_id: crate::microvm::MICROVM_CONSOLE_STABLE_ID.to_owned(),
            kind: crate::microvm::MICROVM_CONSOLE_ATTACHMENT_KIND.to_owned(),
            required: false,
            reconnect_policy: "discard-while-disconnected".to_owned(),
            identity_kind: "disconnected".to_owned(),
            identity: b"discard".to_vec(),
            length: 0,
            reconnect_timeout_ms: 0,
        };
        validate_restore_console_attachments(&[]).unwrap();
        validate_restore_console_attachments(std::slice::from_ref(&boot)).unwrap();

        let mut control = boot.clone();
        control.stable_id = crate::microvm::MICROVM_CONTROL_CONSOLE_STABLE_ID.to_owned();
        control.kind = crate::microvm::MICROVM_CONTROL_CONSOLE_ATTACHMENT_KIND.to_owned();
        for policy in ["discard-while-disconnected", "recreate-listener"] {
            control.reconnect_policy = policy.to_owned();
            let error =
                validate_restore_console_attachments(&[boot.clone(), control.clone()]).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("management RPC cannot restore control-console snapshots")
            );
            assert!(
                error
                    .to_string()
                    .contains("before authenticated broker activation")
            );
        }
    }

    #[cfg(guest_arch = "x86_64")]
    #[test]
    fn ttrpc_microvm_profile_has_a_fixed_wire_identity_and_processor_policy() {
        // The profile's numeric value is 1, whatever microVM ABI version it
        // selects; the ABI version is a snapshot's.
        assert_eq!(vmservice::vm_config::MachineProfile::Microvm as i32, 1);
        assert!(vmservice::vm_config::MachineProfile::from_i32(2).is_none());

        // The command line's rules accept 1, 2, 4, or 8 processors.
        for processor_count in [0, 1, 2, 3, 4, 5, 8, 16] {
            let options = command_line_view(vmservice::CreateVmRequest {
                config: Some(vmservice::VmConfig {
                    machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                    memory_config: Some(vmservice::MemoryConfig {
                        memory_mb: 128,
                        ..Default::default()
                    }),
                    processor_config: Some(vmservice::ProcessorConfig {
                        processor_count,
                        ..Default::default()
                    }),
                    boot_config: Some(vmservice::vm_config::BootConfig::DirectBoot(
                        Default::default(),
                    )),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .unwrap();
            assert_eq!(options.processors, processor_count);
            let supported = [1, 2, 4, 8].contains(&processor_count);
            match options.validate_microvm_options() {
                Ok(()) => assert!(supported, "{processor_count}"),
                Err(error) => {
                    assert!(!supported, "{processor_count}: {error:#}");
                    assert_eq!(
                        error.to_string(),
                        format!("microVM does not support {processor_count} vCPUs")
                    );
                }
            }
        }
    }
}
