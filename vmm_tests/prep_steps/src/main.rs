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
use petri::pipette::PipetteClient;
use petri::pipette::cmd;
use petri_artifacts_common::tags::MachineArch;
use pipette_protocol::PIPETTE_PORT;
use vm_resource::IntoResource;

fn main() -> anyhow::Result<()> {
    DefaultPool::run_with(async |driver| async_main(&driver).await)
}

async fn async_main(driver: &pal_async::DefaultDriver) -> anyhow::Result<()> {
    let step = std::env::args().nth(1).unwrap_or_default();
    match step.as_str() {
        "" | "standard" => {
            let name = "prep_steps";
            let (logger, artifacts, source_disk) = build(name)?;
            let r = run(driver, name, &logger, artifacts, source_disk).await;
            logger.log_test_result(name, &r, false);
            r
        }
        "no-vmbus" => {
            let name = "prep_steps_no_vmbus";
            let (logger, artifacts, source_disk, virtio_win) = build_no_vmbus(name)?;
            let r = run_no_vmbus(driver, name, &logger, artifacts, source_disk, virtio_win).await;
            logger.log_test_result(name, &r, false);
            r
        }
        other => anyhow::bail!("unknown prep step: {other:?} (expected 'standard' or 'no-vmbus')"),
    }
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
                    resolver.require_source(petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64, petri::RemoteAccess::Allow),
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
    let logger = petri::try_init_tracing(output_dir, tracing::level_filters::LevelFilter::DEBUG)?;
    Ok((logger, artifacts, source_disk.erase()))
}

async fn run(
    driver: &pal_async::DefaultDriver,
    name: &str,
    logger: &PetriLogSource,
    artifacts: PetriVmArtifacts<OpenVmmPetriBackend>,
    source_disk: ResolvedArtifact,
) -> anyhow::Result<()> {
    tracing::info!("Running VMM test prep steps");

    let source_disk = source_disk.get();
    let Some((result_disk_path, drop_guard)) = prepare_result_disk(source_disk, "-prepped.vhd")?
    else {
        return Ok(());
    };

    // Randomize GPT GUIDs so the result disk doesn't collide with the source
    // if they're ever both attached to the same VM.
    change_gpt_disk_guid(&result_disk_path)?;

    let result_disk = openvmm_helpers::disk::open_disk_type(
        &result_disk_path,
        openvmm_helpers::disk::OpenDiskOptions {
            read_only: false,
            direct: false,
        },
    )
    .await?;

    let (vm, agent) =
        boot_vm_with_target_disk(driver, name, logger, artifacts, result_disk).await?;

    copy_imc_to_target(&agent).await?;

    // Unload the target hive.
    let shell = agent.windows_shell();
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
}

fn build_no_vmbus(
    name: &str,
) -> anyhow::Result<(
    PetriLogSource,
    PetriVmArtifacts<OpenVmmPetriBackend>,
    ResolvedArtifact,
    ResolvedArtifact,
)> {
    // Use WS2022 as both the boot VM and the source disk to prep.
    let (artifacts, source_disk, output_dir, virtio_win) = build_with_artifacts(
        name,
        |resolver| {
            let boot_disk = resolver.require_source(
                petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64,
                petri::RemoteAccess::Allow,
            );
            let artifacts = PetriVmArtifacts::<OpenVmmPetriBackend>::new(
                &resolver,
                Firmware::uefi(
                    &resolver,
                    MachineArch::X86_64,
                    UefiGuest::Vhd(BootImageConfig::from_vhd(boot_disk)),
                ),
                MachineArch::X86_64,
                true,
            )
            .unwrap();
            let source_disk = resolver.require(
                petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64,
            );
            let output_dir =
                resolver.require(petri_artifacts_common::artifacts::TEST_LOG_DIRECTORY);
            let virtio_win = resolver
                .require(petri_artifacts_vmm_test::artifacts::virtio_win::VIRTIO_WIN_DRIVERS);
            (artifacts, source_disk, output_dir, virtio_win)
        },
    )?;

    let output_dir = output_dir.get();
    let logger = petri::try_init_tracing(output_dir, tracing::level_filters::LevelFilter::DEBUG)?;
    Ok((logger, artifacts, source_disk.erase(), virtio_win.erase()))
}

