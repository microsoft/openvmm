// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use anyhow::ensure;
use petri::PetriGuestStateLifetime;
#[cfg(windows)]
use petri::PetriHaltReason;
#[cfg(windows)]
use petri::PetriHardwareSealingPolicy;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ResolvedArtifact;
use petri::ShutdownKind;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use petri_artifacts_common::tags::OsFlavor;
use petri_artifacts_vmm_test::artifacts::guest_tools::TPM_GUEST_TESTS_LINUX_X64;
use petri_artifacts_vmm_test::artifacts::guest_tools::TPM_GUEST_TESTS_WINDOWS_X64;
#[cfg(windows)]
use petri_artifacts_vmm_test::artifacts::host_tools::TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64;
#[cfg(windows)]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_AARCH64;
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_X64;
use petri_artifacts_vmm_test::artifacts::test_vmgs::VMGS_WITH_16K_TPM;
use pipette_client::PipetteClient;
use std::path::Path;
#[cfg(windows)]
use vmm_test_igvm_agent as igvm_agent_rpc_server;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::vmm_test;
#[cfg(windows)]
use vmm_test_macros::vmm_test_with;

const AK_CERT_NONZERO_BYTES: usize = 2500;
const AK_CERT_TOTAL_BYTES: usize = 4096;

const TPM_GUEST_TESTS_LINUX_GUEST_PATH: &str = "/tmp/tpm_guest_tests";
const TPM_GUEST_TESTS_WINDOWS_GUEST_PATH: &str = "C:\\tpm_guest_tests.exe";

#[cfg(windows)]
fn ensure_rpc_server_running(
    rpc_server_path: &Path,
) -> anyhow::Result<Option<igvm_agent_rpc_server::RpcServerGuard>> {
    // For local single-test runs we start and own the server (see vmm_test_igvm_agent/README.md).
    // If it's already running (e.g., CI), do nothing.
    if igvm_agent_rpc_server::ensure_rpc_server_running().is_ok() {
        return Ok(None);
    }

    if !igvm_agent_rpc_server::local_autostart_enabled() {
        anyhow::bail!(
            "test_igvm_agent_rpc_server is not running. Flowey should start it in CI; for local single-test runs set {}=1 to opt-in to auto-starting it.",
            igvm_agent_rpc_server::LOCAL_AUTOSTART_ENV
        );
    }

    // Otherwise start locally and keep the guard alive so the server is terminated when the test ends.
    igvm_agent_rpc_server::start_rpc_server(rpc_server_path)
        .map(Some)
        .context("failed to start test_igvm_agent_rpc_server")
}

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
                        "10",
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
                        "10",
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

    /// Read the vTPM Attestation Key public modulus (`HCLAkPub.n`) from the
    /// attestation report's runtime claims.
    ///
    /// The modulus uniquely identifies the AK, so comparing it across reboots
    /// detects whether the AK was regenerated.
    #[cfg(windows)]
    async fn read_ak_pub_modulus(&self) -> anyhow::Result<String> {
        let output = self.read_report().await?;

        // The report binary prints preamble lines followed by:
        //   Runtime claims JSON:
        //   { ...pretty-printed JSON... }
        const MARKER: &str = "Runtime claims JSON:";
        let json_start = output
            .find(MARKER)
            .map(|i| i + MARKER.len())
            .with_context(|| format!("report output missing runtime claims JSON: {output}"))?;

        // Parse the first JSON value, ignoring any trailing output.
        let claims = serde_json::Deserializer::from_str(output[json_start..].trim_start())
            .into_iter::<serde_json::Value>()
            .next()
            .context("no JSON value found after runtime claims marker")?
            .context("failed to parse runtime claims JSON")?;

        let modulus = claims
            .get("keys")
            .and_then(|keys| keys.as_array())
            .context("runtime claims missing keys array")?
            .iter()
            .find(|key| key.get("kid").and_then(|v| v.as_str()) == Some("HCLAkPub"))
            .context("runtime claims missing HCLAkPub key")?
            .get("n")
            .and_then(|n| n.as_str())
            .context("HCLAkPub missing modulus")?;

        Ok(modulus.to_string())
    }

    /// Define an NV index with the given size.
    #[cfg(windows)]
    async fn nv_define(&self, index: &str, size: &str) -> anyhow::Result<String> {
        let guest_binary_path = &self.guest_binary_path;
        match self.os_flavor {
            OsFlavor::Linux => {
                let sh = self.agent.unix_shell();
                cmd!(sh, "{guest_binary_path}")
                    .args(["nv_define", "--index", index, "--size", size])
                    .read()
                    .await
            }
            OsFlavor::Windows => {
                let sh = self.agent.windows_shell();
                cmd!(sh, "{guest_binary_path}")
                    .args(["nv_define", "--index", index, "--size", size])
                    .read()
                    .await
            }
            _ => unreachable!(),
        }
    }

    /// Write hex data to an NV index.
    #[cfg(windows)]
    async fn nv_write(&self, index: &str, data_hex: &str) -> anyhow::Result<String> {
        let guest_binary_path = &self.guest_binary_path;
        match self.os_flavor {
            OsFlavor::Linux => {
                let sh = self.agent.unix_shell();
                cmd!(sh, "{guest_binary_path}")
                    .args(["nv_write", "--index", index, "--data-hex", data_hex])
                    .read()
                    .await
            }
            OsFlavor::Windows => {
                let sh = self.agent.windows_shell();
                cmd!(sh, "{guest_binary_path}")
                    .args(["nv_write", "--index", index, "--data-hex", data_hex])
                    .read()
                    .await
            }
            _ => unreachable!(),
        }
    }

    /// Read an NV index and verify against expected hex data.
    #[cfg(windows)]
    async fn nv_read_with_expected_hex(
        &self,
        index: &str,
        expected_hex: &str,
    ) -> anyhow::Result<String> {
        let guest_binary_path = &self.guest_binary_path;
        match self.os_flavor {
            OsFlavor::Linux => {
                let sh = self.agent.unix_shell();
                cmd!(sh, "{guest_binary_path}")
                    .args([
                        "nv_read",
                        "--index",
                        index,
                        "--expected-data-hex",
                        expected_hex,
                    ])
                    .read()
                    .await
            }
            OsFlavor::Windows => {
                let sh = self.agent.windows_shell();
                cmd!(sh, "{guest_binary_path}")
                    .args([
                        "nv_read",
                        "--index",
                        index,
                        "--expected-data-hex",
                        expected_hex,
                    ])
                    .read()
                    .await
            }
            _ => unreachable!(),
        }
    }
}

