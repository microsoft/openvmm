// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::test_helpers::TestNvmeMmioRegistration;
use crate::BAR0_LEN;
use crate::NvmeController;
use crate::NvmeControllerCaps;
use crate::PAGE_SIZE64;
use crate::prp::PrpRange;
use crate::spec;
use crate::spec::nvm;
use crate::tests::test_helpers::read_completion_from_queue;
use crate::tests::test_helpers::test_memory;
use crate::tests::test_helpers::write_command_to_queue;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigSpace;
use disklayer_ram::ram_disk;
use guestmem::GuestMemory;
use guid::Guid;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pci_core::bus_range::AssignedBusRange;
use pci_core::dma::DmaTarget;
use pci_core::msi::MsiConnection;
use pci_core::test_helpers::TestPciInterruptController;
use user_driver::backoff::Backoff;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

fn instantiate_controller(
    driver: DefaultDriver,
    gm: &GuestMemory,
    int_controller: Option<&TestPciInterruptController>,
) -> NvmeController {
    let mut mmio_reg = TestNvmeMmioRegistration {};
    let vm_task_driver = &VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let msi_conn = MsiConnection::new();
    let dma_target = DmaTarget::new(AssignedBusRange::new(), 0, gm.clone(), &msi_conn);
    let controller = NvmeController::new(
        vm_task_driver,
        &dma_target,
        &mut mmio_reg,
        NvmeControllerCaps {
            msix_count: 64,
            max_io_queues: 64,
            subsystem_id: Guid::new_random(),
        },
    );

    if let Some(intc) = int_controller {
        msi_conn.connect(intc.signal_msi());
    }
    controller
}

fn write_msix_table_entry(
    controller: &mut NvmeController,
    table_index: u16,
    address: u64,
    data: u32,
    masked: bool,
) {
    // This code works by writing to MMIO space, as if all the BARs are squished together.
    // The first BAR is of length DOORBELLS.end.  The MSI-X table comes after that.
    let mmio_address = BAR0_LEN + (table_index as u64 * 16);
    let mut data_control = data as u64;
    if masked {
        data_control |= 1u64 << 32;
    }
    controller
        .mmio_write(mmio_address, address.as_bytes())
        .unwrap();
    controller
        .mmio_write(mmio_address + 8, data_control.as_bytes())
        .unwrap();
}

pub async fn wait_for_msi(
    driver: DefaultDriver,
    intc: &TestPciInterruptController,
    timeout_in_milliseconds: u32,
    expected_address: u64,
    expected_data: u32,
) {
    let wait_periods = timeout_in_milliseconds / 10;
    let mut backoff = Backoff::new(&driver);

    for _i in 0..wait_periods {
        let int = intc.get_next_interrupt();
        if let Some(int_inner) = int {
            assert_eq!(int_inner.0, expected_address);
            assert_eq!(int_inner.1, expected_data);
            return;
        }

        backoff.back_off().await;
    }

    // Should never drop off the end, here.
    panic!();
}

pub async fn instantiate_and_build_admin_queue(
    acq_buffer: &PrpRange,
    acq_entries: u32,
    asq_buffer: &PrpRange,
    asq_entries: u32,
    trigger_interrupt: bool,
    int_controller: Option<&TestPciInterruptController>,
    driver: DefaultDriver,
    gm: &GuestMemory,
) -> NvmeController {
    let mut nvmec = instantiate_controller(driver.clone(), gm, int_controller);
    // Set the BARs.
    nvmec
        .pci_cfg_write(0x10, ByteEnabledDwordWrite::with_all_bytes_enabled(0))
        .unwrap();
    nvmec
        .pci_cfg_write(
            0x20,
            ByteEnabledDwordWrite::with_all_bytes_enabled(BAR0_LEN as u32),
        )
        .unwrap();

    // Find the MSI-X cap struct.
    let mut cfg_dword = 0;
    nvmec
        .pci_cfg_read(
            0x34,
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut cfg_dword),
        )
        .unwrap();
    cfg_dword &= 0xff;
    loop {
        // Read a cap struct header and pull out the fields.
        let mut cap_header = 0;
        nvmec
            .pci_cfg_read(
                cfg_dword as u16,
                ByteEnabledDwordRead::with_all_bytes_enabled(&mut cap_header),
            )
            .unwrap();
        if cap_header & 0xff == 0x11 {
            // Read the table BIR and offset.
            let mut table_loc = 0;
            nvmec
                .pci_cfg_read(
                    cfg_dword as u16 + 4,
                    ByteEnabledDwordRead::with_all_bytes_enabled(&mut table_loc),
                )
                .unwrap();
            // Code in other places assumes that the MSI-X table is at the beginning
            // of BAR 4.  If this becomes a fluid concept, capture the values
            // here and use them, rather than just asserting on them.
            assert_eq!(table_loc & 0x7, 4);
            assert_eq!(table_loc >> 3, 0);

            // Found MSI-X, enable it.
            nvmec
                .pci_cfg_write(
                    cfg_dword as u16,
                    ByteEnabledDwordWrite::with_all_bytes_enabled(0x80000000),
                )
                .unwrap();
            break;
        }
        // Isolate the ptr to the next cap struct.
        cfg_dword = (cap_header >> 8) & 0xff;
        if cfg_dword == 0 {
            // Hit the end.
            panic!();
        }
    }

    // Turn on MMIO access by writing to the Command register in config space.  Enable
    // MMIO and DMA.
    nvmec
        .pci_cfg_write(4, ByteEnabledDwordWrite::with_all_bytes_enabled(6))
        .unwrap();

    // Set the ACQ base.
    let base = acq_buffer.range().gpns()[0] * PAGE_SIZE64;
    nvmec.write_bar0(0x30, base.as_bytes()).unwrap();

    // Set ASQ base.
    let base = asq_buffer.range().gpns()[0] * PAGE_SIZE64;
    nvmec.write_bar0(0x28, base.as_bytes()).unwrap();

    // Set AQA.
    let aqa: u32 = (asq_entries - 1) | ((acq_entries - 1) << 16);
    nvmec.write_bar0(0x24, aqa.as_bytes()).unwrap();

    // Set MSI-X table entry for the admin queue.
    write_msix_table_entry(&mut nvmec, 0, 0xfeed0000, 0x1111, !trigger_interrupt);

    let mut backoff = Backoff::new(&driver);

    // Enable the controller.
    let mut dword = 0u32;
    nvmec.read_bar0(0x14, dword.as_mut_bytes()).unwrap();
    dword |= 1;
    nvmec.write_bar0(0x14, dword.as_bytes()).unwrap();
    backoff.back_off().await;
    nvmec.read_bar0(0x14, dword.as_mut_bytes()).unwrap();
    assert!(dword & 1 != 0);

    // Read CSTS
    let mut ready = false;
    for _i in 0..5 {
        nvmec.read_bar0(0x1c, dword.as_mut_bytes()).unwrap();
        let csts = spec::Csts::from(dword);
        assert_eq!(csts.cfs(), false);
        if csts.rdy() {
            ready = true;
            break;
        }
        backoff.back_off().await;
    }
    assert!(ready);
    nvmec
}

