// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use chipset_device::io::IoResult;
use chipset_device::mmio::ControlMmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::PciAerInjection;
use chipset_device::pci::PciConfigSpace;
use pci_bus::GenericPciBusDevice;
use pci_core::capabilities::extended::PciExtendedCapability;
use pci_core::capabilities::extended::aer::AerExtendedCapability;
use pci_core::capabilities::extended::aer::AerInjectedErrorKind;
use pci_core::capabilities::extended::aer::AerInjection;
use pci_core::msi::SignalMsi;
use pci_core::spec::caps::ExtendedCapabilityId;
use pci_core::spec::caps::aer::AerExtendedCapabilityHeader;
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::Mutex;

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

pub struct TestPcieEndpoint<R, W>
where
    R: Fn(u16, &mut u32) -> Option<IoResult> + 'static + Send,
    W: FnMut(u16, u32) -> Option<IoResult> + 'static + Send,
{
    cfg_read_closure: R,
    cfg_write_closure: W,
}

impl<R, W> TestPcieEndpoint<R, W>
where
    R: Fn(u16, &mut u32) -> Option<IoResult> + 'static + Send,
    W: FnMut(u16, u32) -> Option<IoResult> + 'static + Send,
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
    R: Fn(u16, &mut u32) -> Option<IoResult> + 'static + Send,
    W: FnMut(u16, u32) -> Option<IoResult> + 'static + Send,
{
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> Option<IoResult> {
        (self.cfg_read_closure)(offset, value)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> Option<IoResult> {
        (self.cfg_write_closure)(offset, value)
    }
}

impl<R, W> Debug for TestPcieEndpoint<R, W>
where
    R: Fn(u16, &mut u32) -> Option<IoResult> + 'static + Send,
    W: FnMut(u16, u32) -> Option<IoResult> + 'static + Send,
{
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(fmt, "TestPcieEndpoint")
    }
}

/// Adapts a switch to `GenericPciBusDevice` for topology tests.
pub struct SwitchAdapter(pub crate::switch::GenericPcieSwitch);

impl GenericPciBusDevice for SwitchAdapter {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> Option<IoResult> {
        Some(PciConfigSpace::pci_cfg_read(&mut self.0, offset, value))
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> Option<IoResult> {
        Some(PciConfigSpace::pci_cfg_write(&mut self.0, offset, value))
    }

    fn pci_cfg_read_with_routing(
        &mut self,
        secondary_bus: u8,
        target_bus: u8,
        function: u8,
        offset: u16,
        value: &mut u32,
    ) -> Option<IoResult> {
        Some(
            self.0
                .pci_cfg_read_with_routing(secondary_bus, target_bus, function, offset, value),
        )
    }

    fn pci_cfg_write_with_routing(
        &mut self,
        secondary_bus: u8,
        target_bus: u8,
        function: u8,
        offset: u16,
        value: u32,
    ) -> Option<IoResult> {
        Some(
            self.0
                .pci_cfg_write_with_routing(secondary_bus, target_bus, function, offset, value),
        )
    }

    fn pci_inject_aer_with_routing(
        &mut self,
        secondary_bus: u8,
        target_bus: u8,
        function: u8,
        injection: PciAerInjection,
    ) -> Option<bool> {
        Some(
            self.0
                .pci_inject_aer_with_routing(secondary_bus, target_bus, function, injection),
        )
    }

    fn pci_inject_dpc_with_routing(
        &mut self,
        secondary_bus: u8,
        target_bus: u8,
        function: u8,
        action: chipset_device::pci::PcieDpcRoutingAction,
    ) -> Option<bool> {
        Some(
            self.0
                .pci_inject_dpc_with_routing(secondary_bus, target_bus, function, action),
        )
    }
}

#[derive(Default, Debug)]
pub struct RecordingSignalMsi {
    records: Mutex<Vec<(Option<u32>, u64, u32)>>,
}

impl RecordingSignalMsi {
    pub fn pop(&self) -> Option<(Option<u32>, u64, u32)> {
        self.records
            .lock()
            .expect("recording MSI mutex poisoned")
            .pop()
    }
}

impl SignalMsi for RecordingSignalMsi {
    fn signal_msi(&self, devid: Option<u32>, address: u64, data: u32) {
        self.records
            .lock()
            .expect("recording MSI mutex poisoned")
            .push((devid, address, data));
    }
}

pub struct TestAerEndpoint {
    devfn: u8,
    aer: Arc<Mutex<AerExtendedCapability>>,
}

impl TestAerEndpoint {
    pub fn new(devfn: u8, aer: Arc<Mutex<AerExtendedCapability>>) -> Self {
        Self { devfn, aer }
    }
}

impl GenericPciBusDevice for TestAerEndpoint {
    fn pci_cfg_read(&mut self, _offset: u16, value: &mut u32) -> Option<IoResult> {
        *value = !0;
        Some(IoResult::Ok)
    }

    fn pci_cfg_write(&mut self, _offset: u16, _value: u32) -> Option<IoResult> {
        Some(IoResult::Ok)
    }

    fn pci_inject_aer_with_routing(
        &mut self,
        secondary_bus: u8,
        target_bus: u8,
        function: u8,
        injection: PciAerInjection,
    ) -> Option<bool> {
        if target_bus != secondary_bus || function != self.devfn {
            return Some(false);
        }

        let mut aer = self.aer.lock().expect("endpoint AER mutex poisoned");
        let _ = aer.inject(AerInjection {
            kind: match injection.kind {
                chipset_device::pci::PciAerErrorKind::Correctable => {
                    AerInjectedErrorKind::Correctable
                }
                chipset_device::pci::PciAerErrorKind::Uncorrectable => {
                    AerInjectedErrorKind::Uncorrectable
                }
            },
            status_bits: injection.status_bits,
            header_log: injection.header_log,
            source_id: injection.source_id,
        });

        Some(true)
    }
}

pub fn find_cap_offset_type1(
    cfg: &pci_core::cfg_space_emu::ConfigSpaceType1Emulator,
    cap_id: u8,
) -> u16 {
    let mut dword = 0u32;
    cfg.read_u32(0x34, &mut dword).unwrap();
    let mut ptr = (dword & 0xff) as u16;
    let mut hop = 0;
    while ptr != 0 && hop < 48 {
        cfg.read_u32(ptr, &mut dword).unwrap();
        if (dword & 0xff) as u8 == cap_id {
            return ptr;
        }
        ptr = ((dword >> 8) & 0xff) as u16;
        hop += 1;
    }

    panic!("capability id {cap_id:#x} not found");
}

pub fn find_ext_cap_offset_type1(
    cfg: &pci_core::cfg_space_emu::ConfigSpaceType1Emulator,
    cap_id: ExtendedCapabilityId,
) -> u16 {
    let mut offset = 0x100u16;
    let mut dword = 0u32;
    let mut hop = 0;

    while offset != 0 && hop < 64 {
        cfg.read_u32(offset, &mut dword).unwrap();
        if (dword & 0xffff) as u16 == cap_id.0 {
            return offset;
        }

        offset = ((dword >> 20) & 0xfff) as u16;
        hop += 1;
    }

    panic!("extended capability id {:#x} not found", cap_id.0);
}

pub fn read_aer_dword(aer: &AerExtendedCapability, offset: AerExtendedCapabilityHeader) -> u32 {
    let mut v = 0u32;
    aer.read(
        offset.0,
        ByteEnabledDwordRead::with_all_bytes_enabled(&mut v),
    );
    v
}
