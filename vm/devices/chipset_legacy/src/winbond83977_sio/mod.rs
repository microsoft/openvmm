// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Combo Floppy controller + SuperIO config controller, as specified by the
//! Winbond W83977ATF SIO chipset.
//!
//! SIO Extended Function registers are shared with floppy disk controller
//! registers. IO port reads/writes are forwarded to SIO config controller when
//! the chipset is in config mode and are forwarded to the FDC otherwise.

#![warn(missing_docs)]

pub use self::maybe_floppy_disk_controller::MaybeStubFloppyDiskController;

use self::super_io::SioController;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pio::PortIoIntercept;
use chipset_device::pio::RegisterPortIoIntercept;
use chipset_device::poll_device::PollDevice;
use chipset_device::ChipsetDevice;
use floppy::DriveRibbon;
use guestmem::GuestMemory;
use inspect::InspectMut;
use std::task::Context;
use thiserror::Error;
use vmcore::device_state::ChangeDeviceState;
use vmcore::isa_dma_channel::IsaDmaChannel;
use vmcore::line_interrupt::LineInterrupt;

mod super_io;

const PRI_FLOPPY_BASE_ADDR: u16 = 0x3F0;
const SEC_FLOPPY_BASE_ADDR: u16 = 0x370;

const PRI_EXT_FUNC_ENABLE_REG: u16 = 0x3F0;
const SEC_EXT_FUNC_ENABLE_REG: u16 = 0x370;
const PRI_EXT_FUNC_DATA_REG: u16 = 0x3F1;
const SEC_EXT_FUNC_DATA_REG: u16 = 0x371;

/// Combo Floppy controller + SuperIO config controller, as specified by the
/// Winbond W83977ATF SIO chipset.
///
/// DEVNOTE: This device simply *aggregates* multiple sub-devices into a single
/// `ChipsetDevice`, accounting for weird quirks like overlapping port-io and
/// shared interrupt lines.
///
/// Notably: this device contains no additional volatile state that needs to be
/// saved/restored, outside of the state of its sub-devices.
#[derive(InspectMut)]
pub struct Winbond83977FloppySioDevice<FDC: MaybeStubFloppyDiskController> {
    // Sub-emulators
    #[inspect(mut)]
    sio: SioController,
    #[inspect(mut)]
    primary_fdc: FDC,
    #[inspect(mut)]
    secondary_fdc: FDC,
}

