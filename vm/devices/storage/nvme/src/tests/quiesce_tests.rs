// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests that `ChangeDeviceState::stop` leaves the controller doing no work,
//! and that it does so without waiting on the disk.

use super::controller_tests::create_io_queue_pair;
use super::controller_tests::instantiate_and_build_admin_queue_tapped;
use super::controller_tests::sq_db;
use super::controller_tests::write_msix_table_entry;
use super::test_helpers::DiskGate;
use super::test_helpers::MsiTap;
use super::test_helpers::bounded;
use super::test_helpers::bounded_at;
use super::test_helpers::gated_disk;
use super::test_helpers::read_completion_from_queue;
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
use pci_core::test_helpers::TestPciInterruptController;
use std::panic::Location;
use user_driver::backoff::Backoff;
use vmcore::device_state::ChangeDeviceState;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const IO_CQ_GPA: u64 = 0x4000;
const IO_SQ_GPA: u64 = 0x5000;
const IO_MSI_DATA: u32 = 0x2222;

/// Where the held read lands. `read_cmd` leaves NLB at 0, which NVMe counts
/// from zero, so the read moves one 512-byte sector.
const READ_DATA_GPA: u64 = 0x8000;
const READ_DATA_LEN: usize = 512;
/// Not zero, because the gated disk's read writes zeros.
const READ_FILL: u8 = 0xa5;

struct Fixture {
    nvmec: NvmeController,
    gm: GuestMemory,
    tap: MsiTap,
    io_sq: PrpRange,
    io_cq: PrpRange,
    admin_sq: PrpRange,
    admin_cq: PrpRange,
    intc: TestPciInterruptController,
    /// The admin SQ slot the next admin command goes into.
    next_admin_slot: u32,
}

/// A controller with one I/O queue pair (qid 1, vector 1) and a gated disk
/// as namespace 1.
async fn fixture(driver: DefaultDriver) -> (Fixture, DiskGate) {
    fixture_with(driver, false).await
}

/// [`fixture`], with a gated disk that supports persistent reservations if
/// `reservations`.
async fn fixture_with(driver: DefaultDriver, reservations: bool) -> (Fixture, DiskGate) {
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
    let (disk, gate) = gated_disk(2048, reservations);
    bounded(
        "add_namespace() did not return",
        nvmec.client().add_namespace(1, disk),
    )
    .await
    .unwrap();
    write_msix_table_entry(&mut nvmec, 1, 0xfeed0000, IO_MSI_DATA, false);
    let next_admin_slot = create_io_queue_pair(
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
            admin_sq,
            admin_cq,
            intc,
            next_admin_slot,
        },
        gate,
    )
}

/// Returns once every task the controller has already woken has run.
///
/// This is an inspect round trip through the coordinator. The test executor
/// runs spawned tasks from one FIFO queue and polls the test future again
/// only once that queue is empty, so the answer comes after every task woken
/// before the request. A stopped `TaskControl` answers inspect from its
/// state, so this does not wait for a stopped worker.
#[track_caller]
fn settle(nvmec: &mut NvmeController) -> impl Future<Output = ()> + '_ {
    let at = Location::caller();
    bounded_at("coordinator did not answer inspect", at, async {
        inspect::inspect("", nvmec).resolve().await;
    })
}

fn read_cmd(cid: u16, data_gpa: u64) -> spec::Command {
    let mut cmd = spec::Command::new_zeroed();
    cmd.cdw0.set_opcode(nvm::NvmOpcode::READ.0);
    cmd.cdw0.set_cid(cid);
    cmd.nsid = 1;
    cmd.dptr[0] = data_gpa;
    cmd
}

/// Submits a read with cid 7 into a pre-filled buffer and returns once the
/// gated disk holds it, with the MSIs left over from setup taken out of the
/// tap.
async fn hold_a_read(f: &mut Fixture, gate: &mut DiskGate) {
    f.gm.fill_at(READ_DATA_GPA, READ_FILL, READ_DATA_LEN)
        .unwrap();
    write_command_to_queue(&f.gm, &f.io_sq, 0, &read_cmd(7, READ_DATA_GPA));
    f.nvmec.write_bar0(sq_db(1), 1u32.as_bytes()).unwrap();
    gate.wait_entered().await;
    f.tap.take_pending();
}

