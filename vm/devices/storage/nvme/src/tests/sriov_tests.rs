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
        crate::VF_DEVICE_ID as u32,
        "VF device ID should be the NVMe VF device ID"
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
    // PF shares a subsystem with its VFs, so it must report multi_controller.
    assert!(
        id.cmic.multi_controller(),
        "PF should report cmic.multi_controller"
    );
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
    // The VFs were never brought online, so every BAR0 register reads back as
    // all-ones. A successful all-ones read still proves the access routed to
    // the VF (an unrouted address would return an MMIO error instead).
    let vf0_bar0_addr = vf_bar0_base;
    let mut cap = 0u64;
    c.mmio_read(vf0_bar0_addr, cap.as_mut_bytes()).unwrap();
    assert_eq!(cap, !0, "VF0 (offline) CAP should read all-ones");

    // Read CAP from VF 1 via MMIO.
    let vf1_bar0_addr = vf_bar0_base + BAR0_LEN;
    let mut cap1 = 0u64;
    c.mmio_read(vf1_bar0_addr, cap1.as_mut_bytes()).unwrap();
    assert_eq!(cap1, !0, "VF1 (offline) CAP should read all-ones");

    // Read Version register (offset 8) from VF 0.
    let mut ver = 0u32;
    c.mmio_read(vf0_bar0_addr + 8, ver.as_mut_bytes()).unwrap();
    assert_eq!(ver, !0, "VF0 (offline) version should read all-ones");

    // Writing CC.EN to an offline VF is dropped, but the write must still
    // route (not error).
    let cc_val = 0u32;
    c.mmio_write(vf0_bar0_addr + 0x14, cc_val.as_bytes())
        .unwrap();

    // Read CSTS from VF 0 — routes successfully and reads all-ones while
    // offline (CSTS == ~0 is the host's "device gone" sentinel).
    let mut csts = 0u32;
    c.mmio_read(vf0_bar0_addr + 0x1c, csts.as_mut_bytes())
        .unwrap();
    assert_eq!(csts, !0, "VF0 (offline) CSTS should read all-ones");
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

    // Detach namespace 1 from the PF (it was attached there by
    // `add_namespace`) so it can be attached to the private VF.
    let mut pf_list = spec::ControllerList::new_zeroed();
    pf_list.num_identifiers = 1;
    pf_list.identifiers[0] = 1; // PF controller ID
    gm.write_plain(0x02000u64, &pf_list).unwrap();

    let mut detach_cmd = spec::Command::new_zeroed();
    detach_cmd
        .cdw0
        .set_opcode(spec::AdminOpcode::NAMESPACE_ATTACHMENT.0);
    detach_cmd.cdw0.set_cid(12);
    detach_cmd.nsid = 1;
    detach_cmd.cdw10 = spec::Cdw10NamespaceAttachment::new()
        .with_sel(spec::NamespaceAttachmentSelection::DETACH.0)
        .into();
    detach_cmd.dptr[0] = 0x02000;
    let cqe = pf_admin_command(
        &mut c,
        &gm,
        &pf_asq,
        &pf_acq,
        1,
        &detach_cmd,
        &int_controller,
        driver.clone(),
    )
    .await;
    assert_eq!(
        cqe.status.status(),
        spec::Status::SUCCESS.0,
        "NS detach from PF failed"
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
        2,
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
        id.cmic.multi_controller(),
        "VF should report cmic.multi_controller"
    );
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

/// Controller ID of the single VF created by [`SriovVfHarness`].
const VF_CNTLID: u16 = 2;

/// Controller ID of the PF (primary controller).
const PF_CNTLID: u16 = 1;

/// A test harness with a PF (enabled, namespace 1 added) and a single VF
/// created with its BARs configured. The VF's secondary controller starts
/// **offline** and its NVMe controller is **not** yet enabled.
///
/// Memory layout (page-aligned GPAs):
///   0x00000 - PF ACQ        0x04000 - VF ACQ
///   0x01000 - PF ASQ        0x05000 - VF ASQ
///   0x02000 - controller list (NS attachment)
///   0x06000 - VF identify output
///   0x80000 - VF BAR0 region 0xA0000 - VF BAR4 (MSI-X) region
struct SriovVfHarness {
    c: NvmeController,
    gm: GuestMemory,
    driver: DefaultDriver,
    int_controller: TestPciInterruptController,
    pf_asq: PrpRange,
    pf_acq: PrpRange,
    vf_asq: PrpRange,
    vf_acq: PrpRange,
    vf_bar0_base: u64,
    vf_bar4_base: u64,
    pf_slot: usize,
    vf_sq_tail: usize,
    vf_cq_head: usize,
}

