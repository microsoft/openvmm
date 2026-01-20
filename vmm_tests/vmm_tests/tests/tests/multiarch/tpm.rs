// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use anyhow::ensure;
use petri::PetriGuestStateLifetime;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ResolvedArtifact;
use petri::ShutdownKind;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use petri_artifacts_common::tags::OsFlavor;
use petri_artifacts_vmm_test::artifacts::guest_tools::TPM_GUEST_TESTS_LINUX_X64;
use petri_artifacts_vmm_test::artifacts::guest_tools::TPM_GUEST_TESTS_WINDOWS_X64;
use pipette_client::PipetteClient;
use std::path::Path;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::vmm_test;

const AK_CERT_NONZERO_BYTES: usize = 2500;
const AK_CERT_TOTAL_BYTES: usize = 4096;

const TPM_GUEST_TESTS_LINUX_GUEST_PATH: &str = "/tmp/tpm_guest_tests";
const TPM_GUEST_TESTS_WINDOWS_GUEST_PATH: &str = "C:\\tpm_guest_tests.exe";

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

struct TpmGuestTests<'a> {
    os_flavor: OsFlavor,
    guest_binary_path: String,
    agent: &'a PipetteClient,
}

impl<'a> TpmGuestTests<'a> {
    async fn send_tpm_guest_tests(
        agent: &'a PipetteClient,
        host_binary_path: &Path,
        guest_binary_path: &str,
        os_flavor: OsFlavor,
    ) -> anyhow::Result<Self> {
        let guest_binary = std::fs::read(host_binary_path)
            .with_context(|| format!("failed to read {}", host_binary_path.display()))?;
        agent
            .write_file(guest_binary_path, guest_binary.as_slice())
            .await
            .context("failed to copy tpm_guest_tests binary into the guest")?;

        match os_flavor {
            OsFlavor::Linux => {
                let sh = agent.unix_shell();
                cmd!(sh, "chmod +x {guest_binary_path}").run().await?;

                Ok(Self {
                    os_flavor,
                    guest_binary_path: guest_binary_path.to_string(),
                    agent,
                })
            }
            OsFlavor::Windows => Ok(Self {
                os_flavor,
                guest_binary_path: guest_binary_path.to_string(),
                agent,
            }),
            _ => unreachable!(),
        }
    }

    async fn read_ak_cert(&self) -> anyhow::Result<String> {
        let guest_binary_path = &self.guest_binary_path;
        match self.os_flavor {
            OsFlavor::Linux => {
                let sh = self.agent.unix_shell();
                cmd!(sh, "{guest_binary_path}")
                    .args(["ak_cert"])
                    .read()
                    .await
            }
            OsFlavor::Windows => {
                let sh = self.agent.windows_shell();
                cmd!(sh, "{guest_binary_path}")
                    .args(["ak_cert"])
                    .read()
                    .await
            }
            _ => unreachable!(),
        }
    }

    async fn read_ak_cert_with_expected_hex(&self, expected_hex: &str) -> anyhow::Result<String> {
        let guest_binary_path = &self.guest_binary_path;

        match self.os_flavor {
            OsFlavor::Linux => {
                let sh = self.agent.unix_shell();
                cmd!(sh, "{guest_binary_path}")
                    .args([
                        "ak_cert",
                        "--expected-data-hex",
                        expected_hex,
                        "--retry",
                        "3",
                    ])
                    .read()
                    .await
            }
            OsFlavor::Windows => {
                let sh = self.agent.windows_shell();
                cmd!(sh, "{guest_binary_path}")
                    .args([
                        "ak_cert",
                        "--expected-data-hex",
                        expected_hex,
                        "--retry",
                        "3",
                    ])
                    .read()
                    .await
            }
            _ => unreachable!(),
        }
    }

    #[cfg(windows)]
    async fn read_report(&self) -> anyhow::Result<String> {
        let guest_binary_path = &self.guest_binary_path;
        match self.os_flavor {
            OsFlavor::Linux => {
                let sh = self.agent.unix_shell();
                cmd!(sh, "{guest_binary_path}")
                    .args(["report", "--show-runtime-claims"])
                    .read()
                    .await
            }
            OsFlavor::Windows => {
                let sh = self.agent.windows_shell();
                cmd!(sh, "{guest_binary_path}")
                    .args(["report", "--show-runtime-claims"])
                    .read()
                    .await
            }
            _ => unreachable!(),
        }
    }
}