#[derive(Debug, Error)]
#[expect(missing_docs)]
pub enum NewWinbond83977FloppySioDeviceError<FdcError> {
    #[error("failed to share interrupt line")]
    LineShare(#[source] vmcore::line_interrupt::NewLineError),
    #[error("failed to init primary floppy controller")]
    BadPrimaryFdc(#[source] FdcError),
    #[error("failed to init secondary floppy controller")]
    BadSecondaryFdc(#[source] FdcError),
}

impl<FDC: MaybeStubFloppyDiskController> Winbond83977FloppySioDevice<FDC> {
    /// Create a new `Winbond83977FloppySioDevice`
    pub fn new(
        guest_memory: GuestMemory,
        interrupt: LineInterrupt,
        register_pio: &mut dyn RegisterPortIoIntercept,
        primary_disk_drive: DriveRibbon,
        secondary_disk_drive: DriveRibbon,
        primary_dma: Box<dyn IsaDmaChannel>,
        secondary_dma: Box<dyn IsaDmaChannel>,
    ) -> Result<Self, NewWinbond83977FloppySioDeviceError<FDC::NewError>> {
        let secondary_interrupt = interrupt
            .new_shared("floppy secondary")
            .map_err(NewWinbond83977FloppySioDeviceError::LineShare)?;

        Ok(Self {
            sio: SioController::default(),
            primary_fdc: FDC::new(
                guest_memory.clone(),
                interrupt,
                register_pio,
                PRI_FLOPPY_BASE_ADDR,
                primary_disk_drive,
                primary_dma,
            )
            .map_err(NewWinbond83977FloppySioDeviceError::BadPrimaryFdc)?,
            secondary_fdc: FDC::new(
                guest_memory,
                secondary_interrupt,
                register_pio,
                SEC_FLOPPY_BASE_ADDR,
                secondary_disk_drive,
                secondary_dma,
            )
            .map_err(NewWinbond83977FloppySioDeviceError::BadSecondaryFdc)?,
        })
    }
}

impl<FDC: MaybeStubFloppyDiskController> ChangeDeviceState for Winbond83977FloppySioDevice<FDC> {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.sio.reset().await;
        self.primary_fdc.reset().await;
        self.secondary_fdc.reset().await;
    }
}

impl<FDC: MaybeStubFloppyDiskController> ChipsetDevice for Winbond83977FloppySioDevice<FDC> {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl<FDC: MaybeStubFloppyDiskController> PollDevice for Winbond83977FloppySioDevice<FDC> {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        self.primary_fdc.poll_device(cx);
        self.secondary_fdc.poll_device(cx);
    }
}

impl<FDC: MaybeStubFloppyDiskController> PortIoIntercept for Winbond83977FloppySioDevice<FDC> {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        if data.len() != 1 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        if io_port == PRI_EXT_FUNC_DATA_REG || io_port == SEC_EXT_FUNC_DATA_REG {
            // If read from SIO extended function register fails then fall back
            // to reading from floppy port.
            if let Ok(value) = self.sio.config_read() {
                data[0] = value;
                return IoResult::Ok;
            }
        }

        if self.primary_fdc.offset_of(io_port).is_some() {
            return self.primary_fdc.io_read(io_port, data);
        }

        if self.secondary_fdc.offset_of(io_port).is_some() {
            return self.secondary_fdc.io_read(io_port, data);
        }

        IoResult::Err(IoError::InvalidRegister)
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        if data.len() != 1 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        // Floppy controller STATUS_A (offset = 0) and STATUS_B (offset = 1) registers
        // are read-only so forward all writes to these registers to SIO controller.
        match io_port {
            PRI_EXT_FUNC_ENABLE_REG | SEC_EXT_FUNC_ENABLE_REG => {
                self.sio.update_config_state(data[0]);
            }
            PRI_EXT_FUNC_DATA_REG | SEC_EXT_FUNC_DATA_REG => self.sio.config_write(data[0]),
            _ => {
                if self.primary_fdc.offset_of(io_port).is_some() {
                    return self.primary_fdc.io_write(io_port, data);
                }

                if self.secondary_fdc.offset_of(io_port).is_some() {
                    return self.secondary_fdc.io_write(io_port, data);
                }

                return IoResult::Err(IoError::InvalidRegister);
            }
        }

        IoResult::Ok
    }
}

mod maybe_floppy_disk_controller {
    use chipset_device::pio::PortIoIntercept;
    use chipset_device::pio::RegisterPortIoIntercept;
    use chipset_device::poll_device::PollDevice;
    use chipset_device::ChipsetDevice;
    use floppy::DriveRibbon;
    use guestmem::GuestMemory;
    use inspect::InspectMut;
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::isa_dma_channel::IsaDmaChannel;
    use vmcore::line_interrupt::LineInterrupt;

    /// Trait that abstracts over different floppy controller implementations.
    ///
    /// This trait allows re-using the same code for both the fully-featured floppy
    /// controller, and the stub floppy controller (which is required when booting
    /// via BIOS using the Microsoft PCAT firmware blob).
    pub trait MaybeStubFloppyDiskController:
        Sized
        + ChipsetDevice
        + ChangeDeviceState
        + InspectMut
        + PollDevice
        + PortIoIntercept
        + vmcore::save_restore::SaveRestore
    {
        /// Error type returned by `new()`
        type NewError: std::error::Error + Send + Sync + 'static;

        /// Create a new `FloppyDiskController`
        fn new(
            guest_memory: GuestMemory,
            interrupt: LineInterrupt,
            register_pio: &mut dyn RegisterPortIoIntercept,
            pio_base_addr: u16,
            disk_drive: DriveRibbon,
            dma: Box<dyn IsaDmaChannel>,
        ) -> Result<Self, Self::NewError>;

        /// Return the offset of the given IO port, if it is handled by this
        /// device.
        fn offset_of(&self, io_port: u16) -> Option<u16>;
    }

