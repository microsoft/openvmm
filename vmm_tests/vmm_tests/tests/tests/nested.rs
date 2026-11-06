// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Nested vm host definitions

use anyhow::Context;
use petri::PetriVmBuilder;
#[cfg(target_os = "linux")]
use petri::PetriVmmBackend;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::process::Stdio;
use petri_artifacts_vmm_test::env::VMM_TESTS_CONTENT_DIR;

const GUEST_PROCESS_SNAPSHOT: &str = r#"
exec >>"$1" 2>&1
printf 'Snapshot scheduled, collector PID %s\n' "$$"
sleep 60
printf '\nSnapshot started\n'
date -u
for process in /proc/[0-9]*; do
    printf '\n=== %s ===\n' "$process"
    if [ ! -r "$process/status" ]; then
        printf 'Process disappeared or status is unavailable\n'
        continue
    fi
    cat "$process/status"
    printf 'Command: '
    tr '\000' ' ' <"$process/cmdline"
    printf '\nFile descriptors:\n'
    ls -l "$process/fd"
    for thread in "$process"/task/[0-9]*; do
        printf '\n--- %s ---\n' "$thread"
        for field in comm status wchan syscall stack; do
            printf '\n%s:\n' "$field"
            cat "$thread/$field"
        done
    done
done
printf '\nSnapshot complete\n'
date -u
"#;

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
    let diagnostic_file = config
        .log_source()
        .create_attachment("guest-process-state.log")?;
    let content_dir = fs_err::canonicalize(
        std::env::var_os(VMM_TESTS_CONTENT_DIR).context("test content dir not set")?,
    )?;
    let relative = diagnostic_file
        .path()
        .strip_prefix(&content_dir)
        .context("diagnostic file should be under the shared test content dir")?
        .iter()
        .map(|part| part.to_str().context("non-UTF-8 diagnostic path"))
        .collect::<anyhow::Result<Vec<_>>>()?
        .join("/");
    let guest_path = format!("/cidata/vmm_tests/{relative}");
    drop(diagnostic_file);
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
    let prepared = agent
        .command("sh")
        .args([
            "-c",
            "printf 'Guest process diagnostic scheduled\\n' >>\"$1\"",
            "snapshot",
            &guest_path,
        ])
        .output()
        .await?;
    anyhow::ensure!(
        prepared.status.success(),
        "preparing guest diagnostic file failed: {}",
        String::from_utf8_lossy(&prepared.stderr)
    );
    // Collect independently of Pipette's result/output delivery, with a bounded lifetime.
    let diagnostic = agent
        .command("timeout")
        .args([
            "90",
            "sh",
            "-c",
            GUEST_PROCESS_SNAPSHOT,
            "snapshot",
            &guest_path,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .await?;
    drop(diagnostic);
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
