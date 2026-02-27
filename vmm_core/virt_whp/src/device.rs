// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::WhpPartitionInner;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::mmio::ControlMmioIntercept;
use chipset_device::mmio::MmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pci::PciConfigSpace;
use hv1_hypercall::HvInterruptParameters;
use hvdef::Vtl;
use inspect::Inspect;
use inspect::InspectMut;
use parking_lot::Mutex;
use pci_core::bar_mapping::BarMappings;
use pci_core::msi::SignalMsi;
use pci_core::spec::cfg_space;
use std::os::windows::prelude::*;
use std::sync::Arc;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateNotSupported;
use vmcore::vpci_msi::MapVpciInterrupt;
use vmcore::vpci_msi::MsiAddressData;
use vmcore::vpci_msi::RegisterInterruptError;
use vmcore::vpci_msi::VpciInterruptParameters;
use whp::VpciInterruptTarget;
use winapi::um::winnt;

pub struct Device {
    partition: Arc<WhpPartitionInner>,
    vtl: Vtl,
    device_id: u64,
    interrupts: Mutex<Vec<Option<MsiAddressData>>>,
}

impl Device {
    pub(super) fn new_physical(
        partition: Arc<WhpPartitionInner>,
        vtl: Vtl,
        device_id: u64,
        resource: whp::VpciResource,
    ) -> Result<Self, whp::WHvError> {
        partition.vtlp(vtl).whp.create_device(
            device_id,
            resource,
            whp::abi::WHvCreateVpciDeviceFlagPhysicallyBacked,
            None,
        )?;
        Ok(Device {
            partition,
            vtl,
            device_id,
            interrupts: Default::default(),
        })
    }

    pub(super) fn _new_virtual(
        partition: Arc<WhpPartitionInner>,
        vtl: Vtl,
        device_id: u64,
        resource: whp::VpciResource,
    ) -> Result<Self, whp::WHvError> {
        partition.vtlp(vtl).whp.create_device(
            device_id,
            resource,
            whp::abi::WHvCreateVpciDeviceFlagUseLogicalInterrupts,
            None,
        )?;
        Ok(Device {
            partition,
            vtl,
            device_id,
            interrupts: Default::default(),
        })
    }

    pub fn _retarget_interrupt(&self, address: u64, data: u32, params: &HvInterruptParameters<'_>) {
        let mut flags = Default::default();
        if params.multicast && params.target_processors.count() > 1 {
            flags |= whp::abi::WHvVpciInterruptTargetFlagMulticast;
        }
        let target_processors = Vec::from_iter(params.target_processors);
        let target = VpciInterruptTarget::new(params.vector, flags, &target_processors);
        self.device()
            .retarget_interrupt(address, data, &target)
            .expect("BUGBUG");
    }

    fn device(&self) -> whp::Device<'_> {
        self.partition.vtlp(self.vtl).whp.device(self.device_id)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        self.partition
            .vtlp(self.vtl)
            .whp
            .delete_device(self.device_id)
            .expect("device failed to delete");
    }
}

impl SignalMsi for Device {
    fn signal_msi(&self, _rid: u32, address: u64, data: u32) {
        if let Err(err) = self.device().interrupt(address, data) {
            tracelimit::warn_ratelimited!(
                address,
                data,
                err = &err as &dyn std::error::Error,
                "failed to signal MSI",
            );
        }
    }
}

impl MapVpciInterrupt for Device {
    async fn register_interrupt(
        &self,
        vector_count: u32,
        params: &VpciInterruptParameters<'_>,
    ) -> Result<MsiAddressData, RegisterInterruptError> {
        let mut interrupts = self.interrupts.lock();
        let (index, m) = match interrupts.iter_mut().enumerate().find(|(_, m)| m.is_none()) {
            Some((index, m)) => (index, m),
            None => {
                interrupts.push(None);
                (interrupts.len() - 1, interrupts.last_mut().unwrap())
            }
        };

        let mut flags = Default::default();
        if params.multicast && params.target_processors.len() > 1 {
            flags |= whp::abi::WHvVpciInterruptTargetFlagMulticast;
        }
        let target = VpciInterruptTarget::new(params.vector, flags, params.target_processors);
        let (address, data) = self
            .device()
            .map_interrupt(index as u32, vector_count, &target)
            .map_err(RegisterInterruptError::new)?;
        let r = MsiAddressData { address, data };
        *m = Some(r);
        Ok(r)
    }

