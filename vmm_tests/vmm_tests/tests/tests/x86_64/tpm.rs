// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use petri::PetriGuestStateLifetime;
use petri::PetriVmBuilder;
use petri::ShutdownKind;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use petri_artifacts_common::tags::OsFlavor;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::openvmm_test_no_agent;

/// Basic boot tests with TPM enabled.
#[openvmm_test(
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openhcl_uefi_x64(vhd(ubuntu_2404_server_x64))
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
// TODO: Add in-guest TPM tests for Windows as we currently
// do not have an easy way to interact with TPM without a private
// or custom tool.
#[openvmm_test(openhcl_uefi_x64(vhd(ubuntu_2404_server_x64)))]
async fn tpm_ak_cert_persisted(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let config = config
        // See `get_protocol::dps_json::ManagementVtlFeatures`
        // Enables attempt ak cert callback
        .with_openhcl_command_line("HCL_ATTEMPT_AK_CERT_CALLBACK=1")
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .modify_backend(|b| {
            b.with_tpm()
                .with_tpm_state_persistence()
                .with_igvm_attest_test_config(
                    get_resources::ged::IgvmAttestTestConfig::AkCertPersistentAcrossBoot,
                )
        });

    // First boot - AK cert request will be served by GED
    // Second boot - Ak cert request will be bypassed by GED
    // TODO: with_expect_reset shouldn't be needed once with_tpm() is
    // backend-agnostic.
    let (vm, agent) = config.with_expect_reset().run().await?;

    // Use the python script to read AK cert from TPM nv index
    // and verify that the AK cert preserves across boot.
    // TODO: Replace the script with tpm2-tools
    const TEST_FILE: &str = "tpm.py";
    const TEST_CONTENT: &str = include_str!("../../../test_data/tpm.py");

    agent.write_file(TEST_FILE, TEST_CONTENT.as_bytes()).await?;
    assert_eq!(agent.read_file(TEST_FILE).await?, TEST_CONTENT.as_bytes());

    let sh = agent.unix_shell();
    let output = cmd!(sh, "python3 tpm.py").read().await?;

    // Check if the content preserves as expected
    assert!(output.contains("succeeded"));

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Test AK cert retry logic on Linux.
// TODO: Add in-guest TPM tests for Windows as we currently
// do not have an easy way to interact with TPM without a private
// or custom tool.
#[openvmm_test(openhcl_uefi_x64(vhd(ubuntu_2404_server_x64)))]
async fn tpm_ak_cert_retry(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let config = config
        // See `get_protocol::dps_json::ManagementVtlFeatures`
        // Enables attempt ak cert callback
        .with_openhcl_command_line("HCL_ATTEMPT_AK_CERT_CALLBACK=1")
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .modify_backend(|b| {
            b.with_tpm()
                .with_tpm_state_persistence()
                .with_igvm_attest_test_config(
                    get_resources::ged::IgvmAttestTestConfig::AkCertRequestFailureAndRetry,
                )
        });

    // First boot - expect no AK cert from GED
    // Second boot - except get AK cert from GED on the second attempts
    // TODO: with_expect_reset shouldn't be needed once with_tpm() is
    // backend-agnostic.
    let (vm, agent) = config.with_expect_reset().run().await?;

    // Use the python script to read AK cert from TPM nv index
    // and verify that the AK cert preserves across boot.
    // TODO: Replace the script with tpm2-tools
    const TEST_FILE: &str = "tpm.py";
    const TEST_CONTENT: &str = include_str!("../../../test_data/tpm.py");

    agent.write_file(TEST_FILE, TEST_CONTENT.as_bytes()).await?;
    assert_eq!(agent.read_file(TEST_FILE).await?, TEST_CONTENT.as_bytes());

    // The first AK cert request made during boot is expected to
    // get invalid response from GED such that no data is set
    // to nv index. The script should return failure. Also, the nv
    // read made by the script is expected to trigger another AK cert
    // request.
    let sh = agent.unix_shell();
    let output = cmd!(sh, "python3 tpm.py").read().await?;

    // Check if there is no content yet
    assert!(!output.contains("succeeded"));

    // Run the script again to test if the AK cert triggered by nv read
    // succeeds and the data is written into the nv index.
    let sh = agent.unix_shell();
    let output = cmd!(sh, "python3 tpm.py").read().await?;

    // Check if the content is now available
    assert!(output.contains("succeeded"));

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Basic VBS boot test with TPM enabled.
#[openvmm_test_no_agent(
    openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2022_x64)),
    openhcl_uefi_x64[vbs](vhd(ubuntu_2404_server_x64))
)]
async fn vbs_boot_with_tpm(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let config = config.modify_backend(|b| b.with_tpm());

    let mut vm = match os_flavor {
        OsFlavor::Windows => config.run_without_agent().await?,
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

/// VBS boot test with attestation enabled
// TODO: Add in-guest tests to retrieve and verify the report.
#[openvmm_test_no_agent(
    openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2022_x64)),
    openhcl_uefi_x64[vbs](vhd(ubuntu_2404_server_x64))
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