/// Build a PF with SR-IOV, enable it, add namespace 1, and create one VF
/// (offline, controller not enabled) with its BARs configured.
async fn setup_pf_with_offline_vf(driver: DefaultDriver) -> SriovVfHarness {
    let gm = GuestMemory::allocate(4096 * 256); // 1 MB
    let int_controller = TestPciInterruptController::new();
    let mut backoff = Backoff::new(&driver);

    let pf_acq = PrpRange::new(vec![0x00000], 0, PAGE_SIZE64).unwrap();
    let pf_asq = PrpRange::new(vec![0x01000], 0, PAGE_SIZE64).unwrap();
    let vf_acq = PrpRange::new(vec![0x04000], 0, PAGE_SIZE64).unwrap();
    let vf_asq = PrpRange::new(vec![0x05000], 0, PAGE_SIZE64).unwrap();

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
                vf_msix_count: 4,
                vf_max_io_queues: 64,
            }),
        },
    );
    msi_conn.connect(int_controller.signal_msi());

    // Set PF BARs.
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

    // Set PF admin queues (16 entries) and enable the PF.
    c.write_bar0(0x30, 0x00000u64.as_bytes()).unwrap(); // ACQ
    c.write_bar0(0x28, 0x01000u64.as_bytes()).unwrap(); // ASQ
    c.write_bar0(0x24, 0xf000fu32.as_bytes()).unwrap(); // AQA: 16 entries
    c.mmio_write(BAR0_LEN, 0xfeed0000u64.as_bytes()).unwrap();
    c.mmio_write(BAR0_LEN + 8, 0x1111u64.as_bytes()).unwrap();
    c.write_bar0(0x14, 1u32.as_bytes()).unwrap();
    loop {
        backoff.back_off().await;
        let mut csts = 0u32;
        c.read_bar0(0x1c, csts.as_mut_bytes()).unwrap();
        if spec::Csts::from(csts).rdy() {
            break;
        }
    }

    // Add namespace 1 to the PF.
    let disk = disklayer_ram::ram_disk(1 << 20, false).unwrap(); // 1 MB disk
    c.client().add_namespace(1, disk).await.unwrap();

    // Enable the VF and configure its BARs.
    set_num_vfs(&mut c, 1);
    set_vf_enable(&mut c, true).unwrap();
    set_vf_mse(&mut c, true);

    let vf_bar0_base: u64 = 0x80000;
    let vf_bar4_base: u64 = 0xA0000;
    c.pci_cfg_write(SRIOV_VF_BAR0, vf_bar0_base as u32).unwrap();
    c.pci_cfg_write(SRIOV_VF_BAR1, 0).unwrap();
    c.pci_cfg_write(SRIOV_VF_BAR4, vf_bar4_base as u32).unwrap();

    let mut h = SriovVfHarness {
        c,
        gm,
        driver,
        int_controller,
        pf_asq,
        pf_acq,
        vf_asq,
        vf_acq,
        vf_bar0_base,
        vf_bar4_base,
        pf_slot: 0,
        vf_sq_tail: 0,
        vf_cq_head: 0,
    };

    // `add_namespace` allocates namespace 1 and attaches it to the PF. Detach
    // it from the PF so it can be attached to the (private) VF by tests.
    assert_eq!(
        h.detach_from_pf(1).await.status.status(),
        spec::Status::SUCCESS.0,
        "detaching namespace 1 from the PF failed"
    );
    h
}

impl SriovVfHarness {
    /// Submit a command on the PF admin queue and wait for its completion.
    async fn pf_command(&mut self, command: &spec::Command) -> spec::Completion {
        let slot = self.pf_slot;
        self.pf_slot += 1;
        pf_admin_command(
            &mut self.c,
            &self.gm,
            &self.pf_asq,
            &self.pf_acq,
            slot,
            command,
            &self.int_controller,
            self.driver.clone(),
        )
        .await
    }

