// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Nested vm host definitions

use petri::PetriVmBuilder;
#[cfg(target_os = "linux")]
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

/// Reboot a nested-enabled Linux guest and verify virtualization remains exposed.
#[cfg(target_os = "linux")]
#[vmm_test_macros::vmm_test_with(
    openvmm,
    requires(nested_virt, nested_reset),
    configs(linux_direct_x64)
)]
async fn reboot_nested_virt(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let (mut vm, mut agent) = config
        .with_nested_virt()
        .with_no_vmbus()
        .with_processor_topology(petri::ProcessorTopology {
            vp_count: 2,
            ..Default::default()
        })
        .modify_backend(|b| b.with_pcie_root_topology(1, 1, 1))
        .run()
        .await?;

    let mut previous_boot_id = None;
    for boot in 0..=3 {
        agent.ping().await?;
        let shell = agent.unix_shell();
        let boot_id = shell.read_file("/proc/sys/kernel/random/boot_id").await?;
        anyhow::ensure!(
            previous_boot_id.as_ref() != Some(&boot_id),
            "boot {boot}: guest did not reboot"
        );
        previous_boot_id = Some(boot_id);

        let cpuinfo = shell.read_file("/proc/cpuinfo").await?;
        let flags: Vec<_> = cpuinfo
            .lines()
            .filter_map(|line| line.split_once(':'))
            .filter(|(name, _)| name.trim() == "flags")
            .map(|(_, flags)| flags)
            .collect();
        anyhow::ensure!(
            flags.len() == 2,
            "boot {boot}: expected flags for 2 CPUs, found {}",
            flags.len()
        );
        anyhow::ensure!(
            flags.iter().all(|flags| flags
                .split_whitespace()
                .any(|flag| matches!(flag, "vmx" | "svm"))),
            "boot {boot}: nested virtualization is not exposed on every CPU"
        );

        if boot != 3 {
            agent.reboot().await?;
            agent = vm.wait_for_reset().await?;
        }
    }

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
