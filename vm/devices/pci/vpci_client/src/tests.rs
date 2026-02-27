// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unit tests.

#![cfg(test)]

use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use chipset_device::pci::PciConfigSpace;
use closeable_mutex::CloseableMutex;
use guestmem::GuestMemory;
use guid::Guid;
use openhcl_tdisp::TdispVirtualDeviceInterface;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::task::Spawn;
use std::sync::Arc;
use task_control::StopTask;
use tdisp::TdispHostDeviceTargetEmulator;
use tdisp::test_helpers::TDISP_MOCK_DEVICE_ID;
use tdisp::test_helpers::TDISP_MOCK_GUEST_PROTOCOL;
use tdisp::test_helpers::TDISP_MOCK_SUPPORTED_FEATURES;
use tdisp::test_helpers::make_null_tdisp_interface;
use test_with_tracing::test;
use vmbus_channel::simple::SimpleVmbusDevice;
use vmcore::vpci_msi::MapVpciInterrupt;
use vmcore::vpci_msi::MsiAddressData;
use vmcore::vpci_msi::VpciInterruptMapper;
use vmcore::vpci_msi::VpciInterruptParameters;
use vpci::bus::VpciBusDevice;
use vpci::test_helpers::TestVpciInterruptController;

struct NoopDevice {
    tdisp_interface: TdispHostDeviceTargetEmulator,
}

impl ChipsetDevice for NoopDevice {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }

    fn supports_tdisp(&mut self) -> Option<&mut dyn tdisp::TdispHostDeviceTarget> {
        Some(&mut self.tdisp_interface)
    }
}

impl PciConfigSpace for NoopDevice {
    fn pci_cfg_read(&mut self, _offset: u16, value: &mut u32) -> IoResult {
        *value = 0;
        IoResult::Ok
    }

    fn pci_cfg_write(&mut self, _offset: u16, _value: u32) -> IoResult {
        IoResult::Ok
    }
}

struct BusWrapper(VpciBusDevice);

impl super::MemoryAccess for BusWrapper {
    fn gpa(&mut self) -> u64 {
        0x123456780000
    }

    fn read(&mut self, addr: u64) -> u32 {
        let mut data = [0; 4];
        self.0
            .supports_mmio()
            .unwrap()
            .mmio_read(addr, &mut data)
            .unwrap();
        u32::from_ne_bytes(data)
    }

    fn write(&mut self, addr: u64, value: u32) {
        self.0
            .supports_mmio()
            .unwrap()
            .mmio_write(addr, &value.to_ne_bytes())
            .unwrap();
    }
}

fn make_noop_device() -> Arc<CloseableMutex<NoopDevice>> {
    Arc::new(CloseableMutex::new(NoopDevice {
        tdisp_interface: make_null_tdisp_interface("vpci-unit-test"),
    }))
}

#[async_test]
async fn test_negotiate_version(driver: DefaultDriver) {
    let device = make_noop_device();
    let msi_controller = TestVpciInterruptController::new();
    let (bus, mut channel) = VpciBusDevice::new(
        Guid::new_random(),
        device,
        &mut ExternallyManagedMmioIntercepts,
        VpciInterruptMapper::new(msi_controller),
        None,
    )
    .unwrap();

    let (host, guest) = vmbus_channel::connected_async_channels(32768);

    let mut runner = channel.open(host, GuestMemory::empty()).unwrap();
    let _task = driver.spawn("server", async move {
        StopTask::run_with(std::future::pending(), async |stop| {
            let _ = channel.run(stop, &mut runner).await;
        })
        .await
    });

    let (_client, devices) =
        super::VpciClient::connect(&driver, guest, Box::new(BusWrapper(bus)), mesh::channel().0)
            .await
            .unwrap();

    let (device, _removed) = devices.into_iter().next().unwrap().init().await.unwrap();
    let MsiAddressData { address, data } = device
        .register_interrupt(
            1,
            &VpciInterruptParameters {
                vector: 5,
                multicast: false,
                target_processors: &[1, 2, 3],
            },
        )
        .await
        .unwrap();

    assert_eq!(device.read_cfg(256), 0);

    device.unregister_interrupt(address, data).await;
}

/// Tests that VPCI can negotiate basic TDISP commands with a device.
/// This test covers:
/// - VMBUS VPCI packet serialization for VpciTdispCommand
/// - TDISP command serialization
/// - VPCI VMBUS server interface receiving and responding to TDISP commands
/// - VPCI VMBUS client interface sending and receiving TDISP commands
/// - Basic TDISP state machine processing
#[async_test]
async fn test_tdisp_interface_get_device_interface_info(driver: DefaultDriver) {
    let device = make_noop_device();
    let msi_controller = TestVpciInterruptController::new();
    let (bus, mut channel) = VpciBusDevice::new(
        Guid::new_random(),
        device,
        &mut ExternallyManagedMmioIntercepts,
        VpciInterruptMapper::new(msi_controller),
        None,
    )
    .unwrap();

    let (host, guest) = vmbus_channel::connected_async_channels(32768);

    let mut runner = channel.open(host, GuestMemory::empty()).unwrap();
    let _task = driver.spawn("server", async move {
        StopTask::run_with(std::future::pending(), async |stop| {
            let _ = channel.run(stop, &mut runner).await;
        })
        .await
    });

    let (_client, devices) =
        super::VpciClient::connect(&driver, guest, Box::new(BusWrapper(bus)), mesh::channel().0)
            .await
            .unwrap();

    let (device, _removed) = devices.into_iter().next().unwrap().init().await.unwrap();
    let interface = device.tdisp_get_device_interface_info().await;

    match interface {
        Ok(interface) => {
            assert_eq!(
                interface.guest_protocol_type,
                TDISP_MOCK_GUEST_PROTOCOL as i32
            );
            assert_eq!(interface.supported_features, TDISP_MOCK_SUPPORTED_FEATURES);
            assert_eq!(interface.tdisp_device_id, TDISP_MOCK_DEVICE_ID);
        }
        Err(err) => panic!("unexpected error: {err}"),
    }
}
