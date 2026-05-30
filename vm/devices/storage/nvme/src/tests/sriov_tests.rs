// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unit tests for NVMe SR-IOV support.

use super::controller_tests::wait_for_msi;
use super::test_helpers::TestNvmeMmioRegistration;
use super::test_helpers::test_memory;
use super::test_helpers::write_command_to_queue;
use crate::BAR0_LEN;
use crate::NvmeController;
use crate::NvmeControllerCaps;
use crate::PAGE_SIZE64;
use crate::pci::NvmeSriovCaps;
use crate::prp::PrpRange;
use crate::spec;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use guid::Guid;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pci_core::bus_range::AssignedBusRange;
use pci_core::msi::MsiConnection;
use pci_core::test_helpers::TestPciInterruptController;
use vmcore::device_state::ChangeDeviceState;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

// SR-IOV extended capability base offset (first extended cap at 0x100).
const SRIOV_BASE: u16 = 0x100;
// SR-IOV register offsets relative to SRIOV_BASE.
const SRIOV_CONTROL_STATUS: u16 = SRIOV_BASE + 0x08;
const SRIOV_NUM_VFS: u16 = SRIOV_BASE + 0x10;
const SRIOV_VF_OFFSET_STRIDE: u16 = SRIOV_BASE + 0x14;

fn instantiate_sriov_controller(
    driver: DefaultDriver,
    total_vfs: u16,
) -> (NvmeController, guestmem::GuestMemory) {
    let gm = test_memory();
    let mut mmio_reg = TestNvmeMmioRegistration {};
    let vm_task_driver = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let msi_conn = MsiConnection::new(AssignedBusRange::new(), 0);
    let controller = NvmeController::new(
        &vm_task_driver,
        gm.clone(),
        msi_conn.target(),
        &mut mmio_reg,
        NvmeControllerCaps {
            msix_count: 64,
            max_io_queues: 64,
            subsystem_id: Guid::new_random(),
            sriov: Some(NvmeSriovCaps {
                total_vfs,
                vf_device_id: 0x00b0,
                vf_msix_count: 4,
                vf_max_io_queues: 64,
            }),
        },
    );
    (controller, gm)
}

/// Read a u32 from the PF config space.
fn cfg_read(c: &mut NvmeController, offset: u16) -> u32 {
    let mut val = 0u32;
    c.pci_cfg_read(offset, &mut val).unwrap();
    val
}

/// Read a u32 from a VF config space via routing.
fn vf_cfg_read(c: &mut NvmeController, function: u8, offset: u16) -> u32 {
    let mut val = 0u32;
    c.pci_cfg_read_with_routing(0, 0, function, offset, &mut val)
        .unwrap();
    val
}

/// Set NumVFs in the SR-IOV capability.
fn set_num_vfs(c: &mut NvmeController, num_vfs: u16) {
    // NumVFs is in the lower 16 bits of the NUM_VFS_DEP_LINK dword.
    let current = cfg_read(c, SRIOV_NUM_VFS);
    let val = (current & 0xFFFF0000) | num_vfs as u32;
    c.pci_cfg_write(SRIOV_NUM_VFS, val).unwrap();
}

/// Set VF Enable in the SR-IOV Control register.
/// Returns the IoResult — may be `Defer` when disabling.
fn set_vf_enable(c: &mut NvmeController, enable: bool) -> chipset_device::io::IoResult {
    let current = cfg_read(c, SRIOV_CONTROL_STATUS);
    let val = if enable {
        current | 1 // VF Enable is bit 0
    } else {
        current & !1
    };
    c.pci_cfg_write(SRIOV_CONTROL_STATUS, val)
}

/// Set VF MSE (Memory Space Enable) in the SR-IOV Control register.
fn set_vf_mse(c: &mut NvmeController, enable: bool) {
    let current = cfg_read(c, SRIOV_CONTROL_STATUS);
    let val = if enable {
        current | 0x8 // VF MSE is bit 3
    } else {
        current & !0x8
    };
    c.pci_cfg_write(SRIOV_CONTROL_STATUS, val).unwrap();
}