    async fn unregister_interrupt(&self, address: u64, data: u32) {
        let mut interrupts = self.interrupts.lock();
        let (index, m) = interrupts
            .iter_mut()
            .enumerate()
            .find(|(_, m)| m.as_ref().map(|x| (x.address, x.data)) == Some((address, data)))
            .expect("interrupt not found");

        self.device().unmap_interrupt(index as u32).expect("BUGBUG");

        *m = None;
    }
}

fn probe_power_register(device: &whp::Device<'_>) -> Option<u32> {
    let read = |offset| {
        let mut data = [0; 4];
        if device
            .read_register(whp::abi::WHvVpciConfigSpace, offset, &mut data)
            .is_ok()
        {
            u32::from_ne_bytes(data)
        } else {
            0
        }
    };

    let mut next = read(0x34) & !3;
    while next != 0 {
        let val = read(next as u16);
        let cap = val & 0xff;
        if cap == 1 {
            return Some(next + 4);
        }
        next = (val >> 16) & 0xff;
    }
    None
}

fn parse_probed_bars(probed_bars: [u32; 6]) -> [u32; 6] {
    let mut bar_flags = [0; 6];
    let mut i = 0;
    while i < probed_bars.len() {
        bar_flags[i] = probed_bars[i] & 0xf;
        if cfg_space::BarEncodingBits::from_bits(probed_bars[i]).type_64_bit() {
            i += 2;
        } else {
            i += 1;
        }
    }
    bar_flags
}

#[derive(Debug)]
struct MmioMapping(whp::abi::WHV_VPCI_MMIO_MAPPING);

unsafe impl Send for MmioMapping {}
unsafe impl Sync for MmioMapping {}

impl MmioMapping {
    fn matches(&self, bar: u8, offset: u16, len: usize, write: bool) -> bool {
        self.0.Location.0 == bar as i32
            && self.fits(offset, len)
            && ((write
                && self
                    .0
                    .Flags
                    .is_set(whp::abi::WHvVpciMmioRangeFlagWriteAccess))
                || (!write
                    && self
                        .0
                        .Flags
                        .is_set(whp::abi::WHvVpciMmioRangeFlagReadAccess)))
    }

    fn fits(&self, offset: u16, len: usize) -> bool {
        let offset = offset as u64;
        offset >= self.0.OffsetInBytes
            && offset < self.0.OffsetInBytes + self.0.SizeInBytes
            && self.0.OffsetInBytes + self.0.SizeInBytes - offset >= len as u64
    }

    fn read(&self, offset: u16, data: &mut [u8]) {
        assert!(
            self.0
                .Flags
                .is_set(whp::abi::WHvVpciMmioRangeFlagReadAccess)
                && self.fits(offset, data.len())
        );
        unsafe {
            std::ptr::copy_nonoverlapping(
                (self.0.VirtualAddress as *const u8)
                    .add((offset as u64 - self.0.OffsetInBytes) as usize),
                data.as_mut_ptr(),
                data.len(),
            )
        }
    }

    fn write(&self, offset: u16, data: &[u8]) {
        assert!(
            self.0
                .Flags
                .is_set(whp::abi::WHvVpciMmioRangeFlagWriteAccess)
                && self.fits(offset, data.len())
        );
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.0
                    .VirtualAddress
                    .cast::<u8>()
                    .add((offset as u64 - self.0.OffsetInBytes) as usize),
                data.len(),
            )
        }
    }
}

/// A type to control the intercept mapping of a BAR.
struct BarIntercept {
    /// Associated BAR register index
    pub index: u8,
    /// Size of MMIO region
    pub len: u64,
    /// Intercept control
    pub control: Box<dyn ControlMmioIntercept>,
}

pub struct BarIntercepts(Vec<BarIntercept>);

impl Inspect for BarIntercepts {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut res = req.respond();
        for bar in self.0.iter() {
            res.field(&format!("bar{}", bar.index), format!("{:#x?}", bar.len));
        }
    }
}

impl BarIntercepts {
    fn new(register_mmio: &mut dyn RegisterMmioIntercept, probed_bars: [u32; 6]) -> BarIntercepts {
        let mut controls = Vec::new();
        let parsed = BarMappings::parse(&[0; 6], &probed_bars);
        for parsed_mapping in parsed.iter() {
            controls.push(BarIntercept {
                index: parsed_mapping.index,
                len: parsed_mapping.len,
                control: register_mmio
                    .new_io_region(&format!("bar{}", parsed_mapping.index), parsed_mapping.len),
            });
        }
        Self(controls)
    }

