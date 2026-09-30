// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A mocked TDISP flow for an emulated OpenVMM TDISP device. This test flow
//! ensures that a full end-to-end flow of attestation and BAR acceptance is
//! correctly handled for an emulated TDISP device relayed over VPCI.
//!
//! Runs when the relay is started with
//! `OPENHCL_TEST_CONFIG=TDISP_VPCI_FLOW_TEST`. This flow is exercised
//! automatically within the openvmm test suite.
//!
//! The expected behavior is:
//! - An emulated TDISP device is created and relayed through the VPCI bus. (At
//!   the time of writing, this is an emulated NVMe device). The device requires
//!   attestation and the acceptance of its BAR0 range before it answers to any
//!   BAR requests.
//! - The device creates and advertises a TDISP capability and synthesized TDI
//!   report to the guest.
//! - The guest successfully attests the device over VPCI and accepts its
//!   register BAR range.
//! - Once attested and the BAR range accepted, the device's registers become
//!   accessible.
//! - Lastly, the TDI is unbound and the device's registers are expected to
//!   become inaccessible again.

use crate::CreateMemoryAccess;
use anyhow::Context as _;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigByteEnable;
use pci_core::spec::cfg_space::BarEncodingBits;
use pci_core::spec::cfg_space::Command;
use pci_core::spec::cfg_space::HeaderType00;
use std::sync::Arc;
use tdisp::TdispGuestUnbindReason;
use tdisp::TdispTdiState;
use tdisp::test_helpers::TDISP_MOCK_DEVICE_ID;
use tdisp::test_helpers::TDISP_MOCK_GUEST_PROTOCOL;
use tdisp::test_helpers::TDISP_MOCK_SUPPORTED_FEATURES;
use vpci_client::MemoryAccess;
use vpci_client::VpciDevice;

/// The BAR the emulated device keeps its registers behind, which is the range
/// TDISP has to accept into the guest before it answers.
const REGISTER_BAR: u16 = 0;

/// Exercises the mocked TDISP flow against `device`. Takes the fake relayed
/// TDISP device through a full attestation and teardown flow, checking on the
/// way that its registers are unreachable until the TDI has been attested and
/// the range accepted, and unreachable again once it is unbound.
///
/// * `device` - The relayed device to exercise.
/// * `mmio_access` - Used to reach the device's registers once its BAR has been
///   programmed.
/// * `bar_mmio` - Base of the window to program into the device's register BAR.
///   Reserved by the relay for this flow, so nothing else decodes there.
pub(crate) async fn run_test_flow(
    device: Arc<VpciDevice>,
    mmio_access: &dyn CreateMemoryAccess,
    bar_mmio: u64,
) -> anyhow::Result<()> {
    tracing::info!(
        "tdisp_test_mock_flow: exercising TDISP flow because OPENHCL_TEST_CONFIG=TDISP_VPCI_FLOW_TEST was set"
    );

    assert_eq!(device.tdisp().tdi_state().await, TdispTdiState::Unlocked);

    let device_interface_info = device
        .tdisp()
        .get_device_interface_info(TDISP_MOCK_GUEST_PROTOCOL)
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
    assert_eq!(device.tdisp().tdi_state().await, TdispTdiState::Unlocked);

    run_bar_access_flow(device.clone(), mmio_access, bar_mmio)
        .await
        .context("tdisp_test_mock_flow: failed to exercise TDISP attestation flow")?;

    Ok(())
}

/// Programs `bar_mmio` into the device's register BAR and turns on MMIO
/// decoding, so the device answers at a known address.
///
/// Returns the length the BAR decodes, which TDISP needs in order to accept the
/// range.
///
/// * `device` - The relayed device to program.
/// * `bar_mmio` - Base address to give the BAR.
fn program_register_bar(device: &VpciDevice, bar_mmio: u64) -> u64 {
    let bar_offset = HeaderType00::BAR0.0 + REGISTER_BAR * 4;

    // Size the BAR the way a PCI enumerator does. The client shadows BAR
    // writes rather than passing them to the device, so probing here costs
    // the device nothing.
    device.write_cfg(
        bar_offset,
        ByteEnabledDwordWrite::with_all_bytes_enabled(!0),
    );
    let mut mask = 0;
    device.read_cfg(
        bar_offset,
        ByteEnabledDwordRead::with_all_bytes_enabled(&mut mask),
    );
    let length = u64::from(!(mask & !0xf)) + 1;
    let is_64_bit = BarEncodingBits::from_bits(mask).type_64_bit();

    device.write_cfg(
        bar_offset,
        ByteEnabledDwordWrite::with_all_bytes_enabled(bar_mmio as u32),
    );
    if is_64_bit {
        device.write_cfg(
            bar_offset + 4,
            ByteEnabledDwordWrite::with_all_bytes_enabled((bar_mmio >> 32) as u32),
        );
    }

    // Enabling MMIO is what pushes the shadowed BAR through to the device.
    device.write_cfg(
        HeaderType00::STATUS_COMMAND.0,
        ByteEnabledDwordWrite::new(
            Command::new().with_mmio_enabled(true).into_bits().into(),
            PciConfigByteEnable::LOW_WORD,
        ),
    );

    tracing::info!(
        bar_mmio,
        length,
        is_64_bit,
        "tdisp_test_mock_flow: programmed the register BAR"
    );

    length
}

