use crate::NvmeController;
use crate::NvmeControllerCaps;
use crate::NvmeControllerClient;
use crate::spec;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pci::PciConfigSpace;
use guestmem::GuestMemory;
use inspect::Inspect;
use pci_core::msi::RegisterMsi;
use std::any::Any;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateNotSupported;
use vmcore::vm_task::VmTaskDriverSource;

// The function can respond with two types of actions.
pub enum FaultInjectionAction {
    /// No fault injection.
    Continue,
    /// Inject a fault that will cause the operation to fail.
    Return,
}

#[derive(Inspect)]
pub struct NvmeControllerFaultInjection {
    #[inspect(skip)]
    inner: NvmeController,
    /// Fault injection callback for NVMe controller operations
    #[inspect(skip)]
    fi: Box<dyn Fn(&str, Vec<Box<dyn Any>>, FaultInjectionAction) -> u32 + Send + Sync>,
}

impl NvmeControllerFaultInjection {
    /// Creates a new NVMe controller with fault injection.
    pub fn new(
        driver_source: &VmTaskDriverSource,
        guest_memory: GuestMemory,
        register_msi: &mut dyn RegisterMsi,
        register_mmio: &mut dyn RegisterMmioIntercept,
        caps: NvmeControllerCaps,
        fi: Box<dyn Fn(&str, Vec<Box<dyn Any>>, FaultInjectionAction) -> u32 + Send + Sync>,
    ) -> Self {
        Self {
            inner: NvmeController::new(
                driver_source,
                guest_memory,
                register_msi,
                register_mmio,
                caps,
            ),
            fi,
        }
    }

    /// Returns a client for manipulating the NVMe controller at runtime.
    pub fn client(&self) -> NvmeControllerClient {
        self.inner.client()
    }

    /// Reads from the virtual BAR 0.
    pub fn read_bar0(&mut self, addr: u16, data: &mut [u8]) -> IoResult {
        self.inner.read_bar0(addr, data)
    }

    /// Writes to the virtual BAR 0.
    pub fn write_bar0(&mut self, addr: u16, data: &[u8]) -> IoResult {
        self.inner.write_bar0(addr, data)
    }

    pub fn fatal_error(&mut self) {
        self.inner.fatal_error();
    }
}

impl ChangeDeviceState for NvmeControllerFaultInjection {
    fn start(&mut self) {
        self.inner.start();
    }

    async fn stop(&mut self) {
        self.inner.stop().await;
    }

    async fn reset(&mut self) {
        self.inner.reset().await;
    }
}

impl ChipsetDevice for NvmeControllerFaultInjection {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        self.inner.supports_mmio()
    }

    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        self.inner.supports_pci()
    }
}

impl MmioIntercept for NvmeControllerFaultInjection {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        self.inner.mmio_read(addr, data)
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        self.inner.mmio_write(addr, data)
    }
}

impl PciConfigSpace for NvmeControllerFaultInjection {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        self.inner.pci_cfg_read(offset, value)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        self.inner.pci_cfg_write(offset, value)
    }
}

impl SaveRestore for NvmeControllerFaultInjection {
    type SavedState = SavedStateNotSupported;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        self.inner.save()
    }

    fn restore(
        &mut self,
        state: Self::SavedState,
    ) -> Result<(), vmcore::save_restore::RestoreError> {
        self.inner.restore(state)
    }
}
