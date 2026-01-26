// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::test_helpers::TestNvmeMmioRegistration;
use crate::BAR0_LEN;
use crate::NvmeFaultController;
use crate::NvmeFaultControllerCaps;
use crate::PAGE_SIZE64;
use crate::command_match::CommandMatchBuilder;
use crate::prp::PrpRange;
use crate::spec;
use crate::tests::test_helpers::read_completion_from_queue;
use crate::tests::test_helpers::test_memory;
use crate::tests::test_helpers::write_command_to_queue;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
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
    fault_configuration: FaultConfiguration,
) -> NvmeFaultController {
    let mut mmio_reg = TestNvmeMmioRegistration {};
    let vm_task_driver = &VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let msi_conn = MsiConnection::new(1);
    let controller = NvmeFaultController::new(
        vm_task_driver,
        gm.clone(),
        msi_conn.target(),
        &mut mmio_reg,
        NvmeFaultControllerCaps {
            msix_count: 64,
            max_io_queues: 64,
            subsystem_id: Guid::new_random(),
        },
        fault_configuration,
    );

    if let Some(intc) = int_controller {
        msi_conn.connect(0, intc.signal_msi());
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
    nvmec.pci_cfg_write(0x10, 0).unwrap();
    nvmec.pci_cfg_write(0x20, BAR0_LEN as u32).unwrap();

    // Find the MSI-X cap struct.
    let mut cfg_dword = 0;
    nvmec.pci_cfg_read(0x34, &mut cfg_dword).unwrap();
    cfg_dword &= 0xff;
    loop {
        // Read a cap struct header and pull out the fields.
        let mut cap_header = 0;
        nvmec
            .pci_cfg_read(cfg_dword as u16, &mut cap_header)
            .unwrap();
        if cap_header & 0xff == 0x11 {
            // Read the table BIR and offset.
            let mut table_loc = 0;
            nvmec
                .pci_cfg_read(cfg_dword as u16 + 4, &mut table_loc)
                .unwrap();
            // Code in other places assumes that the MSI-X table is at the beginning
            // of BAR 4.  If this becomes a fluid concept, capture the values
            // here and use them, rather than just asserting on them.
            assert_eq!(table_loc & 0x7, 4);
            assert_eq!(table_loc >> 3, 0);

            // Found MSI-X, enable it.
            nvmec.pci_cfg_write(cfg_dword as u16, 0x80000000).unwrap();
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
    nvmec.pci_cfg_write(4, 6).unwrap();

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
