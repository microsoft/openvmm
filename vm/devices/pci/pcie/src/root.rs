// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI Express root complex and root port emulation.

use crate::BDF_BUS_SHIFT;
use crate::BDF_DEVICE_FUNCTION_MASK;
use crate::MAX_FUNCTIONS_PER_BUS;
use crate::PAGE_OFFSET_MASK;
use crate::PAGE_SHIFT;
use crate::PAGE_SIZE64;
use crate::ROOT_PORT_DEVICE_ID;
use crate::VENDOR_ID;
use crate::port::GenericPciePortDefinition;
use crate::port::PcieDownstreamPort;
use crate::port::PciePortSettings;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::ControlMmioIntercept;
use chipset_device::mmio::MmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigAddress;
use chipset_device::pci::PciConfigByteEnable;
use chipset_device::poll_device::PollDevice;
use cxl_spec::CxlComponentRegisters;
use inspect::Inspect;
use inspect::InspectMut;
use memory_range::MemoryRange;
use pci_bus::GenericPciBusDevice;
use pci_core::bus_cfg::PciBusCfgAccessCallbacks;
use pci_core::bus_cfg::PciBusCfgAccessHandler;
use pci_core::bus_range::AssignedBusRange;
use pci_core::msi::MsiTarget;
use pci_core::spec::caps::pci_express::DevicePortType;
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;
use std::ops::RangeInclusive;
use std::sync::Arc;
use thiserror::Error;
use vmcore::device_state::ChangeDeviceState;

/// Error returned when a root complex configuration is invalid.
#[derive(Debug, Error)]
pub enum InvalidRootComplexError {
    /// Too many root ports were requested for the available device/function
    /// slots.
    #[error("requested {port_count} root ports, but only {max} are supported")]
    TooManyPorts {
        /// Number of root ports requested.
        port_count: usize,
        /// Maximum number of root ports supported.
        max: usize,
    },
    /// A root port's devfn could not be assigned.
    #[error(transparent)]
    Devfn(#[from] crate::PortDevfnError),
    /// A root port was assigned to a device number reserved for another use.
    #[error("root port {port_index} would be placed at reserved device number {device:#x}")]
    ReservedDeviceCollision {
        /// Index of the root port that collided.
        port_index: usize,
        /// The reserved device number it would have occupied.
        device: u8,
    },
}

/// A generic PCI Express root complex emulator.
#[derive(InspectMut)]
pub struct GenericPcieRootComplex {
    /// The lowest valid bus number under the root complex.
    start_bus: u8,
    /// The highest valid bus number under the root complex.
    end_bus: u8,
    /// Intercept control for the ECAM MMIO region.
    ecam: Box<dyn ControlMmioIntercept>,
    /// Intercept control for the CHBCR MMIO region, when present.
    chbcr: Option<Box<dyn ControlMmioIntercept>>,
    /// CXL Component Registers backing CHBCR accesses in CXL mode.
    cxl_component_registers: Option<CxlComponentRegisters>,
    /// Devices on the root complex bus, sorted by devfn
    /// (device << 3 | function).
    #[inspect(with = "|x| inspect::iter_by_key(x.iter().map(|(k, v)| (k, v)))")]
    devices: Vec<(u8, BusDevice)>,
    /// Bitmask of reserved root-bus device numbers (bit N => device N).
    reserved_device_numbers: u32,
    /// Bus config space accesses handler.
    bus_cfg_handler: PciBusCfgAccessHandler,
}

/// A device occupying a slot on the root complex bus.
enum BusDevice {
    /// A root port providing downstream connectivity.
    RootPort { name: Arc<str>, port: Box<RootPort> },
    /// A Root Complex Integrated Endpoint (RCiEP).
    Rciep {
        name: Arc<str>,
        dev: Box<dyn GenericPciBusDevice>,
    },
}

impl Inspect for BusDevice {
    fn inspect(&self, req: inspect::Request<'_>) {
        match self {
            BusDevice::RootPort { port, .. } => {
                port.as_ref().inspect(req);
            }
            BusDevice::Rciep { name, .. } => {
                req.value(name.as_ref());
            }
        }
    }
}

/// Information about a downstream port in a PCIe topology.
pub struct DownstreamPortInfo {
    /// The devfn (device << 3 | function) of this port on the root complex bus.
    pub devfn: u8,
    /// The port name.
    pub name: Arc<str>,
    /// Shared bus range, updated by the config space emulator when the
    /// guest programs secondary/subordinate bus numbers.
    pub bus_range: AssignedBusRange,
}

/// A flat description of a PCIe switch without hierarchy.
pub struct GenericSwitchDefinition {
    /// The name of the switch.
    pub name: Arc<str>,
    /// Number of downstream ports.
    pub num_downstream_ports: u8,
    /// The parent port this switch is connected to.
    pub parent_port: Arc<str>,
    /// Whether hotplug is enabled for this switch.
    pub hotplug: bool,
    /// Express-level settings for downstream switch ports.
    pub dsp_settings: PciePortSettings,
}

impl GenericSwitchDefinition {
    /// Create a new switch definition.
    pub fn new(
        name: impl Into<Arc<str>>,
        num_downstream_ports: u8,
        parent_port: impl Into<Arc<str>>,
        hotplug: bool,
        dsp_settings: PciePortSettings,
    ) -> Self {
        Self {
            name: name.into(),
            num_downstream_ports,
            parent_port: parent_port.into(),
            hotplug,
            dsp_settings,
        }
    }
}

/// Builder for [`GenericPcieRootComplex`].
///
/// Obtain via [`GenericPcieRootComplex::builder`], configure optional
/// settings, then call [`build`](GenericPcieRootComplexBuilder::build).
pub struct GenericPcieRootComplexBuilder<'a> {
    register_mmio: &'a mut dyn RegisterMmioIntercept,
    bus_range: RangeInclusive<u8>,
    ecam_range: MemoryRange,
    root_ports: Option<(Vec<GenericPciePortDefinition>, &'a MsiTarget)>,
    first_port_device_number: u8,
    reserved_device_numbers: u32,
    chbcr_range: Option<MemoryRange>,
}

fn device_number_is_reserved(reserved_device_numbers: u32, device: u8) -> bool {
    let bit = 1u32 << device;
    reserved_device_numbers & bit != 0
}

impl<'a> GenericPcieRootComplexBuilder<'a> {
    /// Add root ports to the complex.
    ///
    /// `msi_target` is the MSI target for all root ports; the caller is
    /// responsible for creating the `MsiConnection` and connecting it to
    /// the platform's interrupt controller.
    pub fn root_ports(
        mut self,
        ports: Vec<GenericPciePortDefinition>,
        msi_target: &'a MsiTarget,
    ) -> Self {
        self.root_ports = Some((ports, msi_target));
        self
    }

    /// Set the first PCI device number to assign to root ports (default 0).
    ///
    /// Root ports are placed at consecutive device numbers starting from
    /// this value. Use a non-zero value to reserve lower device numbers
    /// for RCiEPs (e.g., an IOMMU at device 0).
    pub fn first_port_device_number(mut self, device: u8) -> Self {
        self.first_port_device_number = device;
        self
    }

    /// Reserve root-bus device numbers via a bitmask.
    ///
    /// Root ports and RCiEPs are assigned into device/function slots; any
    /// device number whose bit is set here is treated as occupied by another entity
    /// (e.g. a phantom IOAPIC), and a configuration that would place a root
    /// port on it is rejected at [`build`](Self::build) time.
    pub fn reserved_device_numbers(mut self, mask: u32) -> Self {
        self.reserved_device_numbers = mask;
        self
    }

    /// Set the CHBCR (Component Register BAR) MMIO range for CXL mode.
    ///
    /// When set, CXL component registers are allocated and a CHBCR MMIO
    /// region is mapped.
    pub fn chbcr_range(mut self, range: Option<MemoryRange>) -> Self {
        self.chbcr_range = range;
        self
    }

    /// Build the root complex.
    ///
    /// Returns an error if the root port count exceeds the available
    /// device/function slots (32 devices × 8 functions = 256 max).
    pub fn build(self) -> Result<GenericPcieRootComplex, InvalidRootComplexError> {
        let Self {
            register_mmio,
            bus_range,
            ecam_range,
            root_ports,
            first_port_device_number,
            reserved_device_numbers,
            chbcr_range,
        } = self;

        let start_bus = *bus_range.start();
        let end_bus = *bus_range.end();

        let mut ecam = register_mmio.new_io_region("ecam", ecam_range.len());
        ecam.map(ecam_range.start());

        // Presence of CHBCR range indicates CXL mode, which needs a component-register
        // backing object even if no capability payload blocks are registered yet.
        let cxl_component_registers = chbcr_range.as_ref().map(|_| CxlComponentRegisters::new());

        let chbcr = chbcr_range.map(|range| {
            tracing::info!(
                root_bus_start = start_bus,
                root_bus_end = end_bus,
                start = range.start(),
                end = range.end(),
                len = range.len(),
                "pcie root complex CHBCR range"
            );
            let mut region = register_mmio.new_io_region("chbcr", range.len());
            region.map(range.start());
            region
        });

        let mut devices: Vec<(u8, BusDevice)> = Vec::new();

        if let Some((ports, msi_target)) = root_ports {
            let port_count = ports.len();
            let max = 32usize.saturating_sub(first_port_device_number as usize) * 8;
            if port_count > max {
                return Err(InvalidRootComplexError::TooManyPorts { port_count, max });
            }

            // Assign each root port a devfn (honoring explicit requests and
            // filling the rest from `first_port_device_number`), shared with
            // the switch downstream-port assignment.
            let placements = crate::assign_port_devfns(&ports, first_port_device_number)?;

            for (i, (definition, placement)) in ports.into_iter().zip(placements).enumerate() {
                let device = placement.devfn >> crate::BDF_DEVICE_SHIFT;
                if device_number_is_reserved(reserved_device_numbers, device) {
                    return Err(InvalidRootComplexError::ReservedDeviceCollision {
                        port_index: i,
                        device,
                    });
                }
                let hotplug_slot_number = if definition.hotplug {
                    Some(i as u32 + 1)
                } else {
                    None
                };
                let port_msi_target = msi_target.with_devfn(placement.devfn);
                let root_port = RootPort::new(
                    register_mmio,
                    definition.name.clone(),
                    placement.multi_function,
                    hotplug_slot_number,
                    &port_msi_target,
                    definition.settings,
                );
                devices.push((
                    placement.devfn,
                    BusDevice::RootPort {
                        name: definition.name,
                        port: Box::new(root_port),
                    },
                ));
            }

            // `devices` is searched with `binary_search_by_key` on devfn, so it
            // must be kept sorted. Explicit devfns may be assigned out of
            // order, so sort here.
            devices.sort_by_key(|(devfn, _)| *devfn);
        }

        Ok(GenericPcieRootComplex {
            start_bus,
            end_bus,
            ecam,
            chbcr,
            cxl_component_registers,
            devices,
            reserved_device_numbers,
            bus_cfg_handler: PciBusCfgAccessHandler::new(),
        })
    }
}

impl GenericPcieRootComplex {
    /// Returns a builder for constructing a new `GenericPcieRootComplex`.
    pub fn builder<'a>(
        register_mmio: &'a mut dyn RegisterMmioIntercept,
        bus_range: RangeInclusive<u8>,
        ecam_range: MemoryRange,
    ) -> GenericPcieRootComplexBuilder<'a> {
        assert_eq!(
            ecam_size_from_bus_numbers(*bus_range.start(), *bus_range.end()),
            ecam_range.len(),
            "ECAM range size does not match bus range"
        );

