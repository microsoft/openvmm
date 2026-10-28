// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Nested vm host definitions

use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::openvmm::OpenVmmPetriBackend;

pub(crate) async fn vm_host(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_nested_virt()
        .with_no_vmbus()
        .modify_backend(|b| b.with_pcie_root_topology(1, 1, 1))
        .run()
        .await?;

    vm.run_nested_test(&agent, Default::default()).await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

#[cfg(target_os = "linux")]
#[vmm_test_macros::vmm_test(
    qemu_linux_direct_aarch64,
    openvmm_linux_direct_x64,
    openvmm_linux_direct_aarch64
)]
async fn shared_directory<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    fs_err::write(
        directory.path().join("marker"),
        b"shared after agent startup",
    )?;
    let cache = tempfile::tempdir()?;
    fs_err::write(cache.path().join("marker"), b"child share")?;
    let (vm, agent) = config
        .with_shared_directory("a-cache", cache.path(), "/shared directory/cache")
        .with_shared_directory("z-data", directory.path(), "/shared directory/././")
        .run()
        .await?;
    assert_eq!(
        agent.read_file("/shared directory/marker").await?,
        b"shared after agent startup",
    );
    agent
        .write_file("/shared directory/result", b"guest write".as_slice())
        .await?;
    assert_eq!(
        fs_err::read(directory.path().join("result"))?,
        b"guest write"
    );
    assert_eq!(
        agent.read_file("/shared directory/cache/marker").await?,
        b"child share",
    );
    agent
        .write_file("/shared directory/cache/result", b"child write".as_slice())
        .await?;
    assert_eq!(fs_err::read(cache.path().join("result"))?, b"child write");
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
