#![expect(missing_docs)]

use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::pci::PciConfigSpace;
use pal_async::driver::SpawnDriver;
use std::sync::Arc;
use user_driver::DmaClient;
use vmbus_client::ConnectResult;
use vmbus_client::local_use::Input;
use vpci_client::VpciDevice;

pub struct VpciRelay {}

impl VpciRelay {}

pub fn foo(connection: ConnectResult) {
    for offer in connection.offers {
        do_it(offer);
    }
}

async fn do_it(
    driver: impl SpawnDriver + Clone,
    offer: vmbus_client::OfferInfo,
    dma_client: &dyn DmaClient,
) {
    let channel = vmbus_client::local_use::open_channel(
        driver,
        offer,
        Input {
            ring_pages: 20,
            ring_offset_in_pages: 10,
        },
        dma_client,
    )
    .await?;
    let vpci_client = vpci_client::VpciClient::connect(&driver, channel, mmio, devices).await?;
    let relayed_device = RelayedVpciDevice(todo!());
}

pub struct RelayedVpciDevice(Arc<VpciDevice>);

impl RelayedVpciDevice {
    pub fn new(device: Arc<VpciDevice>) -> Self {
        Self(device)
    }
}

impl ChipsetDevice for RelayedVpciDevice {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }
}

impl PciConfigSpace for RelayedVpciDevice {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        *value = self.0.read_cfg(offset);
        IoResult::Ok
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        self.0.write_cfg(offset, value);
        IoResult::Ok
    }
}