#[async_test]
async fn test_basic_registers(driver: DefaultDriver) {
    let gm = test_memory();
    let mut nvmec = instantiate_controller(driver, &gm, None);
    let mut dword = 0u32;

    // Read controller caps, version.
    nvmec.read_bar0(0, dword.as_mut_bytes()).unwrap();
    assert_eq!(dword, 0xFF0100FF);
    let mut qword = 0u64;
    nvmec.read_bar0(0, qword.as_mut_bytes()).unwrap();
    assert_eq!(qword, 0x20FF0100FF);
    nvmec.read_bar0(8, dword.as_mut_bytes()).unwrap();
    assert_eq!(dword, 0x20000);

    // Read ACQ and write it back, see that it sticks.
    nvmec.read_bar0(0x30, qword.as_mut_bytes()).unwrap();
    assert_eq!(qword, 0);
    qword = 0x1000;
    nvmec.write_bar0(0x30, qword.as_bytes()).unwrap();
    nvmec.read_bar0(0x30, qword.as_mut_bytes()).unwrap();
    assert_eq!(qword, 0x1000);
}

#[async_test]
async fn test_invalid_configuration(driver: DefaultDriver) {
    let gm = test_memory();
    let mut nvmec = instantiate_controller(driver, &gm, None);
    let mut dword = 0u32;
    nvmec.read_bar0(0x14, dword.as_mut_bytes()).unwrap();
    // Set MPS to some disallowed value
    dword |= 0x380;
    nvmec.write_bar0(0x14, dword.as_bytes()).unwrap();
    // Read CSTS, expect fatal error
    nvmec.read_bar0(0x1c, dword.as_mut_bytes()).unwrap();
    assert!(dword & 2 != 0);
}

#[async_test]
async fn test_enable_controller(driver: DefaultDriver) {
    let gm = test_memory();
    let mut nvmec = instantiate_controller(driver, &gm, None);

    // Set the ACQ base to 0x1000 and the ASQ base to 0x2000.
    let mut qword = 0x1000;
    nvmec.write_bar0(0x30, qword.as_bytes()).unwrap();
    qword = 0x2000;
    nvmec.write_bar0(0x28, qword.as_bytes()).unwrap();

    // Set the queues so that they have four entries apiece.
    let mut dword = 0x30003;
    nvmec.write_bar0(0x24, dword.as_bytes()).unwrap();

    // Enable the controller.
    nvmec.read_bar0(0x14, dword.as_mut_bytes()).unwrap();
    dword |= 1;
    nvmec.write_bar0(0x14, dword.as_bytes()).unwrap();
    nvmec.read_bar0(0x14, dword.as_mut_bytes()).unwrap();
    assert!(dword & 1 != 0);

    // Read CSTS
    nvmec.read_bar0(0x1c, dword.as_mut_bytes()).unwrap();
    assert!(dword & 2 == 0);
}

#[async_test]
async fn test_multi_page_admin_queues(driver: DefaultDriver) {
    let gm = test_memory();
    let mut nvmec = instantiate_controller(driver, &gm, None);

    // Set the ACQ base to 0x1000 and the ASQ base to 0x3000.
    let mut qword = 0x1000;
    nvmec.write_bar0(0x30, qword.as_bytes()).unwrap();
    qword = 0x3000;
    nvmec.write_bar0(0x28, qword.as_bytes()).unwrap();

    // Set the queues so that they have 512 entries apiece.
    let mut dword = 0x1ff01ff;
    nvmec.write_bar0(0x24, dword.as_bytes()).unwrap();

    // Enable the controller.
    nvmec.read_bar0(0x14, dword.as_mut_bytes()).unwrap();
    dword |= 1;
    nvmec.write_bar0(0x14, dword.as_bytes()).unwrap();
    nvmec.read_bar0(0x14, dword.as_mut_bytes()).unwrap();
    assert!(dword & 1 != 0);

    // Read CSTS
    nvmec.read_bar0(0x1c, dword.as_mut_bytes()).unwrap();
    assert!(dword & 2 == 0);
}