    pub fn map(&mut self, mappings: &BarMappings) {
        for (intercept, mapping) in self.0.iter_mut().zip(mappings.iter()) {
            assert!(intercept.len == mapping.len);
            intercept.control.map(mapping.base_address);
        }
    }

    pub fn unmap(&mut self) {
        for intercept in self.0.iter_mut() {
            intercept.control.unmap();
        }
    }
}

pub struct AssignedPciDevice {
    device: Arc<Device>,
    probed_bars: [u32; 6],
    bar_flags: [u32; 6],
    bars: [u32; 6],
    power_reg: Option<u32>,
    power_state: u32,
    active_bars: BarMappings,
    mmio: Vec<MmioMapping>,
    mmio_enabled: bool,
    bar_intercepts: BarIntercepts,
    // use a bare u16 (instead of `cfg_space::Command`) to avoid any possible
    // truncation during passthrough
    command: u16,
}

impl InspectMut for AssignedPciDevice {
    fn inspect_mut(&mut self, _req: inspect::Request<'_>) {
        // TODO
    }
}

impl AssignedPciDevice {
    pub fn new(
        register_mmio: &mut dyn RegisterMmioIntercept,
        device: Arc<Device>,
    ) -> Result<Self, whp::WHvError> {
        let probed_bars = device.device().probed_bars()?.Value;
        let bar_flags = parse_probed_bars(probed_bars);
        let power_reg = probe_power_register(&device.device());
        let bar_intercepts = BarIntercepts::new(register_mmio, probed_bars);
        Ok(Self {
            device,
            probed_bars,
            bar_flags,
            bars: [0; 6],
            power_reg,
            power_state: 3,
            active_bars: Default::default(),
            mmio: Vec::new(),
            bar_intercepts,
            command: 0,
            mmio_enabled: false,
        })
    }

    fn read_phys_config(&self, offset: u16) -> u32 {
        let mut data = [0; 4];
        match self
            .device
            .device()
            .read_register(whp::abi::WHvVpciConfigSpace, offset, &mut data)
        {
            Ok(_) => u32::from_ne_bytes(data),
            Err(e) => {
                tracing::warn!(
                    offset,
                    error = &e as &dyn std::error::Error,
                    "config space read",
                );
                !0
            }
        }
    }

    fn write_phys_config(&self, offset: u16, value: u32) {
        match self.device.device().write_register(
            whp::abi::WHvVpciConfigSpace,
            offset,
            &value.to_ne_bytes(),
        ) {
            Ok(_) => (),
            Err(e) => tracing::warn!(
                offset,
                error = &e as &dyn std::error::Error,
                "config space write",
            ),
        }
    }

    fn set_power_state(&mut self, power_state: u32) {
        let win_power_state = match power_state & 3 {
            0 => winnt::PowerDeviceD0,
            1 => winnt::PowerDeviceD1,
            2 => winnt::PowerDeviceD2,
            3 => winnt::PowerDeviceD3,
            _ => unreachable!(),
        };
        match self.device.device().set_power_state(win_power_state) {
            Ok(_) => tracing::info!(power_state, "power state"),
            Err(e) => tracing::error!(
                power_state,
                error = &e as &dyn std::error::Error,
                "failed to set power state",
            ),
        }
        self.power_state = power_state;
    }

    fn enable_mmio(&mut self) {
        if !self.mmio_enabled {
            match self.device.device().map_mmio() {
                Ok(mmio) => {
                    self.mmio = mmio.into_iter().map(MmioMapping).collect();
                    self.active_bars = BarMappings::parse(&self.bars, &self.probed_bars);
                    self.bar_intercepts.map(&self.active_bars);
                    self.mmio_enabled = true;
                    // TODO: map MMIO on command write for efficient access
                }
                Err(e) => tracing::error!(error = &e as &dyn std::error::Error, "mmio map failed"),
            }
        }
    }

    fn disable_mmio(&mut self) {
        if self.mmio_enabled {
            self.bar_intercepts.unmap();
            self.active_bars = Default::default();
            self.mmio.clear();
            self.device
                .device()
                .unmap_mmio()
                .expect("unmap should not fail");
        }
    }
}

impl ChangeDeviceState for AssignedPciDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        // TODO
    }
}