fn read_buffer_untouched(gm: &GuestMemory) -> bool {
    let mut buf = [0u8; READ_DATA_LEN];
    gm.read_at(READ_DATA_GPA, &mut buf).unwrap();
    buf.iter().all(|&b| b == READ_FILL)
}

fn csts(nvmec: &mut NvmeController) -> spec::Csts {
    let mut dword = 0u32;
    nvmec.read_bar0(0x1c, dword.as_mut_bytes()).unwrap();
    spec::Csts::from(dword)
}

/// Waits until a CSTS read shows the controller reset finished.
async fn wait_reset_done(f: &mut Fixture, driver: &DefaultDriver) {
    let mut backoff = Backoff::new(driver);
    while csts(&mut f.nvmec).rdy() {
        backoff.back_off().await;
    }
}

/// Asserts the held read (cid 7) has done nothing the guest can see: its
/// data buffer is unwritten, its completion slot is empty, and no MSI was
/// raised since `hold_a_read`. The caller must `settle` first.
///
/// The buffer is checked too, because a worker that ran the read and threw
/// the result away (as a reset's drain does) posts nothing.
#[track_caller]
fn assert_held_read_did_nothing(f: &mut Fixture, when: &str) {
    assert!(
        read_buffer_untouched(&f.gm),
        "the held read ran on and wrote its data buffer {when}"
    );
    let cqe = read_completion_from_queue(&f.gm, &f.io_cq, 0);
    assert!(
        !cqe.status.phase(),
        "a completion was posted {when} (cid {})",
        cqe.cid
    );
    let msis = f.tap.take_pending();
    assert!(msis.is_empty(), "an MSI was raised {when}: data {msis:#x?}");
}

/// Asserts the held read's result was thrown away: nothing posted to its
/// completion slot and no MSI raised. The caller must `settle` first.
#[track_caller]
fn assert_held_read_discarded(f: &mut Fixture) {
    let cqe = read_completion_from_queue(&f.gm, &f.io_cq, 0);
    assert!(
        !cqe.status.phase(),
        "a completion the reset discards was posted (cid {})",
        cqe.cid
    );
    let msis = f.tap.take_pending();
    assert!(
        msis.is_empty(),
        "an MSI was raised for a read the reset discards: data {msis:#x?}"
    );
}

/// stop() returns while a command is held in the disk. The command neither
/// runs on nor is posted until start(), and then completes exactly once with
/// its original cid.
#[async_test]
async fn stop_returns_with_a_command_in_flight(driver: DefaultDriver) {
    let (mut f, mut gate) = fixture(driver).await;
    hold_a_read(&mut f, &mut gate).await;

    bounded(
        "stop() did not return with a read held in the disk",
        f.nvmec.stop(),
    )
    .await;

    // Released while stopped: a worker that still runs finishes the read and
    // posts it within the `settle` round trip; a suspended one does not.
    gate.release();
    settle(&mut f.nvmec).await;
    assert_held_read_did_nothing(&mut f, "while stopped");

    f.nvmec.start();
    f.tap.next_with_data(IO_MSI_DATA).await;
    let cqe = read_completion_from_queue(&f.gm, &f.io_cq, 0);
    assert!(cqe.status.phase(), "no completion in the CQ after start()");
    assert_eq!(cqe.cid, 7, "the completion carries another command's cid");
    assert!(
        !read_buffer_untouched(&f.gm),
        "the read completed without writing its data"
    );

    settle(&mut f.nvmec).await;
    let second = read_completion_from_queue(&f.gm, &f.io_cq, 1);
    assert!(
        !second.status.phase(),
        "a second completion was posted after start() (cid {})",
        second.cid
    );
    let msis = f.tap.take_pending();
    assert!(
        msis.is_empty(),
        "more MSIs after the completion: data {msis:#x?}"
    );
}

/// A doorbell rung while stopped is not consumed until start().
#[async_test]
async fn no_command_runs_between_stop_and_start(driver: DefaultDriver) {
    let (mut f, mut gate) = fixture(driver).await;
    // Use up the gated read so later reads complete at once.
    write_command_to_queue(&f.gm, &f.io_sq, 0, &read_cmd(1, 0x8000));
    f.nvmec.write_bar0(sq_db(1), 1u32.as_bytes()).unwrap();
    gate.wait_entered().await;
    gate.release();
    f.tap.next_with_data(IO_MSI_DATA).await;

    bounded("stop() did not return", f.nvmec.stop()).await;
    write_command_to_queue(&f.gm, &f.io_sq, 1, &read_cmd(2, 0x8000));
    f.nvmec.write_bar0(sq_db(1), 2u32.as_bytes()).unwrap();
    settle(&mut f.nvmec).await;

    assert!(
        !gate.try_entered(),
        "the stopped controller sent a command rung after stop() to the disk"
    );
    let msis = f.tap.take_pending();
    assert!(
        msis.is_empty(),
        "the stopped controller raised an MSI for a command rung after stop(): data {msis:#x?}"
    );

    f.nvmec.start();
    f.tap.next_with_data(IO_MSI_DATA).await;
    let cqe = read_completion_from_queue(&f.gm, &f.io_cq, 1);
    assert_eq!(cqe.cid, 2);
}

