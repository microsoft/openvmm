#![cfg(test)]

use async_trait::async_trait;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use chipset_device::pci::PciConfigSpace;
use closeable_mutex::CloseableMutex;
use futures::StreamExt;
use guestmem::GuestMemory;
use guid::Guid;
use mesh::rpc::RpcSend;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::task::Spawn;
use parking_lot::Mutex;
use std::sync::Arc;
use std::task::Context;
use task_control::StopTask;
use test_with_tracing::test;
use vmbus_async::pipe::MessagePipe;
use vmbus_channel::SignalVmbusChannel;
use vmbus_channel::bus::OfferInput;
use vmbus_channel::bus::OpenData;
use vmbus_channel::bus::OpenRequest;
use vmbus_channel::bus::ParentBus;
use vmbus_channel::simple::SimpleVmbusDevice;
use vmbus_core::protocol::GpadlId;
use vmbus_core::protocol::UserDefinedData;
use vmbus_ring::IncomingRing;
use vmcore::interrupt::Interrupt;
use vmcore::notify::Notify;
use vmcore::notify::PolledNotify;
use vmcore::slim_event::SlimEvent;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;
use vmcore::vpci_msi::MapVpciInterrupt;
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

struct MmioGuy {}

impl super::MemoryAccess for VpciBusDevice {
    fn gpa(&mut self) -> u64 {
        0x123456780000
    }

    fn read(&mut self, offset: u64) -> u32 {
        let mut data = [0; 4];
        self.supports_mmio()
            .unwrap()
            .mmio_read(offset, &mut data)
            .unwrap();
        u32::from_ne_bytes(data)
    }

    fn write(&mut self, offset: u64, value: u32) {
        self.supports_mmio()
            .unwrap()
            .mmio_write(offset, &value.to_ne_bytes())
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

    let (devices_send, mut devices_recv) = mesh::channel();

    let vpci = super::VpciClient::connect(&driver, guest, Box::new(bus), devices_send)
        .await
        .unwrap();

    let device = devices_recv.next().await.unwrap();
    device
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
}
