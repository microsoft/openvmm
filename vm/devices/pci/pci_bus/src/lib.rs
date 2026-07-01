// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Generic PCI Bus infrastructure.
//!
//! [`GenericPciBus`] is a [`ChipsetDevice`] that implements a chipset and
//! architecture agnostic PCI bus.
//!
//! [`GenericPciBus`] can be configured to support various spec-compliant PCI
//! configuration space access mechanisms, such as legacy port-io based
//! configuration space access, ECAM (Enhanced Configuration Access Mechanism),
//! etc...
//!
//! Incoming config space accesses are then routed to connected
//! [`GenericPciBusDevice`] devices.

#![forbid(unsafe_code)]

use bitfield_struct::bitfield;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigAddress;
use chipset_device::pci::PciConfigByteEnable;
use chipset_device::pio::ControlPortIoIntercept;
use chipset_device::pio::PortIoIntercept;
use chipset_device::pio::RegisterPortIoIntercept;
use chipset_device::poll_device::PollDevice;
use inspect::Inspect;
use inspect::InspectMut;
use pci_core::bus_cfg::PciBusCfgAccessCallbacks;
use pci_core::bus_cfg::PciBusCfgAccessHandler;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::task::Context;
use thiserror::Error;
use vmcore::device_state::ChangeDeviceState;
use zerocopy::FromZeros;

/// Standard x86 IO ports associated with PCI
#[expect(missing_docs)] // self explanatory constants
pub mod standard_x86_io_ports {
    pub const ADDR_START: u16 = 0xCF8;
    pub const ADDR_END: u16 = 0xCFB;

    pub const DATA_START: u16 = 0xCFC;
    pub const DATA_END: u16 = 0xCFF;
}

/// An abstract interface for a PCI device accessed via the [`GenericPciBus`].
///
/// This trait is nearly identical to [`chipset_device::pci::PciConfigSpace`],
/// except for the fact that the return values are wrapped in an `Option`, where
/// `None` indicates that the backing device is no longer responding to
/// accesses.
///
/// e.g: a GenericPciBusDevice backed by a `Weak` pointer to a device could get
/// invalidated, in which case, these APIs would return `None`.
///
/// This trait decouples the PCI bus implementation from any concrete
/// `ChipsetDevice` ownership model being employed by upper-level code (i.e:
/// Arc/Weak + Mutex vs. Channels, etc...).
///
/// This is also the reason why the read/write methods are fallible: the PCI bus
/// should be resilient to backing devices unexpectedly going offline.
///
/// PCI devices can optionally implement routing functionality (like switches and bridges)
/// by providing implementations for the forwarding methods.
pub trait GenericPciBusDevice: 'static + Send {
    /// Dispatch a PCI config space read to the device with the given address.
    fn pci_cfg_read(&mut self, offset: u16, value: ByteEnabledDwordRead<'_>) -> Option<IoResult>;

    /// Dispatch a PCI config space write to the device with the given address.
    fn pci_cfg_write(&mut self, offset: u16, value: ByteEnabledDwordWrite) -> Option<IoResult>;

    /// Handle a PCI configuration space read with full routing context.
    ///
    /// This method receives configuration space accesses with the target bus
    /// and function number. The interpretation of `function` depends on the
    /// bus topology: on a legacy PCI bus it carries packed device/function
    /// bits (0..=255), while downstream of a PCIe port the device number is
    /// always zero so all 8 bits represent functions within a single
    /// endpoint.
    ///
    /// A device can distinguish Type 0 (local) from Type 1 (forwarded)
    /// configuration cycles by comparing `target_bus` and `secondary_bus`:
    /// when they are equal the access targets this device directly (Type 0),
    /// otherwise it should be routed downstream (Type 1). An SR-IOV
    /// capable device can use `secondary_bus` together with `target_bus` and
    /// `function` to compute the VF number.
    ///
    /// The default implementation dispatches function 0 to
    /// [`pci_cfg_read`](Self::pci_cfg_read) and returns all-1s for other
    /// functions (the standard "no device present" response). Routing
    /// components (switches, bridges) and multi-function devices should
    /// override this method.
    ///
    /// Returns `None` if the backing device is no longer responding.
    fn pci_cfg_read_with_routing(
        &mut self,
        secondary_bus: u8,
        target_bus: u8,
        function: u8,
        offset: u16,
        mut value: ByteEnabledDwordRead<'_>,
    ) -> Option<IoResult> {
        if secondary_bus == target_bus && function == 0 {
            self.pci_cfg_read(offset, value)
        } else {
            value.set(!0);
            Some(IoResult::Ok)
        }
    }

    /// Handle a PCI configuration space write with full routing context.
    ///
    /// This method receives configuration space accesses with the target bus
    /// and function number. The interpretation of `function` depends on the
    /// bus topology: on a legacy PCI bus it carries packed device/function
    /// bits (0..=255), while downstream of a PCIe port the device number is
    /// always zero so all 8 bits represent functions within a single
    /// endpoint.
    ///
    /// A device can distinguish Type 0 (local) from Type 1 (forwarded)
    /// configuration cycles by comparing `target_bus` and `secondary_bus`:
    /// when they are equal the access targets this device directly (Type 0),
    /// otherwise it should be routed downstream (Type 1). An SR-IOV
    /// capable device can use `secondary_bus` together with `target_bus` and
    /// `function` to compute the VF number.
    ///
    /// The default implementation dispatches function 0 to
    /// [`pci_cfg_write`](Self::pci_cfg_write) and silently drops writes to
    /// other functions. Routing components (switches, bridges) and
    /// multi-function devices should override this method.
    ///
    /// Returns `None` if the backing device is no longer responding.
    fn pci_cfg_write_with_routing(
        &mut self,
        secondary_bus: u8,
        target_bus: u8,
        function: u8,
        offset: u16,
        value: ByteEnabledDwordWrite,
    ) -> Option<IoResult> {
        if secondary_bus == target_bus && function == 0 {
            self.pci_cfg_write(offset, value)
        } else {
            Some(IoResult::Ok)
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Inspect)]
#[inspect(display)]
struct PciAddr {
    bus: u8,
    device: u8,
    function: u8,
}

impl std::fmt::Display for PciAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Use standard-ish BDF notation (bb:dd.f).
        write!(
            f,
            "{:02x}:{:02x}.{:x}",
            self.bus, self.device, self.function
        )
    }
}