/// A queue that already failed keeps its in-flight command across a stop
/// and start: the stop does not wait for it, and the command is neither run
/// on nor posted, while stopped or after start().
///
/// On a running VM nothing polls a failed queue's commands again, and a
/// pause must not change that.
#[async_test]
async fn stop_does_not_post_a_failed_queues_in_flight_command(driver: DefaultDriver) {
    let (mut f, mut gate) = fixture(driver).await;
    hold_a_read(&mut f, &mut gate).await;
    // A tail past the end of the SQ is a queue error: the handler fails
    // with the read still held in the disk.
    f.nvmec.write_bar0(sq_db(1), 0xffffu32.as_bytes()).unwrap();
    settle(&mut f.nvmec).await;

    bounded(
        "stop() did not return with a failed queue's read held in the disk",
        f.nvmec.stop(),
    )
    .await;

    gate.release();
    settle(&mut f.nvmec).await;
    assert_held_read_did_nothing(&mut f, "while stopped");

    f.nvmec.start();
    settle(&mut f.nvmec).await;
    assert_held_read_did_nothing(&mut f, "after start() on a failed queue");
}

/// A stop that arrives while a controller reset waits for in-flight I/O
/// pauses the reset instead of finishing it. The stop returns with the read
/// still held, the reset makes no progress while stopped, and it finishes
/// after start(), discarding the read's result.
#[async_test]
async fn stop_pauses_a_controller_reset_in_progress(driver: DefaultDriver) {
    let (mut f, mut gate) = fixture(driver.clone()).await;
    hold_a_read(&mut f, &mut gate).await;

    // CC.EN = 0 starts a reset, which cannot finish while the read is held.
    f.nvmec.write_bar0(0x14, 0u32.as_bytes()).unwrap();
    settle(&mut f.nvmec).await;
    assert!(
        csts(&mut f.nvmec).rdy(),
        "the reset finished with the read still held"
    );

    bounded(
        "stop() did not return with a reset waiting on a held read",
        f.nvmec.stop(),
    )
    .await;

    // Released while stopped: a reset that is blocked on the read rather
    // than paused finishes within the `settle` round trip.
    gate.release();
    settle(&mut f.nvmec).await;
    assert!(
        csts(&mut f.nvmec).rdy(),
        "the controller reset made progress while stopped"
    );
    assert_held_read_did_nothing(&mut f, "while stopped");

    f.nvmec.start();
    bounded(
        "the controller reset did not finish after start()",
        wait_reset_done(&mut f, &driver),
    )
    .await;
    let after = csts(&mut f.nvmec);
    assert_eq!(
        u32::from(after),
        0,
        "the reset left CSTS at {after:?}, not fully disabled"
    );
    settle(&mut f.nvmec).await;
    assert_held_read_discarded(&mut f);
}

/// Second I/O queue pair for the start-during-reset test: qid 2, vector 2.
const IO2_CQ_GPA: u64 = 0x6000;
const IO2_SQ_GPA: u64 = 0x7000;
const IO2_MSI_DATA: u32 = 0x3333;

