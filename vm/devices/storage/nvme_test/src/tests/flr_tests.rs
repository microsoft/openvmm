// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for Function Level Reset (FLR) functionality.

use super::test_helpers::TestNvmeMmioRegistration;
use crate::FaultConfiguration;
use crate::NvmeFaultController;
use crate::NvmeFaultControllerCaps;
use chipset_device::pci::PciConfigSpace;
use guestmem::GuestMemory;
use guid::Guid;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pci_core::msi::MsiInterruptSet;
use pci_core::spec::caps::CapabilityId;
use pci_core::spec::caps::pci_express::PciExpressCapabilityHeader;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;

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
        FaultConfiguration { admin_fault: None },
    )
}

#[async_test]
async fn test_flr_capability_advertised(driver: DefaultDriver) {
    let gm = test_memory();
    let mut controller = instantiate_controller_with_flr(driver, &gm, true);

    // Find the PCI Express capability
    let mut cap_ptr = 0x40u16; // Standard capabilities start at 0x40
    let mut found_pcie_cap = false;

    // Walk through capabilities list
    for _ in 0..16 {
        // Reasonable limit on capability chain length
        let mut cap_header = 0u32;
        controller.pci_cfg_read(cap_ptr, &mut cap_header).unwrap();

        let cap_id = (cap_header & 0xFF) as u8;
        let next_ptr = ((cap_header >> 8) & 0xFF) as u16;

        if cap_id == CapabilityId::PCI_EXPRESS.0 {
            found_pcie_cap = true;

            // Read Device Capabilities register to check FLR support
            let mut device_caps = 0u32;
            controller
                .pci_cfg_read(
                    cap_ptr + PciExpressCapabilityHeader::DEVICE_CAPS.0,
                    &mut device_caps,
                )
                .unwrap();

            // Check Function Level Reset bit (bit 29, not 28)
            let flr_supported = (device_caps & (1 << 29)) != 0;
            assert!(
                flr_supported,
                "FLR should be advertised in Device Capabilities"
            );
            break;
        }

        if next_ptr == 0 {
            break;
        }
        cap_ptr = next_ptr;
    }

    assert!(
        found_pcie_cap,
        "PCI Express capability should be present when FLR is enabled"
    );
}

#[async_test]
async fn test_no_flr_capability_when_disabled(driver: DefaultDriver) {
    let gm = test_memory();
    let mut controller = instantiate_controller_with_flr(driver, &gm, false);

    // Find the PCI Express capability - it should not be present
    let mut cap_ptr = 0x40u16; // Standard capabilities start at 0x40
    let mut found_pcie_cap = false;

    // Walk through capabilities list
    for _ in 0..16 {
        // Reasonable limit on capability chain length
        let mut cap_header = 0u32;
        controller.pci_cfg_read(cap_ptr, &mut cap_header).unwrap();

        let cap_id = (cap_header & 0xFF) as u8;
        let next_ptr = ((cap_header >> 8) & 0xFF) as u16;

        if cap_id == CapabilityId::PCI_EXPRESS.0 {
            found_pcie_cap = true;
            break;
        }

        if next_ptr == 0 {
            break;
        }
        cap_ptr = next_ptr;
    }

    assert!(
        !found_pcie_cap,
        "PCI Express capability should not be present when FLR is disabled"
    );
}

#[async_test]
async fn test_flr_trigger(driver: DefaultDriver) {
    let gm = test_memory();
    let mut controller = instantiate_controller_with_flr(driver, &gm, true);

    // Find the PCI Express capability
    let mut cap_ptr = 0x40u16; // Standard capabilities start at 0x40
    let mut pcie_cap_offset = None;

    // Walk through capabilities list
    for _ in 0..16 {
        // Reasonable limit on capability chain length
        let mut cap_header = 0u32;
        controller.pci_cfg_read(cap_ptr, &mut cap_header).unwrap();

        let cap_id = (cap_header & 0xFF) as u8;
        let next_ptr = ((cap_header >> 8) & 0xFF) as u16;

        if cap_id == CapabilityId::PCI_EXPRESS.0 {
            pcie_cap_offset = Some(cap_ptr);
            break;
        }

        if next_ptr == 0 {
            break;
        }
        cap_ptr = next_ptr;
    }

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

    // The FLR bit should be self-clearing, so read it back to verify
    let mut post_flr_ctl_sts = 0u32;
    controller
        .pci_cfg_read(device_ctl_sts_offset, &mut post_flr_ctl_sts)
        .unwrap();

    // The FLR bit should be cleared now
    assert_eq!(
        post_flr_ctl_sts & flr_bit,
        0,
        "FLR bit should be self-clearing"
    );

    // The device should be reset - check that controller status reflects reset state
    // Note: In a real implementation, we'd need to check that the device actually reset,
    // but for this test, we just verify the FLR trigger mechanism works
}

fn test_memory() -> GuestMemory {
    GuestMemory::allocate(0x10000)
}