        GenericPcieRootComplexBuilder {
            register_mmio,
            bus_range,
            ecam_range,
            root_ports: None,
            first_port_device_number: 0,
            reserved_device_numbers: 0,
            chbcr_range: None,
        }
    }

    /// Reads CHBCR bytes from the CXL component-register backing object.
    ///
    /// `offset` is relative to the start of the CHBCR MMIO range.
    fn read_chbcr_component_registers(&self, offset: u16, data: &mut [u8]) -> IoResult {
        let Some(component_regs) = &self.cxl_component_registers else {
            data.fill(0);
            return IoResult::Ok;
        };

        match component_regs.read(offset, data) {
            IoResult::Err(IoError::InvalidRegister) => {
                // Treat unmapped CHBCR offsets as reserved MMIO reads.
                data.fill(0);
                IoResult::Ok
            }
            res => res,
        }
    }

    /// Writes CHBCR bytes into the CXL component-register backing object.
    ///
    /// `offset` is relative to the start of the CHBCR MMIO range.
    fn write_chbcr_component_registers(&mut self, offset: u16, data: &[u8]) -> IoResult {
        let Some(component_regs) = &mut self.cxl_component_registers else {
            return IoResult::Ok;
        };

        match component_regs.write(offset, data) {
            // Treat unmapped CHBCR offsets as handled MMIO writes.
            IoResult::Err(IoError::InvalidRegister) => IoResult::Ok,
            res => res,
        }
    }

    /// Attach the provided `GenericPciBusDevice` to the port identified by
    /// its devfn (device << 3 | function).
    pub fn add_pcie_device(
        &mut self,
        port_devfn: u8,
        name: impl AsRef<str>,
        dev: Box<dyn GenericPciBusDevice>,
    ) -> Result<(), Arc<str>> {
        let root_port = match self.devices.iter_mut().find(|(d, _)| *d == port_devfn) {
            Some((_, BusDevice::RootPort { port, .. })) => port,
            Some((_, BusDevice::Rciep { name: existing, .. })) => return Err(existing.clone()),
            None => return Err(format!("devfn {port_devfn} is not a root port").into()),
        };

        root_port.connect_device(name, dev)
    }

    /// Enumerate the downstream ports of the root complex.
    pub fn downstream_ports(&self) -> Vec<DownstreamPortInfo> {
        self.devices
            .iter()
            .filter_map(|(devfn, d)| match d {
                BusDevice::RootPort { name, port } => Some(DownstreamPortInfo {
                    devfn: *devfn,
                    name: name.clone(),
                    bus_range: port.port.bus_range(),
                }),
                BusDevice::Rciep { .. } => None,
            })
            .collect()
    }

    /// Hot-add a device to a named port.
    pub fn hotplug_add_device(
        &mut self,
        port_name: &str,
        device_name: &str,
        device: Box<dyn GenericPciBusDevice>,
    ) -> anyhow::Result<()> {
        let root_port = self
            .devices
            .iter_mut()
            .find_map(|(_, d)| match d {
                BusDevice::RootPort { name, port } if name.as_ref() == port_name => Some(port),
                BusDevice::RootPort { .. } | BusDevice::Rciep { .. } => None,
            })
            .ok_or_else(|| anyhow::anyhow!("port '{}' not found", port_name))?;
        root_port.port.hotplug_add_device(device_name, device)
    }

    /// Hot-remove the device from a named port.
    pub fn hotplug_remove_device(&mut self, port_name: &str) -> anyhow::Result<()> {
        let root_port = self
            .devices
            .iter_mut()
            .find_map(|(_, d)| match d {
                BusDevice::RootPort { name, port } if name.as_ref() == port_name => Some(port),
                BusDevice::RootPort { .. } | BusDevice::Rciep { .. } => None,
            })
            .ok_or_else(|| anyhow::anyhow!("port '{}' not found", port_name))?;
        root_port.port.hotplug_remove_device()
    }

    /// Attach a Root Complex Integrated Endpoint (RCiEP) at the given
    /// devfn (device << 3 | function) on the start bus of this root complex.
    ///
    /// RCiEPs are Type 0 PCI functions that appear directly on the start
    /// bus alongside root ports (e.g., an AMD IOMMU at device 0).
    /// They do not sit behind a downstream port and have a fixed BDF.
    ///
    /// Most RCiEPs should be registered at function 0. Config space
    /// accesses to other functions of the same device will be forwarded
    /// to the function 0 device via
    /// [`pci_cfg_read_with_routing`](GenericPciBusDevice::pci_cfg_read_with_routing),
    /// whose default implementation returns all-1s (no device present).
    /// A multi-function RCiEP should override `pci_cfg_read_with_routing`
    /// and `pci_cfg_write_with_routing` to handle non-zero functions.
    pub fn add_rciep(
        &mut self,
        devfn: u8,
        name: impl Into<Arc<str>>,
        dev: Box<dyn GenericPciBusDevice>,
    ) -> Result<(), Arc<str>> {
        let device = devfn >> crate::BDF_DEVICE_SHIFT;
        if device_number_is_reserved(self.reserved_device_numbers, device) {
            return Err(format!("reserved device number {device:#x}").into());
        }

        let name = name.into();
        match self.devices.binary_search_by_key(&devfn, |(d, _)| *d) {
            Ok(i) => {
                let existing = match &self.devices[i].1 {
                    BusDevice::RootPort { name, .. } | BusDevice::Rciep { name, .. } => {
                        name.clone()
                    }
                };
                return Err(existing);
            }
            Err(i) => {
                self.devices
                    .insert(i, (devfn, BusDevice::Rciep { name, dev }));
            }
        }
        Ok(())
    }

    fn parse_ecam_access(
        &self,
        addr: u64,
        len: usize,
    ) -> Result<(PciConfigAddress, PciConfigByteEnable), IoError> {
        let ecam_offset = self.ecam.offset_of(addr).ok_or(IoError::InvalidRegister)?;
        let ecam_based_bdf = (ecam_offset >> PAGE_SHIFT) as u16;
        let bus = ((ecam_based_bdf >> BDF_BUS_SHIFT) as u8) + self.start_bus;
        let devfn = (ecam_based_bdf & BDF_DEVICE_FUNCTION_MASK) as u8;
        let offset = (ecam_offset & PAGE_OFFSET_MASK) as u16;

        let addr = PciConfigAddress::new(bus, devfn, offset / 4).ok_or(IoError::InvalidRegister)?;
        let byte_enable = PciConfigByteEnable::from_offset_len(offset, len)?;
        Ok((addr, byte_enable))
    }

    fn mmio_read_non_ecam(&mut self, addr: u64, data: &mut [u8]) -> Option<IoResult> {
        if let Some(chbcr) = &self.chbcr {
            if let Some(offset) = chbcr.offset_of(addr) {
                let Some(offset) = u16::try_from(offset).ok() else {
                    data.fill(0);
                    return Some(IoResult::Ok);
                };
                return Some(self.read_chbcr_component_registers(offset, data));
            }
        }

        for (_, d) in self.devices.iter_mut() {
            if let BusDevice::RootPort { port, .. } = d {
                if let Some((bar, offset)) = port.port.find_bar(addr) {
                    if let Err(err) =
                        validate_aligned_access(addr, data.len(), &BAR_ALLOWED_ACCESS_SIZES)
                    {
                        return Some(IoResult::Err(err));
                    }
                    return Some(port.port.bar_mmio_read(bar, offset, data));
                }
            }
        }

        None
    }

    fn mmio_write_non_ecam(&mut self, addr: u64, data: &[u8]) -> Option<IoResult> {
        if let Some(chbcr) = &self.chbcr {
            if let Some(offset) = chbcr.offset_of(addr) {
                let Some(offset) = u16::try_from(offset).ok() else {
                    return Some(IoResult::Err(IoError::InvalidRegister));
                };
                return Some(self.write_chbcr_component_registers(offset, data));
            }
        }

        for (_, d) in self.devices.iter_mut() {
            if let BusDevice::RootPort { port, .. } = d {
                if let Some((bar, offset)) = port.port.find_bar(addr) {
                    if let Err(err) =
                        validate_aligned_access(addr, data.len(), &BAR_ALLOWED_ACCESS_SIZES)
                    {
                        return Some(IoResult::Err(err));
                    }
                    return Some(port.port.bar_mmio_write(bar, offset, data));
                }
            }
        }

        None
    }
}

fn ecam_size_from_bus_numbers(start_bus: u8, end_bus: u8) -> u64 {
    assert!(end_bus >= start_bus);
    let bus_count = (end_bus as u16) - (start_bus as u16) + 1;
    (bus_count as u64) * (MAX_FUNCTIONS_PER_BUS as u64) * PAGE_SIZE64
}

impl ChangeDeviceState for GenericPcieRootComplex {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        for (_, d) in self.devices.iter_mut() {
            if let BusDevice::RootPort { port, .. } = d {
                port.port.cfg_space.reset();
            }
        }
    }
}

impl ChipsetDevice for GenericPcieRootComplex {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PollDevice for GenericPcieRootComplex {
    fn poll_device(&mut self, cx: &mut std::task::Context<'_>) {
        let mut callback =
            PciBusCfgAccessCallbackView::new(&self.start_bus, &self.end_bus, &mut self.devices);
        self.bus_cfg_handler.poll(cx, &mut callback);
    }
}

const BAR_ALLOWED_ACCESS_SIZES: [usize; 4] = [1, 2, 4, 8];

fn validate_aligned_access(
    address: u64,
    len: usize,
    allowed_sizes: &[usize],
) -> Result<(), IoError> {
    if !allowed_sizes.contains(&len) {
        return Err(IoError::InvalidAccessSize);
    }

    if !address.is_multiple_of(len as u64) {
        return Err(IoError::UnalignedAccess);
    }

    Ok(())
}

impl MmioIntercept for GenericPcieRootComplex {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        if let Some(result) = self.mmio_read_non_ecam(addr, data) {
            return result;
        }