impl From<PciConfigAddress> for PciAddr {
    fn from(address: PciConfigAddress) -> Self {
        Self {
            bus: address.bus,
            device: address.device(),
            function: address.function(),
        }
    }
}

#[derive(Inspect)]
struct GenericPciBusState {
    pio_addr_reg: AddressRegister,
}

/// A generic PCI bus.
#[derive(InspectMut)]
pub struct GenericPciBus {
    // Runtime glue
    pio_addr: Box<dyn ControlPortIoIntercept>,
    pio_data: Box<dyn ControlPortIoIntercept>,
    #[inspect(with = "|x| inspect::iter_by_key(x).map_value(|(name, _)| name)")]
    pci_devices: BTreeMap<PciAddr, (Arc<str>, Box<dyn GenericPciBusDevice>)>,

    // Config space bookkeeping
    bus_cfg_handler: PciBusCfgAccessHandler,

    // Volatile state
    state: GenericPciBusState,
}

/// Error indicating that a PCI slot is already occupied.
#[derive(Debug, Error)]
#[error("PCI slot already occupied by device '{existing_device_name}'")]
pub struct PciSlotOccupiedError<D> {
    /// Name of the existing device occupying the slot.
    pub existing_device_name: Arc<str>,
    /// The device that was attempted to be added.
    pub device: D,
}

impl GenericPciBus {
    /// Create a new [`GenericPciBus`] with the specified (4-byte) IO ports.
    pub fn new(
        register_pio: &mut dyn RegisterPortIoIntercept,
        pio_addr: u16,
        pio_data: u16,
    ) -> GenericPciBus {
        let mut addr_control = register_pio.new_io_region("addr", 4);
        let mut data_control = register_pio.new_io_region("data", 4);
        addr_control.map(pio_addr);
        data_control.map(pio_data);
        GenericPciBus {
            pio_addr: addr_control,
            pio_data: data_control,
            pci_devices: BTreeMap::new(),
            bus_cfg_handler: PciBusCfgAccessHandler::new(),
            state: GenericPciBusState {
                pio_addr_reg: AddressRegister::new(),
            },
        }
    }

    /// Try to add a PCI device.
    pub fn add_pci_device<D: GenericPciBusDevice>(
        &mut self,
        bus: u8,
        device: u8,
        function: u8,
        name: impl AsRef<str>,
        dev: D,
    ) -> Result<(), PciSlotOccupiedError<D>> {
        let key = PciAddr {
            bus,
            device,
            function,
        };

        if let Some((name, _)) = self.pci_devices.get(&key) {
            return Err(PciSlotOccupiedError {
                existing_device_name: name.clone(),
                device: dev,
            });
        }

        self.pci_devices
            .insert(key, (name.as_ref().into(), Box::new(dev)));
        Ok(())
    }

    /// Handle a read from the ADDR register
    fn handle_addr_read(&self, mut value: ByteEnabledDwordRead<'_>) -> IoResult {
        value.set(self.state.pio_addr_reg.0);
        IoResult::Ok
    }

