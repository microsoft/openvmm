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
use crate::config::X2ApicConfig;
use crate::config::X86TopologyConfig;
use guid::Guid;
use mesh::MeshPayload;
use std::fmt::Write as _;
use vmotherboard::options::BaseChipsetManifest;

/// Returns whether a processor count is valid for the microVM.
pub const fn microvm_processor_count_supported(processor_count: u32) -> bool {
    matches!(processor_count, 1 | 2 | 4 | 8)
}

/// Command line owned by the microVM profile.
pub const MICROVM_BASE_COMMAND_LINE: &str = "earlycon=xe9 console=hvc0 reboot=t panic=-1";
/// Maximum microVM command-line size, including its trailing NUL.
pub const MICROVM_COMMAND_LINE_MAX_SIZE: usize = 64 * 1024;
/// The microVM publishes no level-triggered virtio IRQs in its MP table.
pub const MICROVM_LEVEL_TRIGGERED_IRQS: [u32; 0] = [];

fn validate_microvm_command_line(config: &Config) -> anyhow::Result<()> {
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
    let base_tokens = MICROVM_BASE_COMMAND_LINE
        .split_ascii_whitespace()
        .collect::<Vec<_>>();
    anyhow::ensure!(
        tokens.starts_with(&base_tokens),
        "microVM command line does not begin with the ABI base tokens"
    );
    for prefix in ["earlycon=", "console="] {
        let count = tokens
            .iter()
            .filter(|token| token.starts_with(prefix))
            .count();
        anyhow::ensure!(
            count == 1,
            "microVM command line has an invalid number of {prefix} tokens"
        );
    }
    Ok(())
}

/// Builds the microVM command line and rejects profile-owned user tokens.
pub fn build_microvm_command_line(user_args: &[String]) -> anyhow::Result<String> {
    for arg in user_args {
        if arg.contains('\0') {
            anyhow::bail!("microVM kernel command line contains an embedded NUL");
        }
        if arg.split_ascii_whitespace().any(|token| {
            ["earlycon=", "console=", "nr_cpus="]
                .iter()
                .any(|reserved| token.starts_with(reserved))
        }) {
            anyhow::bail!(
                "microVM kernel command line cannot override profile-owned configuration"
            );
        }
    }

    let mut cmdline = MICROVM_BASE_COMMAND_LINE.to_owned();
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
pub struct MicrovmConfig {}

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
        return Ok(());
    };

    validate_microvm_command_line(config)?;
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
        config.virtio_devices.is_empty(),
        "microVM does not support virtio devices"
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
    fn microvm_processor_limit_is_host_owned() {
        let mut cmdline = build_microvm_command_line(&[]).unwrap();
        append_microvm_processor_limit(&mut cmdline, 8).unwrap();
        assert_eq!(cmdline, format!("{MICROVM_BASE_COMMAND_LINE} nr_cpus=8"));
        assert!(append_microvm_processor_limit(&mut cmdline, 3).is_err());
        assert!(build_microvm_command_line(&["nr_cpus=1".to_owned()]).is_err());
    }
}