#[async_test]
async fn test_send_identify(driver: DefaultDriver) {
    let dm1 = PrpRange::new(vec![0], 0, PAGE_SIZE64).unwrap();
    let dm2 = PrpRange::new(vec![0x1000], 0, PAGE_SIZE64).unwrap();
    let gm = test_memory();
    let int_controller = TestPciInterruptController::new();

    // Build a controller with 64 entries in the admin queue (just so that the ASQ fits in one page).
    let mut nvmec = instantiate_and_build_admin_queue(
        &dm1,
        64,
        &dm2,
        64,
        true,
        Some(&int_controller),
        driver.clone(),
        &gm,
    )
    .await;

    // There should be no MSI-X triggered at this point.
    let next_int = int_controller.get_next_interrupt();
    assert!(next_int.is_none());

    // Construct an admin queue command into the first entry in the ASQ, which is at 0x1000 in the "test memory".
    let mut entry = spec::Command::new_zeroed();
    entry.cdw0.set_opcode(spec::AdminOpcode::IDENTIFY.0);
    let cdw10 = spec::Cdw10Identify::new().with_cns(spec::Cns::CONTROLLER.0);
    entry.cdw10 = u32::from(cdw10);
    entry.dptr[0] = 1;

    write_command_to_queue(&gm, &dm2, 0, &entry);

    // Ring the admin queue doorbell.
    nvmec.write_bar0(0x1000, 1u32.as_bytes()).unwrap();

    wait_for_msi(driver.clone(), &int_controller, 1000, 0xfeed0000, 0x1111).await;

    let cqe = read_completion_from_queue(&gm, &dm1, 0);
    assert_eq!(cqe.status.status(), spec::Status::SUCCESS.0);
}

// =============================================================================
// Regression tests for the I/O worker `io_count` accounting.
//
// Each I/O queue worker (one per I/O CQ) tracks `sq.io_count` — the number of
// dispatched I/Os that have not yet been credited back. Every dispatched I/O
// must increment exactly once and decrement exactly once over its lifetime,
// regardless of whether the completion is posted directly to the CQ or first
// queued in `state.completions` (because the CQ was full) and posted later.
// Two failure modes have historically lived in this code:
//
//   * Inline completions for commands targeting an invalid namespace are
//     synthesized without incrementing `io_count`. If the decrement path
//     decrements anyway, `io_count` underflows and panics.
//
//   * Dispatched I/Os that hit a full CQ get re-queued in `state.completions`
//     and drained later as `Event::CompletionReady`. If the decrement is
//     skipped for that drain path, `io_count` leaks; after enough leaks it
//     exceeds `MAX_IO_QUEUE_DEPTH` and the SQ is permanently throttled.
//
// The tests below exercise both shapes through the real `NvmeController`.

/// Helper: create an I/O completion queue and an I/O submission queue bound
/// to it via admin commands, draining the admin completions as it goes.
/// Returns the next free admin slot.
#[expect(clippy::too_many_arguments)]
async fn create_io_queue_pair(
    nvmec: &mut NvmeController,
    gm: &GuestMemory,
    admin_cq_buf: &PrpRange,
    admin_sq_buf: &PrpRange,
    int_controller: &TestPciInterruptController,
    driver: DefaultDriver,
    starting_admin_slot: u32,
    qid: u16,
    cq_gpa: u64,
    sq_gpa: u64,
    cq_qsize_z: u16,
    sq_qsize_z: u16,
    cq_iv: u16,
) -> u32 {
    let mut admin_slot = starting_admin_slot;

    // CREATE_IO_COMPLETION_QUEUE
    let mut command = spec::Command::new_zeroed();
    command
        .cdw0
        .set_opcode(spec::AdminOpcode::CREATE_IO_COMPLETION_QUEUE.0);
    command.cdw10 = spec::Cdw10CreateIoQueue::new()
        .with_qid(qid)
        .with_qsize_z(cq_qsize_z)
        .into();
    command.cdw11 = spec::Cdw11CreateIoCompletionQueue::new()
        .with_pc(true)
        .with_ien(true)
        .with_iv(cq_iv)
        .into();
    command.dptr[0] = cq_gpa;
    command.cdw0.set_cid(0xc100 + qid);

    write_command_to_queue(gm, admin_sq_buf, admin_slot as usize, &command);
    nvmec
        .write_bar0(0x1000, (admin_slot + 1).as_bytes())
        .unwrap();
    wait_for_msi(driver.clone(), int_controller, 1000, 0xfeed0000, 0x1111).await;
    let cqe = read_completion_from_queue(gm, admin_cq_buf, admin_slot as usize);
    assert_eq!(cqe.status.status(), spec::Status::SUCCESS.0);
    assert_eq!(cqe.cid, 0xc100 + qid);
    admin_slot += 1;

    // CREATE_IO_SUBMISSION_QUEUE bound to the CQ just created.
    let mut command = spec::Command::new_zeroed();
    command
        .cdw0
        .set_opcode(spec::AdminOpcode::CREATE_IO_SUBMISSION_QUEUE.0);
    command.cdw10 = spec::Cdw10CreateIoQueue::new()
        .with_qid(qid)
        .with_qsize_z(sq_qsize_z)
        .into();
    command.cdw11 = spec::Cdw11CreateIoSubmissionQueue::new()
        .with_pc(true)
        .with_qprio(0)
        .with_cqid(qid)
        .into();
    command.dptr[0] = sq_gpa;
    command.cdw0.set_cid(0xc200 + qid);

    write_command_to_queue(gm, admin_sq_buf, admin_slot as usize, &command);
    nvmec
        .write_bar0(0x1000, (admin_slot + 1).as_bytes())
        .unwrap();
    wait_for_msi(driver.clone(), int_controller, 1000, 0xfeed0000, 0x1111).await;
    let cqe = read_completion_from_queue(gm, admin_cq_buf, admin_slot as usize);
    assert_eq!(cqe.status.status(), spec::Status::SUCCESS.0);
    assert_eq!(cqe.cid, 0xc200 + qid);
    admin_slot + 1
}

