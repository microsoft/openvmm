// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use chipset_device::io::IoResult;
use chipset_device::mmio::ControlMmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use pci_bus::GenericPciBusDevice;
use std::fmt::Debug;
use std::sync::mpsc;

pub struct TestPcieMmioRegistration {}

impl RegisterMmioIntercept for TestPcieMmioRegistration {
    fn new_io_region(&mut self, _debug_name: &str, len: u64) -> Box<dyn ControlMmioIntercept> {
        Box::new(TestPcieControlMmioIntercept { mapping: None, len })
    }
}

pub struct TestPcieControlMmioIntercept {
    pub mapping: Option<u64>,
    pub len: u64,
}

impl ControlMmioIntercept for TestPcieControlMmioIntercept {
    /// Enables the IO region.
    fn map(&mut self, addr: u64) {
        match self.mapping {
            Some(_) => panic!("already mapped"),
            None => self.mapping = Some(addr),
        }
    }

    /// Disables the IO region.
    fn unmap(&mut self) {
        match self.mapping {
            Some(_) => self.mapping = None,
            None => panic!("not mapped"),
        }
    }

    /// Return the currently mapped address.
    ///
    /// Returns `None` if the region is currently unmapped.
    fn addr(&self) -> Option<u64> {
        self.mapping
    }

    fn len(&self) -> u64 {
        self.len
    }

    /// Return the offset of `addr` from the region's base address.
    ///
    /// Returns `None` if the provided `addr` is outside of the memory
    /// region, or the region is currently unmapped.
    fn offset_of(&self, addr: u64) -> Option<u64> {
        self.mapping.map(|base_addr| addr - base_addr)
    }

    fn region_name(&self) -> &str {
        "???"
    }
}

pub struct TestPciDevice;

impl GenericPciBusDevice for TestPciDevice {
    fn pci_cfg_read(&mut self, _offset: u16, _value: ByteEnabledDwordRead<'_>) -> Option<IoResult> {
        Some(IoResult::Ok)
    }

    fn pci_cfg_write(&mut self, _offset: u16, _value: ByteEnabledDwordWrite) -> Option<IoResult> {
        Some(IoResult::Ok)
    }
}

pub struct BlockingEndpoint {
    access_started: mpsc::Sender<()>,
    release_access: mpsc::Receiver<()>,
}

impl BlockingEndpoint {
    pub fn new() -> (Self, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (access_started, access_started_rx) = mpsc::channel();
        let (release_access, release_access_rx) = mpsc::channel();
        (
            Self {
                access_started,
                release_access: release_access_rx,
            },
            access_started_rx,
            release_access,
        )
    }
}

impl GenericPciBusDevice for BlockingEndpoint {
    fn pci_cfg_read(
        &mut self,
        _offset: u16,
        mut value: ByteEnabledDwordRead<'_>,
    ) -> Option<IoResult> {
        self.access_started.send(()).unwrap();
        self.release_access.recv().unwrap();
        value.set(0x1234_5678);
        Some(IoResult::Ok)
    }

    fn pci_cfg_write(&mut self, _offset: u16, _value: ByteEnabledDwordWrite) -> Option<IoResult> {
        Some(IoResult::Ok)
    }
}

pub struct TestPcieEndpoint<R, W>
where
    R: Fn(u16, ByteEnabledDwordRead<'_>) -> Option<IoResult> + 'static + Send,
    W: FnMut(u16, ByteEnabledDwordWrite) -> Option<IoResult> + 'static + Send,
{
    cfg_read_closure: R,
    cfg_write_closure: W,
}

impl<R, W> TestPcieEndpoint<R, W>
where
    R: Fn(u16, ByteEnabledDwordRead<'_>) -> Option<IoResult> + 'static + Send,
    W: FnMut(u16, ByteEnabledDwordWrite) -> Option<IoResult> + 'static + Send,
{
    pub fn new(cfg_read_closure: R, cfg_write_closure: W) -> Self {
        Self {
            cfg_read_closure,
            cfg_write_closure,
        }
    }
}

impl<R, W> GenericPciBusDevice for TestPcieEndpoint<R, W>
where
    R: Fn(u16, ByteEnabledDwordRead<'_>) -> Option<IoResult> + 'static + Send,
    W: FnMut(u16, ByteEnabledDwordWrite) -> Option<IoResult> + 'static + Send,
{
    fn pci_cfg_read(&mut self, offset: u16, value: ByteEnabledDwordRead<'_>) -> Option<IoResult> {
        (self.cfg_read_closure)(offset, value)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: ByteEnabledDwordWrite) -> Option<IoResult> {
        (self.cfg_write_closure)(offset, value)
    }
}

impl<R, W> Debug for TestPcieEndpoint<R, W>
where
    R: Fn(u16, ByteEnabledDwordRead<'_>) -> Option<IoResult> + 'static + Send,
    W: FnMut(u16, ByteEnabledDwordWrite) -> Option<IoResult> + 'static + Send,
{
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(fmt, "TestPcieEndpoint")
    }
}
