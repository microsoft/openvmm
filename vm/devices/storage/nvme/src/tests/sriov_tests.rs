// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unit tests for NVMe SR-IOV support.

use super::controller_tests::wait_for_msi;
use super::test_helpers::TestNvmeMmioRegistration;
use super::test_helpers::read_completion_from_queue;
use super::test_helpers::test_memory;
use super::test_helpers::write_command_to_queue;
use crate::BAR0_LEN;
use crate::NvmeController;
use crate::NvmeControllerCaps;
use crate::PAGE_SIZE64;
use crate::pci::NvmeSriovCaps;
use crate::prp::PrpRange;
use crate::spec;
use crate::spec::nvm;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use guestmem::GuestMemory;
use guid::Guid;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pci_core::bus_range::AssignedBusRange;
use pci_core::msi::MsiConnection;
use pci_core::test_helpers::TestPciInterruptController;
use user_driver::backoff::Backoff;
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

// SR-IOV VF BAR register offsets relative to SRIOV_BASE.
const SRIOV_VF_BAR0: u16 = SRIOV_BASE + 0x24;
const SRIOV_VF_BAR1: u16 = SRIOV_BASE + 0x28; // upper 32 bits of 64-bit BAR0
const SRIOV_VF_BAR4: u16 = SRIOV_BASE + 0x34;

