// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Perform preparation steps for our VMM tests.
//!
//! Currently this means booting a Windows VM to perform one task:
//! 1. Mount the VHD that will be used for Windows-based CVM tests and install
//!    pipette into it.
//!
//! This tool is intentionally as minimal as possible, to keep tests easily
//! reproducible. Anything that can be done through pipette during the test
//! run should be done there instead of here.

#![forbid(unsafe_code)]

use pal_async::DefaultPool;
use petri::ArtifactResolver;
use petri::BootImageConfig;
use petri::Firmware;
use petri::PetriLogSource;
use petri::PetriTestParams;
use petri::PetriVmArtifacts;
use petri::PetriVmBuilder;
use petri::ResolvedArtifact;
use petri::TestArtifactRequirements;
use petri::UefiGuest;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use petri_artifacts_common::tags::MachineArch;
use vm_resource::IntoResource;

fn main() -> anyhow::Result<()> {
    let name = "prep_steps";
    let (logger, artifacts, source_disk) = build(name)?;
    let r = run(name, &logger, artifacts, source_disk);
    logger.log_test_result(name, &r);
    r
}

fn build(
    name: &str,
) -> anyhow::Result<(
    PetriLogSource,
    PetriVmArtifacts<OpenVmmPetriBackend>,
    ResolvedArtifact,
)> {
    // Create a VM config that should be able to run anywhere and boot quickly:
    // an OpenVMM UEFI x86_64 VM with a DataCenterCore Windows image.
    let (artifacts, source_disk, output_dir) = build_with_artifacts(name, |resolver| {
        let artifacts = PetriVmArtifacts::<OpenVmmPetriBackend>::new(
            &resolver,
            Firmware::uefi(
                &resolver,
                MachineArch::X86_64,
                UefiGuest::Vhd(BootImageConfig::from_vhd(
                    resolver.require(petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64),
                )),
            ),
            MachineArch::X86_64,
            true,
        )
        .unwrap();
        let source_disk = resolver.require(
            petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64,
        );
        let output_dir = resolver.require(petri_artifacts_common::artifacts::TEST_LOG_DIRECTORY);
        (artifacts, source_disk, output_dir)
    })?;

    let output_dir = output_dir.get();
    let logger = petri::try_init_tracing(output_dir)?;
    Ok((logger, artifacts, source_disk.erase()))
}

fn run(
    name: &str,
    logger: &PetriLogSource,
    artifacts: PetriVmArtifacts<OpenVmmPetriBackend>,
    source_disk: ResolvedArtifact,
) -> anyhow::Result<()> {
    tracing::info!("Running VMM test prep steps");

    let source_disk = source_disk.get();
    // FUTURE: This file path should be obtainable from the artifact infrstructure.
    // For now the logic of getting a prepped image filename from its source is
    // duplicated.
    let result_disk = source_disk.with_file_name(
        source_disk
            .file_name()
            .unwrap()
            .to_string_lossy()
            .replace(".vhd", "-prepped.vhd"),
    );
    if result_disk.exists() {
        tracing::warn!("Result disk already exists, recreating it.");
    } else {
        tracing::info!("Copying source disk to result disk.");
    }
    // Create a drop guard so that if anything goes wrong anywhere the incomplete
    // result disk is deleted.
    let drop_guard = DeleteFileOnDrop(result_disk.clone());
    std::fs::copy(source_disk, &result_disk)?;
    tracing::info!("Copied source disk successfully.");
    let result_disk = openvmm_helpers::disk::open_disk_type(&result_disk, false)?;

    DefaultPool::run_with(async move |driver| {
        let (vm, agent) = PetriVmBuilder::new(
            PetriTestParams {
                test_name: name,
                logger,
                // FUTURE: To properly support post_test_hooks we'd need to catch panics
                // and early failure returns. Not worth it for this simple prep step tool.
                post_test_hooks: &mut vec![],
            },
            artifacts,
            &driver,
        )?
        // Add the second disk as a separate controller to avoid interfering with
        // the boot disk.
        .modify_backend(|v| {
            v.with_custom_config(|c| {
                c.vmbus_devices.push((
                    openvmm_defs::config::DeviceVtl::Vtl0,
                    storvsp_resources::ScsiControllerHandle {
                        instance_id: guid::guid!("766e96f8-2ceb-437e-afe3-a93169e48aff"),
                        max_sub_channel_count: 1,
                        io_queue_depth: None,
                        devices: vec![storvsp_resources::ScsiDeviceAndPath {
                            path: storvsp_resources::ScsiPath {
                                path: 0,
                                target: 0,
                                lun: 0,
                            },
                            device: scsidisk_resources::SimpleScsiDiskHandle {
                                read_only: false,
                                parameters: Default::default(),
                                disk: result_disk,
                            }
                            .into_resource(),
                        }],
                        requests: None,
                        poll_mode_queue_depth: None,
                    }
                    .into_resource(),
                ))
            })
        })
        .run()
        .await?;

        // Reuse the IMC hive from petri/guest-bootstrap to configure pipette.
        // This ensures we stay in sync with any changes in petri.
        agent
            .write_file(
                "C:\\imc.hiv",
                include_bytes!("../../../petri/guest-bootstrap/imc.hiv").as_slice(),
            )
            .await?;

        // Load the IMC hive to read keys from.
        let shell = agent.windows_shell();
        cmd!(shell, "reg")
            .args(["load", "HKLM\\IMCTemp", "C:\\imc.hiv"])
            .run()
            .await?;

        // Load the target's SYSTEM hive to write to.
        cmd!(shell, "reg")
            .args([
                "load",
                "HKLM\\TargetTemp",
                "E:\\Windows\\System32\\config\\SYSTEM",
            ])
            .run()
            .await?;

        // Copy the keys over.
        // Until a machine boots it doesn't have a 'CurrentControlSet', so we
        // copy to 'ControlSet001' instead.
        cmd!(shell, "reg")
            .args([
                "copy",
                "HKLM\\IMCTemp\\SYSTEM\\CurrentControlSet",
                "HKLM\\TargetTemp\\ControlSet001",
                "/s",
                "/f",
            ])
            .run()
            .await?;

        // Unload the target hive.
        cmd!(shell, "reg")
            .args(["unload", "HKLM\\TargetTemp"])
            .run()
            .await?;

        agent.power_off().await?;
        vm.wait_for_clean_teardown().await?;

        // Now that everything is done we can keep the file.
        std::mem::forget(drop_guard);
        tracing::info!("Prep steps completed successfully.");

        Ok(())
    })
}

fn build_with_artifacts<R>(
    name: &str,
    mut f: impl FnMut(ArtifactResolver<'_>) -> R,
) -> anyhow::Result<R> {
    let resolver =
        petri_artifact_resolver_openvmm_known_paths::OpenvmmKnownPathsTestArtifactResolver::new(
            name,
        );
    let mut requirements = TestArtifactRequirements::new();
    f(ArtifactResolver::collector(&mut requirements));
    let artifacts = requirements.resolve(&resolver)?;
    Ok(f(ArtifactResolver::resolver(&artifacts)))
}

struct DeleteFileOnDrop(std::path::PathBuf);

impl Drop for DeleteFileOnDrop {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0) {
            tracing::error!("Failed to delete file {}: {}", self.0.display(), e);
        } else {
            tracing::info!("Deleted file {}", self.0.display());
        }
    }
}
