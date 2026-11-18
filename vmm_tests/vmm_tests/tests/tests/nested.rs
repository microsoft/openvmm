// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Nested vm host definitions

use anyhow::Context as _;
use petri::PetriVmBuilder;
#[cfg(target_os = "linux")]
use petri::PetriVmmBackend;
use petri::openvmm::OpenVmmPetriBackend;
use petri_artifacts_vmm_test::artifacts::host_tools::NEXTEST_VMM_TESTS_ARCHIVE_LINUX_X64_MUSL;

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

pub(crate) async fn vm_host_mshv(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_mshv_platform()
        .with_memory(petri::MemoryConfig {
            startup_bytes: 8 * petri::SIZE_1_GB,
            ..Default::default()
        })
        .with_processor_topology(petri::ProcessorTopology {
            vp_count: 2,
            ..Default::default()
        })
        .run()
        .await?;
    vm.run_nested_test(&agent, Default::default()).await?;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Run the L2 test before and after an L1 reboot to exercise used nested state.
async fn reboot_l1(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let (mut vm, mut agent) = config
        .with_nested_virt()
        .with_no_vmbus()
        .modify_backend(|b| b.with_pcie_root_topology(1, 1, 1))
        .run()
        .await?;

    let mut previous_boot_id = None;
    for boot in 0..=1 {
        agent.ping().await?;
        let boot_id = agent
            .unix_shell()
            .read_file("/proc/sys/kernel/random/boot_id")
            .await?;
        anyhow::ensure!(
            previous_boot_id.as_ref() != Some(&boot_id),
            "boot {boot}: L1 guest did not reboot"
        );
        previous_boot_id = Some(boot_id);
        tracing::info!(boot, "running L2 test");
        vm.run_nested_test_with_suffix(
            &agent,
            Default::default(),
            if boot == 0 { "_before" } else { "_after" },
        )
        .await
        .with_context(|| format!("boot {boot}: L2 test failed"))?;

        if boot == 0 {
            agent.reboot().await?;
            agent = vm.wait_for_reset().await?;
        }
    }

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Reboot a two-VP Linux L1 three times with nesting enabled and VMBus disabled.
/// Check changed boot IDs, connectivity, and VMX/SVM exposure on both VPs.
/// This does not launch an L2; `reboot_l2` separately exercises an L2 reboot.
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

/// Reboot a Linux L2 three times. The `reboot_l1` variant reruns this after
/// rebooting L1, proving L2 can launch after previously used nested state resets.
#[vmm_test_macros::vmm_test(
    nested(
        (vm_host, openvmm_linux_direct_x64),
        (NEXTEST_VMM_TESTS_ARCHIVE_LINUX_X64_MUSL, "tests", "nested", openvmm_linux_direct_x64),
    ),
    nested(
        (reboot_l1, openvmm_linux_direct_x64, requires(nested_reset)),
        (NEXTEST_VMM_TESTS_ARCHIVE_LINUX_X64_MUSL, "tests", "nested", openvmm_linux_direct_x64),
    ),
)]
async fn reboot_l2(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let (mut vm, mut agent) = config.run().await?;
    let mut previous_boot_id = None;
    for boot in 0..=3 {
        agent.ping().await?;
        let boot_id = agent
            .unix_shell()
            .read_file("/proc/sys/kernel/random/boot_id")
            .await?;
        anyhow::ensure!(
            previous_boot_id.as_ref() != Some(&boot_id),
            "boot {boot}: L2 guest did not reboot"
        );
        previous_boot_id = Some(boot_id);
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