async fn run_no_vmbus(
    driver: &pal_async::DefaultDriver,
    name: &str,
    logger: &PetriLogSource,
    artifacts: PetriVmArtifacts<OpenVmmPetriBackend>,
    source_disk: ResolvedArtifact,
    virtio_win: ResolvedArtifact,
) -> anyhow::Result<()> {
    use anyhow::Context;

    tracing::info!("Running no-vmbus prep steps");

    let source_disk = source_disk.get();
    let Some((result_disk_path, drop_guard)) =
        prepare_result_disk(source_disk, "-no-vmbus-prepped.vhd")?
    else {
        return Ok(());
    };

    change_gpt_disk_guid(&result_disk_path)?;
    tracing::info!("Changed target disk GUID to avoid collision with boot disk.");

    // Read NetKVM driver files from the virtio-win artifact.
    let driver_dir = virtio_win.get().join("NetKVM/2k22/amd64");
    const NETKVM_FILES: &[&str] = &[
        "netkvm.cat",
        "netkvm.inf",
        "netkvm.sys",
        "netkvmco.exe",
        "netkvmp.exe",
    ];
    let driver_files: Vec<(&str, Vec<u8>)> = NETKVM_FILES
        .iter()
        .map(|name| {
            let path = driver_dir.join(name);
            let data = std::fs::read(&path).with_context(|| {
                format!("failed to read NetKVM driver file: {}", path.display())
            })?;
            Ok((*name, data))
        })
        .collect::<anyhow::Result<_>>()?;

    let result_disk = openvmm_helpers::disk::open_disk_type(
        &result_disk_path,
        openvmm_helpers::disk::OpenDiskOptions {
            read_only: false,
            direct: false,
        },
    )
    .await?;

    let (vm, agent) =
        boot_vm_with_target_disk(driver, name, logger, artifacts, result_disk).await?;

    let shell = agent.windows_shell();

    // Create the driver directory on the target disk.
    cmd!(shell, "cmd.exe /c mkdir E:\\drivers").run().await?;

    // Copy NetKVM driver files to the target disk for offline injection.
    for (name, data) in &driver_files {
        agent
            .write_file(&format!("E:\\drivers\\{name}"), data.as_slice())
            .await?;
    }

    // Inject the NetKVM driver into the offline Windows installation on E:.
    cmd!(shell, "dism.exe")
        .args([
            "/image:E:\\",
            "/add-driver",
            "/driver:E:\\drivers\\netkvm.inf",
        ])
        .run()
        .await?;

    copy_imc_to_target(&agent).await?;

    // Override pipette ImagePath to use TCP transport — Windows has no
    // virtio-vsock driver, so pipette listens on TCP instead.
    cmd!(shell, "reg")
        .args([
            "add",
            "HKLM\\TargetTemp\\ControlSet001\\Services\\pipette",
            "/v",
            "ImagePath",
            "/t",
            "REG_EXPAND_SZ",
            "/d",
            "D:\\pipette.exe --service --transport tcp",
            "/f",
        ])
        .run()
        .await?;

    // Add a Windows Firewall rule to allow inbound TCP on the pipette
    // port. Without this, the firewall blocks the consomme port forward
    // connection.
    let firewall_rule = format!(
        "v2.10|Action=Allow|Active=TRUE|Dir=In|Protocol=6|LPort={}|Name=Pipette TCP|",
        PIPETTE_PORT
    );
    cmd!(shell, "reg")
        .args([
            "add",
            "HKLM\\TargetTemp\\ControlSet001\\Services\\SharedAccess\\Parameters\\FirewallPolicy\\FirewallRules",
            "/v",
            "pipette-tcp-in",
            "/t",
            "REG_SZ",
            "/d",
            &firewall_rule,
            "/f",
        ])
        .run()
        .await?;

    cmd!(shell, "reg")
        .args(["unload", "HKLM\\TargetTemp"])
        .run()
        .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    std::mem::forget(drop_guard);
    tracing::info!("No-vmbus prep steps completed successfully.");

    Ok(())
}

