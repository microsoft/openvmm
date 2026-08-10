// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TPM tests that run against the older 1.38 TPM reference implementation.
//!
//! Each test here is a thin wrapper that reuses the corresponding test body
//! from [`super::tpm`], selecting [`PetriTpmVersion::V138`] instead of the
//! default 1.85 implementation.
//!
//! Test function names are kept short (and the module name is `tpm138` rather
//! than something more descriptive) because Hyper-V truncates VM names to 100
//! characters; see `resolve_test_config` in `test_igvm_agent_rpc_server`.

use petri::PetriTpmVersion;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ResolvedArtifact;
use petri::openvmm::OpenVmmPetriBackend;
use petri_artifacts_vmm_test::artifacts::guest_tools::TPM_GUEST_TESTS_LINUX_X64;
use petri_artifacts_vmm_test::artifacts::guest_tools::TPM_GUEST_TESTS_WINDOWS_X64;
#[cfg(windows)]
use petri_artifacts_vmm_test::artifacts::host_tools::TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64;
#[cfg(windows)]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_AARCH64;
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_X64;
use petri_artifacts_vmm_test::artifacts::test_vmgs::VMGS_WITH_16K_TPM;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::vmm_test;
#[cfg(windows)]
use vmm_test_macros::vmm_test_with;

/// 1.38 variant of [`super::tpm::boot_with_tpm`].
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
async fn boot_tpm<T: PetriVmmBackend>(config: PetriVmBuilder<T>) -> anyhow::Result<()> {
    super::tpm::boot_with_tpm_impl(config, PetriTpmVersion::V138).await
}

/// 1.38 variant of [`super::tpm::tpm_ak_cert_persisted`].
#[openvmm_test(
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64],
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[TPM_GUEST_TESTS_WINDOWS_X64]
)]
async fn ged_ak_persist<T>(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    extra_deps: (ResolvedArtifact<T>,),
) -> anyhow::Result<()> {
    super::tpm::tpm_ak_cert_persisted_impl(config, extra_deps, PetriTpmVersion::V138).await
}

/// 1.38 variant of [`super::tpm::tpm_ak_cert_retry`].
#[openvmm_test(
    openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64],
    openhcl_uefi_x64(vhd(windows_datacenter_core_2022_x64))[TPM_GUEST_TESTS_WINDOWS_X64]
)]
async fn ged_ak_retry<T>(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    extra_deps: (ResolvedArtifact<T>,),
) -> anyhow::Result<()> {
    super::tpm::tpm_ak_cert_retry_impl(config, extra_deps, PetriTpmVersion::V138).await
}

/// 1.38 variant of [`super::tpm::vbs_boot_with_attestation`].
#[openvmm_test(
    openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped)),
    ignore(reason = "OpenVMM VBS Ubuntu attestation boot is not yet reliable (microsoft/openvmm#2608)", openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64)))
)]
async fn vbs_boot(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    super::tpm::vbs_boot_with_attestation_impl(config, PetriTpmVersion::V138).await
}

/// 1.38 variant of [`super::tpm::tpm_test_platform_hierarchy_disabled`].
#[openvmm_test(openhcl_uefi_x64(vhd(ubuntu_2504_server_x64)))]
async fn plat_hier(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    super::tpm::tpm_test_platform_hierarchy_disabled_impl(config, PetriTpmVersion::V138).await
}

/// 1.38 variant of [`super::tpm::tpm_servicing`].
#[vmm_test(
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[LATEST_STANDARD_X64, VMGS_WITH_16K_TPM],
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[LATEST_STANDARD_X64, VMGS_WITH_16K_TPM],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[LATEST_STANDARD_AARCH64, VMGS_WITH_16K_TPM]
)]
async fn servicing<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    extra_deps: (
        ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
        ResolvedArtifact<VMGS_WITH_16K_TPM>,
    ),
) -> anyhow::Result<()> {
    super::tpm::tpm_servicing_impl(config, extra_deps, PetriTpmVersion::V138).await
}

/// 1.38 variant of [`super::tpm::ak_cert_cache`].
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
async fn ak_cache<T, S, U: PetriVmmBackend>(
    config: PetriVmBuilder<U>,
    extra_deps: (ResolvedArtifact<T>, ResolvedArtifact<S>),
) -> anyhow::Result<()> {
    super::tpm::ak_cert_cache_impl(config, extra_deps, PetriTpmVersion::V138).await
}