    /// Re-drive the PF enable sequence after a device reset: reprogram BARs,
    /// MSI-X, and admin queues, enable the controller, and wait for RDY. The
    /// admin queue indices are reset since the queues are freshly created.
    async fn reenable_pf(&mut self) {
        // Set PF BARs.
        self.c.pci_cfg_write(0x10, 0).unwrap();
        self.c.pci_cfg_write(0x20, BAR0_LEN as u32).unwrap();

        // Find and enable MSI-X.
        let mut cap_ptr = cfg_read(&mut self.c, 0x34) & 0xFF;
        loop {
            let cap_header = cfg_read(&mut self.c, cap_ptr as u16);
            if cap_header & 0xFF == 0x11 {
                self.c.pci_cfg_write(cap_ptr as u16, 0x80000000).unwrap();
                break;
            }
            cap_ptr = (cap_header >> 8) & 0xFF;
            assert_ne!(cap_ptr, 0, "MSI-X capability not found");
        }

        // Enable MMIO + DMA.
        self.c.pci_cfg_write(4, 6).unwrap();

        // Set PF admin queues (16 entries) and enable the PF.
        self.c.write_bar0(0x30, 0x00000u64.as_bytes()).unwrap(); // ACQ
        self.c.write_bar0(0x28, 0x01000u64.as_bytes()).unwrap(); // ASQ
        self.c.write_bar0(0x24, 0xf000fu32.as_bytes()).unwrap(); // AQA: 16 entries
        self.c
            .mmio_write(BAR0_LEN, 0xfeed0000u64.as_bytes())
            .unwrap();
        self.c
            .mmio_write(BAR0_LEN + 8, 0x1111u64.as_bytes())
            .unwrap();
        self.c.write_bar0(0x14, 1u32.as_bytes()).unwrap();
        let mut backoff = Backoff::new(&self.driver);
        loop {
            backoff.back_off().await;
            let mut csts = 0u32;
            self.c.read_bar0(0x1c, csts.as_mut_bytes()).unwrap();
            if spec::Csts::from(csts).rdy() {
                break;
            }
        }
        self.pf_slot = 0;
    }

    /// Issue a Virtualization Management command to online/offline the VF's
    /// secondary controller.
    async fn set_secondary_online(&mut self, online: bool) -> spec::Completion {
        let act = if online {
            spec::VirtualizationManagementAction::SECONDARY_ONLINE
        } else {
            spec::VirtualizationManagementAction::SECONDARY_OFFLINE
        };
        let mut cmd = spec::Command::new_zeroed();
        cmd.cdw0
            .set_opcode(spec::AdminOpcode::VIRTUALIZATION_MANAGEMENT.0);
        cmd.cdw0.set_cid(10);
        cmd.cdw10 = spec::Cdw10VirtualizationManagement::new()
            .with_act(act.0)
            .with_cntlid(VF_CNTLID)
            .into();
        self.pf_command(&cmd).await
    }

    /// Attach or detach namespace `nsid` to/from the VF via the PF admin
    /// Namespace Attachment command.
    async fn namespace_attachment(&mut self, nsid: u32, attach: bool) -> spec::Completion {
        let cmd = self.namespace_attachment_command(nsid, attach);
        self.pf_command(&cmd).await
    }

    /// Detach namespace `nsid` from the PF itself via the Namespace Attachment
    /// command, so it can be attached to a VF as a private namespace.
    async fn detach_from_pf(&mut self, nsid: u32) -> spec::Completion {
        let mut ctrl_list = spec::ControllerList::new_zeroed();
        ctrl_list.num_identifiers = 1;
        ctrl_list.identifiers[0] = PF_CNTLID;
        self.gm.write_plain(0x02000u64, &ctrl_list).unwrap();

        let mut cmd = spec::Command::new_zeroed();
        cmd.cdw0
            .set_opcode(spec::AdminOpcode::NAMESPACE_ATTACHMENT.0);
        cmd.cdw0.set_cid(12);
        cmd.nsid = nsid;
        cmd.cdw10 = spec::Cdw10NamespaceAttachment::new()
            .with_sel(spec::NamespaceAttachmentSelection::DETACH.0)
            .into();
        cmd.dptr[0] = 0x02000;
        self.pf_command(&cmd).await
    }

    /// Build a Namespace Attachment command (writing the controller list to
    /// guest memory) without submitting it.
    fn namespace_attachment_command(&mut self, nsid: u32, attach: bool) -> spec::Command {
        let mut ctrl_list = spec::ControllerList::new_zeroed();
        ctrl_list.num_identifiers = 1;
        ctrl_list.identifiers[0] = VF_CNTLID;
        self.gm.write_plain(0x02000u64, &ctrl_list).unwrap();

        let sel = if attach {
            spec::NamespaceAttachmentSelection::ATTACH
        } else {
            spec::NamespaceAttachmentSelection::DETACH
        };
        let mut cmd = spec::Command::new_zeroed();
        cmd.cdw0
            .set_opcode(spec::AdminOpcode::NAMESPACE_ATTACHMENT.0);
        cmd.cdw0.set_cid(11);
        cmd.nsid = nsid;
        cmd.cdw10 = spec::Cdw10NamespaceAttachment::new().with_sel(sel.0).into();
        cmd.dptr[0] = 0x02000;
        cmd
    }