/// Returns the device's register BAR to its unprogrammed state, so that the
/// guest that follows starts from a device with nothing mapped.
///
/// * `device` - The relayed device to clear.
fn clear_register_bar(device: &VpciDevice) {
    device.write_cfg(
        HeaderType00::STATUS_COMMAND.0,
        ByteEnabledDwordWrite::new(0, PciConfigByteEnable::LOW_WORD),
    );
    let bar_offset = HeaderType00::BAR0.0 + REGISTER_BAR * 4;
    device.write_cfg(bar_offset, ByteEnabledDwordWrite::with_all_bytes_enabled(0));
    device.write_cfg(
        bar_offset + 4,
        ByteEnabledDwordWrite::with_all_bytes_enabled(0),
    );
}

/// Attests the device, then checks that its registers are reachable only while
/// TDISP says they are.
///
/// * `device` - The relayed device to exercise.
/// * `mmio_access` - Used to reach the device's registers.
/// * `bar_mmio` - Base of the window to program into the device's register BAR.
async fn run_bar_access_flow(
    device: Arc<VpciDevice>,
    mmio_access: &dyn CreateMemoryAccess,
    bar_mmio: u64,
) -> anyhow::Result<()> {
    let bar_length = program_register_bar(&device, bar_mmio);
    let mut bar = mmio_access
        .create_memory_access(bar_mmio)
        .context("tdisp_test_mock_flow: failed to map the device's register BAR")?;

    let read_registers = |bar: &mut Box<dyn MemoryAccess>| {
        let mut data = [0u8; 4];
        bar.read(bar_mmio, &mut data);
        u32::from_ne_bytes(data)
    };

    // Nothing has been attested, so the device must not answer. A window that
    // decodes nothing reads as all ones.
    let blocked = read_registers(&mut bar);
    tracing::info!(
        blocked,
        "tdisp_test_mock_flow: register BAR before attestation"
    );
    assert_eq!(
        blocked, !0,
        "the device answered with all FFs on its BAR before the TDI was attested"
    );

    run_attest_flow(device.clone()).await?;

    // Attested, so the range can be accepted into the guest's context. This is
    // the same call the relay makes when a guest enables MMIO.
    device
        .tdisp()
        .on_mmio_reconfigured(REGISTER_BAR, bar_mmio, bar_length)
        .await
        .context("tdisp_test_mock_flow: failed to unblock the register BAR")?;

    // The device answers now, and its first register is never all ones.
    let unblocked = read_registers(&mut bar);
    tracing::info!(
        unblocked,
        "tdisp_test_mock_flow: register BAR after the range was accepted"
    );
    assert_ne!(
        unblocked, !0,
        "the device answered with FFs on its BAR after the range was accepted"
    );

    // Unbinding takes the range away again, which is what leaves the device
    // safe for the guest that follows.
    device
        .tdisp()
        .unbind(TdispGuestUnbindReason::Graceful)
        .await;
    assert_eq!(device.tdisp().tdi_state().await, TdispTdiState::Unlocked);

    let reblocked = read_registers(&mut bar);
    tracing::info!(reblocked, "tdisp_test_mock_flow: register BAR after unbind");
    assert_eq!(
        reblocked, !0,
        "the device still answered with FFs on its BAR after the TDI was unbound"
    );

    clear_register_bar(&device);

    Ok(())
}

/// Attests `device` through the TDISP flow, checking the capabilities it
/// reports on the way and that the TDI ends up in `TdispTdiState::Run`.
async fn run_attest_flow(device: Arc<VpciDevice>) -> anyhow::Result<()> {
    // Ensure the device appears to be tdisp capable
    let tdisp_capabilities = device
        .tdisp()
        .query_capabilities()
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
    assert_eq!(device.tdisp().tdi_state().await, TdispTdiState::Unlocked);

    // If the above interface works, try to attest the device through the TDISP flow and ensure that it succeeds.
    device
        .tdisp()
        .attest(tdisp_capabilities)
        .await
        .context("tdisp_test_mock_flow: failed to attest device over vpci")?;

    assert_eq!(device.tdisp().tdi_state().await, TdispTdiState::Run);

    Ok(())
}