/// Doorbell offset for SQ `qid`. NVMe doorbell stride is 4 bytes and the
/// admin SQ is at offset `0x1000`; SQ `qid` is at `0x1000 + (2*qid)*4`.
const fn sq_db(qid: u16) -> u64 {
    0x1000 + (2 * qid as u64) * 4
}

/// Doorbell offset for CQ `qid`. CQ `qid` is at `0x1000 + (2*qid + 1)*4`.
const fn cq_db(qid: u16) -> u64 {
    0x1000 + (2 * qid as u64 + 1) * 4
}

/// Smoke test: an I/O command targeting a non-existent namespace must
/// complete with INVALID_NAMESPACE_OR_FORMAT instead of panicking.
///
/// The original bug here was an unconditional `sq.io_count -= 1` at the
/// bottom of the I/O worker loop, which underflowed because the inline
/// invalid-namespace completion never incremented `io_count`.
#[async_test]
async fn test_io_command_invalid_namespace(driver: DefaultDriver) {
    let admin_cq_buf = PrpRange::new(vec![0], 0, PAGE_SIZE64).unwrap();
    let admin_sq_buf = PrpRange::new(vec![0x1000], 0, PAGE_SIZE64).unwrap();
    let gm = test_memory();
    let int_controller = TestPciInterruptController::new();

    let mut nvmec = instantiate_and_build_admin_queue(
        &admin_cq_buf,
        64,
        &admin_sq_buf,
        64,
        true,
        Some(&int_controller),
        driver.clone(),
        &gm,
    )
    .await;

    // Set up MSI-X for the I/O CQ (vector 1).
    write_msix_table_entry(&mut nvmec, 1, 0xfeed0000, 0x2222, false);

    let io_cq_gpa: u64 = 0x4000;
    let io_sq_gpa: u64 = 0x5000;
    let _admin_slot = create_io_queue_pair(
        &mut nvmec,
        &gm,
        &admin_cq_buf,
        &admin_sq_buf,
        &int_controller,
        driver.clone(),
        0,
        /* qid = */ 1,
        io_cq_gpa,
        io_sq_gpa,
        /* cq_qsize_z = */ 16,
        /* sq_qsize_z = */ 16,
        /* cq_iv = */ 1,
    )
    .await;

    // Send a READ to NSID=0xFFFF (no namespaces are attached, so any NSID
    // is invalid).
    let mut io_cmd = spec::Command::new_zeroed();
    io_cmd.cdw0.set_opcode(nvm::NvmOpcode::READ.0);
    io_cmd.cdw0.set_cid(42);
    io_cmd.nsid = 0xFFFF;

    let io_sq_buf = PrpRange::new(vec![io_sq_gpa], 0, PAGE_SIZE64).unwrap();
    let io_cq_buf = PrpRange::new(vec![io_cq_gpa], 0, PAGE_SIZE64).unwrap();
    write_command_to_queue(&gm, &io_sq_buf, 0, &io_cmd);
    nvmec.write_bar0(sq_db(1), 1u32.as_bytes()).unwrap();

    wait_for_msi(driver.clone(), &int_controller, 1000, 0xfeed0000, 0x2222).await;

    let cqe = read_completion_from_queue(&gm, &io_cq_buf, 0);
    assert_eq!(cqe.cid, 42);
    assert_eq!(
        cqe.status.status(),
        spec::Status::INVALID_NAMESPACE_OR_FORMAT.0,
        "command to invalid namespace should return INVALID_NAMESPACE_OR_FORMAT"
    );
}

