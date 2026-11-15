// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-VM test IGVM agent façade.
//!
//! Each VM gets its own [`TestIgvmAgent`] keyed by VM name.  The test plan
//! for a given VM is resolved by matching its name against a hardcoded
//! mapping (see [`resolve_test_config`]).  A default plan can also be
//! installed via the CLI `--test_config` flag; it applies to VMs whose
//! names do not match any known pattern.
//!
//! **Naming convention** – Hyper-V VM names are capped at 100 characters
//! (see `make_vm_safe_name`).  Longer names retain only the first
//! 96 characters followed by a four-character hash.  Include the module
//! prefix when checking lengths: the Windows CVM `cvm_guest` names are
//! 94 characters in `multiarch::tpm` and 97 in `multiarch::tpm138`.

use get_resources::ged::IgvmAttestTestConfig;
use guid::Guid;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use test_igvm_agent_lib::Error;
use test_igvm_agent_lib::IgvmAgentTestSetting;
use test_igvm_agent_lib::TestIgvmAgent;

/// Errors surfaced by the test IGVM agent façade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestAgentFacadeError {
    /// The request payload could not be processed by the agent.
    InvalidRequest,
    /// The underlying agent reported an unexpected failure.
    AgentFailure,
}

/// Convenience result type for façade invocations.
pub type TestAgentResult<T> = Result<T, TestAgentFacadeError>;

/// Per-VM agent registry.
struct AgentRegistry {
    /// Default setting (from CLI `--test_config`), applied to VMs that
    /// don't match any hardcoded pattern.
    default_setting: Option<IgvmAgentTestSetting>,
    /// Live per-VM agents, lazily created on first request. Each agent has its
    /// own lock so request processing for unrelated VMs can run concurrently.
    agents: HashMap<String, Arc<Mutex<TestIgvmAgent>>>,
    /// Maps a VM's `VmId` GUID to its resolved test config.
    ///
    /// The two RPC entry points receive different `VmName` values: the IGVM
    /// attest path (`RpcIGVmAttest`) gets the descriptive Hyper-V VM name
    /// (which [`resolve_test_config`] matches against), while the GSP path
    /// (`RpcVmGspRequest`) only gets the VM's runtime GUID. Both paths do,
    /// however, share the same `VmId`. The attest path runs first on every
    /// boot (OpenHCL performs key release before requesting GSP data), so it
    /// records the resolved config here keyed by `VmId`, allowing the GSP
    /// path to recover it.
    vm_id_settings: HashMap<Guid, IgvmAgentTestSetting>,
}

static REGISTRY: OnceLock<Mutex<AgentRegistry>> = OnceLock::new();

fn registry() -> &'static Mutex<AgentRegistry> {
    REGISTRY.get_or_init(|| {
        Mutex::new(AgentRegistry {
            default_setting: None,
            agents: HashMap::new(),
            vm_id_settings: HashMap::new(),
        })
    })
}