    /// Handle a write to the ADDR register
    fn handle_addr_write(&mut self, value: ByteEnabledDwordWrite) -> IoResult {
        let addr = value.merge(self.state.pio_addr_reg.0);
        let addr_fixup = {
            let mut addr = AddressRegister(addr);
            addr.fixup();
            addr
        };

        self.state.pio_addr_reg = addr_fixup;
        IoResult::Ok
    }

    /// Handle a read from the DATA register
    fn handle_data_read(&mut self, mut value: ByteEnabledDwordRead<'_>) -> IoResult {
        tracing::trace!(%self.state.pio_addr_reg, "data read");

        if !self.state.pio_addr_reg.enabled() {
            tracelimit::warn_ratelimited!("addr enable bit is set to disabled");
            value.set(!0);
            return IoResult::Ok;
        }

        let Some(address) = self.state.pio_addr_reg.config_address() else {
            tracelimit::warn_ratelimited!("addr register has invalid offset");
            value.set(!0);
            return IoResult::Ok;
        };

        let mut callback = PciBusCfgAccessCallbackView::new(&mut self.pci_devices);
        self.bus_cfg_handler.read(address, value, &mut callback)
    }

    /// Handler a write to the DATA register
    fn handle_data_write(&mut self, value: ByteEnabledDwordWrite) -> IoResult {
        tracing::trace!(%self.state.pio_addr_reg, "data write");

        if !self.state.pio_addr_reg.enabled() {
            tracelimit::warn_ratelimited!("addr enable bit is set to disabled");
            return IoResult::Ok;
        }

        let Some(address) = self.state.pio_addr_reg.config_address() else {
            tracelimit::warn_ratelimited!("addr register has invalid offset");
            return IoResult::Ok;
        };

        let mut callback = PciBusCfgAccessCallbackView::new(&mut self.pci_devices);
        self.bus_cfg_handler.write(address, value, &mut callback)
    }

    fn trace_error(&self, e: IoError, operation: &'static str) {
        let error = match e {
            IoError::InvalidRegister => "offset not supported",
            IoError::InvalidAccessSize => "invalid access size",
            IoError::UnalignedAccess => "unaligned access",
            IoError::NoResponse => "no response",
        };
        tracelimit::warn_ratelimited!(
            address = %self.state.pio_addr_reg.address(),
            offset = self.state.pio_addr_reg.register(),
            "pci config space {} operation error: {}",
            operation,
            error
        );
    }
}

impl ChangeDeviceState for GenericPciBus {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.state.pio_addr_reg = AddressRegister::new();
    }
}

impl ChipsetDevice for GenericPciBus {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PortIoIntercept for GenericPciBus {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        let byte_enable = match PciConfigByteEnable::from_offset_len(io_port, data.len()) {
            Ok(be) => be,
            Err(e) => return IoResult::Err(e),
        };

        let mut buffer = 0;
        let mut value = ByteEnabledDwordRead::new(&mut buffer, byte_enable);

        let res = match io_port {
            _ if self.pio_addr.offset_of(io_port).is_some() => {
                self.handle_addr_read(value.reborrow())
            }
            _ if self.pio_data.offset_of(io_port).is_some() => {
                self.handle_data_read(value.reborrow())
            }
            _ => {
                return IoResult::Err(IoError::InvalidRegister);
            }
        };

        tracing::trace!(?io_port, ?res, ?data, "io port read");

        match res {
            IoResult::Ok => {
                value.fill_intercept_buffer(data);
                IoResult::Ok
            }
            IoResult::Err(e) => {
                self.trace_error(e, "read");
                // Regardless of the pci error that occurred we return all zeros.
                // This is technically device-specific behavior, but it's what all
                // hyper-v devices do and it's worked for us so far.
                data.zero();
                IoResult::Ok
            }
            IoResult::Defer(deferral) => IoResult::Defer(deferral),
        }
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        let byte_enable = match PciConfigByteEnable::from_offset_len(io_port, data.len()) {
            Ok(be) => be,
            Err(e) => return IoResult::Err(e),
        };

        let value = ByteEnabledDwordWrite::from_intercept_buffer(byte_enable, data);

        tracing::trace!(?io_port, data = value.extract(), "io port write");

        let res = match io_port {
            _ if self.pio_addr.offset_of(io_port).is_some() => self.handle_addr_write(value),
            _ if self.pio_data.offset_of(io_port).is_some() => self.handle_data_write(value),
            _ => IoResult::Err(IoError::InvalidRegister),
        };

        match res {
            IoResult::Ok => IoResult::Ok,
            IoResult::Err(e) => {
                self.trace_error(e, "write");
                IoResult::Ok
            }
            IoResult::Defer(deferral) => IoResult::Defer(deferral),
        }
    }
}

impl PollDevice for GenericPciBus {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        self.bus_cfg_handler.poll(cx);
    }
}

struct PciBusCfgAccessCallbackView<'a> {
    pci_devices: &'a mut BTreeMap<PciAddr, (Arc<str>, Box<dyn GenericPciBusDevice>)>,
}