/// Regression test: when multiple invalid-namespace inline completions are
/// queued in `state.completions` (because the CQ is full) and the SQ is then
/// deleted before they drain, `delete_sq` must not decrement `io_count` for
/// them — those completions never incremented it.
///
/// Pre-fix, `delete_sq` unconditionally decremented `io_count` for every
/// queued completion belonging to the SQ being deleted. Because inline
/// invalid-namespace completions never incremented `io_count`, this
/// underflowed (the worker had `io_count == 0`) and panicked. The panic
/// killed the admin worker task, so the DELETE_SQ completion would never
/// be posted and this test would time out waiting for its admin MSI.
#[async_test]
async fn test_invalid_namespace_queued_then_delete_sq(driver: DefaultDriver) {
    let admin_cq_buf = PrpRange::new(vec![0], 0, PAGE_SIZE64).unwrap();
    let admin_sq_buf = PrpRange::new(vec![0x1000], 0, PAGE_SIZE64).unwrap();
    let gm = test_memory();
    let int_controller = TestPciInterruptController::new();

    let mut nvmec = instantiate_and_build_admin_queue(
        &admin_cq_buf,
        64,
        &admin_sq_buf,
        64,
        true,
        Some(&int_controller),
        driver.clone(),
        &gm,
    )
    .await;

    // Set up MSI-X for the I/O CQ (vector 1).
    write_msix_table_entry(&mut nvmec, 1, 0xfeed0000, 0x2222, false);

    // Create a tiny I/O CQ (qsize_z=1 → 2 entries, only 1 usable before
    // `cq.write` returns `Ok(false)` and pushes onto `state.completions`),
    // and a comfortably-sized I/O SQ.
    let io_cq_gpa: u64 = 0x4000;
    let io_sq_gpa: u64 = 0x5000;
    let mut admin_slot = create_io_queue_pair(
        &mut nvmec,
        &gm,
        &admin_cq_buf,
        &admin_sq_buf,
        &int_controller,
        driver.clone(),
        0,
        /* qid = */ 1,
        io_cq_gpa,
        io_sq_gpa,
        /* cq_qsize_z = */ 1,
        /* sq_qsize_z = */ 16,
        /* cq_iv = */ 1,
    )
    .await;

    let io_sq_buf = PrpRange::new(vec![io_sq_gpa], 0, PAGE_SIZE64).unwrap();

    // Submit four invalid-NS READs without consuming any CQ completions.
    // The first one fills the CQ; the remaining three are queued in
    // `state.completions` with `decrement_io_count: false`.
    for i in 0..4u16 {
        let mut io_cmd = spec::Command::new_zeroed();
        io_cmd.cdw0.set_opcode(nvm::NvmOpcode::READ.0);
        io_cmd.cdw0.set_cid(100 + i);
        io_cmd.nsid = 0xFFFF; // invalid
        write_command_to_queue(&gm, &io_sq_buf, i as usize, &io_cmd);
    }
    nvmec.write_bar0(sq_db(1), 4u32.as_bytes()).unwrap();

    // Wait for the single CQ slot to be filled (the first interrupt), so we
    // know the worker has had a chance to process the SQ entries and queue
    // the rest in `state.completions`. (We deliberately do *not* bump the
    // CQ head doorbell — leaving the queued completions in place for
    // `delete_sq` to encounter.)
    wait_for_msi(driver.clone(), &int_controller, 1000, 0xfeed0000, 0x2222).await;
    // Let the worker finish queueing the rest before we issue DELETE_SQ.
    let mut backoff = Backoff::new(&driver);
    backoff.back_off().await;

    // DELETE_IO_SUBMISSION_QUEUE for our SQ. Pre-fix, `delete_sq` would
    // panic on `sq.io_count -= 1` while retaining `state.completions`
    // (io_count was zero throughout because invalid-NS never increments
    // it). The admin worker would die and we'd never see this completion.
    let mut command = spec::Command::new_zeroed();
    command
        .cdw0
        .set_opcode(spec::AdminOpcode::DELETE_IO_SUBMISSION_QUEUE.0);
    command.cdw10 = spec::Cdw10DeleteIoQueue::new().with_qid(1).into();
    command.cdw0.set_cid(50);
    write_command_to_queue(&gm, &admin_sq_buf, admin_slot as usize, &command);
    nvmec
        .write_bar0(0x1000, (admin_slot + 1).as_bytes())
        .unwrap();

    // We expect the admin completion for DELETE_SQ to come back. If the
    // worker panicked, this will time out.
    wait_for_msi(driver.clone(), &int_controller, 5000, 0xfeed0000, 0x1111).await;
    let cqe = read_completion_from_queue(&gm, &admin_cq_buf, admin_slot as usize);
    assert_eq!(
        cqe.status.status(),
        spec::Status::SUCCESS.0,
        "DELETE_SQ should complete cleanly even with inline invalid-NS \
         completions queued in state.completions"
    );
    assert_eq!(cqe.cid, 50);
    admin_slot += 1;

    // Sanity check: the controller is still healthy enough to accept
    // another admin command (a panicked admin worker would not respond).
    let mut command = spec::Command::new_zeroed();
    command.cdw0.set_opcode(spec::AdminOpcode::IDENTIFY.0);
    command.cdw10 = u32::from(spec::Cdw10Identify::new().with_cns(spec::Cns::CONTROLLER.0));
    command.dptr[0] = 0x6000;
    command.cdw0.set_cid(51);
    write_command_to_queue(&gm, &admin_sq_buf, admin_slot as usize, &command);
    nvmec
        .write_bar0(0x1000, (admin_slot + 1).as_bytes())
        .unwrap();
    wait_for_msi(driver.clone(), &int_controller, 1000, 0xfeed0000, 0x1111).await;
    let cqe = read_completion_from_queue(&gm, &admin_cq_buf, admin_slot as usize);
    assert_eq!(cqe.status.status(), spec::Status::SUCCESS.0);
    assert_eq!(cqe.cid, 51);
}

