#![expect(missing_docs)]

use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::pci::PciConfigSpace;
use std::sync::Arc;
use vpci_client::VpciDevice;

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
