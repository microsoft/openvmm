// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests that `ChangeDeviceState::stop` leaves the controller doing no work.

use super::controller_tests::create_io_queue_pair;
use super::controller_tests::instantiate_and_build_admin_queue_tapped;
use super::controller_tests::sq_db;
use super::controller_tests::write_msix_table_entry;
use super::test_helpers::DiskGate;
use super::test_helpers::MsiTap;
use super::test_helpers::gated_disk;
use super::test_helpers::read_completion_from_queue;
use super::test_helpers::settle;
use super::test_helpers::test_memory;
use super::test_helpers::write_command_to_queue;
use crate::NvmeController;
use crate::PAGE_SIZE64;
use crate::prp::PrpRange;
use crate::spec;
use crate::spec::nvm;
use guestmem::GuestMemory;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::task::Spawn;
use pci_core::test_helpers::TestPciInterruptController;
use std::task::Poll;
use vmcore::device_state::ChangeDeviceState;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const IO_CQ_GPA: u64 = 0x4000;
const IO_SQ_GPA: u64 = 0x5000;
const IO_MSI_DATA: u32 = 0x2222;

struct Fixture {
    nvmec: NvmeController,
    gm: GuestMemory,
    tap: MsiTap,
    io_sq: PrpRange,
    io_cq: PrpRange,
}

/// A controller with one I/O queue pair (qid 1, vector 1) and a gated disk
/// as namespace 1.
async fn fixture(driver: DefaultDriver) -> (Fixture, DiskGate) {
    let admin_cq = PrpRange::new(vec![0], 0, PAGE_SIZE64).unwrap();
    let admin_sq = PrpRange::new(vec![0x1000], 0, PAGE_SIZE64).unwrap();
    let gm = test_memory();
    let intc = TestPciInterruptController::new();
    let (tap_send, tap_recv) = mesh::channel();
    let mut nvmec = instantiate_and_build_admin_queue_tapped(
        &admin_cq,
        64,
        &admin_sq,
        64,
        &intc,
        driver.clone(),
        &gm,
        tap_send,
    )
    .await;
    let (disk, gate) = gated_disk(2048);
    nvmec.client().add_namespace(1, disk).await.unwrap();
    write_msix_table_entry(&mut nvmec, 1, 0xfeed0000, IO_MSI_DATA, false);
    create_io_queue_pair(
        &mut nvmec, &gm, &admin_cq, &admin_sq, &intc, driver, 0, 1, IO_CQ_GPA, IO_SQ_GPA, 16, 16, 1,
    )
    .await;
    (
        Fixture {
            nvmec,
            gm,
            tap: MsiTap(tap_recv),
            io_sq: PrpRange::new(vec![IO_SQ_GPA], 0, PAGE_SIZE64).unwrap(),
            io_cq: PrpRange::new(vec![IO_CQ_GPA], 0, PAGE_SIZE64).unwrap(),
        },
        gate,
    )
}

/// Returns once every task already woken has run, without going through the
/// coordinator.
async fn run_woken_tasks(driver: &DefaultDriver) {
    let (send, recv) = mesh::oneshot();
    driver
        .spawn("run-woken-tasks", async move { send.send(()) })
        .detach();
    recv.await.unwrap();
}

fn read_cmd(cid: u16, data_gpa: u64) -> spec::Command {
    let mut cmd = spec::Command::new_zeroed();
    cmd.cdw0.set_opcode(nvm::NvmOpcode::READ.0);
    cmd.cdw0.set_cid(cid);
    cmd.nsid = 1;
    cmd.dptr[0] = data_gpa;
    cmd
}

/// stop() waits for an in-flight command and posts its completion.
#[async_test]
async fn stop_waits_for_in_flight_command(driver: DefaultDriver) {
    let (mut f, mut gate) = fixture(driver.clone()).await;
    write_command_to_queue(&f.gm, &f.io_sq, 0, &read_cmd(7, 0x8000));
    f.nvmec.write_bar0(sq_db(1), 1u32.as_bytes()).unwrap();
    gate.wait_entered().await;

    let mut stop = std::pin::pin!(f.nvmec.stop());
    assert!(matches!(futures::poll!(stop.as_mut()), Poll::Pending));
    // Let stop() reach the drain before releasing the read, so the completion
    // can only come from the drain and not from the I/O task's run loop. Each
    // step (admin task stopped, I/O task stopped, drain started) takes one
    // round on this single-threaded executor.
    for _ in 0..8 {
        run_woken_tasks(&driver).await;
    }
    assert!(matches!(futures::poll!(stop.as_mut()), Poll::Pending));

    gate.release();
    stop.await;

    let cqe = read_completion_from_queue(&f.gm, &f.io_cq, 0);
    assert_eq!(
        cqe.cid, 7,
        "completion must be posted before stop() returns"
    );
    assert!(cqe.status.phase());
}