    /// Submit a PF admin command without waiting; returns its queue slot.
    fn pf_submit(&mut self, command: &spec::Command) -> usize {
        let slot = self.pf_slot;
        self.pf_slot += 1;
        write_command_to_queue(&self.gm, &self.pf_asq, slot, command);
        self.c
            .write_bar0(0x1000, ((slot + 1) as u32).as_bytes())
            .unwrap();
        slot
    }

    /// Read and consume the PF completion at the given slot.
    fn pf_consume(&mut self, slot: usize) -> spec::Completion {
        let cqe = read_completion_from_queue(&self.gm, &self.pf_acq, slot);
        self.c
            .write_bar0(0x1004, ((slot + 1) as u32).as_bytes())
            .unwrap();
        cqe
    }

    /// Wait until an interrupt with each of the two data values has been
    /// observed (in any order). Used when a single PF admin command produces
    /// both a PF completion and a routed VF AEN.
    async fn wait_for_two_msi(&mut self, data_a: u32, data_b: u32) {
        let mut backoff = Backoff::new(&self.driver);
        let (mut got_a, mut got_b) = (false, false);
        for _ in 0..100 {
            while let Some(int) = self.int_controller.get_next_interrupt() {
                got_a |= int.1 == data_a;
                got_b |= int.1 == data_b;
            }
            if got_a && got_b {
                return;
            }
            backoff.back_off().await;
        }
        panic!("did not observe both interrupts (a={got_a}, b={got_b})");
    }

    /// Enable MSI-X and admin queues on the VF and write CC.EN=1, then poll
    /// CSTS until RDY (returns `true`) or CFS (returns `false`).
    async fn enable_vf_controller(&mut self) -> bool {
        // Enable MSI-X on the VF via config space routing.
        let mut vf_cap_ptr = vf_cfg_read(&mut self.c, 1, 0x34) & 0xFF;
        loop {
            let cap_header = vf_cfg_read(&mut self.c, 1, vf_cap_ptr as u16);
            if cap_header & 0xFF == 0x11 {
                self.c
                    .pci_cfg_write_with_routing(0, 0, 1, vf_cap_ptr as u16, 0x80000000)
                    .unwrap();
                break;
            }
            vf_cap_ptr = (cap_header >> 8) & 0xFF;
            assert_ne!(vf_cap_ptr, 0, "VF MSI-X capability not found");
        }

        // VF MSI-X table entry 0 for the VF admin CQ.
        self.c
            .mmio_write(self.vf_bar4_base, 0xfeed0000u64.as_bytes())
            .unwrap();
        self.c
            .mmio_write(self.vf_bar4_base + 8, 0x3333u64.as_bytes())
            .unwrap();

        // VF admin queues (16 entries) and CC.EN = 1.
        self.c
            .mmio_write(self.vf_bar0_base + 0x30, 0x04000u64.as_bytes())
            .unwrap();
        self.c
            .mmio_write(self.vf_bar0_base + 0x28, 0x05000u64.as_bytes())
            .unwrap();
        self.c
            .mmio_write(self.vf_bar0_base + 0x24, 0xf000fu32.as_bytes())
            .unwrap();
        self.c
            .mmio_write(self.vf_bar0_base + 0x14, 1u32.as_bytes())
            .unwrap();

        let mut backoff = Backoff::new(&self.driver);
        loop {
            backoff.back_off().await;
            let mut csts = 0u32;
            self.c
                .mmio_read(self.vf_bar0_base + 0x1c, csts.as_mut_bytes())
                .unwrap();
            let csts = spec::Csts::from(csts);
            if csts.cfs() {
                return false;
            }
            if csts.rdy() {
                return true;
            }
        }
    }

    /// Read the VF's CSTS register as a raw u32.
    fn vf_csts_raw(&mut self) -> u32 {
        let mut csts = 0u32;
        self.c
            .mmio_read(self.vf_bar0_base + 0x1c, csts.as_mut_bytes())
            .unwrap();
        csts
    }

    /// Write a command to the VF admin SQ and ring its doorbell.
    fn vf_submit(&mut self, command: &spec::Command) {
        write_command_to_queue(&self.gm, &self.vf_asq, self.vf_sq_tail, command);
        self.vf_sq_tail += 1;
        self.c
            .mmio_write(
                self.vf_bar0_base + 0x1000,
                (self.vf_sq_tail as u32).as_bytes(),
            )
            .unwrap();
    }

