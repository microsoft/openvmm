// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::test_helpers::TestNvmeMmioRegistration;
use crate::BAR0_LEN;
use crate::MAX_NSID;
use crate::NvmeFaultController;
use crate::NvmeFaultControllerCaps;
use crate::PAGE_SIZE64;
use crate::command_match::CommandMatchBuilder;
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
use disk_backend::Disk;
use disk_prwrap::DiskWithReservations;
use disklayer_ram::ram_disk;
use guestmem::GuestMemory;
use guid::Guid;
use mesh::CellUpdater;
use nvme_resources::fault::AdminQueueFaultBehavior;
use nvme_resources::fault::AdminQueueFaultConfig;
use nvme_resources::fault::FaultConfiguration;
use nvme_spec::Command;
use nvme_spec::Completion;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::timer::PolledTimer;
use pci_core::msi::MsiConnection;
use pci_core::test_helpers::TestPciInterruptController;
use std::time::Duration;
use user_driver::backoff::Backoff;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

fn instantiate_controller(
    driver: DefaultDriver,
    gm: &GuestMemory,
    int_controller: Option<&TestPciInterruptController>,
    fault_configuration: FaultConfiguration,
) -> NvmeFaultController {
    let mut mmio_reg = TestNvmeMmioRegistration {};
    let vm_task_driver = &VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let msi_conn = MsiConnection::new();
    let controller = NvmeFaultController::new(
        vm_task_driver,
        gm.clone(),
        &msi_conn.target(),
        &mut mmio_reg,
        NvmeFaultControllerCaps {
            msix_count: 64,
            max_io_queues: 64,
            subsystem_id: Guid::new_random(),
        },
        fault_configuration,
        false,
    );

    if let Some(intc) = int_controller {
        msi_conn.connect(intc.signal_msi());
    }
    controller
}

fn write_msix_table_entry(
    controller: &mut NvmeFaultController,
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
    fault_configuration: FaultConfiguration,
) -> NvmeFaultController {
    let mut nvmec = instantiate_controller(driver.clone(), gm, int_controller, fault_configuration);
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
    let fault_configuration = FaultConfiguration::new(CellUpdater::new(false).cell());
    let mut nvmec = instantiate_controller(driver, &gm, None, fault_configuration);
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
    let fault_configuration = FaultConfiguration::new(CellUpdater::new(false).cell());

    let mut nvmec = instantiate_controller(driver, &gm, None, fault_configuration);
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
    let fault_configuration = FaultConfiguration::new(CellUpdater::new(false).cell());
    let mut nvmec = instantiate_controller(driver, &gm, None, fault_configuration);

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
    let fault_configuration = FaultConfiguration::new(CellUpdater::new(false).cell());
    let mut nvmec = instantiate_controller(driver, &gm, None, fault_configuration);

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

async fn send_identify(
    driver: DefaultDriver,
    fault_configuration: FaultConfiguration,
) -> Completion {
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
        fault_configuration,
    )
    .await;

    // There should be no MSI-X triggered at this point.
    let next_int = int_controller.get_next_interrupt();
    assert!(next_int.is_none());

    // Construct an admin queue command into the first entry in the ASQ, which is at 0x1000 in the "test memory".
    let mut entry = Command::new_zeroed();
    entry.cdw0.set_opcode(spec::AdminOpcode::IDENTIFY.0);
    let cdw10 = spec::Cdw10Identify::new().with_cns(spec::Cns::CONTROLLER.0);
    entry.cdw10 = u32::from(cdw10);
    entry.dptr[0] = 1;

    write_command_to_queue(&gm, &dm2, 0, &entry);

    // Ring the admin queue doorbell.
    nvmec.write_bar0(0x1000, 1u32.as_bytes()).unwrap();

    wait_for_msi(driver.clone(), &int_controller, 1000, 0xfeed0000, 0x1111).await;

    read_completion_from_queue(&gm, &dm1, 0)
}

#[async_test]
async fn test_send_identify_no_fault(driver: DefaultDriver) {
    let fault_configuration = FaultConfiguration::new(CellUpdater::new(false).cell());
    let cqe = send_identify(driver, fault_configuration).await;

    assert_eq!(cqe.status.status(), spec::Status::SUCCESS.0);
    assert_eq!(cqe.cid, 0);
}