/// Basic boot tests with TPM enabled.
#[vmm_test(
    ignore(reason = "OpenVMM TPM needs OpenSSL, not yet buildable on Windows CI", openvmm_uefi_aarch64(vhd(windows_11_enterprise_aarch64))),
    ignore(reason = "OpenVMM TPM needs OpenSSL, not yet buildable on Windows CI", openvmm_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))),
    ignore(reason = "OpenVMM TPM needs OpenSSL, not yet buildable on Windows CI", openvmm_uefi_x64(vhd(windows_datacenter_core_2022_x64))),
    ignore(reason = "OpenVMM TPM needs OpenSSL, not yet buildable on Windows CI", openvmm_uefi_x64(vhd(ubuntu_2504_server_x64))),
    openvmm_openhcl_uefi_x64(vhd(alpine_3_23_x64)),
    openvmm_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    hyperv_openhcl_uefi_aarch64(vhd(windows_11_enterprise_aarch64)),
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64)),
    hyperv_openhcl_uefi_x64(vhd(alpine_3_23_x64)),
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64)),
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)),
    openvmm_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped)),
    ignore(reason = "OpenVMM VBS boot on Ubuntu is unreliable (microsoft/openvmm#2608)", openvmm_openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64))),
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

/// Hyper-V variant of TPM AK cert persisted test.
///
/// First boot: AK cert request is served by the RPC agent.
/// Second boot: AK cert is served from the persistent cache.
///
/// Config mapping: the `test_igvm_agent_rpc_server` resolves each VM's
/// test config by matching `{image}_{isolation}_{test_fn}` substrings
/// in the Hyper-V VM name (see `resolve_test_config`).  Each
/// image/isolation combination listed in the `#[vmm_test]` attribute
/// must have a corresponding entry in `KNOWN_TEST_CONFIGS`.  For this
/// test function (`ak_cert_cache`), they all map to
/// `AkCertPersistentAcrossBootExtended`.
#[cfg(windows)]
#[vmm_test(
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[tdx](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
)]
async fn ak_cert_cache<T, S, U: PetriVmmBackend>(
    config: PetriVmBuilder<U>,
    extra_deps: (ResolvedArtifact<T>, ResolvedArtifact<S>),
) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (tpm_guest_tests_artifact, rpc_server_artifact) = extra_deps;

    let rpc_server_path = rpc_server_artifact.get();
    let _rpc_guard = ensure_rpc_server_running(rpc_server_path)?;

    let (mut vm, mut agent) = config
        .with_tpm(true)
        .with_tpm_state_persistence(true)
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .run()
        .await?;

    let guest_binary_path = match os_flavor {
        OsFlavor::Linux => TPM_GUEST_TESTS_LINUX_GUEST_PATH,
        OsFlavor::Windows => TPM_GUEST_TESTS_WINDOWS_GUEST_PATH,
        _ => unreachable!(),
    };

    agent.reboot().await?;
    agent = vm.wait_for_reset().await?;

    let host_binary_path = tpm_guest_tests_artifact.get();
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

