// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This is a port of the PCI VGA adapter from Hyper-V.
//!
//! It, in turn, was originally an emulator of an S3 Trio, but over time it was
//! defeatured to be a generic SVGA card. It still uses the S3 Trio registers
//! for SVGA mode switching, and it has some extra Hyper-V enlightenments, so it
//! must be paired with the proprietary Hyper-V SVGA BIOS.
//!
//! This code needs a lot of cleanup, and various features need to be completed
//! (especially around supporting different graphics modes). Ultimately, the
//! standard core VGA portion should be split out so that alternate SVGA mode
//! switching can be layered on top (e.g. to support the bochs mode switching
//! interface that SeaVGABios uses).

#![expect(missing_docs)]

mod emu;
mod non_linear;
mod render;
mod spec;
mod text_mode;

use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use chipset_device::pio::PortIoIntercept;
use framebuffer::FramebufferLocalControl;
use guestmem::MapRom;
use inspect::InspectMut;
use render::Renderer;
use thiserror::Error;
use video_core::FramebufferFormat;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateNotSupported;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vmtime::VmTimeSource;

#[derive(InspectMut)]
pub struct VgaDevice {
    #[inspect(flatten)]
    emu: emu::Emulator,
    renderer: Renderer,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to map framebuffer")]
    Framebuffer(#[source] std::io::Error),
}

impl VgaDevice {
    pub fn new(
        driver: &VmTaskDriver,
        vmtime: &VmTimeSource,
        mut control: FramebufferLocalControl,
        rom: Option<Box<dyn MapRom>>,
    ) -> Result<Self, Error> {
        control.set_format(FramebufferFormat {
            width: 800,
            height: 600,
            bytes_per_line: 800 * 4,
            offset: 0,
        });

        let vram = control.memory().map_err(Error::Framebuffer)?;
        let renderer = Renderer::new(driver, control.clone(), vram.clone());
        let emu = emu::Emulator::new(control, vram, vmtime, rom, renderer.control());
        Ok(Self { emu, renderer })
    }
}

impl ChangeDeviceState for VgaDevice {
    fn start(&mut self) {
        self.renderer.start();
    }

    async fn stop(&mut self) {
        self.renderer.stop().await;
    }

    async fn reset(&mut self) {
        self.emu.reset();
    }
}

impl ChipsetDevice for VgaDevice {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }

    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }
}

impl PciConfigSpace for VgaDevice {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        self.emu.notify_pci_config_access_read(offset, value)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        self.emu.notify_pci_config_access_write(offset, value)
    }

    fn suggested_bdf(&mut self) -> Option<(u8, u8, u8)> {
        Some((0, 8, 0)) // to match legacy Hyper-V behavior
    }
}

impl MmioIntercept for VgaDevice {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        self.emu.notify_mmio_read(addr, data);
        IoResult::Ok
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        self.emu.notify_mmio_write(addr, data);
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, std::ops::RangeInclusive<u64>)] {
        // N.B. The VM's RAM must be configured as unmapped in this region.
        &[("vga", 0xa0000..=0xbffff)]
    }
}

impl PortIoIntercept for VgaDevice {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        let v = self.emu.io_port_read(io_port, data.len() as u16);
        data.copy_from_slice(&v.to_ne_bytes()[..data.len()]);
        IoResult::Ok
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        let mut v = [0; 4];
        v[..data.len()].copy_from_slice(data);
        self.emu
            .io_port_write(io_port, data.len() as u16, u32::from_ne_bytes(v));
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, std::ops::RangeInclusive<u16>)] {
        &[
            ("mda", 0x3b0..=0x3bf),
            ("vga", 0x3c0..=0x3cf),
            ("cga", 0x3d0..=0x3df),
            ("s3", 0x4ae8..=0x4ae8),
        ]
    }
}

impl SaveRestore for VgaDevice {
    type SavedState = SavedStateNotSupported;

    fn save(&mut self) -> Result<Self::SavedState, vmcore::save_restore::SaveError> {
        todo!()
    }

    fn restore(
        &mut self,
        state: Self::SavedState,
    ) -> Result<(), vmcore::save_restore::RestoreError> {
        match state {}
    }
}
