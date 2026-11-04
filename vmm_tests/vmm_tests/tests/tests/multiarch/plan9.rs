// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use openvmm_defs::config::PcieDeviceConfig;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use std::os::unix::fs::MetadataExt;
use virtio_resources::VirtioPciDeviceHandle;
use virtio_resources::p9::VirtioPlan9Handle;
use vm_resource::IntoResource;
use vmm_test_macros::openvmm_test;

#[openvmm_test(
    uefi_x64(vhd(ubuntu_2504_server_x64)),
    uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
)]
async fn plan9_file_operations(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    fs_err::write(directory.path().join("host-file"), b"from host")?;
    std::os::unix::fs::symlink("host-file", directory.path().join("host-link"))?;
    let share = VirtioPlan9Handle {
        tag: "plan9".into(),
        root_path: directory
            .path()
            .to_str()
            .context("Plan 9 share path is not UTF-8")?
            .to_owned(),
        debug: false,
    }
    .into_resource();
    let (vm, agent) = config
        .modify_backend(move |b| {
            // Plan 9 fids and open files do not support save/restore.
            b.without_save_restore()
                .with_pcie_root_topology(1, 1, 1)
                .with_custom_config(|c| {
                    c.pcie_devices.push(PcieDeviceConfig {
                        port_name: "s0rc0rp0".into(),
                        resource: VirtioPciDeviceHandle(share).into_resource(),
                    });
                })
        })
        .run()
        .await?;
    let shell = agent.unix_shell();
    cmd!(shell, "mkdir -p /mnt/plan9").run().await?;
    cmd!(
        shell,
        "mount -t 9p -o trans=virtio,version=9p2000.L plan9 /mnt/plan9"
    )
    .run()
    .await?;
    agent
        .write_file(
            "/tmp/plan9_file_operations.py",
            include_bytes!("../../../test_data/plan9_file_operations.py").as_slice(),
        )
        .await?;
    cmd!(shell, "python3 /tmp/plan9_file_operations.py")
        .run()
        .await?;

    assert_eq!(fs_err::read(directory.path().join("file"))?, b"from guest");
    assert_eq!(
        fs_err::read_link(directory.path().join("symlink"))?,
        std::path::Path::new("file")
    );
    let file = fs_err::metadata(directory.path().join("file"))?;
    let link = fs_err::metadata(directory.path().join("hardlink"))?;
    assert_eq!(file.ino(), link.ino());
    assert_eq!(file.nlink(), 2);
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