impl<'a> PciBusCfgAccessCallbackView<'a> {
    fn new(
        pci_devices: &'a mut BTreeMap<PciAddr, (Arc<str>, Box<dyn GenericPciBusDevice>)>,
    ) -> Self {
        Self { pci_devices }
    }
}

impl<'a> PciBusCfgAccessCallbacks for PciBusCfgAccessCallbackView<'a> {
    fn read(&mut self, addr: PciConfigAddress, mut value: ByteEnabledDwordRead<'_>) -> IoResult {
        let address = PciAddr::from(addr);
        match self.pci_devices.get_mut(&address) {
            Some((name, device)) => {
                let offset = addr.byte_offset();
                let res = device.pci_cfg_read(offset, value.reborrow());
                if let Some(result) = res {
                    tracing::trace!(
                        device = &**name,
                        %address,
                        offset,
                        ?value,
                        "cfg space read"
                    );
                    result
                } else {
                    // TODO: should probably unregister from bus?
                    // but then again, shouldn't the device do that as part of
                    // its destructor?
                    tracelimit::warn_ratelimited!(
                        device = &**name,
                        %address,
                        offset,
                        "cfg space read failed, device went away"
                    );
                    value.set(!0);
                    IoResult::Ok
                }
            }
            None => {
                tracing::trace!(%address, "no device found - returning F's");
                value.set(!0);
                IoResult::Ok
            }
        }
    }

    fn write(&mut self, addr: PciConfigAddress, value: ByteEnabledDwordWrite) -> IoResult {
        let address = PciAddr::from(addr);
        match self.pci_devices.get_mut(&address) {
            Some((name, device)) => {
                let offset = addr.byte_offset();
                let res = device.pci_cfg_write(offset, value);
                if let Some(result) = res {
                    tracing::trace!(
                        device = &**name,
                        %address,
                        offset,
                        ?value,
                        "cfg space write"
                    );
                    result
                } else {
                    // TODO: should probably unregister from bus?
                    // but then again, shouldn't the device do that as part of
                    // its destructor?
                    tracelimit::warn_ratelimited!(
                        device = &**name,
                        %address,
                        offset,
                        "cfg space write failed, device went away"
                    );
                    IoResult::Ok
                }
            }
            None => {
                tracing::debug!(%address, "no device found");
                IoResult::Ok
            }
        }
    }
}

#[rustfmt::skip]
#[derive(Inspect)]
#[bitfield(u32)]
struct AddressRegister {
    #[bits(8)] register: u8,
    #[bits(3)] function: u8,
    #[bits(5)] device: u8,
    #[bits(8)] bus: u8,
    #[bits(7)] reserved: u8,
    #[bits(1)] enabled: bool,
}

impl AddressRegister {
    fn address(&self) -> PciAddr {
        PciAddr {
            bus: self.bus(),
            device: self.device(),
            function: self.function(),
        }
    }

    fn config_address(&self) -> Option<PciConfigAddress> {
        PciConfigAddress::new(
            self.bus(),
            (self.device() << 3) | self.function(),
            (self.register() / 4).into(),
        )
    }

    /// Set all reserved / zero bits to zero
    fn fixup(&mut self) {
        // the register accessed is always DWORD aligned
        // (the low two bits are hard-coded to 0)
        self.set_register(self.register() & !0b11);
        self.set_reserved(0);
    }
}

impl core::fmt::Display for AddressRegister {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{:04x}", self.address(), self.register())
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "pci.bus")]
        pub struct SavedState {
            #[mesh(1)]
            pub pio_addr_reg: u32,
        }
    }

    #[derive(Debug, Error)]
    enum GenericPciBusRestoreError {
        #[error("saved address contained non-zero reserved bits")]
        AddressNonZeroReserved,
        #[error("saved address contained non-dword aligned register bits")]
        AddressNotDwordAligned,
    }

    impl SaveRestore for GenericPciBus {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let GenericPciBusState { pio_addr_reg } = self.state;

            let saved_state = state::SavedState {
                pio_addr_reg: pio_addr_reg.into(),
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState { pio_addr_reg } = state;

            self.state = GenericPciBusState {
                pio_addr_reg: pio_addr_reg.into(),
            };

            // saved state sanity checks
            {
                if self.state.pio_addr_reg.reserved() != 0 {
                    return Err(RestoreError::InvalidSavedState(
                        GenericPciBusRestoreError::AddressNonZeroReserved.into(),
                    ));
                }

                if self.state.pio_addr_reg.register() & 0b11 != 0 {
                    return Err(RestoreError::InvalidSavedState(
                        GenericPciBusRestoreError::AddressNotDwordAligned.into(),
                    ));
                }
            }

            Ok(())
        }
    }
}