impl ChipsetDevice for AssignedPciDevice {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }

    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }
}

impl SaveRestore for AssignedPciDevice {
    type SavedState = SavedStateNotSupported;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Err(SaveError::NotSupported)
    }

    fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
        match state {}
    }
}

impl PciConfigSpace for AssignedPciDevice {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        match offset {
            0x10 | 0x14 | 0x18 | 0x1c | 0x20 | 0x24 => {
                let i = (offset - 0x10) as usize / 4;
                *value = self.bars[i]
            }
            _ => {
                let phys = self.read_phys_config(offset);
                *value = if Some(offset as u32) == self.power_reg {
                    self.power_state | (phys & !3)
                } else {
                    phys
                }
            }
        }

        IoResult::Ok
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        match offset {
            4 => {
                // Power on/off the device if there is no power cap.
                let command = cfg_space::Command::from_bits(value as u16);
                if command.mmio_enabled() {
                    if self.power_reg.is_none() && self.power_state != 0 {
                        tracing::info!("implicitly transitioning to D0");
                        self.set_power_state(0);
                    }
                    if self.power_state == 0 {
                        self.enable_mmio();
                    }
                } else {
                    self.disable_mmio();
                    if self.power_reg.is_none() && self.power_state != 3 {
                        tracing::info!("implicitly transitioning to D3");
                        self.set_power_state(3);
                    }
                }
                self.write_phys_config(offset, value);
                self.command = command.into_bits();
            }

            0x10 | 0x14 | 0x18 | 0x1c | 0x20 | 0x24 => {
                let i = (offset - 0x10) as usize / 4;
                self.bars[i] = value & self.probed_bars[i] | self.bar_flags[i];
            }
            _ => {
                if Some(offset as u32) == self.power_reg {
                    let power_state = value & 3;
                    if power_state == 0 && cfg_space::Command::from(self.command).mmio_enabled() {
                        self.set_power_state(power_state);
                        self.enable_mmio();
                    } else {
                        self.disable_mmio();
                        self.set_power_state(power_state);
                    }
                } else {
                    self.write_phys_config(offset, value)
                }
            }
        }

        IoResult::Ok
    }
}

impl MmioIntercept for AssignedPciDevice {
    fn mmio_read(&mut self, address: u64, data: &mut [u8]) -> IoResult {
        if let Some((bar, offset)) = self.active_bars.find(address) {
            if let Some(mmio) = self
                .mmio
                .iter()
                .find(|mmio| mmio.matches(bar, offset, data.len(), false))
            {
                mmio.read(offset, data);
                return IoResult::Ok;
            }
            match self.device.device().read_register(
                whp::abi::WHV_VPCI_DEVICE_REGISTER_SPACE(bar.into()),
                offset,
                data,
            ) {
                Ok(_) => return IoResult::Ok,
                Err(e) => {
                    tracing::warn!(address, error = &e as &dyn std::error::Error, "MMIO read",)
                }
            }
        }
        data.fill(!0);
        IoResult::Ok
    }

    fn mmio_write(&mut self, address: u64, data: &[u8]) -> IoResult {
        if let Some((bar, offset)) = self.active_bars.find(address) {
            if let Some(mmio) = self
                .mmio
                .iter()
                .find(|mmio| mmio.matches(bar, offset, data.len(), true))
            {
                mmio.write(offset, data);
                return IoResult::Ok;
            }
            match self.device.device().write_register(
                whp::abi::WHV_VPCI_DEVICE_REGISTER_SPACE(bar.into()),
                offset,
                data,
            ) {
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(address, error = &e as &dyn std::error::Error, "MMIO write",)
                }
            }
        }
        IoResult::Ok
    }
}

#[derive(Debug)]
pub struct DeviceHandle(pub whp::VpciResource);

impl Clone for DeviceHandle {
    fn clone(&self) -> Self {
        Self(self.0.as_handle().try_clone_to_owned().expect("oom").into())
    }
}

impl From<OwnedHandle> for DeviceHandle {
    fn from(handle: OwnedHandle) -> Self {
        Self(handle.into())
    }
}

impl AsHandle for DeviceHandle {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.0.as_handle()
    }
}

impl From<DeviceHandle> for OwnedHandle {
    fn from(handle: DeviceHandle) -> OwnedHandle {
        handle.0.into()
    }
}

mesh::payload::os_resource!(DeviceHandle, OwnedHandle);
