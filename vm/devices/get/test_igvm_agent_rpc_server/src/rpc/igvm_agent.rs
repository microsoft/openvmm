// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-VM test IGVM agent façade.
//!
//! Each VM gets its own [`TestIgvmAgent`] keyed by VM name.  The test plan
//! for a given VM is resolved by matching its name against a hardcoded
//! mapping (see [`config_for_vm_name`]).  A default plan can also be
//! installed via the CLI `--test_config` flag; it applies to VMs whose
//! names do not match any known pattern.
//!
//! **Naming convention** – Hyper-V VM names are capped at 100 characters
//! (see `petri::vm::make_vm_safe_name`).  The prefix added by the test
//! macro can consume ~85 characters on the worst-case image name, leaving
//! only ~15 characters for the test function name.  Keep test function
//! names short (≤ 15 chars) so the distinctive part is never truncated.

use get_resources::ged::IgvmAttestTestConfig;
use parking_lot::Mutex;
use std::collections::HashMap;
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
    /// Live per-VM agents, lazily created on first request.
    agents: HashMap<String, TestIgvmAgent>,
}

static REGISTRY: OnceLock<Mutex<AgentRegistry>> = OnceLock::new();

fn registry() -> &'static Mutex<AgentRegistry> {
    REGISTRY.get_or_init(|| {
        Mutex::new(AgentRegistry {
            default_setting: None,
            agents: HashMap::new(),
        })
    })
}

/// Hardcoded mapping from VM name substrings to test configurations.
///
/// The substring must be short enough to survive Hyper-V's 100-char name
/// limit even on the longest image prefixes (~85 chars).  Keep patterns
/// ≤ 15 characters.
///
/// Hyper-V VM names are built as:
///   `{module}::{vmm}_{firmware}_{arch}_{image}_{isolation}_{test_fn}`
/// We match by `contains` on the test function name portion.
fn config_for_vm_name(vm_name: &str) -> Option<IgvmAgentTestSetting> {
    /// (substring, config) pairs – order does not matter since each
    /// pattern is unique.
    const KNOWN_TEST_CONFIGS: &[(&str, IgvmAttestTestConfig)] = &[
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
            "ubuntu_2504_server_x64_snp_use_hw_unseal",
            IgvmAttestTestConfig::KeyReleaseFailure,
        ),
        (
            "windows_datacenter_core_2025_x64_prepped_snp_use_hw_unseal",
            IgvmAttestTestConfig::KeyReleaseFailure,
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

/// Process an attestation request payload for the given VM.
///
/// On first contact the VM's agent is created and configured:
/// 1. If the VM name matches a hardcoded pattern, that config is used.
/// 2. Otherwise the default plan (if any) is installed.
pub fn process_igvm_attest(vm_name: &str, report: &[u8]) -> TestAgentResult<Vec<u8>> {
    let mut reg = registry().lock();

    // Clone the default setting before entering the entry API so the
    // borrow checker is happy.
    let default_setting = reg.default_setting.clone();

    let agent = reg.agents.entry(vm_name.to_owned()).or_insert_with(|| {
        let mut agent = TestIgvmAgent::new(vm_name);
        if let Some(setting) = config_for_vm_name(vm_name) {
            agent.install_plan_from_setting(&setting);
        } else if let Some(ref default) = default_setting {
            agent.install_plan_from_setting(default);
        }
        tracing::info!(vm_name, "created per-VM test agent");
        agent
    });

    let (payload, expected_len) = agent.handle_request(report).map_err(|err| match err {
        Error::InvalidIgvmAttestRequest => TestAgentFacadeError::InvalidRequest,
        _ => TestAgentFacadeError::AgentFailure,
    })?;
    if payload.len() != expected_len as usize {
        return Err(TestAgentFacadeError::InvalidRequest);
    }
    Ok(payload)
}