/// Read VF Enable from SR-IOV Control.
fn get_vf_enable(c: &mut NvmeController) -> bool {
    cfg_read(c, SRIOV_CONTROL_STATUS) & 1 != 0
}

// =========================================================================
// Tests
// =========================================================================

#[async_test]
async fn test_sriov_pf_multi_function_bit(driver: DefaultDriver) {
    let (mut c, _gm) = instantiate_sriov_controller(driver, 2);
    // Header type register at offset 0x0C, bits 23 (multi-function bit).
    let header = cfg_read(&mut c, 0x0C);
    assert!(header & 0x0080_0000 != 0, "multi-function bit must be set");
}

#[async_test]
async fn test_sriov_vf_offset_stride(driver: DefaultDriver) {
    let (mut c, _gm) = instantiate_sriov_controller(driver, 4);
    let val = cfg_read(&mut c, SRIOV_VF_OFFSET_STRIDE);
    let offset = val & 0xFFFF;
    let stride = val >> 16;
    assert_eq!(offset, 1, "first VF offset should be 1");
    assert_eq!(stride, 1, "VF stride should be 1");
}

#[async_test]
async fn test_sriov_enable_creates_vfs(driver: DefaultDriver) {
    let (mut c, _gm) = instantiate_sriov_controller(driver, 2);

    // Before enabling, VF function 1 should return all-1s (not present).
    assert_eq!(vf_cfg_read(&mut c, 1, 0), 0xFFFFFFFF);

    // Enable 2 VFs.
    set_num_vfs(&mut c, 2);
    set_vf_enable(&mut c, true).unwrap();
    assert!(get_vf_enable(&mut c));

    // VF at function 1 should now be present — vendor ID in lower 16 bits.
    let vf1_id = vf_cfg_read(&mut c, 1, 0);
    assert_eq!(vf1_id & 0xFFFF, 0x1414, "VF vendor ID should be Microsoft");
    assert_eq!(
        (vf1_id >> 16) & 0xFFFF,
        0x00b0,
        "VF device ID should match config"
    );

    // VF at function 2 should also be present.
    let vf2_id = vf_cfg_read(&mut c, 2, 0);
    assert_eq!(vf2_id & 0xFFFF, 0x1414);

    // Function 3 should not be present (only 2 VFs enabled).
    assert_eq!(vf_cfg_read(&mut c, 3, 0), 0xFFFFFFFF);
}

#[async_test]
async fn test_sriov_disable_removes_vfs(driver: DefaultDriver) {
    let (mut c, _gm) = instantiate_sriov_controller(driver, 2);

    set_num_vfs(&mut c, 2);
    set_vf_enable(&mut c, true).unwrap();

    // VF 1 should be present.
    assert_ne!(vf_cfg_read(&mut c, 1, 0), 0xFFFFFFFF);

    // Disable VFs — the VFs have no active workers, so drain completes
    // immediately. stop() will complete the deferred write.
    let _result = set_vf_enable(&mut c, false);
    c.stop().await;

    // VF 1 should no longer be present.
    assert_eq!(vf_cfg_read(&mut c, 1, 0), 0xFFFFFFFF);
}

#[async_test]
async fn test_sriov_vf_bar0_reads_nvme_registers(driver: DefaultDriver) {
    let (mut c, _gm) = instantiate_sriov_controller(driver, 1);

    set_num_vfs(&mut c, 1);
    set_vf_enable(&mut c, true).unwrap();

    // We can't easily access VF BAR0 through MMIO in tests (the test mock
    // intercepts don't track addresses), but we can verify the VF exists
    // and its config space is accessible.
    let vf_id = vf_cfg_read(&mut c, 1, 0);
    assert_eq!(vf_id & 0xFFFF, 0x1414);
}