/// Resolve the test configuration for a VM by matching its name against
/// known `{image}_{isolation}_{test_fn}` substrings.
///
/// Hyper-V VM names are built as:
///   `{module}::{vmm}_{firmware}_{arch}_{image}_{isolation}_{test_fn}`
///
/// We intentionally list each image/isolation combination separately
/// rather than matching on just the short test-function suffix.  The RPC
/// server is shared across *all* concurrent VMs, and each VM gets its
/// own agent keyed by its full name.  If two tests share the same
/// function name but run with different images or isolation types, a
/// short suffix match would silently hand them the same config even
/// though they may require different plans.  Enumerating every
/// combination ensures each VM is mapped unambiguously.
///
/// Petri strips the crate name from the module path, leaving prefixes such
/// as `multiarch::tpm::` and `multiarch::tpm138::`.  Names over 100 characters
/// are truncated to 96 characters plus a four-character hash.  Keep
/// `{test_fn}` short enough that the full pattern survives this conversion.
///
/// When adding a new image or isolation variant for an existing test
/// function, add a corresponding entry here.
fn resolve_test_config(vm_name: &str) -> Option<IgvmAgentTestSetting> {
    /// (substring, config) pairs — order does not matter since each
    /// pattern is unique.
    const KNOWN_TEST_CONFIGS: &[(&str, IgvmAttestTestConfig)] = &[
        (
            "ubuntu_2504_server_x64_vbs_cvm_guest",
            IgvmAttestTestConfig::AkCertNoRenewal,
        ),
        (
            "ubuntu_2504_server_x64_snp_cvm_guest",
            IgvmAttestTestConfig::AkCertNoRenewal,
        ),
        (
            "ubuntu_2504_server_x64_tdx_cvm_guest",
            IgvmAttestTestConfig::AkCertNoRenewal,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_vbs_cvm_guest",
            IgvmAttestTestConfig::AkCertNoRenewal,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_snp_cvm_guest",
            IgvmAttestTestConfig::AkCertNoRenewal,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_tdx_cvm_guest",
            IgvmAttestTestConfig::AkCertNoRenewal,
        ),
        (
            "ubuntu_2504_server_x64_ak_cert_retry",
            IgvmAttestTestConfig::AkCertRequestFailureAndRetryExtended,
        ),
        (
            "windows_datacenter_core_2022_x64_ak_cert_retry",
            IgvmAttestTestConfig::AkCertRequestFailureAndRetryExtended,
        ),
        (
            "ubuntu_2504_server_x64_vbs_ak_cert_retry",
            IgvmAttestTestConfig::AkCertRequestFailureAndRetryExtended,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_vbs_ak_cert_retry",
            IgvmAttestTestConfig::AkCertRequestFailureAndRetryExtended,
        ),
        (
            "ubuntu_2504_server_x64_snp_ak_cert_retry",
            IgvmAttestTestConfig::AkCertRequestFailureAndRetryExtended,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_snp_ak_cert_retry",
            IgvmAttestTestConfig::AkCertRequestFailureAndRetryExtended,
        ),
        (
            "ubuntu_2504_server_x64_tdx_ak_cert_retry",
            IgvmAttestTestConfig::AkCertRequestFailureAndRetryExtended,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_tdx_ak_cert_retry",
            IgvmAttestTestConfig::AkCertRequestFailureAndRetryExtended,
        ),
        (
            "ubuntu_2504_server_x64_ak_cert_cache",
            IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended,
        ),
        (
            "windows_datacenter_core_2022_x64_ak_cert_cache",
            IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended,
        ),
        (
            "ubuntu_2504_server_x64_vbs_ak_cert_cache",
            IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_vbs_ak_cert_cache",
            IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended,
        ),
        (
            "ubuntu_2504_server_x64_snp_ak_cert_cache",
            IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_snp_ak_cert_cache",
            IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended,
        ),
        (
            "ubuntu_2504_server_x64_tdx_ak_cert_cache",
            IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_tdx_ak_cert_cache",
            IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended,
        ),
        (
            "ubuntu_2504_server_x64_snp_skip_hw_unseal",
            IgvmAttestTestConfig::KeyReleaseFailureSkipHwUnsealing,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_snp_skip_hw_unseal",
            IgvmAttestTestConfig::KeyReleaseFailureSkipHwUnsealing,
        ),
        (
            "ubuntu_2504_server_x64_tdx_skip_hw_unseal",
            IgvmAttestTestConfig::KeyReleaseFailureSkipHwUnsealing,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_tdx_skip_hw_unseal",
            IgvmAttestTestConfig::KeyReleaseFailureSkipHwUnsealing,
        ),
        (
            "ubuntu_2504_server_x64_snp_use_hw_unseal",
            IgvmAttestTestConfig::KeyReleaseFailure,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_snp_use_hw_unseal",
            IgvmAttestTestConfig::KeyReleaseFailure,
        ),
        (
            "ubuntu_2504_server_x64_tdx_use_hw_unseal",
            IgvmAttestTestConfig::KeyReleaseFailure,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_tdx_use_hw_unseal",
            IgvmAttestTestConfig::KeyReleaseFailure,
        ),
        (
            "ubuntu_2504_server_x64_vbs_ak_pub_refresh",
            IgvmAttestTestConfig::StateRefresh,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_vbs_ak_pub_refresh",
            IgvmAttestTestConfig::StateRefresh,
        ),
        (
            "ubuntu_2504_server_x64_tdx_ak_pub_refresh",
            IgvmAttestTestConfig::StateRefresh,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_tdx_ak_pub_refresh",
            IgvmAttestTestConfig::StateRefresh,
        ),
        (
            "ubuntu_2504_server_x64_snp_ak_pub_refresh",
            IgvmAttestTestConfig::StateRefresh,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_snp_ak_pub_refresh",
            IgvmAttestTestConfig::StateRefresh,
        ),
        (
            "ubuntu_2504_server_x64_snp_hw_ak_stable",
            IgvmAttestTestConfig::StateRefresh,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_snp_hw_ak_stable",
            IgvmAttestTestConfig::StateRefresh,
        ),
        (
            "ubuntu_2504_server_x64_tdx_hw_ak_stable",
            IgvmAttestTestConfig::StateRefresh,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_tdx_hw_ak_stable",
            IgvmAttestTestConfig::StateRefresh,
        ),
        // `multiarch::tpm138` runs the same tests against the 1.38 vTPM. Its
        // test-function names are abbreviated to keep VM names under the
        // 100-character limit, so they need their own patterns here.
        (
            "ubuntu_2504_server_x64_ak_retry",
            IgvmAttestTestConfig::AkCertRequestFailureAndRetryExtended,
        ),
        (
            "windows_datacenter_core_2022_x64_ak_retry",
            IgvmAttestTestConfig::AkCertRequestFailureAndRetryExtended,
        ),
        (
            "ubuntu_2504_server_x64_vbs_ak_retry",
            IgvmAttestTestConfig::AkCertRequestFailureAndRetryExtended,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_vbs_ak_retry",
            IgvmAttestTestConfig::AkCertRequestFailureAndRetryExtended,
        ),
        (
            "ubuntu_2504_server_x64_snp_ak_retry",
            IgvmAttestTestConfig::AkCertRequestFailureAndRetryExtended,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_snp_ak_retry",
            IgvmAttestTestConfig::AkCertRequestFailureAndRetryExtended,
        ),
        (
            "ubuntu_2504_server_x64_tdx_ak_retry",
            IgvmAttestTestConfig::AkCertRequestFailureAndRetryExtended,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_tdx_ak_retry",
            IgvmAttestTestConfig::AkCertRequestFailureAndRetryExtended,
        ),
        (
            "ubuntu_2504_server_x64_ak_cache",
            IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended,
        ),
        (
            "windows_datacenter_core_2022_x64_ak_cache",
            IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended,
        ),
        (
            "ubuntu_2504_server_x64_vbs_ak_cache",
            IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_vbs_ak_cache",
            IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended,
        ),
        (
            "ubuntu_2504_server_x64_snp_ak_cache",
            IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_snp_ak_cache",
            IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended,
        ),
        (
            "ubuntu_2504_server_x64_tdx_ak_cache",
            IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_tdx_ak_cache",
            IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended,
        ),
        (
            "ubuntu_2504_server_x64_snp_skip_unseal",
            IgvmAttestTestConfig::KeyReleaseFailureSkipHwUnsealing,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_snp_skip_unseal",
            IgvmAttestTestConfig::KeyReleaseFailureSkipHwUnsealing,
        ),
        (
            "ubuntu_2504_server_x64_snp_use_unseal",
            IgvmAttestTestConfig::KeyReleaseFailure,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_snp_use_unseal",
            IgvmAttestTestConfig::KeyReleaseFailure,
        ),
        (
            "ubuntu_2504_server_x64_vbs_ak_refresh",
            IgvmAttestTestConfig::StateRefresh,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_vbs_ak_refresh",
            IgvmAttestTestConfig::StateRefresh,
        ),
        (
            "ubuntu_2504_server_x64_tdx_ak_refresh",
            IgvmAttestTestConfig::StateRefresh,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_tdx_ak_refresh",
            IgvmAttestTestConfig::StateRefresh,
        ),
        (
            "ubuntu_2504_server_x64_snp_ak_refresh",
            IgvmAttestTestConfig::StateRefresh,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_snp_ak_refresh",
            IgvmAttestTestConfig::StateRefresh,
        ),
        (
            "ubuntu_2504_server_x64_snp_hw_ak_stbl",
            IgvmAttestTestConfig::StateRefresh,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_snp_hw_ak_stbl",
            IgvmAttestTestConfig::StateRefresh,
        ),
    ];

    for &(pattern, config) in KNOWN_TEST_CONFIGS {
        if vm_name.contains(pattern) {
            tracing::info!(vm_name, pattern, "matched test config for VM");
            return Some(IgvmAgentTestSetting::TestConfig(config));
        }
    }

    None
}

