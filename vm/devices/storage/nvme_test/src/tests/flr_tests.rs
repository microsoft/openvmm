// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for Function Level Reset (FLR) functionality.

use std::time::Duration;

use super::test_helpers::TestNvmeMmioRegistration;
use crate::NvmeFaultController;
use crate::NvmeFaultControllerCaps;
use crate::tests::test_helpers::find_pci_capability;
use chipset_device::pci::PciConfigSpace;
use guestmem::GuestMemory;
use guid::Guid;
use mesh::CellUpdater;
use nvme_resources::fault::AdminQueueFaultConfig;
use nvme_resources::fault::FaultConfiguration;
use nvme_resources::fault::PciFaultConfig;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::timer::PolledTimer;
use pci_core::capabilities::pci_express::PCI_EXPRESS_DEVICE_CAPS_FLR_BIT_MASK;
use pci_core::msi::MsiInterruptSet;
use pci_core::spec::caps::CapabilityId;
use pci_core::spec::caps::pci_express::PciExpressCapabilityHeader;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::IntoBytes;

fn instantiate_controller_with_flr(
    driver: DefaultDriver,
    gm: &GuestMemory,
    flr_support: bool,
) -> NvmeFaultController {
    let vm_task_driver = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let mut msi_interrupt_set = MsiInterruptSet::new();
    let mut mmio_reg = TestNvmeMmioRegistration {};

    NvmeFaultController::new(
        &vm_task_driver,
        gm.clone(),
        &mut msi_interrupt_set,
        &mut mmio_reg,
        NvmeFaultControllerCaps {
            msix_count: 64,
            max_io_queues: 64,
            subsystem_id: Guid::new_random(),
            flr_support,
        },
        FaultConfiguration {
            fault_active: CellUpdater::new(false).cell(),
            admin_fault: AdminQueueFaultConfig::new(),
            pci_fault: PciFaultConfig::new(),
        },
    )
}

#[async_test]
async fn test_flr_capability_advertised(driver: DefaultDriver) {
    let gm = test_memory();
    let mut controller = instantiate_controller_with_flr(driver, &gm, true);

    // Find the PCI Express capability
    let cap_ptr = find_pci_capability(&mut controller, CapabilityId::PCI_EXPRESS.0)
        .expect("PCI Express capability should be present when FLR is enabled");

    // Read Device Capabilities register to check FLR support
    let mut device_caps = 0u32;
    controller
        .pci_cfg_read(
            cap_ptr + PciExpressCapabilityHeader::DEVICE_CAPS.0,
            &mut device_caps,
        )
        .unwrap();

    // Check Function Level Reset bit (bit 28 in Device Capabilities)
    let flr_supported = (device_caps & PCI_EXPRESS_DEVICE_CAPS_FLR_BIT_MASK) != 0;
    assert!(
        flr_supported,
        "FLR should be advertised in Device Capabilities"
    );
}

#[async_test]
async fn test_no_flr_capability_when_disabled(driver: DefaultDriver) {
    let gm = test_memory();
    let mut controller = instantiate_controller_with_flr(driver, &gm, false);

    // Find the PCI Express capability - it should not be present
    let pcie_cap_offset = find_pci_capability(&mut controller, CapabilityId::PCI_EXPRESS.0);

    assert!(
        pcie_cap_offset.is_none(),
        "PCI Express capability should not be present when FLR is disabled"
    );
}

#[async_test]
async fn test_flr_trigger(driver: DefaultDriver) {
    let gm = test_memory();
    let mut controller = instantiate_controller_with_flr(driver.clone(), &gm, true);

    // Set the ACQ base to 0x1000 and the ASQ base to 0x2000.
    let mut qword = 0x1000;
    controller.write_bar0(0x30, qword.as_bytes()).unwrap();
    qword = 0x2000;
    controller.write_bar0(0x28, qword.as_bytes()).unwrap();

    // Set the queues so that they have four entries apiece.
    let mut dword = 0x30003;
    controller.write_bar0(0x24, dword.as_bytes()).unwrap();

    // Enable the controller.
    controller.read_bar0(0x14, dword.as_mut_bytes()).unwrap();
    dword |= 1;
    controller.write_bar0(0x14, dword.as_bytes()).unwrap();
    controller.read_bar0(0x14, dword.as_mut_bytes()).unwrap();
    assert!(dword & 1 != 0);

    // Read CSTS
    controller.read_bar0(0x1c, dword.as_mut_bytes()).unwrap();
    assert!(dword & 2 == 0);

    // Find the PCI Express capability
    let pcie_cap_offset = find_pci_capability(&mut controller, CapabilityId::PCI_EXPRESS.0);

    let pcie_cap_offset = pcie_cap_offset.expect("PCI Express capability should be present");

    // Read Device Control/Status register to get initial state
    let device_ctl_sts_offset = pcie_cap_offset + PciExpressCapabilityHeader::DEVICE_CTL_STS.0;
    let mut initial_ctl_sts = 0u32;
    controller
        .pci_cfg_read(device_ctl_sts_offset, &mut initial_ctl_sts)
        .unwrap();

    // Trigger FLR by setting the Initiate Function Level Reset bit (bit 15 in Device Control)
    let flr_bit = 1u32 << 15;
    let new_ctl_sts = initial_ctl_sts | flr_bit;
    controller
        .pci_cfg_write(device_ctl_sts_offset, new_ctl_sts)
        .unwrap();

    // According to the spec, we must wait at least 100ms after issuing an FLR before accessing the device again.
    PolledTimer::new(&driver)
        .sleep(Duration::from_millis(100))
        .await;

    // The FLR bit should always read 0, even after the reset.
    let mut post_flr_ctl_sts = 0u32;
    controller
        .pci_cfg_read(device_ctl_sts_offset, &mut post_flr_ctl_sts)
        .unwrap();
    assert_eq!(
        post_flr_ctl_sts & flr_bit,
        0,
        "FLR bit should always read 0, even after the reset."
    );

    // Check that the controller is disabled after FLR
    controller.read_bar0(0x14, dword.as_mut_bytes()).unwrap();
    assert!(dword == 0);
}

fn test_memory() -> GuestMemory {
    GuestMemory::allocate(0x10000)
}