#[async_test]
async fn test_send_identify_with_sq_fault(driver: DefaultDriver) {
    let mut faulty_identify = Command::new_zeroed();
    faulty_identify.cdw0.set_cid(10);
    // Use a valid Identify CONTROLLER request (which ignores NSID) so the
    // command still completes successfully; this test only verifies that the
    // fault overwrote the command (observed via the changed CID).
    faulty_identify.cdw10 = u32::from(spec::Cdw10Identify::new().with_cns(spec::Cns::CONTROLLER.0));

    let fault_configuration = FaultConfiguration::new(CellUpdater::new(true).cell())
        .with_admin_queue_fault(
            AdminQueueFaultConfig::new().with_submission_queue_fault(
                CommandMatchBuilder::new()
                    .match_cdw0_opcode(spec::AdminOpcode::IDENTIFY.0)
                    .build(),
                AdminQueueFaultBehavior::Update(faulty_identify),
            ),
        );

    let cqe = send_identify(driver, fault_configuration).await;

    assert_eq!(cqe.status.status(), spec::Status::SUCCESS.0);
    assert_eq!(cqe.cid, 10); // The CID should have been overwritten by the fault.
}

/// Doorbell offset for SQ `qid`. NVMe doorbell stride is 4 bytes and the
/// admin SQ is at offset `0x1000`; SQ `qid` is at `0x1000 + (2*qid)*4`.
const fn sq_db(qid: u16) -> u64 {
    0x1000 + (2 * qid as u64) * 4
}

/// Creates an I/O completion queue and an I/O submission queue bound to it
/// with admin commands, checking each admin completion. Returns the next free
/// admin slot.
#[expect(clippy::too_many_arguments)]
async fn create_io_queue_pair(
    nvmec: &mut NvmeFaultController,
    gm: &GuestMemory,
    admin_cq_buf: &PrpRange,
    admin_sq_buf: &PrpRange,
    int_controller: &TestPciInterruptController,
    driver: DefaultDriver,
    starting_admin_slot: u32,
    qid: u16,
    cq_gpa: u64,
    sq_gpa: u64,
    qsize_z: u16,
    cq_iv: u16,
) -> u32 {
    let mut cq_command = Command::new_zeroed();
    cq_command
        .cdw0
        .set_opcode(spec::AdminOpcode::CREATE_IO_COMPLETION_QUEUE.0);
    cq_command.cdw0.set_cid(0xc100 + qid);
    cq_command.cdw10 = spec::Cdw10CreateIoQueue::new()
        .with_qid(qid)
        .with_qsize_z(qsize_z)
        .into();
    cq_command.cdw11 = spec::Cdw11CreateIoCompletionQueue::new()
        .with_pc(true)
        .with_ien(true)
        .with_iv(cq_iv)
        .into();
    cq_command.dptr[0] = cq_gpa;

    let mut sq_command = Command::new_zeroed();
    sq_command
        .cdw0
        .set_opcode(spec::AdminOpcode::CREATE_IO_SUBMISSION_QUEUE.0);
    sq_command.cdw0.set_cid(0xc200 + qid);
    sq_command.cdw10 = spec::Cdw10CreateIoQueue::new()
        .with_qid(qid)
        .with_qsize_z(qsize_z)
        .into();
    sq_command.cdw11 = spec::Cdw11CreateIoSubmissionQueue::new()
        .with_pc(true)
        .with_cqid(qid)
        .into();
    sq_command.dptr[0] = sq_gpa;

    let mut admin_slot = starting_admin_slot;
    for command in [cq_command, sq_command] {
        let cqe = submit_admin_command(
            nvmec,
            gm,
            admin_sq_buf,
            admin_cq_buf,
            int_controller,
            driver.clone(),
            admin_slot,
            &command,
        )
        .await;
        assert_eq!(cqe.cid, command.cdw0.cid());
        assert_eq!(cqe.status.status(), spec::Status::SUCCESS.0);
        admin_slot += 1;
    }
    admin_slot
}