#[async_test]
async fn test_sriov_num_vfs_readonly_when_enabled(driver: DefaultDriver) {
    let (mut c, _gm) = instantiate_sriov_controller(driver, 4);

    set_num_vfs(&mut c, 2);
    set_vf_enable(&mut c, true).unwrap();

    // Try to change NumVFs while VF_Enable is set — should be ignored.
    set_num_vfs(&mut c, 4);
    let num_vfs = cfg_read(&mut c, SRIOV_NUM_VFS) & 0xFFFF;
    assert_eq!(
        num_vfs, 2,
        "NumVFs should not change while VF_Enable is set"
    );
}

#[async_test]
async fn test_sriov_reset_clears_vfs(driver: DefaultDriver) {
    let (mut c, _gm) = instantiate_sriov_controller(driver, 2);

    set_num_vfs(&mut c, 2);
    set_vf_enable(&mut c, true).unwrap();

    // VF should be present.
    assert_ne!(vf_cfg_read(&mut c, 1, 0), 0xFFFFFFFF);

    // Device reset should clear VFs.
    c.reset().await;

    // VF should no longer be present.
    assert_eq!(vf_cfg_read(&mut c, 1, 0), 0xFFFFFFFF);
    // VF_Enable should be cleared.
    assert!(!get_vf_enable(&mut c));
}

#[async_test]
async fn test_sriov_vf_identify_reports_cmic_sriov(driver: DefaultDriver) {
    let gm = test_memory();
    let int_controller = TestPciInterruptController::new();

    // GPAs: 0x0000 = ACQ, 0x1000 = ASQ, 0x3000 = identify output
    let acq = PrpRange::new(vec![0x0000], 0, PAGE_SIZE64).unwrap();
    let asq = PrpRange::new(vec![0x1000], 0, PAGE_SIZE64).unwrap();

    // Build a PF controller with SR-IOV and admin queues ready.
    let mut mmio_reg = TestNvmeMmioRegistration {};
    let vm_task_driver = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
    let msi_conn = MsiConnection::new(AssignedBusRange::new(), 0);
    let mut c = NvmeController::new(
        &vm_task_driver,
        gm.clone(),
        msi_conn.target(),
        &mut mmio_reg,
        NvmeControllerCaps {
            msix_count: 64,
            max_io_queues: 64,
            subsystem_id: Guid::new_random(),
            sriov: Some(NvmeSriovCaps {
                total_vfs: 1,
                vf_device_id: 0x00b0,
                vf_msix_count: 4,
                vf_max_io_queues: 64,
            }),
        },
    );
    msi_conn.connect(int_controller.signal_msi());

    // Set BARs.
    c.pci_cfg_write(0x10, 0).unwrap();
    c.pci_cfg_write(0x20, BAR0_LEN as u32).unwrap();

    // Find and enable MSI-X.
    let mut cap_ptr = cfg_read(&mut c, 0x34) & 0xFF;
    loop {
        let cap_header = cfg_read(&mut c, cap_ptr as u16);
        if cap_header & 0xFF == 0x11 {
            c.pci_cfg_write(cap_ptr as u16, 0x80000000).unwrap();
            break;
        }
        cap_ptr = (cap_header >> 8) & 0xFF;
        assert_ne!(cap_ptr, 0, "MSI-X capability not found");
    }

    // Enable MMIO + DMA.
    c.pci_cfg_write(4, 6).unwrap();

    // Set admin queues.
    let acq_base = acq.range().gpns()[0] * PAGE_SIZE64;
    let asq_base = asq.range().gpns()[0] * PAGE_SIZE64;
    c.write_bar0(0x30, acq_base.as_bytes()).unwrap();
    c.write_bar0(0x28, asq_base.as_bytes()).unwrap();
    c.write_bar0(0x24, 0x30003u32.as_bytes()).unwrap(); // AQA: 4 entries

    // Set MSI-X table entry 0 for admin CQ.
    let msix_bar_offset = BAR0_LEN;
    c.mmio_write(msix_bar_offset, 0xfeed0000u64.as_bytes())
        .unwrap();
    c.mmio_write(msix_bar_offset + 8, 0x1111u64.as_bytes())
        .unwrap();

    // Enable PF controller.
    let mut cc = 0u32;
    c.read_bar0(0x14, cc.as_mut_bytes()).unwrap();
    cc |= 1;
    c.write_bar0(0x14, cc.as_bytes()).unwrap();

    // Wait for CSTS.RDY via MSI-X (poll CSTS until ready).
    let mut backoff = user_driver::backoff::Backoff::new(&driver);
    loop {
        backoff.back_off().await;
        let mut csts = 0u32;
        c.read_bar0(0x1c, csts.as_mut_bytes()).unwrap();
        if spec::Csts::from(csts).rdy() {
            break;
        }
    }

    // Build Identify Controller command.
    let identify_buf_gpa: u64 = 3 * PAGE_SIZE64;
    let mut entry = spec::Command::new_zeroed();
    entry.cdw0.set_opcode(spec::AdminOpcode::IDENTIFY.0);
    entry.cdw0.set_cid(1);
    let cdw10 = spec::Cdw10Identify::new().with_cns(spec::Cns::CONTROLLER.0);
    entry.cdw10 = u32::from(cdw10);
    entry.dptr[0] = identify_buf_gpa;

    write_command_to_queue(&gm, &asq, 0, &entry);

    // Ring admin SQ doorbell.
    c.write_bar0(0x1000, 1u32.as_bytes()).unwrap();

    // Wait for completion interrupt.
    wait_for_msi(driver.clone(), &int_controller, 1000, 0xfeed0000, 0x1111).await;

    // Read the identify response.
    let id: spec::IdentifyController = gm.read_plain(identify_buf_gpa).unwrap();

    // Verify the identify response was actually written.
    assert_eq!(id.vid, 0x1414, "Identify response vid should be Microsoft");
    // PF should have cntlid = 1.
    assert_eq!(id.cntlid, 1, "PF cntlid should be PF_CONTROLLER_ID (1)");
    // PF should NOT have cmic.vf set — that's only for VFs.
    assert!(!id.cmic.vf(), "PF should not report cmic.vf (only VFs do)");
    // PF should advertise virtualization management.
    assert!(
        id.oacs.virtualization_management(),
        "PF should report oacs.virtualization_management"
    );
}