/// start() during a controller reset that the stop paused resumes only the
/// reset. The admin and I/O tasks stay stopped, because the reset owns them
/// and discards anything they would do.
///
/// The reset drains the I/O queues in order and waits on queue 1's held
/// read, so queue 2 is one it has not reached yet: a start that resumed it
/// would let it run. The admin probe covers a reset that stops the admin
/// task later than it does today.
#[async_test]
async fn start_leaves_the_tasks_of_a_paused_reset_stopped(driver: DefaultDriver) {
    let (mut f, mut gate) = fixture(driver.clone()).await;
    write_msix_table_entry(&mut f.nvmec, 2, 0xfeed0000, IO2_MSI_DATA, false);
    f.next_admin_slot = create_io_queue_pair(
        &mut f.nvmec,
        &f.gm,
        &f.admin_cq,
        &f.admin_sq,
        &f.intc,
        driver.clone(),
        f.next_admin_slot,
        2,
        IO2_CQ_GPA,
        IO2_SQ_GPA,
        16,
        16,
        2,
    )
    .await;
    let io2_sq = PrpRange::new(vec![IO2_SQ_GPA], 0, PAGE_SIZE64).unwrap();
    let io2_cq = PrpRange::new(vec![IO2_CQ_GPA], 0, PAGE_SIZE64).unwrap();
    hold_a_read(&mut f, &mut gate).await;

    // CC.EN = 0 starts a reset that waits on queue 1's held read.
    f.nvmec.write_bar0(0x14, 0u32.as_bytes()).unwrap();
    settle(&mut f.nvmec).await;
    assert!(
        csts(&mut f.nvmec).rdy(),
        "the reset finished with the read still held"
    );
    bounded(
        "stop() did not return with a reset waiting on a held read",
        f.nvmec.stop(),
    )
    .await;

    // Work for each task the reset owns, rung while stopped. The I/O probe
    // names a namespace that does not exist, so a queue that runs completes
    // it without the disk; the admin probe is a plain Get Features.
    let mut io_probe = read_cmd(9, 0x9000);
    io_probe.nsid = 99;
    write_command_to_queue(&f.gm, &io2_sq, 0, &io_probe);
    f.nvmec.write_bar0(sq_db(2), 1u32.as_bytes()).unwrap();
    let admin_slot = f.next_admin_slot;
    let mut admin_probe = spec::Command::new_zeroed();
    admin_probe
        .cdw0
        .set_opcode(spec::AdminOpcode::GET_FEATURES.0);
    admin_probe.cdw0.set_cid(0xd00d);
    admin_probe.cdw10 = spec::Cdw10GetFeatures::new()
        .with_fid(spec::Feature::ARBITRATION.0)
        .into();
    write_command_to_queue(&f.gm, &f.admin_sq, admin_slot as usize, &admin_probe);
    f.nvmec
        .write_bar0(0x1000, (admin_slot + 1).as_bytes())
        .unwrap();

    f.nvmec.start();
    // Two round trips: the first can be answered in the same coordinator
    // poll that handles Start, before a task woken by Start has run.
    settle(&mut f.nvmec).await;
    settle(&mut f.nvmec).await;
    assert!(
        csts(&mut f.nvmec).rdy(),
        "the reset finished with the read still held"
    );
    let assert_probes_untouched = |f: &mut Fixture, when: &str| {
        let io = read_completion_from_queue(&f.gm, &io2_cq, 0);
        assert!(
            !io.status.phase(),
            "an I/O queue the paused reset owns ran {when}: it posted cid {:#x}",
            io.cid
        );
        let admin = read_completion_from_queue(&f.gm, &f.admin_cq, admin_slot as usize);
        assert!(
            !admin.status.phase(),
            "the admin queue the paused reset owns ran {when}: it posted cid {:#x}",
            admin.cid
        );
        let msis = f.tap.take_pending();
        assert!(msis.is_empty(), "an MSI was raised {when}: data {msis:#x?}");
    };
    assert_probes_untouched(&mut f, "after start()");

    // The reset finishes once the read returns, and discards everything.
    gate.release();
    bounded(
        "the controller reset did not finish after start()",
        wait_reset_done(&mut f, &driver),
    )
    .await;
    settle(&mut f.nvmec).await;
    assert_probes_untouched(&mut f, "while the reset finished");
}

