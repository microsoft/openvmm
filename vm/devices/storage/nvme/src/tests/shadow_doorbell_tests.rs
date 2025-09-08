// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::PAGE_SIZE64;
use crate::prp::PrpRange;
use crate::spec;
use crate::tests::controller_tests::instantiate_and_build_admin_queue;
use crate::tests::controller_tests::wait_for_msi;
use crate::tests::test_helpers::read_completion_from_queue;
use crate::tests::test_helpers::test_memory;
use crate::tests::test_helpers::write_command_to_queue;
use guestmem::GuestMemory;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pci_core::test_helpers::TestPciInterruptController;
use user_driver::backoff::Backoff;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const CQ_BASE: u64 = 0x0;
const SQ_BASE: u64 = 0x1000;
const DOORBELL_BUFFER_BASE: u64 = 0x2000;
const EVT_IDX_BUFFER_BASE: u64 = 0x3000;
const DCQ_BASE: u64 = 0x4000;
const DSQ_BASE: u64 = 0x5000;

/// Sets up an admin queue with a single command to configure the shadow doorbells.  Leaves the
/// admin queue tail at 1, if create_dq_pair is false, 3 otherwise.
async fn setup_shadow_doorbells(
    driver: DefaultDriver,
    cq_buf: &PrpRange,
    sq_buf: &PrpRange,
    gm: &GuestMemory,
    int_controller: &TestPciInterruptController,
    dq_bases: Option<(u64, u64)>,
) -> crate::NvmeController {
    // Build a controller with 64 entries in the admin queue (just so that the ASQ fits in one page).
    let mut nvmec = instantiate_and_build_admin_queue(
        cq_buf,
        64,
        sq_buf,
        64,
        true,
        Some(int_controller),
        driver.clone(),
        gm,
    )
    .await;

    let mut slot = 0;
    let mut backoff = Backoff::new(&driver);

    if let Some((cq_base, sq_base)) = dq_bases {
        let mut command = spec::Command::new_zeroed();
        command
            .cdw0
            .set_opcode(spec::AdminOpcode::CREATE_IO_COMPLETION_QUEUE.0);
        command.cdw10 = spec::Cdw10CreateIoQueue::new()
            .with_qid(1)
            .with_qsize_z(16)
            .into();
        command.cdw11 = spec::Cdw11CreateIoCompletionQueue::new()
            .with_pc(true)
            .with_ien(false)
            .with_iv(0)
            .into();
        command.dptr[0] = cq_base;
        command.cdw0.set_cid(10);

        write_command_to_queue(gm, sq_buf, slot, &command);
        // Ring the admin queue doorbell.  Doorbell base is 0x1000.
        nvmec
            .write_bar0(0x1000, (slot as u32 + 1).as_bytes())
            .unwrap();
        backoff.back_off().await;
        wait_for_msi(driver.clone(), int_controller, 1000, 0xfeed0000, 0x1111).await;
        backoff.back_off().await;
        let cqe = read_completion_from_queue(gm, cq_buf, slot);
        assert_eq!(cqe.cid, 10);
        assert_eq!(
            cqe.status.status(),
            spec::Status::SUCCESS.status_code() as u16
        );

        slot += 1;
        let mut command = spec::Command::new_zeroed();
        command
            .cdw0
            .set_opcode(spec::AdminOpcode::CREATE_IO_SUBMISSION_QUEUE.0);
        command.cdw10 = spec::Cdw10CreateIoQueue::new()
            .with_qid(1)
            .with_qsize_z(16)
            .into();
        command.cdw11 = spec::Cdw11CreateIoSubmissionQueue::new()
            .with_pc(true)
            .with_qprio(0)
            .with_cqid(1)
            .into();
        command.dptr[0] = sq_base;
        command.cdw0.set_cid(11);

        write_command_to_queue(gm, sq_buf, slot, &command);
        nvmec
            .write_bar0(0x1000, (slot as u32 + 1).as_bytes())
            .unwrap();
        backoff.back_off().await;
        wait_for_msi(driver.clone(), int_controller, 1000, 0xfeed0000, 0x1111).await;
        backoff.back_off().await;
        let cqe = read_completion_from_queue(gm, cq_buf, slot);
        assert_eq!(cqe.cid, 11);
        assert_eq!(
            cqe.status.status(),
            spec::Status::SUCCESS.status_code() as u16
        );

        slot += 1;
    }

    let mut command = spec::Command::new_zeroed();
    command
        .cdw0
        .set_opcode(spec::AdminOpcode::DOORBELL_BUFFER_CONFIG.0);
    command.dptr[0] = DOORBELL_BUFFER_BASE;
    command.dptr[1] = EVT_IDX_BUFFER_BASE;

    write_command_to_queue(gm, sq_buf, slot, &command);

    // Update the shadow doorbell, so that uninitialized (or zeroed) memory
    // doesn't get immediately misinterpreted as a doorbell value.
    let new_sq_db = slot as u32 + 1;
    gm.write_plain::<u32>(DOORBELL_BUFFER_BASE, &new_sq_db)
        .unwrap();
    nvmec.write_bar0(0x1000, new_sq_db.as_bytes()).unwrap();

    backoff.back_off().await;
    wait_for_msi(driver.clone(), int_controller, 1000, 0xfeed0000, 0x1111).await;
    backoff.back_off().await;

    let cqe = read_completion_from_queue(gm, cq_buf, slot);
    assert_eq!(
        cqe.status.status(),
        spec::Status::SUCCESS.status_code() as u16
    );

    nvmec
}

