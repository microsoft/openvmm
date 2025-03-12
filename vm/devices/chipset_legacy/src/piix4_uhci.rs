// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PIIX4 - USB configuration

use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pci::PciConfigSpace;
use inspect::InspectMut;
use vmcore::device_state::ChangeDeviceState;

/// PIIX4 (PCI device function 2) - USB configuration (stub)
///
/// See section 3.3 in the PIIX4 data sheet.
///
/// We only minimally support the UHCI controller because it is part of the
/// chipset that we emulate.
///
/// If we wanted to support USB in the future, it is highly unlikely that we
/// would implement it as part of the legacy chipset.
#[derive(Debug, InspectMut)]
#[non_exhaustive] // force the use of `new`
pub struct Piix4UsbUhciStub {}

impl Piix4UsbUhciStub {
    pub fn new() -> Self {
        Self {}
    }
}

impl ChangeDeviceState for Piix4UsbUhciStub {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {}
}

impl ChipsetDevice for Piix4UsbUhciStub {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }
}

/// Sidestep the config space emulator, and match legacy stub behavior directly
impl PciConfigSpace for Piix4UsbUhciStub {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        use pci_core::spec::cfg_space::HeaderType00;
        *value = match HeaderType00(offset) {
            HeaderType00::BIST_HEADER => 0,
            HeaderType00::BAR4 => 0,
            HeaderType00::STATUS_COMMAND => 0x02800000,
            // Always return the default value, which indicates
            // that the controller is hardwired to PCI IRQ Lane D (i.e: 4).
            HeaderType00::LATENCY_INTERRUPT => 0x000000FF | (4 << 8),
            // Return an invalid value so UHCI controller is ignored by the BIOS and
            // the OS. On a real implementation, the correct value would be 0x71128086.
            HeaderType00::DEVICE_VENDOR => 0xFFFFFFFF,
            // Return zero so UHCI controller is ignored by the BIOS and the OS.
            // On a real implementation, the correct value would be 0x0C030008.
            HeaderType00::CLASS_REVISION => 0x00000000,
            _ if offset < 0x40 => 0, // stub-out all other standard cfg regs
            _ => {
                tracing::debug!(?offset, "unimplemented config space read");
                return IoResult::Err(IoError::InvalidRegister);
            }
        };

        IoResult::Ok
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        use pci_core::spec::cfg_space::HeaderType00;
        match HeaderType00(offset) {
            HeaderType00::BIST_HEADER => {}
            HeaderType00::BAR4 => {}
            HeaderType00::STATUS_COMMAND => {}
            HeaderType00::LATENCY_INTERRUPT => {}
            HeaderType00::DEVICE_VENDOR => {}
            _ => {
                tracing::debug!(?offset, ?value, "unimplemented config space write");
                return IoResult::Err(IoError::InvalidRegister);
            }
        }

        IoResult::Ok
    }

    fn suggested_bdf(&mut self) -> Option<(u8, u8, u8)> {
        Some((0, 7, 2)) // as per PIIX4 spec
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::NoSavedState;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    // This is a stub device, with no saved state
    impl SaveRestore for Piix4UsbUhciStub {
        type SavedState = NoSavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(NoSavedState)
        }

        fn restore(&mut self, NoSavedState: Self::SavedState) -> Result<(), RestoreError> {
            Ok(())
        }
    }
}