/// A device reset of a stopped controller finishes a controller reset the
/// stop paused, rather than waiting for a start that never comes. The start
/// that follows, as in every VM reset, runs nothing the reset discarded.
#[async_test]
async fn device_reset_finishes_a_paused_controller_reset(driver: DefaultDriver) {
    let (mut f, mut gate) = fixture(driver).await;
    hold_a_read(&mut f, &mut gate).await;
    f.nvmec.write_bar0(0x14, 0u32.as_bytes()).unwrap();
    settle(&mut f.nvmec).await;
    assert!(
        csts(&mut f.nvmec).rdy(),
        "the reset finished with the read still held"
    );
    bounded(
        "stop() did not return with a reset waiting on a held read",
        f.nvmec.stop(),
    )
    .await;

    // The device reset waits for the read, as every controller reset does.
    gate.release();
    bounded(
        "reset() did not finish a paused controller reset",
        f.nvmec.reset(),
    )
    .await;
    let after = csts(&mut f.nvmec);
    assert_eq!(
        u32::from(after),
        0,
        "the device reset left CSTS at {after:?}"
    );
    settle(&mut f.nvmec).await;
    assert_held_read_discarded(&mut f);

    // Work on the admin queue and I/O queue 1 that the reset tore down,
    // rung before start(): a task that start() brings back runs it.
    let mut io_probe = read_cmd(9, 0x9000);
    io_probe.nsid = 99;
    write_command_to_queue(&f.gm, &f.io_sq, 1, &io_probe);
    f.nvmec.write_bar0(sq_db(1), 2u32.as_bytes()).unwrap();
    let admin_slot = f.next_admin_slot;
    let mut admin_probe = spec::Command::new_zeroed();
    admin_probe
        .cdw0
        .set_opcode(spec::AdminOpcode::GET_FEATURES.0);
    admin_probe.cdw0.set_cid(0xd00d);
    admin_probe.cdw10 = spec::Cdw10GetFeatures::new()
        .with_fid(spec::Feature::ARBITRATION.0)
        .into();
    write_command_to_queue(&f.gm, &f.admin_sq, admin_slot as usize, &admin_probe);
    f.nvmec
        .write_bar0(0x1000, (admin_slot + 1).as_bytes())
        .unwrap();

    f.nvmec.start();
    // Two round trips, as in `start_leaves_the_tasks_of_a_paused_reset_stopped`.
    settle(&mut f.nvmec).await;
    settle(&mut f.nvmec).await;
    for slot in 0..2 {
        let io = read_completion_from_queue(&f.gm, &f.io_cq, slot);
        assert!(
            !io.status.phase(),
            "an I/O queue the reset tore down ran after start(): it posted cid {:#x}",
            io.cid
        );
    }
    let admin = read_completion_from_queue(&f.gm, &f.admin_cq, admin_slot as usize);
    assert!(
        !admin.status.phase(),
        "the admin queue the reset tore down ran after start(): it posted cid {:#x}",
        admin.cid
    );
    let msis = f.tap.take_pending();
    assert!(
        msis.is_empty(),
        "an MSI was raised after start(): data {msis:#x?}"
    );
    let after = csts(&mut f.nvmec);
    assert_eq!(
        u32::from(after),
        0,
        "start() after the device reset left CSTS at {after:?}"
    );
}

/// Admin MSI data: `instantiate_and_build_admin_queue_tapped` points vector
/// 0 here.
const ADMIN_MSI_DATA: u32 = 0x1111;

/// Writes a Get Features of `fid` for namespace 1 with `cid` into the next
/// admin SQ slot, rings the admin SQ tail past it, and returns the slot.
fn submit_get_feature(f: &mut Fixture, fid: spec::Feature, cid: u16) -> usize {
    let slot = f.next_admin_slot;
    f.next_admin_slot += 1;
    let mut cmd = spec::Command::new_zeroed();
    cmd.cdw0.set_opcode(spec::AdminOpcode::GET_FEATURES.0);
    cmd.cdw0.set_cid(cid);
    cmd.nsid = 1;
    cmd.cdw10 = spec::Cdw10GetFeatures::new().with_fid(fid.0).into();
    write_command_to_queue(&f.gm, &f.admin_sq, slot as usize, &cmd);
    f.nvmec.write_bar0(sq_db(0), (slot + 1).as_bytes()).unwrap();
    slot as usize
}

/// [`submit_get_feature`] of Reservation Persistence, the Get Features
/// that asks the backend.
fn submit_get_features(f: &mut Fixture, cid: u16) -> usize {
    submit_get_feature(f, spec::Feature::NVM_RESERVATION_PERSISTENCE, cid)
}

/// Submits a Get Features with `cid` and returns its admin slot once its
/// query is held in the gated disk, with the MSIs left over from setup
/// taken out of the tap.
async fn hold_a_get_features(f: &mut Fixture, gate: &mut DiskGate, cid: u16) -> usize {
    let slot = submit_get_features(f, cid);
    gate.wait_entered().await;
    f.tap.take_pending();
    slot
}