/// Regression test: dispatched I/Os whose completion hits a full CQ are
/// re-queued in `state.completions`. When drained later as
/// `Event::CompletionReady`, the dispatch-side `io_count += 1` must still
/// be balanced by a `-= 1`; otherwise `io_count` leaks and eventually
/// exceeds `MAX_IO_QUEUE_DEPTH`, permanently throttling the SQ.
///
/// Setup: a 2-entry I/O CQ (one usable slot) and a 16-entry SQ filled
/// with FLUSH commands against a ram-disk-backed namespace. With the
/// tiny CQ, dispatched completions almost immediately back up into
/// `state.completions`. With a working accounting scheme, the worker
/// drains all 16 as the test bumps the CQ head doorbell. With a broken
/// scheme, only the first ~9 dispatch and complete; the remaining SQ
/// entries are abandoned because `io_count` stays at `MAX_IO_QUEUE_DEPTH`,
/// and waiting for those completions times out.
#[async_test]
async fn test_full_cq_does_not_leak_io_count(driver: DefaultDriver) {
    let admin_cq_buf = PrpRange::new(vec![0], 0, PAGE_SIZE64).unwrap();
    let admin_sq_buf = PrpRange::new(vec![0x1000], 0, PAGE_SIZE64).unwrap();
    let gm = test_memory();
    let int_controller = TestPciInterruptController::new();

    let mut nvmec = instantiate_and_build_admin_queue(
        &admin_cq_buf,
        64,
        &admin_sq_buf,
        64,
        true,
        Some(&int_controller),
        driver.clone(),
        &gm,
    )
    .await;

    // Attach a 1 MiB ram disk as nsid=1 so FLUSHes on it actually exercise
    // the dispatched-I/O code path (rather than the inline invalid-NS
    // path). FLUSH on a ram disk is effectively a no-op, so it completes
    // quickly and predictably.
    let disk = ram_disk(1 << 20, /* read_only = */ false).unwrap();
    nvmec.client().add_namespace(1, disk).await.unwrap();

    // Set up MSI-X for the I/O CQ (vector 1).
    write_msix_table_entry(&mut nvmec, 1, 0xfeed0000, 0x2222, false);

    // Tiny CQ (qsize_z=1 → 2 entries, capacity 1), generous SQ.
    let io_cq_gpa: u64 = 0x4000;
    let io_sq_gpa: u64 = 0x5000;
    let _admin_slot = create_io_queue_pair(
        &mut nvmec,
        &gm,
        &admin_cq_buf,
        &admin_sq_buf,
        &int_controller,
        driver.clone(),
        0,
        /* qid = */ 1,
        io_cq_gpa,
        io_sq_gpa,
        /* cq_qsize_z = */ 1,
        /* sq_qsize_z = */ 31,
        /* cq_iv = */ 1,
    )
    .await;

    let io_sq_buf = PrpRange::new(vec![io_sq_gpa], 0, PAGE_SIZE64).unwrap();
    let io_cq_buf = PrpRange::new(vec![io_cq_gpa], 0, PAGE_SIZE64).unwrap();

    // Submit 16 FLUSH commands. With CQ capacity 1, the worker can only
    // post one completion directly; the rest must be queued in
    // `state.completions` and drained as we bump the CQ head doorbell.
    const N: u16 = 16;
    for i in 0..N {
        let mut io_cmd = spec::Command::new_zeroed();
        io_cmd.cdw0.set_opcode(nvm::NvmOpcode::FLUSH.0);
        io_cmd.cdw0.set_cid(200 + i);
        io_cmd.nsid = 1;
        write_command_to_queue(&gm, &io_sq_buf, i as usize, &io_cmd);
    }
    nvmec.write_bar0(sq_db(1), (N as u32).as_bytes()).unwrap();

    // Drain all N completions. Each iteration: wait for the next CQ MSI,
    // read the entry at the current CQ slot, bump the CQ head doorbell to
    // free the slot. The worker must refill the slot from
    // `state.completions` (and dispatch more SQ entries as its `io_count`
    // drops below `MAX_IO_QUEUE_DEPTH`). With a leaking `io_count`, the
    // SQ stalls and not all N will arrive.
    let mut received = std::collections::BTreeSet::new();
    for i in 0..N as usize {
        // CQ has 2 slots (qsize_z=1 → len 2), so the producer's tail
        // alternates between slots 0 and 1 as it wraps.
        let slot = i % 2;
        wait_for_msi(driver.clone(), &int_controller, 5000, 0xfeed0000, 0x2222).await;
        let cqe = read_completion_from_queue(&gm, &io_cq_buf, slot);
        assert_eq!(
            cqe.status.status(),
            spec::Status::SUCCESS.0,
            "FLUSH #{} (cid={}) returned status {:#x}",
            i,
            cqe.cid,
            cqe.status.status()
        );
        assert!(
            received.insert(cqe.cid),
            "duplicate completion for cid {}",
            cqe.cid
        );
        // Advance the head doorbell past the slot we just consumed.
        let new_head = (((i + 1) % 2) as u32).to_le();
        nvmec.write_bar0(cq_db(1), new_head.as_bytes()).unwrap();
    }

    // Every submitted command must have completed exactly once.
    let expected: std::collections::BTreeSet<u16> = (200..200 + N).collect();
    assert_eq!(
        received, expected,
        "missing FLUSH completions — likely io_count leak throttled the SQ"
    );
}