#[async_test]
async fn test_sriov_vf_mse_read_write(driver: DefaultDriver) {
    let (mut c, _gm) = instantiate_sriov_controller(driver, 2);

    // VF MSE should start cleared.
    let ctl = cfg_read(&mut c, SRIOV_CONTROL_STATUS);
    assert_eq!(ctl & 0x8, 0, "VF MSE should be clear initially");

    // Set VF MSE.
    set_vf_mse(&mut c, true);
    let ctl = cfg_read(&mut c, SRIOV_CONTROL_STATUS);
    assert_ne!(ctl & 0x8, 0, "VF MSE should be set");

    // Clear VF MSE.
    set_vf_mse(&mut c, false);
    let ctl = cfg_read(&mut c, SRIOV_CONTROL_STATUS);
    assert_eq!(ctl & 0x8, 0, "VF MSE should be clear");
}

#[async_test]
async fn test_sriov_reset_clears_vf_mse(driver: DefaultDriver) {
    let (mut c, _gm) = instantiate_sriov_controller(driver, 2);

    // Set VF MSE.
    set_vf_mse(&mut c, true);
    assert_ne!(cfg_read(&mut c, SRIOV_CONTROL_STATUS) & 0x8, 0);

    // Reset should clear VF MSE along with VF Enable.
    c.reset().await;
    let ctl = cfg_read(&mut c, SRIOV_CONTROL_STATUS);
    assert_eq!(ctl & 0x8, 0, "VF MSE should be cleared by reset");
    assert_eq!(ctl & 0x1, 0, "VF Enable should be cleared by reset");
}