/// 1.38 variant of [`super::tpm::ak_cert_retry`].
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
async fn ak_retry<T, S, U: PetriVmmBackend>(
    config: PetriVmBuilder<U>,
    extra_deps: (ResolvedArtifact<T>, ResolvedArtifact<S>),
) -> anyhow::Result<()> {
    super::tpm::ak_cert_retry_hyperv_impl(config, extra_deps, PetriTpmVersion::V138).await
}

/// 1.38 variant of [`super::tpm::cvm_tpm_guest_tests`].
#[cfg(windows)]
#[vmm_test(
    hyperv_openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[tdx](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
)]
async fn cvm_guest<T, S, U: PetriVmmBackend>(
    config: PetriVmBuilder<U>,
    extra_deps: (ResolvedArtifact<T>, ResolvedArtifact<S>),
) -> anyhow::Result<()> {
    super::tpm::cvm_tpm_guest_tests_impl(config, extra_deps, PetriTpmVersion::V138).await
}

/// 1.38 variant of [`super::tpm::ak_pub_refresh`].
#[cfg(windows)]
#[vmm_test(
    hyperv_openhcl_uefi_x64[vbs](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[vbs](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[tdx](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[tdx](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
)]
async fn ak_refresh<T, S, U: PetriVmmBackend>(
    config: PetriVmBuilder<U>,
    extra_deps: (ResolvedArtifact<T>, ResolvedArtifact<S>),
) -> anyhow::Result<()> {
    super::tpm::ak_pub_refresh_impl(config, extra_deps, PetriTpmVersion::V138).await
}

/// 1.38 variant of [`super::tpm::skip_hw_unseal`].
#[cfg(windows)]
#[vmm_test_with(unstable(reason = "SNP hardware-unseal key-release test is unreliable in CI; awaiting a fix"), configs(
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64))[TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped))[TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
))]
async fn skip_unseal<T, U: PetriVmmBackend>(
    config: PetriVmBuilder<U>,
    extra_deps: (ResolvedArtifact<T>,),
) -> anyhow::Result<()> {
    super::tpm::skip_hw_unseal_impl(config, extra_deps, PetriTpmVersion::V138).await
}

/// 1.38 variant of [`super::tpm::use_hw_unseal`].
#[cfg(windows)]
#[vmm_test_with(unstable(reason = "SNP hardware-unseal key-release test is unreliable in CI; awaiting a fix"), configs(
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
))]
async fn use_unseal<T, S, U: PetriVmmBackend>(
    config: PetriVmBuilder<U>,
    extra_deps: (ResolvedArtifact<T>, ResolvedArtifact<S>),
) -> anyhow::Result<()> {
    super::tpm::use_hw_unseal_impl(config, extra_deps, PetriTpmVersion::V138).await
}

/// 1.38 variant of [`super::tpm::hw_seal_hash`].
#[cfg(windows)]
#[vmm_test(
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
)]
async fn seal_hash<T, S, U: PetriVmmBackend>(
    config: PetriVmBuilder<U>,
    extra_deps: (ResolvedArtifact<T>, ResolvedArtifact<S>),
) -> anyhow::Result<()> {
    super::tpm::hw_seal_hash_impl(config, extra_deps, PetriTpmVersion::V138).await
}

/// 1.38 variant of [`super::tpm::hw_seal_signer`].
#[cfg(windows)]
#[vmm_test(
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
)]
async fn seal_signer<T, S, U: PetriVmmBackend>(
    config: PetriVmBuilder<U>,
    extra_deps: (ResolvedArtifact<T>, ResolvedArtifact<S>),
) -> anyhow::Result<()> {
    super::tpm::hw_seal_signer_impl(config, extra_deps, PetriTpmVersion::V138).await
}

/// 1.38 variant of [`super::tpm::hw_ak_stable`].
#[cfg(windows)]
#[vmm_test(
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64))[TPM_GUEST_TESTS_LINUX_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
    hyperv_openhcl_uefi_x64[snp](vhd(windows_datacenter_core_2025_x64_prepped))[TPM_GUEST_TESTS_WINDOWS_X64, TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64],
)]
async fn hw_ak_stbl<T, S, U: PetriVmmBackend>(
    config: PetriVmBuilder<U>,
    extra_deps: (ResolvedArtifact<T>, ResolvedArtifact<S>),
) -> anyhow::Result<()> {
    super::tpm::hw_ak_stable_impl(config, extra_deps, PetriTpmVersion::V138).await
}