        let (address, byte_enable) = match self.parse_ecam_access(addr, data.len()) {
            Ok(result) => result,
            Err(err) => return IoResult::Err(err),
        };

        let mut value_u32 = !0;
        let mut value = ByteEnabledDwordRead::new(&mut value_u32, byte_enable);
        let mut callback =
            PciBusCfgAccessCallbackView::new(&self.start_bus, &self.end_bus, &mut self.devices);

        let result = self
            .bus_cfg_handler
            .read(address, value.reborrow(), &mut callback);

        if matches!(result, IoResult::Ok) {
            value.fill_intercept_buffer(data);
        }

        result
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        if let Some(result) = self.mmio_write_non_ecam(addr, data) {
            return result;
        }

        let (address, byte_enable) = match self.parse_ecam_access(addr, data.len()) {
            Ok(result) => result,
            Err(err) => return IoResult::Err(err),
        };

        let value = ByteEnabledDwordWrite::from_intercept_buffer(byte_enable, data);
        let mut callback =
            PciBusCfgAccessCallbackView::new(&self.start_bus, &self.end_bus, &mut self.devices);
        self.bus_cfg_handler.write(address, value, &mut callback)
    }
}

/// The target of a PCIe configuration space access.
enum CfgAccessTarget<'a> {
    /// The access targets a Root Complex Integrated Endpoint (RCiEP) on
    /// the internal bus of the root complex.
    Rciep(&'a mut dyn GenericPciBusDevice),
    /// The access targets a root port on the internal bus of the root
    /// complex.
    RootPort(&'a mut RootPort),
    /// The access targets a device function assigned to the hierarchy
    /// underneath of a root port.
    DownstreamDevice(&'a mut RootPort),
}

struct PciBusCfgAccessCallbackView<'a> {
    start_bus: &'a u8,
    end_bus: &'a u8,
    devices: &'a mut Vec<(u8, BusDevice)>,
}

impl<'a> PciBusCfgAccessCallbackView<'a> {
    fn new(start_bus: &'a u8, end_bus: &'a u8, devices: &'a mut Vec<(u8, BusDevice)>) -> Self {
        Self {
            start_bus,
            end_bus,
            devices,
        }
    }

    fn route_cfg_access<'b>(&'b mut self, addr: PciConfigAddress) -> Option<CfgAccessTarget<'b>> {
        //fn route_cfg_access<'a>(&'a mut self, addr: PciConfigAddress) -> Option<CfgAccessTarget<'a>> {
        if addr.bus == *self.start_bus {
            // Look up the exact devfn first; if not found, fall back to
            // function 0 of the same device so that multi-function
            // endpoints can handle the access via
            // `pci_cfg_read_with_routing`.
            let devfn_fn0 = addr.device_function & !7;
            let mut idx = None;
            let mut exact = false;
            for (i, (d, _)) in self.devices.iter().enumerate() {
                if *d == addr.device_function {
                    idx = Some(i);
                    exact = true;
                    break;
                }
                if *d == devfn_fn0 {
                    idx = Some(i);
                }
                if *d > addr.device_function {
                    break;
                }
            }
            match idx.map(|i| (exact, &mut self.devices[i].1)) {
                // Exact devfn match for a root port — return its config space.
                Some((true, BusDevice::RootPort { port, .. })) => {
                    return Some(CfgAccessTarget::RootPort(port));
                }
                // Fallback (fn0) match for a root port — the target function
                // is not a root port, so this devfn is unroutable.
                Some((false, BusDevice::RootPort { .. })) => {
                    return None;
                }
                Some((_, BusDevice::Rciep { dev, .. })) => {
                    return Some(CfgAccessTarget::Rciep(dev.as_mut()));
                }
                _ => {
                    return None;
                }
            }
        } else if addr.bus > *self.start_bus && addr.bus <= *self.end_bus {
            for (_, d) in self.devices.iter_mut() {
                if let BusDevice::RootPort { port, .. } = d {
                    if port.port.cfg_space.assigned_bus_range().contains(&addr.bus) {
                        return Some(CfgAccessTarget::DownstreamDevice(port));
                    }
                }
            }
        }

        None
    }
}

impl<'a> PciBusCfgAccessCallbacks for PciBusCfgAccessCallbackView<'a> {
    fn read(&mut self, addr: PciConfigAddress, value: &mut u32) -> IoResult {
        let Some(target) = self.route_cfg_access(addr) else {
            tracing::trace!(?addr, "unroutable config space access");
            *value = !0;
            return IoResult::Ok;
        };

        match target {
            CfgAccessTarget::Rciep(dev) => dev
                .pci_cfg_read_with_routing(
                    addr.bus,
                    addr.bus,
                    addr.device_function,
                    addr.byte_offset(),
                    value,
                )
                .unwrap_or_else(|| {
                    *value = !0;
                    IoResult::Ok
                }),
            CfgAccessTarget::RootPort(port) => {
                port.port.cfg_space.read_u32(addr.byte_offset(), value)
            }
            CfgAccessTarget::DownstreamDevice(port) => port.forward_cfg_read(addr, value),
        }
    }

    fn write(&mut self, addr: PciConfigAddress, value: u32) -> IoResult {
        let Some(target) = self.route_cfg_access(addr) else {
            tracing::trace!(?addr, "unroutable config space access");
            return IoResult::Ok;
        };

        match target {
            CfgAccessTarget::Rciep(dev) => dev
                .pci_cfg_write_with_routing(
                    addr.bus,
                    addr.bus,
                    addr.device_function,
                    addr.byte_offset(),
                    value,
                )
                .unwrap_or(IoResult::Ok),
            CfgAccessTarget::RootPort(port) => {
                port.port.cfg_space.write_u32(addr.byte_offset(), value)
            }
            CfgAccessTarget::DownstreamDevice(port) => port.forward_cfg_write(addr, value),
        }
    }
}

#[derive(Inspect)]
struct RootPort {
    /// The common PCIe port implementation.
    #[inspect(flatten)]
    port: PcieDownstreamPort,
}

impl RootPort {
    /// Constructs a new [`RootPort`] emulator.
    ///
    /// # Arguments
    /// * `name` - The name for this root port
    /// * `hotplug_slot_number` - The slot number for hotplug support. `Some(slot_number)` enables hotplug, `None` disables it
    /// * `msi_target` - MSI target for interrupt delivery
    /// * `settings` - Express-level port settings (ACS, etc.)
    pub fn new(
        register_mmio: &mut dyn RegisterMmioIntercept,
        name: impl Into<Arc<str>>,
        multi_function: bool,
        hotplug_slot_number: Option<u32>,
        msi_target: &MsiTarget,
        settings: PciePortSettings,
    ) -> Self {
        let name_str = name.into();

        let hardware_ids = HardwareIds {
            vendor_id: VENDOR_ID,
            device_id: ROOT_PORT_DEVICE_ID,
            revision_id: 0,
            prog_if: ProgrammingInterface::NONE,
            sub_class: Subclass::BRIDGE_PCI_TO_PCI,
            base_class: ClassCode::BRIDGE,
            type0_sub_vendor_id: 0,
            type0_sub_system_id: 0,
        };

        let port = PcieDownstreamPort::new(
            name_str.to_string(),
            hardware_ids,
            DevicePortType::RootPort,
            multi_function,
            hotplug_slot_number,
            msi_target,
            settings,
            Some(register_mmio),
            None,
        );

        Self { port }
    }

    /// Try to connect a PCIe device, returning an existing device name if the
    /// port is already occupied.
    fn connect_device(
        &mut self,
        name: impl AsRef<str>,
        dev: Box<dyn GenericPciBusDevice>,
    ) -> Result<(), Arc<str>> {
        let device_name = name.as_ref();
        let port_name = self.port.name.clone();

        match self.port.add_pcie_device(&port_name, device_name, dev) {
            Ok(()) => Ok(()),
            Err(_error) => {
                // If the connection failed, it means the port is already occupied
                // We need to get the name of the existing device
                if let Some((existing_name, _)) = &self.port.link {
                    tracing::warn!(
                        "RootPort: '{}' failed to connect device '{}', port already occupied by '{}'",
                        port_name,
                        device_name,
                        existing_name
                    );
                    Err(existing_name.clone())
                } else {
                    // This shouldn't happen if add_pcie_device works correctly
                    tracing::error!(
                        "RootPort: '{}' connection failed for device '{}' but no existing device found",
                        port_name,
                        device_name
                    );
                    panic!("Port connection failed but no existing device found")
                }
            }
        }
    }

    fn forward_cfg_read(&mut self, addr: PciConfigAddress, value: &mut u32) -> IoResult {
        self.port.forward_cfg_read_with_routing(addr, value)
    }

    fn forward_cfg_write(&mut self, addr: PciConfigAddress, value: u32) -> IoResult {
        self.port.forward_cfg_write_with_routing(addr, value)
    }
}

mod save_restore {
    use super::*;
    use pci_core::cfg_space_emu::ConfigSpaceType1Emulator;
    use std::collections::HashSet;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use super::ConfigSpaceType1Emulator;
        use super::CxlComponentRegisters;
        use super::SaveRestore;
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        type RootPortCfgSpaceSavedState = <ConfigSpaceType1Emulator as SaveRestore>::SavedState;
        type CxlComponentRegistersSavedState = <CxlComponentRegisters as SaveRestore>::SavedState;

        /// Saved state for a single root port.
        #[derive(Protobuf)]
        #[mesh(package = "pcie.root")]
        pub struct PortSavedState {
            /// The devfn (device << 3 | function) of this port.
            #[mesh(1)]
            pub devfn: u8,
            /// The root port Type 1 configuration space state.
            #[mesh(2)]
            pub cfg_space: RootPortCfgSpaceSavedState,
            /// Optional CXL component-register state for this port.
            #[mesh(3)]
            pub cxl_component_registers: Option<CxlComponentRegistersSavedState>,
        }