/// A doorbell rung while stopped is not processed until start().
#[async_test]
async fn no_command_runs_between_stop_and_start(driver: DefaultDriver) {
    let (mut f, mut gate) = fixture(driver).await;
    // Use up the gated read so later reads complete at once.
    write_command_to_queue(&f.gm, &f.io_sq, 0, &read_cmd(1, 0x8000));
    f.nvmec.write_bar0(sq_db(1), 1u32.as_bytes()).unwrap();
    gate.wait_entered().await;
    gate.release();
    f.tap.next_with_data(IO_MSI_DATA).await;

    f.nvmec.stop().await;
    write_command_to_queue(&f.gm, &f.io_sq, 1, &read_cmd(2, 0x8000));
    f.nvmec.write_bar0(sq_db(1), 2u32.as_bytes()).unwrap();
    settle(&mut f.nvmec).await;

    assert!(
        !gate.try_entered(),
        "the stopped controller sent a command rung after stop() to the disk"
    );
    assert!(
        f.tap.0.try_recv().is_err(),
        "the stopped controller raised an MSI for a command rung after stop()"
    );

    f.nvmec.start();
    f.tap.next_with_data(IO_MSI_DATA).await;
    let cqe = read_completion_from_queue(&f.gm, &f.io_cq, 1);
    assert_eq!(cqe.cid, 2);
}

/// stop() does not post the in-flight commands of a queue that already failed.
#[async_test]
async fn stop_does_not_post_a_failed_queues_in_flight_command(driver: DefaultDriver) {
    let (mut f, mut gate) = fixture(driver).await;
    write_command_to_queue(&f.gm, &f.io_sq, 0, &read_cmd(7, 0x8000));
    f.nvmec.write_bar0(sq_db(1), 1u32.as_bytes()).unwrap();
    gate.wait_entered().await;
    // A tail past the end of the SQ fails the queue while the read is held.
    f.nvmec.write_bar0(sq_db(1), 0xffffu32.as_bytes()).unwrap();
    settle(&mut f.nvmec).await;

    {
        let mut stop = std::pin::pin!(f.nvmec.stop());
        assert!(matches!(futures::poll!(stop.as_mut()), Poll::Pending));
        gate.release();
        stop.await;
    }
    settle(&mut f.nvmec).await;

    let cqe = read_completion_from_queue(&f.gm, &f.io_cq, 0);
    assert!(
        !cqe.status.phase(),
        "stop posted a failed queue's in-flight command (cid {})",
        cqe.cid
    );
    // Setup left admin MSIs in the tap; only an I/O vector MSI is wrong.
    while let Ok((_, data)) = f.tap.0.try_recv() {
        assert_ne!(
            data, IO_MSI_DATA,
            "stop raised an MSI for a failed queue's in-flight command"
        );
    }
}

/// stop() lets a guest-initiated controller reset finish, discarding the
/// reset's in-flight I/O and leaving the controller disabled.
#[async_test]
async fn stop_finishes_a_controller_reset_in_progress(driver: DefaultDriver) {
    let (mut f, mut gate) = fixture(driver.clone()).await;
    write_command_to_queue(&f.gm, &f.io_sq, 0, &read_cmd(7, 0x8000));
    f.nvmec.write_bar0(sq_db(1), 1u32.as_bytes()).unwrap();
    gate.wait_entered().await;

    // CC.EN = 0 starts a reset that cannot finish while the read is held.
    f.nvmec.write_bar0(0x14, 0u32.as_bytes()).unwrap();
    settle(&mut f.nvmec).await;

    {
        let mut stop = std::pin::pin!(f.nvmec.stop());
        assert!(matches!(futures::poll!(stop.as_mut()), Poll::Pending));
        // Let any stop request reach the coordinator while the read is held.
        run_woken_tasks(&driver).await;
        assert!(matches!(futures::poll!(stop.as_mut()), Poll::Pending));
        gate.release();
        stop.await;
    }

    let cqe = read_completion_from_queue(&f.gm, &f.io_cq, 0);
    assert!(
        !cqe.status.phase(),
        "a completion the reset discards was posted (cid {})",
        cqe.cid
    );
    let mut csts = 0u32;
    f.nvmec.read_bar0(0x1c, csts.as_mut_bytes()).unwrap();
    assert_eq!(csts, 0, "the stopped controller is not fully disabled");
}