/// Hyper-V variant of TPM AK cert retry test.
///
/// The RPC agent is configured to fail the first AK cert request and
/// succeed on retry.  The guest-side `tpm_guest_tests` binary verifies
/// that the first read fails and the second (retry) read succeeds with
/// the expected certificate data.
///
/// Config mapping: the `test_igvm_agent_rpc_server` resolves each VM's
/// test config by matching `{image}_{isolation}_{test_fn}` substrings
/// in the Hyper-V VM name (see `resolve_test_config`).  Each
/// image/isolation combination listed in the `#[vmm_test]` attribute
/// must have a corresponding entry in `KNOWN_TEST_CONFIGS`.  For this
/// test function (`ak_cert_retry`), they all map to
/// `AkCertRequestFailureAndRetryExtended`.
#[cfg(windows)]
#[vmm_test(
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[tdx](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
)]
async fn ak_cert_retry<T, S, U: PetriVmmBackend>(
    config: PetriVmBuilder<U>,
    extra_deps: (ResolvedArtifact<T>, ResolvedArtifact<S>),
) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (tpm_guest_tests_artifact, rpc_server_artifact) = extra_deps;

    let rpc_server_path = rpc_server_artifact.get();
    let _rpc_guard = ensure_rpc_server_running(rpc_server_path)?;

    let (vm, agent) = config
        .with_tpm(true)
        .with_tpm_state_persistence(true)
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .run()
        .await?;

    let guest_binary_path = match os_flavor {
        OsFlavor::Linux => TPM_GUEST_TESTS_LINUX_GUEST_PATH,
        OsFlavor::Windows => TPM_GUEST_TESTS_WINDOWS_GUEST_PATH,
        _ => unreachable!(),
    };

    let host_binary_path = tpm_guest_tests_artifact.get();
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
    ignore(reason = "OpenVMM VBS Ubuntu attestation boot is not yet reliable (microsoft/openvmm#2608)", openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64)))
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
///
/// Exercises the CVM vTPM end-to-end against the test IGVM agent RPC server:
/// verifies the AK certificate and attestation report runtime claims, and
/// that the vTPM Attestation Key (AK) public key is stable across a reboot.
///
/// AK stability: the AK is derived deterministically from the TPM
/// endorsement-hierarchy seed. With a correct OSS ms-tpm-20-ref crypto
/// backend (prebuilt `tpm-oss-openssl/libtpm.a` from openvmm-deps),
/// re-deriving the AK on the next boot must produce the same public key. A
/// previous `DfStart` (`CryptRand.c`) out-of-bounds read made the
/// 64-bit-radix derivation non-deterministic; this guards the fix shipped via
/// openvmm-deps. (See `ak_pub_refresh` for the contrasting case where a
/// host-requested state refresh deliberately rotates the AK.)
///
/// The test requires the test_igvm_agent_rpc_server to be running.
/// In CI, the server is started by flowey before tests run.
/// For local development, either start the server manually or set
/// `VMM_TEST_IGVM_AGENT_LOCAL_AUTOSTART=1` to let the test spin it up.
#[cfg(windows)]
#[vmm_test(
    hyperv_openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[tdx](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
)]
async fn cvm_tpm_guest_tests<T, S, U: PetriVmmBackend>(
    config: PetriVmBuilder<U>,
    extra_deps: (ResolvedArtifact<T>, ResolvedArtifact<S>),
) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (tpm_guest_tests_artifact, rpc_server_artifact) = extra_deps;

    // Verify (or start) the RPC server. Flowey handles CI; local nextest can start it here.
    let rpc_server_path = rpc_server_artifact.get();
    let _rpc_guard = ensure_rpc_server_running(rpc_server_path)?;

    let config = config
        .with_tpm(true)
        .with_tpm_state_persistence(true)
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk);

    let (mut vm, agent) = config.run().await?;

    let guest_binary_path = match os_flavor {
        OsFlavor::Linux => TPM_GUEST_TESTS_LINUX_GUEST_PATH,
        OsFlavor::Windows => TPM_GUEST_TESTS_WINDOWS_GUEST_PATH,
        _ => unreachable!(),
    };
    let host_binary_path = tpm_guest_tests_artifact.get();
    let tpm_guest_tests =
        TpmGuestTests::send_tpm_guest_tests(&agent, host_binary_path, guest_binary_path, os_flavor)
            .await?;

    // Verify AK cert with the test IGVM agent RPC server
    let expected_hex = expected_ak_cert_hex();
    let ak_cert_output = tpm_guest_tests
        .read_ak_cert_with_expected_hex(expected_hex.as_str())
        .await?;

    ensure!(
        ak_cert_output.contains("AK certificate matches expected value"),
        format!("{ak_cert_output}")
    );

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

    // Capture the AK public modulus on this (first) boot for the stability
    // check below.
    let ak_pub_first = tpm_guest_tests.read_ak_pub_modulus().await?;
    ensure!(
        !ak_pub_first.is_empty(),
        "AK pub modulus should not be empty on first boot"
    );

    // Reboot. With no state refresh requested, the AK must be re-derived
    // identically.
    agent.reboot().await?;
    let agent = vm.wait_for_reset().await?;

    // Second boot: re-send the binary and capture the AK public modulus again.
    let host_binary_path = tpm_guest_tests_artifact.get();
    let tpm_guest_tests =
        TpmGuestTests::send_tpm_guest_tests(&agent, host_binary_path, guest_binary_path, os_flavor)
            .await?;
    let ak_pub_second = tpm_guest_tests.read_ak_pub_modulus().await?;

    ensure!(
        ak_pub_first == ak_pub_second,
        "AK pub must remain stable across reboot, but it changed \
         (first={ak_pub_first}, second={ak_pub_second})"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Verify that a host/agent-requested TPM state refresh regenerates the vTPM
/// Attestation Key (AK) across a reboot for a normal stateful CVM.
///
/// The `test_igvm_agent_rpc_server` is configured (via the `StateRefresh`
/// config, resolved by VM name in its `KNOWN_TEST_CONFIGS`) to report
/// `state_refresh_request` in its GSP RPC response. OpenHCL propagates this
/// into `refresh_tpm_seeds`, which regenerates the vTPM seeds (and therefore
/// the AK) on the next boot. The test reads the AK public modulus
/// (`HCLAkPub.n`) before and after the reboot and asserts it CHANGED.
///
/// This is the counterpart to the AK-stability check in `cvm_tpm_guest_tests`:
/// together they prove the AK is deterministic across reboots by default, yet
/// a state refresh deliberately rotates it.
#[cfg(windows)]
#[vmm_test(
    hyperv_openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[tdx](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
)]
async fn ak_pub_refresh<T, S, U: PetriVmmBackend>(
    config: PetriVmBuilder<U>,
    extra_deps: (ResolvedArtifact<T>, ResolvedArtifact<S>),
) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (tpm_guest_tests_artifact, rpc_server_artifact) = extra_deps;

    let rpc_server_path = rpc_server_artifact.get();
    let _rpc_guard = ensure_rpc_server_running(rpc_server_path)?;

    let (mut vm, agent) = config
        .with_tpm(true)
        .with_tpm_state_persistence(true)
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .run()
        .await?;

    let guest_binary_path = match os_flavor {
        OsFlavor::Linux => TPM_GUEST_TESTS_LINUX_GUEST_PATH,
        OsFlavor::Windows => TPM_GUEST_TESTS_WINDOWS_GUEST_PATH,
        _ => unreachable!(),
    };

    // First boot: capture the AK public modulus.
    let host_binary_path = tpm_guest_tests_artifact.get();
    let tpm_guest_tests =
        TpmGuestTests::send_tpm_guest_tests(&agent, host_binary_path, guest_binary_path, os_flavor)
            .await?;
    let ak_pub_first = tpm_guest_tests.read_ak_pub_modulus().await?;
    ensure!(
        !ak_pub_first.is_empty(),
        "AK pub modulus should not be empty on first boot"
    );

    // Reboot. The agent requests a TPM state refresh via the GSP RPC, so the
    // vTPM seeds (and the AK) must be regenerated.
    agent.reboot().await?;
    let agent = vm.wait_for_reset().await?;

    // Second boot: capture the AK public modulus again.
    let host_binary_path = tpm_guest_tests_artifact.get();
    let tpm_guest_tests =
        TpmGuestTests::send_tpm_guest_tests(&agent, host_binary_path, guest_binary_path, os_flavor)
            .await?;
    let ak_pub_second = tpm_guest_tests.read_ak_pub_modulus().await?;

    ensure!(
        ak_pub_first != ak_pub_second,
        "AK pub must change across reboot when a state refresh is requested, \
         but it stayed the same (value={ak_pub_first})"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test that `skip_hw_unsealing` signal from IGVM agent causes VMGS
/// unlock to fail on second boot.
///
/// First boot: KEY_RELEASE succeeds, VMGS is encrypted with hardware
/// key protector, TPM state is sealed.
/// Second boot: KEY_RELEASE fails with `skip_hw_unsealing` signal.
/// The attestation code skips hardware unsealing even though the
/// hardware key protector and derived keys are available, causing
/// `initialize_platform_security` to fall through to a scheme-specific
/// error (KP / GSP / GspById).  Underhill reports the failure to the
/// host via `complete_start_vtl0`, and the host terminates the VM.
///
/// Config mapping: the `test_igvm_agent_rpc_server` resolves each VM's
/// test config by matching `{image}_{isolation}_{test_fn}` substrings
/// in the Hyper-V VM name (see `resolve_test_config`).  Each
/// image/isolation combination listed in the `#[vmm_test]` attribute
/// must have a corresponding entry in `KNOWN_TEST_CONFIGS`.  For this
/// test function (`skip_hw_unseal`), they all map to
/// `KeyReleaseFailureSkipHwUnsealing`.
#[cfg(windows)]
#[vmm_test_with(unstable(reason = "SNP hardware-unseal key-release test is unreliable in CI; awaiting a fix"), configs(
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64))[TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped))[TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
))]
async fn skip_hw_unseal<T, U: PetriVmmBackend>(
    config: PetriVmBuilder<U>,
    extra_deps: (ResolvedArtifact<T>,),
) -> anyhow::Result<()> {
    let (rpc_server_artifact,) = extra_deps;

    let rpc_server_path = rpc_server_artifact.get();
    let _rpc_guard = ensure_rpc_server_running(rpc_server_path)?;

    let (mut vm, agent) = config
        .with_tpm(true)
        .with_tpm_state_persistence(true)
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .run()
        .await?;

    // First boot: KEY_RELEASE succeeds. TPM state is sealed with hardware
    // key protector. No guest-side verification needed — just let the boot
    // complete so the VMGS state is populated.

    // Reboot: triggers second KEY_RELEASE which fails with skip_hw_unsealing.
    // VMGS unlock will fail because hardware unsealing fallback is skipped.
    // initialize_platform_security returns an error, underhill reports the
    // failure to the host via complete_start_vtl0, and the host terminates
    // the VM.
    agent.reboot().await?;

    // Wait for the VM to reset and then fail on the second boot.
    //
    // Depending on timing, two outcomes are possible:
    //
    // 1. wait_for_halt() returns Reset (the CVM restart check saw the VM
    //    briefly reach Running state before underhill failed), and then the
    //    subsequent wait_for_teardown() fails because the VM termination
    //    does not produce a recognized halt event.
    //
    // 2. wait_for_halt() itself fails because the VM never reached Running
    //    state within the allowed CVM restart timeout (underhill failed
    //    before Hyper-V reported the VM as Running).
    //
    // Both outcomes confirm the expected behavior: the VM cannot boot after
    // hardware unsealing is skipped.
    let halt_reason = vm.wait_for_halt().await?;
    match halt_reason.reason {
        PetriHaltReason::Reset => {
            tracing::info!("Got reset event; waiting for second boot termination...");
            let second_halt_reason = vm.wait_for_teardown().await?;
            if !matches!(second_halt_reason.reason, PetriHaltReason::Other) {
                anyhow::bail!("Unexpected second boot halt reason: {second_halt_reason:?}")
            }
        }
        PetriHaltReason::Other => {
            tracing::info!("VM failed to restart as expected: {halt_reason:?}");
            vm.teardown().await?;
        }
        _ => anyhow::bail!("Unexpected halt reason: {halt_reason:?}"),
    }

    Ok(())
}