    impl MaybeStubFloppyDiskController for floppy::FloppyDiskController {
        type NewError = floppy::NewFloppyDiskControllerError;

        fn new(
            guest_memory: GuestMemory,
            interrupt: LineInterrupt,
            register_pio: &mut dyn RegisterPortIoIntercept,
            pio_base_addr: u16,
            disk_drive: DriveRibbon,
            dma: Box<dyn IsaDmaChannel>,
        ) -> Result<Self, Self::NewError> {
            Self::new(
                guest_memory,
                interrupt,
                register_pio,
                pio_base_addr,
                disk_drive,
                dma,
            )
        }

        fn offset_of(&self, io_port: u16) -> Option<u16> {
            self.offset_of(io_port)
        }
    }

    impl MaybeStubFloppyDiskController for floppy_pcat_stub::StubFloppyDiskController {
        type NewError = std::convert::Infallible;

        fn new(
            _guest_memory: GuestMemory,
            interrupt: LineInterrupt,
            register_pio: &mut dyn RegisterPortIoIntercept,
            pio_base_addr: u16,
            _disk_drive: DriveRibbon,
            _dma: Box<dyn IsaDmaChannel>,
        ) -> Result<Self, Self::NewError> {
            Ok(Self::new(interrupt, register_pio, pio_base_addr))
        }

        fn offset_of(&self, io_port: u16) -> Option<u16> {
            self.offset_of(io_port)
        }
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use super::super_io::SioController;
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SaveRestore;
        use vmcore::save_restore::SavedStateRoot;

        // would be nice to call this "chipset.winbond83977_superio_stub", but
        // we can't easily change it without breaking compat with earlier
        // underhill revisions
        //
        // in the future, there will be more robust saved state infrastructure
        // that would allow these sorts of transforms... but that code doesn't
        // exist just yet.
        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "chipset.winbond83977_superio", rename = "SavedState")]
        pub struct StubSavedState {
            #[mesh(1)]
            pub floppy1: <floppy_pcat_stub::StubFloppyDiskController as SaveRestore>::SavedState,
            #[mesh(2)]
            pub floppy2: <floppy_pcat_stub::StubFloppyDiskController as SaveRestore>::SavedState,
            #[mesh(3)]
            pub sio: <SioController as SaveRestore>::SavedState,
        }

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "chipset.winbond83977_superio_nonstub")]
        pub struct FullSavedState {
            #[mesh(1)]
            pub floppy1: <floppy::FloppyDiskController as SaveRestore>::SavedState,
            #[mesh(2)]
            pub floppy2: <floppy::FloppyDiskController as SaveRestore>::SavedState,
            #[mesh(3)]
            pub sio: <SioController as SaveRestore>::SavedState,
        }
    }

    macro_rules! impl_save_restore {
        ($saved_sate:ident, $ty:path) => {
            impl SaveRestore for Winbond83977FloppySioDevice<$ty> {
                type SavedState = state::$saved_sate;

                fn save(&mut self) -> Result<Self::SavedState, SaveError> {
                    let saved_state = state::$saved_sate {
                        floppy1: self.primary_fdc.save()?,
                        floppy2: self.secondary_fdc.save()?,
                        sio: self.sio.save()?,
                    };
                    Ok(saved_state)
                }

                fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
                    let state::$saved_sate {
                        floppy1,
                        floppy2,
                        sio,
                    } = state;

                    self.primary_fdc.restore(floppy1)?;
                    self.secondary_fdc.restore(floppy2)?;
                    self.sio.restore(sio)?;
                    Ok(())
                }
            }
        };
    }

    impl_save_restore!(StubSavedState, floppy_pcat_stub::StubFloppyDiskController);
    impl_save_restore!(FullSavedState, floppy::FloppyDiskController);
}
