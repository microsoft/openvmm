// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The OpenVMM device platform for an mshv root partition.

use super::NIC_MAC_ADDRESS;
use super::OpenVmmPetriBackend;
use crate::BootDeviceType;
use crate::Firmware;
use crate::PetriVmBuilder;
use petri_artifacts_common::tags::MachineArch;

impl PetriVmBuilder<OpenVmmPetriBackend> {
    /// Add the mshv platform to a UEFI guest: nested virtualization, PCIe on
    /// segment 1, TCP Pipette, and 9p shares, without VMBus or virtio-vsock.
    ///
    /// This does not build or replace guest media. It appends a root complex
    /// rather than changing existing ones, and composes with backend modifiers
    /// registered before or after this call. Segment 1 must be available.
    pub fn with_mshv_platform(self) -> anyhow::Result<Self> {
        anyhow::ensure!(
            self.arch() == MachineArch::X86_64
                && MachineArch::host() == MachineArch::X86_64
                && cfg!(target_os = "linux"),
            "mshv platform requires x86-64 Linux"
        );
        anyhow::ensure!(
            matches!(self.config.firmware, Firmware::Uefi { .. }),
            "mshv platform requires UEFI firmware"
        );
        Ok(self
            .with_no_vmbus()
            .without_virtio_vsock()
            .with_nested_virt()
            .with_serial_output()
            .with_boot_device_type(BootDeviceType::PcieVirtioBlk)
            .with_pcie_boot_port("s1rc0rp0")
            .try_modify_backend(|backend| {
                let share_count = backend.guest_shares.len();
                backend
                    .with_pcie_root_topology_at(1, 1, 1, 2 + share_count as u64)?
                    .with_tcp_pipette_nic("s1rc0rp1", NIC_MAC_ADDRESS)
                    .with_pcie_guest_shares((0..share_count).map(|i| format!("s1rc0rp{}", i + 2)))
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::PetriVmConfigOpenVmm;
    use super::*;
    use crate::Disk;
    use crate::PetriLogSource;
    use crate::PetriVmArtifacts;
    use crate::ResolvedArtifact;
    use crate::UefiConfig;
    use crate::UefiGuest;
    use petri_artifacts_common::tags::OsFlavor;
    use test_with_tracing::test;

    fn builder(
        driver: &pal_async::DefaultDriver,
        logs: &PetriLogSource,
        firmware: &std::path::Path,
    ) -> anyhow::Result<PetriVmBuilder<OpenVmmPetriBackend>> {
        Ok(PetriVmBuilder::minimal(
            "platform",
            logs,
            PetriVmArtifacts {
                backend: OpenVmmPetriBackend::from_binary("unused-openvmm".into()),
                firmware: Firmware::Uefi {
                    uefi_firmware: ResolvedArtifact::from_local_path(firmware.to_path_buf()),
                    guest: UefiGuest::Custom {
                        disk: Disk::Memory(64 * crate::SIZE_1_MB),
                        os_flavor: OsFlavor::Linux,
                    },
                    uefi_config: UefiConfig {
                        disable_frontpage: false,
                        ..Default::default()
                    },
                },
                arch: MachineArch::X86_64,
                agent_image: None,
                openhcl_agent_image: None,
                pipette_binary: None,
                nested_test: None,
            },
            driver,
        )?
        .with_external_pipette())
    }

    async fn configure(
        builder: PetriVmBuilder<OpenVmmPetriBackend>,
    ) -> anyhow::Result<PetriVmConfigOpenVmm> {
        let mut builder = builder.add_boot_disk().add_agent_disks()?;
        let properties = builder.properties();
        let modify = builder.modify_vmm_config.take();
        let config = PetriVmConfigOpenVmm::new(
            &ResolvedArtifact::from_local_path("unused-openvmm".into()),
            builder.config,
            &builder.resources,
            &properties,
        )
        .await?;
        match modify {
            Some(modify) => modify.0(config),
            None => Ok(config),
        }
    }

    #[test]
    fn external_agent_uses_prepared_linux_initrd() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let logs = PetriLogSource::file_only(temp.path())?;
        let base = temp.path().join("base");
        let prepared = temp.path().join("prepared");
        fs_err::write(&base, b"base initrd")?;
        fs_err::write(&prepared, b"prepared initrd")?;
        pal_async::DefaultPool::run_with(async |driver| {
            let builder = PetriVmBuilder::minimal(
                "direct",
                &logs,
                PetriVmArtifacts {
                    backend: OpenVmmPetriBackend::from_binary("unused-openvmm".into()),
                    firmware: Firmware::LinuxDirect {
                        kernel: ResolvedArtifact::from_local_path(base.clone()),
                        initrd: ResolvedArtifact::from_local_path(base.clone()),
                    },
                    arch: MachineArch::host(),
                    agent_image: None,
                    openhcl_agent_image: None,
                    pipette_binary: None,
                    nested_test: None,
                },
                &driver,
            )?
            .with_external_pipette()
            .with_prebuilt_initrd(crate::PetriInitrd {
                path: crate::TempOrPersistentPath::Persistent(prepared),
                rdinit_param: "/external-init".into(),
            })
            .with_serial_output();
            assert!(!builder.properties().uses_pipette_as_init);
            let backend = configure(builder).await?;
            assert!(backend.resources.linux_direct_serial_agent.is_none());
            let openvmm_defs::config::LoadMode::Linux {
                initrd: Some(mut file),
                cmdline,
                ..
            } = backend.config.load_mode
            else {
                panic!("expected Linux direct")
            };
            assert!(cmdline.contains("rdinit=/external-init"));
            let mut contents = String::new();
            std::io::Read::read_to_string(&mut file, &mut contents)?;
            assert_eq!(contents, "prepared initrd");
            Ok(())
        })
    }

    #[test]
    fn platform_composes_without_replacing_uefi_or_other_roots() -> anyhow::Result<()> {
        if MachineArch::host() != MachineArch::X86_64 || !cfg!(target_os = "linux") {
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let firmware = tempfile::NamedTempFile::new()?;
        let logs = PetriLogSource::file_only(temp.path())?;
        pal_async::DefaultPool::run_with(async |driver| {
            let builder = builder(&driver, &logs, firmware.path())?
                .modify_backend(|b| b.with_pcie_root_topology(1, 1, 1))
                .with_mshv_platform()?
                .with_shared_directory("data", temp.path(), "/mounted elsewhere")?
                .modify_backend(|b| b.with_pcie_root_topology(1, 1, 1));
            assert!(matches!(
                builder.config.firmware,
                Firmware::Uefi {
                    uefi_config: UefiConfig {
                        disable_frontpage: false,
                        ..
                    },
                    ..
                }
            ));
            assert!(!builder.properties().is_linux_direct);
            assert!(builder.properties().no_vmbus);
            assert!(!builder.properties().use_virtio_vsock);
            let backend = configure(builder).await?;
            assert_eq!(
                backend
                    .config
                    .pcie_root_complexes
                    .iter()
                    .map(|rc| rc.segment)
                    .collect::<Vec<_>>(),
                [0, 1, 2]
            );
            assert_eq!(
                backend.config.pcie_root_complexes[0].ports[0].name,
                "s0rc0rp0"
            );
            assert_eq!(backend.config.pcie_root_complexes[1].name, "s1rc0");
            assert_eq!(backend.config.pcie_root_complexes[1].ports.len(), 3);
            for port in ["s1rc0rp0", "s1rc0rp1", "s1rc0rp2"] {
                assert!(
                    backend
                        .config
                        .pcie_devices
                        .iter()
                        .any(|d| d.port_name == port)
                );
            }
            assert!(backend.guest_shares.is_empty());
            Ok(())
        })
    }

    #[test]
    fn segment_conflicts_stop_later_modifiers() -> anyhow::Result<()> {
        if MachineArch::host() != MachineArch::X86_64 || !cfg!(target_os = "linux") {
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let firmware = tempfile::NamedTempFile::new()?;
        let logs = PetriLogSource::file_only(temp.path())?;
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        pal_async::DefaultPool::run_with(async |driver| {
            let builder = builder(&driver, &logs, firmware.path())?
                .try_modify_backend(|b| b.with_pcie_root_topology_at(1, 1, 1, 1))
                .with_mshv_platform()?
                .modify_backend({
                    let called = called.clone();
                    move |b| {
                        called.store(true, std::sync::atomic::Ordering::Relaxed);
                        b
                    }
                });
            let error = configure(builder).await.err().expect("segment conflict");
            assert!(error.to_string().contains("overlaps"), "{error:#}");
            Ok::<_, anyhow::Error>(())
        })?;
        assert!(!called.load(std::sync::atomic::Ordering::Relaxed));
        Ok(())
    }
}