/// Regression test for the Asynchronous Event Configuration feature
/// (Set/Get Features FID 0Bh).
///
/// The NVMe Base specification lists this Feature as mandatory for I/O
/// controllers (Base 2.0c section 3.1.2.1.1 / Base 2.3 section 3.1.3.6
/// Figure 32). Initiators that strictly follow the spec may refuse to
/// allocate any Asynchronous Event Request resources when the Set
/// Features command for this Feature is rejected, which breaks
/// downstream AEN delivery (including the changed-namespace AEN that
/// drives namespace hot-add notification).
///
/// This test pins the contract: Set Features 0Bh must succeed and Get
/// Features 0Bh must return the same value the host last wrote.
#[async_test]
async fn test_set_get_features_async_event_config(driver: DefaultDriver) {
    let admin_cq_buf = PrpRange::new(vec![0], 0, PAGE_SIZE64).unwrap();
    let admin_sq_buf = PrpRange::new(vec![0x1000], 0, PAGE_SIZE64).unwrap();
    let gm = test_memory();
    let int_controller = TestPciInterruptController::new();

    let mut nvmec = instantiate_and_build_admin_queue(
        &admin_cq_buf,
        64,
        &admin_sq_buf,
        64,
        true,
        Some(&int_controller),
        driver.clone(),
        &gm,
    )
    .await;

    // A non-trivial mask that exercises bits in both the low SMART/Health
    // byte and the higher notice classes (including bit 8 — Attached
    // Namespace Attribute Notices, which is the bit that gates the
    // namespace-change AEN). Also flips a high opaque bit (bit 21, Lost
    // Host Communication Notices) to prove the round-trip is byte-exact
    // rather than masked to a subset.
    const MASK: u32 = 0x0020_011F;

    // ----- Set Features 0Bh -----
    let mut cmd = spec::Command::new_zeroed();
    cmd.cdw0.set_opcode(spec::AdminOpcode::SET_FEATURES.0);
    cmd.cdw0.set_cid(601);
    cmd.cdw10 =
        u32::from(spec::Cdw10SetFeatures::new().with_fid(spec::Feature::ASYNC_EVENT_CONFIG.0));
    cmd.cdw11 = MASK;
    write_command_to_queue(&gm, &admin_sq_buf, 0, &cmd);
    nvmec.write_bar0(0x1000, 1u32.as_bytes()).unwrap();

    wait_for_msi(driver.clone(), &int_controller, 1000, 0xfeed0000, 0x1111).await;
    let set_cqe = read_completion_from_queue(&gm, &admin_cq_buf, 0);
    assert_eq!(set_cqe.cid, 601);
    assert_eq!(
        set_cqe.status.status(),
        spec::Status::SUCCESS.0,
        "Set Features 0Bh must succeed (mandatory for I/O controllers)"
    );

    // Advance the admin CQ head past slot 0 so the worker can post the
    // next completion into slot 1.
    nvmec.write_bar0(cq_db(0), 1u32.as_bytes()).unwrap();

    // ----- Get Features 0Bh -----
    let mut cmd = spec::Command::new_zeroed();
    cmd.cdw0.set_opcode(spec::AdminOpcode::GET_FEATURES.0);
    cmd.cdw0.set_cid(602);
    cmd.cdw10 =
        u32::from(spec::Cdw10GetFeatures::new().with_fid(spec::Feature::ASYNC_EVENT_CONFIG.0));
    write_command_to_queue(&gm, &admin_sq_buf, 1, &cmd);
    nvmec.write_bar0(0x1000, 2u32.as_bytes()).unwrap();

    wait_for_msi(driver.clone(), &int_controller, 1000, 0xfeed0000, 0x1111).await;
    let get_cqe = read_completion_from_queue(&gm, &admin_cq_buf, 1);
    assert_eq!(get_cqe.cid, 602);
    assert_eq!(get_cqe.status.status(), spec::Status::SUCCESS.0);
    assert_eq!(
        get_cqe.dw0, MASK,
        "Get Features 0Bh must echo back the previously configured mask byte-exactly"
    );
}

