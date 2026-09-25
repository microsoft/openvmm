// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command-line options of the microVM machine profile.

use super::EndpointConfigCli;
use super::Options;
use super::SerialConfigCli;
use super::SmtConfigCli;
use anyhow::Context;
use clap::ValueEnum;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::X2ApicConfig;
use openvmm_defs::microvm::MachineProfile;
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
    /// Capture a microVM snapshot to this directory when the guest writes PMIO 0x605.
    #[clap(long, value_name = "DIR", conflicts_with = "restore_snapshot")]
    pub snapshot_destination: Option<PathBuf>,

    /// Maximum time allowed to quiesce the VM for a guest-requested snapshot.
    #[clap(long, value_name = "MILLISECONDS", default_value_t = 5000)]
    pub snapshot_quiesce_timeout_ms: u64,

    /// Required host-network implementation contract for microVM `--net`.
    #[clap(long, value_enum, value_name = "PROFILE")]
    pub network_profile: Option<MicrovmNetworkProfileCli>,

    /// attach the microVM virtio-fs device
    ///
    /// A snapshot with the filesystem requires the same canonical host path,
    /// guest target, and mode on restore.
    #[clap(
        long = "mount",
        value_name = "GUEST_TARGET,HOST_PATH[,ro|rw]",
        conflicts_with_all = ["virtio_fs", "virtio_fs_shmem"]
    )]
    pub microvm_mount: Option<MicrovmMountCli>,
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

/// Parses a bare `<IPv4>/<prefix>` `--net` endpoint into the microVM network
/// configuration.
pub(super) fn parse_endpoint(network: &str) -> Result<EndpointConfigCli, String> {
    network
        .parse()
        .map(EndpointConfigCli::Microvm)
        .map_err(|error| format!("invalid microVM network: {error}"))
}

impl Options {
    pub(crate) fn validate_microvm_options(&self) -> anyhow::Result<()> {
        if self.machine != MachineProfileCli::Microvm {
            anyhow::ensure!(
                self.microvm.network_profile.is_none() && self.microvm.microvm_mount.is_none(),
                "--network-profile and --mount require a microVM machine"
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
        }
        if self.restore_snapshot.is_some() {
            anyhow::ensure!(
                self.net.is_empty(),
                "microVM restore takes network addressing from saved state; do not pass --net"
            );
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
        anyhow::ensure!(
            self.disk.is_empty()
                && self.nvme.is_empty()
                && self.nvme_pci.is_empty()
                && self.vmbus_scsi.is_empty()
                && self.openhcl_controller.is_empty()
                && self.ide.is_empty()
                && self.floppy.is_empty()
                && self.virtio_blk.is_empty(),
            "microVM does not expose storage devices"
        );
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
    fn test_microvm_snapshot_capture_options() {
        let capture = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--processors",
            "2",
            "--snapshot-destination",
            "snapshot",
        ])
        .unwrap();
        assert_eq!(capture.microvm.snapshot_quiesce_timeout_ms, 5000);
        capture.validate_microvm_options().unwrap();

        for extra in [
            vec!["--snapshot-quiesce-timeout-ms", "0"],
            vec!["--memory", "size=1G,shared=off"],
        ] {
            let options = Options::try_parse_from(
                [
                    "openvmm",
                    "--machine",
                    "microvm",
                    "--snapshot-destination",
                    "snapshot",
                ]
                .into_iter()
                .chain(extra),
            )
            .unwrap();
            assert!(options.validate_microvm_options().is_err());
        }

        assert!(
            Options::try_parse_from([
                "openvmm",
                "--machine",
                "microvm",
                "--snapshot-destination",
                "snapshot",
                "--restore-snapshot",
                "snapshot",
            ])
            .is_err()
        );

        let restore = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
        ])
        .unwrap();
        assert_eq!(restore.memory, Default::default());
        restore.validate_microvm_options().unwrap();
        for override_arg in ["--kernel", "--initrd"] {
            assert!(
                Options::try_parse_from([
                    "openvmm",
                    "--machine",
                    "microvm",
                    "--restore-snapshot",
                    "snapshot",
                    override_arg,
                    "file",
                ])
                .is_err()
            );
        }
    }

    #[test]
    fn test_microvm_network_options() {
        let network = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--net",
            "10.0.0.2/24",
            "--network-profile",
            "portable",
        ])
        .unwrap();
        network.validate_microvm_options().unwrap();
        assert!(matches!(
            &network.net[0].endpoint,
            EndpointConfigCli::Microvm(config) if config.prefix_length == 24
        ));

        for args in [
            vec!["openvmm", "--machine", "microvm", "--net", "10.0.0.2/24"],
            vec![
                "openvmm",
                "--machine",
                "microvm",
                "--network-profile",
                "portable",
            ],
            vec![
                "openvmm",
                "--machine",
                "microvm",
                "--net",
                "10.0.0.2/24",
                "--net",
                "10.0.1.2/24",
                "--network-profile",
                "portable",
            ],
            vec!["openvmm", "--network-profile", "portable"],
        ] {
            let options = Options::try_parse_from(args).unwrap();
            assert!(options.validate_microvm_options().is_err());
        }
        assert!(
            Options::try_parse_from(["openvmm", "--machine", "microvm", "--net", "10.0.0.0/24"])
                .is_err()
        );
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
        append_microvm_virtio_discovery(&mut with_devices, None, None, true).unwrap();
        assert_eq!(
            with_devices,
            format!("{MICROVM_CONSOLE_COMMAND_LINE} virtio_mmio.device=0x1000@0xd0002000:7")
        );

        let filesystem = openvmm_defs::microvm::MicrovmFilesystemConfig::new(
            "/mnt/share".to_owned(),
            openvmm_defs::microvm::MicrovmFilesystemAccess::ReadOnly,
        )
        .unwrap();
        let mut with_filesystem = build_microvm_command_line(&[], false).unwrap();
        append_microvm_virtio_discovery(&mut with_filesystem, None, Some(&filesystem), false)
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
            "nr_cpus=1",
            "virtfs_dir=/other",
            "virtfs_tag=other",
            "virtfs_mode=rw",
        ] {
            assert!(build_microvm_command_line(&[reserved.into()], false).is_err());
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
            vec!["openvmm", "--mount", "/mnt/share,host"],
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
            vec!["openvmm", "--machine", "microvm", "--net", "consomme"],
        ] {
            let options = Options::try_parse_from(args).unwrap();
            assert!(options.validate_microvm_options().is_err());
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
}
