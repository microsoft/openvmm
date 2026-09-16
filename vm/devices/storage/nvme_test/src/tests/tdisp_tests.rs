// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for the TDISP gate on the controller's register BAR.

use super::test_helpers::TestNvmeMmioRegistration;
use crate::NvmeFaultController;
use crate::NvmeFaultControllerCaps;
use chipset_device::ChipsetDevice;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigSpace;
use guestmem::GuestMemory;
use guid::Guid;
use mesh::CellUpdater;
use nvme_resources::fault::FaultConfiguration;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pci_core::msi::MsiConnection;
use tdisp::Command;
use tdisp::GuestToHostCommand;
use tdisp::GuestToHostResponseExt;
use tdisp::TdispGuestOperationErrorCode;
use tdisp::TdispMmioRangeAction;
use tdisp::test_helpers::TDISP_MOCK_GUEST_PROTOCOL;
use tdisp_proto::TdispCommandRequestBind;
use tdisp_proto::TdispCommandRequestGetDeviceInterfaceInfo;
use tdisp_proto::TdispCommandRequestModifyMmioRange;
use tdisp_proto::TdispCommandRequestStartTdi;
use tdisp_proto::TdispCommandRequestUnbind;
use tdisp_proto::TdispGuestUnbindReason;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;

/// The base the tests program into BAR 0.
const BAR0_BASE: u64 = 0;

/// Builds a controller that acts as an emulated TDISP device, with BAR 0
/// programmed and MMIO decoding enabled.
fn tdisp_controller(driver: DefaultDriver, gm: &GuestMemory) -> NvmeFaultController {
    let mut mmio_reg = TestNvmeMmioRegistration {};
    let vm_task_driver = &VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let msi_conn = MsiConnection::new();
    let mut controller = NvmeFaultController::new(
        vm_task_driver,
        gm.clone(),
        &msi_conn.target(),
        &mut mmio_reg,
        NvmeFaultControllerCaps {
            msix_count: 64,
            max_io_queues: 64,
            subsystem_id: Guid::new_random(),
        },
        FaultConfiguration::new(CellUpdater::new(false).cell()),
        true,
    );

    controller
        .pci_cfg_write(
            0x10,
            ByteEnabledDwordWrite::with_all_bytes_enabled(BAR0_BASE as u32),
        )
        .unwrap();
    // Enable MMIO decoding and bus mastering.
    controller
        .pci_cfg_write(4, ByteEnabledDwordWrite::with_all_bytes_enabled(6))
        .unwrap();

    controller
}

/// Sends `command` to the controller's TDISP interface and asserts the host
/// accepted it.
fn send_tdisp(controller: &mut NvmeFaultController, command: Command) {
    let response = controller
        .supports_tdisp_host()
        .expect("the controller is acting as a TDISP device")
        .tdisp_handle_guest_command(GuestToHostCommand {
            device_id: 0,
            command: Some(command),
        })
        .expect("the host handled the command");

    assert_eq!(
        response.error_code(),
        Some(TdispGuestOperationErrorCode::Success),
        "command failed: {response:?}"
    );
}

/// Drives the TDI from Unlocked to Run, which is where a guest is allowed to
/// ask for its MMIO ranges.
fn attest(controller: &mut NvmeFaultController) {
    send_tdisp(
        controller,
        Command::GetDeviceInterfaceInfo(TdispCommandRequestGetDeviceInterfaceInfo {
            guest_protocol_type: TDISP_MOCK_GUEST_PROTOCOL as i32,
        }),
    );
    send_tdisp(controller, Command::Bind(TdispCommandRequestBind {}));
    send_tdisp(
        controller,
        Command::StartTdi(TdispCommandRequestStartTdi {}),
    );
}

/// Asks the host to unblock or block the register BAR's range.
fn modify_bar0_range(controller: &mut NvmeFaultController, action: TdispMmioRangeAction) {
    send_tdisp(
        controller,
        Command::ModifyMmioRange(TdispCommandRequestModifyMmioRange {
            action: action as i32,
            range_id: 0,
            gpa_base: BAR0_BASE,
            range_len_bytes: crate::BAR0_LEN,
        }),
    );
}