/// Submits a single admin command in slot `admin_slot` and returns its
/// completion.
#[expect(clippy::too_many_arguments)]
async fn submit_admin_command(
    nvmec: &mut NvmeFaultController,
    gm: &GuestMemory,
    asq: &PrpRange,
    acq: &PrpRange,
    int_controller: &TestPciInterruptController,
    driver: DefaultDriver,
    admin_slot: u32,
    command: &Command,
) -> Completion {
    write_command_to_queue(gm, asq, admin_slot as usize, command);
    nvmec
        .write_bar0(0x1000, (admin_slot + 1).as_bytes())
        .unwrap();
    wait_for_msi(driver, int_controller, 1000, 0xfeed0000, 0x1111).await;
    read_completion_from_queue(gm, acq, admin_slot as usize)
}

/// An I/O command answers each kind of NSID with the status NVMe Base 2.1
/// gives it: an invalid NSID (0h, above NN) Invalid Namespace or Format, an
/// inactive one Invalid Field in Command (section 3.2.1.5), for a read and a
/// Flush alike. The broadcast FFFFFFFFh answers by opcode: Invalid Namespace
/// or Format for a Flush (Flush Behavior 10b, section 7.2), Invalid Field in
/// Command for anything else (Figure 92). That includes the reserved opcode
/// 03h, which the attached namespace answers Invalid Command Opcode: the NSID
/// is checked before the opcode. Every NSID answer sets Do Not Retry; the
/// opcode refusal and the successes do not.
///
/// The rows to the attached namespace run last on the same SQ, so the rows
/// above cannot pass by refusing every NSID or every Flush.
#[async_test]
async fn test_io_command_status_per_nsid(driver: DefaultDriver) {
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
        FaultConfiguration::new(CellUpdater::new(false).cell()),
    )
    .await;

    let disk = ram_disk(1 << 20, /* read_only = */ false).unwrap();
    nvmec.client().add_namespace(1, disk).await.unwrap();

    write_msix_table_entry(&mut nvmec, 1, 0xfeed0000, 0x2222, false);

    let io_cq_gpa: u64 = 0x4000;
    let io_sq_gpa: u64 = 0x5000;
    let read_buf_gpa: u64 = 0x8000;
    create_io_queue_pair(
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
        /* qsize_z = */ 16,
        /* cq_iv = */ 1,
    )
    .await;
    let io_sq_buf = PrpRange::new(vec![io_sq_gpa], 0, PAGE_SIZE64).unwrap();
    let io_cq_buf = PrpRange::new(vec![io_cq_gpa], 0, PAGE_SIZE64).unwrap();

    struct Row {
        what: &'static str,
        opcode: nvm::NvmOpcode,
        nsid: u32,
        status: spec::Status,
        dnr: bool,
    }
    let rows = [
        Row {
            what: "read, NSID 0h",
            opcode: nvm::NvmOpcode::READ,
            nsid: 0,
            status: spec::Status::INVALID_NAMESPACE_OR_FORMAT,
            dnr: true,
        },
        Row {
            what: "flush, NSID 0h",
            opcode: nvm::NvmOpcode::FLUSH,
            nsid: 0,
            status: spec::Status::INVALID_NAMESPACE_OR_FORMAT,
            dnr: true,
        },
        Row {
            what: "read, inactive NSID",
            opcode: nvm::NvmOpcode::READ,
            nsid: 2,
            status: spec::Status::INVALID_FIELD_IN_COMMAND,
            dnr: true,
        },
        Row {
            what: "flush, inactive NSID",
            opcode: nvm::NvmOpcode::FLUSH,
            nsid: 2,
            status: spec::Status::INVALID_FIELD_IN_COMMAND,
            dnr: true,
        },
        Row {
            what: "read, NSID NN (inactive)",
            opcode: nvm::NvmOpcode::READ,
            nsid: MAX_NSID,
            status: spec::Status::INVALID_FIELD_IN_COMMAND,
            dnr: true,
        },
        Row {
            what: "read, NSID above NN",
            opcode: nvm::NvmOpcode::READ,
            nsid: MAX_NSID + 1,
            status: spec::Status::INVALID_NAMESPACE_OR_FORMAT,
            dnr: true,
        },
        Row {
            what: "read, NSID FFFFFFFFh",
            opcode: nvm::NvmOpcode::READ,
            nsid: 0xffff_ffff,
            status: spec::Status::INVALID_FIELD_IN_COMMAND,
            dnr: true,
        },
        Row {
            what: "flush, NSID FFFFFFFFh",
            opcode: nvm::NvmOpcode::FLUSH,
            nsid: 0xffff_ffff,
            status: spec::Status::INVALID_NAMESPACE_OR_FORMAT,
            dnr: true,
        },
        Row {
            what: "opcode 03h, NSID FFFFFFFFh",
            opcode: nvm::NvmOpcode(0x03),
            nsid: 0xffff_ffff,
            status: spec::Status::INVALID_FIELD_IN_COMMAND,
            dnr: true,
        },
        Row {
            what: "opcode 03h, attached NSID",
            opcode: nvm::NvmOpcode(0x03),
            nsid: 1,
            status: spec::Status::INVALID_COMMAND_OPCODE,
            dnr: false,
        },
        Row {
            what: "read, attached NSID",
            opcode: nvm::NvmOpcode::READ,
            nsid: 1,
            status: spec::Status::SUCCESS,
            dnr: false,
        },
        Row {
            what: "flush, attached NSID",
            opcode: nvm::NvmOpcode::FLUSH,
            nsid: 1,
            status: spec::Status::SUCCESS,
            dnr: false,
        },
    ];
    for (slot, row) in rows.iter().enumerate() {
        let cid = 0x900 + slot as u16;
        let mut command = Command::new_zeroed();
        command.cdw0.set_opcode(row.opcode.0);
        command.cdw0.set_cid(cid);
        command.nsid = row.nsid;
        // One block from LBA 0 for the read (NLB is zero-based); a Flush
        // ignores it.
        command.dptr[0] = read_buf_gpa;
        write_command_to_queue(&gm, &io_sq_buf, slot, &command);
        nvmec
            .write_bar0(sq_db(1), (slot as u32 + 1).as_bytes())
            .unwrap();
        wait_for_msi(driver.clone(), &int_controller, 1000, 0xfeed0000, 0x2222).await;

        let cqe = read_completion_from_queue(&gm, &io_cq_buf, slot);
        assert_eq!(cqe.cid, cid, "{}: completion for another command", row.what);
        assert_eq!(
            cqe.status.status(),
            row.status.0,
            "{}: expected {:?}",
            row.what,
            row.status
        );
        assert_eq!(cqe.status.dnr(), row.dnr, "{}: DNR", row.what);
    }
}

