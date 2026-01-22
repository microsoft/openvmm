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
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::task::Spawn;
use std::sync::Arc;
use task_control::StopTask;
use test_with_tracing::test;
use vmbus_channel::simple::SimpleVmbusDevice;
use vmcore::vpci_msi::MapVpciInterrupt;
use vmcore::vpci_msi::MsiAddressData;
use vmcore::vpci_msi::VpciInterruptMapper;
use vmcore::vpci_msi::VpciInterruptParameters;
use vpci::bus::VpciBusDevice;
use vpci::test_helpers::TestVpciInterruptController;

struct NoopDevice;

impl ChipsetDevice for NoopDevice {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
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

#[async_test]
async fn test_negotiate_version(driver: DefaultDriver) {
    let device = Arc::new(CloseableMutex::new(NoopDevice));
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
