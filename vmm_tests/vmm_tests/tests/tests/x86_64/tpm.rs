// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use anyhow::ensure;
use petri::PetriGuestStateLifetime;
use petri::PetriVmBuilder;
use petri::ResolvedArtifact;
use petri::ShutdownKind;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use petri_artifact_resolver_openvmm_known_paths::get_repo_root;
use petri_artifacts_common::tags::OsFlavor;
use petri_artifacts_vmm_test::artifacts::guest_tools::TPM_GUEST_TESTS_LINUX_X64;
use petri_artifacts_vmm_test::artifacts::guest_tools::TPM_GUEST_TESTS_WINDOWS_X64;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::openvmm_test_no_agent;

const AK_CERT_NONZERO_BYTES: usize = 2500;
const AK_CERT_TOTAL_BYTES: usize = 4096;

fn expected_ak_cert_hex() -> String {
    use std::fmt::Write as _;

    let mut data = vec![0xab; AK_CERT_NONZERO_BYTES];
    data.resize(AK_CERT_TOTAL_BYTES, 0);

    let mut hex = String::with_capacity(data.len() * 2 + 2);
    hex.push_str("0x");
    for byte in data {
        write!(&mut hex, "{:02x}", byte).expect("write! to String should not fail");
    }

    hex
}