/// Basic boot tests with TPM enabled.
#[vmm_test(
    // TODO: enable openvmm TPM tests once we can build OpenSSL on Windows in CI
    // openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    // openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    // openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    // openvmm_uefi_x64(vhd(ubuntu_2504_server_x64)),
    openvmm_openhcl_uefi_x64(vhd(alpine_3_23_x64)),
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64(vhd(alpine_3_23_x64)),
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    openvmm_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped)),
    // openvmm_openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64_prepped)),
    hyperv_openhcl_uefi_x64[tdx](vhd(ubuntu_2504_server_x64))
)]
async fn boot_with_tpm<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_tpm(true)
        .with_tpm_state_persistence(true)
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .run()
        .await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Test AK cert is persistent across boots.
#[openvmm_test(
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64],
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[TPM_GUEST_TESTS_WINDOWS_X64]
)]
async fn tpm_ak_cert_persisted<T>(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    extra_deps: (ResolvedArtifact<T>,),
) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (mut vm, mut agent) = config
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_tpm(true)
        .with_tpm_state_persistence(true)
        .modify_backend(|b| {
            b.with_igvm_attest_test_config(
                get_resources::ged::IgvmAttestTestConfig::AkCertPersistentAcrossBoot,
            )
        })
        .run()
        .await?;

    let guest_binary_path = match os_flavor {
        // Ubuntu automatically reboots when the TPM is enabled
        OsFlavor::Linux => {
            // First boot - AK cert request will be served by GED.
            // Second boot - Ak cert request will be bypassed by GED.
            TPM_GUEST_TESTS_LINUX_GUEST_PATH
        }
        OsFlavor::Windows => {
            // First boot - AK cert request will be served by GED
            // Second boot - Ak cert request will be bypassed by GED.
            agent.reboot().await?;
            agent = vm.wait_for_reset().await?;

            TPM_GUEST_TESTS_WINDOWS_GUEST_PATH
        }
        _ => unreachable!(),
    };

    let (artifact,) = extra_deps;
    let host_binary_path = artifact.get();
    let tpm_guest_tests =
        TpmGuestTests::send_tpm_guest_tests(&agent, host_binary_path, guest_binary_path, os_flavor)
            .await?;

    let expected_hex = expected_ak_cert_hex();
    let output = tpm_guest_tests
        .read_ak_cert_with_expected_hex(expected_hex.as_str())
        .await?;

    ensure!(
        output.contains("AK certificate matches expected value"),
        format!("{output}")
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test AK cert retry logic.
#[openvmm_test(
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64],
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[TPM_GUEST_TESTS_WINDOWS_X64]
)]
async fn tpm_ak_cert_retry<T>(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    extra_deps: (ResolvedArtifact<T>,),
) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (vm, agent) = config
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_tpm(true)
        .with_tpm_state_persistence(true)
        .modify_backend(|b| {
            b.with_igvm_attest_test_config(
                get_resources::ged::IgvmAttestTestConfig::AkCertRequestFailureAndRetry,
            )
        })
        .run()
        .await?;

    let guest_binary_path = match os_flavor {
        OsFlavor::Linux => {
            // First boot - expect no AK cert from GED
            // Second boot - expect get AK cert from GED on the second attempts
            TPM_GUEST_TESTS_LINUX_GUEST_PATH
        }
        OsFlavor::Windows => {
            // At this point, two AK cert requests are made. One is during tpm
            // initialization, another one is during boot triggering by a NV read (Windows-specific).
            // Both requests are expected to fail due to the GED configuration.
            TPM_GUEST_TESTS_WINDOWS_GUEST_PATH
        }
        _ => unreachable!(),
    };

    let (artifact,) = extra_deps;
    let host_binary_path = artifact.get();
    let tpm_guest_tests =
        TpmGuestTests::send_tpm_guest_tests(&agent, host_binary_path, guest_binary_path, os_flavor)
            .await?;

    // The read attempt is expected to fail and trigger an AK cert renewal request.
    let attempt = tpm_guest_tests.read_ak_cert().await;
    assert!(
        attempt.is_err(),
        "AK certificate read unexpectedly succeeded"
    );

    let expected_hex = expected_ak_cert_hex();
    let output = tpm_guest_tests
        .read_ak_cert_with_expected_hex(expected_hex.as_str())
        .await?;

    ensure!(
        output.contains("AK certificate matches expected value"),
        format!("{output}")
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// VBS boot test with attestation enabled
#[openvmm_test(
    openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped)),
    // openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64))
)]
async fn vbs_boot_with_attestation(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let mut vm = config
        .with_tpm(true)
        .with_tpm_state_persistence(true)
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .run_without_agent()
        .await?;

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
    let (vm, agent) = config
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_tpm(true)
        .run()
        .await?;

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

// VBS attestation test with agent
// TODO: Enable windows test when prep run dependency is supported for openvmm-based vbs tests and
// remove `vbs_boot_with_attestation` test.
// TODO: Enable Linux test when boot failure is resolved.
// #[openvmm_test(
//     openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64],
//     openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64],
// )]
// async fn vbs_attestation_with_agent<T>(
//     config: PetriVmBuilder<OpenVmmPetriBackend>,
//     extra_deps: (ResolvedArtifact<T>,),
// ) -> anyhow::Result<()> {
//     let os_flavor = config.os_flavor();
//     let config = config
//         .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
//         .modify_backend(|b| b.with_tpm().with_tpm_state_persistence(true));

//     let (vm, agent, guest_binary_path) = match os_flavor {
//         OsFlavor::Linux => {
//             let (vm, agent) = config.with_expect_reset().run().await?;

//             (vm, agent, TPM_GUEST_TESTS_LINUX_GUEST_PATH)
//         }
//         OsFlavor::Windows => {
//             let (vm, agent) = config.run().await?;

//             (vm, agent, TPM_GUEST_TESTS_WINDOWS_GUEST_PATH)
//         }
//         _ => unreachable!(),
//     };

//     let (artifact,) = extra_deps;
//     let host_binary_path = artifact.get();
//     let tpm_guest_tests =
//         TpmGuestTests::send_tpm_guest_tests(&agent, host_binary_path, guest_binary_path, os_flavor)
//             .await?;

//     let expected_hex = expected_ak_cert_hex();
//     let ak_cert_output = tpm_guest_tests
//         .read_ak_cert_with_expected_hex(expected_hex.as_str())
//         .await?;

//     ensure!(
//         ak_cert_output.contains("AK certificate matches expected value"),
//         format!("{ak_cert_output}")
//     );

//     let report_output = tpm_guest_tests
//         .read_report()
//         .await
//         .context("failed to execute tpm_guest_tests report inside the guest")?;

//     ensure!(
//         report_output.contains("Runtime claims JSON"),
//         format!("{report_output}")
//     );
//     ensure!(
//         report_output.contains("\"vmUniqueId\""),
//         format!("{report_output}")
//     );

//     agent.power_off().await?;
//     vm.wait_for_clean_teardown().await?;

//     Ok(())
// }

/// CVM with guest tpm tests on Hyper-V.
#[cfg(windows)]
#[vmm_test(
    hyperv_openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64],
    hyperv_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[tdx](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64],
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64],
)]
async fn cvm_tpm_guest_tests<T, U: PetriVmmBackend>(
    config: PetriVmBuilder<U>,
    extra_deps: (ResolvedArtifact<T>,),
) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    // TODO: Add test IGVMAgent RPC server to support the boot-time attestation.
    let config = config
        .with_tpm(true)
        .with_tpm_state_persistence(false)
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk);

    let (vm, agent) = config.run().await?;

    let guest_binary_path = match os_flavor {
        OsFlavor::Linux => TPM_GUEST_TESTS_LINUX_GUEST_PATH,
        OsFlavor::Windows => TPM_GUEST_TESTS_WINDOWS_GUEST_PATH,
        _ => unreachable!(),
    };
    let (artifact,) = extra_deps;
    let host_binary_path = artifact.get();
    let tpm_guest_tests =
        TpmGuestTests::send_tpm_guest_tests(&agent, host_binary_path, guest_binary_path, os_flavor)
            .await?;

    // TODO: Add test IGVMAgent RPC server to support AK Cert
    // let expected_hex = expected_ak_cert_hex();
    // let ak_cert_output = tpm_guest_tests.read_ak_cert_with_expected_hex(expected_hex.as_str()).await?;

    // ensure!(
    //     ak_cert_output.contains("AK certificate matches expected value"),
    //     format!("{ak_cert_output}")
    // );

    let report_output = tpm_guest_tests
        .read_report()
        .await
        .context("failed to execute tpm_guest_tests report inside the guest")?;

    ensure!(
        report_output.contains("Runtime claims JSON"),
        format!("{report_output}")
    );
    ensure!(
        report_output.contains("\"vmUniqueId\""),
        format!("{report_output}")
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}