fn get_feature_command(fid: spec::Feature, cdw11: u32) -> Command {
    let mut command = Command::new_zeroed();
    command.cdw0.set_opcode(spec::AdminOpcode::GET_FEATURES.0);
    command.cdw10 = spec::Cdw10GetFeatures::new().with_fid(fid.0).into();
    command.cdw11 = cdw11;
    command
}

/// Get Features of Reservation Persistence (FID 83h) answers each kind of
/// NSID with the status NVMe Base 2.1 gives it: FFFFFFFFh with Invalid
/// Field in Command (section 5.1.25.1.30), an invalid NSID (0h, or above NN)
/// with Invalid Namespace or Format and an inactive one with Invalid Field in
/// Command (Figure 92, section 3.2.1.5), and an attached namespace with
/// reservations with its PTPL state.
#[async_test]
async fn test_get_features_reservation_persistence_status_per_nsid(driver: DefaultDriver) {
    let acq = PrpRange::new(vec![0], 0, PAGE_SIZE64).unwrap();
    let asq = PrpRange::new(vec![0x1000], 0, PAGE_SIZE64).unwrap();
    let gm = test_memory();
    let int_controller = TestPciInterruptController::new();

    let mut nvmec = instantiate_and_build_admin_queue(
        &acq,
        64,
        &asq,
        64,
        true,
        Some(&int_controller),
        driver.clone(),
        &gm,
        FaultConfiguration::new(CellUpdater::new(false).cell()),
    )
    .await;

    // PTPL defaults to off; set it so the success row reads a value that
    // only the backend can have supplied.
    let disk = Disk::new(DiskWithReservations::new(ram_disk(1 << 20, false).unwrap())).unwrap();
    disk.pr()
        .unwrap()
        .register(None, 0x1234, Some(true))
        .await
        .unwrap();
    nvmec.client().add_namespace(1, disk).await.unwrap();

    struct Row {
        what: &'static str,
        nsid: u32,
        status: spec::Status,
    }
    let rows = [
        Row {
            what: "the broadcast NSID FFFFFFFFh",
            nsid: 0xffff_ffff,
            status: spec::Status::INVALID_FIELD_IN_COMMAND,
        },
        Row {
            what: "the invalid NSID 0h",
            nsid: 0,
            status: spec::Status::INVALID_NAMESPACE_OR_FORMAT,
        },
        Row {
            what: "an invalid NSID above NN",
            nsid: MAX_NSID + 1,
            status: spec::Status::INVALID_NAMESPACE_OR_FORMAT,
        },
        Row {
            what: "an inactive NSID",
            nsid: 2,
            status: spec::Status::INVALID_FIELD_IN_COMMAND,
        },
        Row {
            what: "the attached namespace with reservations",
            nsid: 1,
            status: spec::Status::SUCCESS,
        },
    ];
    for (slot, row) in rows.iter().enumerate() {
        let mut command = get_feature_command(spec::Feature::NVM_RESERVATION_PERSISTENCE, 0);
        command.cdw0.set_cid(0x830 + slot as u16);
        command.nsid = row.nsid;
        let cqe = submit_admin_command(
            &mut nvmec,
            &gm,
            &asq,
            &acq,
            &int_controller,
            driver.clone(),
            slot as u32,
            &command,
        )
        .await;
        assert_eq!(
            cqe.cid,
            0x830 + slot as u16,
            "{}: the completion is another command's",
            row.what
        );
        assert_eq!(
            cqe.status.status(),
            row.status.0,
            "{}: expected {:?}",
            row.what,
            row.status
        );
        if row.status == spec::Status::SUCCESS {
            assert_eq!(cqe.dw0, 1, "{}: DW0 is not the PTPL state", row.what);
        }
    }
}