/// Whether admin CQ slot `slot` holds a completion.
fn admin_cq_holds(f: &Fixture, slot: usize) -> bool {
    read_completion_from_queue(&f.gm, &f.admin_cq, slot)
        .status
        .phase()
}

/// Waits for the admin MSI, then checks that `slot` holds the answer to the
/// Get Features with `cid` (success, PTPL set in DW0 as the gated disk
/// reports it, SQHD just past the command), and that no second completion
/// and no further admin MSI follow.
async fn expect_one_answer(f: &mut Fixture, slot: usize, cid: u16) {
    f.tap.next_with_data(ADMIN_MSI_DATA).await;
    let cqe = read_completion_from_queue(&f.gm, &f.admin_cq, slot);
    assert!(
        cqe.status.phase(),
        "an admin MSI but no completion in the Get Features' slot"
    );
    assert_eq!(cqe.cid, cid, "the completion is not the Get Features'");
    assert_eq!(
        cqe.status.status(),
        spec::Status::SUCCESS.0,
        "the Get Features failed"
    );
    assert_eq!(cqe.dw0, 1, "DW0 is not the backend's PTPL answer");
    assert_eq!(
        cqe.sqhd as usize,
        slot + 1,
        "SQHD is not just past the Get Features"
    );
    settle(&mut f.nvmec).await;
    assert!(
        !admin_cq_holds(f, slot + 1),
        "the Get Features completed twice"
    );
    let msis = f.tap.take_pending();
    assert!(
        !msis.contains(&ADMIN_MSI_DATA),
        "more admin MSIs after the answer: data {msis:#x?}"
    );
}

/// stop() returns while a Get Features of Reservation Persistence waits on
/// the disk. The command is neither answered nor posted while stopped, and
/// after start() it completes exactly once, with its cid and the backend's
/// answer, without asking the backend again.
#[async_test]
async fn stop_returns_with_a_get_features_held_in_the_disk(driver: DefaultDriver) {
    let (mut f, mut gate) = fixture_with(driver, true).await;
    let slot = hold_a_get_features(&mut f, &mut gate, 0x51).await;

    bounded(
        "stop() did not return with a Get Features held in the disk",
        f.nvmec.stop(),
    )
    .await;

    // Released while stopped: an admin task that still runs posts the
    // answer within the `settle` round trip; a suspended one does not.
    gate.release();
    settle(&mut f.nvmec).await;
    assert!(
        !admin_cq_holds(&f, slot),
        "the Get Features completed while the device was stopped"
    );
    let early = f.tap.take_pending();
    assert!(
        early.is_empty(),
        "MSIs while the device was stopped: data {early:#x?}"
    );

    f.nvmec.start();
    expect_one_answer(&mut f, slot, 0x51).await;
    assert!(
        !gate.try_entered(),
        "start() asked the backend again instead of waiting on the held query"
    );
}

/// Inspects of the running admin task while a Get Features waits on the
/// disk. `TaskControl` answers each one by stopping the task, reading its
/// state and starting it again. Each inspect finds the held command, the
/// backend is asked only once, and the command completes exactly once when
/// the disk answers.
#[async_test]
async fn a_held_get_features_survives_inspects_of_the_running_admin_task(driver: DefaultDriver) {
    let (mut f, mut gate) = fixture_with(driver, true).await;
    let slot = hold_a_get_features(&mut f, &mut gate, 0x53).await;

    for round in 1..=3 {
        let held = bounded(
            "an inspect of the running admin task did not answer",
            async {
                let mut inspection = inspect::inspect("held_command_cid", &mut f.nvmec);
                inspection.resolve().await;
                inspection.results()
            },
        )
        .await;
        assert_eq!(
            held.to_string(),
            0x53.to_string(),
            "inspect {round} did not find the held Get Features in the admin task"
        );
    }
    settle(&mut f.nvmec).await;
    assert!(
        !gate.try_entered(),
        "a restart for an inspect asked the backend again"
    );
    assert!(
        !admin_cq_holds(&f, slot),
        "the Get Features completed before the disk answered"
    );

    gate.release();
    expect_one_answer(&mut f, slot, 0x53).await;
    assert!(
        !gate.try_entered(),
        "the backend was asked again after the answer"
    );

    // The answer dropped the query with its command: a second Get Features
    // asks the backend itself.
    let next = submit_get_features(&mut f, 0x54);
    expect_one_answer(&mut f, next, 0x54).await;
    gate.wait_entered().await;
}