/// Reads the first DWORD of the register BAR, which is the low half of the
/// NVMe `CAP` register.
fn read_bar0_start(controller: &mut NvmeFaultController) -> u32 {
    let mut data = [0u8; 4];
    controller
        .supports_mmio()
        .unwrap()
        .mmio_read(BAR0_BASE, &mut data)
        .unwrap();
    u32::from_ne_bytes(data)
}

#[async_test]
async fn register_bar_is_dark_until_the_range_is_unblocked(driver: DefaultDriver) {
    let gm = GuestMemory::allocate(0x1000);
    let mut controller = tdisp_controller(driver, &gm);

    // Nothing has been attested, so the BAR must not answer.
    assert_eq!(read_bar0_start(&mut controller), !0);

    // Attestation alone is not enough: the range still has to be accepted.
    attest(&mut controller);
    assert_eq!(read_bar0_start(&mut controller), !0);

    // Once the range is unblocked the registers are readable, and `CAP` is
    // never all ones.
    modify_bar0_range(&mut controller, TdispMmioRangeAction::UnblockMmioRange);
    let cap = read_bar0_start(&mut controller);
    assert_ne!(cap, !0);

    // Blocking the range again closes the window.
    modify_bar0_range(&mut controller, TdispMmioRangeAction::BlockMmioRange);
    assert_eq!(read_bar0_start(&mut controller), !0);

    // And so does unbinding from a range that is still unblocked.
    modify_bar0_range(&mut controller, TdispMmioRangeAction::UnblockMmioRange);
    assert_eq!(read_bar0_start(&mut controller), cap);
    send_tdisp(
        &mut controller,
        Command::Unbind(TdispCommandRequestUnbind {
            unbind_reason: TdispGuestUnbindReason::Graceful as i32,
        }),
    );
    assert_eq!(read_bar0_start(&mut controller), !0);
}

#[async_test]
async fn writes_are_dropped_while_the_range_is_blocked(driver: DefaultDriver) {
    let gm = GuestMemory::allocate(0x1000);
    let mut controller = tdisp_controller(driver, &gm);

    // The controller's interrupt mask register, which is writable once the
    // range is open.
    const INTMS: u64 = 0x0c;
    let write = |controller: &mut NvmeFaultController, value: u32| {
        controller
            .supports_mmio()
            .unwrap()
            .mmio_write(BAR0_BASE + INTMS, &value.to_ne_bytes())
            .unwrap()
    };

    attest(&mut controller);
    write(&mut controller, 0x1);

    // The write went nowhere, so the register still reads as its initial value
    // once the range is opened.
    modify_bar0_range(&mut controller, TdispMmioRangeAction::UnblockMmioRange);
    let mut data = [0u8; 4];
    controller
        .supports_mmio()
        .unwrap()
        .mmio_read(BAR0_BASE + INTMS, &mut data)
        .unwrap();
    assert_eq!(u32::from_ne_bytes(data), 0);
}

/// A controller that is not acting as a TDISP device has no gate at all.
#[async_test]
async fn a_plain_controller_answers_without_tdisp(driver: DefaultDriver) {
    let gm = GuestMemory::allocate(0x1000);
    let mut mmio_reg = TestNvmeMmioRegistration {};
    let vm_task_driver = &VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let msi_conn = MsiConnection::new();
    let mut controller = NvmeFaultController::new(
        vm_task_driver,
        gm.clone(),
        &msi_conn.target(),
        &mut mmio_reg,
        NvmeFaultControllerCaps {
            msix_count: 64,
            max_io_queues: 64,
            subsystem_id: Guid::new_random(),
        },
        FaultConfiguration::new(CellUpdater::new(false).cell()),
        false,
    );
    controller
        .pci_cfg_write(
            0x10,
            ByteEnabledDwordWrite::with_all_bytes_enabled(BAR0_BASE as u32),
        )
        .unwrap();
    controller
        .pci_cfg_write(4, ByteEnabledDwordWrite::with_all_bytes_enabled(6))
        .unwrap();

    assert!(controller.supports_tdisp_host().is_none());
    assert_ne!(read_bar0_start(&mut controller), !0);
}