fn configure_ak_cert_persisted_vm(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> PetriVmBuilder<OpenVmmPetriBackend> {
    config
        .with_openhcl_command_line("HCL_ATTEMPT_AK_CERT_CALLBACK=1")
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .modify_backend(|b| {
            b.with_tpm()
                .with_tpm_state_persistence()
                .with_igvm_attest_test_config(
                    get_resources::ged::IgvmAttestTestConfig::AkCertPersistentAcrossBoot,
                )
        })
}

fn configure_ak_cert_retry_vm(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> PetriVmBuilder<OpenVmmPetriBackend> {
    config
        .with_openhcl_command_line("HCL_ATTEMPT_AK_CERT_CALLBACK=1")
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .modify_backend(|b| {
            b.with_tpm()
                .with_tpm_state_persistence()
                .with_igvm_attest_test_config(
                    get_resources::ged::IgvmAttestTestConfig::AkCertRequestFailureAndRetry,
                )
        })
}

fn prepped_windows_2025_disk_path() -> anyhow::Result<PathBuf> {
    let images_dir = std::env::var("VMM_TEST_IMAGES")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("images"));
    let images_dir = if images_dir.is_absolute() {
        images_dir
    } else {
        get_repo_root()?.join(images_dir)
    };

    let base_filename = petri_artifacts_vmm_test::artifacts::test_vhd::
        GEN2_WINDOWS_DATA_CENTER_CORE2025_X64::FILENAME;
    let prepped_filename = base_filename.replace(".vhd", "-prepped.vhd");
    Ok(images_dir.join(prepped_filename))
}

fn ensure_windows_2025_prepped_vhd() -> anyhow::Result<()> {
    static PREP_ONCE: OnceLock<()> = OnceLock::new();

    PREP_ONCE.get_or_try_init(|| {
        let prepped_path = prepped_windows_2025_disk_path()?;
        if prepped_path.exists() {
            return Ok(());
        }

        let status = Command::new("cargo")
            .current_dir(get_repo_root()?)
            .args(["run", "-p", "prep_steps"])
            .status()
            .context("failed to execute `cargo run -p prep_steps`")?;

        if !status.success() {
            anyhow::bail!("prep_steps exited with status {status}");
        }

        if !prepped_path.exists() {
            anyhow::bail!(
                "prep_steps completed but prepped VHD not found at {}",
                prepped_path.display()
            );
        }

        Ok(())
    })?;

    Ok(())
}

/// Basic boot tests with TPM enabled.
#[openvmm_test(
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))
)]
async fn boot_with_tpm(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let config = config.modify_backend(|b| b.with_tpm());

    let (vm, agent) = match os_flavor {
        OsFlavor::Windows => config.run().await?,
        OsFlavor::Linux => {
            config
                .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
                // TODO: this shouldn't be needed once with_tpm() is
                // backend-agnostic.
                .with_expect_reset()
                .run()
                .await?
        }
        _ => unreachable!(),
    };

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Test AK cert is persistent across boots on Linux.
// #[cfg_attr(target_os = "windows", ignore = "requires Linux guest tooling")]
#[openvmm_test(
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64]
)]
async fn tpm_ak_cert_persisted_linux(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    extra_deps: (ResolvedArtifact<TPM_GUEST_TESTS_LINUX_X64>,),
) -> anyhow::Result<()> {
    ensure!(
        config.os_flavor() == OsFlavor::Linux,
        "test invoked with unexpected guest flavor"
    );

    // First boot - AK cert request will be served by GED.
    // Second boot - Ak cert request will be bypassed by GED.
    let config = configure_ak_cert_persisted_vm(config);
    // TODO: with_expect_reset shouldn't be needed once with_tpm() is backend-agnostic.
    let (mut vm, mut agent) = config.with_expect_reset().run().await?;

    let (linux_artifact,) = extra_deps;
    let host_binary = linux_artifact.get();
    let guest_binary_path = "/tmp/tpm_guest_tests";

    let guest_binary = std::fs::read(host_binary)
        .with_context(|| format!("failed to read {}", host_binary.display()))?;
    agent
        .write_file(guest_binary_path, guest_binary.as_slice())
        .await?;

    let sh = agent.unix_shell();
    cmd!(sh, "chmod +x {guest_binary_path}").run().await?;

    let expected_hex = expected_ak_cert_hex();
    let output = cmd!(sh, "{guest_binary_path}")
        .args(["--ak-cert", "--expected-data-hex", expected_hex.as_str()])
        .read()
        .await?;

    ensure!(
        output.contains("AK certificate matches expected value"),
        format!("{output}")
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test AK cert is persistent across boots on Windows.
#[openvmm_test(
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[TPM_GUEST_TESTS_WINDOWS_X64]
)]
async fn tpm_ak_cert_persisted_windows(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    extra_deps: (ResolvedArtifact<TPM_GUEST_TESTS_WINDOWS_X64>,),
) -> anyhow::Result<()> {
    ensure!(
        config.os_flavor() == OsFlavor::Windows,
        "test invoked with unexpected guest flavor"
    );

    let config = configure_ak_cert_persisted_vm(config);
    let (mut vm, mut agent) = config.run().await?;

    // First boot - AK cert request will be served by GED.
    // Second boot - Ak cert request will be bypassed by GED.
    agent.reboot().await?;
    let mut agent = vm.wait_for_reset().await?;

    let (windows_artifact,) = extra_deps;
    let host_binary = windows_artifact.get();
    let guest_binary = std::fs::read(host_binary)
        .with_context(|| format!("failed to read {}", host_binary.display()))?;
    let guest_binary_path = "C:\\tpm_guest_tests.exe";

    agent
        .write_file(guest_binary_path, guest_binary.as_slice())
        .await
        .context("failed to copy tpm_guest_tests.exe into the guest")?;

    let sh = agent.windows_shell();
    let expected_hex = expected_ak_cert_hex();

    let output = cmd!(sh, "{guest_binary_path}")
        .args(["--ak-cert", "--expected-data-hex", expected_hex.as_str()])
        .read()
        .await
        .context("failed to execute tpm_guest_tests.exe inside the guest")?;

    ensure!(
        output.contains("AK certificate matches expected value"),
        format!("{output}")
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test AK cert retry logic on Linux.
// #[cfg_attr(target_os = "windows", ignore = "requires Linux guest tooling")]
#[openvmm_test(
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64]
)]
async fn tpm_ak_cert_retry_linux(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    extra_deps: (ResolvedArtifact<TPM_GUEST_TESTS_LINUX_X64>,),
) -> anyhow::Result<()> {
    ensure!(
        config.os_flavor() == OsFlavor::Linux,
        "test invoked with unexpected guest flavor"
    );

    let config = configure_ak_cert_retry_vm(config);
    // TODO: with_expect_reset shouldn't be needed once with_tpm() is backend-agnostic.
    let (mut vm, mut agent) = config.with_expect_reset().run().await?;

    let (linux_artifact,) = extra_deps;
    let host_binary = linux_artifact.get();
    let guest_binary_path = "/tmp/tpm_guest_tests";

    let guest_binary = std::fs::read(host_binary)
        .with_context(|| format!("failed to read {}", host_binary.display()))?;
    agent
        .write_file(guest_binary_path, guest_binary.as_slice())
        .await?;

    let sh = agent.unix_shell();
    cmd!(sh, "chmod +x {guest_binary_path}").run().await?;

    let first_attempt = cmd!(sh, "{guest_binary_path}")
        .args(["--ak-cert"])
        .read()
        .await;
    assert!(
        first_attempt.is_err(),
        "AK certificate read unexpectedly succeeded"
    );

    let expected_hex = expected_ak_cert_hex();
    let output = cmd!(sh, "{guest_binary_path}")
        .args([
            "--ak-cert",
            "--expected-data-hex",
            expected_hex.as_str(),
            "--retry",
            "3",
        ])
        .read()
        .await?;

    ensure!(
        output.contains("AK certificate matches expected value"),
        format!("{output}")
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test AK cert retry logic on Windows.
#[openvmm_test(
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[TPM_GUEST_TESTS_WINDOWS_X64]
)]
async fn tpm_ak_cert_retry_windows(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    extra_deps: (ResolvedArtifact<TPM_GUEST_TESTS_WINDOWS_X64>,),
) -> anyhow::Result<()> {
    ensure!(
        config.os_flavor() == OsFlavor::Windows,
        "test invoked with unexpected guest flavor"
    );

    let config = configure_ak_cert_retry_vm(config);
    let (mut vm, mut agent) = config.run().await?;

    let (windows_artifact,) = extra_deps;
    let host_binary = windows_artifact.get();
    let guest_binary = std::fs::read(host_binary)
        .with_context(|| format!("failed to read {}", host_binary.display()))?;
    let guest_binary_path = "C:\\tpm_guest_tests.exe";

    agent
        .write_file(guest_binary_path, guest_binary.as_slice())
        .await
        .context("failed to copy tpm_guest_tests.exe into the guest")?;

    {
        let sh = agent.windows_shell();
        let output = cmd!(sh, "{guest_binary_path}")
            .args(["--ak-cert"])
            .read()
            .await
            .context("failed to execute tpm_guest_tests.exe inside the guest")?;

        ensure!(
            output.contains("AK certificate data"),
            "tpm_guest_tests.exe --ak-cert did not report AK certificate data: {output}",
        );
    }

    agent.reboot().await?;
    let mut agent = vm.wait_for_reset().await?;

    let expected_hex = expected_ak_cert_hex();
    let sh = agent.windows_shell();
    let output = cmd!(sh, "{guest_binary_path}")
        .args([
            "--ak-cert",
            "--expected-data-hex",
            expected_hex.as_str(),
            "--retry",
            "3",
        ])
        .read()
        .await
        .context("failed to execute tpm_guest_tests.exe inside the guest")?;

    ensure!(
        output.contains("AK certificate matches expected value"),
        format!("{output}")
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// VBS boot test with attestation enabled
// TODO: Add in-guest tests to retrieve and verify the report.
#[openvmm_test_no_agent(
    openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2022_x64)),
    // openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64))
)]
async fn vbs_boot_with_attestation(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let config = config.modify_backend(|b| b.with_tpm().with_tpm_state_persistence());

    let mut vm = match os_flavor {
        OsFlavor::Windows => {
            config
                .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
                .run_without_agent()
                .await?
        }
        OsFlavor::Linux => {
            config
                .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
                // TODO: this shouldn't be needed once with_tpm() is
                // backend-agnostic.
                .with_expect_reset()
                .run_without_agent()
                .await?
        }
        _ => unreachable!(),
    };

    vm.send_enlightened_shutdown(ShutdownKind::Shutdown).await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Test that TPM platform hierarchy is disabled for guest access on Linux.
/// The platform hierarchy should only be accessible by the host/hypervisor.
#[openvmm_test(openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)))]
async fn tpm_test_platform_hierarchy_disabled(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let config = config
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .modify_backend(|b| b.with_tpm())
        // TODO: this shouldn't be needed once with_tpm() is
        // backend-agnostic.
        .with_expect_reset();

    let (vm, agent) = config.run().await?;

    // Use the python script to test that platform hierarchy operations fail
    const TEST_FILE: &str = "tpm_platform_hierarchy.py";
    const TEST_CONTENT: &str = include_str!("../../../test_data/tpm_platform_hierarchy.py");

    agent.write_file(TEST_FILE, TEST_CONTENT.as_bytes()).await?;
    assert_eq!(agent.read_file(TEST_FILE).await?, TEST_CONTENT.as_bytes());

    let sh = agent.unix_shell();
    let output = cmd!(sh, "python3 tpm_platform_hierarchy.py").read().await?;

    println!("TPM platform hierarchy test output: {}", output);

    // Check if platform hierarchy operations properly failed as expected
    assert!(output.contains("succeeded"));

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// VBS attestation test with agent
// TODO: Enable linux test when agent is supported.
#[openvmm_test(
    openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64],
    // openhcl_uefi_x64[vbs](vhd(ubuntu_2204_server_x64))
)]
async fn vbs_attestation_with_agent(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    extra_deps: (ResolvedArtifact<TPM_GUEST_TESTS_WINDOWS_X64>,),
) -> anyhow::Result<()> {
    ensure_windows_2025_prepped_vhd()?;

    let os_flavor = config.os_flavor();
    let (tpm_guest_tests_artifact,) = extra_deps;
    let tpm_guest_tests_host_path = tpm_guest_tests_artifact.get();
    let config = config
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .modify_backend(|b| b.with_tpm().with_tpm_state_persistence());

    let (vm, agent) = match os_flavor {
        OsFlavor::Windows => {
            let (vm, agent) = config.run().await?;

            let tpm_guest_tests_bytes =
                std::fs::read(tpm_guest_tests_host_path).with_context(|| {
                    format!("failed to read {}", tpm_guest_tests_host_path.display())
                })?;

            agent
                .write_file("C:\\tpm_guest_tests.exe", tpm_guest_tests_bytes.as_slice())
                .await
                .context("failed to copy tpm_guest_tests.exe into the guest")?;

            let sh = agent.windows_shell();
            let output = cmd!(sh, "C:\\tpm_guest_tests.exe")
                .args(["--ak-cert"])
                .read()
                .await
                .context("failed to execute tpm_guest_tests.exe inside the guest")?;

            assert!(
                output.contains("AK certificate data"),
                "tpm_guest_tests.exe --ak-cert did not report AK certificate data: {output}",
            );

            (vm, agent)
        }
        OsFlavor::Linux => {
            unreachable!()
        }
        _ => unreachable!(),
    };

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