/// Test that KEY_RELEASE failure without skip_hw_unsealing signal allows
/// hardware unsealing fallback to succeed.
///
/// First boot: KEY_RELEASE succeeds, VMGS is encrypted with hardware
/// key protector, TPM state is sealed.  AK cert is verified.
/// Second boot: KEY_RELEASE fails (plain failure, no skip_hw_unsealing
/// signal), hardware unsealing fallback is attempted and succeeds because
/// the hardware key protector was saved on first boot.  The VM boots
/// normally and the AK cert remains accessible.
///
/// Config mapping: the `test_igvm_agent_rpc_server` resolves each VM's
/// test config by matching `{image}_{isolation}_{test_fn}` substrings
/// in the Hyper-V VM name (see `resolve_test_config`).  Each
/// image/isolation combination listed in the `#[vmm_test]` attribute
/// must have a corresponding entry in `KNOWN_TEST_CONFIGS`.  For this
/// test function (`use_hw_unseal`), they all map to
/// `KeyReleaseFailure`.
#[cfg(windows)]
#[vmm_test_with(unstable(reason = "SNP hardware-unseal key-release test is unreliable in CI; awaiting a fix"), configs(
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
))]
async fn use_hw_unseal<T, S, U: PetriVmmBackend>(
    config: PetriVmBuilder<U>,
    extra_deps: (ResolvedArtifact<T>, ResolvedArtifact<S>),
) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (tpm_guest_tests_artifact, rpc_server_artifact) = extra_deps;

    let rpc_server_path = rpc_server_artifact.get();
    let _rpc_guard = ensure_rpc_server_running(rpc_server_path)?;

    let (mut vm, agent) = config
        .with_tpm(true)
        .with_tpm_state_persistence(true)
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .run()
        .await?;

    let guest_binary_path = match os_flavor {
        OsFlavor::Linux => TPM_GUEST_TESTS_LINUX_GUEST_PATH,
        OsFlavor::Windows => TPM_GUEST_TESTS_WINDOWS_GUEST_PATH,
        _ => unreachable!(),
    };
    let host_binary_path = tpm_guest_tests_artifact.get();
    let tpm_guest_tests =
        TpmGuestTests::send_tpm_guest_tests(&agent, host_binary_path, guest_binary_path, os_flavor)
            .await?;

    // First boot: KEY_RELEASE succeeds. Verify AK cert is present.
    let expected_hex = expected_ak_cert_hex();
    let ak_cert_output = tpm_guest_tests
        .read_ak_cert_with_expected_hex(expected_hex.as_str())
        .await?;

    ensure!(
        ak_cert_output.contains("AK certificate matches expected value"),
        format!("{ak_cert_output}")
    );

    // Reboot: triggers second KEY_RELEASE which fails (plain failure,
    // no skip_hw_unsealing signal).  Hardware unsealing fallback kicks
    // in and succeeds — the VM boots normally.
    agent.reboot().await?;
    let agent = vm.wait_for_reset().await?;

    // Verify AK cert is still accessible after the hw unsealing fallback.
    let host_binary_path = tpm_guest_tests_artifact.get();
    let tpm_guest_tests =
        TpmGuestTests::send_tpm_guest_tests(&agent, host_binary_path, guest_binary_path, os_flavor)
            .await?;

    let ak_cert_output = tpm_guest_tests
        .read_ak_cert_with_expected_hex(expected_hex.as_str())
        .await?;

    ensure!(
        ak_cert_output.contains("AK certificate matches expected value"),
        "AK cert should still be accessible after hw unsealing fallback: {ak_cert_output}"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test that TPM NVRAM size persists across servicing.
#[vmm_test(
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[LATEST_STANDARD_X64, VMGS_WITH_16K_TPM],
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[LATEST_STANDARD_X64, VMGS_WITH_16K_TPM],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[LATEST_STANDARD_AARCH64, VMGS_WITH_16K_TPM]
)]
async fn tpm_servicing<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (igvm_file, vmgs_file): (
        ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
        ResolvedArtifact<VMGS_WITH_16K_TPM>,
    ),
) -> anyhow::Result<()> {
    let mut flags = config.default_servicing_flags();
    flags.override_version_checks = true;

    let config = config
        .with_tpm(true)
        .with_tpm_state_persistence(true)
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_initial_vmgs(vmgs_file);

    let (mut vm, agent) = config.run().await?;

    agent.ping().await?;

    let inspect_before = vm
        .inspect_openhcl("vm/tpm/worker/nvram_size", None, None)
        .await?;

    vm.restart_openhcl(igvm_file.clone(), flags).await?;
    agent.ping().await?;

    let inspect_after = vm
        .inspect_openhcl("vm/tpm/worker/nvram_size", None, None)
        .await?;
    assert_eq!(inspect_before, inspect_after);

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// NV index used by the hardware sealing persistence tests.
#[cfg(windows)]
const TEST_NV_INDEX: &str = "0x1500016";
/// Size of the test NV index in bytes.
#[cfg(windows)]
const TEST_NV_SIZE: &str = "64";
/// Test data written to the NV index (hex).
#[cfg(windows)]
const TEST_NV_DATA_HEX: &str = "0xdeadbeefcafebabe0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f303132333435363738";

/// Test that hardware sealing with hash-based key derivation persists
/// TPM NV index data across reboots.
///
/// Configuration: `no_persistent_secrets=true` (NoPersistentSecrets
/// isolation) with `HardwareSealedSecretsHashPolicy`.  The VMGS is
/// encrypted using a hardware-sealed key derived from the measurement
/// hash.
///
/// First boot: define NV index, write test data, read and verify.
/// Second boot: read the same NV index and verify data persisted.
#[cfg(windows)]
#[vmm_test(
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
)]
async fn hw_seal_hash<T, S, U: PetriVmmBackend>(
    config: PetriVmBuilder<U>,
    extra_deps: (ResolvedArtifact<T>, ResolvedArtifact<S>),
) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (tpm_guest_tests_artifact, rpc_server_artifact) = extra_deps;

    let rpc_server_path = rpc_server_artifact.get();
    let _rpc_guard = ensure_rpc_server_running(rpc_server_path)?;

    let (mut vm, agent) = config
        .with_tpm(true)
        .with_tpm_state_persistence(false)
        .with_hardware_sealing_policy(PetriHardwareSealingPolicy::HashPolicy)
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .run()
        .await?;

    let guest_binary_path = match os_flavor {
        OsFlavor::Linux => TPM_GUEST_TESTS_LINUX_GUEST_PATH,
        OsFlavor::Windows => TPM_GUEST_TESTS_WINDOWS_GUEST_PATH,
        _ => unreachable!(),
    };
    let host_binary_path = tpm_guest_tests_artifact.get();
    let tpm_guest_tests =
        TpmGuestTests::send_tpm_guest_tests(&agent, host_binary_path, guest_binary_path, os_flavor)
            .await?;

    // First boot: define NV index, write test data, read and verify.
    let define_output = tpm_guest_tests
        .nv_define(TEST_NV_INDEX, TEST_NV_SIZE)
        .await?;
    ensure!(
        define_output.contains("defined successfully"),
        "NV define should succeed: {define_output}"
    );

    let write_output = tpm_guest_tests
        .nv_write(TEST_NV_INDEX, TEST_NV_DATA_HEX)
        .await?;
    ensure!(
        write_output.contains("succeeded"),
        "NV write should succeed: {write_output}"
    );

    let read_output = tpm_guest_tests
        .nv_read_with_expected_hex(TEST_NV_INDEX, TEST_NV_DATA_HEX)
        .await?;
    ensure!(
        read_output.contains("matches expected value"),
        "NV read should match on first boot: {read_output}"
    );

    // Reboot to test persistence.
    agent.reboot().await?;
    let agent = vm.wait_for_reset().await?;

    // Second boot: re-send the binary and verify NV data persisted.
    let host_binary_path = tpm_guest_tests_artifact.get();
    let tpm_guest_tests =
        TpmGuestTests::send_tpm_guest_tests(&agent, host_binary_path, guest_binary_path, os_flavor)
            .await?;

    let read_output = tpm_guest_tests
        .nv_read_with_expected_hex(TEST_NV_INDEX, TEST_NV_DATA_HEX)
        .await?;
    ensure!(
        read_output.contains("matches expected value"),
        "NV data should persist across reboot with HashPolicy: {read_output}"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test that hardware sealing with signer-based key derivation persists
/// TPM NV index data across reboots.
///
/// Same as `hw_seal_hash` but uses `HardwareSealedSecretsSignerPolicy`
/// instead.
#[cfg(windows)]
#[vmm_test(
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
)]
async fn hw_seal_signer<T, S, U: PetriVmmBackend>(
    config: PetriVmBuilder<U>,
    extra_deps: (ResolvedArtifact<T>, ResolvedArtifact<S>),
) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (tpm_guest_tests_artifact, rpc_server_artifact) = extra_deps;

    let rpc_server_path = rpc_server_artifact.get();
    let _rpc_guard = ensure_rpc_server_running(rpc_server_path)?;

    let (mut vm, agent) = config
        .with_tpm(true)
        .with_tpm_state_persistence(false)
        .with_hardware_sealing_policy(PetriHardwareSealingPolicy::SignerPolicy)
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .run()
        .await?;

    let guest_binary_path = match os_flavor {
        OsFlavor::Linux => TPM_GUEST_TESTS_LINUX_GUEST_PATH,
        OsFlavor::Windows => TPM_GUEST_TESTS_WINDOWS_GUEST_PATH,
        _ => unreachable!(),
    };
    let host_binary_path = tpm_guest_tests_artifact.get();
    let tpm_guest_tests =
        TpmGuestTests::send_tpm_guest_tests(&agent, host_binary_path, guest_binary_path, os_flavor)
            .await?;

    // First boot: define NV index, write test data, read and verify.
    let define_output = tpm_guest_tests
        .nv_define(TEST_NV_INDEX, TEST_NV_SIZE)
        .await?;
    ensure!(
        define_output.contains("defined successfully"),
        "NV define should succeed: {define_output}"
    );

    let write_output = tpm_guest_tests
        .nv_write(TEST_NV_INDEX, TEST_NV_DATA_HEX)
        .await?;
    ensure!(
        write_output.contains("succeeded"),
        "NV write should succeed: {write_output}"
    );

    let read_output = tpm_guest_tests
        .nv_read_with_expected_hex(TEST_NV_INDEX, TEST_NV_DATA_HEX)
        .await?;
    ensure!(
        read_output.contains("matches expected value"),
        "NV read should match on first boot: {read_output}"
    );

    // Reboot to test persistence.
    agent.reboot().await?;
    let agent = vm.wait_for_reset().await?;

    // Second boot: re-send the binary and verify NV data persisted.
    let host_binary_path = tpm_guest_tests_artifact.get();
    let tpm_guest_tests =
        TpmGuestTests::send_tpm_guest_tests(&agent, host_binary_path, guest_binary_path, os_flavor)
            .await?;

    let read_output = tpm_guest_tests
        .nv_read_with_expected_hex(TEST_NV_INDEX, TEST_NV_DATA_HEX)
        .await?;
    ensure!(
        read_output.contains("matches expected value"),
        "NV data should persist across reboot with SignerPolicy: {read_output}"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Verify that stateless + hardware sealing mode keeps the vTPM AK stable
/// across a reboot even when the IGVM agent requests a TPM state refresh.
///
/// The `test_igvm_agent_rpc_server` is configured (via the `StateRefresh`
/// config, resolved by VM name in its `KNOWN_TEST_CONFIGS`) to report
/// `state_refresh_request` in its GSP RPC response. A state refresh would
/// normally cause OpenHCL to refresh the vTPM seeds and regenerate the AK.
///
/// In stateless + hardware sealing mode, however, OpenHCL skips the GSP
/// callout entirely, so the requested state refresh must be ignored and the
/// AK must NOT be regenerated. The test compares the AK public modulus
/// (`HCLAkPub`) from the attestation report runtime claims across a reboot
/// and asserts it is unchanged.
///
/// This is the sealing-mode counterpart to `ak_pub_refresh`: with the same
/// state-refresh request, a normal stateful CVM rotates its AK, but a
/// stateless + hardware sealing CVM keeps it stable.
#[cfg(windows)]
#[vmm_test(
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
)]
async fn hw_ak_stable<T, S, U: PetriVmmBackend>(
    config: PetriVmBuilder<U>,
    extra_deps: (ResolvedArtifact<T>, ResolvedArtifact<S>),
) -> anyhow::Result<()> {
    let os_flavor = config.os_flavor();
    let (tpm_guest_tests_artifact, rpc_server_artifact) = extra_deps;

    let rpc_server_path = rpc_server_artifact.get();
    let _rpc_guard = ensure_rpc_server_running(rpc_server_path)?;

    let (mut vm, agent) = config
        .with_tpm(true)
        .with_tpm_state_persistence(false)
        .with_hardware_sealing_policy(PetriHardwareSealingPolicy::HashPolicy)
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .run()
        .await?;

    let guest_binary_path = match os_flavor {
        OsFlavor::Linux => TPM_GUEST_TESTS_LINUX_GUEST_PATH,
        OsFlavor::Windows => TPM_GUEST_TESTS_WINDOWS_GUEST_PATH,
        _ => unreachable!(),
    };
    let host_binary_path = tpm_guest_tests_artifact.get();
    let tpm_guest_tests =
        TpmGuestTests::send_tpm_guest_tests(&agent, host_binary_path, guest_binary_path, os_flavor)
            .await?;

    // First boot: capture the AK public modulus.
    let ak_pub_first = tpm_guest_tests.read_ak_pub_modulus().await?;
    ensure!(
        !ak_pub_first.is_empty(),
        "AK pub modulus should not be empty on first boot"
    );

    // Reboot. The agent requests a TPM state refresh via the GSP RPC, but
    // stateless + hardware sealing mode must ignore it.
    agent.reboot().await?;
    let agent = vm.wait_for_reset().await?;

    // Second boot: re-send the binary and capture the AK public modulus again.
    let host_binary_path = tpm_guest_tests_artifact.get();
    let tpm_guest_tests =
        TpmGuestTests::send_tpm_guest_tests(&agent, host_binary_path, guest_binary_path, os_flavor)
            .await?;

    let ak_pub_second = tpm_guest_tests.read_ak_pub_modulus().await?;

    ensure!(
        ak_pub_first == ak_pub_second,
        "AK pub must remain stable across reboot in stateless + sealing mode, \
         but it changed (first={ak_pub_first}, second={ak_pub_second})"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}