    /// Wait for a VF admin CQ completion (via MSI-X) and consume it.
    async fn vf_wait_completion(&mut self) -> spec::Completion {
        wait_for_msi(
            self.driver.clone(),
            &self.int_controller,
            1000,
            0xfeed0000,
            0x3333,
        )
        .await;
        let cqe = read_completion_from_queue(&self.gm, &self.vf_acq, self.vf_cq_head);
        self.vf_cq_head += 1;
        self.c
            .mmio_write(
                self.vf_bar0_base + 0x1004,
                (self.vf_cq_head as u32).as_bytes(),
            )
            .unwrap();
        cqe
    }

    /// Issue Identify Active Namespace List (CNS 0x02) on the VF and return
    /// the reported NSIDs.
    async fn vf_active_namespaces(&mut self) -> Vec<u32> {
        let out_gpa = 0x06000u64;
        let mut id_cmd = spec::Command::new_zeroed();
        id_cmd.cdw0.set_opcode(spec::AdminOpcode::IDENTIFY.0);
        id_cmd.cdw0.set_cid(60);
        id_cmd.cdw10 = spec::Cdw10Identify::new()
            .with_cns(spec::Cns::ACTIVE_NAMESPACES.0)
            .into();
        id_cmd.nsid = 0;
        id_cmd.dptr[0] = out_gpa;
        self.vf_submit(&id_cmd);
        let cqe = self.vf_wait_completion().await;
        assert_eq!(
            cqe.status.status(),
            spec::Status::SUCCESS.0,
            "VF identify active namespaces failed"
        );
        // The response is a zero-terminated list of u32 NSIDs.
        let mut nsids = Vec::new();
        for i in 0..1024u64 {
            let nsid: u32 = self.gm.read_plain(out_gpa + i * 4).unwrap();
            if nsid == 0 {
                break;
            }
            nsids.push(nsid);
        }
        nsids
    }

    /// Issue an Identify namespace-list command (CNS in `cns`) on the PF and
    /// return the reported NSIDs.
    async fn pf_namespace_list(&mut self, cns: spec::Cns) -> Vec<u32> {
        let out_gpa = 0x07000u64;
        let mut id_cmd = spec::Command::new_zeroed();
        id_cmd.cdw0.set_opcode(spec::AdminOpcode::IDENTIFY.0);
        id_cmd.cdw0.set_cid(61);
        id_cmd.cdw10 = spec::Cdw10Identify::new().with_cns(cns.0).into();
        id_cmd.nsid = 0;
        id_cmd.dptr[0] = out_gpa;
        let cqe = self.pf_command(&id_cmd).await;
        assert_eq!(
            cqe.status.status(),
            spec::Status::SUCCESS.0,
            "PF identify namespace list failed"
        );
        let mut nsids = Vec::new();
        for i in 0..1024u64 {
            let nsid: u32 = self.gm.read_plain(out_gpa + i * 4).unwrap();
            if nsid == 0 {
                break;
            }
            nsids.push(nsid);
        }
        nsids
    }

    /// Build a Namespace Attachment command targeting an arbitrary controller.
    fn attachment_command_to(&mut self, nsid: u32, cntlid: u16, attach: bool) -> spec::Command {
        let mut ctrl_list = spec::ControllerList::new_zeroed();
        ctrl_list.num_identifiers = 1;
        ctrl_list.identifiers[0] = cntlid;
        self.gm.write_plain(0x02000u64, &ctrl_list).unwrap();

        let sel = if attach {
            spec::NamespaceAttachmentSelection::ATTACH
        } else {
            spec::NamespaceAttachmentSelection::DETACH
        };
        let mut cmd = spec::Command::new_zeroed();
        cmd.cdw0
            .set_opcode(spec::AdminOpcode::NAMESPACE_ATTACHMENT.0);
        cmd.cdw0.set_cid(13);
        cmd.nsid = nsid;
        cmd.cdw10 = spec::Cdw10NamespaceAttachment::new().with_sel(sel.0).into();
        cmd.dptr[0] = 0x02000;
        cmd
    }
}

/// An offline VF presents all-ones BAR0 registers, so a host enabling it
/// never observes RDY (and reads CSTS == ~0, the "device gone" sentinel).
#[async_test]
async fn test_sriov_vf_enable_while_offline_reads_all_ones(driver: DefaultDriver) {
    let mut h = setup_pf_with_offline_vf(driver).await;

    // The VF was never brought online; enabling it must not reach RDY.
    let ready = h.enable_vf_controller().await;
    assert!(!ready, "offline VF must not reach RDY");

    // Every BAR0 register reads back as all-ones while offline.
    assert_eq!(h.vf_csts_raw(), !0, "offline CSTS must read all-ones");
    let mut cap = 0u64;
    h.c.mmio_read(h.vf_bar0_base, cap.as_mut_bytes()).unwrap();
    assert_eq!(cap, !0, "offline CAP must read all-ones");
    let mut vs = 0u32;
    h.c.mmio_read(h.vf_bar0_base + 0x08, vs.as_mut_bytes())
        .unwrap();
    assert_eq!(vs, !0, "offline VS must read all-ones");
}