/// Install a default test plan used as a fallback for VMs that don't
/// match any hardcoded pattern.
pub fn install_default_plan(setting: &IgvmAgentTestSetting) {
    let mut reg = registry().lock();
    reg.default_setting = Some(setting.clone());
}

/// Whether the resolved test config for the VM with the given `vm_id` requests
/// that the GSP RPC report `state_refresh_request`.
///
/// The GSP RPC (`RpcVmGspRequest`) only receives the VM's runtime GUID, not the
/// descriptive Hyper-V name that [`resolve_test_config`] matches against. It
/// therefore looks up the config recorded by the IGVM attest path (keyed by
/// `VmId`), falling back to the default setting (from CLI `--test-config`).
/// Returns `false` (the original behavior, raising `RPC_S_SERVER_UNAVAILABLE`)
/// unless the config is [`IgvmAttestTestConfig::StateRefresh`].
pub fn gsp_state_refresh_requested(vm_id: &Guid) -> bool {
    let reg = registry().lock();
    let setting = reg
        .vm_id_settings
        .get(vm_id)
        .cloned()
        .or_else(|| reg.default_setting.clone());

    matches!(
        setting,
        Some(IgvmAgentTestSetting::TestConfig(
            IgvmAttestTestConfig::StateRefresh
        ))
    )
}