/// Boot a prep VM with a target disk attached as a second SCSI controller.
///
/// The target disk is exposed to the guest as drive E:. Returns the running
/// VM and pipette agent.
async fn boot_vm_with_target_disk(
    driver: &pal_async::DefaultDriver,
    name: &str,
    logger: &PetriLogSource,
    artifacts: PetriVmArtifacts<OpenVmmPetriBackend>,
    target_disk: vm_resource::Resource<vm_resource::kind::DiskHandleKind>,
) -> anyhow::Result<(petri::PetriVm<OpenVmmPetriBackend>, PipetteClient)> {
    PetriVmBuilder::new(
        PetriTestParams {
            test_name: name,
            logger,
            // FUTURE: To properly support post_test_hooks we'd need to catch panics
            // and early failure returns. Not worth it for this simple prep step tool.
            post_test_hooks: &mut vec![],
        },
        artifacts,
        driver,
    )?
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
                            disk: target_disk,
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
    .await
}

/// Copy the IMC hive's pipette service registration into the target disk's
/// SYSTEM hive.
///
/// Loads the IMC hive and the target's SYSTEM hive, then copies
/// `CurrentControlSet` into `ControlSet001`. Leaves `HKLM\TargetTemp`
/// loaded so the caller can make further modifications before unloading.
async fn copy_imc_to_target(agent: &PipetteClient) -> anyhow::Result<()> {
    // Reuse the IMC hive from petri/guest-bootstrap to configure pipette.
    // This ensures we stay in sync with any changes in petri.
    agent
        .write_file(
            "C:\\imc.hiv",
            include_bytes!("../../../petri/guest-bootstrap/imc.hiv").as_slice(),
        )
        .await?;

    // No need to unload IMCTemp — the VM is powered off after prep.
    let shell = agent.windows_shell();
    cmd!(shell, "reg")
        .args(["load", "HKLM\\IMCTemp", "C:\\imc.hiv"])
        .run()
        .await?;

    cmd!(shell, "reg")
        .args([
            "load",
            "HKLM\\TargetTemp",
            "E:\\Windows\\System32\\config\\SYSTEM",
        ])
        .run()
        .await?;

    // Copy the keys over. Until a machine boots it doesn't have a
    // 'CurrentControlSet', so we copy to 'ControlSet001' instead.
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

    Ok(())
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

/// Copy a source VHD to a result disk with the given suffix, handling reuse.
///
/// Returns `None` if the result disk already exists and `PETRI_REUSE_PREPPED_VHDS`
/// is set. Otherwise returns the result path and a drop guard that deletes the
/// file on failure (caller must `std::mem::forget` the guard on success).
fn prepare_result_disk(
    source_disk: &std::path::Path,
    suffix: &str,
) -> anyhow::Result<Option<(std::path::PathBuf, DeleteFileOnDrop)>> {
    let result_disk = source_disk.with_file_name(
        source_disk
            .file_name()
            .unwrap()
            .to_string_lossy()
            .replace(".vhd", suffix),
    );
    if result_disk.exists() {
        if std::env::var("PETRI_REUSE_PREPPED_VHDS")
            .ok()
            .is_some_and(|v| v.eq_ignore_ascii_case("true") || v == "1")
        {
            tracing::info!("Result disk already exists, skipping...");
            return Ok(None);
        } else {
            tracing::warn!("Result disk already exists, recreating it.");
        }
    } else {
        tracing::info!("Copying source disk to result disk.");
    }
    let drop_guard = DeleteFileOnDrop(result_disk.clone());
    std::fs::copy(source_disk, &result_disk)?;
    tracing::info!("Copied source disk successfully.");
    Ok(Some((result_disk, drop_guard)))
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

/// Change the GPT disk and partition GUIDs in a fixed VHD file so that
/// Windows doesn't treat it as a duplicate of another disk with the same
/// GUIDs.
fn change_gpt_disk_guid(path: &std::path::Path) -> anyhow::Result<()> {
    use std::io::Seek;

    let sector_size = 512u64;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let mut gpt = gptman::GPT::read_from(&mut file, sector_size)?;
    gpt.header.disk_guid = guid::Guid::new_random().into();
    // Also change partition GUIDs so Windows doesn't see duplicate volumes.
    for (_, partition) in gpt.iter_mut() {
        partition.unique_partition_guid = guid::Guid::new_random().into();
    }
    file.seek(std::io::SeekFrom::Start(0))?;
    gpt.write_into(&mut file)?;
    Ok(())
}
