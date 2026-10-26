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
    pub fn with_mshv_platform(self) -> Self {
        assert!(
            self.arch() == MachineArch::X86_64
                && MachineArch::host() == MachineArch::X86_64
                && cfg!(target_os = "linux"),
            "mshv platform requires x86-64 Linux"
        );
        assert!(
            matches!(self.config.firmware, Firmware::Uefi { .. }),
            "mshv platform requires UEFI firmware"
        );
        self.with_no_vmbus()
            .with_virtio_vsock(false)
            .with_nested_virt()
            .with_serial_output()
            .with_boot_device_type(BootDeviceType::PcieVirtioBlk)
            .with_pcie_boot_port("s1rc0rp0")
            .modify_backend(|backend| {
                backend
                    .with_pcie_root_topology_at(1, 1, 1, 2)
                    .with_tcp_pipette_nic("s1rc0rp1", NIC_MAC_ADDRESS)
            })
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
    use crate::mshv_boot::MshvBoot;
    use petri_artifacts_common::tags::OsFlavor;
    use test_with_tracing::test;
    #[cfg(any(windows, target_os = "linux"))]
    use vm_resource::IntoResource;

    fn builder_with_guest(
        driver: &pal_async::DefaultDriver,
        logs: &PetriLogSource,
        firmware: &std::path::Path,
        guest: UefiGuest,
        arch: MachineArch,
    ) -> anyhow::Result<PetriVmBuilder<OpenVmmPetriBackend>> {
        Ok(PetriVmBuilder::minimal(
            "platform",
            logs,
            PetriVmArtifacts {
                backend: OpenVmmPetriBackend::from_binary("unused-openvmm".into()),
                firmware: Firmware::Uefi {
                    uefi_firmware: ResolvedArtifact::from_local_path(firmware.to_path_buf()),
                    guest,
                    uefi_config: UefiConfig {
                        disable_frontpage: false,
                        ..Default::default()
                    },
                },
                arch,
                agent_image: None,
                openhcl_agent_image: None,
                pipette_binary: None,
                nested_test: None,
            },
            driver,
        )?
        .with_external_pipette())
    }

    fn mshv_guest(dir: &std::path::Path) -> anyhow::Result<UefiGuest> {
        let boot = dir.join("boot");
        fs_err::create_dir_all(boot.join("Windows/System32"))?;
        fs_err::create_dir_all(boot.join("EFI/BOOT"))?;
        for name in [
            "HvLoader.efi",
            "lxhvloader.dll",
            "Windows/System32/hvix64.exe",
            "Windows/System32/hvax64.exe",
            "EFI/BOOT/bootx64.efi",
            "EFI/BOOT/grubx64.efi",
        ] {
            fs_err::write(boot.join(name), name)?;
        }
        let kernel = dir.join("kernel");
        fs_err::write(&kernel, b"root kernel")?;
        let initrd = tempfile::NamedTempFile::new_in(dir)?;
        fs_err::write(initrd.path(), b"prepared initrd")?;
        Ok(UefiGuest::Mshv(MshvBoot {
            kernel: ResolvedArtifact::from_local_path(kernel),
            initrd: crate::PetriInitrd {
                path: crate::TempOrPersistentPath::Temp(std::sync::Arc::new(
                    initrd.into_temp_path(),
                )),
                rdinit_param: "/caller-init".into(),
            },
            boot_artifacts: ResolvedArtifact::from_local_path(dir.to_path_buf()),
        }))
    }

    #[test]
    fn mshv_guest_prepares_an_owned_boot_disk() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let logs = PetriLogSource::file_only(temp.path())?;
        pal_async::DefaultPool::run_with(async |driver| {
            let guest = mshv_guest(temp.path())?;
            let UefiGuest::Mshv(boot) = &guest else {
                unreachable!()
            };
            let initrd = boot.initrd.path.as_ref().to_path_buf();
            let builder = builder_with_guest(
                &driver,
                &logs,
                &temp.path().join("unused-firmware"),
                guest,
                MachineArch::X86_64,
            )?
            .with_boot_device_type(BootDeviceType::PcieVirtioBlk)
            .with_pcie_boot_port("s1rc0rp0");
            assert!(builder.config.pcie_virtio_blk_drives.is_empty());
            assert_eq!(builder.properties().os_flavor, OsFlavor::Linux);
            assert!(!builder.properties().is_linux_direct);
            let builder = builder.add_boot_disk()?;
            let [drive] = builder.config.pcie_virtio_blk_drives.as_slice() else {
                panic!("expected one mshv boot disk")
            };
            assert_eq!(drive.port_name, "s1rc0rp0");
            let Some(Disk::Temporary(path)) = &drive.drive.disk else {
                panic!("expected an owned temporary disk")
            };
            let output = path.to_path_buf();
            assert!(output.exists());
            assert!(initrd.exists());
            drop(builder);
            assert!(!output.exists());
            assert!(!initrd.exists());
            Ok(())
        })
    }

    #[test]
    fn mshv_guest_rejects_unsupported_firmware_and_reports_missing_inputs() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let logs = PetriLogSource::file_only(temp.path())?;
        pal_async::DefaultPool::run_with(async |driver| {
            let error = builder_with_guest(
                &driver,
                &logs,
                &temp.path().join("unused-firmware"),
                mshv_guest(temp.path())?,
                MachineArch::Aarch64,
            )
            .expect_err("mshv requires x86-64");
            assert!(error.to_string().contains("x86-64 UEFI"), "{error:#}");
            let error = PetriVmBuilder::minimal(
                "unsupported",
                &logs,
                PetriVmArtifacts {
                    backend: OpenVmmPetriBackend::from_binary("unused-openvmm".into()),
                    firmware: Firmware::OpenhclUefi {
                        guest: mshv_guest(temp.path())?,
                        igvm_firmware: crate::IgvmFirmwareSource::VmgsOrInBox,
                        isolation: None,
                        uefi_config: Default::default(),
                        openhcl_config: Default::default(),
                    },
                    arch: MachineArch::X86_64,
                    agent_image: None,
                    openhcl_agent_image: None,
                    pipette_binary: None,
                    nested_test: None,
                },
                &driver,
            )
            .expect_err("mshv with OpenHCL is unsupported");
            assert!(error.to_string().contains("OpenHCL"), "{error:#}");
            let builder = builder_with_guest(
                &driver,
                &logs,
                &temp.path().join("unused-firmware"),
                mshv_guest(temp.path())?,
                MachineArch::X86_64,
            )?;
            fs_err::remove_file(temp.path().join("kernel"))?;
            let error = builder.add_boot_disk().expect_err("missing kernel");
            assert!(
                error.to_string().contains("composing mshv boot disk"),
                "{error:#}"
            );
            assert!(format!("{error:#}").contains("kernel"), "{error:#}");
            Ok(())
        })
    }

    async fn configure(
        builder: PetriVmBuilder<OpenVmmPetriBackend>,
    ) -> anyhow::Result<PetriVmConfigOpenVmm> {
        let mut builder = builder.add_boot_disk()?.add_agent_disks()?;
        let properties = builder.properties();
        let modify = builder.modify_vmm_config.take();
        let config = PetriVmConfigOpenVmm::new(
            &ResolvedArtifact::from_local_path("unused-openvmm".into()),
            builder.config,
            &builder.resources,
            &properties,
        )
        .await?;
        let config = match modify {
            Some(modify) => modify.0(config),
            None => config,
        };
        Ok(config.with_guest_shares())
    }

    #[test]
    fn linux_direct_boot_respects_agent_ownership() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let logs = PetriLogSource::file_only(temp.path())?;
        let base = temp.path().join("base");
        let prepared = temp.path().join("prepared");
        fs_err::write(&base, b"base initrd")?;
        fs_err::write(&prepared, b"prepared initrd")?;
        pal_async::DefaultPool::run_with(async |driver| {
            // Legacy serial bootstrap requires 16550 wait-for-RTS, unavailable on PL011.
            for (arch, external, prebuilt) in [
                (MachineArch::X86_64, false, false),
                (MachineArch::X86_64, false, true),
                (MachineArch::X86_64, true, false),
                (MachineArch::X86_64, true, true),
                (MachineArch::Aarch64, false, true),
                (MachineArch::Aarch64, true, false),
                (MachineArch::Aarch64, true, true),
            ] {
                let mut builder = PetriVmBuilder::minimal(
                    "direct",
                    &logs,
                    PetriVmArtifacts {
                        backend: OpenVmmPetriBackend::from_binary("unused-openvmm".into()),
                        firmware: Firmware::LinuxDirect {
                            kernel: ResolvedArtifact::from_local_path(base.clone()),
                            initrd: ResolvedArtifact::from_local_path(base.clone()),
                        },
                        arch,
                        agent_image: None,
                        openhcl_agent_image: None,
                        pipette_binary: None,
                        nested_test: None,
                    },
                    &driver,
                )?
                .with_serial_output();
                if external {
                    builder = builder.with_external_pipette();
                }
                if prebuilt {
                    builder = builder.with_prebuilt_initrd(crate::PetriInitrd {
                        path: crate::TempOrPersistentPath::Persistent(prepared.clone()),
                        rdinit_param: "/external-init".into(),
                    });
                }
                let backend = configure(builder).await?;
                assert_eq!(
                    backend.resources.linux_direct_serial_agent.is_some(),
                    !external && !prebuilt
                );
                let openvmm_defs::config::LoadMode::Linux {
                    initrd: Some(mut file),
                    cmdline,
                    ..
                } = backend.config.load_mode
                else {
                    panic!("expected Linux direct")
                };
                if prebuilt {
                    assert!(cmdline.contains("rdinit=/external-init"));
                } else if external {
                    assert!(
                        !cmdline
                            .split_whitespace()
                            .any(|arg| arg.starts_with("rdinit="))
                    );
                } else {
                    assert!(cmdline.contains("rdinit=/bin/sh"));
                }
                let mut contents = String::new();
                std::io::Read::read_to_string(&mut file, &mut contents)?;
                assert_eq!(
                    contents,
                    if prebuilt {
                        "prepared initrd"
                    } else {
                        "base initrd"
                    }
                );
            }
            Ok(())
        })
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn shares_get_private_ports_after_all_backend_modifiers() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let logs = PetriLogSource::file_only(temp.path())?;
        let input = temp.path().join("boot-input");
        fs_err::write(&input, b"unused boot input")?;
        pal_async::DefaultPool::run_with(async |driver| {
            for custom_topology in [false, true] {
                let mut builder = PetriVmBuilder::minimal(
                    "shares",
                    &logs,
                    PetriVmArtifacts {
                        backend: OpenVmmPetriBackend::from_binary("unused-openvmm".into()),
                        firmware: Firmware::LinuxDirect {
                            kernel: ResolvedArtifact::from_local_path(input.clone()),
                            initrd: ResolvedArtifact::from_local_path(input.clone()),
                        },
                        arch: MachineArch::host(),
                        agent_image: None,
                        openhcl_agent_image: None,
                        pipette_binary: None,
                        nested_test: None,
                    },
                    &driver,
                )?
                .with_shared_directory("z-data", temp.path(), "/data")
                .with_shared_directory("a-cache", temp.path(), "/cache");
                if custom_topology {
                    builder = builder.modify_backend(|b| {
                        b.with_pcie_root_topology_at(1, 1, 1, 1)
                            .with_custom_config(|c| {
                                c.pcie_root_complexes[0].ports[0].name = "petri-share-0".into();
                                c.pcie_devices.push(openvmm_defs::config::PcieDeviceConfig {
                                    port_name: "petri-share-0".into(),
                                    resource: virtio_resources::VirtioPciDeviceHandle(
                                        virtio_resources::rng::VirtioRngHandle.into_resource(),
                                    )
                                    .into_resource(),
                                });
                            })
                    });
                }
                let backend = configure(builder).await?;
                let [root] = backend.config.pcie_root_complexes.as_slice() else {
                    panic!("expected one root complex")
                };
                assert_eq!(root.segment, u16::from(custom_topology));
                let first_share = usize::from(custom_topology);
                for i in 0..2 {
                    let device = &backend.config.pcie_devices[first_share + i];
                    assert_eq!(device.port_name, format!("petri-share-{}", first_share + i));
                    assert!(root.ports.iter().any(|p| p.name == device.port_name));
                }
                assert_eq!(root.ports.len(), first_share + 2);
                assert!(backend.guest_shares.is_empty());
            }
            Ok(())
        })
    }

    #[test]
    fn isolated_agent_is_external_only_when_requested() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let logs = PetriLogSource::file_only(temp.path())?;
        let igvm = temp.path().join("guest.igvm");
        fs_err::write(&igvm, b"unused measured image")?;
        pal_async::DefaultPool::run_with(async |driver| {
            for requested in [false, true] {
                let builder = PetriVmBuilder::minimal(
                    "isolated",
                    &logs,
                    PetriVmArtifacts {
                        backend: OpenVmmPetriBackend::from_binary("unused-openvmm".into()),
                        firmware: Firmware::IgvmLinuxDirect {
                            igvm_path: ResolvedArtifact::from_local_path(igvm.clone()),
                            isolation: crate::IsolationType::Snp,
                        },
                        arch: MachineArch::X86_64,
                        agent_image: None,
                        openhcl_agent_image: None,
                        pipette_binary: requested
                            .then(|| ResolvedArtifact::from_local_path("unused-pipette".into())),
                        nested_test: None,
                    },
                    &driver,
                )?
                .with_serial_output();
                assert_eq!(builder.using_vtl0_pipette(), requested);
                assert!(!builder.properties().uses_pipette_as_init);
                assert!(builder.pipette_binary.is_none());
                let backend = configure(builder).await?;
                assert!(backend.resources.linux_direct_serial_agent.is_none());
                assert!(matches!(
                    backend.config.load_mode,
                    openvmm_defs::config::LoadMode::Igvm { .. }
                ));
            }
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
            let builder = builder_with_guest(
                &driver,
                &logs,
                firmware.path(),
                mshv_guest(temp.path())?,
                MachineArch::X86_64,
            )?
            .modify_backend(|b| b.with_pcie_root_topology(1, 1, 1))
            .with_mshv_platform()
            .with_shared_directory("data", temp.path(), "/mounted elsewhere")
            .modify_backend(|b| b.with_pcie_root_topology(1, 1, 1));
            assert!(matches!(
                builder.config.firmware,
                Firmware::Uefi {
                    guest: UefiGuest::Mshv(_),
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
            for port in ["s1rc0rp0", "s1rc0rp1", "petri-share-0"] {
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
    #[should_panic(expected = "PCI segment range overlaps an existing root complex")]
    fn segment_conflicts_stop_later_modifiers() {
        let temp = tempfile::tempdir().unwrap();
        let firmware = tempfile::NamedTempFile::new().unwrap();
        let logs = PetriLogSource::file_only(temp.path()).unwrap();
        pal_async::DefaultPool::run_with(async |driver| {
            let builder = builder_with_guest(
                &driver,
                &logs,
                firmware.path(),
                UefiGuest::None,
                MachineArch::host(),
            )?
            .modify_backend(|b| b.with_pcie_root_topology_at(1, 1, 1, 1))
            .modify_backend(|b| b.with_pcie_root_topology_at(1, 1, 1, 1))
            .modify_backend(|_| panic!("segment conflict should stop later modifiers"));
            configure(builder).await?;
            Ok::<_, anyhow::Error>(())
        })
        .unwrap();
    }
}