/// Process an attestation request payload for the given VM.
///
/// On first contact the VM's agent is created and configured:
/// 1. If the VM name matches a hardcoded pattern, that config is used.
/// 2. Otherwise the default plan (if any) is installed.
///
/// The resolved config is also recorded keyed by `vm_id` so the GSP RPC path
/// (which only sees the VM's runtime GUID, not the descriptive name) can
/// recover it later in the same boot.
pub fn process_igvm_attest(
    vm_id: Option<Guid>,
    vm_name: &str,
    report: &[u8],
) -> TestAgentResult<Vec<u8>> {
    let agent = {
        let mut reg = registry().lock();

        // Clone the default setting before entering the entry API so the
        // borrow checker is happy.
        let default_setting = reg.default_setting.clone();

        // Record the resolved config keyed by `vm_id` for the GSP RPC path.
        if let Some(vm_id) = vm_id {
            if let Some(setting) = resolve_test_config(vm_name).or_else(|| default_setting.clone())
            {
                reg.vm_id_settings.insert(vm_id, setting);
            }
        }

        reg.agents
            .entry(vm_name.to_owned())
            .or_insert_with(|| {
                let mut agent = TestIgvmAgent::new(vm_name);
                if let Some(setting) = resolve_test_config(vm_name) {
                    agent.install_plan_from_setting(&setting);
                } else if let Some(ref default) = default_setting {
                    agent.install_plan_from_setting(default);
                }
                tracing::info!(vm_name, "created per-VM test agent");
                Arc::new(Mutex::new(agent))
            })
            .clone()
    };

    // Key generation and response encryption can be expensive. Keep them
    // outside the registry lock while preserving request order for this VM.
    let (payload, expected_len) = agent
        .lock()
        .handle_request(report)
        .map_err(|err| match err {
            Error::InvalidIgvmAttestRequest => TestAgentFacadeError::InvalidRequest,
            _ => TestAgentFacadeError::AgentFailure,
        })?;
    if payload.len() != expected_len as usize {
        return Err(TestAgentFacadeError::InvalidRequest);
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use test_with_tracing::test;

    // Mirror petri/src/vm/mod.rs::make_vm_safe_name, which is private, without
    // pulling the VMM test framework into this RPC server's unit tests.
    fn make_vm_safe_name(name: &str) -> String {
        if name.len() <= 100 {
            name.to_owned()
        } else {
            let mut hasher = DefaultHasher::new();
            name.hash(&mut hasher);
            format!("{}{:04x}", &name[..96], hasher.finish() & 0xffff)
        }
    }

    #[test]
    fn resolve_cvm_guest_config() {
        // vmm_test_macros::ResolvedConfig::name_prefix adds the backend,
        // firmware, arch, image and isolation before the function name.
        // petri::test::Test::name strips only the crate name, not these modules.
        for (module, extra_len) in [("multiarch::tpm", 0), ("multiarch::tpm138", 3)] {
            for (image, base_len) in [
                ("ubuntu_2504_server_x64", 76),
                ("windows_datacenter_core_2025_x64_prepped", 94),
            ] {
                for isolation in ["vbs", "snp", "tdx"] {
                    let full_name =
                        format!("{module}::hyperv_openhcl_uefi_x64_{image}_{isolation}_cvm_guest");
                    assert_eq!(full_name.len(), base_len + extra_len, "{full_name}");
                    let vm_name = make_vm_safe_name(&full_name);
                    // All 12 real names fit, including the 97-character
                    // Windows tpm138 names: 96..=100 must NOT be truncated.
                    assert_eq!(vm_name, full_name);
                    assert!(
                        matches!(
                            resolve_test_config(&vm_name),
                            Some(IgvmAgentTestSetting::TestConfig(
                                IgvmAttestTestConfig::AkCertNoRenewal
                            ))
                        ),
                        "{vm_name}"
                    );
                }
            }
        }
    }

    #[test]
    fn resolve_truncated_windows_cvm_guest_config() {
        for isolation in ["vbs", "snp", "tdx"] {
            let full_name = format!(
                "multiarch::tpm::hyperv_openhcl_uefi_x64_windows_datacenter_core_2025_x64_prepped_{isolation}_cvm_guest"
            );
            // Synthetic suffix exercises truncation; the actual cvm_guest
            // names above do not need it. The 94-character pattern survives
            // in the 96-character prefix, followed by the four-character hash.
            let extended_name = format!("{full_name}_variant");
            assert_eq!(extended_name.len(), 102);
            let vm_name = make_vm_safe_name(&extended_name);
            assert_eq!(vm_name.len(), 100);
            assert_eq!(&vm_name[..96], &extended_name[..96]);
            assert!(vm_name[96..].bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(
                matches!(
                    resolve_test_config(&vm_name),
                    Some(IgvmAgentTestSetting::TestConfig(
                        IgvmAttestTestConfig::AkCertNoRenewal
                    ))
                ),
                "{vm_name}"
            );
        }
    }
}