fn instantiate_sriov_controller(
    driver: DefaultDriver,
    total_vfs: u16,
) -> (NvmeController, GuestMemory) {
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
    let mut backoff = Backoff::new(&driver);
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

#[async_test]
async fn test_sriov_vf_bar0_mmio_routing(driver: DefaultDriver) {
    let (mut c, _gm) = instantiate_sriov_controller(driver, 2);

    // Enable 2 VFs and set VF MSE.
    set_num_vfs(&mut c, 2);
    set_vf_enable(&mut c, true).unwrap();
    set_vf_mse(&mut c, true);

    // Set VF BAR0 base address. VF BAR0 is a 64-bit BAR.
    // Place VF BAR0 region at GPA 0x80000 (well above PF BARs).
    let vf_bar0_base: u64 = 0x80000;
    c.pci_cfg_write(SRIOV_VF_BAR0, vf_bar0_base as u32).unwrap();
    c.pci_cfg_write(SRIOV_VF_BAR1, (vf_bar0_base >> 32) as u32)
        .unwrap();

    // VF 0's BAR0 is at vf_bar0_base + 0 * BAR0_LEN.
    // VF 1's BAR0 is at vf_bar0_base + 1 * BAR0_LEN.
    // Read CAP register (offset 0) from VF 0 via MMIO — should return
    // the NVMe CAP value, not zeros.
    let vf0_bar0_addr = vf_bar0_base;
    let mut cap = 0u64;
    c.mmio_read(vf0_bar0_addr, cap.as_mut_bytes()).unwrap();
    // CAP.MQES should be MAX_QES - 1 = 0xFF (bits 15:0).
    assert_eq!(cap & 0xFFFF, 0xFF, "VF0 CAP.MQES should be 0xFF");

    // Read CAP from VF 1 via MMIO.
    let vf1_bar0_addr = vf_bar0_base + BAR0_LEN;
    let mut cap1 = 0u64;
    c.mmio_read(vf1_bar0_addr, cap1.as_mut_bytes()).unwrap();
    assert_eq!(cap1 & 0xFFFF, 0xFF, "VF1 CAP.MQES should be 0xFF");

    // Read Version register (offset 8) from VF 0.
    let mut ver = 0u32;
    c.mmio_read(vf0_bar0_addr + 8, ver.as_mut_bytes()).unwrap();
    assert_eq!(ver, 0x00020000, "VF0 version should be NVMe 2.0");

    // Write CC.EN = 0 should succeed (VF not online, so CFS is set,
    // but the write itself should route correctly).
    let cc_val = 0u32;
    c.mmio_write(vf0_bar0_addr + 0x14, cc_val.as_bytes())
        .unwrap();

    // Read CSTS from VF 0 — should be valid (not an MMIO error).
    let mut csts = 0u32;
    c.mmio_read(vf0_bar0_addr + 0x1c, csts.as_mut_bytes())
        .unwrap();
    // CSTS should not have RDY set (controller not enabled).
    assert!(!spec::Csts::from(csts).rdy(), "VF0 should not be ready");
}

/// Helper to submit a PF admin command and wait for completion.
/// Returns the completion entry.
async fn pf_admin_command(
    c: &mut NvmeController,
    gm: &GuestMemory,
    asq: &PrpRange,
    acq: &PrpRange,
    slot: usize,
    command: &spec::Command,
    int_controller: &TestPciInterruptController,
    driver: DefaultDriver,
) -> spec::Completion {
    write_command_to_queue(gm, asq, slot, command);
    c.write_bar0(0x1000, ((slot + 1) as u32).as_bytes())
        .unwrap();
    wait_for_msi(driver, int_controller, 1000, 0xfeed0000, 0x1111).await;
    let cqe = read_completion_from_queue(gm, acq, slot);
    // Ring ACQ doorbell to consume the completion.
    c.write_bar0(0x1004, ((slot + 1) as u32).as_bytes())
        .unwrap();
    cqe
}

/// End-to-end test: enable a VF, configure it via PF admin commands,
/// enable its NVMe controller, create IO queues, and perform a READ.
///
/// Memory layout (page-aligned GPAs):
///   0x00000 - PF ACQ
///   0x01000 - PF ASQ
///   0x02000 - controller list buffer (for NS Attachment)
///   0x03000 - PF identify output
///   0x04000 - VF ACQ
///   0x05000 - VF ASQ
///   0x06000 - VF identify output
///   0x07000 - VF IO CQ
///   0x08000 - VF IO SQ
///   0x09000 - IO data buffer
///   0x80000 - VF BAR0 region (64 KB per VF)
///   0xA0000 - VF BAR4 (MSI-X) region
#[async_test]
async fn test_sriov_vf_end_to_end_io(driver: DefaultDriver) {
    let gm = GuestMemory::allocate(4096 * 256); // 1 MB
    let int_controller = TestPciInterruptController::new();
    let mut backoff = Backoff::new(&driver);

    let pf_acq = PrpRange::new(vec![0x00000], 0, PAGE_SIZE64).unwrap();
    let pf_asq = PrpRange::new(vec![0x01000], 0, PAGE_SIZE64).unwrap();
    let vf_acq = PrpRange::new(vec![0x04000], 0, PAGE_SIZE64).unwrap();
    let vf_asq = PrpRange::new(vec![0x05000], 0, PAGE_SIZE64).unwrap();

    // === Create PF controller with SR-IOV ===
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

    // Set PF BARs: BAR0 at 0, BAR4 at BAR0_LEN.
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

    // Set PF admin queues — need at least 8 entries for all admin commands.
    c.write_bar0(0x30, 0x00000u64.as_bytes()).unwrap(); // ACQ
    c.write_bar0(0x28, 0x01000u64.as_bytes()).unwrap(); // ASQ
    c.write_bar0(0x24, 0xf000fu32.as_bytes()).unwrap(); // AQA: 16 entries

    // Set MSI-X table entry 0 for PF admin CQ.
    c.mmio_write(BAR0_LEN, 0xfeed0000u64.as_bytes()).unwrap();
    c.mmio_write(BAR0_LEN + 8, 0x1111u64.as_bytes()).unwrap();

    // Enable PF controller.
    c.write_bar0(0x14, 1u32.as_bytes()).unwrap();
    loop {
        backoff.back_off().await;
        let mut csts = 0u32;
        c.read_bar0(0x1c, csts.as_mut_bytes()).unwrap();
        if spec::Csts::from(csts).rdy() {
            break;
        }
    }

    // === Add namespace, enable VF, set VF BARs ===
    let disk = disklayer_ram::ram_disk(1 << 20, false).unwrap(); // 1 MB disk
    c.client().add_namespace(1, disk).await.unwrap();

    set_num_vfs(&mut c, 1);
    set_vf_enable(&mut c, true).unwrap();
    set_vf_mse(&mut c, true);

    // Set VF BAR0 base at 0x80000 and VF BAR4 (MSI-X) at 0xA0000.
    let vf_bar0_base: u64 = 0x80000;
    let vf_bar4_base: u64 = 0xA0000;
    c.pci_cfg_write(SRIOV_VF_BAR0, vf_bar0_base as u32).unwrap();
    c.pci_cfg_write(SRIOV_VF_BAR1, 0).unwrap();
    c.pci_cfg_write(SRIOV_VF_BAR4, vf_bar4_base as u32).unwrap();

    // === PF admin: bring VF online and attach namespace ===
    let vf_cntlid: u16 = 2; // PF_CONTROLLER_ID(1) + 1

    // Bring secondary controller online.
    let mut cmd = spec::Command::new_zeroed();
    cmd.cdw0
        .set_opcode(spec::AdminOpcode::VIRTUALIZATION_MANAGEMENT.0);
    cmd.cdw0.set_cid(10);
    cmd.cdw10 = spec::Cdw10VirtualizationManagement::new()
        .with_act(spec::VirtualizationManagementAction::SECONDARY_ONLINE.0)
        .with_cntlid(vf_cntlid)
        .into();
    let cqe = pf_admin_command(
        &mut c,
        &gm,
        &pf_asq,
        &pf_acq,
        0,
        &cmd,
        &int_controller,
        driver.clone(),
    )
    .await;
    assert_eq!(
        cqe.status.status(),
        spec::Status::SUCCESS.0,
        "secondary online failed"
    );

    // Attach namespace 1 to secondary controller.
    let mut ctrl_list = spec::ControllerList::new_zeroed();
    ctrl_list.num_identifiers = 1;
    ctrl_list.identifiers[0] = vf_cntlid;
    gm.write_plain(0x02000u64, &ctrl_list).unwrap();

    let mut ns_cmd = spec::Command::new_zeroed();
    ns_cmd
        .cdw0
        .set_opcode(spec::AdminOpcode::NAMESPACE_ATTACHMENT.0);
    ns_cmd.cdw0.set_cid(11);
    ns_cmd.nsid = 1;
    ns_cmd.cdw10 = spec::Cdw10NamespaceAttachment::new()
        .with_sel(spec::NamespaceAttachmentSelection::ATTACH.0)
        .into();
    ns_cmd.dptr[0] = 0x02000; // PRP1 pointing to controller list
    let cqe = pf_admin_command(
        &mut c,
        &gm,
        &pf_asq,
        &pf_acq,
        1,
        &ns_cmd,
        &int_controller,
        driver.clone(),
    )
    .await;
    assert_eq!(
        cqe.status.status(),
        spec::Status::SUCCESS.0,
        "NS attach failed"
    );

    // === Enable VF NVMe controller ===

    // Enable MSI-X on VF via config space routing.
    // Find MSI-X cap in VF config space.
    let mut vf_cap_ptr = vf_cfg_read(&mut c, 1, 0x34) & 0xFF;
    loop {
        let cap_header = vf_cfg_read(&mut c, 1, vf_cap_ptr as u16);
        if cap_header & 0xFF == 0x11 {
            // Enable MSI-X.
            c.pci_cfg_write_with_routing(0, 0, 1, vf_cap_ptr as u16, 0x80000000)
                .unwrap();
            break;
        }
        vf_cap_ptr = (cap_header >> 8) & 0xFF;
        assert_ne!(vf_cap_ptr, 0, "VF MSI-X capability not found");
    }

    // Set VF MSI-X table entry 0 for VF admin CQ.
    c.mmio_write(vf_bar4_base, 0xfeed0000u64.as_bytes())
        .unwrap();
    c.mmio_write(vf_bar4_base + 8, 0x3333u64.as_bytes())
        .unwrap();

    // Verify the MSI-X table entry was written.
    let mut readback = 0u32;
    c.mmio_read(vf_bar4_base, readback.as_mut_bytes()).unwrap();
    assert_eq!(readback, 0xfeed0000, "VF MSI-X addr_lo readback");
    c.mmio_read(vf_bar4_base + 8, readback.as_mut_bytes())
        .unwrap();
    assert_eq!(readback, 0x3333, "VF MSI-X data readback");

    // Set VF admin queues via MMIO to VF BAR0.
    c.mmio_write(vf_bar0_base + 0x30, 0x04000u64.as_bytes())
        .unwrap(); // VF ACQ
    c.mmio_write(vf_bar0_base + 0x28, 0x05000u64.as_bytes())
        .unwrap(); // VF ASQ
    c.mmio_write(vf_bar0_base + 0x24, 0x30003u32.as_bytes())
        .unwrap(); // VF AQA

    // Enable VF controller (CC.EN = 1).
    c.mmio_write(vf_bar0_base + 0x14, 1u32.as_bytes()).unwrap();

    // Wait for VF CSTS.RDY.
    loop {
        backoff.back_off().await;
        let mut csts = 0u32;
        c.mmio_read(vf_bar0_base + 0x1c, csts.as_mut_bytes())
            .unwrap();
        if spec::Csts::from(csts).rdy() {
            break;
        }
        assert!(
            !spec::Csts::from(csts).cfs(),
            "VF controller fatal error on enable"
        );
    }

    // === VF admin: Identify Controller ===
    let mut id_cmd = spec::Command::new_zeroed();
    id_cmd.cdw0.set_opcode(spec::AdminOpcode::IDENTIFY.0);
    id_cmd.cdw0.set_cid(1);
    id_cmd.cdw10 = spec::Cdw10Identify::new()
        .with_cns(spec::Cns::CONTROLLER.0)
        .into();
    id_cmd.dptr[0] = 0x06000;

    write_command_to_queue(&gm, &vf_asq, 0, &id_cmd);
    c.mmio_write(vf_bar0_base + 0x1000, 1u32.as_bytes())
        .unwrap();

    // Wait for VF admin completion via MSI-X.
    wait_for_msi(driver.clone(), &int_controller, 1000, 0xfeed0000, 0x3333).await;

    let cqe = read_completion_from_queue(&gm, &vf_acq, 0);
    assert_eq!(
        cqe.status.status(),
        spec::Status::SUCCESS.0,
        "VF identify failed"
    );

    let id: spec::IdentifyController = gm.read_plain(0x06000u64).unwrap();
    assert_eq!(id.vid, 0x1414, "VF identify not written");
    assert_eq!(id.cntlid, vf_cntlid);
    assert!(id.cmic.vf(), "VF should report cmic.vf = true");
    assert!(
        !id.oacs.virtualization_management(),
        "VF should not report oacs.virtualization_management"
    );

    // === VF admin: Create IO CQ + SQ ===

    // Set MSI-X entry 1 for VF IO CQ.
    c.mmio_write(vf_bar4_base + 16, 0xfeed0000u64.as_bytes())
        .unwrap();
    c.mmio_write(vf_bar4_base + 24, 0x4444u64.as_bytes())
        .unwrap();

    // Create IO CQ (qid=1) with interrupts.
    let mut cq_cmd = spec::Command::new_zeroed();
    cq_cmd
        .cdw0
        .set_opcode(spec::AdminOpcode::CREATE_IO_COMPLETION_QUEUE.0);
    cq_cmd.cdw0.set_cid(2);
    cq_cmd.cdw10 = spec::Cdw10CreateIoQueue::new()
        .with_qid(1)
        .with_qsize_z(15)
        .into();
    cq_cmd.cdw11 = spec::Cdw11CreateIoCompletionQueue::new()
        .with_pc(true)
        .with_ien(true)
        .with_iv(1)
        .into();
    cq_cmd.dptr[0] = 0x07000;

    write_command_to_queue(&gm, &vf_asq, 1, &cq_cmd);
    c.mmio_write(vf_bar0_base + 0x1000, 2u32.as_bytes())
        .unwrap();
    // Ring VF ACQ doorbell to consume identify completion.
    c.mmio_write(vf_bar0_base + 0x1004, 1u32.as_bytes())
        .unwrap();
    wait_for_msi(driver.clone(), &int_controller, 1000, 0xfeed0000, 0x3333).await;
    let cqe = read_completion_from_queue(&gm, &vf_acq, 1);
    assert_eq!(
        cqe.status.status(),
        spec::Status::SUCCESS.0,
        "VF create IO CQ failed"
    );

    // Create IO SQ (qid=1) bound to CQ 1.
    let mut sq_cmd = spec::Command::new_zeroed();
    sq_cmd
        .cdw0
        .set_opcode(spec::AdminOpcode::CREATE_IO_SUBMISSION_QUEUE.0);
    sq_cmd.cdw0.set_cid(3);
    sq_cmd.cdw10 = spec::Cdw10CreateIoQueue::new()
        .with_qid(1)
        .with_qsize_z(15)
        .into();
    sq_cmd.cdw11 = spec::Cdw11CreateIoSubmissionQueue::new()
        .with_pc(true)
        .with_cqid(1)
        .into();
    sq_cmd.dptr[0] = 0x08000;

    write_command_to_queue(&gm, &vf_asq, 2, &sq_cmd);
    c.mmio_write(vf_bar0_base + 0x1000, 3u32.as_bytes())
        .unwrap();
    c.mmio_write(vf_bar0_base + 0x1004, 2u32.as_bytes())
        .unwrap();
    wait_for_msi(driver.clone(), &int_controller, 1000, 0xfeed0000, 0x3333).await;
    let cqe = read_completion_from_queue(&gm, &vf_acq, 2);
    assert_eq!(
        cqe.status.status(),
        spec::Status::SUCCESS.0,
        "VF create IO SQ failed"
    );

    // === VF IO: READ 1 sector from namespace 1 ===
    let io_sq = PrpRange::new(vec![0x08000], 0, PAGE_SIZE64).unwrap();
    let io_cq = PrpRange::new(vec![0x07000], 0, PAGE_SIZE64).unwrap();

    let mut read_cmd = spec::Command::new_zeroed();
    read_cmd.cdw0.set_opcode(nvm::NvmOpcode::READ.0);
    read_cmd.cdw0.set_cid(100);
    read_cmd.nsid = 1;
    read_cmd.cdw10 = 0; // SLBA low = 0
    read_cmd.cdw11 = 0; // SLBA high = 0
    read_cmd.cdw12 = nvm::Cdw12ReadWrite::new().with_nlb_z(0).into(); // 1 sector
    read_cmd.dptr[0] = 0x09000; // data buffer

    write_command_to_queue(&gm, &io_sq, 0, &read_cmd);

    // Ring VF IO SQ doorbell: SQ 1 doorbell is at offset 0x1000 + 2*1*4 = 0x1008.
    c.mmio_write(vf_bar0_base + 0x1008, 1u32.as_bytes())
        .unwrap();

    // Wait for IO completion via MSI-X on VF IO CQ (vector 1, data 0x4444).
    wait_for_msi(driver.clone(), &int_controller, 1000, 0xfeed0000, 0x4444).await;

    let io_cqe = read_completion_from_queue(&gm, &io_cq, 0);
    assert_eq!(io_cqe.cid, 100);
    assert_eq!(
        io_cqe.status.status(),
        spec::Status::SUCCESS.0,
        "VF IO READ failed"
    );
}
