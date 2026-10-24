// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use std::os::unix::fs::MetadataExt;
use vmm_test_macros::openvmm_test;

#[openvmm_test(
    uefi_x64(vhd(ubuntu_2404_server_x64)),
    uefi_aarch64(vhd(ubuntu_2404_server_aarch64))
)]
async fn plan9_file_operations(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    fs_err::write(directory.path().join("host-file"), b"from host")?;
    std::os::unix::fs::symlink("host-file", directory.path().join("host-link"))?;
    let (vm, agent) = config
        .with_shared_directory("plan9", directory.path(), "/mnt/plan9")
        .run()
        .await?;
    agent
        .write_file(
            "/tmp/plan9_file_operations.py",
            include_bytes!("../../../test_data/plan9_file_operations.py").as_slice(),
        )
        .await?;
    let shell = agent.unix_shell();
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