/// A host that enables an offline VF keeps polling CSTS (as the Linux driver
/// does while waiting for RDY). Re-reading CSTS must keep returning all-ones
/// and must not panic by re-polling an enable that already settled.
#[async_test]
async fn test_sriov_vf_enable_while_offline_csts_reread(driver: DefaultDriver) {
    let mut h = setup_pf_with_offline_vf(driver).await;

    let ready = h.enable_vf_controller().await;
    assert!(!ready, "offline VF must not reach RDY");

    // Re-read CSTS several times, mimicking a driver still polling for RDY.
    for _ in 0..3 {
        assert_eq!(h.vf_csts_raw(), !0, "offline CSTS must stay all-ones");
    }
}

/// Criterion 9: after a Virtualization Management SECONDARY_ONLINE completes,
/// a subsequent CC.EN on that VF must reach RDY without spuriously setting CFS.
#[async_test]
async fn test_sriov_vf_online_then_enable_reaches_ready(driver: DefaultDriver) {
    let mut h = setup_pf_with_offline_vf(driver).await;

    let cqe = h.set_secondary_online(true).await;
    assert_eq!(
        cqe.status.status(),
        spec::Status::SUCCESS.0,
        "secondary online failed"
    );

    // Online has completed (happens-before), so the immediately-following
    // enable must succeed.
    let ready = h.enable_vf_controller().await;
    assert!(ready, "online VF must reach RDY without CFS");
}

/// Criterion 10: attaching a namespace to an already-enabled VF makes it
/// visible (Identify CNS 0x02) without a VF reset, and raises an Attached
/// Namespace Attribute Changed AEN when an AER is outstanding.
#[async_test]
async fn test_sriov_vf_namespace_hotplug(driver: DefaultDriver) {
    let mut h = setup_pf_with_offline_vf(driver.clone()).await;

    // Bring the VF online and enable it with no namespaces attached.
    assert_eq!(
        h.set_secondary_online(true).await.status.status(),
        spec::Status::SUCCESS.0
    );
    assert!(h.enable_vf_controller().await, "VF should reach RDY");

    // No namespaces are visible yet.
    assert!(
        h.vf_active_namespaces().await.is_empty(),
        "VF should have no namespaces before attach"
    );

    // Submit an Async Event Request on the VF — it stays outstanding until an
    // event occurs.
    let mut aer = spec::Command::new_zeroed();
    aer.cdw0
        .set_opcode(spec::AdminOpcode::ASYNCHRONOUS_EVENT_REQUEST.0);
    aer.cdw0.set_cid(50);
    h.vf_submit(&aer);

    // Attach namespace 1 to the already-enabled VF. This produces both the PF
    // command completion (0x1111) and the routed VF AEN (0x3333), in either
    // order.
    let attach_cmd = h.namespace_attachment_command(1, true);
    let slot = h.pf_submit(&attach_cmd);
    h.wait_for_two_msi(0x1111, 0x3333).await;
    assert_eq!(
        h.pf_consume(slot).status.status(),
        spec::Status::SUCCESS.0,
        "namespace attach failed"
    );

    // The outstanding AER must have completed with a namespace-attribute-
    // changed notice.
    let cqe = read_completion_from_queue(&h.gm, &h.vf_acq, h.vf_cq_head);
    h.vf_cq_head += 1;
    h.c.mmio_write(h.vf_bar0_base + 0x1004, (h.vf_cq_head as u32).as_bytes())
        .unwrap();
    assert_eq!(cqe.cid, 50, "AER completion expected");
    let dw0 = spec::AsynchronousEventRequestDw0::from(cqe.dw0);
    assert_eq!(
        dw0.event_type(),
        spec::AsynchronousEventType::NOTICE.0,
        "AEN should be a NOTICE"
    );
    assert_eq!(
        dw0.information(),
        spec::AsynchronousEventInformationNotice::NAMESPACE_ATTRIBUTE_CHANGED.0,
        "AEN should report namespace attribute changed"
    );

    // The namespace is now visible on the online VF — no reset required.
    let nsids = h.vf_active_namespaces().await;
    assert_eq!(nsids, vec![1], "nsid 1 should be visible on the online VF");
}