/// An admin completion waits for a free admin CQ slot instead of being
/// dropped (NVMe Base 2.1 section 3.3.1.2.1: "the controller shall not post
/// status to that Completion Queue until slots become available"). The admin
/// CQ write asserts that it finds a free slot; this pins the wait that
/// reserves it.
///
/// A 4-entry admin CQ holds three completions. The host never advances the
/// CQ head, so the fourth command must stay unanswered until the head moves
/// by one, and must then post at the tail, slot 3, which a full queue keeps
/// empty.
#[async_test]
async fn test_admin_completion_waits_for_free_cq_slot(driver: DefaultDriver) {
    const ACQ_ENTRIES: u32 = 4;
    let acq = PrpRange::new(vec![0], 0, PAGE_SIZE64).unwrap();
    let asq = PrpRange::new(vec![0x1000], 0, PAGE_SIZE64).unwrap();
    let gm = test_memory();
    let int_controller = TestPciInterruptController::new();

    let mut nvmec = instantiate_and_build_admin_queue(
        &acq,
        ACQ_ENTRIES,
        &asq,
        64,
        true,
        Some(&int_controller),
        driver.clone(),
        &gm,
        FaultConfiguration::new(CellUpdater::new(false).cell()),
    )
    .await;

    let command_in = |slot: u32| {
        let mut command = get_feature_command(spec::Feature::ARBITRATION, 0);
        command.cdw0.set_cid(0xa00 + slot as u16);
        command
    };

    // Fill the CQ: one slot stays empty, so ACQ_ENTRIES - 1 completions fit.
    for slot in 0..ACQ_ENTRIES - 1 {
        let cqe = submit_admin_command(
            &mut nvmec,
            &gm,
            &asq,
            &acq,
            &int_controller,
            driver.clone(),
            slot,
            &command_in(slot),
        )
        .await;
        assert_eq!(cqe.cid, 0xa00 + slot as u16);
        assert_eq!(cqe.status.status(), spec::Status::SUCCESS.0);
    }

    // The CQ is full now. The next command must not be answered.
    let last = ACQ_ENTRIES - 1;
    write_command_to_queue(&gm, &asq, last as usize, &command_in(last));
    nvmec.write_bar0(0x1000, (last + 1).as_bytes()).unwrap();
    PolledTimer::new(&driver)
        .sleep(Duration::from_millis(100))
        .await;
    assert!(
        int_controller.get_next_interrupt().is_none(),
        "the controller interrupted while the admin CQ was full"
    );
    let cqe = read_completion_from_queue(&gm, &acq, last as usize);
    assert!(
        !cqe.status.phase(),
        "a completion was posted into a full admin CQ: {cqe:?}"
    );

    // Consume one entry: the completion must arrive in the freed-up slot.
    nvmec.write_bar0(0x1004, 1u32.as_bytes()).unwrap();
    wait_for_msi(driver.clone(), &int_controller, 1000, 0xfeed0000, 0x1111).await;
    let cqe = read_completion_from_queue(&gm, &acq, last as usize);
    assert!(cqe.status.phase(), "the held completion was never posted");
    assert_eq!(cqe.cid, 0xa00 + last as u16);
    assert_eq!(cqe.status.status(), spec::Status::SUCCESS.0);
}