/// Regression test for Asynchronous Event Configuration mask
/// enforcement.
///
/// The Asynchronous Event Configuration feature (FID 0Bh) controls
/// which classes of asynchronous event notification the controller
/// is allowed to fire (NVMe Base 2.0c section 5.21.1.11 / Base 2.3
/// section 5.2.26.1.5). Bit 8 ("Attached Namespace Attribute Notices")
/// gates the changed-namespace AEN; per spec, "If this bit is cleared
/// to '0', then the controller shall not send the Attached Namespace
/// Attribute Changed asynchronous event to the host."
///
/// This test exercises both the negative path (mask cleared suppresses
/// the queued namespace-change AEN) and the positive path (toggling
/// the mask back on causes the previously-suppressed event to be
/// delivered without needing a fresh trigger). It also confirms the
/// emitted completion encodes the correct event type and log page
/// identifier so a host that reads it knows to fetch the
/// CHANGED_NAMESPACE_LIST log page.
#[async_test]
async fn test_async_event_config_masks_namespace_aen(driver: DefaultDriver) {
    let admin_cq_buf = PrpRange::new(vec![0], 0, PAGE_SIZE64).unwrap();
    let admin_sq_buf = PrpRange::new(vec![0x1000], 0, PAGE_SIZE64).unwrap();
    let gm = test_memory();
    let int_controller = TestPciInterruptController::new();

    let mut nvmec = instantiate_and_build_admin_queue(
        &admin_cq_buf,
        64,
        &admin_sq_buf,
        64,
        true,
        Some(&int_controller),
        driver.clone(),
        &gm,
    )
    .await;

    // -------------------------------------------------------------
    // Step 1: Set the AEC mask to 0 (every notification class
    // disabled, including bit 8 / Attached Namespace Attribute
    // Notices).
    // -------------------------------------------------------------
    let mut cmd = spec::Command::new_zeroed();
    cmd.cdw0.set_opcode(spec::AdminOpcode::SET_FEATURES.0);
    cmd.cdw0.set_cid(701);
    cmd.cdw10 =
        u32::from(spec::Cdw10SetFeatures::new().with_fid(spec::Feature::ASYNC_EVENT_CONFIG.0));
    cmd.cdw11 = 0;
    write_command_to_queue(&gm, &admin_sq_buf, 0, &cmd);
    nvmec.write_bar0(0x1000, 1u32.as_bytes()).unwrap();

    wait_for_msi(driver.clone(), &int_controller, 1000, 0xfeed0000, 0x1111).await;
    let cqe = read_completion_from_queue(&gm, &admin_cq_buf, 0);
    assert_eq!(cqe.cid, 701);
    assert_eq!(cqe.status.status(), spec::Status::SUCCESS.0);
    nvmec.write_bar0(cq_db(0), 1u32.as_bytes()).unwrap();

    // -------------------------------------------------------------
    // Step 2: Park an Asynchronous Event Request. With no AERs in
    // flight, the controller has nothing to complete an AEN against
    // even if the mask permitted firing, so this is required to make
    // the negative assertion meaningful.
    // -------------------------------------------------------------
    let mut cmd = spec::Command::new_zeroed();
    cmd.cdw0
        .set_opcode(spec::AdminOpcode::ASYNCHRONOUS_EVENT_REQUEST.0);
    cmd.cdw0.set_cid(702);
    write_command_to_queue(&gm, &admin_sq_buf, 1, &cmd);
    nvmec.write_bar0(0x1000, 2u32.as_bytes()).unwrap();
    // No MSI is expected at this point - the AER simply parks.

    // -------------------------------------------------------------
    // Step 3: Trigger a namespace change. With the mask cleared the
    // controller MUST NOT fire the corresponding AEN.
    // -------------------------------------------------------------
    let disk = ram_disk(1 << 20, /* read_only = */ false).unwrap();
    nvmec.client().add_namespace(2, disk).await.unwrap();

    // Wait ~500ms watching for any MSI. None should fire.
    let mut backoff = Backoff::new(&driver);
    for _ in 0..50 {
        if let Some(int) = int_controller.get_next_interrupt() {
            panic!(
                "unexpected AEN MSI while AEC mask=0: addr={:#x} data={:#x}",
                int.0, int.1
            );
        }
        backoff.back_off().await;
    }

    // -------------------------------------------------------------
    // Step 4: Toggle the mask back on (just bit 8 / NAN). The
    // namespace change from step 3 is still queued in
    // state.changed_namespaces; the parked AER is still outstanding.
    // The next loop iteration after the Set Features completes must
    // observe the now-enabled mask and fire the AEN.
    // -------------------------------------------------------------
    let mask_on =
        u32::from(spec::Cdw11FeatureAsyncEventConfig::new().with_namespace_attribute_notices(true));
    let mut cmd = spec::Command::new_zeroed();
    cmd.cdw0.set_opcode(spec::AdminOpcode::SET_FEATURES.0);
    cmd.cdw0.set_cid(703);
    cmd.cdw10 =
        u32::from(spec::Cdw10SetFeatures::new().with_fid(spec::Feature::ASYNC_EVENT_CONFIG.0));
    cmd.cdw11 = mask_on;
    write_command_to_queue(&gm, &admin_sq_buf, 2, &cmd);
    nvmec.write_bar0(0x1000, 3u32.as_bytes()).unwrap();

    // Set Features completion arrives in admin CQ slot 1.
    wait_for_msi(driver.clone(), &int_controller, 1000, 0xfeed0000, 0x1111).await;
    let set_cqe = read_completion_from_queue(&gm, &admin_cq_buf, 1);
    assert_eq!(set_cqe.cid, 703);
    assert_eq!(set_cqe.status.status(), spec::Status::SUCCESS.0);
    nvmec.write_bar0(cq_db(0), 2u32.as_bytes()).unwrap();

    // The AEN completion (against the AER parked in step 2) arrives in
    // admin CQ slot 2.
    wait_for_msi(driver.clone(), &int_controller, 1000, 0xfeed0000, 0x1111).await;
    let aen_cqe = read_completion_from_queue(&gm, &admin_cq_buf, 2);
    assert_eq!(
        aen_cqe.cid, 702,
        "AEN completion must be posted against the parked AER's cid"
    );
    assert_eq!(aen_cqe.status.status(), spec::Status::SUCCESS.0);

    let dw0 = spec::AsynchronousEventRequestDw0::from(aen_cqe.dw0);
    assert_eq!(
        dw0.event_type(),
        spec::AsynchronousEventType::NOTICE.0,
        "AEN event_type must be NOTICE"
    );
    assert_eq!(
        dw0.log_page_identifier(),
        spec::LogPageIdentifier::CHANGED_NAMESPACE_LIST.0,
        "AEN log_page_identifier must point at CHANGED_NAMESPACE_LIST"
    );
}