/// Criterion 11: detaching a namespace removes it from the online VF.
#[async_test]
async fn test_sriov_vf_namespace_detach(driver: DefaultDriver) {
    let mut h = setup_pf_with_offline_vf(driver.clone()).await;

    assert_eq!(
        h.set_secondary_online(true).await.status.status(),
        spec::Status::SUCCESS.0
    );
    assert!(h.enable_vf_controller().await, "VF should reach RDY");

    // Attach, confirm visible.
    assert_eq!(
        h.namespace_attachment(1, true).await.status.status(),
        spec::Status::SUCCESS.0
    );
    assert_eq!(h.vf_active_namespaces().await, vec![1]);

    // Detach, confirm gone — without a VF reset.
    assert_eq!(
        h.namespace_attachment(1, false).await.status.status(),
        spec::Status::SUCCESS.0,
        "namespace detach failed"
    );
    assert!(
        h.vf_active_namespaces().await.is_empty(),
        "nsid 1 should be removed from the online VF"
    );

    // Detaching again must report the namespace is not attached.
    assert_eq!(
        h.namespace_attachment(1, false).await.status.status(),
        spec::Status::NAMESPACE_NOT_ATTACHED.0,
        "detaching an unattached namespace should fail"
    );
}

/// A namespace attached to a secondary controller survives an SR-IOV VF
/// disable + re-enable cycle.
///
/// Per NVMe Base 2.1, Namespace Attachment operations are persistent across
/// resets and across Virtualization Management commands that set a secondary
/// offline. Clearing VF Enable implicitly transitions the secondary offline
/// (§8.2.6.3) but must not drop its attachments. Because the secondary
/// controller is persistent (it is not destroyed on VF disable), the namespace
/// is still present — and the PF still records it as attached — after the VF is
/// brought back.
#[async_test]
async fn test_sriov_vf_disable_preserves_namespace(driver: DefaultDriver) {
    let mut h = setup_pf_with_offline_vf(driver.clone()).await;

    // Online the secondary, enable it, attach namespace 1, confirm visible.
    assert_eq!(
        h.set_secondary_online(true).await.status.status(),
        spec::Status::SUCCESS.0
    );
    assert!(h.enable_vf_controller().await, "VF should reach RDY");
    assert_eq!(
        h.namespace_attachment(1, true).await.status.status(),
        spec::Status::SUCCESS.0
    );
    assert_eq!(h.vf_active_namespaces().await, vec![1]);

    // Clear VF Enable. This deactivates the VF and forces the secondary
    // offline, but the secondary controller (and its namespace attachment)
    // persists. Drive the deferred drain to completion via stop().
    let _ = set_vf_enable(&mut h.c, false);
    h.c.stop().await;

    // The VF config space is now absent.
    assert_eq!(vf_cfg_read(&mut h.c, 1, 0), 0xFFFFFFFF);

    // Re-enable VFs. The VF BAR mapping is recomputed from the still-programmed
    // VF BAR registers, so no BAR reprogramming is needed.
    set_vf_enable(&mut h.c, true).unwrap();

    // The secondary is offline after the implicit transition; bring it back
    // online and re-enable its controller — crucially, without re-attaching the
    // namespace.
    assert_eq!(
        h.set_secondary_online(true).await.status.status(),
        spec::Status::SUCCESS.0
    );
    assert!(
        h.enable_vf_controller().await,
        "VF should reach RDY after re-enable"
    );

    // The namespace is still attached and active on the secondary, with no
    // re-attach required.
    assert_eq!(
        h.vf_active_namespaces().await,
        vec![1],
        "namespace attachment must persist across VF disable/re-enable"
    );

    // Drain any interrupts left over from the re-enable/online sequence so the
    // following PF command observes only its own completion.
    while h.int_controller.get_next_interrupt().is_some() {}

    // The PF still records the namespace as attached to the secondary: a
    // re-attach reports Already Attached rather than succeeding.
    assert_eq!(
        h.namespace_attachment(1, true).await.status.status(),
        spec::Status::NAMESPACE_ALREADY_ATTACHED.0,
        "PF must still record the namespace as attached to the secondary"
    );
}