/// The SMART / Health Information log page is not supported per namespace
/// (LPA.SMARTS is 0), so NSID 0h and FFFFFFFFh both return the controller log
/// page and any other NSID is Invalid Field in Command (NVMe Base 2.1 section
/// 5.1.12.1.3).
#[async_test]
async fn test_get_smart_log_page_nsid(driver: DefaultDriver) {
    let acq = PrpRange::new(vec![0], 0, PAGE_SIZE64).unwrap();
    let asq = PrpRange::new(vec![0x1000], 0, PAGE_SIZE64).unwrap();
    let gm = test_memory();
    let int_controller = TestPciInterruptController::new();

    let mut nvmec = instantiate_and_build_admin_queue(
        &acq,
        64,
        &asq,
        64,
        true,
        Some(&int_controller),
        driver.clone(),
        &gm,
        FaultConfiguration::new(CellUpdater::new(false).cell()),
    )
    .await;

    let data_gpa = 0x8000u64;
    for (admin_slot, (nsid, expected)) in [
        (0, spec::Status::SUCCESS),
        (!0, spec::Status::SUCCESS),
        (1, spec::Status::INVALID_FIELD_IN_COMMAND),
    ]
    .into_iter()
    .enumerate()
    {
        gm.write_plain::<[u8; 512]>(data_gpa, &[0xff; 512]).unwrap();

        let mut cmd = Command::new_zeroed();
        cmd.cdw0.set_opcode(spec::AdminOpcode::GET_LOG_PAGE.0);
        cmd.nsid = nsid;
        // 512 bytes = 128 dwords; NUMDL is a 0-based dword count.
        cmd.cdw10 = spec::Cdw10GetLogPage::new()
            .with_lid(spec::LogPageIdentifier::HEALTH_INFORMATION.0)
            .with_numdl_z(127)
            .into();
        cmd.dptr[0] = data_gpa;

        write_command_to_queue(&gm, &asq, admin_slot, &cmd);
        nvmec
            .write_bar0(0x1000, (admin_slot as u32 + 1).as_bytes())
            .unwrap();
        wait_for_msi(driver.clone(), &int_controller, 1000, 0xfeed0000, 0x1111).await;
        let cqe = read_completion_from_queue(&gm, &acq, admin_slot);

        assert_eq!(
            cqe.status.status(),
            expected.0,
            "unexpected status for nsid {nsid:#x}"
        );
        if expected == spec::Status::SUCCESS {
            let page = gm.read_plain::<[u8; 512]>(data_gpa).unwrap();
            assert_eq!(page, [0; 512], "log page not written for nsid {nsid:#x}");
        }
    }
}
