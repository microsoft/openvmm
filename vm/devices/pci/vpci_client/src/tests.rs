#![cfg(test)]

use async_trait::async_trait;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use chipset_device::pci::PciConfigSpace;
use closeable_mutex::CloseableMutex;
use guestmem::GuestMemory;
use guid::Guid;
use pal_async::DefaultDriver;
use pal_async::async_test;
use parking_lot::Mutex;
use std::sync::Arc;
use test_with_tracing::test;
use vmbus_channel::bus::OfferInput;
use vmbus_channel::bus::OpenData;
use vmbus_channel::bus::OpenRequest;
use vmbus_channel::bus::ParentBus;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;
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

#[derive(Clone, Default)]
struct VmbusOfferee(Arc<Mutex<Option<OfferInput>>>);

#[async_trait]
impl ParentBus for VmbusOfferee {
    /// Offers a new channel.
    async fn add_child(
        &self,
        request: vmbus_channel::bus::OfferInput,
    ) -> anyhow::Result<vmbus_channel::bus::OfferResources> {
        let mut this = self.0.lock();
        if this.is_some() {
            return Err(anyhow::anyhow!("Already offered"));
        }
        *this = Some(request);
        Ok(vmbus_channel::bus::OfferResources::new(
            GuestMemory::empty(),
            None,
        ))
    }

    fn clone_bus(&self) -> Box<dyn ParentBus> {
        Box::new(self.clone())
    }
}

//#[async_test]
async fn test_negotiate_version(driver: DefaultDriver) {
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
    let device = Arc::new(CloseableMutex::new(NoopDevice));
    let vmbus = VmbusOfferee::default();
    let msi_controller = TestVpciInterruptController::new();
    let server = vpci::bus::VpciBus::new(
        &driver_source,
        Guid::new_random(),
        device,
        &mut ExternallyManagedMmioIntercepts,
        &vmbus,
        msi_controller,
    )
    .await
    .unwrap();

    let params = vmbus.0.lock().take().unwrap();
    params
        .request_send
        .send(vmbus_channel::bus::ChannelRequest::Open(OpenRequest {
            open_data: OpenData {
                target_vp: 0,
                ring_offset: 10,
                ring_gpadl_id: 0,
                event_flag: b,
                connection_id: todo!(),
                user_data: todo!(),
            },
            interrupt: a,
            use_confidential_ring: false,
            use_confidential_external_memory: false,
        }))
        .await;
}