/// A full device reset restores the configured namespace topology: a namespace
/// the guest moved to a secondary controller is returned to the PF.
///
/// The controller models no non-volatile configuration storage, so a
/// subsystem-wide reset returns to the host-configured layout (all namespaces
/// on the PF) rather than preserving guest-initiated VF attachments. This
/// contrasts with a VF disable, across which attachments persist.
#[async_test]
async fn test_sriov_device_reset_restores_topology(driver: DefaultDriver) {
    let mut h = setup_pf_with_offline_vf(driver.clone()).await;

    // Online + enable the VF and attach namespace 1 to it.
    assert_eq!(
        h.set_secondary_online(true).await.status.status(),
        spec::Status::SUCCESS.0
    );
    assert!(h.enable_vf_controller().await, "VF should reach RDY");
    assert_eq!(
        h.namespace_attachment(1, true).await.status.status(),
        spec::Status::SUCCESS.0
    );
    assert_eq!(h.vf_active_namespaces().await, vec![1]);

    // Setup detached ns1 from the PF, so before reset it is on the VF only.
    assert!(
        h.pf_namespace_list(spec::Cns::ACTIVE_NAMESPACES)
            .await
            .is_empty(),
        "ns1 should be on the VF, not the PF, before reset"
    );

    // Full device reset, then re-enable the PF.
    h.c.reset().await;
    h.reenable_pf().await;

    // Drain any interrupts left over from the pre-reset sequence so the PF
    // admin commands below observe only their own completions.
    while h.int_controller.get_next_interrupt().is_some() {}

    // The namespace is back on the PF (configured topology restored), and still
    // allocated in the subsystem.
    assert_eq!(
        h.pf_namespace_list(spec::Cns::ACTIVE_NAMESPACES).await,
        vec![1],
        "device reset should restore the namespace to the PF"
    );
    assert_eq!(
        h.pf_namespace_list(spec::Cns::ALLOCATED_NAMESPACE_LIST)
            .await,
        vec![1],
        "the namespace should remain allocated in the subsystem"
    );
}

/// A private namespace can be attached to only one controller at a time:
/// attaching it to a second controller (here the PF) while it is attached to
/// the VF must fail with Namespace Is Private, and re-attaching to the same
/// controller must fail with Namespace Already Attached.
#[async_test]
async fn test_sriov_namespace_private_enforcement(driver: DefaultDriver) {
    let mut h = setup_pf_with_offline_vf(driver.clone()).await;

    assert_eq!(
        h.set_secondary_online(true).await.status.status(),
        spec::Status::SUCCESS.0
    );
    assert!(h.enable_vf_controller().await, "VF should reach RDY");

    // Attach namespace 1 to the VF.
    assert_eq!(
        h.namespace_attachment(1, true).await.status.status(),
        spec::Status::SUCCESS.0
    );

    // Re-attaching to the same controller reports Already Attached.
    assert_eq!(
        h.namespace_attachment(1, true).await.status.status(),
        spec::Status::NAMESPACE_ALREADY_ATTACHED.0,
        "re-attach to the same controller should report already attached"
    );

    // Attaching to the PF while attached to the VF reports Is Private.
    let attach_pf = h.attachment_command_to(1, PF_CNTLID, true);
    assert_eq!(
        h.pf_command(&attach_pf).await.status.status(),
        spec::Status::NAMESPACE_IS_PRIVATE.0,
        "attaching a privately-held namespace elsewhere should report is private"
    );
}

/// The PF's allocated namespace list (Identify CNS 0x10) reports a namespace
/// for as long as it is allocated in the subsystem, independent of which
/// controller (if any) it is attached to, while the PF's active list
/// (CNS 0x02) reflects only namespaces attached to the PF.
#[async_test]
async fn test_sriov_pf_allocated_namespace_list(driver: DefaultDriver) {
    let mut h = setup_pf_with_offline_vf(driver.clone()).await;

    // After setup, namespace 1 is allocated but detached from the PF.
    assert_eq!(
        h.pf_namespace_list(spec::Cns::ALLOCATED_NAMESPACE_LIST)
            .await,
        vec![1],
        "allocated list should contain the detached namespace"
    );
    assert!(
        h.pf_namespace_list(spec::Cns::ACTIVE_NAMESPACES)
            .await
            .is_empty(),
        "active list should be empty after detaching from the PF"
    );

    // Attaching to the VF does not change the PF's allocated list, and the
    // namespace remains inactive on the PF.
    assert_eq!(
        h.set_secondary_online(true).await.status.status(),
        spec::Status::SUCCESS.0
    );
    assert!(h.enable_vf_controller().await, "VF should reach RDY");
    assert_eq!(
        h.namespace_attachment(1, true).await.status.status(),
        spec::Status::SUCCESS.0
    );
    assert_eq!(
        h.pf_namespace_list(spec::Cns::ALLOCATED_NAMESPACE_LIST)
            .await,
        vec![1],
        "allocated list should still contain the VF-attached namespace"
    );
    assert!(
        h.pf_namespace_list(spec::Cns::ACTIVE_NAMESPACES)
            .await
            .is_empty(),
        "namespace attached to the VF is not active on the PF"
    );
}