#[async_test]
async fn test_setup_shadow_doorbells(driver: DefaultDriver) {
    let cq_buf = PrpRange::new(vec![CQ_BASE], 0, PAGE_SIZE64).unwrap();
    let sq_buf = PrpRange::new(vec![SQ_BASE], 0, PAGE_SIZE64).unwrap();
    let _sdb_buf = PrpRange::new(vec![DOORBELL_BUFFER_BASE], 0, PAGE_SIZE64).unwrap();
    let _evt_idx_buf = PrpRange::new(vec![EVT_IDX_BUFFER_BASE], 0, PAGE_SIZE64).unwrap();
    let gm = test_memory();
    let int_controller = TestPciInterruptController::new();

    setup_shadow_doorbells(driver.clone(), &cq_buf, &sq_buf, &gm, &int_controller, None).await;
}

#[async_test]
async fn test_setup_sq_ring_with_shadow(driver: DefaultDriver) {
    let cq_buf = PrpRange::new(vec![CQ_BASE], 0, PAGE_SIZE64).unwrap();
    let sq_buf = PrpRange::new(vec![SQ_BASE], 0, PAGE_SIZE64).unwrap();
    let gm = test_memory();
    let int_controller = TestPciInterruptController::new();
    let mut backoff = Backoff::new(&driver);

    // Check that the old value was 0, just to be sure.
    let sdb = gm.read_plain::<u32>(DOORBELL_BUFFER_BASE).unwrap();
    assert_eq!(sdb, 0);

    let mut nvmec =
        setup_shadow_doorbells(driver.clone(), &cq_buf, &sq_buf, &gm, &int_controller, None).await;

    let sdb = gm.read_plain::<u32>(DOORBELL_BUFFER_BASE).unwrap();
    // The doorbell value should be current (one admin queue command has been
    // issued).
    assert_eq!(sdb, 1);

    /* From the NVMe Spec (ver. 2.0a):
    B.5 Updating Controller Doorbell Properties using a Shadow Doorbell Buffer

    B.5.1. Shadow Doorbell Buffer Overview
    Controllers that support the Doorbell Buffer Config command are typically emulated controllers where this
    feature is used to enhance the performance of host software running in Virtual Machines. If supported by
    the controller, host software may enable Shadow Doorbell buffers by submitting the Doorbell Buffer Config
    command (refer to section 5.8).

    After the completion of the Doorbell Buffer Config command, host software shall submit commands by
    updating the appropriate entry in the Shadow Doorbell buffer instead of updating the controller's
    corresponding doorbell property. If updating an entry in the Shadow Doorbell buffer changes the value from
    being less than or equal to the value of the corresponding EventIdx buffer entry to being greater than that
    value, then the host shall also update the controller's corresponding doorbell property to match the value
    of that entry in the Shadow Doorbell buffer. Queue wrap conditions shall be taken into account in all
    comparisons in this paragraph.

    The controller may read from the Shadow Doorbell buffer and update the EventIdx buffer at any time (e.g.,
    before the host writes to the controller's doorbell property).

    B.5.2. Example Algorithm for Controller Doorbell Property Updates
    Host software may use modular arithmetic where the modulus is the queue depth to decide if the controller
    doorbell property should be updated, specifically:

    • Compute X as the new doorbell value minus the corresponding EventIdx value, modulo queue
      depth; and
    • Compute Y as the new doorbell value minus the old doorbell value in the shadow doorbell buffer,
      also modulo queue depth.

    If X is less than or equal to Y, the controller doorbell property should be updated with the new doorbell
    value.
    */

    // First, put one command into the SQ and check the EVT_IDX value.

    let mut entry = spec::Command::new_zeroed();
    entry.cdw0.set_opcode(spec::AdminOpcode::IDENTIFY.0);
    let cdw10 = spec::Cdw10Identify::new().with_cns(spec::Cns::CONTROLLER.0);
    entry.cdw10 = u32::from(cdw10);
    entry.dptr[0] = 0x6000; // unused
    write_command_to_queue(&gm, &sq_buf, 1, &entry);

    let new_sq_db = 2u32;
    // Update the shadow.
    gm.write_plain::<u32>(DOORBELL_BUFFER_BASE, &new_sq_db)
        .unwrap();

    // Ring the admin queue doorbell.
    nvmec.write_bar0(0x1000, new_sq_db.as_bytes()).unwrap();
    backoff.back_off().await;

    // The current implementation will advance the event index immediately.
    let sq_evt_idx = gm.read_plain::<u32>(EVT_IDX_BUFFER_BASE).unwrap();
    assert_eq!(sq_evt_idx, new_sq_db);
}

#[async_test]
async fn test_update_data_queues_with_shadow_doorbells(driver: DefaultDriver) {
    let cq_buf = PrpRange::new(vec![CQ_BASE], 0, PAGE_SIZE64).unwrap();
    let sq_buf = PrpRange::new(vec![SQ_BASE], 0, PAGE_SIZE64).unwrap();
    let gm = test_memory();
    let int_controller = TestPciInterruptController::new();
    let mut backoff = Backoff::new(&driver);

    let _nvmec = setup_shadow_doorbells(
        driver.clone(),
        &cq_buf,
        &sq_buf,
        &gm,
        &int_controller,
        Some((DCQ_BASE, DSQ_BASE)),
    )
    .await;

    backoff.back_off().await;
}