        /// Saved state for the GenericPcieRootComplex.
        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "pcie.root")]
        pub struct SavedState {
            /// The lowest valid bus number under the root complex.
            #[mesh(1)]
            pub start_bus: u8,
            /// The highest valid bus number under the root complex.
            #[mesh(2)]
            pub end_bus: u8,
            /// Saved state for each root port.
            #[mesh(3)]
            pub ports: Vec<PortSavedState>,
            /// Optional CXL component-register state for CHBCR-backed root complexes.
            #[mesh(4)]
            pub cxl_component_registers: Option<CxlComponentRegistersSavedState>,
        }
    }

    impl SaveRestore for GenericPcieRootComplex {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            // Save all root ports in devfn order.
            let mut ports = Vec::new();
            for (devfn, d) in self.devices.iter_mut() {
                if let BusDevice::RootPort { port, .. } = d {
                    ports.push(state::PortSavedState {
                        devfn: *devfn,
                        cfg_space: port.port.cfg_space.save()?,
                        cxl_component_registers: port.port.save_cxl_component_registers_state()?,
                    });
                }
            }

            Ok(state::SavedState {
                start_bus: self.start_bus,
                end_bus: self.end_bus,
                ports,
                cxl_component_registers: self
                    .cxl_component_registers
                    .as_mut()
                    .map(|regs| regs.save())
                    .transpose()?,
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                start_bus,
                end_bus,
                ports,
                cxl_component_registers,
            } = state;

            // Validate that bus numbers match
            if start_bus != self.start_bus || end_bus != self.end_bus {
                return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                    "bus number mismatch: saved ({}-{}), current ({}-{})",
                    start_bus,
                    end_bus,
                    self.start_bus,
                    self.end_bus
                )));
            }

            // Validate port count matches
            let root_port_count = self
                .devices
                .iter()
                .filter(|(_, d)| matches!(d, BusDevice::RootPort { .. }))
                .count();
            if ports.len() != root_port_count {
                return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                    "root port count mismatch: saved {}, current {}",
                    ports.len(),
                    root_port_count
                )));
            }

            let mut seen_ports = HashSet::with_capacity(ports.len());

            // Restore each saved port by devfn.
            for port_state in ports {
                if !seen_ports.insert(port_state.devfn) {
                    return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                        "duplicate root port devfn {} in saved state",
                        port_state.devfn
                    )));
                }

                match self
                    .devices
                    .iter_mut()
                    .find(|(d, _)| *d == port_state.devfn)
                {
                    Some((_, BusDevice::RootPort { port, .. })) => {
                        port.port.cfg_space.restore(port_state.cfg_space)?;
                        port.port.restore_cxl_component_registers_state(
                            port_state.cxl_component_registers,
                        )?;
                    }
                    Some((_, BusDevice::Rciep { .. })) | None => {
                        return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                            "root port devfn {} not found",
                            port_state.devfn
                        )));
                    }
                }
            }

            match (&mut self.cxl_component_registers, cxl_component_registers) {
                (Some(current), Some(saved)) => current.restore(saved)?,
                (None, None) => {}
                (Some(_), None) => {
                    return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                        "CXL mode mismatch: current root complex has CHBCR registers but saved state does not"
                    )));
                }
                (None, Some(_)) => {
                    return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                        "CXL mode mismatch: saved state has CHBCR registers but current root complex does not"
                    )));
                }
            }

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::switch::GenericPcieSwitch;
    use crate::switch::GenericPcieSwitchDefinition;
    use crate::test_helpers::*;
    use chipset_device::io::deferred::DeferredRead;
    use chipset_device::io::deferred::DeferredWrite;
    use chipset_device::io::deferred::defer_read;
    use chipset_device::io::deferred::defer_write;
    use chipset_device::pci::PciConfigSpace;
    use cxl_spec::CxlComponentRegisterType;
    use cxl_spec::component_registers::test_helper::TestCxlComponentRegisterBlock;
    use pal_async::async_test;
    use parking_lot::Mutex;
    use zerocopy::IntoBytes;

    struct DeferredEndpoint {
        state: Arc<Mutex<DeferredEndpointState>>,
    }

    struct DeferredEndpointState {
        read_value: u32,
        defer_reads: bool,
        defer_writes: bool,
        pending_read: Option<DeferredRead>,
        pending_write: Option<DeferredWrite>,
        writes: Vec<(u16, u32)>,
    }

    impl DeferredEndpointState {
        fn new(read_value: u32) -> Self {
            Self {
                read_value,
                defer_reads: false,
                defer_writes: false,
                pending_read: None,
                pending_write: None,
                writes: Vec::new(),
            }
        }
    }

    impl GenericPciBusDevice for DeferredEndpoint {
        fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> Option<IoResult> {
            let mut state = self.state.lock();
            if state.defer_reads {
                let (deferred, token) = defer_read();
                assert!(state.pending_read.replace(deferred).is_none());
                Some(IoResult::Defer(token))
            } else {
                assert_eq!(offset, 0);
                *value = state.read_value;
                Some(IoResult::Ok)
            }
        }

        fn pci_cfg_write(&mut self, offset: u16, value: u32) -> Option<IoResult> {
            let mut state = self.state.lock();
            state.writes.push((offset, value));
            if state.defer_writes {
                let (deferred, token) = defer_write();
                assert!(state.pending_write.replace(deferred).is_none());
                Some(IoResult::Defer(token))
            } else {
                Some(IoResult::Ok)
            }
        }
    }

    struct SwitchAdapter(Arc<Mutex<GenericPcieSwitch>>);

    impl GenericPciBusDevice for SwitchAdapter {
        fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> Option<IoResult> {
            Some(self.0.lock().pci_cfg_read(offset, value))
        }

        fn pci_cfg_write(&mut self, offset: u16, value: u32) -> Option<IoResult> {
            Some(self.0.lock().pci_cfg_write(offset, value))
        }

        fn pci_cfg_read_with_routing(
            &mut self,
            secondary_bus: u8,
            target_bus: u8,
            function: u8,
            offset: u16,
            value: &mut u32,
        ) -> Option<IoResult> {
            Some(self.0.lock().pci_cfg_read_with_routing(
                secondary_bus,
                target_bus,
                function,
                offset,
                value,
            ))
        }

        fn pci_cfg_write_with_routing(
            &mut self,
            secondary_bus: u8,
            target_bus: u8,
            function: u8,
            offset: u16,
            value: u32,
        ) -> Option<IoResult> {
            Some(self.0.lock().pci_cfg_write_with_routing(
                secondary_bus,
                target_bus,
                function,
                offset,
                value,
            ))
        }
    }

    fn instantiate_root_complex(
        start_bus: u8,
        end_bus: u8,
        port_count: u16,
    ) -> GenericPcieRootComplex {
        let port_defs = (0..port_count)
            .map(|i| GenericPciePortDefinition {
                name: format!("test-port-{}", i).into(),
                devfn: None,
                hotplug: false,
                settings: PciePortSettings::default(),
            })
            .collect();

        let mut register_mmio = TestPcieMmioRegistration {};
        let ecam = MemoryRange::new(0..ecam_size_from_bus_numbers(start_bus, end_bus));
        let rc_bus_range = AssignedBusRange::new();
        rc_bus_range.set_bus_range(start_bus, end_bus);
        let msi_conn = pci_core::msi::MsiConnection::new();
        GenericPcieRootComplex::builder(&mut register_mmio, start_bus..=end_bus, ecam)
            .root_ports(port_defs, &msi_conn.msi_target(rc_bus_range, 0))
            .build()
            .unwrap()
    }

    fn instantiate_root_complex_with_chbcr(
        start_bus: u8,
        end_bus: u8,
        port_count: u16,
        chbcr_start: u64,
    ) -> GenericPcieRootComplex {
        let port_defs = (0..port_count)
            .map(|i| GenericPciePortDefinition {
                name: format!("test-port-{}", i).into(),
                devfn: None,
                hotplug: false,
                settings: PciePortSettings::default(),
            })
            .collect();

        let mut register_mmio = TestPcieMmioRegistration {};
        let ecam = MemoryRange::new(0..ecam_size_from_bus_numbers(start_bus, end_bus));
        let chbcr = MemoryRange::new(
            chbcr_start..(chbcr_start + cxl_spec::spec::CXL_COMPONENT_REGISTERS_SIZE_BYTES),
        );
        let rc_bus_range = AssignedBusRange::new();
        rc_bus_range.set_bus_range(start_bus, end_bus);
        let msi_conn = pci_core::msi::MsiConnection::new();

        GenericPcieRootComplex::builder(&mut register_mmio, start_bus..=end_bus, ecam)
            .root_ports(port_defs, &msi_conn.msi_target(rc_bus_range, 0))
            .chbcr_range(Some(chbcr))
            .build()
            .unwrap()
    }

    fn poll_root(rc: &mut GenericPcieRootComplex) {
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        rc.poll_device(&mut cx);
    }

    fn poll_root_and_switch(
        rc: &mut GenericPcieRootComplex,
        switch: &Arc<Mutex<GenericPcieSwitch>>,
    ) {
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        rc.poll_device(&mut cx);
        switch.lock().poll_device(&mut cx);
        rc.poll_device(&mut cx);
        switch.lock().poll_device(&mut cx);
    }

    #[test]
    fn test_create() {
        assert_eq!(
            instantiate_root_complex(0, 0, 1).downstream_ports().len(),
            1
        );
        assert_eq!(
            instantiate_root_complex(0, 1, 1).downstream_ports().len(),
            1
        );
        assert_eq!(
            instantiate_root_complex(1, 1, 1).downstream_ports().len(),
            1
        );
        assert_eq!(
            instantiate_root_complex(255, 255, 1)
                .downstream_ports()
                .len(),
            1
        );

        assert_eq!(
            instantiate_root_complex(0, 0, 4).downstream_ports().len(),
            4
        );

        assert_eq!(
            instantiate_root_complex(0, 255, 32)
                .downstream_ports()
                .len(),
            32
        );
        assert_eq!(
            instantiate_root_complex(32, 32, 32)
                .downstream_ports()
                .len(),
            32
        );
        assert_eq!(
            instantiate_root_complex(255, 255, 32)
                .downstream_ports()
                .len(),
            32
        );

        // Multi-function packing allows more than 32 ports.
        assert_eq!(
            instantiate_root_complex(0, 255, 64)
                .downstream_ports()
                .len(),
            64
        );
        assert_eq!(
            instantiate_root_complex(0, 255, 256)
                .downstream_ports()
                .len(),
            256
        );
    }

    #[test]
    fn test_ecam_size() {
        // Single bus
        assert_eq!(ecam_size_from_bus_numbers(0, 0), 0x10_0000);
        assert_eq!(ecam_size_from_bus_numbers(32, 32), 0x10_0000);
        assert_eq!(ecam_size_from_bus_numbers(255, 255), 0x10_0000);

        // Two bus
        assert_eq!(ecam_size_from_bus_numbers(0, 1), 0x20_0000);
        assert_eq!(ecam_size_from_bus_numbers(32, 33), 0x20_0000);
        assert_eq!(ecam_size_from_bus_numbers(254, 255), 0x20_0000);

        // Everything
        assert_eq!(ecam_size_from_bus_numbers(0, 255), 0x1000_0000);
    }

    #[test]
    fn test_probe_ports_via_config_space() {
        let mut rc = instantiate_root_complex(0, 255, 4);
        // With multi-function packing, 4 ports are at devfn 0..3
        // (device 0, functions 0..3). ECAM offset = devfn * 4096.
        for devfn in 0..4u64 {
            let mut vendor_device: u32 = 0;
            rc.mmio_read(devfn * 4096, vendor_device.as_mut_bytes())
                .unwrap();
            assert_eq!(vendor_device, 0xC030_1414);

            let mut value_16: u16 = 0;
            rc.mmio_read(devfn * 4096, value_16.as_mut_bytes()).unwrap();
            assert_eq!(value_16, 0x1414);

            rc.mmio_read(devfn * 4096 + 2, value_16.as_mut_bytes())
                .unwrap();
            assert_eq!(value_16, 0xC030);
        }

        for devfn in 4..10u64 {
            let mut value_32: u32 = 0;
            rc.mmio_read(devfn * 4096, value_32.as_mut_bytes()).unwrap();
            assert_eq!(value_32, 0xFFFF_FFFF);

            let mut value_16: u16 = 0;
            rc.mmio_read(devfn * 4096, value_16.as_mut_bytes()).unwrap();
            assert_eq!(value_16, 0xFFFF);
            rc.mmio_read(devfn * 4096 + 2, value_16.as_mut_bytes())
                .unwrap();
            assert_eq!(value_16, 0xFFFF);
        }
    }

    #[test]
    fn test_add_downstream_device_to_port() {
        let mut rc = instantiate_root_complex(0, 0, 1);

        let endpoint1 = TestPcieEndpoint::new(
            |offset, value| match offset {
                0x0 => {
                    *value = 0xAAAA_AAAA;
                    Some(IoResult::Ok)
                }
                _ => Some(IoResult::Err(IoError::InvalidRegister)),
            },
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
        );

        let endpoint2 = TestPcieEndpoint::new(
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
        );

        rc.add_pcie_device(0, "ep1", Box::new(endpoint1)).unwrap();

        rc.add_pcie_device(0, "ep2", Box::new(endpoint2))
            .expect_err("should fail: port already occupied");
    }

    #[test]
    fn test_root_port_cfg_forwarding() {
        const SECONDARY_BUS_NUM_REG: u64 = 0x19;
        const SUBOORDINATE_BUS_NUM_REG: u64 = 0x1A;

        let mut rc = instantiate_root_complex(0, 255, 1);

        // Pre-bus number assignment, random accesses return 1s.
        let mut value_32: u32 = 0;
        rc.mmio_read(256 * 4096, value_32.as_mut_bytes()).unwrap();
        assert_eq!(value_32, 0xFFFF_FFFF);

        // Secondary and suboordinate bus number registers are both
        // read / write, defaulting to 0.
        let mut bus_number: u8 = 0xFF;
        rc.mmio_read(SECONDARY_BUS_NUM_REG, bus_number.as_mut_bytes())
            .unwrap();
        assert_eq!(bus_number, 0);
        rc.mmio_read(SUBOORDINATE_BUS_NUM_REG, bus_number.as_mut_bytes())
            .unwrap();
        assert_eq!(bus_number, 0);

        rc.mmio_write(SECONDARY_BUS_NUM_REG, &[1]).unwrap();
        rc.mmio_read(SECONDARY_BUS_NUM_REG, bus_number.as_mut_bytes())
            .unwrap();
        assert_eq!(bus_number, 1);

        rc.mmio_write(SUBOORDINATE_BUS_NUM_REG, &[2]).unwrap();
        rc.mmio_read(SUBOORDINATE_BUS_NUM_REG, bus_number.as_mut_bytes())
            .unwrap();
        assert_eq!(bus_number, 2);

        // Bus numbers assigned, but no endpoint attached yet.
        rc.mmio_read(256 * 4096, value_32.as_mut_bytes()).unwrap();
        assert_eq!(value_32, 0xFFFF_FFFF);

        let endpoint = TestPcieEndpoint::new(
            |offset, value| match offset {
                0x0 => {
                    *value = 0xDEAD_BEEF;
                    Some(IoResult::Ok)
                }
                _ => Some(IoResult::Err(IoError::InvalidRegister)),
            },
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
        );

        rc.add_pcie_device(0, "test-ep", Box::new(endpoint))
            .unwrap();

        // The secondary bus behind root port 0 has been assigned bus number
        // 1, so now the attached endpoint is accessible.
        rc.mmio_read(256 * 4096, value_32.as_mut_bytes()).unwrap();
        assert_eq!(value_32, 0xDEAD_BEEF);

        // Reassign the secondary bus number to 2.
        rc.mmio_write(SECONDARY_BUS_NUM_REG, &[2]).unwrap();
        rc.mmio_read(SECONDARY_BUS_NUM_REG, bus_number.as_mut_bytes())
            .unwrap();
        assert_eq!(bus_number, 2);

        // The endpoint is no longer accessible at bus number 1, and is now
        // accessible at bus number 2.
        rc.mmio_read(256 * 4096, value_32.as_mut_bytes()).unwrap();
        assert_eq!(value_32, 0xFFFF_FFFF);
        rc.mmio_read(2 * 256 * 4096, value_32.as_mut_bytes())
            .unwrap();
        assert_eq!(value_32, 0xDEAD_BEEF);
    }

    #[async_test]
    async fn test_ecam_deferred_downstream_cfg_access() {
        const SECONDARY_BUS_NUM_REG: u64 = 0x19;
        const SUBORDINATE_BUS_NUM_REG: u64 = 0x1A;
        const ENDPOINT_ECAM: u64 = 256 * 4096;

        let mut rc = instantiate_root_complex(0, 255, 1);
        rc.mmio_write(SECONDARY_BUS_NUM_REG, &[1]).unwrap();
        rc.mmio_write(SUBORDINATE_BUS_NUM_REG, &[1]).unwrap();

        let state = Arc::new(Mutex::new(DeferredEndpointState::new(0x1122_3344)));
        rc.add_pcie_device(
            0,
            "deferred-ep",
            Box::new(DeferredEndpoint {
                state: state.clone(),
            }),
        )
        .unwrap();

        state.lock().defer_reads = true;
        let mut read_value = 0;
        let read = rc.mmio_read(ENDPOINT_ECAM, read_value.as_mut_bytes());
        let IoResult::Defer(read_token) = read else {
            panic!("downstream config read should defer");
        };
        state
            .lock()
            .pending_read
            .take()
            .unwrap()
            .complete(&0x5566_7788u32.as_bytes()[..4]);
        poll_root(&mut rc);
        read_token
            .read_future(read_value.as_mut_bytes())
            .await
            .unwrap();
        assert_eq!(read_value, 0x5566_7788);

        let mut read_value = 0;
        let read = rc.mmio_read(ENDPOINT_ECAM, read_value.as_mut_bytes());
        let IoResult::Defer(read_token) = read else {
            panic!("downstream config read should defer");
        };
        state
            .lock()
            .pending_read
            .take()
            .unwrap()
            .complete_error(IoError::NoResponse);
        poll_root(&mut rc);
        assert!(matches!(
            read_token.read_future(read_value.as_mut_bytes()).await,
            Err(IoError::NoResponse)
        ));

        let partial_write = rc.mmio_write(ENDPOINT_ECAM + 1, &[0xaa]);
        let IoResult::Defer(partial_write_token) = partial_write else {
            panic!("sub-dword downstream config write should defer on read-for-write");
        };
        state
            .lock()
            .pending_read
            .take()
            .unwrap()
            .complete(&0x1122_3344u32.as_bytes()[..4]);
        poll_root(&mut rc);
        partial_write_token.write_future().await.unwrap();
        let mut expected = 0x1122_3344u32.to_ne_bytes();
        expected[1] = 0xaa;
        assert_eq!(
            state.lock().writes.pop(),
            Some((0, u32::from_ne_bytes(expected)))
        );

        state.lock().defer_reads = false;
        state.lock().defer_writes = true;
        let full_write = rc.mmio_write(ENDPOINT_ECAM, 0xaabb_ccddu32.as_bytes());
        let IoResult::Defer(full_write_token) = full_write else {
            panic!("full downstream config write should defer through the root complex");
        };
        state.lock().pending_write.take().unwrap().complete();
        poll_root(&mut rc);
        full_write_token.write_future().await.unwrap();
        assert_eq!(state.lock().writes.pop(), Some((0, 0xaabb_ccdd)));

        let full_write = rc.mmio_write(ENDPOINT_ECAM, 0x1122_3344u32.as_bytes());
        let IoResult::Defer(full_write_token) = full_write else {
            panic!("full downstream config write should defer through the root complex");
        };
        state
            .lock()
            .pending_write
            .take()
            .unwrap()
            .complete_error(IoError::NoResponse);
        poll_root(&mut rc);
        assert!(matches!(
            full_write_token.write_future().await,
            Err(IoError::NoResponse)
        ));
    }

    #[async_test]
    async fn test_ecam_deferred_cfg_access_behind_switch_completes_from_poll() {
        const ROOT_SECONDARY_BUS_NUM_REG: u64 = 0x19;
        const ROOT_SUBORDINATE_BUS_NUM_REG: u64 = 0x1A;
        const SWITCH_BUS: u8 = 1;
        const SWITCH_INTERNAL_BUS: u8 = 2;
        const ENDPOINT_BUS: u8 = 3;
        const ENDPOINT_ECAM: u64 = ENDPOINT_BUS as u64 * 256 * 4096;

        let mut rc = instantiate_root_complex(0, 255, 1);
        rc.mmio_write(ROOT_SECONDARY_BUS_NUM_REG, &[SWITCH_BUS])
            .unwrap();
        rc.mmio_write(ROOT_SUBORDINATE_BUS_NUM_REG, &[10]).unwrap();

        let switch = Arc::new(Mutex::new(
            GenericPcieSwitch::new(GenericPcieSwitchDefinition {
                name: "test-switch".into(),
                downstream_ports: vec![GenericPciePortDefinition {
                    name: "a".into(),
                    devfn: None,
                    hotplug: false,
                    settings: PciePortSettings::default(),
                }],
                msi_target: MsiTarget::disconnected(),
            })
            .unwrap(),
        ));

        switch
            .lock()
            .pci_cfg_write_with_routing(
                SWITCH_BUS,
                SWITCH_BUS,
                0,
                0x18,
                (10u32 << 16) | ((SWITCH_INTERNAL_BUS as u32) << 8) | SWITCH_BUS as u32,
            )
            .unwrap();
        switch
            .lock()
            .pci_cfg_write_with_routing(
                SWITCH_BUS,
                SWITCH_INTERNAL_BUS,
                0,
                0x18,
                ((ENDPOINT_BUS as u32) << 16)
                    | ((ENDPOINT_BUS as u32) << 8)
                    | SWITCH_INTERNAL_BUS as u32,
            )
            .unwrap();

        let state = Arc::new(Mutex::new(DeferredEndpointState::new(0x1122_3344)));
        state.lock().defer_reads = true;
        switch
            .lock()
            .add_pcie_device(
                0,
                "deferred-ep",
                Box::new(DeferredEndpoint {
                    state: state.clone(),
                }),
            )
            .unwrap();

        rc.add_pcie_device(0, "switch", Box::new(SwitchAdapter(switch.clone())))
            .unwrap();

        let mut read_value = 0;
        let read = rc.mmio_read(ENDPOINT_ECAM, read_value.as_mut_bytes());
        let IoResult::Defer(read_token) = read else {
            panic!("downstream config read behind switch should defer");
        };

        poll_root_and_switch(&mut rc, &switch);

        state
            .lock()
            .pending_read
            .take()
            .unwrap()
            .complete(&0x5566_7788u32.as_bytes()[..4]);

        poll_root_and_switch(&mut rc, &switch);

        let mut read_data = [0; 4];
        read_token
            .read_future(&mut read_data)
            .await
            .expect("deferred read should complete");
        assert_eq!(u32::from_ne_bytes(read_data), 0x5566_7788);
    }

    #[test]
    fn test_chbcr_reads_cxl_component_header_in_cxl_mode() {
        let chbcr_start = 0x2000_0000;
        let mut rc = instantiate_root_complex_with_chbcr(0, 0, 1, chbcr_start);

        // CHBCR + 0x1000 maps to cache/mem page 0 header dword.
        let mut header: u32 = 0;
        rc.mmio_read(chbcr_start + 0x1000, header.as_mut_bytes())
            .unwrap();
        assert_eq!(header, 0x0011_0001);
    }

    #[test]
    fn test_chbcr_read_write_redirects_to_component_registers() {
        let chbcr_start = 0x3000_0000;
        let mut rc = instantiate_root_complex_with_chbcr(0, 0, 1, chbcr_start);

        // Install one payload register block into the component-register space.
        assert!(
            rc.cxl_component_registers
                .as_mut()
                .expect("CXL mode must allocate component registers")
                .add_register(Box::new(TestCxlComponentRegisterBlock::new(
                    CxlComponentRegisterType::CXL_CACHE_MEM_REGISTER,
                    16,
                )))
        );

        let value = 0x1122_3344u32;
        rc.mmio_write(chbcr_start + 0x1008, value.as_bytes())
            .unwrap();

        let mut read_back: u32 = 0;
        rc.mmio_read(chbcr_start + 0x1008, read_back.as_mut_bytes())
            .unwrap();
        assert_eq!(read_back, value);
    }

    #[test]
    fn test_chbcr_8byte_read_write_redirects_to_component_registers() {
        let chbcr_start = 0x3001_0000;
        let mut rc = instantiate_root_complex_with_chbcr(0, 0, 1, chbcr_start);

        // Install one payload register block into the component-register space.
        assert!(
            rc.cxl_component_registers
                .as_mut()
                .expect("CXL mode must allocate component registers")
                .add_register(Box::new(TestCxlComponentRegisterBlock::new(
                    CxlComponentRegisterType::CXL_CACHE_MEM_REGISTER,
                    16,
                )))
        );

        let value = 0x1122_3344_5566_7788u64;
        rc.mmio_write(chbcr_start + 0x1008, value.as_bytes())
            .unwrap();

        let mut read_back: u64 = 0;
        rc.mmio_read(chbcr_start + 0x1008, read_back.as_mut_bytes())
            .unwrap();
        assert_eq!(read_back, value);
    }

    #[test]
    fn test_chbcr_unmapped_read_is_handled() {
        let chbcr_start = 0x3002_0000;
        let mut rc = instantiate_root_complex_with_chbcr(0, 0, 1, chbcr_start);

        // This offset is not synthesized/populated and should still be
        // treated as a handled CHBCR MMIO read.
        let mut value: u64 = 0;
        rc.mmio_read(chbcr_start + 0x800, value.as_mut_bytes())
            .unwrap();
        assert_eq!(value, 0);
    }

    #[test]
    fn test_chbcr_unmapped_write_is_handled() {
        let chbcr_start = 0x3003_0000;
        let mut rc = instantiate_root_complex_with_chbcr(0, 0, 1, chbcr_start);

        // Unmapped CHBCR writes should be ignored but treated as handled.
        let value = 0x0123_4567_89ab_cdefu64;
        rc.mmio_write(chbcr_start + 0x800, value.as_bytes())
            .unwrap();
    }

    #[async_test]
    async fn test_reset() {
        const COMMAND_REG: u64 = 0x4;
        const COMMAND_REG_VALUE: u16 = 0x0004;
        const PORT0_ECAM: u64 = 0;
        const PORT1_ECAM: u64 = 4096;

        let mut rc = instantiate_root_complex(0, 255, 2);
        let mut value_16: u16 = 0;

        // Write the command register of both ports with a reasonable value.
        rc.mmio_write(PORT0_ECAM + COMMAND_REG, COMMAND_REG_VALUE.as_bytes())
            .unwrap();
        rc.mmio_write(PORT1_ECAM + COMMAND_REG, COMMAND_REG_VALUE.as_bytes())
            .unwrap();
        rc.mmio_read(PORT0_ECAM + COMMAND_REG, value_16.as_mut_bytes())
            .unwrap();
        assert_eq!(value_16, COMMAND_REG_VALUE);
        rc.mmio_read(PORT1_ECAM + COMMAND_REG, value_16.as_mut_bytes())
            .unwrap();
        assert_eq!(value_16, COMMAND_REG_VALUE);

        // Reset the emulator, and ensure programming was cleared.
        rc.reset().await;
        rc.mmio_read(PORT0_ECAM + COMMAND_REG, value_16.as_mut_bytes())
            .unwrap();
        assert_eq!(value_16, 0);
        rc.mmio_read(PORT1_ECAM + COMMAND_REG, value_16.as_mut_bytes())
            .unwrap();
        assert_eq!(value_16, 0);

        // Re-write the command register of both ports after reset.
        rc.mmio_write(PORT0_ECAM + COMMAND_REG, COMMAND_REG_VALUE.as_bytes())
            .unwrap();
        rc.mmio_write(PORT1_ECAM + COMMAND_REG, COMMAND_REG_VALUE.as_bytes())
            .unwrap();
        rc.mmio_read(PORT0_ECAM + COMMAND_REG, value_16.as_mut_bytes())
            .unwrap();
        assert_eq!(value_16, COMMAND_REG_VALUE);
        rc.mmio_read(PORT1_ECAM + COMMAND_REG, value_16.as_mut_bytes())
            .unwrap();
        assert_eq!(value_16, COMMAND_REG_VALUE);
    }

    #[test]
    fn test_root_port_hotplug_options() {
        // Test with hotplug disabled (None)
        let root_port_no_hotplug = {
            let mut register_mmio = TestPcieMmioRegistration {};
            let c = pci_core::msi::MsiConnection::new();
            RootPort::new(
                &mut register_mmio,
                "test-port-no-hotplug",
                false,
                None,
                &c.target(),
                PciePortSettings::default(),
            )
        };
        // We can't easily verify hotplug is disabled without accessing internal state,
        // but we can verify the port was created successfully
        let mut vendor_device_id: u32 = 0;
        root_port_no_hotplug
            .port
            .cfg_space
            .read_u32(0x0, &mut vendor_device_id)
            .unwrap();
        let expected = (ROOT_PORT_DEVICE_ID as u32) << 16 | (VENDOR_ID as u32);
        assert_eq!(vendor_device_id, expected);

        // Test with hotplug enabled (Some(slot_number))
        let root_port_with_hotplug = {
            let mut register_mmio = TestPcieMmioRegistration {};
            let c = pci_core::msi::MsiConnection::new();
            RootPort::new(
                &mut register_mmio,
                "test-port-hotplug",
                false,
                Some(5),
                &c.target(),
                PciePortSettings::default(),
            )
        };
        let mut vendor_device_id_hotplug: u32 = 0;
        root_port_with_hotplug
            .port
            .cfg_space
            .read_u32(0x0, &mut vendor_device_id_hotplug)
            .unwrap();
        assert_eq!(vendor_device_id_hotplug, expected);
        // The slot number and hotplug capability would be tested via PCIe capability registers
        // but that requires more complex setup
    }

    #[test]
    fn test_root_port_invalid_bus_range_handling() {
        let mut root_port = {
            let mut register_mmio = TestPcieMmioRegistration {};
            let c = pci_core::msi::MsiConnection::new();
            RootPort::new(
                &mut register_mmio,
                "test-port",
                false,
                None,
                &c.target(),
                PciePortSettings::default(),
            )
        };

        // Don't configure bus numbers, so the range should be 0..=0 (invalid)
        let bus_range = root_port.port.cfg_space.assigned_bus_range();
        assert_eq!(bus_range, 0..=0);

        // Test that forwarding returns Ok but doesn't crash when bus range is invalid
        let addr = PciConfigAddress::new(1, 0, 0x0).unwrap();
        let mut value = 0u32;
        let result = root_port
            .port
            .forward_cfg_read_with_routing(addr, &mut value);
        assert!(matches!(result, IoResult::Ok));

        let result = root_port.port.forward_cfg_write_with_routing(addr, value);
        assert!(matches!(result, IoResult::Ok));
    }

    #[test]
    fn test_save_restore_basic() {
        use vmcore::save_restore::SaveRestore;

        let mut rc = instantiate_root_complex(0, 255, 4);

        // Save the initial state
        let saved_state = rc.save().expect("save should succeed");

        // Verify the saved state has the correct values
        assert_eq!(saved_state.start_bus, 0);
        assert_eq!(saved_state.end_bus, 255);
        assert_eq!(saved_state.ports.len(), 4);

        // Restore the state to a new root complex
        let mut rc2 = instantiate_root_complex(0, 255, 4);
        rc2.restore(saved_state).expect("restore should succeed");
    }

    #[test]
    fn test_save_restore_with_bus_configuration() {
        use vmcore::save_restore::SaveRestore;
        use zerocopy::IntoBytes;

        const SECONDARY_BUS_NUM_REG: u64 = 0x19;
        const SUBORDINATE_BUS_NUM_REG: u64 = 0x1A;

        let mut rc = instantiate_root_complex(0, 255, 2);

        // Configure bus numbers on port 0
        rc.mmio_write(SECONDARY_BUS_NUM_REG, &[1]).unwrap();
        rc.mmio_write(SUBORDINATE_BUS_NUM_REG, &[10]).unwrap();

        // Configure bus numbers on port 1 (at devfn 1 with multi-function packing)
        const PORT1_ECAM: u64 = 4096;
        rc.mmio_write(PORT1_ECAM + SECONDARY_BUS_NUM_REG, &[11])
            .unwrap();
        rc.mmio_write(PORT1_ECAM + SUBORDINATE_BUS_NUM_REG, &[20])
            .unwrap();

        // Verify the bus numbers are set
        let mut bus_number: u8 = 0;
        rc.mmio_read(SECONDARY_BUS_NUM_REG, bus_number.as_mut_bytes())
            .unwrap();
        assert_eq!(bus_number, 1);
        rc.mmio_read(SUBORDINATE_BUS_NUM_REG, bus_number.as_mut_bytes())
            .unwrap();
        assert_eq!(bus_number, 10);

        rc.mmio_read(
            PORT1_ECAM + SECONDARY_BUS_NUM_REG,
            bus_number.as_mut_bytes(),
        )
        .unwrap();
        assert_eq!(bus_number, 11);
        rc.mmio_read(
            PORT1_ECAM + SUBORDINATE_BUS_NUM_REG,
            bus_number.as_mut_bytes(),
        )
        .unwrap();
        assert_eq!(bus_number, 20);

        // Save the state
        let saved_state = rc.save().expect("save should succeed");

        // Create a new root complex and restore the state
        let mut rc2 = instantiate_root_complex(0, 255, 2);

        // Verify the new root complex has default bus numbers before restore
        let mut default_bus: u8 = 0xFF;
        rc2.mmio_read(SECONDARY_BUS_NUM_REG, default_bus.as_mut_bytes())
            .unwrap();
        assert_eq!(default_bus, 0);

        // Restore the state
        rc2.restore(saved_state).expect("restore should succeed");

        // Verify the bus numbers are restored
        let mut restored_bus: u8 = 0;
        rc2.mmio_read(SECONDARY_BUS_NUM_REG, restored_bus.as_mut_bytes())
            .unwrap();
        assert_eq!(restored_bus, 1);
        rc2.mmio_read(SUBORDINATE_BUS_NUM_REG, restored_bus.as_mut_bytes())
            .unwrap();
        assert_eq!(restored_bus, 10);

        rc2.mmio_read(
            PORT1_ECAM + SECONDARY_BUS_NUM_REG,
            restored_bus.as_mut_bytes(),
        )
        .unwrap();
        assert_eq!(restored_bus, 11);
        rc2.mmio_read(
            PORT1_ECAM + SUBORDINATE_BUS_NUM_REG,
            restored_bus.as_mut_bytes(),
        )
        .unwrap();
        assert_eq!(restored_bus, 20);
    }

    #[test]
    fn test_save_restore_bus_number_mismatch_error() {
        use vmcore::save_restore::SaveRestore;

        // Create a root complex with specific bus range
        let mut rc = instantiate_root_complex(0, 255, 2);

        // Save the state
        let saved_state = rc.save().expect("save should succeed");

        // Create a new root complex with different bus range
        let mut rc2 = instantiate_root_complex(0, 127, 2);

        // Restore should fail because bus numbers don't match
        let result = rc2.restore(saved_state);
        assert!(result.is_err());
    }

    #[test]
    fn test_save_restore_port_count_mismatch_error() {
        use vmcore::save_restore::SaveRestore;

        // Create a root complex with 4 ports
        let mut rc = instantiate_root_complex(0, 255, 4);

        // Save the state
        let saved_state = rc.save().expect("save should succeed");
        assert_eq!(saved_state.ports.len(), 4);

        // Create a new root complex with only 2 ports
        let mut rc2 = instantiate_root_complex(0, 255, 2);

        // Restore should fail because port counts don't match
        let result = rc2.restore(saved_state);
        assert!(result.is_err());
    }

    #[test]
    fn test_save_restore_with_cxl_component_registers() {
        use vmcore::save_restore::SaveRestore;

        let chbcr_start = 0x3004_0000;
        let mut rc = instantiate_root_complex_with_chbcr(0, 0, 1, chbcr_start);

        assert!(
            rc.cxl_component_registers
                .as_mut()
                .expect("CXL mode must allocate component registers")
                .add_register(Box::new(TestCxlComponentRegisterBlock::new(
                    CxlComponentRegisterType::CXL_CACHE_MEM_REGISTER,
                    16,
                )))
        );

        let programmed = 0x3344_5566u32;
        rc.mmio_write(chbcr_start + 0x1008, programmed.as_bytes())
            .unwrap();

        let saved_state = rc.save().expect("save should succeed");

        let mut rc2 = instantiate_root_complex_with_chbcr(0, 0, 1, chbcr_start);
        assert!(
            rc2.cxl_component_registers
                .as_mut()
                .expect("CXL mode must allocate component registers")
                .add_register(Box::new(TestCxlComponentRegisterBlock::new(
                    CxlComponentRegisterType::CXL_CACHE_MEM_REGISTER,
                    16,
                )))
        );

        rc2.restore(saved_state).expect("restore should succeed");

        let mut read_back: u32 = 0;
        rc2.mmio_read(chbcr_start + 0x1008, read_back.as_mut_bytes())
            .unwrap();
        assert_eq!(read_back, programmed);
    }

    #[test]
    fn test_bus_range_updated_on_cfg_write() {
        const SECONDARY_BUS_NUM_REG: u64 = 0x19;
        const SUBORDINATE_BUS_NUM_REG: u64 = 0x1A;

        let mut rc = instantiate_root_complex(0, 255, 1);

        let endpoint = TestPcieEndpoint::new(
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
            |_, _| Some(IoResult::Err(IoError::InvalidRegister)),
        );

        // Get the bus_range from the port before attaching a device.
        let bus_range = rc.downstream_ports().into_iter().next().unwrap().bus_range;
        assert_eq!(bus_range.bus_range(), (0, 0));

        rc.add_pcie_device(0, "ep", Box::new(endpoint)).unwrap();

        // Program secondary=5, subordinate=10 via ECAM MMIO writes.
        rc.mmio_write(SECONDARY_BUS_NUM_REG, &[5]).unwrap();
        rc.mmio_write(SUBORDINATE_BUS_NUM_REG, &[10]).unwrap();

        // The shared AssignedBusRange should reflect the new values.
        assert_eq!(bus_range.bus_range(), (5, 10));

        // Reprogram bus numbers and verify tracking follows.
        rc.mmio_write(SECONDARY_BUS_NUM_REG, &[20]).unwrap();
        rc.mmio_write(SUBORDINATE_BUS_NUM_REG, &[30]).unwrap();
        assert_eq!(bus_range.bus_range(), (20, 30));
    }

    #[test]
    fn test_rciep_ecam_read_write() {
        // Create a root complex with root ports starting at device 1,
        // leaving device 0 free for an RCiEP.
        let mut register_mmio = TestPcieMmioRegistration {};
        let start_bus: u8 = 0;
        let end_bus: u8 = 0;
        let ecam = MemoryRange::new(0..ecam_size_from_bus_numbers(start_bus, end_bus));
        let rc_bus_range = AssignedBusRange::new();
        rc_bus_range.set_bus_range(start_bus, end_bus);
        let msi_conn = pci_core::msi::MsiConnection::new();
        let port_defs = vec![GenericPciePortDefinition {
            name: "port-0".into(),
            devfn: None,
            hotplug: false,
            settings: PciePortSettings::default(),
        }];
        let mut rc = GenericPcieRootComplex::builder(&mut register_mmio, start_bus..=end_bus, ecam)
            .root_ports(port_defs, &msi_conn.msi_target(rc_bus_range, 0))
            .first_port_device_number(1)
            .build()
            .unwrap();

        // Attach an RCiEP at device 0 function 0 (devfn 0).
        let rciep = TestPcieEndpoint::new(
            |offset, value| {
                if offset == 0 {
                    *value = 0xDEAD_BEEF;
                }
                Some(IoResult::Ok)
            },
            |_, _| Some(IoResult::Ok),
        );
        rc.add_rciep(0, "rciep-0", Box::new(rciep)).unwrap();

        // ECAM read at device 0, function 0 should hit the RCiEP.
        let mut vendor_device: u32 = 0;
        rc.mmio_read(0, vendor_device.as_mut_bytes()).unwrap();
        assert_eq!(vendor_device, 0xDEAD_BEEF);

        // ECAM write at device 0, function 0 should route to the RCiEP
        // (the test endpoint accepts all writes).
        rc.mmio_write(0, &0x1234_5678u32.to_le_bytes()).unwrap();

        // Root port at device 1 should still be accessible.
        let mut root_port_vendor: u32 = 0;
        // device 1, function 0 → devfn 8 → offset 8 * 4096
        rc.mmio_read(8 * 4096, root_port_vendor.as_mut_bytes())
            .unwrap();
        assert_eq!(root_port_vendor, 0xC030_1414);

        // An unoccupied device slot should return all-1s.
        let mut empty: u32 = 0;
        // device 2, function 0 → devfn 16 → offset 16 * 4096
        rc.mmio_read(16 * 4096, empty.as_mut_bytes()).unwrap();
        assert_eq!(empty, 0xFFFF_FFFF);
    }

    #[test]
    fn test_rciep_function0_fallback() {
        // Test that a config read to function 1 of an RCiEP device falls
        // back to the function-0 device via pci_cfg_read_with_routing,
        // which by default returns all-1s for non-zero functions.
        let mut register_mmio = TestPcieMmioRegistration {};
        let start_bus: u8 = 0;
        let end_bus: u8 = 0;
        let ecam = MemoryRange::new(0..ecam_size_from_bus_numbers(start_bus, end_bus));
        let mut rc = GenericPcieRootComplex::builder(&mut register_mmio, start_bus..=end_bus, ecam)
            .build()
            .unwrap();

        let rciep = TestPcieEndpoint::new(
            |offset, value| {
                if offset == 0 {
                    *value = 0xCAFE_F00D;
                }
                Some(IoResult::Ok)
            },
            |_, _| Some(IoResult::Ok),
        );
        rc.add_rciep(0, "rciep-0", Box::new(rciep)).unwrap();

        // Function 0 should return the device's vendor/device ID.
        let mut val: u32 = 0;
        rc.mmio_read(0, val.as_mut_bytes()).unwrap();
        assert_eq!(val, 0xCAFE_F00D);

        // Function 1 (devfn 1) falls back to the function-0 device's
        // pci_cfg_read_with_routing, which returns all-1s by default.
        let mut val_fn1: u32 = 0;
        // devfn 1 → offset 1 * 4096
        rc.mmio_read(4096, val_fn1.as_mut_bytes()).unwrap();
        assert_eq!(val_fn1, 0xFFFF_FFFF);
    }

    #[test]
    fn test_rciep_collision_with_root_port() {
        // Verify that adding an RCiEP at a devfn already occupied by a
        // root port returns an error with the port's name.
        let mut rc = instantiate_root_complex(0, 0, 1);

        let rciep = TestPcieEndpoint::new(|_, _| Some(IoResult::Ok), |_, _| Some(IoResult::Ok));
        // Root port 0 sits at devfn 0; adding an RCiEP there should fail.
        let err = rc
            .add_rciep(0, "rciep-collision", Box::new(rciep))
            .expect_err("should fail: devfn occupied by root port");
        assert_eq!(err.as_ref(), "test-port-0");
    }

    #[test]
    fn test_rciep_collision_with_rciep() {
        // Verify that adding two RCiEPs at the same devfn returns an error.
        let mut register_mmio = TestPcieMmioRegistration {};
        let ecam = MemoryRange::new(0..ecam_size_from_bus_numbers(0, 0));
        let mut rc = GenericPcieRootComplex::builder(&mut register_mmio, 0..=0u8, ecam)
            .build()
            .unwrap();

        let rciep1 = TestPcieEndpoint::new(|_, _| Some(IoResult::Ok), |_, _| Some(IoResult::Ok));
        let rciep2 = TestPcieEndpoint::new(|_, _| Some(IoResult::Ok), |_, _| Some(IoResult::Ok));
        rc.add_rciep(0, "rciep-first", Box::new(rciep1)).unwrap();
        let err = rc
            .add_rciep(0, "rciep-second", Box::new(rciep2))
            .expect_err("should fail: devfn already has an RCiEP");
        assert_eq!(err.as_ref(), "rciep-first");
    }

    #[test]
    fn test_rciep_collision_with_reserved_device_number() {
        // Reserve device 0 and verify RCiEP insertion at devfn 0 is rejected.
        let mut register_mmio = TestPcieMmioRegistration {};
        let ecam = MemoryRange::new(0..ecam_size_from_bus_numbers(0, 0));
        let mut rc = GenericPcieRootComplex::builder(&mut register_mmio, 0..=0u8, ecam)
            .reserved_device_numbers(1 << 0)
            .build()
            .unwrap();

        let rciep = TestPcieEndpoint::new(|_, _| Some(IoResult::Ok), |_, _| Some(IoResult::Ok));
        let err = rc
            .add_rciep(0, "rciep-reserved", Box::new(rciep))
            .expect_err("should fail: device 0 is reserved");
        assert_eq!(err.as_ref(), "reserved device number 0x0");
    }

    #[test]
    fn test_multi_function_header_bit() {
        // With >1 port, bit 23 of register 0x0C (header type bit 7) must be set
        // to indicate a multi-function device.
        let mut rc = instantiate_root_complex(0, 255, 2);
        let mut header_type_reg: u32 = 0;
        // Register 0x0C for port 0 (devfn 0).
        rc.mmio_read(0x0C, header_type_reg.as_mut_bytes()).unwrap();
        // Bit 23 = multi-function flag in the header type byte (offset 0x0E).
        assert_ne!(
            header_type_reg & (1 << 23),
            0,
            "multi-function bit must be set"
        );

        // With exactly 1 port, the multi-function bit should NOT be set.
        let mut rc_single = instantiate_root_complex(0, 255, 1);
        let mut header_single: u32 = 0;
        rc_single
            .mmio_read(0x0C, header_single.as_mut_bytes())
            .unwrap();
        assert_eq!(
            header_single & (1 << 23),
            0,
            "single-function: multi-function bit must be clear"
        );
    }

    #[test]
    fn test_multi_function_bit_is_per_device() {
        // Two ports placed at explicit devfns on *distinct* devices (each the
        // sole function of its device) must NOT advertise the multi-function
        // bit, even though there is more than one port.
        let mut register_mmio = TestPcieMmioRegistration {};
        let ecam = MemoryRange::new(0..ecam_size_from_bus_numbers(0, 0));
        let msi_conn = pci_core::msi::MsiConnection::new();
        let port_defs = vec![
            GenericPciePortDefinition {
                name: "a".into(),
                devfn: Some(8), // device 1, function 0
                hotplug: false,
                settings: PciePortSettings::default(),
            },
            GenericPciePortDefinition {
                name: "b".into(),
                devfn: Some(16), // device 2, function 0
                hotplug: false,
                settings: PciePortSettings::default(),
            },
        ];
        let mut rc = GenericPcieRootComplex::builder(&mut register_mmio, 0..=0u8, ecam)
            .root_ports(port_defs, &msi_conn.target())
            .build()
            .unwrap();

        for devfn in [8u64, 16] {
            let mut header: u32 = 0;
            rc.mmio_read(devfn * 4096 + 0x0C, header.as_mut_bytes())
                .unwrap();
            assert_eq!(
                header & (1 << 23),
                0,
                "devfn {devfn}: sole function of its device must not set the multi-function bit"
            );
        }
    }

    #[test]
    fn test_too_many_ports_returns_error() {
        // 257 ports starting at device 0 requires device 32, which is out of range.
        let port_defs: Vec<GenericPciePortDefinition> = (0..257)
            .map(|i| GenericPciePortDefinition {
                name: format!("port-{}", i).into(),
                devfn: None,
                hotplug: false,
                settings: PciePortSettings::default(),
            })
            .collect();
        let mut register_mmio = TestPcieMmioRegistration {};
        let ecam = MemoryRange::new(0..ecam_size_from_bus_numbers(0, 255));
        let rc_bus_range = AssignedBusRange::new();
        rc_bus_range.set_bus_range(0, 255);
        let msi_conn = pci_core::msi::MsiConnection::new();
        let result = GenericPcieRootComplex::builder(&mut register_mmio, 0..=255u8, ecam)
            .root_ports(port_defs, &msi_conn.msi_target(rc_bus_range, 0))
            .build();
        assert!(
            result.is_err(),
            "257 ports should exceed the 256-port limit"
        );
    }

    /// Builds a root complex from explicit per-port devfn requests, returning
    /// the assigned devfns in port-name order.
    fn build_with_devfns(
        ports: Vec<(&str, Option<u8>)>,
        first_port_device_number: u8,
    ) -> Result<Vec<(String, u8)>, InvalidRootComplexError> {
        let port_defs: Vec<GenericPciePortDefinition> = ports
            .into_iter()
            .map(|(name, devfn)| GenericPciePortDefinition {
                name: name.into(),
                devfn,
                hotplug: false,
                settings: PciePortSettings::default(),
            })
            .collect();
        let mut register_mmio = TestPcieMmioRegistration {};
        let ecam = MemoryRange::new(0..ecam_size_from_bus_numbers(0, 0));
        let msi_conn = pci_core::msi::MsiConnection::new();
        let rc = GenericPcieRootComplex::builder(&mut register_mmio, 0..=0u8, ecam)
            .root_ports(port_defs, &msi_conn.target())
            .first_port_device_number(first_port_device_number)
            .build()?;
        Ok(rc
            .downstream_ports()
            .into_iter()
            .map(|p| (p.name.to_string(), p.devfn))
            .collect())
    }

    #[test]
    fn test_devfn_auto_allocation() {
        // All-None ports pack onto consecutive devfns starting at 0.
        let ports = build_with_devfns(vec![("a", None), ("b", None), ("c", None)], 0).unwrap();
        assert_eq!(
            ports,
            vec![("a".into(), 0), ("b".into(), 1), ("c".into(), 2)]
        );
    }

    #[test]
    fn test_devfn_auto_allocation_respects_first_device() {
        // None ports start at the first-port device number (device 1 → devfn 8).
        let ports = build_with_devfns(vec![("a", None), ("b", None)], 1).unwrap();
        assert_eq!(ports, vec![("a".into(), 8), ("b".into(), 9)]);
    }

    #[test]
    fn test_devfn_explicit_and_auto_mix() {
        // Explicit devfn is honored; subsequent None ports skip used devfns.
        let ports =
            build_with_devfns(vec![("a", Some(0)), ("b", None), ("c", Some(16))], 0).unwrap();
        // Sorted by devfn: a@0, b@1, c@16.
        assert_eq!(
            ports,
            vec![("a".into(), 0), ("b".into(), 1), ("c".into(), 16)]
        );
    }

    #[test]
    fn test_devfn_none_then_explicit_zero_conflicts() {
        // [None, Some(0)]: the None port takes devfn 0, so the explicit
        // request for 0 collides and fails (allocation is in order).
        let err = build_with_devfns(vec![("a", None), ("b", Some(0))], 0).unwrap_err();
        assert!(matches!(
            err,
            InvalidRootComplexError::Devfn(crate::PortDevfnError::DevfnInUse { devfn: 0, .. })
        ));
    }

    #[test]
    fn test_devfn_explicit_duplicate_conflicts() {
        let err = build_with_devfns(vec![("a", Some(5)), ("b", Some(5))], 0).unwrap_err();
        assert!(matches!(
            err,
            InvalidRootComplexError::Devfn(crate::PortDevfnError::DevfnInUse { devfn: 5, .. })
        ));
    }

    #[test]
    fn test_devfn_nonzero_function_without_function_zero_fails() {
        // device 5, function 1 (devfn 0x29) with no function 0 is undiscoverable.
        let err = build_with_devfns(vec![("a", Some((5 << 3) | 1))], 0).unwrap_err();
        assert!(matches!(
            err,
            InvalidRootComplexError::Devfn(crate::PortDevfnError::MissingFunctionZero {
                device: 5
            })
        ));
    }

    #[test]
    fn test_devfn_function_zero_present_allows_higher_functions() {
        // device 5 functions 0 and 1 present: discoverable, so this is allowed.
        let ports =
            build_with_devfns(vec![("a", Some(5 << 3)), ("b", Some((5 << 3) | 1))], 0).unwrap();
        assert_eq!(ports, vec![("a".into(), 40), ("b".into(), 41)]);
    }
}
