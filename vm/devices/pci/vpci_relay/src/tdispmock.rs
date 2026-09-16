// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A mocked TDISP flow for the emulated TDISP devices OpenVMM produces for
//! tests.
//!
//! Runs when the relay is started with `OPENHCL_TEST_CONFIG=TDISP_VPCI_FLOW_TEST`,
//! in place of the capability probe a real device gets, and asserts that the
//! device answers with the mocked values the emulated device is expected to
//! report. A failure here means the paravisor and the emulated device disagree
//! about the TDISP protocol.

use anyhow::Context as _;
use openhcl_tdisp::TdispVirtualDeviceInterface;
use std::sync::Arc;
use tdisp::TdispTdiState;
use tdisp::test_helpers::TDISP_MOCK_DEVICE_ID;
use tdisp::test_helpers::TDISP_MOCK_GUEST_PROTOCOL;
use tdisp::test_helpers::TDISP_MOCK_SUPPORTED_FEATURES;
use vpci_client::VpciDevice;
use vpci_client::tdisp::TdispVpciAttestationInterface;

/// Exercises the mocked TDISP flow against `device`, leaving its TDI in
/// `TdispTdiState::Run`.
pub(crate) async fn run_test_flow(device: Arc<VpciDevice>) -> anyhow::Result<()> {
    // For now, exercise just the "get device interface" flow and ensure that the device responds as
    // TDISP capable and with the right mocked device information.

    tracing::info!(
        "tdisp_test_mock_flow: exercising TDISP flow because OPENHCL_TEST_CONFIG=TDISP_VPCI_FLOW_TEST was set"
    );

    assert_eq!(device.tdisp_tdi_state().await, TdispTdiState::Unlocked);

    let device_interface_info = device
        .tdisp_get_device_interface_info(TDISP_MOCK_GUEST_PROTOCOL)
        .await
        .context("tdisp_test_mock_flow: failed to get device interface info over vpci")?;

    tracing::info!(
        "tdisp_test_mock_flow: device interface info: {:?}",
        device_interface_info
    );

    assert_eq!(
        device_interface_info.guest_protocol_type,
        TDISP_MOCK_GUEST_PROTOCOL as i32
    );
    assert_eq!(device_interface_info.tdisp_device_id, TDISP_MOCK_DEVICE_ID);
    assert_eq!(
        device_interface_info.supported_features,
        TDISP_MOCK_SUPPORTED_FEATURES
    );
    assert_eq!(device.tdisp_tdi_state().await, TdispTdiState::Unlocked);

    run_attest_flow(device.clone())
        .await
        .context("tdisp_test_mock_flow: failed to exercise TDISP attestation flow")?;

    Ok(())
}

/// Attests `device` through the TDISP flow, checking the capabilities it
/// reports on the way and that the TDI ends up in `TdispTdiState::Run`.
async fn run_attest_flow(device: Arc<VpciDevice>) -> anyhow::Result<()> {
    // Ensure the device appears to be tdisp capable
    let tdisp_capabilities = device
        .tdisp_query_capabilities()
        .await
        .context("tdisp_test_mock_flow: failed to query TDISP capabilities over vpci")?;

    assert_eq!(
        tdisp_capabilities.guest_protocol_type,
        TDISP_MOCK_GUEST_PROTOCOL as i32
    );
    assert_eq!(tdisp_capabilities.tdisp_device_id, TDISP_MOCK_DEVICE_ID);
    assert_eq!(
        tdisp_capabilities.supported_features,
        TDISP_MOCK_SUPPORTED_FEATURES
    );
    assert_eq!(device.tdisp_tdi_state().await, TdispTdiState::Unlocked);

    // If the above interface works, try to attest the device through the TDISP flow and ensure that it succeeds.
    device
        .tdisp_attest_device(tdisp_capabilities)
        .await
        .context("tdisp_test_mock_flow: failed to attest device over vpci")?;

    assert_eq!(device.tdisp_tdi_state().await, TdispTdiState::Run);

    Ok(())
}
