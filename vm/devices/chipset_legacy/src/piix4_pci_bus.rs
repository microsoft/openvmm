// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PIIX4 - PCI bus

use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::pio::PortIoIntercept;
use chipset_device::pio::RegisterPortIoIntercept;
use chipset_device::poll_device::PollDevice;
use inspect::InspectMut;
use pci_bus::GenericPciBus;
use vmcore::device_state::ChangeDeviceState;

/// IO ports as specified by the PIIX4 data sheet
mod io_ports {
    pub const PCI_ADDR_START: u16 = 0xCF8;
    pub const RESET_CF9: u16 = 0xCF9; // it's just sandwiched in there
    pub const PCI_DATA_START: u16 = 0xCFC;
}

/// The PCI bus as implemented on the PIIX4 chipset.
///
/// Identical to a standard PCI bus, aside from the addition of the `RESET_CF9`
/// register, because for _some reason_, _someone_ thought it'd be a great idea
/// to throw a one-byte register that performs a machine reset *right in the
/// middle of the PCI addr register* >:(
#[derive(InspectMut)]
pub struct Piix4PciBus {
    // Runtime glue
    #[inspect(skip)]
    reset_evt: Box<dyn Fn() + Send + Sync>,

    // Sub-emulator
    #[inspect(mut)]
    bus: GenericPciBus,
}

impl Piix4PciBus {
    /// Create a new [`Piix4PciBus`]
    pub fn new(
        register_pio: &mut dyn RegisterPortIoIntercept,
        reset_evt: Box<dyn Fn() + Send + Sync>,
    ) -> Self {
        Piix4PciBus {
            reset_evt,
            bus: GenericPciBus::new(
                register_pio,
                io_ports::PCI_ADDR_START,
                io_ports::PCI_DATA_START,
            ),
        }
    }

    /// bypass the PIIX4 specific stuff, and get a handle to the underlying PCI
    /// bus implementation
    pub fn as_pci_bus(&mut self) -> &mut GenericPciBus {
        &mut self.bus
    }

    fn handle_reset_cf9_read(&mut self, data: &mut [u8]) {
        if data.len() != 1 {
            tracelimit::warn_ratelimited!(len = ?data.len(), "unexpected RESET_CF9 read len");
            return;
        }

        tracelimit::warn_ratelimited!("read from the RESET_CF9 io port");
        data[0] = 0;
    }

    fn handle_reset_cf9_write(&mut self, data: &[u8]) {
        if data.len() != 1 {
            tracelimit::warn_ratelimited!(len = ?data.len(), "unexpected RESET_CF9 write len");
            return;
        }

        if (data[0] & 0x6) != 0 {
            tracing::info!("initiating guest reset via RESET_CF9");
            (self.reset_evt)();
        }
    }
}

impl ChangeDeviceState for Piix4PciBus {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.bus.reset().await;
    }
}

impl ChipsetDevice for Piix4PciBus {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self.as_pci_bus())
    }
}

impl PortIoIntercept for Piix4PciBus {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        if data.len() == 1 && io_port == io_ports::RESET_CF9 {
            self.handle_reset_cf9_read(data);
            return IoResult::Ok;
        }

        self.bus.io_read(io_port, data)
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        if data.len() == 1 && io_port == io_ports::RESET_CF9 {
            self.handle_reset_cf9_write(data);
            return IoResult::Ok;
        }

        self.bus.io_write(io_port, data)
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use pci_bus::GenericPciBus;
        use vmcore::save_restore::SaveRestore;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "chipset.piix4.pci_bus")]
        pub struct SavedState {
            #[mesh(1)]
            pub bus: <GenericPciBus as SaveRestore>::SavedState,
        }
    }

    impl SaveRestore for Piix4PciBus {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let Piix4PciBus { reset_evt: _, bus } = self;

            let saved_state = state::SavedState { bus: bus.save()? };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState { bus } = state;

            self.bus.restore(bus)?;

            Ok(())
        }
    }
}