/// An admin command behind a held Get Features waits for it. Neither
/// completes while the disk holds the query; then they complete in SQ order,
/// each with SQHD just past itself.
#[async_test]
async fn an_admin_command_behind_a_held_get_features_waits_for_it(driver: DefaultDriver) {
    let (mut f, mut gate) = fixture_with(driver, true).await;
    let held = hold_a_get_features(&mut f, &mut gate, 0x51).await;
    let behind = submit_get_feature(&mut f, spec::Feature::NUMBER_OF_QUEUES, 0x52);

    settle(&mut f.nvmec).await;
    settle(&mut f.nvmec).await;
    for slot in [held, behind] {
        let cqe = read_completion_from_queue(&f.gm, &f.admin_cq, slot);
        assert!(
            !cqe.status.phase(),
            "admin CQ slot {slot} got cid {:#x} while the Get Features was held",
            cqe.cid
        );
    }
    let msis = f.tap.take_pending();
    assert!(
        msis.is_empty(),
        "MSIs while the Get Features was held: data {msis:#x?}"
    );

    gate.release();
    f.tap.next_with_data(ADMIN_MSI_DATA).await;
    settle(&mut f.nvmec).await;
    let first = read_completion_from_queue(&f.gm, &f.admin_cq, held);
    assert!(
        first.status.phase(),
        "no completion after the disk answered"
    );
    assert_eq!(first.cid, 0x51, "the later command completed first");
    assert_eq!(first.dw0, 1, "DW0 is not the backend's PTPL answer");
    assert_eq!(
        first.sqhd as usize,
        held + 1,
        "SQHD is not just past the held Get Features"
    );
    let second = read_completion_from_queue(&f.gm, &f.admin_cq, behind);
    assert!(
        second.status.phase(),
        "the command behind the held one did not complete"
    );
    assert_eq!(
        second.cid, 0x52,
        "the second completion is not the later command's"
    );
    assert_eq!(
        second.status.status(),
        spec::Status::SUCCESS.0,
        "the later Get Features failed"
    );
    assert_eq!(
        second.sqhd as usize,
        behind + 1,
        "SQHD is not just past the later command"
    );
    assert!(
        !admin_cq_holds(&f, behind + 1),
        "a third admin completion was posted"
    );
}

/// Waits until a CSTS read shows the controller ready.
async fn wait_ready(f: &mut Fixture, driver: &DefaultDriver) {
    let mut backoff = Backoff::new(driver);
    while !csts(&mut f.nvmec).rdy() {
        backoff.back_off().await;
    }
}

/// A controller reset finishes while a Get Features is held in the disk,
/// and discards it: the re-enabled controller posts nothing for it, even
/// once the disk answers.
#[async_test]
async fn a_controller_reset_discards_a_held_get_features(driver: DefaultDriver) {
    let (mut f, mut gate) = fixture_with(driver.clone(), true).await;
    hold_a_get_features(&mut f, &mut gate, 0x55).await;

    f.nvmec.write_bar0(0x14, 0u32.as_bytes()).unwrap();
    bounded(
        "the controller reset did not finish with a Get Features held in the disk",
        wait_reset_done(&mut f, &driver),
    )
    .await;

    // The new admin CQ starts at slot 0 with phase 1, so clear the old
    // entries before re-enabling.
    let acq_gpa = f.admin_cq.range().gpns()[0] * PAGE_SIZE64;
    f.gm.fill_at(acq_gpa, 0, PAGE_SIZE64 as usize).unwrap();
    f.nvmec.write_bar0(0x14, 1u32.as_bytes()).unwrap();
    bounded(
        "the controller did not become ready again",
        wait_ready(&mut f, &driver),
    )
    .await;
    f.next_admin_slot = 0;

    // A query that survived the reset answers now.
    gate.release();
    settle(&mut f.nvmec).await;
    settle(&mut f.nvmec).await;
    let stale = read_completion_from_queue(&f.gm, &f.admin_cq, 0);
    assert!(
        !stale.status.phase(),
        "the re-enabled controller posted cid {:#x} with no command submitted",
        stale.cid
    );
    let msis = f.tap.take_pending();
    assert!(
        !msis.contains(&ADMIN_MSI_DATA),
        "an admin MSI with no command submitted: data {msis:#x?}"
    );

    // The new admin queue works, and its first completion is its own.
    let slot = submit_get_features(&mut f, 0x56);
    expect_one_answer(&mut f, slot, 0x56).await;
}
