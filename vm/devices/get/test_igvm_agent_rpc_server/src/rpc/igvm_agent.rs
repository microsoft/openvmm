// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared façade that exposes the test IGVM agent through a singleton instance.

use parking_lot::Mutex;
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

static TEST_AGENT: OnceLock<Mutex<TestIgvmAgent>> = OnceLock::new();

fn global_agent() -> &'static Mutex<TestIgvmAgent> {
    TEST_AGENT.get_or_init(|| Mutex::new(TestIgvmAgent::new()))
}

fn guard_agent() -> parking_lot::MutexGuard<'static, TestIgvmAgent> {
    global_agent().lock()
}

/// Install a scripted test plan for the shared test agent instance.
pub fn install_plan(setting: &IgvmAgentTestSetting) {
    let mut agent = guard_agent();
    agent.install_plan_from_setting(setting);
}

/// Process an attestation request payload using the shared test agent.
pub fn process_igvm_attest(report: &[u8]) -> TestAgentResult<Vec<u8>> {
    let mut agent = guard_agent();
    let (payload, expected_len) = agent.handle_request(report).map_err(|err| match err {
        Error::InvalidIgvmAttestRequest => TestAgentFacadeError::InvalidRequest,
        _ => TestAgentFacadeError::AgentFailure,
    })?;
    if payload.len() != expected_len as usize {
        return Err(TestAgentFacadeError::InvalidRequest);
    }
    Ok(payload)
}
