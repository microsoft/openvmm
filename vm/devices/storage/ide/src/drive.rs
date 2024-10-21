// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements ATA/ATAPI disk drives
//!

mod atapi_drive;
mod hard_drive;

use crate::DmaType;
use crate::DriveMedia;
use crate::DriveType;
use crate::NewDeviceError;
use atapi_drive::AtapiDrive;
use guestmem::GuestMemory;
use hard_drive::HardDrive;
use ide_resources::IdePath;
use inspect::Inspect;
use inspect::InspectMut;
use std::fmt::Debug;
use std::task::Context;

#[derive(Debug, Copy, Clone, Inspect)]
pub enum DriveRegister {
    ErrorFeatures,
    SectorCount,
    LbaLow,
    LbaMid,
    LbaHigh,
    DeviceHead,
    StatusCmd,
    AlternateStatusDeviceControl,
}

#[derive(InspectMut)]
#[inspect(tag = "device_type")]
#[inspect(extra = "DiskDrive::inspect_status")]
pub(crate) enum DiskDrive {
    #[inspect(transparent)]
    HardDevice(HardDrive),
    #[inspect(transparent)]
    OpticalDevice(AtapiDrive),
}

impl DiskDrive {
    pub fn new(media: DriveMedia, disk_path: IdePath) -> Result<Self, NewDeviceError> {
        match media {
            DriveMedia::HardDrive(device) => {
                Ok(DiskDrive::HardDevice(HardDrive::new(device, disk_path)?))
            }
            DriveMedia::OpticalDrive(device) => {
                Ok(DiskDrive::OpticalDevice(AtapiDrive::new(device, disk_path)))
            }
        }
    }

    fn inspect_status(&mut self, resp: &mut inspect::Response<'_>) {
        resp.field(
            "status",
            crate::protocol::Status::from_bits(
                self.read_register(DriveRegister::AlternateStatusDeviceControl),
            ),
        );
    }

    pub fn reset(&mut self) {
        match self {
            DiskDrive::HardDevice(device) => device.reset(),
            DiskDrive::OpticalDevice(device) => device.reset(),
        }
    }
    pub fn drive_type(&self) -> DriveType {
        match self {
            DiskDrive::HardDevice(_) => DriveType::Hard,
            DiskDrive::OpticalDevice(_) => DriveType::Optical,
        }
    }
    pub fn handle_read_dma_descriptor_error(&mut self) -> bool {
        match self {
            DiskDrive::HardDevice(device) => device.handle_read_dma_descriptor_error(),
            DiskDrive::OpticalDevice(device) => device.handle_read_dma_descriptor_error(),
        }
    }
    pub fn read_register(&mut self, register: DriveRegister) -> u8 {
        match self {
            DiskDrive::HardDevice(device) => device.read_register(register),
            DiskDrive::OpticalDevice(device) => device.read_register(register),
        }
    }
    pub fn write_register(&mut self, register: DriveRegister, data: u8) {
        match self {
            DiskDrive::HardDevice(device) => device.write_register(register, data),
            DiskDrive::OpticalDevice(device) => device.write_register(register, data),
        }
    }
    pub(crate) fn pio_read(&mut self, data: &mut [u8]) {
        match self {
            DiskDrive::HardDevice(device) => device.pio_read(data),
            DiskDrive::OpticalDevice(device) => device.pio_read(data),
        }
    }
    pub(crate) fn pio_write(&mut self, data: &[u8]) {
        match self {
            DiskDrive::HardDevice(device) => device.pio_write(data),
            DiskDrive::OpticalDevice(device) => device.pio_write(data),
        }
    }
    pub fn interrupt_pending(&self) -> bool {
        match self {
            DiskDrive::HardDevice(device) => device.interrupt_pending(),
            DiskDrive::OpticalDevice(device) => device.interrupt_pending(),
        }
    }
    pub fn dma_request(&self) -> Option<(&DmaType, usize)> {
        match self {
            DiskDrive::HardDevice(device) => device.dma_request(),
            DiskDrive::OpticalDevice(device) => device.dma_request(),
        }
    }
    pub fn dma_transfer(&mut self, guest_memory: &GuestMemory, gpa: u64, len: usize) {
        match self {
            DiskDrive::HardDevice(device) => device.dma_transfer(guest_memory, gpa, len),
            DiskDrive::OpticalDevice(device) => device.dma_transfer(guest_memory, gpa, len),
        }
    }
    pub fn dma_advance_buffer(&mut self, len: usize) {
        match self {
            DiskDrive::HardDevice(device) => device.dma_advance_buffer(len),
            DiskDrive::OpticalDevice(device) => device.dma_advance_buffer(len),
        }
    }
    pub fn set_prd_exhausted(&mut self) {
        match self {
            DiskDrive::HardDevice(device) => device.set_prd_exhausted(),
            DiskDrive::OpticalDevice(_) => {} // Only needed for enlightened operations on hard drives
        }
    }
    pub fn poll_device(&mut self, cx: &mut Context<'_>) {
        match self {
            DiskDrive::HardDevice(device) => device.poll_device(cx),
            DiskDrive::OpticalDevice(device) => device.poll_device(cx),
        }
    }
}

pub mod save_restore {
    use self::state::SavedDriveState;
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;

    pub trait DriveSaveRestore {
        fn save(&self) -> Result<SavedDriveState, SaveError>;
        fn restore(&mut self, state: SavedDriveState) -> Result<(), RestoreError>;
    }

    pub mod state {
        use crate::drive::atapi_drive::save_restore::state::SavedAtapiDriveState;
        use crate::drive::hard_drive::save_restore::state::SavedHardDriveState;
        use mesh::payload::Protobuf;

        #[derive(Protobuf)]
        #[mesh(package = "storage.ide.device")]
        pub enum SavedDriveState {
            #[mesh(1)]
            HardDevice(SavedHardDriveState),
            #[mesh(2)]
            OpticalDevice(SavedAtapiDriveState),
        }
    }

    impl DriveSaveRestore for DiskDrive {
        fn save(&self) -> Result<SavedDriveState, SaveError> {
            match self {
                DiskDrive::HardDevice(device) => Ok(SavedDriveState::HardDevice(device.save()?)),
                DiskDrive::OpticalDevice(device) => {
                    Ok(SavedDriveState::OpticalDevice(device.save()?))
                }
            }
        }
        fn restore(&mut self, state: SavedDriveState) -> Result<(), RestoreError> {
            match self {
                DiskDrive::HardDevice(device) => match state {
                    SavedDriveState::HardDevice(state) => device.restore(state),
                    _ => unreachable!(),
                },
                DiskDrive::OpticalDevice(device) => match state {
                    SavedDriveState::OpticalDevice(state) => device.restore(state),
                    _ => unreachable!(),
                },
            }
        }
    }
}
